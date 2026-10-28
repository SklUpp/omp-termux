//! Expand deferred block anchors (`N*`) into concrete line edits.
//!
//! Resolution goes through the native `pi-ast` tree-sitter path; the parser
//! cannot expand a block until file text exists. After this runs no `Block`
//! edit remains.

use pi_ast::block::{BlockRangeOptions, block_range_at};

use super::parser::{BlockMode, Cursor, Edit, PasteAt, Range};

/// Inclusive line span of a resolved syntactic block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockSpan {
	pub start: u32,
	pub end: u32,
}

fn block_span(text: &str, path: &str, line: u32) -> Option<BlockSpan> {
	block_range_at(BlockRangeOptions {
		code: text.to_string(),
		lang: None,
		path: Some(path.to_string()),
		line,
	})
	.ok()
	.flatten()
	.map(|range| BlockSpan { start: range.start_line, end: range.end_line })
}

fn is_structural_closer(line: &str) -> bool {
	let trimmed = line.trim();
	if trimmed.is_empty() {
		return false;
	}
	let core = trimmed.trim_end_matches(|ch| ch == ';' || ch == ',').trim_end();
	!core.is_empty() && core.chars().all(|ch| matches!(ch, ')' | ']' | '}'))
}

fn block_form(mode: BlockMode, line: u32, register: Option<&str>) -> String {
	let suffix = register.map(|name| format!(" @{name}")).unwrap_or_default();
	match mode {
		BlockMode::Replace => format!("PUT {line}*{suffix}"),
		BlockMode::InsertAfter => format!("PUT >{line}*{suffix}"),
		BlockMode::PasteAfter => format!("PUT >{line}*{suffix}"),
		BlockMode::Cut => format!("CUT {line}*{suffix}"),
	}
}

fn block_unresolved_message(line_num: u32, anchor: u32, mode: BlockMode, register: Option<&str>) -> String {
	let form = block_form(mode, anchor, register);
	let fallback = match mode {
		BlockMode::Cut => match register {
			Some(name) => format!("CUT {anchor}.=M @{name}"),
			None => format!("CUT {anchor}.=M"),
		},
		_ => match register {
			Some(name) => format!("PUT {anchor}.=M @{name}"),
			None => format!("PUT {anchor}.=M:"),
		},
	};
	format!(
		"line {line_num}: `{form}` did not resolve a syntactic block beginning on line {anchor} \
		 (unsupported language, blank line, no node starts there, or the block contains a syntax error). \
		 Use an explicit range such as `{fallback}`, or anchor on the line that OPENS a multi-line construct."
	)
}

fn block_single_line_message(line_num: u32, anchor: u32, mode: BlockMode, register: Option<&str>) -> String {
	let form = block_form(mode, anchor, register);
	let plain = match mode {
		BlockMode::Cut => format!("CUT {anchor}"),
		BlockMode::InsertAfter | BlockMode::PasteAfter => format!("PUT >{anchor}"),
		BlockMode::Replace => format!("PUT {anchor}:"),
	};
	format!(
		"line {line_num}: `{form}` resolved a single-line block — line {anchor} is a bare statement, not the \
		 opening line of a multi-line construct. For only this statement use `{plain}`."
	)
}

fn push_delete_span(edits: &mut Vec<Edit>, span: BlockSpan, line_num: u32) {
	for line in span.start..=span.end {
		edits.push(Edit::Delete { line, line_num });
	}
}

/// Resolve every block edit in `edits` against `text` (language inferred from
/// `path`). Unresolvable after-block ops lower to their plain after-line form
/// with a warning; unresolvable replace/cut ops fail the patch.
pub fn resolve_block_edits(
	edits: Vec<Edit>,
	text: &str,
	path: &str,
	warnings: &mut Vec<String>,
) -> Result<Vec<Edit>, String> {
	if !edits.iter().any(|edit| matches!(edit, Edit::Block { .. })) {
		return Ok(edits);
	}
	let lines: Vec<&str> = text.split('\n').collect();
	let mut resolved = Vec::new();
	for edit in edits {
		let Edit::Block { anchor, payloads, mode, register, line_num } = edit else {
			resolved.push(edit);
			continue;
		};
		let span = block_span(text, path, anchor);
		let Some(span) = span else {
			if !matches!(mode, BlockMode::InsertAfter | BlockMode::PasteAfter) {
				return Err(block_unresolved_message(line_num, anchor, mode, register.as_deref()));
			}
			let anchor_text = lines
				.get(anchor.saturating_sub(1) as usize)
				.copied()
				.unwrap_or("");
			let is_closer = is_structural_closer(anchor_text);
			let form = block_form(mode, anchor, register.as_deref());
			let plain = if mode == BlockMode::PasteAfter {
				format!("PUT >{anchor}")
			} else {
				format!("PUT >{anchor}:")
			};
			warnings.push(if is_closer {
				format!(
					"`{form}` anchors on a closing delimiter, so it was applied as plain `{plain}`. \
					 Anchor on the line that OPENS the construct."
				)
			} else {
				format!(
					"`{form}` could not resolve a syntactic block on line {anchor}, so it was applied as \
					 plain `{plain}`. Verify the landing line; anchor on a line that OPENS a construct."
				)
			});
			if mode == BlockMode::PasteAfter {
				resolved.push(Edit::Paste {
					at: PasteAt::Gap(Cursor::After(anchor)),
					register,
					line_num,
				});
			} else {
				for payload in payloads {
					resolved.push(Edit::Insert {
						cursor: Cursor::After(anchor),
						text: payload,
						line_num,
						replacement: false,
					});
				}
			}
			continue;
		};
		if span.start == span.end {
			return Err(block_single_line_message(line_num, anchor, mode, register.as_deref()));
		}
		match mode {
			BlockMode::PasteAfter => {
				resolved.push(Edit::Paste {
					at: PasteAt::Gap(Cursor::After(span.end)),
					register,
					line_num,
				});
			}
			BlockMode::Cut => {
				resolved.push(Edit::Cut {
					range: Range { start: span.start, end: span.end },
					register,
					line_num,
				});
				push_delete_span(&mut resolved, span, line_num);
			}
			BlockMode::InsertAfter => {
				for payload in payloads {
					resolved.push(Edit::Insert {
						cursor: Cursor::After(span.end),
						text: payload,
						line_num,
						replacement: false,
					});
				}
			}
			BlockMode::Replace => {
				if let Some(register) = register {
					resolved.push(Edit::Paste {
						at: PasteAt::Span(Range { start: span.start, end: span.end }),
						register: Some(register),
						line_num,
					});
					continue;
				}
				for payload in payloads {
					resolved.push(Edit::Insert {
						cursor: Cursor::Before(span.start),
						text: payload,
						line_num,
						replacement: true,
					});
				}
				push_delete_span(&mut resolved, span, line_num);
			}
		}
	}
	Ok(resolved)
}
