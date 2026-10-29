//! `sloppy` wire mode: `§`/`«` operations with `⟪old│new⟫` selections,
//! `…` gaps, and `＋` add lines.
//!
//! Ports the documented grammar and matching core of
//! `packages/coding-agent/src/edit/sloppy.ts`: literal-anchored raw matching,
//! a whitespace-insensitive normalized retry for marker-free patterns, fuzzy
//! retry for a single unique literal, unique-match enforcement (`«*` applies to
//! every match), gap captures, inline selection rewrites, and atomic
//! all-or-nothing application per section. Recovery heuristics that invent byte
//! boundaries from near-matches (echo repair, punctuation-pair variants,
//! closest-desired-block) are intentionally not ported; those payloads fail
//! closed with the upstream "did not match" guidance instead of guessing.

use std::collections::HashMap;

use serde_json::Value;

use super::{
	fsops, paths,
	replace::find_match,
	text::{self, DEFAULT_FUZZY_THRESHOLD},
};
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
#[allow(dead_code)]
struct Operation {
	pattern_text: String,
	source_pattern_text: String,
	rewrite: Rewrite,
	all: bool,
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
#[allow(dead_code)]
struct ParsedPattern {
	tokens: Vec<Token>,
	selection_boundaries: Vec<usize>,
	insertion: bool,
	selection_start: usize,
	selection_end: usize,
	selection_pairs: Vec<(usize, usize)>,
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
		let (absolute, mut display) = paths::resolve_edit_path(&authored, &ctx.cwd, true, &ctx.cancellation_token).await?;
		// Models copy read selectors (`file.ts:23`, `file.ts:grep=x`) into edit paths.
		let (absolute, display) = match tokio::fs::metadata(&absolute).await {
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
		let new_text = apply_operations(&text, &section.body, &display, &mut notes).map_err(|message| {
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
		verify_explicit_match(content, &parsed, path, number)?;
	}
	Ok(Operation {
		pattern_text: final_pattern,
		source_pattern_text,
		rewrite,
		all,
		recovery_note: note,
	})
}

fn is_register_reference(trimmed: &str) -> bool {
	register_number(trimmed).is_some()
}

fn register_number(trimmed: &str) -> Option<usize> {
	let rest = trimmed.strip_prefix(REWRITE_HEADER)?;
	if rest.is_empty() || !rest.chars().all(|ch| ch.is_ascii_digit()) {
		return None;
	}
	let number: usize = rest.parse().ok()?;
	(number >= 1).then_some(number)
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
		if !source_pattern.contains(GAP)
			&& let Some(block) = closest_desired_block(content, &source_pattern)
		{
			let mut operation = create_operation(&block, &source_pattern, all, number, content, path)?;
			operation.recovery_note = Some(format!(
				"Note: operation {number} stated desired text without markers; the closest matching block was replaced with it."
			));
			operations.push(operation);
			return Ok(());
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
		let operation = create_operation(&source_pattern, "", all, number, content, path)?;
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

/// `« old` / `» new` gluing: a marker followed by whitespace and text is split
/// into the control line and its content.
fn split_glued_marker(line: &str) -> Option<(String, String)> {
	let trimmed_start = line.trim_start_matches([' ', '\t']);
	let indent_len = line.len() - trimmed_start.len();
	if indent_len > 0 {
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
	let content = rest.trim_start_matches([' ', '\t']);
	if content.len() == rest.len() || content.is_empty() {
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

fn relocate_selection_lines(pattern: &str) -> String {
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
		if previous_normalized.text[first + 1..].contains(&old) {
			index += 1;
			continue;
		}
		if previous_normalized.text == old {
			index += 1;
			continue;
		}
		let raw_start = previous_normalized.starts[previous_normalized.text[..first].chars().count()];
		let raw_end = previous_normalized.ends[previous_normalized.text[..first].chars().count() + old.chars().count() - 1];
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
					out.push_str(SELECT_OPEN);
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
			added.push(format!("{}{}", &line[..indent_len], &line[indent_len + ADD_LINE.len_utf8()..]));
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
			selection_boundaries: Vec::new(),
			insertion: false,
			selection_start: 0,
			selection_end: 1,
			selection_pairs: Vec::new(),
		});
	}
	let mut tokens: Vec<Token> = Vec::new();
	let mut literal = String::new();
	let mut capture_count = 0usize;
	let mut boundaries: Vec<usize> = Vec::new();
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
			.map(|pair| (boundaries[pair * 2], boundaries[pair * 2 + 1]))
			.collect()
	} else {
		Vec::new()
	};
	Ok(ParsedPattern { tokens, selection_boundaries: boundaries, insertion, selection_start, selection_end, selection_pairs })
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
	let mut starts = Vec::new();
	let mut from = 0usize;
	while let Some(found) = content[from..].find(first) {
		let position = from + found;
		starts.push(position);
		from = position + first.len();
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
	// Group boundaries that mark empty selections (adjacent boundaries).
	let mut boundary_offsets: Vec<usize> = Vec::new();
	for boundary in &pattern.selection_boundaries {
		let offset = if *boundary >= positions.len() {
			cursor
		} else {
			positions[*boundary].0
		};
		boundary_offsets.push(offset);
	}
	let selection_spans: Vec<(usize, usize)> = if !boundary_offsets.is_empty() && boundary_offsets.len() % 2 == 0 {
		(0..boundary_offsets.len() / 2)
			.map(|pair| {
				let start = boundary_offsets[pair * 2];
				let end = boundary_offsets[pair * 2 + 1];
				(start, end.max(start))
			})
			.collect()
	} else {
		Vec::new()
	};
	let last = positions.last().copied().unwrap_or((start, cursor));
	let match_end = cursor.max(last.1);
	let match_start = positions.first().map(|(start, _)| *start).unwrap_or(start);
	Some(Candidate {
		start: match_start,
		end: match_end,
		match_start,
		match_end,
		captures,
		selection_spans,
	})
}

fn collect_normalized_candidates(content: &str, pattern: &ParsedPattern) -> Vec<Candidate> {
	// Only marker-free literal patterns have a faithful whitespace-insensitive
	// retry; gap/selection patterns must match raw to keep byte boundaries.
	if pattern.tokens.len() != 1 || !pattern.selection_pairs.is_empty() {
		return Vec::new();
	}
	let Token::Literal { normalized, .. } = &pattern.tokens[0] else {
		return Vec::new();
	};
	let source = normalize_text(content);
	let needle = normalized;
	if needle.is_empty() {
		return Vec::new();
	}
	let mut out = Vec::new();
	let mut from = 0usize;
	while let Some(found) = source.text[from..].find(needle.as_str()) {
		let position = from + found;
		let start = source.start(position, 0);
		let end = source.end(position + needle.len(), content.len());
		out.push(Candidate {
			start,
			end,
			match_start: start,
			match_end: end,
			captures: Vec::new(),
			selection_spans: Vec::new(),
		});
		from = position + needle.len().max(1);
		if out.len() > MAX_CANDIDATES {
			break;
		}
	}
	out
}

fn collect_fuzzy_candidates(content: &str, pattern: &ParsedPattern, _path: &str) -> Vec<Candidate> {
	if pattern.tokens.len() != 1 || !pattern.selection_pairs.is_empty() {
		return Vec::new();
	}
	let Token::Literal { text, .. } = &pattern.tokens[0] else {
		return Vec::new();
	};
	let outcome = find_match(content, text, true, DEFAULT_FUZZY_THRESHOLD, &[]);
	if outcome.occurrences > 1 {
		return Vec::new();
	}
	let Some(found) = outcome.matched else {
		return Vec::new();
	};
	vec![Candidate {
		start: found.start_index,
		end: found.start_index + found.actual_text.len(),
		match_start: found.start_index,
		match_end: found.start_index + found.actual_text.len(),
		captures: Vec::new(),
		selection_spans: Vec::new(),
	}]
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
			let char_index = run_normalized.text.chars().count() - old_normalized.chars().count();
			let cut = run_normalized.starts[char_index];
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
		let char_index = previous_normalized.text[..first].chars().count();
		let raw_start = previous_normalized.starts[char_index];
		let raw_end = previous_normalized.ends[char_index + old.chars().count() - 1];
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
						let char_index = prefix_normalized.text[..first].chars().count();
						let raw_start = prefix_normalized.starts[char_index];
						let raw_end = prefix_normalized.ends[char_index + old_normalized.chars().count() - 1];
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
	push(gap_join_candidate(pattern, inline), &mut candidates);
	if let Some(trailing) = trailing {
		push(gap_join_candidate(&trailing, inline), &mut candidates);
	}
	candidates
}

fn locate_with_echo_recovery(
	content: &str,
	operation: &Operation,
	number: usize,
	path: &str,
) -> Result<(Operation, ParsedPattern, Vec<Candidate>), String> {
	match locate(content, &parse_pattern(&operation.pattern_text, number)?, operation, number, path) {
		Ok(candidates) => {
			let pattern = parse_pattern(&operation.pattern_text, number)?;
			return Ok((operation.clone(), pattern, candidates));
		},
		Err(original) => {
			for candidate in recover_pattern_candidates(&operation.pattern_text, matches!(&operation.rewrite, Rewrite::Inline(_))) {
				if let Ok(pattern) = parse_pattern(&candidate, number) {
					let mut retry = operation.clone();
					retry.pattern_text = candidate;
					if let Ok(candidates) = locate(content, &pattern, &retry, number, path) {
						return Ok((retry, pattern, candidates));
					}
				}
			}
			for variant in punctuation_pair_variants(operation) {
				if let Ok(pattern) = parse_pattern(&variant.pattern_text, number)
					&& let Ok(candidates) = locate(content, &pattern, &variant, number, path)
				{
					let partial = candidates.iter().any(|candidate| {
						candidate.selection_spans.iter().any(|(start, end)| {
							if start >= end {
								return false;
							}
							let character = content[*start..].chars().next();
							let before = content[..*start].chars().next_back();
							let after = content[*end..].chars().next();
							character.is_some_and(|ch| before == Some(ch) || after == Some(ch))
						})
					});
					if partial {
						continue;
					}
					return Ok((variant, pattern, candidates));
				}
			}
			Err(original)
		},
	}
}

fn locate(content: &str, pattern: &ParsedPattern, operation: &Operation, number: usize, path: &str) -> Result<Vec<Candidate>, String> {
	let mut candidates = collect_raw_candidates(content, pattern);
	if candidates.is_empty() {
		candidates = collect_normalized_candidates(content, pattern);
	}
	if candidates.is_empty() {
		candidates = collect_fuzzy_candidates(content, pattern, path);
	}
	if candidates.len() > MAX_CANDIDATES {
		return Err(format!("Operation {number} pattern is too broad; add another distinctive {GAP} fragment."));
	}
	if candidates.is_empty() {
		return Err(format!(
			"Operation {number} did not match {path}.\nThe MATCH text must match the file byte-for-byte, including whitespace. Re-read the region and copy its exact text, or use {GAP} for an unknown gap and {SELECT_OPEN}old{SELECT_DIVIDER}new{SELECT_CLOSE} for changes."
		));
	}
	if candidates.len() > 1 && !operation.all {
		let lines: Vec<String> = content.split('\n').map(str::to_string).collect();
		let previews: Vec<String> = candidates
			.iter()
			.take(3)
			.map(|candidate| {
				let line = text::line_number_at(content, candidate.start);
				text::preview_window(&lines, line.saturating_sub(1), 2, 80)
			})
			.collect();
		return Err(format!(
			"Operation {number} is ambiguous: {} ordered tuples match.\n\n{}\n\nAdd context that only the intended match has — add an anchor line, or use {OPENER}* to change every match.",
			candidates.len(),
			previews.join("\n\n")
		));
	}
	Ok(candidates)
}

fn verify_explicit_match(content: &str, pattern: &ParsedPattern, path: &str, number: usize) -> Result<(), String> {
	let operation = Operation {
		pattern_text: String::new(),
		source_pattern_text: String::new(),
		rewrite: Rewrite::Explicit(String::new()),
		all: false,
		recovery_note: None,
	};
	let _ = locate(content, pattern, &operation, number, path)?;
	Ok(())
}

// ── Applying ──────────────────────────────────────────────────────────────

pub fn apply_operations(content: &str, input: &str, path: &str, notes: &mut Vec<String>) -> Result<String, String> {
	let operations = parse_operations(input, content, path)?;
	let mut current = content.to_string();
	let earlier_rewrites: Vec<String> = operations
		.iter()
		.map(|operation| match &operation.rewrite {
			Rewrite::Explicit(text) => text.clone(),
			Rewrite::Inline(replacements) => replacements.join("\n"),
		})
		.collect();
	let mut changed = false;
	for (index, operation) in operations.iter().enumerate() {
		let number = index + 1;
		if let Some(note) = &operation.recovery_note {
			notes.push(note.clone());
		}
		let (operation, pattern, candidates) = locate_with_echo_recovery(&current, operation, number, path)?;
		let mut ordered = candidates.clone();
		ordered.sort_by_key(|candidate| std::cmp::Reverse(candidate.start));
		let mut local_changed = false;
		match &operation.rewrite {
			Rewrite::Explicit(rewrite) => {
				let rewrite = resolve_references(rewrite, &earlier_rewrites, index)?;
				for candidate in ordered {
					let (span_start, span_end) = selection_span(&pattern, &candidate);
					let captures = if pattern.selection_pairs.len() == 1 {
						selection_captures(&pattern, &candidate, 0)
					} else {
						candidate.captures.clone()
					};
					let replacement = render_rewrite(&rewrite, &captures);
					let existing = current[span_start..span_end].to_string();
					if existing == replacement {
						continue;
					}
					current.replace_range(span_start..span_end, &replacement);
					local_changed = true;
				}
			},
			Rewrite::Inline(replacements) => {
				if pattern.selection_pairs.len() != replacements.len() {
					return Err(format!("Operation {number} inline replacements do not align with its selections."));
				}
				let replacements: Vec<String> = replacements
					.iter()
					.map(|rewrite| resolve_references(rewrite, &earlier_rewrites, index))
					.collect::<Result<Vec<_>, String>>()?;
				for candidate in ordered {
					let mut spans: Vec<((usize, usize), String)> = candidate
						.selection_spans
						.iter()
						.enumerate()
						.filter_map(|(selection_index, span)| {
							let rewrite = replacements.get(selection_index)?;
							let captures = selection_captures(&pattern, &candidate, selection_index);
							Some((*span, render_rewrite(rewrite, &captures)))
						})
						.collect();
					spans.sort_by_key(|(span, _)| std::cmp::Reverse(span.0));
					for ((start, end), replacement) in spans {
						if current[start..end] == replacement {
							continue;
						}
						current.replace_range(start..end, &replacement);
						local_changed = true;
					}
				}
			},
		}
		if !local_changed {
			notes.push(format!("Note: operation {number} made no change to {path}."));
		}
		changed |= local_changed;
	}
	if !changed {
		return Err(format!("Edits to {path} made no change."));
	}
	Ok(current)
}

fn resolve_references(rewrite: &str, earlier: &[String], index: usize) -> Result<String, String> {
	let mut out = Vec::new();
	for line in rewrite.split('\n') {
		if let Some(reference) = register_number(line.trim()) {
			if reference >= index + 1 {
				return Err(format!(
					"{REWRITE_HEADER}{reference} must reference an earlier operation, not self/forward."
				));
			}
			let Some(referenced) = earlier.get(reference - 1) else {
				return Err(format!("{REWRITE_HEADER}{reference} must reference an earlier operation."));
			};
			out.push(referenced.clone());
		} else {
			out.push(line.to_string());
		}
	}
	Ok(out.join("\n"))
}

fn selection_span(pattern: &ParsedPattern, candidate: &Candidate) -> (usize, usize) {
	if pattern.selection_pairs.len() == 1 {
		return candidate
			.selection_spans
			.first()
			.copied()
			.unwrap_or((candidate.match_start, candidate.match_end));
	}
	(candidate.match_start, candidate.match_end)
}

fn selection_captures(pattern: &ParsedPattern, candidate: &Candidate, selection_index: usize) -> Vec<String> {
	let Some((start, end)) = pattern.selection_pairs.get(selection_index) else {
		return candidate.captures.clone();
	};
	let mut indices = Vec::new();
	let mut capture_index = 0usize;
	for (token_index, token) in pattern.tokens.iter().enumerate() {
		if let Token::Gap { .. } = token {
			if token_index >= *start && token_index < *end {
				indices.push(capture_index);
			}
			capture_index += 1;
		}
	}
	indices
		.iter()
		.filter_map(|index| candidate.captures.get(*index).cloned())
		.collect()
}

fn render_rewrite(rewrite: &str, captures: &[String]) -> String {
	let mut out = String::with_capacity(rewrite.len());
	let mut capture_index = 0usize;
	for ch in rewrite.chars() {
		if ch == GAP {
			if let Some(capture) = captures.get(capture_index) {
				out.push_str(capture);
			}
			capture_index += 1;
		} else {
			out.push(ch);
		}
	}
	out
}
