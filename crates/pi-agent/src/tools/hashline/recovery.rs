//! Stale-tag recovery by remapping anchors through unchanged lines.
//!
//! Mirrors upstream `recovery.ts`: every anchored line must map to one
//! unchanged, contiguous region of the live file and all anchors must move by a
//! single offset; then the edit is replayed on live content. Anything changed,
//! split or ambiguous fails closed so the caller can surface a mismatch.

use std::collections::{BTreeSet, HashMap, HashSet};

use similar::{DiffOp, TextDiff};

use super::{
	apply,
	budget::Budget,
	clipboard::Clipboard,
	parser::{Cursor, Edit, PasteAt},
	snapshot::SnapshotStore,
};

const RECOVERY_EXTERNAL_WARNING: &str =
	"Recovered from a stale file hash using a previous read snapshot (file changed externally between read and edit).";
const RECOVERY_SESSION_CHAIN_WARNING: &str =
	"Recovered from a stale file hash using an earlier in-session snapshot (a prior edit in this session advanced the hash).";
pub const RECOVERY_LINE_REMAP_WARNING: &str =
	"Recovered by remapping stale line anchors to unchanged current lines (file changed since the tagged read). Verify the diff matches your intent.";

pub struct RecoveryResult {
	pub text: String,
	pub first_changed: Option<u32>,
	pub warnings: Vec<String>,
}

fn edit_anchors(edit: &Edit) -> Vec<u32> {
	match edit {
		Edit::Delete { line, .. } => vec![*line],
		Edit::Block { anchor, .. } => vec![*anchor],
		Edit::Cut { range, .. } => (range.start..=range.end).collect(),
		Edit::Paste { at, .. } => match at {
			PasteAt::Span(range) => (range.start..=range.end).collect(),
			PasteAt::Gap(Cursor::Before(line) | Cursor::After(line)) => vec![*line],
			PasteAt::Gap(_) => Vec::new(),
		},
		Edit::Insert { cursor, .. } => match cursor {
			Cursor::Before(line) | Cursor::After(line) => vec![*line],
			_ => Vec::new(),
		},
	}
}

fn collect_anchor_lines(edits: &[Edit]) -> Vec<u32> {
	edits.iter().flat_map(edit_anchors).collect()
}

/// Previous-line → current-line map over lines the diff calls equal.
fn build_line_map(previous: &str, current: &str) -> HashMap<u32, u32> {
	let diff = TextDiff::from_lines(previous, current);
	let mut map = HashMap::new();
	for op in diff.ops() {
		if let DiffOp::Equal { old_index, new_index, len } = op {
			for offset in 0..*len {
				map.insert((*old_index + offset + 1) as u32, (*new_index + offset + 1) as u32);
			}
		}
	}
	map
}

fn duplicated_values(lines: &[&str]) -> HashSet<String> {
	let mut seen = HashSet::new();
	let mut duplicated = HashSet::new();
	for line in lines {
		if !seen.insert(line.to_string()) {
			duplicated.insert(line.to_string());
		}
	}
	duplicated
}

#[derive(Clone, Copy)]
struct AnchorNeighbors {
	before: Option<u32>,
	after: Option<u32>,
}

fn compute_anchor_neighbors(anchor_lines: &BTreeSet<u32>, line_count: u32) -> HashMap<u32, AnchorNeighbors> {
	let sorted: Vec<u32> = anchor_lines.iter().copied().collect();
	let mut neighbors = HashMap::new();
	let mut index = 0;
	while index < sorted.len() {
		let mut end = index;
		while end + 1 < sorted.len() && sorted[end + 1] == sorted[end] + 1 {
			end += 1;
		}
		let start = sorted[index];
		let last = sorted[end];
		let before = (start > 1 && start - 1 <= line_count).then(|| start - 1);
		let after = (last + 1 <= line_count).then(|| last + 1);
		for line in &sorted[index..=end] {
			neighbors.insert(*line, AnchorNeighbors { before, after });
		}
		index = end + 1;
	}
	neighbors
}

fn validate_duplicate_context(
	line: u32,
	mapped: u32,
	neighbors: AnchorNeighbors,
	line_map: &HashMap<u32, u32>,
) -> bool {
	let mut checked = false;
	if let Some(before) = neighbors.before {
		checked = true;
		if line_map.get(&before).copied() != Some(mapped - (line - before)) {
			return false;
		}
	}
	if let Some(after) = neighbors.after {
		checked = true;
		if line_map.get(&after).copied() != Some(mapped + (after - line)) {
			return false;
		}
	}
	checked
}

fn validate_unique_context(
	line: u32,
	mapped: u32,
	neighbors: AnchorNeighbors,
	line_map: &HashMap<u32, u32>,
) -> bool {
	let offset = mapped as i64 - line as i64;
	if let Some(after) = neighbors.after {
		if line_map.get(&after).copied() == Some((after as i64 + offset) as u32) {
			return true;
		}
	}
	match neighbors.before {
		Some(before) => line_map.get(&before).copied() == Some((before as i64 + offset) as u32),
		None => false,
	}
}

fn validate_remapped_context(
	previous: &str,
	current: &str,
	line_map: &HashMap<u32, u32>,
	edits: &[Edit],
) -> bool {
	let previous_lines: Vec<&str> = previous.split('\n').collect();
	let current_lines: Vec<&str> = current.split('\n').collect();
	let anchor_lines: BTreeSet<u32> = collect_anchor_lines(edits).into_iter().collect();
	let duplicated_previous = duplicated_values(&previous_lines);
	let duplicated_current = duplicated_values(&current_lines);
	let neighbors = compute_anchor_neighbors(&anchor_lines, previous_lines.len() as u32);

	for (line, anchors) in &neighbors {
		let Some(mapped) = line_map.get(line).copied() else { return false };
		let previous_text = previous_lines.get((line - 1) as usize).copied().unwrap_or("");
		let current_text = current_lines.get((mapped - 1) as usize).copied().unwrap_or("");
		if !duplicated_previous.contains(previous_text) && !duplicated_current.contains(current_text) {
			if !validate_unique_context(*line, mapped, *anchors, line_map) {
				return false;
			}
			continue;
		}
		if !validate_duplicate_context(*line, mapped, *anchors, line_map) {
			return false;
		}
	}
	true
}

fn map_line(line_map: &HashMap<u32, u32>, line: u32, offsets: &mut Vec<i64>) -> Option<u32> {
	let mapped = line_map.get(&line).copied()?;
	offsets.push(mapped as i64 - line as i64);
	Some(mapped)
}

fn remap_edits(previous: &str, current: &str, edits: &[Edit]) -> Option<(Vec<Edit>, i64)> {
	let line_map = build_line_map(previous, current);
	if !validate_remapped_context(previous, current, &line_map, edits) {
		return None;
	}
	let mut offsets: Vec<i64> = Vec::new();
	let mut remapped = Vec::with_capacity(edits.len());

	for edit in edits {
		let mapped = match edit {
			Edit::Delete { line, line_num } => {
				let mapped = map_line(&line_map, *line, &mut offsets)?;
				Edit::Delete { line: mapped, line_num: *line_num }
			}
			Edit::Block { anchor, payloads, mode, register, line_num } => {
				let mapped = map_line(&line_map, *anchor, &mut offsets)?;
				Edit::Block {
					anchor: mapped,
					payloads: payloads.clone(),
					mode: *mode,
					register: register.clone(),
					line_num: *line_num,
				}
			}
			Edit::Cut { range, register, line_num } => {
				let start = map_line(&line_map, range.start, &mut offsets)?;
				let mut end = start;
				for line in range.start + 1..=range.end {
					end = map_line(&line_map, line, &mut offsets)?;
				}
				Edit::Cut { range: super::parser::Range { start, end }, register: register.clone(), line_num: *line_num }
			}
			Edit::Paste { at, register, line_num } => match at {
				PasteAt::Span(range) => {
					let start = map_line(&line_map, range.start, &mut offsets)?;
					let mut end = start;
					for line in range.start + 1..=range.end {
						end = map_line(&line_map, line, &mut offsets)?;
					}
					Edit::Paste {
						at: PasteAt::Span(super::parser::Range { start, end }),
						register: register.clone(),
						line_num: *line_num,
					}
				}
				PasteAt::Gap(cursor) => {
					let cursor = match cursor {
						Cursor::Before(line) => Cursor::Before(map_line(&line_map, *line, &mut offsets)?),
						Cursor::After(line) => Cursor::After(map_line(&line_map, *line, &mut offsets)?),
						other => *other,
					};
					Edit::Paste { at: PasteAt::Gap(cursor), register: register.clone(), line_num: *line_num }
				}
			},
			Edit::Insert { cursor, text, line_num, replacement } => {
				let cursor = match cursor {
					Cursor::Before(line) => Cursor::Before(map_line(&line_map, *line, &mut offsets)?),
					Cursor::After(line) => Cursor::After(map_line(&line_map, *line, &mut offsets)?),
					other => *other,
				};
				Edit::Insert { cursor, text: text.clone(), line_num: *line_num, replacement: *replacement }
			}
		};
		remapped.push(mapped);
	}

	let first = *offsets.first()?;
	if offsets.iter().any(|offset| *offset != first) {
		return None;
	}
	Some((remapped, first))
}

/// Attempt recovery for a stale tag. `None` means no safe path forward.
pub fn try_recover(
	store: &SnapshotStore,
	canonical: &str,
	tag: &str,
	current: &str,
	edits: &[Edit],
	clipboard: &mut Clipboard,
	token: &tokio_util::sync::CancellationToken,
	budget: &mut Budget,
) -> Option<RecoveryResult> {
	let snapshot = store.by_hash(canonical, tag)?;
	let head_is_snapshot = store
		.head(canonical)
		.is_some_and(|head| head.hash == snapshot.hash && head.text == snapshot.text);
	let recovery_warning =
		if head_is_snapshot { RECOVERY_EXTERNAL_WARNING } else { RECOVERY_SESSION_CHAIN_WARNING };

	let (remapped, offset) = remap_edits(&snapshot.text, current, edits)?;
	let applied = apply::apply_edits_checked(current, &remapped, clipboard, Some(token), budget).ok()?;
	if applied.text == current {
		return None;
	}
	let mut warnings = Vec::with_capacity(applied.warnings.len() + 1);
	warnings.push(if offset == 0 {
		recovery_warning.to_string()
	} else {
		RECOVERY_LINE_REMAP_WARNING.to_string()
	});
	warnings.extend(applied.warnings);
	Some(RecoveryResult { text: applied.text, first_changed: applied.first_changed_line, warnings })
}
