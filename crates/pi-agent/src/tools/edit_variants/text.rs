//! Text primitives shared by the edit engines: line-ending/BOM round-trip,
//! fuzzy normalization, Levenshtein similarity, indentation adjustment, and
//! preview/diff formatting.

use std::collections::HashMap;

use super::super::hashline::hash::{
	LineEnding, detect_line_ending, normalize_to_lf, restore_line_endings, strip_bom,
};

pub use crate::tools::hashline::hash::{detect_line_ending as detect_eol, normalize_to_lf as to_lf};
pub use crate::tools::hashline::hash::{restore_line_endings as restore_eol, strip_bom as split_bom};

/// Default fuzzy similarity threshold (`edit.fuzzyThreshold`).
pub const DEFAULT_FUZZY_THRESHOLD: f64 = 0.95;

/// Count leading space/tab characters.
pub fn leading_whitespace_len(line: &str) -> usize {
	line.chars().take_while(|ch| *ch == ' ' || *ch == '\t').count()
}

pub fn leading_whitespace(line: &str) -> &str {
	&line[..leading_whitespace_len(line)]
}

pub fn is_blank(line: &str) -> bool {
	line.trim().is_empty()
}

/// Normalize a line for fuzzy comparison: trim, unify quote/dash punctuation,
/// collapse runs of spaces/tabs.
pub fn normalize_for_fuzzy(line: &str) -> String {
	let trimmed = line.trim();
	if trimmed.is_empty() {
		return String::new();
	}
	let mut out = String::with_capacity(trimmed.len());
	let mut last_space = false;
	for ch in trimmed.chars() {
		let mapped = match ch {
			'\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{201f}' | '\u{00ab}' | '\u{00bb}' => '"',
			'\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' | '`' | '\u{00b4}' => '\'',
			'\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
			' ' | '\t' => ' ',
			other => other,
		};
		if mapped == ' ' {
			if last_space {
				continue;
			}
			last_space = true;
		} else {
			last_space = false;
		}
		out.push(mapped);
	}
	out
}

/// Unicode punctuation normalization used by the sequence matcher.
pub fn normalize_unicode(text: &str) -> String {
	let trimmed = text.trim();
	if trimmed.is_ascii() {
		return trimmed.to_string();
	}
	let mut out = String::with_capacity(trimmed.len());
	for ch in trimmed.chars() {
		match ch {
			'\u{2010}'..='\u{2015}' | '\u{2212}' => out.push('-'),
			'\u{2018}'..='\u{201b}' => out.push('\''),
			'\u{201c}'..='\u{201f}' => out.push('"'),
			'\u{00a0}' | '\u{2002}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => out.push(' '),
			'\u{2260}' => out.push_str("!="),
			'\u{00bd}' => out.push_str("1/2"),
			'\u{feff}' => {},
			other => out.push(other),
		}
	}
	out
}

/// Levenshtein distance over chars, with common prefix/suffix trimming.
pub fn levenshtein_distance(a: &str, b: &str) -> usize {
	if a == b {
		return 0;
	}
	let a_chars: Vec<char> = a.chars().collect();
	let b_chars: Vec<char> = b.chars().collect();
	let mut start = 0;
	let shared = a_chars.len().min(b_chars.len());
	while start < shared && a_chars[start] == b_chars[start] {
		start += 1;
	}
	let mut a_end = a_chars.len();
	let mut b_end = b_chars.len();
	while a_end > start && b_end > start && a_chars[a_end - 1] == b_chars[b_end - 1] {
		a_end -= 1;
		b_end -= 1;
	}
	let a_slice = &a_chars[start..a_end];
	let b_slice = &b_chars[start..b_end];
	if a_slice.is_empty() {
		return b_slice.len();
	}
	if b_slice.is_empty() {
		return a_slice.len();
	}
	let (rows, cols) = if a_slice.len() >= b_slice.len() {
		(a_slice, b_slice)
	} else {
		(b_slice, a_slice)
	};
	let mut row: Vec<usize> = (0..=cols.len()).collect();
	for (line, a_ch) in rows.iter().enumerate() {
		let mut diagonal = row[0];
		row[0] = line + 1;
		for (column, b_ch) in cols.iter().enumerate() {
			let above = row[column + 1];
			row[column + 1] = if a_ch == b_ch {
				diagonal
			} else {
				(above + 1).min(row[column] + 1).min(diagonal + 1)
			};
			diagonal = above;
		}
	}
	row[cols.len()]
}

/// Similarity in `0.0..=1.0`.
pub fn similarity(a: &str, b: &str) -> f64 {
	if a.is_empty() && b.is_empty() {
		return 1.0;
	}
	let max_len = a.chars().count().max(b.chars().count());
	if max_len == 0 {
		return 1.0;
	}
	1.0 - levenshtein_distance(a, b) as f64 / max_len as f64
}

/// Character offsets of each line's start.
pub fn line_offsets(lines: &[String]) -> Vec<usize> {
	let mut offsets = Vec::with_capacity(lines.len());
	let mut offset = 0usize;
	for (index, line) in lines.iter().enumerate() {
		offsets.push(offset);
		offset += line.len();
		if index + 1 < lines.len() {
			offset += 1;
		}
	}
	offsets
}

/// 1-based line number containing `offset`.
pub fn line_number_at(content: &str, offset: usize) -> usize {
	content[..offset.min(content.len())].bytes().filter(|byte| *byte == b'\n').count() + 1
}

/// Numbered preview around `offset`, matching upstream `numberedPreview`.
pub fn numbered_preview(content: &str, offset: usize) -> String {
	let lines: Vec<&str> = content.split('\n').collect();
	let center = line_number_at(content, offset).saturating_sub(1);
	let start = center.saturating_sub(3);
	let end = (center + 4).min(lines.len());
	let mut out = String::new();
	for (index, line) in lines[start..end].iter().enumerate() {
		let number = start + index + 1;
		let truncated: String = line.chars().take(200).collect();
		out.push_str(&format!("{number:>5} | {truncated}\n"));
	}
	out.trim_end().to_string()
}

/// Preview window used by occurrence/ambiguity diagnostics.
pub fn preview_window(lines: &[String], center: usize, context: usize, max_len: usize) -> String {
	let start = center.saturating_sub(context);
	let end = (center + context + 1).min(lines.len());
	lines[start..end]
		.iter()
		.enumerate()
		.map(|(index, line)| {
			let number = start + index + 1;
			let truncated: String = if line.chars().count() > max_len {
				let mut text: String = line.chars().take(max_len.saturating_sub(1)).collect();
				text.push('…');
				text
			} else {
				line.clone()
			};
			format!("  {number} | {truncated}")
		})
		.collect::<Vec<_>>()
		.join("\n")
}

/// Unified diff between two LF-normalized texts (3 lines of context).
pub fn unified_diff(path: &str, old: &str, new: &str) -> String {
	similar::TextDiff::from_lines(old, new)
		.unified_diff()
		.context_radius(3)
		.header(&format!("a/{path}"), &format!("b/{path}"))
		.to_string()
}

/// First changed line (1-based) between two texts, if any.
pub fn first_changed_line(old: &str, new: &str) -> Option<usize> {
	let old_lines: Vec<&str> = old.split('\n').collect();
	let new_lines: Vec<&str> = new.split('\n').collect();
	for index in 0..old_lines.len().max(new_lines.len()) {
		if old_lines.get(index) != new_lines.get(index) {
			return Some(index + 1);
		}
	}
	None
}

/// Apply a uniform indentation delta to every non-blank line.
pub fn apply_indent_delta(lines: &[String], delta: i32, indent_char: char) -> Vec<String> {
	lines
		.iter()
		.map(|line| {
			if is_blank(line) {
				return line.clone();
			}
			if delta > 0 {
				let prefix: String = std::iter::repeat_n(indent_char, delta as usize).collect();
				format!("{prefix}{line}")
			} else {
				let remove = (-delta) as usize;
				let count = leading_whitespace_len(line).min(remove);
				line[count..].to_string()
			}
		})
		.collect()
}

/// Compute the first index where two line sequences differ.
pub fn first_different_line(old: &[&str], new: &[&str]) -> (String, String) {
	let max = old.len().max(new.len());
	for index in 0..max {
		let old_line = old.get(index).copied().unwrap_or("");
		let new_line = new.get(index).copied().unwrap_or("");
		if old_line != new_line {
			return (old_line.to_string(), new_line.to_string());
		}
	}
	(old.first().copied().unwrap_or("").to_string(), new.first().copied().unwrap_or("").to_string())
}

/// Deduplicate repeated blocks for patch fallback variants.
pub fn collapse_repeated_blocks(old_lines: &[String], new_lines: &[String]) -> Option<(Vec<String>, Vec<String>)> {
	let new_set: std::collections::HashSet<&String> = new_lines.iter().collect();
	let shared: std::collections::HashSet<&String> = old_lines.iter().filter(|line| new_set.contains(line)).collect();
	let collapse = |lines: &[String]| -> Option<Vec<String>> {
		let mut output = lines.to_vec();
		let mut changed = false;
		let mut index = 0;
		while index < output.len() {
			let mut collapsed = false;
			if shared.contains(&output[index]) {
				let mut size = (output.len() - index) / 2;
				while size >= 2 {
					let same = (0..size).all(|offset| {
						output[index + offset] == output[index + size + offset]
							&& shared.contains(&output[index + offset])
					});
					if same {
						output.drain(index + size..index + size * 2);
						changed = true;
						collapsed = true;
						break;
					}
					size -= 1;
				}
			}
			if !collapsed {
				index += 1;
			}
		}
		changed.then_some(output)
	};
	let collapsed_old = collapse(old_lines);
	let collapsed_new = collapse(new_lines);
	if collapsed_old.is_none() && collapsed_new.is_none() {
		return None;
	}
	Some((
		collapsed_old.unwrap_or_else(|| old_lines.to_vec()),
		collapsed_new.unwrap_or_else(|| new_lines.to_vec()),
	))
}

/// Map trimmed line content back to the actual file line (used when applying
/// a fuzzy-matched hunk so untouched lines keep their real indentation).
pub fn realign_context_lines(
	pattern_lines: &[String],
	actual_lines: &[String],
	new_lines: &[String],
	pattern_min: usize,
	delta: Option<i32>,
	indent_char: char,
) -> Vec<String> {
	let mut by_trimmed: HashMap<&str, Vec<&String>> = HashMap::new();
	for line in actual_lines {
		let trimmed = line.trim();
		if trimmed.is_empty() {
			continue;
		}
		by_trimmed.entry(trimmed).or_default().push(line);
	}
	let mut used: HashMap<&str, usize> = HashMap::new();
	new_lines
		.iter()
		.map(|new_line| {
			if is_blank(new_line) {
				return new_line.clone();
			}
			let trimmed = new_line.trim();
			if let Some(matches) = by_trimmed.get(trimmed) {
				if matches.len() == 1 {
					return matches[0].clone();
				}
				if matches.iter().any(|line| *line == new_line) {
					return new_line.clone();
				}
				let used_count = used.entry(trimmed).or_insert(0);
				if *used_count < matches.len() {
					let line = matches[*used_count].clone();
					*used_count += 1;
					return line;
				}
			}
			if let Some(delta) = delta
				&& delta != 0
				&& leading_whitespace_len(new_line) == pattern_min
			{
				return apply_indent_delta(std::slice::from_ref(new_line), delta, indent_char)
					.pop()
					.unwrap_or_else(|| new_line.clone());
			}
			new_line.clone()
		})
		.collect()
}

/// `stripCommentPrefix` used by the sequence matcher.
pub fn strip_comment_prefix(line: &str) -> String {
	let mut trimmed = line.trim_start();
	for prefix in ["/*", "*/", "//", "*", "#", ";"] {
		if let Some(rest) = trimmed.strip_prefix(prefix) {
			trimmed = rest;
			break;
		}
	}
	if trimmed.starts_with("/ ") {
		trimmed = &trimmed[1..];
	}
	trimmed.trim_start().to_string()
}

/// Re-export so engines share one line-ending type.
pub type Eol = LineEnding;

/// Split BOM + LF-normalized text and its dominant ending.
pub fn normalize_file(raw: &str) -> (String, String, Eol) {
	let (bom, text) = strip_bom(raw);
	let ending = detect_line_ending(text);
	(bom.to_string(), normalize_to_lf(text), ending)
}

/// Reassemble BOM + original line endings.
pub fn denormalize_file(bom: &str, text: &str, ending: Eol) -> String {
	format!("{bom}{}", restore_line_endings(text, ending))
}
