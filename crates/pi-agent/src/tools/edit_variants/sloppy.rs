//! `sloppy` wire mode: `§`/`«` operations with `⟪old│new⟫` selections,
//! `…` gaps, and `＋` add lines.
//!
//! Ports the documented grammar and matching core of
//! `packages/coding-agent/src/edit/sloppy.ts`: literal-anchored raw matching,
//! a whitespace-insensitive normalized retry for whole multi-literal, gap, and
//! selection-bearing patterns, fuzzy retry under upstream's combination caps,
//! a unique single-punctuation-insertion fallback outside `«*`, unique-match
//! enforcement (`«*` applies to every match), gap captures and gap re-emission
//! in REWRITE, inline selection rewrites, parse and apply recovery, and atomic
//! all-or-nothing application per section. A REWRITE is final text: selection
//! markers are refused, an unclaimed whole-line `…` is refused instead of being
//! written literally, and a `…` that re-emits a multiline capture is only
//! claimed when the gap ends its line. Byte boundaries are never invented from
//! near-matches beyond the ported recovery passes; unresolved payloads fail
//! closed with upstream guidance.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use serde_json::Value;

use super::{fsops, paths, text};
use crate::{
	state::{AgentContext, ToolResult},
	tools::ToolError,
};

pub const OPENER: char = '«';
pub const REWRITE_HEADER: char = '»';
pub const SELECT_OPEN: char = '⟪';
pub const SELECT_CLOSE: char = '⟫';
pub const SELECT_DIVIDER: char = '│';
pub const GAP: char = '…';
pub const ADD_LINE: char = '＋';
pub const SECTION_OPENER: char = '§';

const MAX_CANDIDATES: usize = 200;
const MAX_COMBINATIONS: usize = 20_000;

pub const DESCRIPTION: &str = "Sloppy `§` edit payload.\n\n\
Sections start with `§path` (or `[path]`); `§*path` edits every match. Lines:\n\
- `«` or `«*` opens an operation (continued in the current file with bare `«`).\n\
- MATCH lines are the current text; `⟪old│new⟫` replaces inside a line, `⟪│new⟫` inserts, `⟪old│⟫` deletes, `…` skips an unknown gap.\n\
- `＋text` adds a line at that position.\n\
- `»` starts the final REWRITE; an empty rewrite deletes the match. `»N` re-emits operation N's rewrite.\n\
Text matches byte-for-byte; a non-unique match is refused. Operations apply atomically per file and across sections — no file is written unless every operation applies.";

pub fn schema() -> Value {
	serde_json::json!({
		"type": "object",
		"properties": {
			"input": {
				"type": "string",
				"description": "Sloppy payload: `§path` sections containing `«*` operations with MATCH lines and a `»` REWRITE. See `sloppy` tool description."
			}
		},
		"required": ["input"]
	})
}

#[derive(Debug, Clone)]
pub struct SloppySection {
	pub path: String,
	pub body: String,
}

#[derive(Debug, Clone)]
struct Operation {
	pattern_text: String,
	source_pattern_text: String,
	rewrite: Rewrite,
	all: bool,
	/// Pattern-only op applied as a deletion; justified only when another op
	/// re-emits the block.
	assumed_deletion: bool,
	/// Marker-less op read as desired text; a no-op means the assertion holds.
	desired_state: bool,
	recovery_note: Option<String>,
}

#[derive(Debug, Clone)]
enum Rewrite {
	Explicit(String),
	Inline(Vec<String>),
}

#[derive(Debug, Clone)]
enum Token {
	Literal { text: String, normalized: String },
	Gap { capture_index: usize, line_bounded: bool },
}

#[derive(Debug, Clone)]
struct ParsedPattern {
	tokens: Vec<Token>,
	insertion: bool,
	line_insertion: bool,
	selection_start: usize,
	selection_end: usize,
	selection_pairs: Vec<SelectionPair>,
	literal_fallback: Option<LiteralFallback>,
}

#[derive(Debug, Clone)]
struct LiteralFallback {
	normalized: String,
	selection_start: usize,
	selection_end: usize,
	insertion: bool,
}

#[derive(Debug, Clone, Copy)]
struct SelectionPair {
	start: usize,
	end: usize,
	line_insertion: bool,
	/// The whole selection spans gaps only; its raw capture includes padding
	/// outside the desired text, so the apply side trims to the anchor line.
	gap_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Boundary {
	Start,
	End,
	Empty,
}

#[derive(Debug, Clone)]
struct Candidate {
	start: usize,
	end: usize,
	match_start: usize,
	match_end: usize,
	captures: Vec<String>,
	selection_spans: Vec<(usize, usize)>,
}

#[derive(Debug, Clone)]
struct Normalized {
	text: String,
	starts: Vec<usize>,
	ends: Vec<usize>,
}

impl Normalized {
	fn start(&self, byte_index: usize, fallback: usize) -> usize {
		self.starts.get(byte_index).copied().unwrap_or(fallback)
	}

	fn end(&self, byte_index: usize, fallback: usize) -> usize {
		if byte_index == 0 {
			return 0;
		}
		self.ends.get(byte_index.saturating_sub(1)).copied().unwrap_or(fallback)
	}
}

pub async fn execute(args: &Value, ctx: &AgentContext) -> Result<ToolResult, ToolError> {
	let input = super::require_str(args, "input")?;
	let sections = split_sections(&input);
	if sections.is_empty() {
		return Err(ToolError::InvalidArguments(
			"No `§path` sections found. Start with a header such as `[src/main.rs]` followed by `§path`.".to_string(),
		));
	}
	let multi_file = sections.len() > 1;
	let mut prepared = Vec::with_capacity(sections.len());
	for section in &sections {
		let authored = section.path.clone();
		let (absolute, display) = paths::resolve_edit_path(&authored, &ctx.cwd, true, &ctx.cancellation_token).await?;
		// Models copy read selectors (`file.ts:23`, `file.ts:grep=x`) into edit paths.
		let (absolute, mut display) = match tokio::fs::metadata(&absolute).await {
			Ok(_) => (absolute, display),
			Err(_) => {
				let stripped = strip_read_selector(&authored);
				match stripped {
					Some(base) if base != authored => {
						let (resolved, resolved_display) =
							paths::resolve_edit_path(&base, &ctx.cwd, true, &ctx.cancellation_token).await?;
						(resolved, resolved_display)
					},
					_ => (absolute, display),
				}
			},
		};
		if display.is_empty() {
			display = authored;
		}
		let raw = fsops::read_text(&absolute, &display).await?;
		let (bom, text, ending) = text::normalize_file(&raw);
		let mut notes = Vec::new();
		let new_text = apply_operations(&text, &section.body, &display, &absolute.to_string_lossy(), &mut notes).map_err(|message| {
			if multi_file {
				ToolError::ExecutionFailed(format!(
					"[{display}]: {message}\nNo files were modified — sections apply atomically."
				))
			} else {
				ToolError::ExecutionFailed(message)
			}
		})?;
		if new_text == text {
			return Err(ToolError::ExecutionFailed(format!(
				"Edits to {display} resulted in no changes being made."
			)));
		}
		prepared.push((absolute, display, raw, bom, ending, text, new_text, notes));
	}
	let mut lines = vec!["Success. Updated the following files:".to_string()];
	let mut files = Vec::new();
	let mut all_warnings = Vec::new();
	for (absolute, display, raw, bom, ending, text, new_text, notes) in prepared {
		let final_text = text::denormalize_file(&bom, &new_text, ending);
		fsops::commit_text(&absolute, &display, &raw, &final_text, &ctx.cancellation_token, None).await?;
		let diff = text::unified_diff(&display, &text, &new_text);
		lines.push(format!("M {display}"));
		if !notes.is_empty() {
			lines.push(format!("Warnings for {display}:"));
			lines.extend(notes.iter().cloned());
			all_warnings.extend(notes.iter().cloned());
		}
		files.push(serde_json::json!({ "path": display, "op": "update", "diff": diff }));
	}
	let content = lines.join("\n");
	Ok(super::result(content, serde_json::json!({ "files": files, "warnings": all_warnings })))
}

fn strip_read_selector(path: &str) -> Option<String> {
	let colon = path.rfind(':')?;
	let base = &path[..colon];
	if base.is_empty() || base.contains('/') && base.ends_with('/') {
		return None;
	}
	if base.ends_with(':') {
		return None;
	}
	Some(base.to_string())
}

// ── Section splitting ─────────────────────────────────────────────────────

pub fn split_sections(input: &str) -> Vec<SloppySection> {
	let raw: Vec<String> = input.split('\n').map(str::to_string).collect();
	let stripped = strip_envelope_noise(&raw);
	let mut lines: Vec<&str> = stripped.iter().map(String::as_str).collect();
	while lines.first().is_some_and(|line| line.trim().is_empty()) {
		lines.remove(0);
	}
	let path_header = |line: &str| -> Option<String> {
		let trimmed = line.trim();
		let inner = trimmed.strip_prefix('[')?.strip_suffix(']')?;
		(!inner.trim().is_empty()).then(|| inner.trim().to_string())
	};
	let opener = |line: &str| -> Option<(bool, String)> {
		let rest = line.strip_prefix(SECTION_OPENER)?;
		let (all, rest) = match rest.strip_prefix('*') {
			Some(rest) => (true, rest),
			None => (false, rest),
		};
		Some((all, rest.trim().to_string()))
	};
	let first_is_header = lines
		.first()
		.is_some_and(|line| path_header(line).is_some() || opener(line).is_some_and(|(_, path)| !path.is_empty()));
	if !first_is_header {
		return Vec::new();
	}
	let mut sections: Vec<SloppySection> = Vec::new();
	let mut bodies: HashMap<String, String> = HashMap::new();
	let mut current_path = String::new();
	let mut current_body: Vec<String> = Vec::new();
	let mut flush = |paths: &mut Vec<String>, current_path: &str, current_body: &mut Vec<String>, bodies: &mut HashMap<String, String>| {
		if current_path.is_empty() {
			current_body.clear();
			return;
		}
		let body = bodies.entry(current_path.to_string()).or_insert_with(|| {
			paths.push(current_path.to_string());
			String::new()
		});
		if !body.is_empty() {
			body.push('\n');
		}
		body.push_str(&current_body.join("\n"));
		current_body.clear();
	};
	let mut order: Vec<String> = Vec::new();
	for (index, line) in lines.iter().enumerate() {
		if let Some((all, path)) = opener(line) {
			if !path.is_empty() {
				flush(&mut order, &current_path, &mut current_body, &mut bodies);
				current_path = path;
			}
			current_body.push(if all { format!("{OPENER}*") } else { OPENER.to_string() });
			continue;
		}
		if let Some(header) = path_header(line)
			&& (index == 0 || starts_operation(&lines, index + 1))
		{
			flush(&mut order, &current_path, &mut current_body, &mut bodies);
			current_path = header;
			continue;
		}
		current_body.push((*line).to_string());
	}
	flush(&mut order, &current_path, &mut current_body, &mut bodies);
	for path in order {
		let body = bodies.remove(&path).unwrap_or_default();
		sections.push(SloppySection { path, body });
	}
	sections
}

fn starts_operation(lines: &[&str], from: usize) -> bool {
	for line in lines.iter().skip(from) {
		let trimmed = line.trim();
		if trimmed.is_empty() {
			continue;
		}
		return trimmed.starts_with(OPENER) || trimmed.starts_with(SECTION_OPENER);
	}
	false
}

// ── Operation parsing ─────────────────────────────────────────────────────

fn parse_operations(input: &str, content: &str, path: &str) -> Result<Vec<Operation>, String> {
	let payload = normalize_input(input);
	let mut lines: Vec<String> = payload.split('\n').map(str::to_string).collect();
	while lines.first().is_some_and(|line| line.trim().is_empty()) {
		lines.remove(0);
	}
	while lines.last().is_some_and(|line| line.trim().is_empty()) {
		lines.pop();
	}
	if lines.is_empty() {
		return Err(format!("Empty patch. Start with {OPENER}."));
	}
	if let Some(recovered) = recover_alternating_separators(&lines, content)
		.or_else(|| recover_bracket_pairs(&lines, content))
	{
		lines = recovered;
	}
	let mut operations: Vec<Operation> = Vec::new();
	#[derive(PartialEq, Eq, Clone, Copy)]
	enum State {
		Outside,
		Pattern,
		Rewrite,
	}
	let mut state = State::Outside;
	let mut all_matches = false;
	let mut pattern_lines: Vec<String> = Vec::new();
	let mut rewrite_lines: Vec<String> = Vec::new();
	let mut pending_separator: Vec<(usize, String)> = Vec::new();

	fn parsed_opener(line: &str) -> Result<Option<bool>, String> {
		let trimmed = line.trim();
		let Some(rest) = trimmed.strip_prefix(OPENER) else {
			return Ok(None);
		};
		if rest.is_empty() {
			return Ok(Some(false));
		}
		if rest == "*" {
			return Ok(Some(true));
		}
		if !rest.is_empty() && rest.chars().all(|ch| ch.is_ascii_digit()) {
			return Err(format!(
				"{trimmed} is not a valid opener. Use {OPENER} with a pattern that matches once — add context only the intended match has — or {OPENER}* to change every match."
			));
		}
		Ok(None)
	}

	let mut index = 0;
	while index < lines.len() {
		let line = lines[index].clone();
		let trimmed = line.trim();
		let register_reference = is_register_reference(trimmed);
		if trimmed == "«»" {
			if state == State::Pattern && pattern_lines.iter().any(|entry| !entry.trim().is_empty()) {
				state = State::Rewrite;
			}
			index += 1;
			continue;
		}
		let opener = parsed_opener(&line)?;
		if opener.is_none()
			&& (trimmed.starts_with(OPENER) && trimmed != "«»"
				|| trimmed.starts_with(REWRITE_HEADER) && trimmed != "»" && !register_reference)
		{
			return Err(format!(
				"Invalid control line \"{trimmed}\"; use only {OPENER}, {OPENER}*, {REWRITE_HEADER}, or {REWRITE_HEADER}N in REWRITE."
			));
		}
		if state == State::Outside {
			match opener {
				Some(all) => {
					all_matches = all;
					pattern_lines.clear();
					rewrite_lines.clear();
					state = State::Pattern;
				},
				None if trimmed.is_empty() => {},
				None => return Err(format!("Expected {OPENER} on input line {}.", index + 1)),
			}
			index += 1;
			continue;
		}
		if state == State::Pattern {
			let accumulated = pattern_lines.join("\n");
			let open_count = accumulated.matches(SELECT_OPEN).count();
			let close_count = accumulated.matches(SELECT_CLOSE).count();
			let markers_balanced = open_count == close_count;
			if trimmed == "»" {
				state = State::Rewrite;
			} else if trimmed == "***" && markers_balanced && pattern_lines.iter().any(|entry| !entry.trim().is_empty()) {
				state = State::Rewrite;
			} else if trimmed == "⟫" && markers_balanced && pattern_lines.iter().any(|entry| !entry.trim().is_empty()) {
				state = State::Rewrite;
			} else if register_reference && markers_balanced {
				if !pattern_lines.iter().any(|entry| !entry.trim().is_empty()) {
					return Err(format!("{trimmed} is valid only in REWRITE, never MATCH."));
				}
				if pattern_lines.join("\n").contains(SELECT_OPEN) {
					// Inline selection already carries the change; treat the
					// trailing reference as noise.
					state = State::Rewrite;
				} else {
					return Err(format!(
						"{trimmed} after MATCH reads as the {REWRITE_HEADER} separator, leaving REWRITE empty. Put {REWRITE_HEADER} on its own line, then the final text."
					));
				}
			} else if let Some(all) = opener {
				if pattern_lines.iter().any(|entry| !entry.trim().is_empty()) {
					finish_operation(
						&mut operations,
						&pattern_lines,
						&rewrite_lines,
						all_matches,
						false,
						content,
						path,
						&mut pending_separator,
					)?;
				}
				all_matches = all;
				pattern_lines.clear();
				rewrite_lines.clear();
			} else {
				pattern_lines.push(line);
			}
			index += 1;
			continue;
		}
		// Rewrite state.
		if let Some(all) = opener {
			finish_operation(
				&mut operations,
				&pattern_lines,
				&rewrite_lines,
				all_matches,
				true,
				content,
				path,
				&mut pending_separator,
			)?;
			all_matches = all;
			pattern_lines.clear();
			rewrite_lines.clear();
			state = State::Pattern;
		} else if trimmed == "***" {
			// Stray apply-patch structure; never final text.
		} else if trimmed == "»" {
			let next_content = lines.iter().skip(index + 1).find(|entry| !entry.trim().is_empty());
			if let Some(next) = next_content
				&& parsed_opener(next.as_str())?.is_none()
			{
				return Err(format!(
					"Operation {} has a second {REWRITE_HEADER} line.",
					operations.len() + 1
				));
			}
		} else if trimmed == "⟫" && rewrite_lines.join("\n").matches(SELECT_OPEN).count() == rewrite_lines.join("\n").matches(SELECT_CLOSE).count() {
			// Stray block terminator; REWRITE is final text.
		} else {
			rewrite_lines.push(line);
		}
		index += 1;
	}
	if state == State::Rewrite || state == State::Pattern {
		finish_operation(
			&mut operations,
			&pattern_lines,
			&rewrite_lines,
			all_matches,
			state == State::Rewrite,
			content,
			path,
			&mut pending_separator,
		)?;
	}
	if operations.is_empty() {
		return Err(format!("Empty patch. Start with {OPENER}."));
	}
	for (operation_index, operation) in operations.iter().enumerate() {
		let rewrites: Vec<&String> = match &operation.rewrite {
			Rewrite::Explicit(text) => vec![text],
			Rewrite::Inline(replacements) => replacements.iter().collect(),
		};
		for rewrite in rewrites {
			for line in rewrite.split('\n') {
				if let Some(reference) = register_number(line.trim())
					&& reference >= operation_index + 1
				{
					return Err(format!(
						"{REWRITE_HEADER}{reference} must reference an earlier operation, not self/forward."
					));
				}
			}
		}
	}
	for (operation_index, message) in pending_separator {
		let pattern_normalized = normalize_text(&operations[operation_index].pattern_text).text;
		let justified = operations.iter().enumerate().any(|(other_index, other)| {
			if other_index == operation_index {
				return false;
			}
			let rewrites: Vec<&String> = match &other.rewrite {
				Rewrite::Explicit(text) => vec![text],
				Rewrite::Inline(replacements) => replacements.iter().collect(),
			};
			rewrites.iter().any(|rewrite| {
				normalize_text(rewrite).text.contains(&pattern_normalized)
					|| rewrite.split('\n').any(|line| line.trim() == format!("{REWRITE_HEADER}{}", operation_index + 1))
			})
		});
		if !justified {
			return Err(message);
		}
	}
	Ok(operations)
}

fn create_operation(
	pattern_text: &str,
	rewrite_text: &str,
	all: bool,
	number: usize,
	content: &str,
	path: &str,
) -> Result<Operation, String> {
	let source_pattern_text = pattern_text.to_string();
	let mut embedded = embed_add_lines(pattern_text);
	let mut note: Option<String> = None;
	if let Some(repaired) = recover_stray_close_divider(&embedded) {
		embedded = repaired;
		note = Some(format!(
			"Note: operation {number} wrote {SELECT_CLOSE} where the {SELECT_DIVIDER} divider belongs; {SELECT_OPEN}old{SELECT_CLOSE}new{SELECT_CLOSE} was read as {SELECT_OPEN}old{SELECT_DIVIDER}new{SELECT_CLOSE}."
		));
	}
	if rewrite_text.is_empty() && has_bare_desired(&embedded) {
		embedded = embed_bare_desired(&embedded);
	}
	if !has_inline_selection(&embedded)
		&& !rewrite_text.trim().is_empty()
		&& let Some(directive) = recover_directive_rewrite(&embedded, rewrite_text)
	{
		let mut operation = create_operation(&directive, "", all, number, content, path)?;
		operation.source_pattern_text = source_pattern_text;
		operation.recovery_note = Some(format!(
			"Note: operation {number}'s REWRITE listed {SELECT_OPEN}old{SELECT_DIVIDER}new{SELECT_CLOSE} directives; they were applied inside the MATCH."
		));
		return Ok(operation);
	}
	let inline = parse_inline_pattern(&embedded, number)?;
	if inline.replacements.is_some() && !rewrite_text.trim().is_empty() {
		return recover_mixed_rewrite(&inline, rewrite_text, all, number, &source_pattern_text, content, path);
	}
	if let Some(replacements) = &inline.replacements
		&& replacements.len() == 1
		&& replacements[0].is_empty()
		&& let Some(remainder) = whole_match_deletion_pattern(&inline.pattern_text)
	{
		let parsed = parse_pattern(&remainder, number)?;
		verify_explicit_match(content, &parsed, path, number, all)?;
		return Ok(Operation {
			pattern_text: remainder,
			source_pattern_text,
			rewrite: Rewrite::Explicit(String::new()),
			all,
			assumed_deletion: false,
			desired_state: false,
			recovery_note: note,
		});
	}
	let mut final_pattern = match inline.replacements {
		Some(_) => relocate_selection_lines(&inline.pattern_text),
		None => embedded.clone(),
	};
	if inline.replacements.is_none()
		&& !rewrite_text.trim().is_empty()
		&& let Some(expanded) = expand_echoed_line_selection(&final_pattern, rewrite_text)
	{
		note = Some(format!(
			"Note: operation {number}'s REWRITE restated the whole selection-bearing line, so the full line was replaced."
		));
		final_pattern = expanded;
	}
	let parsed = parse_pattern(&final_pattern, number)?;
	let rewrite = match inline.replacements {
		Some(replacements) => Rewrite::Inline(replacements),
		None => Rewrite::Explicit(rewrite_text.to_string()),
	};
	if matches!(&rewrite, Rewrite::Explicit(text) if text.is_empty()) {
		verify_explicit_match(content, &parsed, path, number, all)?;
	}
	Ok(Operation {
		pattern_text: final_pattern,
		source_pattern_text,
		rewrite,
		all,
		assumed_deletion: false,
		desired_state: false,
		recovery_note: note,
	})
}

/// A sole `⟪│⟫` line at the end of MATCH means "the match becomes nothing":
/// insertion of nothing is a no-op, so the only consistent reading is deletion.
fn whole_match_deletion_pattern(pattern: &str) -> Option<String> {
	let line_start = pattern.rfind('\n').map(|index| index + 1).unwrap_or(0);
	if pattern[line_start..].trim() != format!("{SELECT_OPEN}{SELECT_CLOSE}") {
		return None;
	}
	let remainder = if line_start > 0 { &pattern[..line_start - 1] } else { "" };
	(!normalize_text(remainder).text.is_empty()).then(|| remainder.to_string())
}

/// Reconcile inline `⟪old│new⟫` with a non-empty REWRITE. Inline wins only
/// when the REWRITE merely restates it; otherwise the REWRITE is final text
/// and the current sides become a plain MATCH.
fn recover_mixed_rewrite(
	inline: &InlineParse,
	rewrite_text: &str,
	all: bool,
	number: usize,
	source_pattern_text: &str,
	content: &str,
	path: &str,
) -> Result<Operation, String> {
	let replacements = inline.replacements.as_deref().unwrap_or_default();
	let mut current_text = String::new();
	let mut desired_text = String::new();
	let mut replacement_index = 0usize;
	let mut index = 0usize;
	while index < inline.pattern_text.len() {
		let Some(open_rel) = inline.pattern_text[index..].find(SELECT_OPEN) else {
			let tail = &inline.pattern_text[index..];
			current_text.push_str(tail);
			desired_text.push_str(tail);
			break;
		};
		let open = index + open_rel;
		let body = open + SELECT_OPEN.len_utf8();
		let Some(close_rel) = inline.pattern_text[body..].find(SELECT_CLOSE) else {
			break;
		};
		let close = body + close_rel;
		let between = &inline.pattern_text[index..open];
		let selected = &inline.pattern_text[body..close];
		current_text.push_str(between);
		current_text.push_str(selected);
		desired_text.push_str(between);
		desired_text.push_str(replacements.get(replacement_index).map(String::as_str).unwrap_or(selected));
		replacement_index += 1;
		index = close + SELECT_CLOSE.len_utf8();
	}
	let normalized_rewrite = normalize_text(rewrite_text).text;
	let normalized_desired = normalize_text(&desired_text).text;
	let rewrite_lines: Vec<String> = rewrite_text
		.split('\n')
		.map(|line| normalize_text(line).text)
		.filter(|line| !line.is_empty())
		.collect();
	let echoes_desired_sides = !rewrite_lines.is_empty()
		&& rewrite_lines.len() == replacements.len()
		&& rewrite_lines
			.iter()
			.enumerate()
			.all(|(line_index, line)| *line == normalize_text(&replacements[line_index]).text);
	let rewrite_is_redundant = normalized_rewrite.is_empty()
		|| normalized_rewrite == normalized_desired
		|| normalized_rewrite == normalize_text(&current_text).text
		|| normalized_desired.contains(&normalized_rewrite)
		|| echoes_desired_sides;
	let mixed_forms = format!(
		"Note: operation {number} combined {SELECT_OPEN}current{SELECT_DIVIDER}desired{SELECT_CLOSE} replacements with a {REWRITE_HEADER} REWRITE"
	);
	if rewrite_is_redundant {
		let mut operation = create_operation(source_pattern_text, "", all, number, content, path)?;
		let note = format!(
			"{mixed_forms}; the REWRITE only restated the inline result and was ignored. Use one form per operation."
		);
		operation.recovery_note = Some(match operation.recovery_note {
			Some(existing) => format!("{note}\n{existing}"),
			None => note,
		});
		return Ok(operation);
	}
	let mut operation = create_operation(&current_text, rewrite_text, all, number, content, path)?;
	let note = format!(
		"{mixed_forms}; the {REWRITE_HEADER} REWRITE was applied as the final text for the match. Use one form per operation."
	);
	operation.recovery_note = Some(match operation.recovery_note {
		Some(existing) => format!("{note}\n{existing}"),
		None => note,
	});
	Ok(operation)
}

fn is_register_reference(trimmed: &str) -> bool {
	register_number(trimmed).is_some()
}

fn register_number(trimmed: &str) -> Option<usize> {
	let rest = trimmed.strip_prefix(REWRITE_HEADER)?;
	if rest.is_empty() || !rest.chars().all(|ch| ch.is_ascii_digit()) || rest.starts_with('0') {
		return None;
	}
	let number: usize = rest.parse().ok()?;
	(number >= 1).then_some(number)
}

fn desired_state_operation(
	source_pattern: &str,
	all: bool,
	number: usize,
	content: &str,
	path: &str,
) -> Option<(Operation, Option<Operation>)> {
	let mut desired = create_operation(source_pattern, source_pattern, all, number, content, path).ok()?;
	desired.desired_state = true;
	let pattern = parse_pattern(&desired.pattern_text, number).ok()?;
	match locate(content, &pattern, &desired, number, path, None) {
		Err(error) if error.contains(" is ambiguous: ") => {
			let mut all_operation = desired.clone();
			all_operation.all = true;
			let all_candidates = locate(content, &pattern, &all_operation, number, path, None).ok()?;
			if all_candidates.len() != 2 {
				return None;
			}
			let (first, second) = if all_candidates[0].match_start < all_candidates[1].match_start {
				(&all_candidates[0], &all_candidates[1])
			} else {
				(&all_candidates[1], &all_candidates[0])
			};
			if first.match_end > second.match_start
				|| !content[first.match_end..second.match_start].trim().is_empty()
			{
				return None;
			}
			let span_start = content[..first.match_start].rfind('\n').map(|value| value + 1).unwrap_or(0);
			let span_text = &content[span_start..second.match_end];
			let replacement = &content[span_start..first.match_end];
			let mut collapse = create_operation(span_text, replacement, false, number, content, path).ok()?;
			collapse.recovery_note = Some(format!(
				"Note: operation {number} stated desired text that currently appears twice back-to-back; the duplicate copy was collapsed."
			));
			Some((desired, Some(collapse)))
		},
		Ok(candidates) => {
			let matched = candidates.first()?;
			let matched_text = &content[matched.match_start..matched.match_end];
			let matched_normalized = normalize_text(matched_text).text;
			let mut neighbors_duplicate = false;
			if matched_normalized.chars().count() >= 8 {
				let before = normalize_text(&content[..matched.match_start]).text;
				let after = normalize_text(&content[matched.match_end..]).text;
				let matched_chars: Vec<char> = matched_normalized.chars().collect();
				let mut overlap = matched_chars.len();
				while overlap >= 8 && !neighbors_duplicate {
					let prefix: String = matched_chars[..overlap].iter().collect();
					let suffix: String = matched_chars[matched_chars.len() - overlap..].iter().collect();
					neighbors_duplicate = before.ends_with(&prefix) || after.starts_with(&suffix);
					overlap -= 1;
				}
			}
			if neighbors_duplicate {
				desired.recovery_note = Some(format!(
					"Note: operation {number} stated desired text without markers; the closest matching block was replaced with it. Mark changes explicitly with {SELECT_OPEN}old{SELECT_DIVIDER}new{SELECT_CLOSE}."
				));
				Some((desired, None))
			} else {
				None
			}
		},
		Err(_) => None,
	}
}

fn finish_operation(
	operations: &mut Vec<Operation>,
	pattern_lines: &[String],
	rewrite_lines: &[String],
	all: bool,
	saw_separator: bool,
	content: &str,
	path: &str,
	pending_separator: &mut Vec<(usize, String)>,
) -> Result<(), String> {
	let source_pattern = normalize_block_for(pattern_lines, false);
	if source_pattern.trim().is_empty() {
		return Ok(());
	}
	let rewrite_text = normalize_block_for(rewrite_lines, true);
	let number = operations.len() + 1;
	let inline_or_add = source_pattern.contains(SELECT_OPEN) || source_pattern.contains(ADD_LINE) || has_bare_desired(&source_pattern);
	if !saw_separator && !inline_or_add {
		if let Some((pattern_text, rewrite)) = recover_missing_separator(pattern_lines, content) {
			let mut operation = create_operation(&pattern_text, &rewrite, all, number, content, path)?;
			operation.recovery_note = Some(format!(
				"Note: operation {number} omitted {REWRITE_HEADER}; the MATCH/REWRITE split was inferred from a unique exact block."
			));
			operations.push(operation);
			return Ok(());
		}
		for candidate in diff_shaped_candidates(&source_pattern) {
			if let Ok(operation) = create_operation(&candidate, "", all, number, content, path) {
				operations.push(operation);
				return Ok(());
			}
		}
		if !source_pattern.contains(GAP) {
			if let Some((operation, collapse)) =
				desired_state_operation(&source_pattern, all, number, content, path)
			{
				if let Some(collapse) = collapse {
					operations.push(collapse);
				} else {
					operations.push(operation);
				}
				return Ok(());
			}
			if let Some(block) = closest_desired_block(content, &source_pattern) {
				let mut operation = create_operation(&block, &source_pattern, all, number, content, path)?;
				operation.recovery_note = Some(format!(
					"Note: operation {number} stated desired text without markers; the closest matching block was replaced with it."
				));
				operations.push(operation);
				return Ok(());
			}
		}
		if !source_pattern.contains('\n') || normalize_text(&source_pattern).text.len() < 24 {
			let mut payload: Vec<String> = pattern_lines.to_vec();
			payload.push(REWRITE_HEADER.to_string());
			payload.push("<new text>".to_string());
			return Err(format!(
				"Operation {number} needs {REWRITE_HEADER}.\nCopy-ready corrected payload (fill in the new text):\n{}",
				payload.join("\n")
			));
		}
		// Multiline pattern-only block: assume deletion, justified only when a
		// sibling re-emits it. The justification check runs after parsing.
		let mut operation = create_operation(&source_pattern, "", all, number, content, path)?;
		operation.assumed_deletion = true;
		operations.push(operation);
		pending_separator.push((number - 1, format!("Operation {number} needs {REWRITE_HEADER}.")));
		return Ok(());
	}
	let operation = create_operation(&source_pattern, &rewrite_text, all, number, content, path)?;
	operations.push(operation);
	Ok(())
}

fn normalize_block_for(lines: &[String], rewrite: bool) -> String {
	let mut cleaned: Vec<String> = lines.to_vec();
	if !rewrite {
		for line in &mut cleaned {
			if line.trim().is_empty() {
				*line = String::new();
			}
		}
	}
	while cleaned.last().is_some_and(|line| line.trim().is_empty()) {
		cleaned.pop();
	}
	if rewrite && cleaned.first().is_some_and(|line| line.trim() == "//") {
		cleaned.remove(0);
	}
	if rewrite {
		for line in &mut cleaned {
			let indent_len = text::leading_whitespace_len(line);
			if line[indent_len..].starts_with(ADD_LINE) {
				line.replace_range(indent_len..indent_len + ADD_LINE.len_utf8(), "");
			}
		}
		let is_old = |line: &str| line.starts_with('-') && !line.starts_with("---");
		let is_new = |line: &str| line.starts_with('+') && !line.starts_with("+++");
		let has_old = cleaned.iter().any(|line| is_old(line));
		let has_new = cleaned.iter().any(|line| is_new(line));
		if has_old && has_new {
			cleaned = cleaned
				.into_iter()
				.filter(|line| !is_old(line) && !(line.starts_with("---") || line.starts_with("+++")))
				.map(|line| line.strip_prefix('+').map(str::to_string).unwrap_or(line))
				.collect();
		} else {
			let non_blank = cleaned.iter().any(|line| !line.trim().is_empty());
			let all_plus = non_blank
				&& cleaned
					.iter()
					.filter(|line| !line.trim().is_empty())
					.all(|line| line.starts_with('+'));
			if all_plus {
				cleaned = cleaned
					.into_iter()
					.map(|line| line.strip_prefix('+').map(str::to_string).unwrap_or(line))
					.collect();
			}
		}
	}
	cleaned.join("\n")
}

// ── Payload normalization and parse recovery ─────────────────────────────

fn is_path_header_line(line: &str) -> bool {
	let trimmed = line.trim();
	trimmed
		.strip_prefix('[')
		.and_then(|rest| rest.strip_suffix(']'))
		.is_some_and(|inner| !inner.trim().is_empty())
}

fn parse_section_opener_line(line: &str) -> Option<(bool, String)> {
	let rest = line.strip_prefix(SECTION_OPENER)?;
	let (all, rest) = match rest.strip_prefix('*') {
		Some(rest) => (true, rest),
		None => (false, rest),
	};
	Some((all, rest.trim().to_string()))
}

fn envelope_words(line: &str) -> bool {
	let trimmed = line.trim_start();
	let rest = match trimmed.strip_prefix("Begin") {
		Some(rest) => rest,
		None => match trimmed.strip_prefix("End") {
			Some(rest) => rest,
			None => return false,
		},
	};
	let rest = rest.strip_prefix(" of").unwrap_or(rest);
	let rest = rest.trim_start();
	matches!(rest.split_whitespace().next(), Some("patch" | "edits" | "edit" | "file"))
}

fn envelope_line(line: &str) -> bool {
	let trimmed = line.trim();
	let Some(rest) = trimmed.strip_prefix("***") else {
		return false;
	};
	let rest = rest.trim_start();
	if envelope_words(rest) {
		return true;
	}
	rest.starts_with("Abort")
		|| rest.starts_with("Update File:")
		|| rest.starts_with("Add File:")
		|| rest.starts_with("Delete File:")
}

fn envelope_end(line: &str) -> bool {
	let trimmed = line.trim();
	let Some(rest) = trimmed.strip_prefix("***") else {
		return false;
	};
	rest.trim_start().starts_with("End")
}

/// Drop envelope sentinels, join split `***` / `End Patch` pairs, and discard
/// commentary that follows an End sentinel until the next `[path]` header.
fn strip_envelope_noise(raw: &[String]) -> Vec<String> {
	let mut lines = Vec::new();
	let mut skipping = false;
	let mut index = 0;
	while index < raw.len() {
		let mut line = raw[index].clone();
		if line.trim() == "***"
			&& index + 1 < raw.len()
			&& envelope_words(raw[index + 1].trim())
		{
			line = format!("*** {}", raw[index + 1].trim());
			index += 1;
		}
		if envelope_line(&line) {
			skipping = envelope_end(&line);
			index += 1;
			continue;
		}
		if skipping {
			if is_path_header_line(&line) {
				skipping = false;
				lines.push(line);
			}
			index += 1;
			continue;
		}
		lines.push(line);
		index += 1;
	}
	lines
}

/// `«pattern` / `« old` / `»new` gluing: a marker followed by content is split
/// into the control line and its content. `«»`, a bare marker, and a `»N`
/// register reference stay intact.
fn split_glued_marker(line: &str) -> Option<(String, String)> {
	let trimmed_start = line.trim_start_matches([' ', '\t']);
	let indent_len = line.len() - trimmed_start.len();
	if indent_len > 0 {
		return None;
	}
	if line.trim() == "«»" {
		return None;
	}
	let (marker, rest) = if let Some(rest) = line.strip_prefix("«*") {
		("«*", rest)
	} else if let Some(rest) = line.strip_prefix(OPENER) {
		("«", rest)
	} else if let Some(rest) = line.strip_prefix(REWRITE_HEADER) {
		("»", rest)
	} else {
		return None;
	};
	let content = if rest.starts_with([' ', '\t']) {
		rest.trim_start_matches([' ', '\t'])
	} else {
		rest
	};
	if content.is_empty() {
		return None;
	}
	if marker == "»" && register_number(content).is_some() {
		return None;
	}
	Some((marker.to_string(), content.to_string()))
}

fn normalize_input(input: &str) -> String {
	let raw: Vec<String> = input.split('\n').map(str::to_string).collect();
	let mut lines: Vec<String> = strip_envelope_noise(&raw);
	for line in &mut lines {
		if let Some((all, _path)) = parse_section_opener_line(line) {
			*line = format!("{OPENER}{}", if all { "*" } else { "" });
		}
	}
	let mut split_lines = Vec::new();
	for line in lines {
		match split_glued_marker(&line) {
			Some((marker, content)) => {
				split_lines.push(marker);
				split_lines.push(content);
			},
			None => split_lines.push(line),
		}
	}
	let mut lines = split_lines;
	while lines.first().is_some_and(|line| line.trim().is_empty()) {
		lines.remove(0);
	}
	let fence = lines.first().is_some_and(|line| {
		let trimmed = line.trim();
		trimmed.starts_with("```")
			&& matches!(
				trimmed.trim_start_matches("```").trim().to_ascii_lowercase().as_str(),
				"" | "text" | "typescript" | "ts" | "tsx" | "javascript" | "js"
			)
	});
	if fence {
		lines.remove(0);
		while lines.last().is_some_and(|line| line.trim().is_empty()) {
			lines.pop();
		}
		if lines.last().is_some_and(|line| line.trim() == "```") {
			lines.pop();
		}
	}
	while lines.first().is_some_and(|line| line.trim().is_empty()) {
		lines.remove(0);
	}
	while lines.last().is_some_and(|line| line.trim().is_empty()) {
		lines.pop();
	}
	lines.join("\n")
}

fn exact_occurrences(content: &str, pattern: &str) -> Vec<usize> {
	if pattern.is_empty() {
		return Vec::new();
	}
	let mut out = Vec::new();
	let mut from = 0usize;
	while let Some(found) = content[from..].find(pattern) {
		let at = from + found;
		out.push(at);
		from = at + pattern.len();
	}
	out
}

#[derive(Debug, Clone, Copy)]
struct Occurrence {
	start: usize,
	end: usize,
	distance: usize,
	punctuation_edits: usize,
}

fn operator_signature(text: &str) -> String {
	text.chars()
		.filter(|ch| !(ch.is_alphanumeric() || matches!(ch, '_' | '$')))
		.collect()
}

fn differs_by_one_punctuation_insertion(left: &str, right: &str) -> bool {
	let left_chars: Vec<char> = left.chars().collect();
	let right_chars: Vec<char> = right.chars().collect();
	if left_chars.len().abs_diff(right_chars.len()) != 1 {
		return false;
	}
	let (shorter, longer) = if left_chars.len() < right_chars.len() {
		(&left_chars, &right_chars)
	} else {
		(&right_chars, &left_chars)
	};
	let mut short_index = 0usize;
	let mut inserted: Option<char> = None;
	for &long_char in longer {
		if shorter.get(short_index) == Some(&long_char) {
			short_index += 1;
			continue;
		}
		if inserted.is_some() {
			return false;
		}
		inserted = Some(long_char);
	}
	inserted.is_some_and(|ch| !matches!(ch, '{' | '}' | '(' | ')' | '[' | ']'))
}

fn exact_overlapping_occurrences(content: &str, pattern: &str) -> Vec<Occurrence> {
	if pattern.is_empty() || content.len() < pattern.len() {
		return Vec::new();
	}
	let mut out = Vec::new();
	let mut from = 0usize;
	while from <= content.len() - pattern.len() {
		let Some(found) = content[from..].find(pattern) else { break };
		let start = from + found;
		out.push(Occurrence { start, end: start + pattern.len(), distance: 0, punctuation_edits: 0 });
		from = content[start..]
			.chars()
			.next()
			.map_or(content.len(), |ch| start + ch.len_utf8());
	}
	out
}

fn char_starts(text: &str) -> Vec<usize> {
	text.char_indices().map(|(offset, _)| offset).collect()
}

fn char_index_at(starts: &[usize], byte: usize) -> usize {
	starts.partition_point(|start| *start < byte)
}

fn fuzzy_occurrences(content: &str, pattern: &str, allow_single_punctuation_insertion: bool) -> Vec<Occurrence> {
	// Upstream gates on JS string length, i.e. UTF-16 code units, not bytes.
	let content_units = content.encode_utf16().count();
	if content.is_empty() || content_units > 50_000 {
		return Vec::new();
	}
	let content_starts = char_starts(content);
	let pattern_starts = char_starts(pattern);
	let content_chars = content_starts.len();
	let pattern_chars = pattern_starts.len();
	if pattern_chars < 6 {
		return exact_overlapping_occurrences(content, pattern);
	}
	let limit = 3usize.min(1.max((pattern_chars as f64 * 0.12).floor() as usize));
	let seed_chars = 5usize.min(3.max(pattern_chars - limit));
	let offsets = [0usize, (pattern_chars - seed_chars) / 2, pattern_chars - seed_chars];
	let structural = operator_signature(pattern);
	let mut candidate_starts = std::collections::BTreeSet::new();
	for offset in offsets {
		let seed_start = pattern_starts[offset];
		let seed_end = pattern_starts.get(offset + seed_chars).copied().unwrap_or(pattern.len());
		let seed = &pattern[seed_start..seed_end];
		if seed.len() > content.len() {
			continue;
		}
		let mut from = 0usize;
		while from <= content.len() - seed.len() {
			let Some(found) = content[from..].find(seed) else { break };
			let anchor = from + found;
			let anchor_chars = char_index_at(&content_starts, anchor);
			for delta in -(limit as isize)..=(limit as isize) {
				let start = anchor_chars as isize - offset as isize + delta;
				if start >= 0 && (start as usize) < content_chars {
					candidate_starts.insert(start as usize);
				}
			}
			from = content[anchor..]
				.chars()
				.next()
				.map_or(content.len(), |ch| anchor + ch.len_utf8());
		}
	}
	if candidate_starts.is_empty() && content_units <= 10_000 {
		candidate_starts.extend(0..content_chars);
	}
	let mut raw: Vec<Occurrence> = Vec::new();
	for start_chars in candidate_starts {
		let mut best: Option<Occurrence> = None;
		for length in pattern_chars.saturating_sub(limit).max(1)..=pattern_chars + limit {
			let end_chars = start_chars + length;
			if end_chars > content_chars {
				continue;
			}
			let start = content_starts[start_chars];
			let end = if end_chars == content_chars { content.len() } else { content_starts[end_chars] };
			let candidate_text = &content[start..end];
			let candidate_signature = operator_signature(candidate_text);
			let punctuation_edits = usize::from(candidate_signature != structural);
			if punctuation_edits != 0
				&& (!allow_single_punctuation_insertion
					|| !differs_by_one_punctuation_insertion(&structural, &candidate_signature))
			{
				continue;
			}
			let distance = text::levenshtein_distance(pattern, candidate_text);
			if distance > limit || best.as_ref().is_some_and(|best| distance >= best.distance) {
				continue;
			}
			best = Some(Occurrence { start, end, distance, punctuation_edits });
		}
		if let Some(best) = best {
			raw.push(best);
		}
	}
	raw.sort_by(|left, right| left.distance.cmp(&right.distance).then(left.start.cmp(&right.start)));
	let mut distinct: Vec<Occurrence> = Vec::new();
	for candidate in raw {
		if distinct
			.iter()
			.any(|kept| candidate.start < kept.end && candidate.end > kept.start)
		{
			continue;
		}
		distinct.push(candidate);
	}
	distinct.sort_by_key(|occurrence| occurrence.start);
	distinct
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateMode {
	Normalized,
	Fuzzy,
}

struct CandidateCollection {
	candidates: Vec<Candidate>,
	overflow: bool,
}

struct MappedCollector<'a> {
	content: &'a str,
	normalized: &'a Normalized,
	pattern: &'a ParsedPattern,
	allow_single_punctuation_insertion: bool,
	literal_indices: Vec<usize>,
	occurrences: Vec<Option<Vec<Occurrence>>>,
	chosen: Vec<Option<Occurrence>>,
	candidates: Vec<Candidate>,
	combinations: usize,
	overflow: bool,
}

impl MappedCollector<'_> {
	fn source_start(&self, offset: usize, fallback: usize) -> usize {
		self.normalized.start(offset, fallback)
	}

	fn source_end(&self, offset: usize, fallback: usize) -> usize {
		self.normalized.end(offset, fallback)
	}

	fn resolve(&self, boundary: usize, kind: Boundary, line_insert: bool) -> usize {
		let preceding_literal = |at: usize| -> Option<usize> {
			(0..at.min(self.pattern.tokens.len()))
				.rev()
				.find(|index| matches!(self.pattern.tokens[*index], Token::Literal { .. }))
		};
		let following_literal = |at: usize| -> Option<usize> {
			(at.min(self.pattern.tokens.len())..self.pattern.tokens.len())
				.find(|index| matches!(self.pattern.tokens[*index], Token::Literal { .. }))
		};
		let previous = preceding_literal(boundary).and_then(|index| self.chosen[index]);
		let next = following_literal(boundary).and_then(|index| self.chosen[index]);
		let immediate_previous =
			boundary > 0 && matches!(self.pattern.tokens.get(boundary - 1), Some(Token::Literal { .. }));
		let immediate_next = matches!(self.pattern.tokens.get(boundary), Some(Token::Literal { .. }));
		match kind {
			Boundary::Empty => {
				if let Some(next) = next {
					return self.source_start(next.start, self.normalized.text.len());
				}
				if let Some(previous) = previous {
					let offset = self.source_end(previous.end, self.normalized.text.len());
					if line_insert
						&& offset > 0
						&& self.content.as_bytes().get(offset - 1).copied() != Some(b'\n')
						&& let Some(newline) = self.content[offset..].find('\n')
					{
						return offset + newline + 1;
					}
					return offset;
				}
				0
			},
			Boundary::Start => {
				if immediate_next && let Some(next) = next {
					return self.source_start(next.start, self.normalized.text.len());
				}
				if let Some(previous) = previous {
					return self.source_end(previous.end, self.normalized.text.len());
				}
				if let Some(next) = next {
					return self.source_start(next.start, self.normalized.text.len());
				}
				0
			},
			Boundary::End => {
				if immediate_previous && let Some(previous) = previous {
					return self.source_end(previous.end, self.normalized.text.len());
				}
				if let Some(next) = next {
					return self.source_start(next.start, self.normalized.text.len());
				}
				if let Some(previous) = previous {
					return self.source_end(previous.end, self.normalized.text.len());
				}
				0
			},
		}
	}

	fn push_candidate(&mut self) {
		let punctuation_total: usize = self
			.literal_indices
			.iter()
			.filter_map(|index| self.chosen[*index])
			.map(|occurrence| occurrence.punctuation_edits)
			.sum();
		if self.allow_single_punctuation_insertion && punctuation_total > 1 {
			return;
		}
		self.combinations += 1;
		let start = self.resolve(
			self.pattern.selection_start,
			if self.pattern.insertion { Boundary::Empty } else { Boundary::Start },
			self.pattern.line_insertion,
		);
		let end = self.resolve(
			self.pattern.selection_end,
			if self.pattern.insertion { Boundary::Empty } else { Boundary::End },
			self.pattern.line_insertion,
		);
		let Some(first) = self.literal_indices.first().and_then(|index| self.chosen[*index]) else {
			return;
		};
		let Some(last) = self.literal_indices.last().and_then(|index| self.chosen[*index]) else {
			return;
		};
		if start > end {
			return;
		}
		let mut captures = vec![String::new(); self.pattern.tokens.iter().filter(|token| matches!(token, Token::Gap { .. })).count()];
		for (token_index, token) in self.pattern.tokens.iter().enumerate() {
			let Token::Gap { capture_index, .. } = token else { continue };
			let before = (0..token_index)
				.rev()
				.find(|index| matches!(self.pattern.tokens[*index], Token::Literal { .. }))
				.and_then(|index| self.chosen[index]);
			let after = (token_index + 1..self.pattern.tokens.len())
				.find(|index| matches!(self.pattern.tokens[*index], Token::Literal { .. }))
				.and_then(|index| self.chosen[index]);
			let (Some(before), Some(after)) = (before, after) else { return };
			let capture_start = self.source_end(before.end, self.content.len());
			let capture_end = self.source_start(after.start, self.content.len());
			if capture_start > capture_end || !self.content.is_char_boundary(capture_start) || !self.content.is_char_boundary(capture_end) {
				return;
			}
			captures[*capture_index] = self.content[capture_start..capture_end].to_string();
		}
		let selection_spans: Vec<(usize, usize)> = self
			.pattern
			.selection_pairs
			.iter()
			.map(|pair| {
				let empty = pair.start == pair.end;
				let pair_start = self.resolve(pair.start, if empty { Boundary::Empty } else { Boundary::Start }, pair.line_insertion);
				let pair_end = self.resolve(pair.end, if empty { Boundary::Empty } else { Boundary::End }, pair.line_insertion);
				(pair_start, pair_end)
			})
			.collect();
		if selection_spans.iter().any(|(start, end)| start > end) {
			return;
		}
		let candidate = Candidate {
			start,
			end,
			match_start: self.source_start(first.start, 0),
			match_end: self.source_end(last.end, self.content.len()),
			captures,
			selection_spans,
		};
		let duplicate_index = self
			.candidates
			.iter()
			.position(|existing| {
				existing.start == candidate.start
					&& existing.end == candidate.end
					&& selected_capture_indices(self.pattern)
						.iter()
						.all(|index| existing.captures.get(*index) == candidate.captures.get(*index))
			});
		match duplicate_index {
			Some(index) => {
				let replace = {
					let existing = &self.candidates[index];
					candidate.match_end.saturating_sub(candidate.match_start)
						< existing.match_end.saturating_sub(existing.match_start)
				};
				if replace {
					self.candidates[index] = candidate;
				}
			},
			None => self.candidates.push(candidate),
		}
	}

	fn visit(&mut self, position: usize) {
		if self.overflow {
			return;
		}
		if self.candidates.len() >= MAX_CANDIDATES || self.combinations >= MAX_COMBINATIONS {
			self.overflow = true;
			return;
		}
		if position == self.literal_indices.len() {
			self.push_candidate();
			return;
		}
		let token_index = self.literal_indices[position];
		let previous_index = position.checked_sub(1).map(|previous| self.literal_indices[previous]);
		let previous = previous_index.and_then(|index| self.chosen[index]);
		let gap_tokens: Vec<bool> = previous_index.map_or_else(Vec::new, |previous| {
			self.pattern.tokens[previous + 1..token_index]
				.iter()
				.filter_map(|token| match token {
					Token::Gap { line_bounded, .. } => Some(*line_bounded),
					_ => None,
				})
				.collect()
		});
		let has_gap = !gap_tokens.is_empty();
		let occurrences = self.occurrences[token_index].clone().unwrap_or_default();
		for occurrence in occurrences {
			if let Some(previous) = previous {
				if has_gap {
					if occurrence.start < previous.end {
						continue;
					}
				} else if occurrence.start != previous.end {
					continue;
				}
				if gap_tokens.iter().any(|line_bounded| *line_bounded) {
					let gap_start = self.source_end(previous.end, self.content.len());
					let gap_end = self.source_start(occurrence.start, self.content.len());
					if gap_start <= gap_end && self.content[gap_start..gap_end].contains('\n') {
						continue;
					}
				}
			}
			self.chosen[token_index] = Some(occurrence);
			self.visit(position + 1);
			self.chosen[token_index] = None;
		}
	}
}

fn collect_mapped_candidates(
	content: &str,
	normalized_source: &Normalized,
	pattern: &ParsedPattern,
	mode: CandidateMode,
	allow_single_punctuation_insertion: bool,
) -> CandidateCollection {
	let literal_indices: Vec<usize> = pattern
		.tokens
		.iter()
		.enumerate()
		.filter_map(|(index, token)| matches!(token, Token::Literal { .. }).then_some(index))
		.collect();
	if literal_indices.is_empty() {
		return CandidateCollection { candidates: Vec::new(), overflow: false };
	}
	let mut occurrences: Vec<Option<Vec<Occurrence>>> = vec![None; pattern.tokens.len()];
	for &index in &literal_indices {
		let Token::Literal { normalized, .. } = &pattern.tokens[index] else { continue };
		let found = match mode {
			CandidateMode::Normalized => {
				exact_overlapping_occurrences(&normalized_source.text, normalized.as_str())
			},
			CandidateMode::Fuzzy => fuzzy_occurrences(
				&normalized_source.text,
				normalized.as_str(),
				allow_single_punctuation_insertion,
			),
		};
		if found.is_empty() {
			return CandidateCollection { candidates: Vec::new(), overflow: false };
		}
		occurrences[index] = Some(found);
	}
	let mut collector = MappedCollector {
		content,
		normalized: normalized_source,
		pattern,
		allow_single_punctuation_insertion,
		literal_indices,
		occurrences,
		chosen: vec![None; pattern.tokens.len()],
		candidates: Vec::new(),
		combinations: 0,
		overflow: false,
	};
	collector.visit(0);
	let mut candidates = collector.candidates;
	let minimal: Vec<Candidate> = candidates
		.iter()
		.filter(|candidate| {
			!candidates
				.iter()
				.any(|other| other.match_start == candidate.match_start && other.match_end < candidate.match_end)
		})
		.cloned()
		.collect();
	candidates = minimal;
	candidates.sort_by(|left, right| {
		left.start
			.cmp(&right.start)
			.then(left.match_start.cmp(&right.match_start))
			.then(left.match_end.cmp(&right.match_end))
	});
	CandidateCollection { candidates, overflow: collector.overflow }
}

fn recover_missing_separator(lines: &[String], content: &str) -> Option<(String, String)> {
	if is_diff_shaped(&lines.join("\n")) {
		return None;
	}
	let mut candidates: Vec<(String, String)> = Vec::new();
	for split in 1..lines.len() {
		let mut remainder_start = split;
		while remainder_start < lines.len() && lines[remainder_start].trim().is_empty() {
			remainder_start += 1;
		}
		if remainder_start >= lines.len() {
			continue;
		}
		let pattern_text = normalize_block_for(&lines[..split], false);
		let rewrite = normalize_block_for(&lines[remainder_start..], true);
		if pattern_text.len() < 4 || rewrite.replace(GAP, "").trim().is_empty() {
			continue;
		}
		if rewrite
			.split('\n')
			.any(|line| !line.trim().is_empty() && line.trim().replace(GAP, "").is_empty())
		{
			continue;
		}
		let matches = exact_occurrences(content, &pattern_text);
		if matches.len() != 1 {
			continue;
		}
		let through_first = normalize_block_for(&lines[..=remainder_start], false);
		if content[matches[0]..].starts_with(&through_first) {
			continue;
		}
		if !candidates
			.iter()
			.any(|candidate| candidate.0 == pattern_text && candidate.1 == rewrite)
		{
			candidates.push((pattern_text, rewrite));
		}
	}
	(candidates.len() == 1).then(|| candidates.remove(0))
}

fn loose_opener(line: &str) -> Option<bool> {
	let trimmed = line.trim();
	let rest = trimmed.strip_prefix(OPENER)?;
	if rest.is_empty() {
		Some(false)
	} else if rest == "*" {
		Some(true)
	} else {
		None
	}
}

fn recover_alternating_separators(lines: &[String], content: &str) -> Option<Vec<String>> {
	if lines.iter().any(|line| line.trim() == "»") {
		return None;
	}
	let headers: Vec<usize> = lines
		.iter()
		.enumerate()
		.filter_map(|(index, line)| loose_opener(line).map(|_| index))
		.collect();
	if headers.len() < 2 || headers.len() % 2 != 0 || headers.first() != Some(&0) {
		return None;
	}
	let normalized_content = normalize_text(content).text;
	let mut recovered = Vec::new();
	let mut pair = 0usize;
	while pair < headers.len() {
		let match_start = headers[pair];
		let rewrite_start = headers[pair + 1];
		let next = headers.get(pair + 2).copied().unwrap_or(lines.len());
		let match_lines = &lines[match_start + 1..rewrite_start];
		let rewrite_lines = &lines[rewrite_start + 1..next];
		let normalized_match = normalize_text(&normalize_block_for(match_lines, false)).text;
		let normalized_rewrite = normalize_text(&normalize_block_for(rewrite_lines, true)).text;
		if normalized_match.is_empty()
			|| normalized_rewrite.is_empty()
			|| rewrite_lines.iter().any(|line| line.contains(SELECT_OPEN))
			|| !normalized_content.contains(&normalized_match)
			|| normalized_content.contains(&normalized_rewrite)
		{
			return None;
		}
		recovered.push(lines[match_start].clone());
		recovered.extend(match_lines.iter().cloned());
		recovered.push(REWRITE_HEADER.to_string());
		recovered.extend(rewrite_lines.iter().cloned());
		pair += 2;
	}
	Some(recovered)
}

fn recover_bracket_pairs(lines: &[String], content: &str) -> Option<Vec<String>> {
	let mut items: Vec<(usize, usize)> = Vec::new();
	let mut open: Option<usize> = None;
	for (index, line) in lines.iter().enumerate() {
		let trimmed = line.trim();
		if loose_opener(line).is_some() {
			if open.is_some() {
				return None;
			}
			open = Some(index);
		} else if trimmed == "»" {
			let Some(opened) = open.take() else {
				return None;
			};
			items.push((opened, index));
		} else if open.is_none() && !trimmed.is_empty() {
			return None;
		}
	}
	if open.is_some() || items.len() < 2 || items.len() % 2 != 0 {
		return None;
	}
	let normalized_content = normalize_text(content).text;
	let mut recovered = Vec::new();
	let mut pair = 0usize;
	while pair < items.len() {
		let first = items[pair];
		let second = items[pair + 1];
		let match_lines = &lines[first.0 + 1..first.1];
		let rewrite_lines = &lines[second.0 + 1..second.1];
		let normalized_match = normalize_text(&normalize_block_for(match_lines, false)).text;
		let normalized_rewrite = normalize_text(&normalize_block_for(rewrite_lines, true)).text;
		if normalized_match.is_empty()
			|| normalized_rewrite.is_empty()
			|| rewrite_lines.iter().any(|line| line.contains(SELECT_OPEN))
			|| !normalized_content.contains(&normalized_match)
			|| normalized_content.contains(&normalized_rewrite)
		{
			return None;
		}
		recovered.push(lines[first.0].clone());
		recovered.extend(match_lines.iter().cloned());
		recovered.push(REWRITE_HEADER.to_string());
		recovered.extend(rewrite_lines.iter().cloned());
		pair += 2;
	}
	Some(recovered)
}

pub(crate) fn relocate_selection_lines(pattern: &str) -> String {
	let mut lines: Vec<String> = pattern.split('\n').map(str::to_string).collect();
	let mut index = 1usize;
	while index < lines.len() {
		let previous = lines[index - 1].clone();
		let current = lines[index].trim();
		let Some(only) = current.strip_prefix(SELECT_OPEN).and_then(|rest| rest.strip_suffix(SELECT_CLOSE)) else {
			index += 1;
			continue;
		};
		if only.contains(SELECT_OPEN)
			|| only.contains(SELECT_CLOSE)
			|| previous.contains(SELECT_OPEN)
			|| previous.contains(GAP)
			|| previous.trim().is_empty()
		{
			index += 1;
			continue;
		}
		let old = normalize_text(only).text;
		if old.is_empty() {
			index += 1;
			continue;
		}
		let previous_normalized = normalize_text(&previous);
		let first = previous_normalized.text.find(&old);
		let Some(first) = first else {
			index += 1;
			continue;
		};
		if previous_normalized.text[first + old.chars().next().map_or(1, char::len_utf8)..].contains(&old) {
			index += 1;
			continue;
		}
		if previous_normalized.text == old {
			index += 1;
			continue;
		}
		let raw_start = previous_normalized.starts[first];
		let raw_end = previous_normalized.ends[first + old.len() - 1];
		lines[index - 1] = format!("{}{}{}", &previous[..raw_start], current, &previous[raw_end..]);
		lines.remove(index);
	}
	lines.join("\n")
}

fn has_inline_selection(pattern: &str) -> bool {
	let mut selected = false;
	let mut index = 0usize;
	while index < pattern.len() {
		if pattern[index..].starts_with(SELECT_OPEN) {
			selected = true;
			index += SELECT_OPEN.len_utf8();
		} else if pattern[index..].starts_with(SELECT_CLOSE) {
			selected = false;
			index += SELECT_CLOSE.len_utf8();
		} else if selected && pattern[index..].starts_with(SELECT_DIVIDER) {
			return true;
		} else {
			index += pattern[index..].chars().next().map(char::len_utf8).unwrap_or(1);
		}
	}
	false
}

fn has_bare_desired(pattern: &str) -> bool {
	let mut from = 0usize;
	while let Some(found) = pattern[from..].find(SELECT_OPEN) {
		let start = from + found + SELECT_OPEN.len_utf8();
		let Some(close) = pattern[start..].find(SELECT_CLOSE) else {
			return false;
		};
		let inner = &pattern[start..start + close];
		if !inner.is_empty()
			&& !inner.contains(SELECT_OPEN)
			&& !inner.contains(SELECT_CLOSE)
			&& !inner.contains(SELECT_DIVIDER)
			&& !inner.contains(GAP)
			&& !inner.contains('\n')
		{
			return true;
		}
		from = start + close + SELECT_CLOSE.len_utf8();
	}
	false
}

fn embed_bare_desired(pattern: &str) -> String {
	let mut out = String::with_capacity(pattern.len() + 16);
	let mut index = 0usize;
	while index < pattern.len() {
		if pattern[index..].starts_with(SELECT_OPEN) {
			let start = index + SELECT_OPEN.len_utf8();
			if let Some(close) = pattern[start..].find(SELECT_CLOSE) {
				let inner = &pattern[start..start + close];
				if !inner.is_empty()
					&& !inner.contains(SELECT_OPEN)
					&& !inner.contains(SELECT_CLOSE)
					&& !inner.contains(SELECT_DIVIDER)
					&& !inner.contains(GAP)
					&& !inner.contains('\n')
				{
					out.push(SELECT_OPEN);
					out.push(GAP);
					out.push(SELECT_DIVIDER);
					out.push_str(inner);
					out.push(SELECT_CLOSE);
					index = start + close + SELECT_CLOSE.len_utf8();
					continue;
				}
			}
		}
		let ch = pattern[index..].chars().next().expect("char boundary");
		out.push(ch);
		index += ch.len_utf8();
	}
	out
}

fn expand_echoed_line_selection(pattern: &str, rewrite: &str) -> Option<String> {
	let rewrite_lines: Vec<&str> = rewrite.split('\n').filter(|line| !line.trim().is_empty()).collect();
	if rewrite_lines.len() != 1 {
		return None;
	}
	let mut lines: Vec<String> = pattern.split('\n').map(str::to_string).collect();
	let index = lines.iter().position(|line| line.contains(SELECT_OPEN))?;
	let line = lines[index].clone();
	if line.contains(GAP) {
		return None;
	}
	let open = line.find(SELECT_OPEN)?;
	let close = line[open + SELECT_OPEN.len_utf8()..].find(SELECT_CLOSE)? + open + SELECT_OPEN.len_utf8();
	if line[open + SELECT_OPEN.len_utf8()..].contains(SELECT_OPEN) {
		return None;
	}
	if lines.iter().enumerate().any(|(at, entry)| at != index && entry.contains(SELECT_OPEN)) {
		return None;
	}
	let prefix = &line[..open];
	let suffix = &line[close + SELECT_CLOSE.len_utf8()..];
	if prefix.trim().is_empty() || suffix.contains(SELECT_CLOSE) {
		return None;
	}
	let selected = &line[open + SELECT_OPEN.len_utf8()..close];
	if selected.contains(SELECT_DIVIDER) {
		return None;
	}
	if !normalize_text(rewrite_lines[0]).text.starts_with(&normalize_text(prefix).text) {
		return None;
	}
	lines[index] = format!("{SELECT_OPEN}{prefix}{selected}{suffix}{SELECT_CLOSE}");
	Some(lines.join("\n"))
}

fn recover_directive_rewrite(pattern: &str, rewrite: &str) -> Option<String> {
	let mut unique: Vec<(String, String)> = Vec::new();
	for line in rewrite.split('\n').map(str::trim).filter(|line| !line.is_empty()) {
		let rest = line.strip_prefix(SELECT_OPEN)?.strip_suffix(SELECT_CLOSE)?;
		let mut parts = rest.split(SELECT_DIVIDER);
		let old = parts.next()?;
		let next = parts.next()?;
		if parts.next().is_some()
			|| old.is_empty()
			|| old.contains(SELECT_OPEN)
			|| old.contains(SELECT_CLOSE)
			|| next.contains(SELECT_OPEN)
			|| next.contains(SELECT_CLOSE)
		{
			return None;
		}
		if !unique.iter().any(|(seen, _)| seen == old) {
			unique.push((old.to_string(), next.to_string()));
		}
	}
	if unique.is_empty() {
		return None;
	}
	unique.sort_by_key(|(old, _)| std::cmp::Reverse(old.chars().count()));
	let mut segments: Vec<(String, bool)> = vec![(pattern.to_string(), false)];
	let mut matched_any = false;
	let mut current_text = String::new();
	for (old, next) in &unique {
		let mut rewritten: Vec<(String, bool)> = Vec::new();
		for (text, locked) in &segments {
			if *locked {
				rewritten.push((text.clone(), true));
				continue;
			}
			let mut from = 0usize;
			while let Some(found) = text[from..].find(old.as_str()) {
				let at = from + found;
				if at > from {
					rewritten.push((text[from..at].to_string(), false));
				}
				rewritten.push((format!("{SELECT_OPEN}{old}{SELECT_DIVIDER}{next}{SELECT_CLOSE}"), true));
				matched_any = true;
				from = at + old.len();
			}
			if from < text.len() {
				rewritten.push((text[from..].to_string(), false));
			}
		}
		segments = rewritten;
	}
	if !matched_any {
		return None;
	}
	for (text, _) in segments {
		current_text.push_str(&text);
	}
	Some(current_text)
}

fn recover_stray_close_divider(pattern: &str) -> Option<String> {
	let opens = pattern.matches(SELECT_OPEN).count();
	let closes = pattern.matches(SELECT_CLOSE).count();
	if closes <= opens {
		return None;
	}
	let mut repaired = String::with_capacity(pattern.len() + 1);
	let mut index = 0usize;
	let mut changed = false;
	while index < pattern.len() {
		if pattern[index..].starts_with(SELECT_OPEN) {
			let open_end = index + SELECT_OPEN.len_utf8();
			if let Some(first_close) = pattern[open_end..].find(SELECT_CLOSE) {
				let first_close = open_end + first_close;
				let second_start = first_close + SELECT_CLOSE.len_utf8();
				if let Some(second_close) = pattern[second_start..].find(SELECT_CLOSE) {
					let second_close = second_start + second_close;
					let old = &pattern[open_end..first_close];
					let new = &pattern[second_start..second_close];
					if !old.contains(SELECT_OPEN)
						&& !old.contains(SELECT_CLOSE)
						&& !old.contains(SELECT_DIVIDER)
						&& !old.contains('\n')
						&& !new.contains(SELECT_OPEN)
						&& !new.contains(SELECT_CLOSE)
						&& !new.contains(SELECT_DIVIDER)
						&& !new.contains('\n')
					{
						repaired.push_str(&format!("{SELECT_OPEN}{old}{SELECT_DIVIDER}{new}{SELECT_CLOSE}"));
						index = second_close + SELECT_CLOSE.len_utf8();
						changed = true;
						continue;
					}
				}
			}
		}
		let ch = pattern[index..].chars().next().expect("char boundary");
		repaired.push(ch);
		index += ch.len_utf8();
	}
	if !changed {
		return None;
	}
	let balanced = repaired.matches(SELECT_OPEN).count() == repaired.matches(SELECT_CLOSE).count();
	balanced.then_some(repaired)
}

fn is_diff_shaped(pattern: &str) -> bool {
	if pattern.contains(SELECT_OPEN) || pattern.contains(ADD_LINE) {
		return false;
	}
	let lines: Vec<&str> = pattern.split('\n').collect();
	let is_minus = |line: &str| line.starts_with('-') && !line.starts_with("--");
	if !lines.iter().any(|line| is_minus(line)) {
		return false;
	}
	lines.iter().any(|line| {
		(line.starts_with('+') && !line.starts_with("++"))
			|| line.trim().starts_with("@@")
			|| (line.starts_with(' ') && line.chars().nth(1).is_some_and(|ch| !ch.is_whitespace()))
	})
}

fn diff_shaped_candidates(pattern: &str) -> Vec<String> {
	if !is_diff_shaped(pattern) {
		return Vec::new();
	}
	let lines: Vec<&str> = pattern.split('\n').collect();
	let is_minus = |line: &str| line.starts_with('-') && !line.starts_with("--");
	let is_plus = |line: &str| line.starts_with('+') && !line.starts_with("++");
	let build = |strip_context_space: bool| -> String {
		let mut out: Vec<String> = Vec::new();
		let mut index = 0usize;
		while index < lines.len() {
			let line = lines[index];
			if line.starts_with("---") || line.starts_with("+++") {
				index += 1;
				continue;
			}
			if line.trim().starts_with("@@") {
				out.push(GAP.to_string());
				index += 1;
				continue;
			}
			if is_minus(line) {
				let mut removed = Vec::new();
				while index < lines.len() && is_minus(lines[index]) {
					removed.push(lines[index][1..].to_string());
					index += 1;
				}
				let mut added = Vec::new();
				while index < lines.len() && is_plus(lines[index]) {
					added.push(lines[index][1..].to_string());
					index += 1;
				}
				out.push(format!("{SELECT_OPEN}{}{SELECT_DIVIDER}{}{SELECT_CLOSE}", removed.join("\n"), added.join("\n")));
				continue;
			}
			if is_plus(line) {
				let mut added = Vec::new();
				while index < lines.len() && is_plus(lines[index]) {
					added.push(lines[index][1..].to_string());
					index += 1;
				}
				if let Some(previous) = out.last().cloned() {
					if previous != GAP.to_string() && !previous.contains(SELECT_OPEN) && !previous.trim().is_empty() {
						out.pop();
						out.push(format!(
							"{SELECT_OPEN}{previous}{SELECT_DIVIDER}{}{SELECT_CLOSE}",
							std::iter::once(previous.clone()).chain(added).collect::<Vec<_>>().join("\n")
						));
						continue;
					}
				}
				for entry in added {
					out.push(format!("{ADD_LINE}{entry}"));
				}
				continue;
			}
			out.push(if strip_context_space {
				line.strip_prefix(' ').unwrap_or(line).to_string()
			} else {
				line.to_string()
			});
			index += 1;
		}
		out.join("\n")
	};
	let spaced = build(true);
	let plain = build(false);
	if spaced == plain { vec![spaced] } else { vec![spaced, plain] }
}

fn closest_desired_block(content: &str, stated: &str) -> Option<String> {
	let stated_normalized = normalize_text(stated).text;
	if stated_normalized.chars().count() < 12 || stated_normalized.chars().count() > 1000 {
		return None;
	}
	let stated_line_count = stated.split('\n').count();
	let lines: Vec<&str> = content.split('\n').collect();
	if lines.len() < stated_line_count {
		return None;
	}
	let mut scores = Vec::new();
	let mut best: Option<(usize, f64)> = None;
	for index in 0..=lines.len() - stated_line_count {
		let window_normalized = normalize_text(&lines[index..index + stated_line_count].join("\n")).text;
		let max_length = 1.max(stated_normalized.chars().count().max(window_normalized.chars().count()));
		if window_normalized == stated_normalized {
			scores.push(0.0);
			best = Some((index, 0.0));
			continue;
		}
		let affix_echo = stated_normalized.starts_with(&window_normalized)
			|| window_normalized.starts_with(&stated_normalized)
			|| stated_normalized.ends_with(&window_normalized)
			|| window_normalized.ends_with(&stated_normalized);
		let length_gap = (window_normalized.chars().count() as f64
			- stated_normalized.chars().count() as f64)
			.abs()
			/ max_length as f64;
		if window_normalized.is_empty() || affix_echo || length_gap > 0.35 {
			scores.push(1.0);
			continue;
		}
		let score = text::levenshtein_distance(&stated_normalized, &window_normalized) as f64 / max_length as f64;
		scores.push(score);
		if best.map_or(true, |(_, best_score)| score < best_score) {
			best = Some((index, score));
		}
	}
	let (best_index, best_score) = best?;
	if best_score == 0.0 || best_score > 0.35 {
		return None;
	}
	for (index, score) in scores.iter().enumerate() {
		if index.abs_diff(best_index) < stated_line_count {
			continue;
		}
		if score - best_score < 0.1 {
			return None;
		}
	}
	let block = lines[best_index..best_index + stated_line_count].join("\n");
	let markers = [SELECT_OPEN, SELECT_CLOSE, SELECT_DIVIDER, GAP, ADD_LINE, REWRITE_HEADER, OPENER, SECTION_OPENER];
	if markers.iter().any(|marker| block.contains(*marker)) {
		return None;
	}
	Some(block)
}

const LITERAL_OPEN: &str = "\u{0}V8LITOPEN\u{0}";
const LITERAL_CLOSE: &str = "\u{0}V8LITCLOSE\u{0}";
const LITERAL_DIVIDER: &str = "\u{0}V8LITDIV\u{0}";

/// Hide selection-marker glyphs inside verbatim add-line content so selection
/// parsing cannot mistake them for structure. Add lines are final text.
fn encode_literal_markers(text: &str) -> String {
	text.replace(SELECT_OPEN, LITERAL_OPEN)
		.replace(SELECT_CLOSE, LITERAL_CLOSE)
		.replace(SELECT_DIVIDER, LITERAL_DIVIDER)
}

fn decode_literal_markers(text: &str) -> String {
	text.replace(LITERAL_OPEN, &SELECT_OPEN.to_string())
		.replace(LITERAL_CLOSE, &SELECT_CLOSE.to_string())
		.replace(LITERAL_DIVIDER, &SELECT_DIVIDER.to_string())
}

fn embed_add_lines(pattern_text: &str) -> String {
	if !pattern_text.contains(ADD_LINE) {
		return pattern_text.to_string();
	}
	let lines: Vec<&str> = pattern_text.split('\n').collect();
	let mut out: Vec<String> = Vec::new();
	let mut index = 0;
	while index < lines.len() {
		let line = lines[index];
		let indent_len = text::leading_whitespace_len(line);
		if !line[indent_len..].starts_with(ADD_LINE) {
			out.push(line.to_string());
			index += 1;
			continue;
		}
		let mut added = Vec::new();
		while index < lines.len() {
			let line = lines[index];
			let indent_len = text::leading_whitespace_len(line);
			if !line[indent_len..].starts_with(ADD_LINE) {
				break;
			}
			added.push(encode_literal_markers(&format!(
				"{}{}",
				&line[..indent_len],
				&line[indent_len + ADD_LINE.len_utf8()..]
			)));
			index += 1;
		}
		let selection = format!("{SELECT_OPEN}{SELECT_DIVIDER}{}{SELECT_CLOSE}", added.join("\n"));
		if index < lines.len() {
			out.push(format!("{selection}{}", lines[index]));
			index += 1;
		} else {
			out.push(selection);
		}
	}
	out.join("\n")
}

struct InlineParse {
	pattern_text: String,
	replacements: Option<Vec<String>>,
}

/// Split `⟪old│new⟫` selections into bare `⟪old⟫` anchors plus replacement
/// text. Bare selections keep their `»` rewrite. Mixing both forms is an
/// error, matching upstream `parseInlinePattern`.
fn parse_inline_pattern(pattern: &str, number: usize) -> Result<InlineParse, String> {
	let opens = pattern.matches(SELECT_OPEN).count();
	let closes = pattern.matches(SELECT_CLOSE).count();
	if opens != closes {
		return Err(if opens > closes {
			format!("Operation {number} has an unclosed selection marker {SELECT_OPEN}; add closing {SELECT_CLOSE}.")
		} else {
			format!("Operation {number} has an unmatched closing selection marker {SELECT_CLOSE}; add opening {SELECT_OPEN}.")
		});
	}
	let mut out = String::with_capacity(pattern.len());
	let mut replacements: Vec<String> = Vec::new();
	let mut saw_bare = false;
	let mut saw_inline = false;
	let mut index = 0usize;
	while index < pattern.len() {
		let ch = pattern[index..].chars().next().expect("char boundary");
		if ch == SELECT_CLOSE {
			return Err(format!(
				"Operation {number} has an unmatched closing selection marker {SELECT_CLOSE}; add opening {SELECT_OPEN}."
			));
		}
		if ch != SELECT_OPEN {
			out.push(ch);
			index += ch.len_utf8();
			continue;
		}
		let body_start = index + ch.len_utf8();
		let Some(close_rel) = pattern[body_start..].find(SELECT_CLOSE) else {
			return Err(format!(
				"Operation {number} has an unclosed selection marker {SELECT_OPEN}; add closing {SELECT_CLOSE}."
			));
		};
		let close = body_start + close_rel;
		let selected = &pattern[body_start..close];
		if selected.contains(SELECT_OPEN) {
			return Err(format!(
				"Operation {number} has nested selection markers; use one selection per replacement."
			));
		}
		let dividers: Vec<usize> = selected.match_indices(SELECT_DIVIDER).map(|(at, _)| at).collect();
		if dividers.is_empty() {
			saw_bare = true;
			out.push_str(&pattern[index..close + SELECT_CLOSE.len_utf8()]);
		} else {
			saw_inline = true;
			if dividers.len() == 1 {
				let divider = dividers[0];
				out.push_str(&format!(
					"{SELECT_OPEN}{}{SELECT_CLOSE}",
					&selected[..divider]
				));
				replacements.push(selected[divider + SELECT_DIVIDER.len_utf8()..].to_string());
			} else if dividers.len() % 2 == 1 {
				// Literal divider characters inside the selection: the middle one
				// is the real divider, the rest stay literal.
				let divider = dividers[(dividers.len() - 1) / 2];
				out.push_str(&format!(
					"{SELECT_OPEN}{}{SELECT_CLOSE}",
					&selected[..divider]
				));
				replacements.push(selected[divider + SELECT_DIVIDER.len_utf8()..].to_string());
			} else {
				// Ambiguous literal dividers: read as a deletion of the selection.
				out.push_str(&format!("{SELECT_OPEN}{selected}{SELECT_CLOSE}"));
				replacements.push(String::new());
			}
		}
		index = close + SELECT_CLOSE.len_utf8();
	}
	if saw_bare && saw_inline {
		return Err(format!(
			"Operation {number} mixes inline and bare selections. Use {SELECT_OPEN}old{SELECT_DIVIDER}new{SELECT_CLOSE} for every selection, or use a {REWRITE_HEADER} rewrite for all bare selections."
		));
	}
	Ok(InlineParse {
		pattern_text: out,
		replacements: saw_inline.then_some(replacements),
	})
}

fn parse_pattern(pattern: &str, number: usize) -> Result<ParsedPattern, String> {
	if pattern.trim().is_empty() {
		return Err(format!("Operation {number} has an empty pattern."));
	}
	let opens = pattern.matches(SELECT_OPEN).count();
	let closes = pattern.matches(SELECT_CLOSE).count();
	if opens != closes {
		return Err(format!(
			"Operation {number} has unbalanced {SELECT_OPEN}{SELECT_CLOSE} selection markers."
		));
	}
	let has_selection = pattern.contains(SELECT_OPEN);
	let has_gap = pattern.contains(GAP);
	if !has_selection && !has_gap {
		let normalized = normalize_text(pattern).text;
		if normalized.is_empty() {
			return Err(format!("Operation {number} has no visible current text."));
		}
		return Ok(ParsedPattern {
			tokens: vec![Token::Literal { text: pattern.to_string(), normalized }],
			insertion: false,
			line_insertion: false,
			selection_start: 0,
			selection_end: 1,
			selection_pairs: Vec::new(),
			literal_fallback: None,
		});
	}
	let mut tokens: Vec<Token> = Vec::new();
	let mut literal = String::new();
	let mut capture_count = 0usize;
	let mut boundaries: Vec<usize> = Vec::new();
	let mut selection_raw_offsets: Vec<usize> = Vec::new();
	let mut selection_at_line_start: Vec<bool> = Vec::new();
	let flush = |literal: &mut String, tokens: &mut Vec<Token>| {
		if literal.is_empty() {
			return;
		}
		let normalized = normalize_text(literal).text;
		if !normalized.is_empty() {
			tokens.push(Token::Literal { text: std::mem::take(literal), normalized });
		} else {
			literal.clear();
		}
	};
	let chars: Vec<char> = pattern.chars().collect();
	let mut index = 0;
	while index < chars.len() {
		let ch = chars[index];
		if ch == GAP {
			flush(&mut literal, &mut tokens);
			if matches!(tokens.last(), Some(Token::Gap { .. })) {
				return Err(format!("Operation {number} has adjacent {GAP}; use one ellipsis."));
			}
			// lineBounded when the gap has visible text on both sides of its line.
			let line_start = pattern[..byte_offset(&chars, index)].rfind('\n').map(|value| value + 1).unwrap_or(0);
			let line_end = pattern[byte_offset(&chars, index)..]
				.find('\n')
				.map(|value| byte_offset(&chars, index) + value)
				.unwrap_or(pattern.len());
			let before = pattern[line_start..byte_offset(&chars, index)]
				.replace(SELECT_OPEN, "")
				.replace(SELECT_CLOSE, "")
				.trim()
				.to_string();
			let after = pattern[byte_offset(&chars, index) + GAP.len_utf8()..line_end]
				.replace(SELECT_OPEN, "")
				.replace(SELECT_CLOSE, "")
				.trim()
				.to_string();
			tokens.push(Token::Gap {
				capture_index: capture_count,
				line_bounded: !before.is_empty() && !after.is_empty(),
			});
			capture_count += 1;
			index += 1;
			continue;
		}
		if ch == SELECT_OPEN || ch == SELECT_CLOSE {
			flush(&mut literal, &mut tokens);
			let raw_index = byte_offset(&chars, index);
			if ch == SELECT_OPEN {
				let line_start = pattern[..raw_index].rfind('\n').map(|value| value + 1).unwrap_or(0);
				selection_at_line_start.push(pattern[line_start..raw_index].trim().is_empty());
			}
			selection_raw_offsets.push(raw_index);
			boundaries.push(tokens.len());
			index += 1;
			continue;
		}
		literal.push(ch);
		index += 1;
	}
	flush(&mut literal, &mut tokens);
	let mut stripped_leading = 0usize;
	while matches!(tokens.first(), Some(Token::Gap { .. })) {
		tokens.remove(0);
		stripped_leading += 1;
	}
	while matches!(tokens.last(), Some(Token::Gap { .. })) {
		tokens.pop();
	}
	if stripped_leading > 0 {
		for token in &mut tokens {
			if let Token::Gap { capture_index, .. } = token {
				*capture_index = capture_index.saturating_sub(stripped_leading);
			}
		}
	}
	for boundary in &mut boundaries {
		*boundary = boundary.saturating_sub(stripped_leading).min(tokens.len());
	}
	let has_literal = tokens.iter().any(|token| match token {
		Token::Literal { normalized, .. } => normalized.chars().any(|ch| ch.is_alphanumeric() || ch == '_' || ch == '$'),
		_ => false,
	});
	if !has_literal {
		return Err(format!(
			"Operation {number} pattern is too generic; include a distinctive name or statement."
		));
	}
	let empty_double = boundaries.len() == 2 && boundaries[0] == boundaries[1];
	let insertion = boundaries.len() == 1 || empty_double;
	let explicit_single = boundaries.len() == 2 && !empty_double;
	let selection_start = if insertion || explicit_single { boundaries[0] } else { 0 };
	let selection_end = if insertion {
		selection_start
	} else if explicit_single {
		boundaries[1]
	} else {
		tokens.len()
	};
	let selection_pairs = if !boundaries.is_empty() && boundaries.len() % 2 == 0 {
		(0..boundaries.len() / 2)
			.map(|pair| {
				let start = boundaries[pair * 2];
				let end = boundaries[pair * 2 + 1];
				let gap_only = start < end
					&& tokens
						.get(start..end)
						.is_some_and(|slice| slice.iter().all(|token| matches!(token, Token::Gap { .. })));
				SelectionPair {
					start,
					end,
					line_insertion: start == end && selection_at_line_start.get(pair).copied().unwrap_or(false),
					gap_only,
				}
			})
			.collect()
	} else {
		Vec::new()
	};
	let line_insertion = insertion && selection_at_line_start.first().copied().unwrap_or(false);
	let literal_fallback = if selection_pairs.len() <= 1 && pattern.contains(GAP) {
		let markers_stripped = |text: &str| text.replace(SELECT_OPEN, "").replace(SELECT_CLOSE, "");
		let normalized = normalize_text(&markers_stripped(pattern)).text;
		let normalized_offset = |raw_offset: usize| {
			normalize_text(&markers_stripped(&pattern[..raw_offset.min(pattern.len())])).text.len()
		};
		let first = selection_raw_offsets.first().copied();
		let second = selection_raw_offsets.get(1).copied();
		Some(LiteralFallback {
			selection_start: if insertion || explicit_single {
				first.map(normalized_offset).unwrap_or(0)
			} else {
				0
			},
			selection_end: if insertion {
				first.map(normalized_offset).unwrap_or(0)
			} else if explicit_single {
				second.map(normalized_offset).unwrap_or(normalized.len())
			} else {
				normalized.len()
			},
			normalized,
			insertion,
		})
	} else {
		None
	};
	Ok(ParsedPattern {
		tokens,
		insertion,
		line_insertion,
		selection_start,
		selection_end,
		selection_pairs,
		literal_fallback,
	})
}

fn byte_offset(chars: &[char], char_index: usize) -> usize {
	chars[..char_index].iter().map(|ch| ch.len_utf8()).sum()
}

// ── Matching ──────────────────────────────────────────────────────────────

fn normalize_text(source: &str) -> Normalized {
	let mut text = String::new();
	let mut starts = Vec::new();
	let mut ends = Vec::new();
	for (offset, ch) in source.char_indices() {
		if ch.is_ascii() {
			if !(ch == '\t' || ch == '\n' || ch == '\r' || ch == '\u{0b}' || ch == '\u{0c}' || ch == ' ') {
				text.push(ch);
				starts.push(offset);
				ends.push(offset + 1);
			}
			continue;
		}
		for mapped in text::normalize_unicode(&ch.to_string()).chars() {
			if mapped.is_whitespace() {
				continue;
			}
			let start = text.len();
			text.push(mapped);
			for _ in start..text.len() {
				starts.push(offset);
				ends.push(offset + ch.len_utf8());
			}
		}
	}
	Normalized { text, starts, ends }
}

fn collect_raw_candidates(content: &str, pattern: &ParsedPattern) -> Vec<Candidate> {
	let literals: Vec<&str> = pattern
		.tokens
		.iter()
		.filter_map(|token| match token {
			Token::Literal { text, .. } => Some(text.as_str()),
			_ => None,
		})
		.collect();
	let Some(first) = literals.first() else {
		return Vec::new();
	};
	if first.is_empty() {
		return Vec::new();
	}
	let mut starts = Vec::new();
	let mut from = 0usize;
	while let Some(found) = content[from..].find(first) {
		let position = from + found;
		starts.push(position);
		// Upstream enumerates overlapping raw occurrences, advancing one code
		// unit past each hit. Advance one whole UTF-8 char here so a
		// self-overlapping literal (`aa` in `aaa`) is still seen as ambiguous.
		from = content[position..]
			.chars()
			.next()
			.map_or(content.len(), |ch| position + ch.len_utf8());
		if starts.len() > MAX_CANDIDATES {
			break;
		}
	}
	let mut candidates = Vec::new();
	for start in starts {
		if let Some(candidate) = match_from(content, pattern, start) {
			candidates.push(candidate);
			if candidates.len() > MAX_CANDIDATES {
				break;
			}
		}
	}
	candidates
}

fn match_from(content: &str, pattern: &ParsedPattern, start: usize) -> Option<Candidate> {
	let mut cursor = start;
	let mut captures: Vec<String> = Vec::new();
	let mut positions: Vec<(usize, usize)> = Vec::new();
	for (index, token) in pattern.tokens.iter().enumerate() {
		match token {
			Token::Literal { text, .. } => {
				if !content[cursor..].starts_with(text) {
					return None;
				}
				let token_start = cursor;
				cursor += text.len();
				positions.push((token_start, cursor));
			},
			Token::Gap { line_bounded, .. } => {
				let next_literal = pattern.tokens[index + 1..].iter().find_map(|token| match token {
					Token::Literal { text, .. } => Some(text.as_str()),
					_ => None,
				})?;
				let gap_start = cursor;
				let search_from = cursor;
				let found = if *line_bounded {
					let line_end = content[search_from..].find('\n').map(|value| search_from + value).unwrap_or(content.len());
					content[search_from..line_end].find(next_literal).map(|value| search_from + value)
				} else {
					content[search_from..].find(next_literal).map(|value| search_from + value)
				}?;
				captures.push(content[cursor..found].to_string());
				cursor = found;
				positions.push((gap_start, found));
			},
		}
	}
	// Upstream `resolveBoundary`: a boundary is the start of its next literal,
	// the end of its previous literal, or (for a zero-width selection) the next
	// literal snapped forward to the following line for whole-line inserts.
	let literal_span = |index: usize| -> Option<(usize, usize)> {
		match pattern.tokens.get(index) {
			Some(Token::Literal { .. }) => positions.get(index).copied(),
			_ => None,
		}
	};
	let preceding_literal = |boundary: usize| -> Option<usize> {
		(0..boundary.min(pattern.tokens.len()))
			.rev()
			.find(|index| matches!(pattern.tokens[*index], Token::Literal { .. }))
	};
	let following_literal = |boundary: usize| -> Option<usize> {
		(boundary.min(pattern.tokens.len())..pattern.tokens.len())
			.find(|index| matches!(pattern.tokens[*index], Token::Literal { .. }))
	};
	let resolve = |boundary: usize, kind: Boundary, line_insert: bool| -> usize {
		let previous = preceding_literal(boundary).and_then(literal_span);
		let next = following_literal(boundary).and_then(literal_span);
		let immediate_previous = boundary > 0 && matches!(pattern.tokens.get(boundary - 1), Some(Token::Literal { .. }));
		let immediate_next = matches!(pattern.tokens.get(boundary), Some(Token::Literal { .. }));
		match kind {
			Boundary::Empty => {
				if let Some((next_start, _)) = next {
					return next_start;
				}
				if let Some((_, previous_end)) = previous {
					if line_insert
						&& previous_end > 0
						&& content.as_bytes().get(previous_end - 1).copied() != Some(b'\n')
						&& let Some(offset) = content[previous_end..].find('\n')
					{
						return previous_end + offset + 1;
					}
					return previous_end;
				}
				0
			},
			Boundary::Start => {
				if immediate_next && let Some((next_start, _)) = next {
					return next_start;
				}
				if let Some((_, previous_end)) = previous {
					return previous_end;
				}
				if let Some((next_start, _)) = next {
					return next_start;
				}
				0
			},
			Boundary::End => {
				if immediate_previous && let Some((_, previous_end)) = previous {
					return previous_end;
				}
				if let Some((next_start, _)) = next {
					return next_start;
				}
				if let Some((_, previous_end)) = previous {
					return previous_end;
				}
				return 0;
			},
		}
	};
	let selection_spans: Vec<(usize, usize)> = pattern
		.selection_pairs
		.iter()
		.map(|pair| {
			let empty = pair.start == pair.end;
			let kind = if empty { Boundary::Empty } else { Boundary::Start };
			let start = resolve(pair.start, kind, pair.line_insertion);
			let end = resolve(pair.end, if empty { Boundary::Empty } else { Boundary::End }, pair.line_insertion);
			(start, end)
		})
		.collect();
	if selection_spans.iter().any(|(start, end)| start > end) {
		return None;
	}
	let match_start = preceding_literal(0)
		.and_then(literal_span)
		.map(|(start, _)| start)
		.unwrap_or(start);
	let match_end = following_literal(0)
		.and_then(|_| pattern.tokens.iter().rposition(|token| matches!(token, Token::Literal { .. })))
		.and_then(literal_span)
		.map(|(_, end)| end)
		.unwrap_or(cursor);
	let (candidate_start, candidate_end) = if pattern.selection_pairs.is_empty() {
		(match_start, match_end)
	} else {
		let start = resolve(
			pattern.selection_start,
			if pattern.insertion { Boundary::Empty } else { Boundary::Start },
			pattern.line_insertion,
		);
		let end = resolve(
			pattern.selection_end,
			if pattern.insertion { Boundary::Empty } else { Boundary::End },
			pattern.line_insertion,
		);
		(start, end)
	};
	Some(Candidate {
		start: candidate_start,
		end: candidate_end,
		match_start,
		match_end,
		captures,
		selection_spans,
	})
}

fn collect_normalized_candidates(content: &str, pattern: &ParsedPattern) -> CandidateCollection {
	let normalized = normalize_text(content);
	collect_mapped_candidates(content, &normalized, pattern, CandidateMode::Normalized, false)
}

fn collect_fuzzy_candidates(content: &str, pattern: &ParsedPattern, _path: &str) -> CandidateCollection {
	let normalized = normalize_text(content);
	collect_mapped_candidates(content, &normalized, pattern, CandidateMode::Fuzzy, false)
}

fn drop_selection_echoes(pattern: &str) -> Option<String> {
	let mut result = String::new();
	let mut run_start = 0usize;
	let mut changed = false;
	let mut index = 0usize;
	while index < pattern.len() {
		if pattern[index..].starts_with(GAP) {
			result.push_str(&pattern[run_start..index + GAP.len_utf8()]);
			index += GAP.len_utf8();
			run_start = index;
			continue;
		}
		if !pattern[index..].starts_with(SELECT_OPEN) {
			let ch = pattern[index..].chars().next()?;
			index += ch.len_utf8();
			continue;
		}
		let open_end = index + SELECT_OPEN.len_utf8();
		let close = pattern[open_end..].find(SELECT_CLOSE)? + open_end;
		let selected = &pattern[open_end..close];
		let run = &pattern[run_start..index];
		let run_normalized = normalize_text(run);
		let old_normalized = normalize_text(selected).text;
		if !old_normalized.is_empty() && run_normalized.text.ends_with(&old_normalized) {
			let cut = run_normalized.starts[run_normalized.text.len() - old_normalized.len()];
			result.push_str(&run[..cut]);
			changed = true;
		} else {
			result.push_str(run);
		}
		result.push_str(&pattern[index..close + SELECT_CLOSE.len_utf8()]);
		index = close + SELECT_CLOSE.len_utf8();
		run_start = index;
	}
	result.push_str(&pattern[run_start..]);
	changed.then_some(result)
}

fn echo_line_candidates(pattern: &str) -> Vec<String> {
	let lines: Vec<String> = pattern.split('\n').map(str::to_string).collect();
	let balanced = |line: &str| line.matches(SELECT_OPEN).count() == line.matches(SELECT_CLOSE).count();
	let mut results = Vec::new();
	for index in 1..lines.len() {
		let previous = lines[index - 1].clone();
		let line = lines[index].clone();
		if !line.contains(SELECT_OPEN) || !balanced(&line) {
			continue;
		}
		if previous.contains(SELECT_OPEN) || previous.contains(GAP) || previous.trim().is_empty() {
			continue;
		}
		let without = {
			let mut out = String::new();
			let mut from = 0usize;
			while let Some(found) = line[from..].find(SELECT_OPEN) {
				let open = from + found;
				out.push_str(&line[from..open]);
				let body = open + SELECT_OPEN.len_utf8();
				if let Some(close) = line[body..].find(SELECT_CLOSE) {
					out.push_str(&line[body..body + close]);
					from = body + close + SELECT_CLOSE.len_utf8();
				} else {
					out.push_str(&line[open..]);
					from = line.len();
				}
			}
			out.push_str(&line[from..]);
			out
		};
		let previous_normalized = normalize_text(&previous);
		let without_normalized = normalize_text(&without).text;
		if !without_normalized.is_empty() && without_normalized == previous_normalized.text {
			let mut candidate: Vec<String> = lines[..index - 1].to_vec();
			candidate.extend(lines[index..].iter().cloned());
			results.push(candidate.join("\n"));
			continue;
		}
		let trimmed = line.trim();
		let Some(inner) = trimmed.strip_prefix(SELECT_OPEN).and_then(|rest| rest.strip_suffix(SELECT_CLOSE)) else {
			continue;
		};
		if inner.contains(SELECT_OPEN) || inner.contains(SELECT_CLOSE) {
			continue;
		}
		let old = normalize_text(inner).text;
		if old.is_empty() {
			continue;
		}
		let Some(first) = previous_normalized.text.find(&old) else {
			continue;
		};
		if previous_normalized.text[first + old.len()..].contains(&old) {
			continue;
		}
		let raw_start = previous_normalized.starts[first];
		let raw_end = previous_normalized.ends[first + old.len() - 1];
		let embedded = format!("{}{}{}", &previous[..raw_start], trimmed, &previous[raw_end..]);
		let mut candidate: Vec<String> = lines[..index - 1].to_vec();
		candidate.push(embedded);
		candidate.extend(lines[index + 1..].iter().cloned());
		results.push(candidate.join("\n"));
	}
	results
}

fn trailing_selection_candidate(pattern: &str) -> Option<String> {
	let lines: Vec<String> = pattern.split('\n').map(str::to_string).collect();
	let mut changed = false;
	let mut embedded = Vec::new();
	for line in lines {
		let mut replaced = line.clone();
		if let Some(close) = line.rfind(SELECT_CLOSE) {
			let suffix = line[close + SELECT_CLOSE.len_utf8()..].trim();
			if suffix.is_empty()
				&& let Some(open) = line[..close].rfind(SELECT_OPEN)
			{
				let prefix = &line[..open];
				let old = &line[open + SELECT_OPEN.len_utf8()..close];
				if !prefix.trim().is_empty() && !prefix.contains(SELECT_OPEN) && !prefix.contains(GAP) {
					let prefix_normalized = normalize_text(prefix);
					let old_normalized = normalize_text(old).text;
					if !old_normalized.is_empty()
						&& let Some(first) = prefix_normalized.text.find(&old_normalized)
						&& !prefix_normalized.text[first + old_normalized.len()..].contains(&old_normalized)
					{
						let raw_start = prefix_normalized.starts[first];
						let raw_end = prefix_normalized.ends[first + old_normalized.len() - 1];
						replaced = format!(
							"{}{SELECT_OPEN}{}{SELECT_CLOSE}{}",
							&prefix[..raw_start],
							&prefix[raw_start..raw_end],
							&prefix[raw_end..]
						);
						changed = true;
					}
				}
			}
		}
		embedded.push(replaced);
	}
	changed.then(|| embedded.join("\n"))
}

fn gap_join_candidate(pattern: &str, inline: bool) -> Option<String> {
	if pattern.contains(GAP) {
		return None;
	}
	let mut units: Vec<String> = Vec::new();
	let mut depth = 0i32;
	let mut current = String::new();
	for ch in pattern.chars() {
		if ch == SELECT_OPEN {
			depth += 1;
		} else if ch == SELECT_CLOSE {
			depth = (depth - 1).max(0);
		}
		if ch == '\n' && depth == 0 {
			units.push(std::mem::take(&mut current));
			continue;
		}
		current.push(ch);
	}
	units.push(current);
	let lines: Vec<String> = units.into_iter().filter(|unit| !unit.trim().is_empty()).collect();
	if lines.len() < 2 {
		return None;
	}
	if inline {
		if !lines.iter().any(|line| line.contains(SELECT_OPEN)) {
			return None;
		}
	} else if !lines.iter().all(|line| line.contains(SELECT_OPEN)) {
		return None;
	}
	let insert_only = |unit: &str| unit.trim() == format!("{SELECT_OPEN}{SELECT_CLOSE}");
	let mut joined = lines[0].clone();
	for index in 1..lines.len() {
		let tight = insert_only(&lines[index - 1]) || (insert_only(&lines[index]) && index == lines.len() - 1);
		joined.push('\n');
		if !tight {
			joined.push(GAP);
			joined.push('\n');
		}
		joined.push_str(&lines[index]);
	}
	Some(joined)
}

fn punctuation_pair_variants(operation: &Operation) -> Vec<Operation> {
	let Rewrite::Inline(base_replacements) = &operation.rewrite else {
		return Vec::new();
	};
	let pattern = &operation.pattern_text;
	let mut variants = Vec::new();
	let mut index = 0usize;
	let mut selection_index = 0usize;
	while let Some(found) = pattern[index..].find(SELECT_OPEN) {
		let open = index + found;
		let Some(close_rel) = pattern[open + SELECT_OPEN.len_utf8()..].find(SELECT_CLOSE) else {
			break;
		};
		let close = open + SELECT_OPEN.len_utf8() + close_rel;
		let old = &pattern[open + SELECT_OPEN.len_utf8()..close];
		let next = base_replacements.get(selection_index).cloned().unwrap_or_default();
		selection_index += 1;
		index = close + SELECT_CLOSE.len_utf8();
		let punct_only = |text: &str| {
			!text.is_empty()
				&& text
					.chars()
					.all(|ch| !ch.is_alphanumeric() && ch != '_' && ch != '$' && !ch.is_whitespace())
		};
		if !punct_only(old) || !punct_only(&next) {
			continue;
		}
		let mut chars: Vec<char> = old.chars().chain(next.chars()).collect();
		chars.sort_unstable();
		chars.dedup();
		if chars.len() != 2 {
			continue;
		}
		for length in [2usize, 1, 3] {
			for (a, b) in [(chars[0], chars[1]), (chars[1], chars[0])] {
				let old_run: String = std::iter::repeat(a).take(length).collect();
				let next_run: String = std::iter::repeat(b).take(length).collect();
				if old_run == old && next_run == next {
					continue;
				}
				let mut variant = operation.clone();
				variant.pattern_text = format!(
					"{}{SELECT_OPEN}{old_run}{SELECT_CLOSE}{}",
					&pattern[..open],
					&pattern[close + SELECT_CLOSE.len_utf8()..]
				);
				let mut replacements = base_replacements.clone();
				if selection_index - 1 < replacements.len() {
					replacements[selection_index - 1] = next_run;
				}
				variant.rewrite = Rewrite::Inline(replacements);
				variant.recovery_note = Some(format!(
					"Note: operation {selection_index}'s punctuation selection was garbled by its own marker glyphs and was resolved against the file."
				));
				variants.push(variant);
			}
		}
	}
	variants
}

/// Trim boundary text double-typed on both a selection and its adjacent
/// literal (`crypto.⟪.? .get⟫` after `crypto.` → `crypto.⟪? .get⟫`): the
/// duplicated characters demand the shared text twice and never match. The
/// literal stays authoritative so the stated final text survives.
fn overlap_trim_candidates(pattern_text: &str) -> Vec<String> {
	let mut results: Vec<String> = Vec::new();
	let mut index = 0usize;
	while index < pattern_text.len() {
		if pattern_text[index..].starts_with(GAP) {
			index += GAP.len_utf8();
			continue;
		}
		let Some(character) = pattern_text[index..].chars().next() else {
			break;
		};
		if character != SELECT_OPEN {
			index += character.len_utf8();
			continue;
		}
		let body_start = index + character.len_utf8();
		let Some(close_rel) = pattern_text[body_start..].find(SELECT_CLOSE) else {
			return results;
		};
		let close = body_start + close_rel;
		let old = &pattern_text[body_start..close];
		let old_normalized = normalize_text(old);
		if old_normalized.text.is_empty() {
			index = close + SELECT_CLOSE.len_utf8();
			continue;
		}
		let line_start = pattern_text[..index].rfind('\n').map(|value| value + 1).unwrap_or(0);
		let previous_boundary = line_start
			.max(
				pattern_text[..index]
					.rfind(SELECT_CLOSE)
					.map(|value| value + SELECT_CLOSE.len_utf8())
					.unwrap_or(0),
			)
			.max(
				pattern_text[..index]
					.rfind(GAP)
					.map(|value| value + GAP.len_utf8())
					.unwrap_or(0),
			);
		let previous_normalized = normalize_text(&pattern_text[previous_boundary..index]).text;
		let line_end = pattern_text[close + SELECT_CLOSE.len_utf8()..]
			.find('\n')
			.map(|value| close + SELECT_CLOSE.len_utf8() + value)
			.unwrap_or(pattern_text.len());
		let next_open = pattern_text[close + SELECT_CLOSE.len_utf8()..]
			.find(SELECT_OPEN)
			.map(|value| close + SELECT_CLOSE.len_utf8() + value)
			.unwrap_or(pattern_text.len());
		let next_gap = pattern_text[close + SELECT_CLOSE.len_utf8()..]
			.find(GAP)
			.map(|value| close + SELECT_CLOSE.len_utf8() + value)
			.unwrap_or(pattern_text.len());
		let next_boundary = line_end.min(next_open).min(next_gap);
		let next_normalized = normalize_text(&pattern_text[close + SELECT_CLOSE.len_utf8()..next_boundary]).text;
		let old_len = old_normalized.text.len();
		let previous_len = previous_normalized.len();
		let mut overlap = (old_len - 1).min(previous_len);
		while overlap >= 1 {
			if old_normalized.text.as_bytes()[..overlap] == previous_normalized.as_bytes()[previous_len - overlap..] {
				let raw_start = old_normalized.starts.get(overlap).copied().unwrap_or(old.len());
				results.push(format!(
					"{}{}{}",
					&pattern_text[..body_start],
					&old[raw_start..],
					&pattern_text[close..]
				));
				break;
			}
			overlap -= 1;
		}
		let mut overlap = (old_len - 1).min(next_normalized.len());
		while overlap >= 1 {
			if old_normalized.text.as_bytes()[old_len - overlap..] == next_normalized.as_bytes()[..overlap] {
				let raw_end = old_normalized.starts.get(old_len - overlap).copied().unwrap_or(old.len());
				results.push(format!(
					"{}{}{}",
					&pattern_text[..body_start],
					&old[..raw_end],
					&pattern_text[close..]
				));
				break;
			}
			overlap -= 1;
		}
		index = close + SELECT_CLOSE.len_utf8();
	}
	results
}

fn recover_pattern_candidates(pattern: &str, inline: bool) -> Vec<String> {
	let mut candidates: Vec<String> = Vec::new();
	let push = |candidate: Option<String>, list: &mut Vec<String>| {
		if let Some(candidate) = candidate
			&& candidate != pattern
			&& !list.contains(&candidate)
		{
			list.push(candidate);
		}
	};
	push(drop_selection_echoes(pattern), &mut candidates);
	for candidate in echo_line_candidates(pattern) {
		push(Some(candidate), &mut candidates);
	}
	let trailing = trailing_selection_candidate(pattern);
	push(trailing.clone(), &mut candidates);
	if let Some(found) = pattern.find(SELECT_CLOSE) {
		let after = found + SELECT_CLOSE.len_utf8();
		if let Some(ch) = pattern[after..].chars().next()
			&& !ch.is_whitespace()
			&& ch != SELECT_OPEN
			&& ch != SELECT_CLOSE
			&& ch != SELECT_DIVIDER
		{
			push(Some(format!("{}{GAP}{}", &pattern[..after], &pattern[after..])), &mut candidates);
		}
	}
	for candidate in overlap_trim_candidates(pattern) {
		push(Some(candidate), &mut candidates);
	}
	push(gap_join_candidate(pattern, inline), &mut candidates);
	if let Some(trailing) = trailing {
		push(gap_join_candidate(&trailing, inline), &mut candidates);
	}
	candidates
}

/// Locate an authored pattern's lines in order even when the payload omitted
/// the `…` gaps between non-adjacent lines. Returns the gap-joined pattern.
fn non_consecutive_pattern(content: &str, pattern_text: &str) -> Option<String> {
	if pattern_text.contains(SELECT_OPEN) || pattern_text.contains(SELECT_CLOSE) || pattern_text.contains(GAP) {
		return None;
	}
	let authored: Vec<&str> = pattern_text.split('\n').filter(|line| !line.trim().is_empty()).collect();
	if authored.len() < 2 {
		return None;
	}
	let file_lines: Vec<&str> = content.split('\n').collect();
	let mut locations: Vec<usize> = Vec::new();
	let mut from = 0usize;
	for authored_line in &authored {
		let needle = normalize_text(authored_line).text;
		let mut found = None;
		for index in from..file_lines.len() {
			if normalize_text(file_lines[index]).text == needle {
				found = Some(index);
				break;
			}
		}
		locations.push(found?);
		from = found? + 1;
	}
	if locations.windows(2).all(|pair| pair[1] == pair[0] + 1) {
		return None;
	}
	Some(authored.join(&format!("{GAP}\n")))
}

/// Preserve skipped source rows when explicit MATCH/REWRITE lines correspond
/// positionally but the MATCH omitted `…` between them.
fn recover_non_consecutive_operation(content: &str, operation: &Operation) -> Option<Operation> {
	let Rewrite::Explicit(text) = &operation.rewrite else {
		return None;
	};
	let corrected = non_consecutive_pattern(content, &operation.pattern_text)?;
	let pattern_lines = operation.pattern_text.split('\n').filter(|line| !line.trim().is_empty()).count();
	let mut rewrite_lines: Vec<&str> = text.split('\n').collect();
	while rewrite_lines.first().is_some_and(|line| line.trim().is_empty()) {
		rewrite_lines.remove(0);
	}
	while rewrite_lines.last().is_some_and(|line| line.trim().is_empty()) {
		rewrite_lines.pop();
	}
	if rewrite_lines.len() != pattern_lines || rewrite_lines.iter().any(|line| line.trim().is_empty()) {
		return None;
	}
	let rewrite = rewrite_lines.join(&format!("{GAP}\n"));
	Some(Operation {
		pattern_text: corrected,
		rewrite: Rewrite::Explicit(rewrite),
		..operation.clone()
	})
}

fn locate_with_echo_recovery(
	content: &str,
	operation: &Operation,
	number: usize,
	path: &str,
	exclusions: Option<&[(usize, usize)]>,
) -> Result<(Operation, ParsedPattern, Vec<Candidate>), String> {
	let first_pattern = parse_pattern(&operation.pattern_text, number)?;
	match locate(content, &first_pattern, operation, number, path, exclusions) {
		Ok(candidates) => return Ok((operation.clone(), first_pattern, candidates)),
		Err(original) => {
			if let Some(retry) = recover_non_consecutive_operation(content, operation)
				&& let Ok(pattern) = parse_pattern(&retry.pattern_text, number)
				&& let Ok(candidates) = locate(content, &pattern, &retry, number, path, exclusions)
			{
				return Ok((retry, pattern, candidates));
			}
			for candidate in recover_pattern_candidates(&operation.pattern_text, matches!(&operation.rewrite, Rewrite::Inline(_))) {
				if let Ok(pattern) = parse_pattern(&candidate, number) {
					let mut retry = operation.clone();
					retry.pattern_text = candidate;
					if let Ok(candidates) = locate(content, &pattern, &retry, number, path, exclusions) {
						return Ok((retry, pattern, candidates));
					}
				}
			}
			for variant in punctuation_pair_variants(operation) {
				if let Ok(pattern) = parse_pattern(&variant.pattern_text, number)
					&& let Ok(candidates) = locate(content, &pattern, &variant, number, path, exclusions)
				{
					return Ok((variant, pattern, candidates));
				}
			}
			Err(original)
		},
	}
}

fn has_add_lines(text: &str) -> bool {
	text.split('\n')
		.any(|line| line.trim_start_matches([' ', '\t']).starts_with(ADD_LINE))
}

fn ceil_char_boundary(text: &str, mut index: usize) -> usize {
	if index >= text.len() {
		return text.len();
	}
	while index < text.len() && !text.is_char_boundary(index) {
		index += 1;
	}
	index
}

fn line_start_at_or_before(content: &str, offset: usize) -> usize {
	let end = ceil_char_boundary(content, offset.saturating_add(1).min(content.len()));
	content[..end].rfind('\n').map(|value| value + 1).unwrap_or(0)
}

fn numbered_preview(content: &str, offset: usize) -> String {
	let lines: Vec<&str> = content.split('\n').collect();
	let anchor = text::line_number_at(content, offset.min(content.len())).saturating_sub(1);
	let mut start = anchor.saturating_sub(4);
	if lines.len().saturating_sub(start) < 10 {
		start = lines.len().saturating_sub(10);
	}
	lines[start..(start + 10).min(lines.len())]
		.iter()
		.enumerate()
		.map(|(index, line)| format!("{}: {line}", start + index + 1))
		.collect::<Vec<_>>()
		.join("\n")
}

fn display_fragment(text: &str) -> String {
	if text.contains('\n') && text.split('\n').count() <= 8 {
		return format!("\n{text}");
	}
	let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
	let compact = if compact.chars().count() > 80 {
		format!("{}…", compact.chars().take(77).collect::<String>())
	} else {
		compact
	};
	format!("{compact:?}")
}

fn literal_occurrences(normalized: &Normalized, token: &str) -> Vec<(usize, usize)> {
	exact_occurrences(&normalized.text, token)
		.into_iter()
		.map(|start| (start, start + token.len()))
		.collect()
}

fn pair_can_align(
	content: &str,
	normalized: &Normalized,
	pattern: &ParsedPattern,
	left_index: usize,
	right_index: usize,
	left: &[(usize, usize)],
	right: &[(usize, usize)],
) -> bool {
	let gaps: Vec<&Token> = pattern.tokens[left_index + 1..right_index]
		.iter()
		.filter(|token| matches!(*token, Token::Gap { .. }))
		.collect();
	left.iter().any(|(_, left_end)| {
		right.iter().any(|(right_start, _)| {
			if gaps.is_empty() && right_start != left_end {
				return false;
			}
			if !gaps.is_empty() && right_start < left_end {
				return false;
			}
			if gaps.iter().any(|token| matches!(*token, Token::Gap { line_bounded: true, .. })) {
				let gap_start = normalized.end(*left_end, content.len());
				let gap_end = normalized.start(*right_start, content.len());
				if content[gap_start.min(gap_end)..gap_end.max(gap_start)].contains('\n') {
					return false;
				}
			}
			true
		})
	})
}

/// Closest line to a missing literal; the best window inside it when the
/// literal is short enough for a sliding comparison.
fn closest_fragment(content: &str, needle: &str, center_offset: Option<usize>) -> (String, usize) {
	let center_line = center_offset.map(|offset| text::line_number_at(content, offset).saturating_sub(1));
	let needle_chars: Vec<char> = needle.chars().collect();
	let mut ranked: Vec<(String, usize, Vec<char>, f64)> = Vec::new();
	let mut offset = 0usize;
	for (line_index, line) in content.split('\n').enumerate() {
		let normalized: Vec<char> = line.chars().filter(|ch| !ch.is_whitespace()).collect();
		if !normalized.is_empty()
			&& center_line.map_or(true, |center| line_index.abs_diff(center) <= 12)
		{
			let score = text::levenshtein_distance(needle, &normalized.iter().collect::<String>()) as f64
				/ (1.max(needle_chars.len()).max(normalized.len()) as f64);
			ranked.push((line.to_string(), offset, normalized, score));
			ranked.sort_by(|left, right| left.3.total_cmp(&right.3));
			ranked.truncate(3);
		}
		offset += line.len() + 1;
	}
	let Some(first) = ranked.first() else {
		if center_offset.is_some() {
			return closest_fragment(content, needle, None);
		}
		return (needle.to_string(), 0);
	};
	let mut best = (first.0.clone(), first.1, first.3);
	if needle_chars.len() <= 160 {
		for (line, line_offset, normalized, _) in &ranked {
			let width = needle_chars.len().min(normalized.len());
			for start in 0..=normalized.len().saturating_sub(width) {
				let candidate: String = normalized[start..start + width].iter().collect();
				let score = text::levenshtein_distance(needle, &candidate) as f64
					/ 1.max(needle_chars.len()).max(candidate.chars().count()) as f64;
				if score >= best.2 {
					continue;
				}
				let raw_start = normalized_byte_for_char(line, start);
				let raw_end = normalized_byte_for_char(line, start + width);
				best = (line[raw_start..raw_end].to_string(), *line_offset + raw_start, score);
			}
		}
	}
	(best.0, best.1)
}

/// Byte offset of the `char_index`-th non-whitespace character in `line`.
fn normalized_byte_for_char(line: &str, char_index: usize) -> usize {
	let mut seen = 0usize;
	for (offset, ch) in line.char_indices() {
		if ch.is_whitespace() {
			continue;
		}
		if seen == char_index {
			return offset;
		}
		seen += 1;
	}
	line.len()
}

fn no_match_guidance(
	content: &str,
	normalized: &Normalized,
	pattern: &ParsedPattern,
	operation: &Operation,
) -> (String, usize, String, Option<String>) {
	let literals: Vec<(usize, &str, &str, Vec<(usize, usize)>)> = pattern
		.tokens
		.iter()
		.enumerate()
		.filter_map(|(index, token)| match token {
			Token::Literal { text, normalized: token_normalized } => Some((
				index,
				text.as_str(),
				token_normalized.as_str(),
				literal_occurrences(normalized, token_normalized),
			)),
			_ => None,
		})
		.collect();
	let missing = literals.iter().find(|(_, _, _, occurrences)| occurrences.is_empty());
	if let Some((_, text, normalized_token, _)) = missing {
		let text = *text;
		let normalized_token = *normalized_token;
		let anchor = literals
			.iter()
			.filter(|(_, _, _, occurrences)| !occurrences.is_empty())
			.min_by(|left, right| {
				right.2.len().cmp(&left.2.len()).then(left.3.len().cmp(&right.3.len()))
			});
		let anchor_offset = anchor
			.and_then(|entry| entry.3.first())
			.map(|(start, _)| normalized.start(*start, 0));
		let closest = closest_fragment(content, normalized_token, anchor_offset);
		let corrected_pattern = operation.pattern_text.replacen(text, &closest.0, 1);
		let line_start = content[..closest.1].rfind('\n').map(|value| value + 1).unwrap_or(0);
		let line_end = content[closest.1..].find('\n').map(|value| closest.1 + value).unwrap_or(content.len());
		let neighbor_line = &content[line_start..line_end];
		let explicit = match &operation.rewrite {
			Rewrite::Explicit(text) => Some(text.as_str()),
			Rewrite::Inline(_) => None,
		};
		let looks_like_addition = explicit.is_some_and(|rewrite| {
			!normalized.text.contains(normalized_token)
				&& (rewrite.is_empty() || rewrite.contains(text))
		});
		let addition_text = explicit.map(|rewrite| if rewrite.is_empty() { text } else { rewrite });
		let addition_retry = (looks_like_addition && !neighbor_line.trim().is_empty())
			.then(|| {
				format!(
					"If you are ADDING this text: match the existing neighbor line it belongs next to, and put the new text in the REWRITE —\n{OPENER}\n{SELECT_OPEN}{SELECT_CLOSE}{neighbor_line}\n{REWRITE_HEADER}\n{}",
					addition_text.unwrap_or_default()
				)
			});
		let anchor_note = anchor
			.map(|entry| format!(" It broke relative to matched anchor {}.", display_fragment(entry.1)))
			.unwrap_or_default();
		return (
			format!("Failed fragment: {} has 0 occurrences.{anchor_note}", display_fragment(text)),
			anchor_offset.unwrap_or(closest.1),
			corrected_pattern,
			addition_retry,
		);
	}
	let mut broken: Option<(&str, &str)> = None;
	let mut reachable: Vec<(usize, usize)> = literals.first().map(|entry| entry.3.clone()).unwrap_or_default();
	for pair in literals.windows(2) {
		let left = &pair[0];
		let right = &pair[1];
		let next: Vec<(usize, usize)> = right
			.3
			.iter()
			.copied()
			.filter(|occurrence| {
				pair_can_align(content, normalized, pattern, left.0, right.0, &reachable, &[*occurrence])
			})
			.collect();
		if next.is_empty() {
			broken = Some((left.1, right.1));
			break;
		}
		reachable = next;
	}
	if broken.is_none() && literals.len() >= 2 {
		broken = Some((literals[literals.len() - 2].1, literals[literals.len() - 1].1));
	}
	let Some((left, right)) = broken else {
		let only = literals.first();
		return (
			format!(
				"Failed fragment: {} could not align.",
				display_fragment(only.map(|entry| entry.1).unwrap_or(operation.pattern_text.as_str()))
			),
			only.and_then(|entry| entry.3.first()).map(|(start, _)| normalized.start(*start, 0)).unwrap_or(0),
			operation.pattern_text.clone(),
			None,
		);
	};
	let left_index = literals.iter().position(|entry| entry.1 == left).unwrap_or(0);
	let right_index = literals
		.iter()
		.enumerate()
		.skip(left_index + 1)
		.find(|(_, entry)| entry.1 == right)
		.map(|(index, _)| index)
		.unwrap_or(left_index + 1);
	let between_has_gap = pattern.tokens[left_index + 1..right_index]
		.iter()
		.any(|token| matches!(token, Token::Gap { .. }));
	let right_at = operation.pattern_text.find(right).filter(|value| *value > 0);
	let corrected_pattern = if between_has_gap {
		operation.pattern_text.clone()
	} else {
		right_at.map_or_else(
			|| operation.pattern_text.clone(),
			|at| format!("{}{}{}", &operation.pattern_text[..at], GAP, &operation.pattern_text[at..]),
		)
	};
	let preview = literals
		.get(left_index)
		.and_then(|entry| entry.3.first())
		.map(|(start, _)| normalized.start(*start, 0))
		.unwrap_or(0);
	(
		format!(
			"Ordered pair broke: {} did not precede {} as written.",
			display_fragment(left),
			display_fragment(right)
		),
		preview,
		corrected_pattern,
		None,
	)
}

fn distinguishing_context(
	content: &str,
	candidate: &Candidate,
	all: &[Candidate],
) -> Option<(&'static str, String)> {
	let others: Vec<&Candidate> = all.iter().filter(|entry| entry.start != candidate.start).collect();
	let usable = |line: &str| line.chars().count() >= 3 && line.chars().any(|ch| ch.is_alphanumeric() || ch == '_' || ch == '$');
	let before_lines = |offset: usize| -> Vec<String> {
		content[..offset].split('\n').map(|line| line.trim().to_string()).collect()
	};
	let before = before_lines(candidate.start);
	let others_before: Vec<Vec<String>> = others.iter().map(|entry| before_lines(entry.start)).collect();
	for back in 2..=12.min(before.len()) {
		let line = &before[before.len() - back];
		if !usable(line) {
			continue;
		}
		if others_before.iter().all(|lines| !lines.contains(line)) {
			return Some(("before", line.clone()));
		}
	}
	let after_lines = |offset: usize| -> Vec<String> {
		content[offset..].split('\n').map(|line| line.trim().to_string()).collect()
	};
	let after = after_lines(candidate.end);
	let others_after: Vec<Vec<String>> = others.iter().map(|entry| after_lines(entry.end)).collect();
	for (forward, line) in after.iter().enumerate().take(12) {
		if !usable(line) {
			continue;
		}
		if others_after.iter().all(|lines| !lines.contains(line)) {
			return Some(("after", line.clone()));
		}
	}
	None
}

fn selected_capture_indices(pattern: &ParsedPattern) -> Vec<usize> {
	pattern
		.tokens
		.get(pattern.selection_start..pattern.selection_end)
		.unwrap_or_default()
		.iter()
		.filter_map(|token| match token {
			Token::Gap { capture_index, .. } => Some(*capture_index),
			_ => None,
		})
		.collect()
}

fn rewrite_gap_count(rewrite: &str) -> usize {
	rewrite.chars().filter(|ch| *ch == GAP).count()
}

fn captures_are_identical(indices: &[usize], candidates: &[Candidate]) -> bool {
	let Some(first) = candidates.first() else {
		return true;
	};
	indices.iter().all(|index| {
		let expected = first.captures.get(*index);
		candidates.iter().all(|candidate| candidate.captures.get(*index) == expected)
	})
}

fn rewrite_is_identical_for_all(pattern: &ParsedPattern, operation: &Operation, candidates: &[Candidate]) -> bool {
	match &operation.rewrite {
		Rewrite::Explicit(text) => {
			let selected = selected_capture_indices(pattern);
			let count = rewrite_gap_count(text).min(selected.len());
			captures_are_identical(&selected[..count], candidates)
		},
		Rewrite::Inline(replacements) => replacements.iter().enumerate().all(|(index, replacement)| {
			let Some(selection) = pattern.selection_pairs.get(index) else {
				return false;
			};
			let indices: Vec<usize> = pattern
				.tokens
				.get(selection.start..selection.end)
				.unwrap_or_default()
				.iter()
				.filter_map(|token| match token {
					Token::Gap { capture_index, .. } => Some(*capture_index),
					_ => None,
				})
				.collect();
			let count = rewrite_gap_count(replacement).min(indices.len());
			captures_are_identical(&indices[..count], candidates)
		}),
	}
}

fn locate(
	content: &str,
	pattern: &ParsedPattern,
	operation: &Operation,
	number: usize,
	path: &str,
	exclusions: Option<&[(usize, usize)]>,
) -> Result<Vec<Candidate>, String> {
	let normalized = normalize_text(content);
	let mut candidates = collect_raw_candidates(content, pattern);
	if candidates.len() > MAX_CANDIDATES {
		return Err(format!("Operation {number} pattern is too broad; add another distinctive {GAP} fragment."));
	}
	if candidates.is_empty() && has_add_lines(&operation.source_pattern_text) {
		return Err(format!(
			"Operation {number} adds whole lines but MATCH did not match byte-for-byte. Re-read the region and copy its exact indentation."
		));
	}
	if candidates.is_empty()
		&& let Some(fallback) = &pattern.literal_fallback
	{
		let exact = exact_occurrences(&normalized.text, &fallback.normalized);
		if !exact.is_empty() && (operation.all || exact.len() == 1) {
			let fallback_candidates: Vec<Candidate> = exact
				.into_iter()
				.map(|occurrence| {
					let match_start = normalized.start(occurrence, 0);
					let match_end = normalized.end(occurrence + fallback.normalized.len(), content.len());
					let fallback_start = occurrence + fallback.selection_start;
					let fallback_end = occurrence + fallback.selection_end;
					let start = if fallback.selection_start == fallback.normalized.len() {
						match_end
					} else {
						normalized.start(fallback_start, match_end)
					};
					let end = if fallback.selection_end == fallback.normalized.len() {
						match_end
					} else if fallback.insertion {
						normalized.start(fallback_end, match_end)
					} else {
						normalized.end(fallback_end, match_end)
					};
					Candidate {
						start,
						end,
						match_start,
						match_end,
						captures: Vec::new(),
						selection_spans: if pattern.selection_pairs.len() == 1 {
							vec![(start, end)]
						} else {
							Vec::new()
						},
					}
				})
				.collect();
			return Ok(if operation.all { fallback_candidates } else { vec![fallback_candidates[0].clone()] });
		}
	}
	if candidates.is_empty() {
		let normalized_result = collect_mapped_candidates(content, &normalized, pattern, CandidateMode::Normalized, false);
		if normalized_result.overflow {
			return Err(format!("Operation {number} pattern is too broad; add another distinctive {GAP} fragment."));
		}
		candidates = normalized_result.candidates;
		if candidates.is_empty() {
			let fuzzy = collect_mapped_candidates(content, &normalized, pattern, CandidateMode::Fuzzy, false);
			if fuzzy.overflow {
				return Err(format!("Operation {number} pattern is too broad; add another distinctive {GAP} fragment."));
			}
			candidates = fuzzy.candidates;
			if candidates.is_empty() && !operation.all {
				let tolerant = collect_mapped_candidates(content, &normalized, pattern, CandidateMode::Fuzzy, true);
				if !tolerant.overflow && tolerant.candidates.len() == 1 {
					candidates = tolerant.candidates;
				}
			}
		}
	}
	if candidates.len() > MAX_CANDIDATES {
		return Err(format!("Operation {number} pattern is too broad; add another distinctive {GAP} fragment."));
	}
	if !candidates.is_empty() && matches!(&operation.rewrite, Rewrite::Inline(_)) {
		candidates.retain(|candidate| {
			!candidate.selection_spans.iter().any(|(start, end)| {
				if start >= end {
					return false;
				}
				let text = &content[*start..*end];
				if !text
					.chars()
					.all(|ch| !ch.is_alphanumeric() && !matches!(ch, '_' | '$') && !ch.is_whitespace())
					|| text.is_empty()
				{
					return false;
				}
				let character = text.chars().next().unwrap_or_default();
				if !text.chars().all(|ch| ch == character) {
					return false;
				}
				content[..*start].chars().next_back() == Some(character) || content[*end..].chars().next() == Some(character)
			})
		});
	}
	if let Some(claims) = exclusions
		&& candidates.len() > 1
	{
		let free: Vec<Candidate> = candidates
			.iter()
			.filter(|candidate| {
				!claims
					.iter()
					.any(|(start, end)| candidate.match_start < *end && *start < candidate.match_end)
			})
			.cloned()
			.collect();
		if !free.is_empty() {
			candidates = free;
		}
	}
	if operation.all && !candidates.is_empty() {
		return Ok(candidates);
	}
	if candidates.len() == 1 {
		return Ok(candidates);
	}
	if candidates.is_empty() {
		let (reason, offset, corrected, addition) = no_match_guidance(content, &normalized, pattern, operation);
		let mut message = format!(
			"Operation {number} {} {path}. {reason}\nCurrent file content near the closest match (no re-read needed):\n{}\nCopy-ready corrected operation:\n{}",
			if operation.all {
				format!("{OPENER}* found 0 matches in")
			} else {
				"did not match".to_string()
			},
			numbered_preview(content, offset),
			operation_payload_with(operation, if operation.all { "*" } else { "" }, &corrected),
		);
		if let Some(addition) = addition {
			message.push('\n');
			message.push_str(&addition);
		}
		return Err(message);
	}
	if candidates.len() <= 4 && !operation.desired_state {
		let outcomes: Vec<String> = candidates
			.iter()
			.map(|candidate| match &operation.rewrite {
				Rewrite::Explicit(text) => {
					format!("{}{}{}", &content[..candidate.start], text, &content[candidate.end..])
				},
				Rewrite::Inline(replacements) => {
					let mut result = content.to_string();
					let mut substitutions: Vec<((usize, usize), &str)> = candidate
						.selection_spans
						.iter()
						.copied()
						.zip(replacements.iter().map(String::as_str))
						.collect();
					substitutions.sort_by_key(|(span, _)| std::cmp::Reverse(span.0));
					for ((start, end), text) in substitutions {
						result.replace_range(start..end, text);
					}
					result
				},
			})
			.collect();
		let normalized_outcomes: std::collections::BTreeSet<String> =
			outcomes.iter().map(|outcome| normalize_text(outcome).text).collect();
		if normalized_outcomes.len() == 1 {
			return Ok(vec![candidates[0].clone()]);
		}
	}
	let retries: Vec<String> = candidates
		.iter()
		.take(2)
		.map(|candidate| {
			let line = text::line_number_at(content, candidate.start);
			let distinguishing = distinguishing_context(content, candidate, &candidates);
			let retry_pattern = match distinguishing {
				None => operation.pattern_text.clone(),
				Some(("before", line)) => format!("{line}{GAP}\n{}", operation.pattern_text),
				Some((_, line)) => format!("{}\n{GAP}\n{line}", operation.pattern_text),
			};
			format!("Near line {line}:\n{}", operation_payload_with(operation, "", &retry_pattern))
		})
		.collect();
	let all_retry = if rewrite_is_identical_for_all(pattern, operation, &candidates) {
		format!("All candidates receive the same rewrite; retry every match:\n{}\n\n", operation_payload_with(operation, "*", &operation.pattern_text))
	} else {
		String::new()
	};
	Err(format!(
		"Operation {number} is ambiguous: {} ordered tuples match.\n\n{all_retry}Add context that only the intended match has — one of these:\n\n{}",
		candidates.len(),
		retries.join("\n\n")
	))
}


fn verify_explicit_match(
	content: &str,
	pattern: &ParsedPattern,
	path: &str,
	number: usize,
	all: bool,
) -> Result<(), String> {
	let operation = Operation {
		pattern_text: String::new(),
		source_pattern_text: String::new(),
		rewrite: Rewrite::Explicit(String::new()),
		all,
		assumed_deletion: false,
		desired_state: false,
		recovery_note: None,
	};
	let _ = locate(content, pattern, &operation, number, path, None)?;
	Ok(())
}

// ── Applying ──────────────────────────────────────────────────────────────

pub fn apply_operations(
	content: &str,
	input: &str,
	path: &str,
	no_op_key: &str,
	notes: &mut Vec<String>,
) -> Result<String, String> {
	let hash = payload_hash(input);
	reset_no_op_hash(no_op_key, hash);
	let operations = parse_operations(input, content, path)?;
	let mut removed_by_operation: Vec<Option<String>> = vec![None; operations.len()];
	let mut planned: Vec<PlannedEdit> = Vec::new();
	let mut deletion_notes: HashMap<usize, String> = HashMap::new();
	let mut recovery_notes: Vec<String> = Vec::new();
	let mut last_match_offset = 0usize;
	// Ambiguous ops defer once: after every sibling plans its edit, their spans
	// exclude already-claimed candidates and a genuinely unique match survives.
	let mut queue: Vec<usize> = (0..operations.len()).collect();
	let mut deferred_ambiguous: std::collections::HashSet<usize> = std::collections::HashSet::new();
	let mut cursor = 0usize;
	while cursor < queue.len() {
		let index = queue[cursor];
		cursor += 1;
		let number = index + 1;
		if let Some(note) = &operations[index].recovery_note {
			recovery_notes.push(note.clone());
		}
		let claims: Option<Vec<(usize, usize)>> = deferred_ambiguous
			.contains(&index)
			.then(|| planned.iter().map(|edit| (edit.start, edit.end)).collect());
		let located = match locate_with_echo_recovery(content, &operations[index], number, path, claims.as_deref()) {
			Ok(located) => located,
			Err(error) => {
				if !deferred_ambiguous.contains(&index) && error.contains(" is ambiguous: ") {
					deferred_ambiguous.insert(index);
					queue.push(index);
					continue;
				}
				return Err(error);
			},
		};
		let (operation, pattern, candidates) = located;
		let mut ordered = candidates.clone();
		if operation.all {
			ordered.sort_by_key(|candidate| std::cmp::Reverse(candidate.start));
		}
		let mut changes = 0usize;
		match &operation.rewrite {
			Rewrite::Inline(replacements) => {
				if pattern.selection_pairs.len() != replacements.len() {
					return Err(format!("Operation {number} inline replacements do not align with its selections."));
				}
				let replacements: Vec<String> = replacements
					.iter()
					.map(|rewrite| resolve_rewrite_references(rewrite, &removed_by_operation))
					.collect::<Result<Vec<_>, String>>()?;
				let mut deleted_text: Option<String> = None;
				for located in &ordered {
					let mut selections: Vec<(usize, (usize, usize), SelectionPair, String)> = Vec::new();
					for (selection_index, span) in located.selection_spans.iter().enumerate() {
						let (Some(selection), Some(rewrite)) =
							(pattern.selection_pairs.get(selection_index), replacements.get(selection_index))
						else {
							return Err(format!(
								"Operation {number} inline replacements do not align with its selections."
							));
						};
						selections.push((selection_index, *span, *selection, rewrite.clone()));
					}
					selections.sort_by_key(|(_, span, _, _)| std::cmp::Reverse(span.0));
					for (selection_index, span, selection, rewrite) in selections {
						let captures = selection_captures(&pattern, located, selection_index);
						let prepared = prepare_inline_selection_edit(content, located, span, selection, &rewrite, &captures, number)?;
						if prepared.replacement == content[prepared.candidate.start..prepared.candidate.end] {
							continue;
						}
						if prepared.deleted_text.is_some() && candidates.len() == 1 && pattern.selection_pairs.len() == 1 {
							deleted_text = prepared.deleted_text.clone();
						}
						// An insert whose every line already exists nearby is usually a
						// diff-habit replacement (context + added lines, old lines assumed
						// gone). Apply it, but say so — the model self-corrects from notes.
						if prepared.candidate.start == prepared.candidate.end && !prepared.replacement.trim().is_empty() {
							let nearby_start = line_start_at_or_before(
								content,
								prepared.candidate.start.saturating_sub(1000),
							);
							let probe = ceil_char_boundary(
								content,
								(prepared.candidate.start + 1000).min(content.len()),
							);
							let nearby_end = content[probe..]
								.find('\n')
								.map(|value| probe + value)
								.unwrap_or(content.len());
							let nearby: std::collections::HashSet<&str> = content[nearby_start..nearby_end]
								.split('\n')
								.map(str::trim)
								.collect();
							let inserted_lines: Vec<&str> = prepared
								.replacement
								.split('\n')
								.filter(|entry| !entry.trim().is_empty())
								.collect();
							if !inserted_lines.is_empty() && inserted_lines.iter().all(|line| nearby.contains(line.trim())) {
								recovery_notes.push(format!(
									"Note: operation {number} inserted {} line(s) that duplicate adjacent code. If you meant to replace or reorder the originals, mark them: ⟪old│new⟫ replaces, ⟪old│⟫ deletes.",
									inserted_lines.len()
								));
							}
						}
						planned.push(PlannedEdit {
							start: prepared.candidate.start,
							end: prepared.candidate.end,
							replacement: prepared.replacement,
							operation_number: number,
						});
						changes += 1;
					}
					last_match_offset = located.match_start;
				}
				if let Some(text) = deleted_text {
					removed_by_operation[index] = Some(text);
				}
				if changes == 0 {
					let hint = inline_would_change_hint(content, &candidates[0], &pattern, &replacements, number, path).unwrap_or_else(|| {
						format!(
							"The desired side equals the current text — identical {SELECT_OPEN}current{SELECT_DIVIDER}desired{SELECT_CLOSE} sides never change the file. Restate the selection with the actual change after {SELECT_DIVIDER}; do not drop the operation."
						)
					});
					return Err(throw_no_op(
						content,
						path,
						Some(number),
						operation.all.then_some(candidates.len()),
						candidates[0].match_start,
						Some(hint),
						no_op_key,
						hash,
					));
				}
			},
			Rewrite::Explicit(explicit) => {
				let lone_reference = register_number(explicit.trim());
				let resolved = resolve_rewrite_references(explicit, &removed_by_operation)?;
				let base = if resolved.trim().is_empty() {
					if pattern.insertion { "\n".to_string() } else { String::new() }
				} else {
					resolved
				};
				if pattern.selection_pairs.len() > 1 {
					let segments = positional_rewrite_segments(
						&base,
						pattern.selection_pairs.len(),
						pattern.tokens.iter().any(|token| matches!(token, Token::Gap { .. })),
					);
					if let Some(segments) = segments {
						let mut positional_changes = 0usize;
						for candidate in &ordered {
							let mut selections: Vec<((usize, usize), String)> = candidate
								.selection_spans
								.iter()
								.copied()
								.enumerate()
								.map(|(index, span)| (span, segments.get(index).cloned().unwrap_or_default()))
								.collect();
							selections.sort_by_key(|(span, _)| std::cmp::Reverse(span.0));
							for (span, replacement) in selections {
								if content[span.0..span.1] == replacement {
									continue;
								}
								planned.push(PlannedEdit {
									start: span.0,
									end: span.1,
									replacement,
									operation_number: number,
								});
								positional_changes += 1;
							}
							last_match_offset = candidate.match_start;
						}
						if positional_changes == 0 {
							let hint = positional_would_change_hint(content, &candidates[0], &pattern, &segments, path);
							return Err(throw_no_op(
								content,
								path,
								Some(number),
								operation.all.then_some(candidates.len()),
								candidates[0].match_start,
								hint,
								no_op_key,
								hash,
							));
						}
						continue;
					}
					if !candidates.iter().all(|candidate| rewrite_proves_whole_span(content, candidate, &base)) {
						let candidate = &candidates[0];
						let one_line_rewrite = base.split_whitespace().collect::<Vec<_>>().join(" ");
						let repeated: Vec<String> = vec![one_line_rewrite; pattern.selection_pairs.len()];
						let header = if operation.all { format!("{OPENER}*") } else { OPENER.to_string() };
						return Err(format!(
							"Operation {number} has {} selections, but REWRITE proves neither positional substitution nor whole-span replacement.\nCopy-ready per-selection interpretation:\n{header}\n{}\n{REWRITE_HEADER}\n{}\nCopy-ready whole-span interpretation:\n{header}\n{}\n{REWRITE_HEADER}\n{}",
							pattern.selection_pairs.len(),
							operation.pattern_text,
							repeated.join("\n"),
							operation.pattern_text,
							rewrite_selection_spans(content, candidate, &repeated),
						));
					}
				}
				for located in &ordered {
					let mut resolved_candidate_rewrite = base.clone();
					if let Some(reference) = lone_reference
						&& !operation.all
						&& let Some(referenced) = removed_by_operation.get(reference - 1).and_then(|value| value.as_deref())
					{
						let anchor_line_start = content[..located.match_start]
							.rfind('\n')
							.map(|value| value + 1)
							.unwrap_or(0);
						let anchor_line_end = content[located.match_end..]
							.find('\n')
							.map(|value| located.match_end + value)
							.unwrap_or(content.len());
						let anchor_line = &content[anchor_line_start..anchor_line_end];
						let single_whole_line = !pattern.insertion
							&& !anchor_line.trim().is_empty()
							&& !content[located.match_start..located.match_end].contains('\n')
							&& content[anchor_line_start..located.match_start].trim().is_empty()
							&& content[located.match_end..anchor_line_end].trim().is_empty();
						let anchor_normalized = normalize_text(anchor_line).text;
						if single_whole_line
							&& !anchor_normalized.is_empty()
							&& !normalize_text(referenced).text.contains(&anchor_normalized)
						{
							resolved_candidate_rewrite = format!("{}\n\n{anchor_line}", referenced.trim_end_matches('\n'));
						}
					}
					let rewrite = if pattern.line_insertion {
						frame_line_insertion(content, located.start, &resolved_candidate_rewrite, false)
					} else {
						resolved_candidate_rewrite
					};
					let prepared = prepare_candidate_edit(content, located, &pattern, &rewrite, number)?;
					if prepared.deleted_text.is_some() {
						if candidates.len() == 1 {
							removed_by_operation[index] = prepared.deleted_text.clone();
						}
						let note = if operation.assumed_deletion {
							format!(
								"Note: operation {number} had no {REWRITE_HEADER} REWRITE and was applied as a move deletion (a later operation re-emits its block)."
							)
						} else {
							let lines = prepared
								.deleted_text
								.as_deref()
								.unwrap_or_default()
								.split('\n')
								.filter(|entry| !entry.trim().is_empty())
								.count();
							format!(
								"Note: operation {number} deleted {lines} line(s); an empty REWRITE means deletion — resend with the final text if you meant to replace."
							)
						};
						deletion_notes.insert(number, note);
					}
					last_match_offset = prepared.candidate.match_start;
					let desired_state_satisfied = operation.desired_state
						&& normalize_text(&prepared.replacement).text
							== normalize_text(&content[prepared.candidate.start..prepared.candidate.end]).text;
					if prepared.replacement == content[prepared.candidate.start..prepared.candidate.end]
						|| desired_state_satisfied
					{
						if operation.all {
							continue;
						}
						if let Some((start, end)) = duplicate_collapse_span(content, &prepared.candidate, &prepared.replacement) {
							planned.push(PlannedEdit {
								start,
								end,
								replacement: prepared.replacement,
								operation_number: number,
							});
							changes += 1;
							continue;
						}
						if operation.desired_state {
							// A desired-state assertion that already holds is satisfied, not
							// an error; sibling ops still apply.
							recovery_notes.push(format!(
								"Note: operation {number} already matches the file; no change was needed there."
							));
							changes += 1;
							continue;
						}
						let hint = lone_reference
							.is_none()
							.then(|| would_change_hint(content, located, &pattern, &rewrite, number, path))
							.flatten();
						return Err(throw_no_op(
							content,
							path,
							Some(number),
							None,
							prepared.candidate.match_start,
							hint,
							no_op_key,
							hash,
						));
					}
					planned.push(PlannedEdit {
						start: prepared.candidate.start,
						end: prepared.candidate.end,
						replacement: prepared.replacement,
						operation_number: number,
					});
					changes += 1;
				}
				if operation.all && changes == 0 {
					let rewrite = if pattern.line_insertion {
						frame_line_insertion(content, candidates[0].start, &base, false)
					} else {
						base.clone()
					};
					let hint = lone_reference
						.is_none()
						.then(|| would_change_hint(content, &candidates[0], &pattern, &rewrite, number, path))
						.flatten();
					return Err(throw_no_op(
						content,
						path,
						Some(number),
						Some(candidates.len()),
						candidates[0].match_start,
						hint,
						no_op_key,
						hash,
					));
				}
			},
		}
	}
	if planned.is_empty() {
		return Err(throw_no_op(content, path, None, None, last_match_offset, None, no_op_key, hash));
	}
	let mut sorted = planned;
	sorted.sort_by(|left, right| left.start.cmp(&right.start).then(left.end.cmp(&right.end)));
	let mut ordered: Vec<PlannedEdit> = Vec::new();
	for current in sorted {
		let previous = ordered.last().cloned();
		let overlaps = previous.as_ref().is_some_and(|previous| {
			current.start < previous.end
				|| (current.start == previous.start
					&& current.end == current.start
					&& previous.end == previous.start)
		});
		if !overlaps {
			ordered.push(current);
			continue;
		}
		let previous = previous.expect("overlap implies a previous edit");
		// Overlapping spans are only a conflict when they disagree. A broad
		// `«*` plus a narrower op over one of its matches (a rename and the
		// line containing it) produce byte-identical text for the shared
		// region — merge instead of rejecting a consistent payload.
		if let Some(merged) = reconcile_overlap(content, &previous, &current) {
			*ordered.last_mut().expect("previous edit") = merged;
			continue;
		}
		if previous.operation_number == current.operation_number {
			// Overlapping candidates of one operation are a matcher artifact;
			// keep the first.
			continue;
		}
		let first_line = text::line_number_at(content, previous.start);
		let second_line = text::line_number_at(content, current.start);
		return Err(format!(
			"Operations {} and {} target overlapping original spans near lines {} and {}.\n\nConflicting candidates:\nOperation {} near line {}:\n{}\n\nOperation {} near line {}:\n{}\n\nKeep whichever states the intended final text and drop the other.",
			previous.operation_number,
			current.operation_number,
			first_line,
			second_line,
			previous.operation_number,
			first_line,
			operation_payload(&operations[previous.operation_number - 1]),
			current.operation_number,
			second_line,
			operation_payload(&operations[current.operation_number - 1]),
		));
	}
	let mut result = content.to_string();
	for edit in ordered.iter().rev() {
		result.replace_range(edit.start..edit.end, &edit.replacement);
	}
	if result == content {
		return Err(throw_no_op(content, path, None, None, last_match_offset, None, no_op_key, hash));
	}
	clear_no_op(no_op_key);
	notes.extend(recovery_notes);
	notes.extend(deletion_notes.into_values());
	Ok(result)
}

#[derive(Debug, Clone)]
struct PlannedEdit {
	start: usize,
	end: usize,
	replacement: String,
	operation_number: usize,
}

struct PreparedEdit {
	candidate: Candidate,
	replacement: String,
	deleted_text: Option<String>,
}

fn resolve_rewrite_references(rewrite: &str, removed: &[Option<String>]) -> Result<String, String> {
	let mut out = Vec::new();
	for line in rewrite.split('\n') {
		if let Some(reference) = register_number(line.trim()) {
			let Some(referenced) = removed.get(reference - 1).and_then(|value| value.as_ref()) else {
				return Err(format!("{REWRITE_HEADER}{reference} must reference an earlier deletion operation."));
			};
			out.push(referenced.clone());
		} else {
			out.push(line.to_string());
		}
	}
	Ok(out.join("\n"))
}

fn selected_captures(pattern: &ParsedPattern, candidate: &Candidate) -> Vec<String> {
	pattern
		.tokens
		.get(pattern.selection_start..pattern.selection_end)
		.unwrap_or_default()
		.iter()
		.filter_map(|token| match token {
			Token::Gap { capture_index, .. } => {
				Some(candidate.captures.get(*capture_index).cloned().unwrap_or_default())
			},
			_ => None,
		})
		.collect()
}

fn selection_captures(pattern: &ParsedPattern, candidate: &Candidate, selection_index: usize) -> Vec<String> {
	let Some(pair) = pattern.selection_pairs.get(selection_index) else {
		return candidate.captures.clone();
	};
	pattern
		.tokens
		.get(pair.start..pair.end)
		.unwrap_or_default()
		.iter()
		.filter_map(|token| match token {
			Token::Gap { capture_index, .. } => {
				Some(candidate.captures.get(*capture_index).cloned().unwrap_or_default())
			},
			_ => None,
		})
		.collect()
}

/// Render the desired text, then drop boundary text the model double-typed on
/// both a selection and its adjacent literal (`crypto.⟪.? .get⟫` after
/// `crypto.`). The literal stays authoritative.
fn align_boundary_echoes(content: &str, candidate: &Candidate, replacement: &str) -> String {
	if replacement.is_empty() || (candidate.start == candidate.match_start && candidate.end == candidate.match_end) {
		return replacement.to_string();
	}
	let prefix = &content[candidate.match_start..candidate.start];
	let suffix = &content[candidate.end..candidate.match_end];
	let normalized_replacement = normalize_text(replacement);
	let normalized_prefix = normalize_text(prefix).text;
	let normalized_suffix = normalize_text(suffix).text;
	let prefix_echo = normalized_prefix.encode_utf16().count() >= 3
		&& normalized_replacement.text.starts_with(normalized_prefix.as_str());
	let suffix_echo = !normalized_suffix.is_empty()
		&& normalized_replacement.text.ends_with(normalized_suffix.as_str())
		&& (normalized_suffix.encode_utf16().count() >= 3 || prefix_echo);
	if !prefix_echo && !suffix_echo {
		return replacement.to_string();
	}
	let mut from = 0usize;
	let mut to = replacement.len();
	if prefix_echo {
		from = if replacement.starts_with(prefix) {
			prefix.len()
		} else {
			normalized_replacement.end(normalized_prefix.len(), replacement.len())
		};
	}
	if suffix_echo {
		to = if replacement.ends_with(suffix) {
			replacement.len() - suffix.len()
		} else {
			normalized_replacement.start(
				normalized_replacement.text.len().saturating_sub(normalized_suffix.len()),
				replacement.len(),
			)
		};
	}
	if from > to {
		return replacement.to_string();
	}
	let mut aligned = replacement[from..to].to_string();
	if prefix.chars().next_back().is_some_and(char::is_whitespace)
		&& aligned.starts_with(char::is_whitespace)
	{
		aligned = aligned.trim_start_matches(char::is_whitespace).to_string();
	}
	if suffix.chars().next().is_some_and(char::is_whitespace) && aligned.ends_with(char::is_whitespace) {
		aligned = aligned.trim_end_matches(char::is_whitespace).to_string();
	}
	aligned
}

/// `lastIndexOf("\n", fromIndex)` including `fromIndex` itself.
fn last_newline_at_or_before(content: &str, from: usize) -> Option<usize> {
	if content.is_empty() {
		return None;
	}
	let mut end = from.min(content.len() - 1) + 1;
	while end > 0 && !content.is_char_boundary(end) {
		end -= 1;
	}
	content[..end].rfind('\n')
}

/// A whole-line deletion swallows a separator blank line so block style stays
/// intact (blank line above a closing delimiter, blank line below an opener).
fn expand_full_line_deletion(content: &str, candidate: &Candidate) -> Candidate {
	if candidate.start == candidate.end {
		return candidate.clone();
	}
	let line_start = last_newline_at_or_before(content, candidate.start.saturating_sub(1))
		.map(|value| value + 1)
		.unwrap_or(0);
	let newline = content[candidate.end..].find('\n').map(|value| candidate.end + value);
	let line_end = newline.unwrap_or(content.len());
	let whitespace_only = |text: &str| text.bytes().all(|byte| byte == b' ' || byte == b'\t');
	if !whitespace_only(&content[line_start..candidate.start])
		|| !whitespace_only(&content[candidate.end..line_end])
	{
		return candidate.clone();
	}
	let mut end = newline.map(|newline| newline + 1).unwrap_or(line_end);
	let mut expanded_start = line_start;
	if line_start > 0 && end < content.len() {
		let previous_end = line_start - 1;
		let previous_start = last_newline_at_or_before(content, previous_end.saturating_sub(1)).map(|value| value + 1).unwrap_or(0);
		let next_end = content[end..].find('\n').map(|value| end + value);
		let next_text = &content[end..next_end.unwrap_or(content.len())];
		let previous_text = &content[previous_start..previous_end];
		let previous_blank = previous_text.trim().is_empty();
		let next_blank = next_text.trim().is_empty();
		if previous_blank && next_blank {
			end = next_end.map(|next_end| next_end + 1).unwrap_or(content.len());
		} else if previous_blank && starts_with_closing_delimiter(next_text) {
			expanded_start = previous_start;
		} else if next_blank && ends_with_opening_delimiter(previous_text) {
			end = next_end.map(|next_end| next_end + 1).unwrap_or(content.len());
		}
	} else if line_start > 0 && end >= content.len() {
		let previous_end = line_start - 1;
		let previous_start = last_newline_at_or_before(content, previous_end.saturating_sub(1)).map(|value| value + 1).unwrap_or(0);
		if content[previous_start..previous_end].trim().is_empty() {
			expanded_start = previous_start;
		}
	}
	Candidate {
		start: expanded_start,
		end,
		..candidate.clone()
	}
}

fn starts_with_closing_delimiter(text: &str) -> bool {
	let trimmed = text.trim_start_matches([' ', '\t']);
	trimmed.starts_with(')') || trimmed.starts_with(']') || trimmed.starts_with('}')
}

fn ends_with_opening_delimiter(text: &str) -> bool {
	let trimmed = text.trim_end_matches(char::is_whitespace);
	trimmed.ends_with('(') || trimmed.ends_with('{') || trimmed.ends_with('[')
}

/// Frame a whole-line insert so it lands as its own line at `offset`.
fn frame_line_insertion(content: &str, offset: usize, desired: &str, blank_separated: bool) -> String {
	if desired.is_empty() {
		return String::new();
	}
	if offset > 0 && offset == content.len() && content.as_bytes()[offset - 1] != b'\n' {
		return if desired.starts_with('\n') {
			desired.to_string()
		} else {
			format!("\n{desired}")
		};
	}
	let mut framed = if desired.ends_with('\n') {
		desired.to_string()
	} else {
		format!("{desired}\n")
	};
	// The authored leading blank was absorbed by an existing blank line above;
	// re-emit the separator on the trailing side.
	if blank_separated && content.as_bytes().get(offset).copied() != Some(b'\n') && !framed.ends_with("\n\n") {
		framed.push('\n');
	}
	framed
}

fn prepare_candidate_edit(
	content: &str,
	candidate: &Candidate,
	pattern: &ParsedPattern,
	rewrite: &str,
	number: usize,
) -> Result<PreparedEdit, String> {
	let mut candidate = candidate.clone();
	let captures = selected_captures(pattern, &candidate);
	let mut replacement = render_rewrite(rewrite, &captures, number)?;
	replacement = align_boundary_echoes(content, &candidate, &replacement);
	let mut deleted_text = None;
	if replacement.is_empty() {
		deleted_text = Some(content[candidate.start..candidate.end].to_string());
		candidate = expand_full_line_deletion(content, &candidate);
	}
	Ok(PreparedEdit { candidate, replacement, deleted_text })
}

fn prepare_inline_selection_edit(
	content: &str,
	located: &Candidate,
	span: (usize, usize),
	selection: SelectionPair,
	rewrite: &str,
	captures: &[String],
	number: usize,
) -> Result<PreparedEdit, String> {
	let mut start = span.0;
	let mut end = span.1;
	// A gap-captured old side spans the raw text between anchors, padding
	// included; the desired text states content only, so boundary whitespace
	// stays outside the replacement and the span stays on the anchor line.
	if selection.gap_only {
		if let Some(newline) = content[start..].find('\n').map(|value| start + value)
			&& newline < end
		{
			end = newline;
		}
		while start < end && matches!(content.as_bytes()[start], b' ' | b'\t') {
			start += 1;
		}
		while end > start && matches!(content.as_bytes()[end - 1], b' ' | b'\t') {
			end -= 1;
		}
	}
	let mut candidate = Candidate {
		start,
		end,
		match_start: start,
		match_end: end,
		captures: located.captures.clone(),
		selection_spans: located.selection_spans.clone(),
	};
	let stripped_leading_blank =
		candidate.start == candidate.end && rewrite.starts_with('\n') && content.as_bytes().get(candidate.start.wrapping_sub(1)) == Some(&b'\n');
	let desired = if stripped_leading_blank { &rewrite[1..] } else { rewrite };
	let blank_separated = stripped_leading_blank
		&& candidate.start >= 2
		&& content.as_bytes().get(candidate.start - 2) == Some(&b'\n');
	let framed = if selection.line_insertion {
		frame_line_insertion(content, candidate.start, desired, blank_separated)
	} else {
		desired.to_string()
	};
	let replacement = render_rewrite(&framed, captures, number)?;
	if replacement.is_empty() && candidate.start != candidate.end {
		let deleted_text = content[candidate.start..candidate.end].to_string();
		candidate = expand_full_line_deletion(content, &candidate);
		return Ok(PreparedEdit { candidate, replacement, deleted_text: Some(deleted_text) });
	}
	Ok(PreparedEdit { candidate, replacement, deleted_text: None })
}

/// Merge two overlapping planned edits when they agree. Each edit is applied
/// independently to the union span; identical results mean the payload is
/// consistent, so one merged edit replaces both. Disagreement returns `None`
/// and the caller reports the conflict.
fn reconcile_overlap(content: &str, left: &PlannedEdit, right: &PlannedEdit) -> Option<PlannedEdit> {
	let start = left.start.min(right.start);
	let end = left.end.max(right.end);
	let container_merge = |outer: &PlannedEdit, inner: &PlannedEdit| -> Option<PlannedEdit> {
		if outer.replacement.is_empty()
			&& !inner.replacement.is_empty()
			&& inner.start >= outer.start
			&& inner.end <= outer.end
		{
			let mut replacement = inner.replacement.clone();
			if outer.end > 0
				&& content.as_bytes().get(outer.end - 1) == Some(&b'\n')
				&& !inner.replacement.ends_with('\n')
			{
				replacement.push('\n');
			}
			return Some(PlannedEdit {
				start: outer.start,
				end: outer.end,
				replacement,
				operation_number: inner.operation_number,
			});
		}
		None
	};
	if let Some(merged) = container_merge(left, right) {
		return Some(merged);
	}
	if let Some(merged) = container_merge(right, left) {
		return Some(merged);
	}
	let project = |edit: &PlannedEdit| -> String {
		let mut projected = String::new();
		projected.push_str(&content[start..edit.start]);
		projected.push_str(&edit.replacement);
		projected.push_str(&content[edit.end..end]);
		projected
	};
	let projected = project(left);
	if projected != project(right) {
		return None;
	}
	Some(PlannedEdit {
		start,
		end,
		replacement: projected,
		operation_number: left.operation_number,
	})
}

fn operation_payload(operation: &Operation) -> String {
	operation_payload_with(operation, if operation.all { "*" } else { "" }, &operation.source_pattern_text)
}

fn operation_payload_with(operation: &Operation, target: &str, pattern_text: &str) -> String {
	let header = format!("{OPENER}{target}");
	match &operation.rewrite {
		Rewrite::Inline(_) => format!("{header}\n{pattern_text}"),
		Rewrite::Explicit(text) => format!("{header}\n{pattern_text}\n{REWRITE_HEADER}\n{text}"),
	}
}

fn positional_rewrite_segments(rewrite: &str, count: usize, pattern_has_gaps: bool) -> Option<Vec<String>> {
	let lines: Vec<&str> = rewrite.split('\n').collect();
	let has_whole_line_gap = lines.iter().any(|line| line.trim() == GAP.to_string());
	if has_whole_line_gap {
		if pattern_has_gaps {
			return None;
		}
		let mut groups: Vec<Vec<&str>> = vec![Vec::new()];
		for line in lines {
			if line.trim() == GAP.to_string() {
				groups.push(Vec::new());
			} else if let Some(current) = groups.last_mut() {
				current.push(line);
			}
		}
		if groups.len() == count {
			return Some(groups.into_iter().map(|group| group.join("\n")).collect());
		}
		return None;
	}
	if lines.len() == count {
		return Some(lines.into_iter().map(str::to_string).collect());
	}
	if !rewrite.contains('\n') {
		let segments: Vec<String> = rewrite.split(GAP).map(str::to_string).collect();
		if segments.len() == count {
			return Some(segments);
		}
	}
	None
}

fn rewrite_proves_whole_span(content: &str, candidate: &Candidate, rewrite: &str) -> bool {
	let normalized_rewrite = normalize_text(rewrite).text;
	let contexts: Vec<String> = candidate
		.selection_spans
		.iter()
		.zip(candidate.selection_spans.iter().skip(1))
		.map(|(span, next)| normalize_text(&content[span.1..next.0]).text)
		.filter(|context| !context.is_empty())
		.collect();
	if contexts.is_empty() {
		return false;
	}
	let mut from = 0usize;
	for context in contexts {
		let Some(found) = normalized_rewrite[from..].find(&context) else {
			return false;
		};
		from += found + context.len();
	}
	true
}

fn rewrite_selection_spans(content: &str, candidate: &Candidate, replacements: &[String]) -> String {
	let mut rewritten = content[candidate.start..candidate.end].to_string();
	let mut indexed: Vec<((usize, usize), String)> = candidate
		.selection_spans
		.iter()
		.copied()
		.zip(replacements.iter().cloned())
		.collect();
	indexed.sort_by_key(|(span, _)| std::cmp::Reverse(span.0));
	for ((start, end), replacement) in indexed {
		rewritten.replace_range(start - candidate.start..end - candidate.start, &replacement);
	}
	rewritten
}

fn exact_and_fuzzy_candidates(content: &str, pattern: &ParsedPattern, path: &str) -> (Vec<Candidate>, bool) {
	let exact = collect_normalized_candidates(content, pattern);
	let fuzzy = collect_fuzzy_candidates(content, pattern, path);
	let overflow = exact.overflow || fuzzy.overflow;
	let mut candidates = exact.candidates;
	for candidate in fuzzy.candidates {
		if candidates.iter().any(|existing| {
			existing.start == candidate.start
				&& existing.end == candidate.end
				&& existing.match_start == candidate.match_start
				&& existing.match_end == candidate.match_end
		}) {
			continue;
		}
		candidates.push(candidate);
	}
	candidates.sort_by(|left, right| {
		left.start
			.cmp(&right.start)
			.then(left.end.cmp(&right.end))
			.then(left.match_start.cmp(&right.match_start))
			.then(left.match_end.cmp(&right.match_end))
	});
	(candidates, overflow)
}

fn would_change_hint(
	content: &str,
	chosen: &Candidate,
	pattern: &ParsedPattern,
	rewrite: &str,
	number: usize,
	path: &str,
) -> Option<String> {
	let (alternatives, overflow) = exact_and_fuzzy_candidates(content, pattern, path);
	if overflow {
		return None;
	}
	for alternative in &alternatives {
		if alternative.start == chosen.start
			&& alternative.end == chosen.end
			&& alternative.match_start == chosen.match_start
			&& alternative.match_end == chosen.match_end
		{
			continue;
		}
		let prepared = match prepare_candidate_edit(content, alternative, pattern, rewrite, number) {
			Ok(prepared) => prepared,
			Err(_) => continue,
		};
		if content[prepared.candidate.start..prepared.candidate.end] == prepared.replacement {
			continue;
		}
		let line = text::line_number_at(content, prepared.candidate.start);
		return Some(format!("Line {line} also matches and WOULD change — target it by adding context unique to it."));
	}
	None
}

fn positional_would_change_hint(
	content: &str,
	chosen: &Candidate,
	pattern: &ParsedPattern,
	replacements: &[String],
	path: &str,
) -> Option<String> {
	let (alternatives, overflow) = exact_and_fuzzy_candidates(content, pattern, path);
	if overflow {
		return None;
	}
	for alternative in &alternatives {
		if alternative.start == chosen.start
			&& alternative.end == chosen.end
			&& alternative.match_start == chosen.match_start
			&& alternative.match_end == chosen.match_end
		{
			continue;
		}
		let changes = alternative
			.selection_spans
			.iter()
			.enumerate()
			.any(|(index, span)| &content[span.0..span.1] != replacements[index].as_str());
		if !changes {
			continue;
		}
		let line = text::line_number_at(content, alternative.start);
		return Some(format!("Line {line} also matches and WOULD change — target it by adding context unique to it."));
	}
	None
}

fn inline_would_change_hint(
	content: &str,
	chosen: &Candidate,
	pattern: &ParsedPattern,
	replacements: &[String],
	number: usize,
	path: &str,
) -> Option<String> {
	let (alternatives, overflow) = exact_and_fuzzy_candidates(content, pattern, path);
	if overflow {
		return None;
	}
	for alternative in &alternatives {
		if alternative.start == chosen.start
			&& alternative.end == chosen.end
			&& alternative.match_start == chosen.match_start
			&& alternative.match_end == chosen.match_end
		{
			continue;
		}
		let changes = alternative.selection_spans.iter().enumerate().any(|(index, span)| {
			let Some(selection) = pattern.selection_pairs.get(index) else {
				return false;
			};
			let captures = selection_captures(pattern, alternative, index);
			let prepared = match prepare_inline_selection_edit(
				content,
				alternative,
				*span,
				*selection,
				replacements.get(index).map(String::as_str).unwrap_or(""),
				&captures,
				number,
			) {
				Ok(prepared) => prepared,
				Err(_) => return false,
			};
			content[prepared.candidate.start..prepared.candidate.end] != prepared.replacement
		});
		if !changes {
			continue;
		}
		let line = text::line_number_at(content, alternative.start);
		return Some(format!("Line {line} also matches and WOULD change — target it by adding context unique to it."));
	}
	None
}

fn normalized_index_at(normalized: &Normalized, offset: usize) -> usize {
	match normalized.starts.binary_search(&offset) {
		Ok(index) => index,
		Err(index) => index,
	}
}

fn duplicate_collapse_span(
	content: &str,
	candidate: &Candidate,
	replacement: &str,
) -> Option<(usize, usize)> {
	const MIN_OVERLAP: usize = 8;
	let rewrite_normalized = normalize_text(replacement).text;
	if rewrite_normalized.len() < MIN_OVERLAP || rewrite_normalized.len() > 5000 {
		return None;
	}
	let normalized = normalize_text(content);
	let match_start = normalized_index_at(&normalized, candidate.start);
	let match_end = normalized_index_at(&normalized, candidate.end);
	for overlap in (MIN_OVERLAP..=rewrite_normalized.len().min(match_start)).rev() {
		if normalized.text[match_start - overlap..match_start] != rewrite_normalized[..overlap] {
			continue;
		}
		let mut start = normalized.start(match_start - overlap, candidate.start);
		let line_start = content[..start].rfind('\n').map(|value| value + 1).unwrap_or(0);
		if content[line_start..start].trim().is_empty() {
			start = line_start;
		}
		return Some((start, candidate.end));
	}
	let available = normalized.text.len().saturating_sub(match_end);
	for overlap in (MIN_OVERLAP..=rewrite_normalized.len().min(available)).rev() {
		if normalized.text[match_end..match_end + overlap] != rewrite_normalized[rewrite_normalized.len() - overlap..] {
			continue;
		}
		let mut end = normalized.end(match_end + overlap - 1, candidate.end);
		let line_end = content[end..].find('\n').map(|value| end + value).unwrap_or(content.len());
		if content[end..line_end].trim().is_empty() {
			end = line_end;
		}
		return Some((candidate.start, end));
	}
	None
}

struct NoOpState {
	hash: u64,
	count: usize,
}

static NO_OP_BY_PATH: LazyLock<Mutex<HashMap<String, NoOpState>>> =
	LazyLock::new(|| Mutex::new(HashMap::new()));

fn payload_hash(input: &str) -> u64 {
	let mut hash = 0u32;
	for unit in input.encode_utf16() {
		hash ^= u32::from(unit);
		hash = hash.wrapping_mul(16_777_619);
	}
	u64::from(hash)
}

fn reset_no_op_hash(path: &str, hash: u64) {
	let mut registry = NO_OP_BY_PATH.lock().unwrap_or_else(|error| error.into_inner());
	if registry.get(path).is_some_and(|state| state.hash != hash) {
		registry.remove(path);
	}
}

fn clear_no_op(path: &str) {
	NO_OP_BY_PATH
		.lock()
		.unwrap_or_else(|error| error.into_inner())
		.remove(path);
}

fn throw_no_op(
	content: &str,
	path: &str,
	operation_number: Option<usize>,
	match_count: Option<usize>,
	offset: usize,
	hint: Option<String>,
	no_op_key: &str,
	hash: u64,
) -> String {
	let count = {
		let mut registry = NO_OP_BY_PATH.lock().unwrap_or_else(|error| error.into_inner());
		let count = match registry.get(no_op_key) {
			Some(state) if state.hash == hash => state.count + 1,
			_ => 1,
		};
		registry.insert(no_op_key.to_string(), NoOpState { hash, count });
		count
	};
	let base = if count >= 3 {
		format!("STOP: identical no-op repeated {count} times for {path}. Re-read current code and send a changed payload, or move on.")
	} else {
		match (operation_number, match_count) {
			(None, _) => format!("Edits to {path} made no change."),
			(Some(number), None) => format!("Operation {number} makes no change to {path}."),
			(Some(number), Some(count)) => {
				format!("Operation {number} {OPENER}* matched {count} occurrences but all make no change to {path}.")
			},
		}
	};
	let grounding = format!(
		"\nYour rewrite normalized to text identical to these lines. Indentation-only changes are applied verbatim; adjust the authored REWRITE if another whitespace change was intended.\nCurrent file content near the closest match (no re-read needed):\n{}",
		numbered_preview(content, offset)
	);
	match hint {
		Some(hint) => format!("{base}{grounding}\n{hint}"),
		None => format!("{base}{grounding}"),
	}
}

fn render_rewrite(rewrite: &str, selected_captures: &[String], number: usize) -> Result<String, String> {
	if rewrite.contains(SELECT_OPEN) || rewrite.contains(SELECT_CLOSE) {
		return Err(format!(
			"Operation {number} has selection markers in REWRITE; PATTERN is current text, REWRITE is final text."
		));
	}
	let sentinel = |index: usize| format!("\u{0}V8GAP{index}\u{0}");
	let mut marker_index = 0usize;
	let mut marked = String::with_capacity(rewrite.len());
	let mut index = 0usize;
	while index < rewrite.len() {
		if rewrite[index..].starts_with(GAP) {
			let line_start = rewrite[..index].rfind('\n').map(|value| value + 1).unwrap_or(0);
			let line_end = rewrite[index..]
				.find('\n')
				.map(|value| index + value)
				.unwrap_or(rewrite.len());
			let line = &rewrite[line_start..line_end];
			if marker_index >= selected_captures.len() {
				// An unclaimed gap alone on its line is context elision, never final
				// text; writing it verbatim splices a literal `…` into the file.
				if line.trim() == GAP.to_string() {
					return Err(format!(
						"Operation {number} REWRITE has a whole-line {GAP} with no MATCH gap to re-emit. REWRITE is final text written verbatim: type the elided lines out, or add a matching {GAP} gap to MATCH. To write a literal {GAP} line, use the write tool."
					));
				}
				marked.push(GAP);
				marker_index += 1;
			} else {
				let capture = &selected_captures[marker_index];
				let open_ended = line.trim() == GAP.to_string()
					|| rewrite[index + GAP.len_utf8()..line_end].trim().is_empty();
				if capture.contains('\n') && !open_ended {
					// A mid-line gap with text after it on the same line cannot
					// re-emit a multi-line capture: it is literal final text.
					marked.push(GAP);
				} else {
					marked.push_str(&sentinel(marker_index));
					marker_index += 1;
				}
			}
			index += GAP.len_utf8();
			continue;
		}
		let character = rewrite[index..].chars().next().expect("char boundary");
		marked.push(character);
		index += character.len_utf8();
	}
	if marker_index == 0 || selected_captures.is_empty() {
		return Ok(decode_literal_markers(&marked));
	}
	let mut rendered = marked;
	for (index, capture) in selected_captures.iter().enumerate() {
		let sentinel = sentinel(index);
		let Some(at) = rendered.find(&sentinel) else {
			continue;
		};
		rendered.replace_range(at..at + sentinel.len(), capture);
	}
	Ok(decode_literal_markers(&rendered))
}
