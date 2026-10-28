//! Clipboard registers for `CUT` and register-backed `PUT`.
//!
//! `CUT` captures source lines before the ordinary delete edits apply. Named
//! registers persist for the session; the anonymous register is batch-local
//! and empty at the start of every call.

use std::collections::BTreeMap;

use super::budget::{Budget, MAX_SESSION_REGISTER_BYTES};
use super::parser::{Edit, PasteAt};

/// Register state for one edit batch.
#[derive(Debug, Clone, Default)]
pub struct Clipboard {
	/// Session registers (`CUT … @name` writes, `PUT … @name` reads).
	pub named: BTreeMap<String, Vec<String>>,
	/// Anonymous register captured by an unlabeled `CUT`.
	pub lines: Option<Vec<String>>,
	/// Descriptions of unlabeled cuts still awaiting a paste.
	pub pending_anon_cuts: Vec<String>,
}

/// Working clipboard for one batch: persisted named registers, no anonymous
/// state.
pub fn start_batch(named: BTreeMap<String, Vec<String>>) -> Clipboard {
	Clipboard { named, ..Clipboard::default() }
}

/// Publish a batch fork back to the session-owned registers: only named
/// registers persist; the anonymous register never crosses calls.
/// Conservative per-entry map overhead (node, key `String`, value `Vec`
/// inline headers) beyond the name and line bytes counted separately.
const MAP_ENTRY_OVERHEAD: usize = 48;

/// Bytes one line vector retains or clones: one `String` header per line plus
/// each line's capacity, so blank lines and repeated names are not free.
pub fn lines_bytes(lines: &[String]) -> usize {
	lines.len() * std::mem::size_of::<String>()
		+ lines
			.iter()
			.map(|line| line.len().max(line.capacity()))
			.sum::<usize>()
}

fn register_bytes(name: &str, lines: &Vec<String>) -> usize {
	name.len()
		+ std::mem::size_of::<String>()
		+ MAP_ENTRY_OVERHEAD
		+ lines.capacity() * std::mem::size_of::<String>()
		+ lines.iter().map(|line| line.len().max(line.capacity())).sum::<usize>()
}

pub fn named_bytes(map: &BTreeMap<String, Vec<String>>) -> usize {
	map.iter()
		.map(|(name, lines)| register_bytes(name, lines))
		.sum()
}

/// Bytes `clone`-ing this batch state copies (named registers, anon lines,
/// pending descriptions).
pub fn batch_bytes(clipboard: &Clipboard) -> usize {
	named_bytes(&clipboard.named)
		+ clipboard.lines.as_ref().map_or(0, |lines| {
			lines.capacity() * std::mem::size_of::<String>()
				+ lines
					.iter()
					.map(|line| line.len().max(line.capacity()))
					.sum::<usize>()
		})
		+ clipboard
			.pending_anon_cuts
			.iter()
			.map(|cut| std::mem::size_of::<String>() + cut.len())
			.sum::<usize>()
}

/// Persist one batch's named registers, refusing to grow the session map past
/// its byte bound (the caller has already landed the file and must disclose).
pub fn commit_batch(
	fork: &Clipboard,
	session: &mut BTreeMap<String, Vec<String>>,
) -> Result<(), String> {
	let mut projected = named_bytes(session);
	for (name, lines) in &fork.named {
		let previous = session.get(name).map_or(0, |lines| register_bytes(name, lines));
		projected = projected
			.saturating_sub(previous)
			.saturating_add(register_bytes(name, lines));
	}
	if projected > MAX_SESSION_REGISTER_BYTES {
		return Err(format!(
			"named registers would reach {projected} bytes, above the {} MiB session bound",
			MAX_SESSION_REGISTER_BYTES / (1024 * 1024)
		));
	}
	for (name, lines) in &fork.named {
		session.insert(name.clone(), lines.clone());
	}
	Ok(())
}

/// True when at least one edit reads or writes a register.
pub fn has_clipboard_edit(edits: &[Edit]) -> bool {
	edits.iter().any(|edit| match edit {
		Edit::Cut { .. } | Edit::Paste { .. } => true,
		Edit::Block { mode, register, .. } => {
			*mode == super::parser::BlockMode::Cut
				|| *mode == super::parser::BlockMode::PasteAfter
				|| register.is_some()
		}
		Edit::Insert { .. } | Edit::Delete { .. } => false,
	})
}

fn describe_cut(range: super::parser::Range, register: Option<&str>) -> String {
	let span = if range.start == range.end {
		format!("{}", range.start)
	} else {
		format!("{}.={}", range.start, range.end)
	};
	match register {
		Some(register) => format!("CUT {span} @{register}"),
		None => format!("CUT {span}"),
	}
}

fn register_list(named: &BTreeMap<String, Vec<String>>) -> String {
	if named.is_empty() {
		return String::new();
	}
	let known = named
		.keys()
		.map(|name| format!("`@{name}`"))
		.collect::<Vec<_>>()
		.join(", ");
	format!(" Available registers: {known}.")
}

enum RegisterRead {
	Lines(Vec<String>),
	/// Named register missing on a gap paste: warn and paste nothing.
	EmptyNamed(String),
}

fn read_register(
	register: Option<&str>,
	target_span: bool,
	clipboard: &mut Clipboard,
	line_num: u32,
	budget: &mut Budget,
) -> Result<RegisterRead, String> {
	if let Some(name) = register {
		if let Some(lines) = clipboard.named.get(name) {
			budget.charge_bytes(register_bytes(name, lines))?;
			return Ok(RegisterRead::Lines(lines.clone()));
		}
		if target_span {
			return Err(format!(
				"line {line_num}: `@{name}` is empty — no `CUT … @{name}` precedes this op in this call and no \
				 persisted register has that name — so pasting it over a range would delete those lines and write \
				 nothing back. Capture the register first (`CUT … @{name}`), or use `CUT` if deleting the range is \
				 what you meant.{}",
				register_list(&clipboard.named)
			));
		}
		return Ok(RegisterRead::EmptyNamed(format!(
			"line {line_num}: `@{name}` was empty — no `CUT … @{name}` precedes this op in this call and no persisted \
			 register has that name — so nothing was pasted.{}",
			register_list(&clipboard.named)
		)));
	}

	if clipboard.pending_anon_cuts.len() > 1 {
		return Err(format!(
			"line {line_num}: {} unlabeled `CUT`s are pending ({}) — an unlabeled paste cannot tell which one you \
			 meant. Label the moves (`CUT … @name` → `PUT … @name`), or keep at most one unlabeled `CUT` before \
			 each unlabeled paste.",
			clipboard.pending_anon_cuts.len(),
			clipboard.pending_anon_cuts.join(", ")
		));
	}
	if let Some(lines) = clipboard.lines.as_ref() {
		budget.charge_bytes(
			lines.capacity() * std::mem::size_of::<String>()
				+ lines
					.iter()
					.map(|line| line.len().max(line.capacity()))
					.sum::<usize>(),
		)?;
	}
	let Some(lines) = clipboard.lines.clone() else {
		return Err(format!(
			"line {line_num}: Nothing to paste: no unlabeled `CUT` precedes this `PUT` in this call, and the anonymous \
			 register never carries across calls. Put `CUT N.=M` / `CUT N*` above it, or use named registers \
			 (`CUT … @name` → `PUT … @name`) for cross-call moves."
		));
	};
	clipboard.pending_anon_cuts.clear();
	Ok(RegisterRead::Lines(lines))
}

fn write_register(
	edit: &Edit,
	file_lines: &[String],
	clipboard: &mut Clipboard,
	budget: &mut Budget,
) -> Result<(), String> {
	let Edit::Cut { range, register, line_num } = edit else {
		return Ok(());
	};
	if range.start < 1 || usize::try_from(range.end).unwrap_or(usize::MAX) > file_lines.len() {
		return Err(format!(
			"line {line_num}: `{}` is out of range (file has {} lines).",
			describe_cut(*range, register.as_deref()),
			file_lines.len()
		));
	}
	let slice = &file_lines[(range.start - 1) as usize..range.end as usize];
	budget.charge_bytes(lines_bytes(slice))?;
	let captured: Vec<String> = slice.to_vec();
	if let Some(name) = register {
		clipboard.named.insert(name.clone(), captured);
	} else {
		clipboard.lines = Some(captured);
		clipboard
			.pending_anon_cuts
			.push(describe_cut(*range, register.as_deref()));
	}
	Ok(())
}

/// Surface anonymous-register sequencing mistakes (empty or ambiguous paste)
/// before the edit touches file text, matching the upstream preflight order.
pub fn validate_sequence(
	edits: &[Edit],
	clipboard: &Clipboard,
	budget: &mut Budget,
) -> Result<(), String> {
	if !has_clipboard_edit(edits) {
		return Ok(());
	}
	budget.charge_bytes(batch_bytes(clipboard))?;
	let mut fork = clipboard.clone();
	let mut scratch = Budget::default();
	for edit in edits {
		match edit {
			Edit::Cut { range, register, .. } => match register {
				Some(name) => {
					fork.named.insert(name.clone(), Vec::new());
				}
				None => {
					fork.lines = Some(Vec::new());
					fork.pending_anon_cuts.push(describe_cut(*range, None));
				}
			},
			Edit::Paste { at, register, line_num } => {
				let target_span = matches!(at, PasteAt::Span(_));
				// Empty named gap pastes warn during apply, not here.
				let _ = read_register(register.as_deref(), target_span, &mut fork, *line_num, &mut scratch)?;
			}
			_ => {}
		}
	}
	Ok(())
}

/// Resolve clipboard edits against the original lines in authored order. Cuts
/// fill a register and emit nothing; pastes expand to inserts (plus per-line
/// deletes for span targets).
pub fn resolve(
	edits: &[Edit],
	file_lines: &[String],
	clipboard: &mut Clipboard,
	warnings: &mut Vec<String>,
	budget: &mut Budget,
) -> Result<Vec<Edit>, String> {
	if !has_clipboard_edit(edits) {
		return Ok(edits.to_vec());
	}
	let mut resolved = Vec::new();
	for edit in edits {
		match edit {
			Edit::Cut { .. } => {
				write_register(edit, file_lines, clipboard, budget)?;
			}
			Edit::Paste { at, register, line_num } => {
				let target_span = matches!(at, PasteAt::Span(_));
				let lines = match read_register(
					register.as_deref(),
					target_span,
					clipboard,
					*line_num,
					budget,
				)? {
					RegisterRead::Lines(lines) => lines,
					RegisterRead::EmptyNamed(warning) => {
						warnings.push(warning);
						continue;
					}
				};
				if lines.len() > super::parser::MAX_CLIPBOARD_PASTED_LINES {
					return Err(format!(
						"line {line_num}: paste expands to {} lines, above the {}-line clipboard limit",
						lines.len(),
						super::parser::MAX_CLIPBOARD_PASTED_LINES
					));
				}
				budget.charge_edits(lines.len())?;
				budget.charge_bytes(lines_bytes(&lines))?;
				match at {
					PasteAt::Gap(cursor) => {
						if lines.len() > super::parser::MAX_CLIPBOARD_PASTED_LINES {
							return Err(format!(
								"line {line_num}: paste expands to {} lines, above the \
								 {}-line clipboard limit",
								lines.len(),
								super::parser::MAX_CLIPBOARD_PASTED_LINES
							));
						}
						for text in lines {
							resolved.push(Edit::Insert {
								cursor: *cursor,
								text,
								line_num: *line_num,
								replacement: false,
							});
						}
					}
					PasteAt::Span(range) => {
						if lines.len() > super::parser::MAX_CLIPBOARD_PASTED_LINES {
							return Err(format!(
								"line {line_num}: paste expands to {} lines, above the \
								 {}-line clipboard limit",
								lines.len(),
								super::parser::MAX_CLIPBOARD_PASTED_LINES
							));
						}
						if range.start < 1
							|| usize::try_from(range.end).unwrap_or(usize::MAX) > file_lines.len()
						{
							let suffix = register
								.as_ref()
								.map(|name| format!(" @{name}"))
								.unwrap_or_default();
							return Err(format!(
								"line {line_num}: `PUT {}.={}{suffix}` is out of range (file has {} lines).",
								range.start,
								range.end,
								file_lines.len()
							));
						}
						for text in lines {
							resolved.push(Edit::Insert {
								cursor: super::parser::Cursor::Before(range.start),
								text,
								line_num: *line_num,
								replacement: true,
							});
						}
						budget
							.charge_edits((range.end - range.start + 1) as usize)?;
						for line in range.start..=range.end {
							resolved.push(Edit::Delete { line, line_num: *line_num });
						}
					}
				}
			}
			other => resolved.push(other.clone()),
		}
	}
	Ok(resolved)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn session_register_bound_refuses_oversized_batch() {
		let fork = Clipboard {
			named: BTreeMap::from([(
				"r".to_string(),
				vec!["x".repeat(MAX_SESSION_REGISTER_BYTES)],
			)]),
			lines: None,
			pending_anon_cuts: Vec::new(),
		};
		let mut session = BTreeMap::new();
		let error = commit_batch(&fork, &mut session).err().expect("must reject");
		assert!(error.contains("session bound"), "{error}");
		assert!(session.is_empty(), "nothing persisted past the bound");
	}

	#[test]
	fn session_register_bound_counts_blank_line_slots_across_batches() {
		// Six blank registers of 115k lines: no content bytes, ~16.5 MiB of
		// `String` slots (under the 16 MiB bound). A later batch that grows
		// one register must be refused without disturbing the last good state.
		let mut session = BTreeMap::new();
		let mut first = Clipboard {
			named: BTreeMap::new(),
			lines: None,
			pending_anon_cuts: Vec::new(),
		};
		for index in 0..6 {
			first
				.named
				.insert(format!("r{index}"), vec![String::new(); 115_000]);
		}
		commit_batch(&first, &mut session).expect("under the bound");

		let mut overflow = Clipboard {
			named: session.clone(),
			lines: None,
			pending_anon_cuts: Vec::new(),
		};
		overflow
			.named
			.insert("r0".to_string(), vec![String::new(); 160_000]);
		let error = commit_batch(&overflow, &mut session)
			.err()
			.expect("must reject the growth");
		assert!(error.contains("session bound"), "{error}");
		assert_eq!(session["r0"].len(), 115_000, "previous state kept");

		let mut shrink = Clipboard {
			named: session.clone(),
			lines: None,
			pending_anon_cuts: Vec::new(),
		};
		shrink.named.insert("r0".to_string(), Vec::new());
		commit_batch(&shrink, &mut session).expect("tool usable after refusal");
		assert!(session["r0"].is_empty());
	}

	#[test]
	fn session_register_bound_counts_name_and_line_slots() {
		// 250k registers of blank lines: no content bytes, but every name,
		// map entry, and empty line vector must be accounted for.
		let mut fork = Clipboard {
			named: BTreeMap::new(),
			lines: None,
			pending_anon_cuts: Vec::new(),
		};
		for index in 0..250_000u32 {
			fork.named.insert(format!("n{index:06}"), Vec::new());
		}
		let mut session = BTreeMap::new();
		let error = commit_batch(&fork, &mut session).err().expect("must reject");
		assert!(error.contains("session bound"), "{error}");
		assert!(session.is_empty());
	}
}
