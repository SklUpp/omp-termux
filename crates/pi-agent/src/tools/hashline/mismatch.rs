//! Stale-tag rejection with current context, mirroring upstream `mismatch.ts`.

use super::hash::HASH_LENGTH;

/// Lines of surrounding context on each side of an anchored line.
pub const MISMATCH_CONTEXT: u32 = 2;

fn numbered(line: u32, text: &str) -> String {
	format!("{line}:{text}")
}

/// Numbered `LINE:TEXT` rows around `anchor_lines` (±[`MISMATCH_CONTEXT`]),
/// `*`-marking anchors, `...` between non-adjacent runs.
pub fn format_anchored_context(anchor_lines: &[u32], file_lines: &[&str]) -> Vec<String> {
	let count = file_lines.len() as u32;
	let mut display = std::collections::BTreeSet::new();
	for line in anchor_lines {
		if *line < 1 || *line > count {
			continue;
		}
		let lo = (*line).saturating_sub(MISMATCH_CONTEXT).max(1);
		let hi = (*line + MISMATCH_CONTEXT).min(count);
		for line_num in lo..=hi {
			display.insert(line_num);
		}
	}
	let anchors: std::collections::BTreeSet<u32> = anchor_lines.iter().copied().collect();
	let mut rows = Vec::new();
	let mut previous: Option<u32> = None;
	for line_num in display {
		if previous.is_some_and(|previous| line_num > previous + 1) {
			rows.push("...".to_string());
		}
		previous = Some(line_num);
		let marker = if anchors.contains(&line_num) { "*" } else { " " };
		let text = file_lines.get((line_num - 1) as usize).copied().unwrap_or("");
		rows.push(format!("{marker}{}", numbered(line_num, text)));
	}
	rows
}

/// Full rejection message: header lines plus anchored context rows.
pub fn mismatch_message(
	path: &str,
	expected: &str,
	actual: &str,
	hash_recognized: bool,
	anchor_lines: &[u32],
	file_lines: &[&str],
) -> String {
	let header: Vec<String> = if hash_recognized {
		vec![
			format!("Edit rejected for {path}: file changed between read and edit."),
			format!(
				"Section is bound to #{expected}, but the current file hashes to #{actual}. \
				 If a prior edit in this session modified this file, copy the `[path#newhash]` header \
				 from that edit's response; otherwise re-read the file with `read` to refresh the tag \
				 before retrying."
			),
		]
	} else {
		vec![
			format!("Edit rejected for {path}: hash #{expected} is not from this session."),
			format!(
				"The current file hashes to #{actual}. Re-read the file with `read` to copy a current \
				 `[path#tag]` header — never invent the tag and never reuse one from a prior session."
			),
		]
	};
	let context = format_anchored_context(anchor_lines, file_lines);
	let mut message = header.join("\n");
	if !context.is_empty() {
		message.push_str("\n\n");
		message.push_str(&context.join("\n"));
	}
	message
}

/// Four hex characters, for callers formatting tag examples in diagnostics.
pub const TAG_LENGTH: usize = HASH_LENGTH;
