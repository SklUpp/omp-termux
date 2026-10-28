//! Pure application of parsed hashline edits to file text.
//!
//! Every line number indexes the original snapshot: hunks are bucketed by
//! anchor line and spliced bottom-up so earlier indices stay valid. No I/O, no
//! syntax heuristics — the edited text is exactly what the author wrote.

use std::collections::BTreeMap;

use super::{
	budget::Budget,
	clipboard::{self, Clipboard},
	parser::{Cursor, Edit},
};

/// Result of applying edits to one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyResult {
	pub text: String,
	pub first_changed_line: Option<u32>,
	pub warnings: Vec<String>,
}

/// Apply `edits` to `text`. `clipboard` carries registers across sections of
/// one batch.
pub fn apply_edits(text: &str, edits: &[Edit], clipboard: &mut Clipboard) -> Result<ApplyResult, String> {
	let mut budget = Budget::default();
	apply_edits_checked(text, edits, clipboard, None, &mut budget)
}

/// Sentinel error string: the caller's cancellation token fired mid-apply.
pub const APPLY_CANCELLED: &str = "edit cancelled";

/// Like [`apply_edits`], with periodic cancellation checkpoints in the splice
/// loops so a large but legal batch cannot run to completion after a cancel.
pub fn apply_edits_checked(
	text: &str,
	edits: &[Edit],
	clipboard: &mut Clipboard,
	token: Option<&tokio_util::sync::CancellationToken>,
	budget: &mut Budget,
) -> Result<ApplyResult, String> {
	if token.is_some_and(|token| token.is_cancelled()) {
		return Err(APPLY_CANCELLED.to_string());
	}
	if edits.is_empty() {
		return Ok(ApplyResult { text: text.to_string(), first_changed_line: None, warnings: Vec::new() });
	}
	// Pay for the line vector before splitting, and for cloned payloads before
	// the splice builds them. Each split line costs a `String` header too.
	let line_slots = text.as_bytes().iter().filter(|byte| **byte == b'\n').count() + 1;
	budget.charge_bytes(text.len() + line_slots * std::mem::size_of::<String>())?;
	let original: Vec<String> = text.split('\n').map(str::to_string).collect();
	let mut warnings = Vec::new();
	let concrete = clipboard::resolve(edits, &original, clipboard, &mut warnings, budget)?;
	budget.charge_bytes(super::parser::payload_bytes(&concrete))?;
	for edit in &concrete {
		if matches!(edit, Edit::Block { .. }) {
			return Err(
				"internal error: block edit reached the applier; resolve block anchors against file text first."
					.to_string(),
			);
		}
	}
	let applied = drop_trailing_phantom_deletes(concrete, &original);
	validate_line_bounds(&applied, &original)?;
	let (text, first_changed_line) = materialize(&original, &applied, token)?;
	Ok(ApplyResult { text, first_changed_line, warnings })
}

fn trailing_phantom_line(file_lines: &[String]) -> usize {
	// A newline-terminated file splits into a trailing "" sentinel. It is
	// addressable for inserts (append past the last line) but is not content,
	// so a delete there only strips the final newline: ignore it.
	if file_lines.len() > 1 && file_lines[file_lines.len() - 1].is_empty() {
		file_lines.len()
	} else {
		0
	}
}

fn drop_trailing_phantom_deletes(edits: Vec<Edit>, file_lines: &[String]) -> Vec<Edit> {
	let phantom = trailing_phantom_line(file_lines);
	if phantom == 0 {
		return edits;
	}
	edits
		.into_iter()
		.filter(|edit| !matches!(edit, Edit::Delete { line, .. } if *line as usize == phantom))
		.collect()
}

fn validate_line_bounds(edits: &[Edit], file_lines: &[String]) -> Result<(), String> {
	let count = file_lines.len();
	for edit in edits {
		let anchor = match edit {
			Edit::Delete { line, .. } => Some(*line),
			Edit::Insert { cursor, .. } => match cursor {
				Cursor::Before(line) | Cursor::After(line) => Some(*line),
				Cursor::Bof | Cursor::Eof => None,
			},
			_ => None,
		};
		if let Some(line) = anchor {
			if line < 1 || line as usize > count {
				return Err(format!("Line {line} does not exist (file has {count} lines)"));
			}
		}
	}
	Ok(())
}

fn insert_at_start(file_lines: &mut Vec<String>, lines: Vec<String>) {
	if lines.is_empty() {
		return;
	}
	if file_lines.len() == 1 && file_lines[0].is_empty() {
		*file_lines = lines;
		return;
	}
	file_lines.splice(0..0, lines);
}

fn insert_at_end(file_lines: &mut Vec<String>, lines: Vec<String>) -> Option<u32> {
	if lines.is_empty() {
		return None;
	}
	if file_lines.len() == 1 && file_lines[0].is_empty() {
		*file_lines = lines;
		return Some(1);
	}
	let has_trailing_newline = file_lines.last().is_some_and(String::is_empty);
	let insert_index = if has_trailing_newline {
		file_lines.len() - 1
	} else {
		file_lines.len()
	};
	file_lines.splice(insert_index..insert_index, lines);
	Some((insert_index + 1) as u32)
}

fn track_first_changed(line: u32, first_changed: &mut Option<u32>) {
	if first_changed.is_none_or(|current| line < current) {
		*first_changed = Some(line);
	}
}

fn materialize(
	original: &[String],
	edits: &[Edit],
	token: Option<&tokio_util::sync::CancellationToken>,
) -> Result<(String, Option<u32>), String> {
	let mut file_lines = original.to_vec();
	let mut first_changed: Option<u32> = None;

	let mut bof_lines = Vec::new();
	let mut eof_lines = Vec::new();
	let mut buckets: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
	for (index, edit) in edits.iter().enumerate() {
		match edit {
			Edit::Insert { cursor: Cursor::Bof, text, .. } => bof_lines.push(text.clone()),
			Edit::Insert { cursor: Cursor::Eof, text, .. } => eof_lines.push(text.clone()),
			_ => {
				let anchor = match edit {
					Edit::Delete { line, .. } => Some(*line),
					Edit::Insert { cursor: Cursor::Before(line) | Cursor::After(line), .. } => Some(*line),
					_ => None,
				};
				if let Some(line) = anchor {
					buckets.entry(line).or_default().push(index);
				}
			}
		}
	}

	let keys: Vec<u32> = buckets.keys().copied().rev().collect();
	for (index, line) in keys.into_iter().enumerate() {
		if index % 1024 == 0 && token.is_some_and(|token| token.is_cancelled()) {
			return Err(APPLY_CANCELLED.to_string());
		}
		let Some(bucket) = buckets.get(&line) else { continue };
		let index = (line - 1) as usize;
		let current = file_lines.get(index).cloned().unwrap_or_default();
		let mut before = Vec::new();
		let mut replacement = Vec::new();
		let mut after = Vec::new();
		let mut delete_line = false;
		for edit_index in bucket {
			match &edits[*edit_index] {
				Edit::Insert { cursor: Cursor::Before(_), text, replacement: true, .. } => {
					replacement.push(text.clone());
				}
				Edit::Insert { cursor: Cursor::Before(_), text, .. } => before.push(text.clone()),
				Edit::Insert { cursor: Cursor::After(_), text, .. } => after.push(text.clone()),
				Edit::Delete { .. } => delete_line = true,
				_ => {}
			}
		}
		if before.is_empty() && replacement.is_empty() && after.is_empty() && !delete_line {
			continue;
		}
		let mut spliced = Vec::with_capacity(before.len() + replacement.len() + after.len() + 1);
		spliced.append(&mut before);
		spliced.append(&mut replacement);
		if !delete_line {
			spliced.push(current);
		}
		spliced.append(&mut after);
		file_lines.splice(index..index + 1, spliced);
		track_first_changed(line, &mut first_changed);
	}

	if !bof_lines.is_empty() {
		insert_at_start(&mut file_lines, bof_lines);
		track_first_changed(1, &mut first_changed);
	}
	if let Some(line) = insert_at_end(&mut file_lines, eof_lines) {
		track_first_changed(line, &mut first_changed);
	}

	Ok((file_lines.join("\n"), first_changed))
}
