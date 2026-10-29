//! `replace` wire mode: one exact-or-fuzzy string replacement per call.
//!
//! Ports `packages/coding-agent/src/edit/modes/replace.ts`. Exact matches win;
//! a non-unique `old_string` is refused unless `replace_all` is set. When no
//! exact match exists, a line-window fuzzy match at `edit.fuzzyThreshold`
//! (default `0.95`) is accepted only when it is unique or decisively dominant.

use serde_json::Value;

use super::{
	fsops, paths,
	text::{self, DEFAULT_FUZZY_THRESHOLD},
};
use crate::{
	state::{AgentContext, ToolResult},
	tools::ToolError,
};

pub const DESCRIPTION: &str = "Single file string replacement; fuzzy whitespace matching.\n\n\
- `path` is required; `old_string` must be non-empty and uniquely identify the change.\n\
- A non-unique `old_string` is refused unless `replace_all: true`.\n\
- Naming a string across a file: `replace_all: true`.\n\
- Do not use this tool to create files; use `write`.";

const MAX_RECORDED_MATCHES: usize = 5;
const OCCURRENCE_PREVIEW_CONTEXT: usize = 5;
const OCCURRENCE_PREVIEW_MAX_LEN: usize = 80;
const FALLBACK_THRESHOLD: f64 = 0.8;
const DOMINANT_FUZZY_MIN_CONFIDENCE: f64 = 0.97;
const DOMINANT_FUZZY_DELTA: f64 = 0.08;

pub fn schema() -> Value {
	serde_json::json!({
		"type": "object",
		"properties": {
			"path": { "type": "string", "description": "File to edit." },
			"old_string": { "type": "string", "description": "Exact current text to replace; MUST be unique unless `replace_all` is true." },
			"new_string": { "type": "string", "description": "Replacement text." },
			"replace_all": { "type": "boolean", "description": "Replace every exact occurrence instead of refusing an ambiguous match." }
		},
		"required": ["path", "old_string", "new_string"]
	})
}

pub async fn execute(args: &Value, ctx: &AgentContext) -> Result<ToolResult, ToolError> {
	let path = super::require_str(args, "path")?;
	let old_string = super::require_str(args, "old_string")?;
	let new_string = super::require_str(args, "new_string")?;
	let replace_all = super::optional_bool(args, "replace_all")?.unwrap_or(false);
	if old_string.is_empty() {
		return Err(ToolError::InvalidArguments("old_string must not be empty.".to_string()));
	}
	let (absolute, display) = paths::resolve_edit_path(&path, &ctx.cwd, true, &ctx.cancellation_token).await?;
	let raw = fsops::read_text(&absolute, &display).await?;
	let (bom, text, ending) = text::normalize_file(&raw);
	let normalized_old = text::to_lf(&old_string);
	let normalized_new = text::to_lf(&new_string);

	let (replaced, count, warnings) =
		replace_text(&text, &normalized_old, &normalized_new, replace_all, true, DEFAULT_FUZZY_THRESHOLD, &display)?;
	if count == 0 {
		return Err(ToolError::ExecutionFailed("old_string was not found".to_string()));
	}
	if replaced == text {
		return Err(ToolError::ExecutionFailed(format!(
			"Edits to {display} resulted in no changes being made."
		)));
	}
	let final_text = text::denormalize_file(&bom, &replaced, ending);
	fsops::commit_text(&absolute, &display, &raw, &final_text, &ctx.cancellation_token, None).await?;
	let diff = text::unified_diff(&display, &text, &replaced);
	let first = text::first_changed_line(&text, &replaced);
	let mut body = format!("[{display}]\n{diff}");
	if !warnings.is_empty() {
		body.push_str("\n\nWarnings:\n");
		body.push_str(&warnings.join("\n"));
	}
	Ok(super::result(
		body,
		serde_json::json!({
			"path": display,
			"op": "update",
			"diff": diff,
			"firstChangedLine": first,
			"replacements": count,
			"warnings": warnings,
		}),
	))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn replace_text(
	content: &str,
	old_text: &str,
	new_text: &str,
	all: bool,
	allow_fuzzy: bool,
	threshold: f64,
	display_path: &str,
) -> Result<(String, usize, Vec<String>), ToolError> {
	if old_text.is_empty() {
		return Err(ToolError::InvalidArguments("old_string must not be empty.".to_string()));
	}
	let mut warnings = Vec::new();
	if all {
		if let Some(exact) = exact_occurrences(content, old_text)
			&& !exact.is_empty()
		{
			return Ok((content.replace(old_text, new_text), exact.len(), warnings));
		}
		let mut replacements: Vec<(usize, usize, String)> = Vec::new();
		loop {
			let excluded: Vec<(usize, usize)> = replacements.iter().map(|(start, end, _)| (*start, *end)).collect();
			let outcome = find_match(content, old_text, allow_fuzzy, threshold, &excluded);
			let use_closest = allow_fuzzy
				&& outcome.closest.as_ref().is_some_and(|closest| {
					closest.confidence >= threshold && outcome.fuzzy_matches <= 1
				});
			let Some(found) = outcome.matched.or_else(|| use_closest.then(|| outcome.closest.clone()).flatten())
			else {
				break;
			};
			let adjusted = adjust_indentation(old_text, &found.actual_text, new_text);
			if adjusted == found.actual_text {
				break;
			}
			replacements.push((found.start_index, found.start_index + found.actual_text.len(), adjusted));
		}
		replacements.sort_by_key(|(start, _, _)| *start);
		let mut out = String::with_capacity(content.len());
		let mut cursor = 0;
		for (start, end, replacement) in &replacements {
			out.push_str(&content[cursor..*start]);
			out.push_str(replacement);
			cursor = *end;
		}
		out.push_str(&content[cursor..]);
		let count = replacements.len();
		return Ok((out, count, warnings));
	}

	let outcome = find_match(content, old_text, allow_fuzzy, threshold, &[]);
	if outcome.occurrences > 1 {
		return Err(ToolError::ExecutionFailed(occurrence_error(display_path, &outcome)));
	}
	let Some(found) = outcome.matched else {
		return Err(ToolError::ExecutionFailed(match_error(
			display_path,
			old_text,
			outcome.closest.as_ref(),
			allow_fuzzy,
			threshold,
			outcome.fuzzy_matches,
		)));
	};
	if outcome.dominant && found.confidence < 1.0 {
		warnings.push(format!(
			"Dominant fuzzy match selected in {display_path} near line {} ({}% similar).",
			found.start_line,
			(found.confidence * 100.0).round() as i64
		));
	}
	let adjusted = adjust_indentation(old_text, &found.actual_text, new_text);
	if adjusted == found.actual_text {
		return Err(ToolError::ExecutionFailed(format!(
			"Edits to {display_path} resulted in no changes being made."
		)));
	}
	let mut out = String::with_capacity(content.len());
	out.push_str(&content[..found.start_index]);
	out.push_str(&adjusted);
	out.push_str(&content[found.start_index + found.actual_text.len()..]);
	Ok((out, 1, warnings))
}

#[derive(Debug, Clone)]
pub(crate) struct FuzzyMatch {
	pub(crate) actual_text: String,
	pub(crate) start_index: usize,
	pub(crate) start_line: usize,
	pub(crate) confidence: f64,
}

#[derive(Debug, Default)]
pub(crate) struct MatchOutcome {
	pub(crate) occurrences: usize,
	pub(crate) occurrence_previews: Vec<String>,
	pub(crate) matched: Option<FuzzyMatch>,
	pub(crate) closest: Option<FuzzyMatch>,
	pub(crate) fuzzy_matches: usize,
	pub(crate) dominant: bool,
}

pub(crate) fn find_match(
	content: &str,
	target: &str,
	allow_fuzzy: bool,
	threshold: f64,
	excluded: &[(usize, usize)],
) -> MatchOutcome {
	if target.is_empty() {
		return MatchOutcome::default();
	}
	let exact = exact_offsets(content, target, excluded);
	if let Some(offsets) = exact {
		if offsets.len() > 1 {
			let lines: Vec<String> = content.split('\n').map(str::to_string).collect();
			let mut previews = Vec::new();
			for offset in offsets.iter().take(MAX_RECORDED_MATCHES) {
				let line = text::line_number_at(content, *offset);
				previews.push(text::preview_window(
					&lines,
					line.saturating_sub(1),
					OCCURRENCE_PREVIEW_CONTEXT,
					OCCURRENCE_PREVIEW_MAX_LEN,
				));
			}
			return MatchOutcome { occurrences: offsets.len(), occurrence_previews: previews, ..Default::default() };
		}
		return MatchOutcome {
			matched: Some(FuzzyMatch {
				actual_text: target.to_string(),
				start_index: offsets[0],
				start_line: text::line_number_at(content, offsets[0]),
				confidence: 1.0,
			}),
			occurrences: 1,
			..Default::default()
		};
	}

	let (best, above_threshold, second_best) = find_best_fuzzy(content, target, threshold, excluded);
	let Some(best) = best else { return MatchOutcome::default() };
	if allow_fuzzy && best.confidence >= threshold {
		if above_threshold == 1 {
			return MatchOutcome {
				matched: Some(best.clone()),
				closest: Some(best),
				occurrences: 1,
				fuzzy_matches: 1,
				..Default::default()
			};
		}
		if above_threshold > 1
			&& best.confidence >= DOMINANT_FUZZY_MIN_CONFIDENCE
			&& best.confidence - second_best >= DOMINANT_FUZZY_DELTA
		{
			return MatchOutcome {
				matched: Some(best.clone()),
				closest: Some(best),
				occurrences: 1,
				occurrence_previews: Vec::new(),
				fuzzy_matches: above_threshold,
				dominant: true,
			};
		}
	}
	MatchOutcome { closest: Some(best), fuzzy_matches: above_threshold, ..Default::default() }
}

fn exact_occurrences(content: &str, target: &str) -> Option<Vec<usize>> {
	exact_offsets(content, target, &[])
}

fn exact_offsets(content: &str, target: &str, excluded: &[(usize, usize)]) -> Option<Vec<usize>> {
	let mut offsets = Vec::new();
	let mut from = 0;
	while from <= content.len().saturating_sub(target.len()) {
		let Some(found) = content[from..].find(target) else { break };
		let start = from + found;
		let end = start + target.len();
		let blocked = excluded.iter().any(|(x, y)| start < *y && end > *x);
		if !blocked {
			offsets.push(start);
		}
		from = end;
	}
	if offsets.is_empty() { None } else { Some(offsets) }
}

fn find_best_fuzzy(
	content: &str,
	target: &str,
	threshold: f64,
	excluded: &[(usize, usize)],
) -> (Option<FuzzyMatch>, usize, f64) {
	let content_lines: Vec<String> = content.split('\n').map(str::to_string).collect();
	let target_lines: Vec<String> = target.split('\n').map(str::to_string).collect();
	if target_lines.is_empty() || target.is_empty() || target_lines.len() > content_lines.len() {
		return (None, 0, 0.0);
	}
	let offsets = text::line_offsets(&content_lines);
	let mut outcome = fuzzy_core(&content_lines, &target_lines, &offsets, threshold, true, excluded);
	if let Some(best) = &outcome.0
		&& best.confidence < threshold
		&& best.confidence >= FALLBACK_THRESHOLD
	{
		let no_depth = fuzzy_core(&content_lines, &target_lines, &offsets, threshold, false, excluded);
		if no_depth.0.as_ref().is_some_and(|candidate| candidate.confidence > best.confidence) {
			outcome = no_depth;
		}
	}
	outcome
}

fn fuzzy_core(
	content_lines: &[String],
	target_lines: &[String],
	offsets: &[usize],
	threshold: f64,
	include_depth: bool,
	excluded: &[(usize, usize)],
) -> (Option<FuzzyMatch>, usize, f64) {
	let target_normalized = normalize_lines(target_lines, include_depth);
	let mut best: Option<FuzzyMatch> = None;
	let mut best_score = -1.0;
	let mut second_best = -1.0;
	let mut above = 0usize;
	if target_lines.len() > content_lines.len() {
		return (None, 0, 0.0);
	}
	for start in 0..=content_lines.len() - target_lines.len() {
		let start_index = offsets[start];
		let end_line = start + target_lines.len() - 1;
		let end_index = (offsets[end_line] + content_lines[end_line].len()).max(start_index + 1);
		if excluded.iter().any(|(x, y)| start_index < *y && end_index > *x) {
			continue;
		}
		let window = &content_lines[start..start + target_lines.len()];
		let window_normalized = normalize_lines(window, include_depth);
		let score: f64 = target_normalized
			.iter()
			.zip(window_normalized.iter())
			.map(|(a, b)| text::similarity(a, b))
			.sum::<f64>()
			/ target_lines.len() as f64;
		if score >= threshold {
			above += 1;
		}
		if score > best_score {
			second_best = best_score;
			best_score = score;
			best = Some(FuzzyMatch {
				actual_text: window.join("\n"),
				start_index,
				start_line: start + 1,
				confidence: score,
			});
		} else if score > second_best {
			second_best = score;
		}
	}
	(best, above, second_best.max(0.0))
}

fn compute_relative_indent_depths(lines: &[String]) -> Vec<usize> {
	let indents: Vec<usize> = lines.iter().map(|line| text::leading_whitespace_len(line)).collect();
	let non_empty: Vec<usize> = lines
		.iter()
		.enumerate()
		.filter(|(_, line)| !line.trim().is_empty())
		.map(|(index, _)| indents[index])
		.collect();
	let min_indent = non_empty.iter().copied().min().unwrap_or(0);
	let steps: Vec<usize> = non_empty.iter().filter(|value| **value > min_indent).map(|value| value - min_indent).collect();
	let unit = steps.iter().copied().min().unwrap_or(1);
	lines
		.iter()
		.enumerate()
		.map(|(index, line)| {
			if line.trim().is_empty() || unit == 0 {
				return 0;
			}
			((indents[index] - min_indent) as f64 / unit as f64).round() as usize
		})
		.collect()
}

fn normalize_lines(lines: &[String], include_depth: bool) -> Vec<String> {
	let depths = include_depth.then(|| compute_relative_indent_depths(lines));
	lines
		.iter()
		.enumerate()
		.map(|(index, line)| {
			let prefix = depths.as_ref().map(|values| values[index].to_string()).unwrap_or_default();
			let trimmed = line.trim();
			if trimmed.is_empty() {
				return format!("{prefix}|");
			}
			format!("{prefix}|{}", text::normalize_for_fuzzy(trimmed))
		})
		.collect()
}

fn occurrence_error(path: &str, outcome: &MatchOutcome) -> String {
	let previews = outcome.occurrence_previews.join("\n\n");
	let more = if outcome.occurrences > MAX_RECORDED_MATCHES {
		format!(" (showing first {MAX_RECORDED_MATCHES} of {})", outcome.occurrences)
	} else {
		String::new()
	};
	format!(
		"Found {} occurrences in {path}{more}:\n\n{previews}\n\nAdd more context lines to disambiguate.",
		outcome.occurrences
	)
}

fn match_error(
	path: &str,
	search_text: &str,
	closest: Option<&FuzzyMatch>,
	allow_fuzzy: bool,
	threshold: f64,
	fuzzy_matches: usize,
) -> String {
	if !allow_fuzzy {
		return format!(
			"Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines."
		);
	}
	let Some(closest) = closest else {
		return format!("Could not find a close enough match in {path}.");
	};
	let similarity = (closest.confidence * 100.0).round() as i64;
	let search_lines: Vec<&str> = search_text.split('\n').collect();
	let actual_lines: Vec<&str> = closest.actual_text.split('\n').collect();
	let (old_line, new_line) = text::first_different_line(&search_lines, &actual_lines);
	let threshold_percent = (threshold * 100.0).round() as i64;
	let hint = if fuzzy_matches > 1 {
		format!("Found {fuzzy_matches} high-confidence matches. Provide more context to make it unique.")
	} else {
		format!("Closest match was below the {threshold_percent}% similarity threshold.")
	};
	[
		format!("Could not find a close enough match in {path}."),
		String::new(),
		format!("Closest match ({similarity}% similar) at line {}:", closest.start_line),
		format!("  - {old_line}"),
		format!("  + {new_line}"),
		hint,
	]
	.join("\n")
}

// ── Indentation adjustment ────────────────────────────────────────────────

struct IndentProfile {
	lines: Vec<String>,
	char: Option<char>,
	space_only: bool,
	tab_only: bool,
	mixed: bool,
	unit: usize,
	non_empty: usize,
}

fn build_profile(text: &str) -> IndentProfile {
	let lines: Vec<String> = text.split('\n').map(str::to_string).collect();
	let mut char: Option<char> = None;
	let mut space_only = true;
	let mut tab_only = true;
	let mut mixed = false;
	let mut non_empty = 0usize;
	let mut indent_counts = Vec::new();
	for line in &lines {
		if line.trim().is_empty() {
			continue;
		}
		non_empty += 1;
		let indent = text::leading_whitespace(line);
		indent_counts.push(indent.len());
		if indent.contains(' ') {
			tab_only = false;
		}
		if indent.contains('\t') {
			space_only = false;
		}
		if indent.contains(' ') && indent.contains('\t') {
			mixed = true;
		}
		if let Some(first) = indent.chars().next() {
			match char {
				None => char = Some(first),
				Some(existing) if existing != first => mixed = true,
				_ => {},
			}
		}
	}
	let mut unit = 0usize;
	if space_only && non_empty > 0 {
		let mut current = 0usize;
		for count in indent_counts {
			if count == 0 {
				continue;
			}
			current = if current == 0 { count } else { gcd(current, count) };
		}
		unit = current;
	}
	if tab_only && non_empty > 0 {
		unit = 1;
	}
	IndentProfile { lines, char, space_only, tab_only, mixed, unit, non_empty }
}

fn gcd(mut a: usize, mut b: usize) -> usize {
	while b != 0 {
		let next = a % b;
		a = b;
		b = next;
	}
	a
}

fn is_indentation_only_rewrite(old: &str, new: &str) -> bool {
	let old_lines: Vec<&str> = old.split('\n').collect();
	let new_lines: Vec<&str> = new.split('\n').collect();
	old_lines.len() == new_lines.len()
		&& old_lines.iter().zip(new_lines.iter()).all(|(a, b)| a.trim() == b.trim())
}

fn convert_leading_tabs_to_spaces(text: &str, spaces_per_tab: usize) -> String {
	text.split('\n')
		.map(|line| {
			let trimmed = line.trim_start();
			if trimmed.is_empty() {
				return line.to_string();
			}
			let indent = text::leading_whitespace(line);
			if !indent.contains('\t') || indent.contains(' ') {
				return line.to_string();
			}
			format!("{}{trimmed}", " ".repeat(indent.len() * spaces_per_tab))
		})
		.collect::<Vec<_>>()
		.join("\n")
}

pub(crate) fn adjust_indentation(old_text: &str, actual_text: &str, new_text: &str) -> String {
	if old_text == actual_text || is_indentation_only_rewrite(old_text, new_text) {
		return new_text.to_string();
	}
	let old_profile = build_profile(old_text);
	let actual_profile = build_profile(actual_text);
	let new_profile = build_profile(new_text);
	if old_profile.non_empty == 0 || actual_profile.non_empty == 0 || new_profile.non_empty == 0 {
		return new_text.to_string();
	}
	if old_profile.mixed || actual_profile.mixed || new_profile.mixed {
		return new_text.to_string();
	}
	if let (Some(old_char), Some(actual_char)) = (old_profile.char, actual_profile.char)
		&& old_char != actual_char
	{
		if actual_profile.space_only
			&& old_profile.tab_only
			&& new_profile.tab_only
			&& actual_profile.unit > 0
			&& indents_scale(&old_profile.lines, &actual_profile.lines, actual_profile.unit)
		{
			return convert_leading_tabs_to_spaces(new_text, actual_profile.unit);
		}
		return new_text.to_string();
	}
	let Some(delta) = uniform_indent_delta(&old_profile.lines, &actual_profile.lines) else {
		return new_text.to_string();
	};
	if delta == 0 {
		return new_text.to_string();
	}
	if let (Some(new_char), Some(actual_char)) = (new_profile.char, actual_profile.char)
		&& new_char != actual_char
	{
		return new_text.to_string();
	}
	let indent_char = actual_profile
		.char
		.or(old_profile.char)
		.or_else(|| actual_text.chars().find(|ch| *ch == ' ' || *ch == '\t'))
		.unwrap_or(' ');
	let lines: Vec<String> = new_text.split('\n').map(str::to_string).collect();
	text::apply_indent_delta(&lines, delta, indent_char).join("\n")
}

fn indents_scale(old_lines: &[String], actual_lines: &[String], unit: usize) -> bool {
	for (old_line, actual_line) in old_lines.iter().zip(actual_lines.iter()) {
		if old_line.trim().is_empty() || actual_line.trim().is_empty() {
			continue;
		}
		let old_indent = text::leading_whitespace_len(old_line);
		if old_indent == 0 {
			continue;
		}
		if text::leading_whitespace_len(actual_line) != old_indent * unit {
			return false;
		}
	}
	true
}

fn uniform_indent_delta(old_lines: &[String], actual_lines: &[String]) -> Option<i32> {
	let mut deltas = Vec::new();
	for (old_line, actual_line) in old_lines.iter().zip(actual_lines.iter()) {
		if old_line.trim().is_empty() || actual_line.trim().is_empty() {
			continue;
		}
		deltas.push(text::leading_whitespace_len(actual_line) as i32 - text::leading_whitespace_len(old_line) as i32);
	}
	let first = *deltas.first()?;
	deltas.iter().all(|delta| *delta == first).then_some(first)
}
