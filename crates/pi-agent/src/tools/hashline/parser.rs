//! Hashline patch parser: section splitting plus the canonical hunk grammar.
//!
//! All line numbers in a section index the original tagged snapshot, never
//! earlier hunks of the same call. Parsing is pure: it never touches the
//! filesystem and never resolves block anchors.

use std::{
	collections::{BTreeSet, HashMap},
	path::{Component, Path, PathBuf},
};

pub const BEGIN_PATCH_MARKER: &str = "*** Begin Patch";
pub const END_PATCH_MARKER: &str = "*** End Patch";
pub const ABORT_MARKER: &str = "*** Abort";

const MAX_EXPANDED_RANGE_LINES: u64 = 100_000;
const REGISTER_NAME_MAX: usize = 64;

pub const REPLACE_PAIR_COALESCED_WARNING: &str =
	"Multiple hunks targeted the same exact range; kept only the last. Issue one `PUT` or `CUT` hunk per range.";
pub const BARE_BODY_AUTO_PIPED_WARNING: &str =
	"Auto-prefixed bare body row(s) with `+`. Body rows must be `+TEXT` literal lines.";
pub const EMPTY_PUT_AUTO_CUT_WARNING: &str =
	"Interpreted an empty `PUT` body as deletion. Use `CUT N.=M` or `CUT N*` for bodyless deletes.";
pub const CUT_COLON_IGNORED_WARNING: &str =
	"Ignored a trailing `:` on bodyless `CUT`. Prefer `CUT N.=M` / `CUT N*` without a colon.";
pub const READ_METADATA_IGNORED_WARNING: &str =
	"Ignored copied read-output elision row(s). Re-read elided ranges before editing them.";

const REM_TAKES_NO_BODY: &str =
	"`REM` deletes the whole file and takes no body rows or line ops. Issue it alone under the header.";
const MOVE_TAKES_NO_BODY: &str =
	"`MV DEST` does not take body rows. Put line edits above the `MV` row; the destination path follows `MV` on the same line.";
const CUT_TAKES_NO_BODY: &str =
	"`CUT` deletes (and captures) the named lines and takes no body rows. To write new content, use `PUT N.=M:` with `+TEXT` rows.";
const COLON_ON_REGISTER_PUT: &str =
	"A register `PUT` pastes captured lines, so it takes no `:`. Write `PUT 5.=9 @name` (or `PUT >40 @name`), and drop the `@name` to write literal body rows.";
const REGISTER_PUT_TAKES_NO_BODY: &str =
	"A register `PUT` pastes captured lines and takes no `+` body rows. To write literal text, drop the `@name` and use `PUT …:` with body rows.";
const COLONLESS_PUT_TAKES_NO_BODY: &str =
	"`PUT` without `:` is clipboard-backed and takes no body rows. Add `:` after the locator to write literal content (`PUT >40:` then `+TEXT` rows).";
const COLONLESS_SPAN_PUT: &str =
	"Colonless `PUT` is clipboard-backed, and span targets need a named register (`PUT 5.=9 @name`); the anonymous register pastes only at gaps (`PUT >40`). To write literal content, add `:` and `+TEXT` body rows.";
const EMPTY_INSERT: &str =
	"`PUT <N:` / `PUT >N:` promises body rows and got none. Write `+TEXT` rows, or drop the `:` to paste a register (`PUT >N` = anonymous, `PUT >N @name` = named).";
/// Total literal body bytes an edit list will clone when materialized.
pub fn payload_bytes(edits: &[Edit]) -> usize {
	edits
		.iter()
		.map(|edit| match edit {
			Edit::Insert { text, .. } => text.len(),
			Edit::Block { payloads, .. } => payloads.iter().map(String::len).sum(),
			_ => 0,
		})
		.sum()
}

/// Aggregate expanded edits a single section may declare, counted from hunk
/// ranges and payload rows *before* any deletes/inserts are materialized.
pub const MAX_SECTION_EXPANDED_LINES: u64 = 100_000;
/// Expanded edits a single clipboard paste may add.
pub const MAX_CLIPBOARD_PASTED_LINES: usize = 100_000;

const MINUS_BULLET_AUTO_PIPED_WARNING: &str =
	"Auto-prefixed bare `- ` bullet row(s) as literal content. `-` rows never remove lines — the range does that; always prefix literal body rows with `+`: `+- item`.";
const DIFF_OLD_ROWS_IGNORED_WARNING: &str =
	"Ignored unified-diff `-old` row(s); the range already removes old content, so only `+new` rows were kept.";
const MINUS_ROW_REJECTED: &str =
	"`-` rows are not valid; the range already names the lines being changed. For Markdown bullets or other literal `-` lines, prefix the literal row with `+`: `+- item`.";

/// Inclusive 1-indexed line range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Range {
	pub start: u32,
	pub end: u32,
}

/// Where an inserted row lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cursor {
	Before(u32),
	After(u32),
	Bof,
	Eof,
}

/// Deferred block operation resolved later against the file's syntax tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockMode {
	Replace,
	InsertAfter,
	PasteAfter,
	Cut,
}

/// Paste destination: a gap (cursor) or a span to replace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteAt {
	Gap(Cursor),
	Span(Range),
}

/// One low-level edit. `line_num` is the authored patch line used for
/// diagnostics and hunk grouping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
	Insert {
		cursor: Cursor,
		text: String,
		line_num: u32,
		/// Body row of a replacement hunk (emitted ahead of the range deletes).
		replacement: bool,
	},
	Delete {
		line: u32,
		line_num: u32,
	},
	Cut {
		range: Range,
		register: Option<String>,
		line_num: u32,
	},
	Paste {
		at: PasteAt,
		register: Option<String>,
		line_num: u32,
	},
	Block {
		anchor: u32,
		payloads: Vec<String>,
		mode: BlockMode,
		register: Option<String>,
		line_num: u32,
	},
}

impl Edit {
	pub fn line_num(&self) -> u32 {
		match self {
			Edit::Insert { line_num, .. }
			| Edit::Delete { line_num, .. }
			| Edit::Cut { line_num, .. }
			| Edit::Paste { line_num, .. }
			| Edit::Block { line_num, .. } => *line_num,
		}
	}
}

/// Whole-file operation carried by a section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileOp {
	Rem,
	Move(String),
}

/// One authored section: a target file plus its raw op lines.
#[derive(Debug, Clone)]
pub struct PatchSection {
	/// Path exactly as the edit tool should open it (relative to the session
	/// cwd unless authored absolute).
	pub path: String,
	/// Absolute target path, resolved against the session cwd.
	pub file_path: PathBuf,
	pub file_hash: Option<String>,
	pub diff: String,
	/// Same-path sections merged across another file's section. Clipboard ops
	/// in such a section are rejected: merging would reorder the register.
	pub interleaved: bool,
}

/// Parsed result of one section body.
#[derive(Debug, Clone, Default)]
pub struct ParsedPatch {
	pub edits: Vec<Edit>,
	pub file_op: Option<FileOp>,
	pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
	Replace { range: Range, register: Option<String> },
	Block { anchor: u32, register: Option<String> },
	InsertBefore { anchor: u32, register: Option<String> },
	InsertAfter { anchor: u32, register: Option<String> },
	InsertAfterBlock { anchor: u32, register: Option<String> },
	Cut { range: Range, register: Option<String> },
	CutBlock { anchor: u32, register: Option<String> },
	Bof { register: Option<String> },
	Eof { register: Option<String> },
	Rem,
	Move(String),
}

impl Target {
	fn set_register(self, register: Option<String>) -> Self {
		match self {
			Target::Replace { range, .. } => Target::Replace { range, register },
			Target::Block { anchor, .. } => Target::Block { anchor, register },
			Target::InsertBefore { anchor, .. } => Target::InsertBefore { anchor, register },
			Target::InsertAfter { anchor, .. } => Target::InsertAfter { anchor, register },
			Target::InsertAfterBlock { anchor, .. } => Target::InsertAfterBlock { anchor, register },
			Target::Cut { range, .. } => Target::Cut { range, register },
			Target::CutBlock { anchor, .. } => Target::CutBlock { anchor, register },
			Target::Bof { .. } => Target::Bof { register },
			Target::Eof { .. } => Target::Eof { register },
			other => other,
		}
	}

	fn register(&self) -> Option<&str> {
		match self {
			Target::Replace { register, .. }
			| Target::Block { register, .. }
			| Target::InsertBefore { register, .. }
			| Target::InsertAfter { register, .. }
			| Target::InsertAfterBlock { register, .. }
			| Target::Cut { register, .. }
			| Target::CutBlock { register, .. }
			| Target::Bof { register }
			| Target::Eof { register } => register.as_deref(),
			Target::Rem | Target::Move(_) => None,
		}
	}
}

#[derive(Debug, Clone)]
struct HunkHeader {
	target: Target,
	had_colon: bool,
}

#[derive(Debug, Clone)]
struct SectionHeader {
	path: String,
	file_hash: Option<String>,
}

#[derive(Debug, Clone)]
struct PayloadRow {
	text: String,
	bare: bool,
	line_num: u32,
	minus: bool,
}

#[derive(Debug, Clone)]
struct Pending {
	target: Target,
	line_num: u32,
	payloads: Vec<PayloadRow>,
	had_colon: bool,
	deferred_blanks: Vec<PayloadRow>,
}

// ── Byte helpers ────────────────────────────────────────────────────────────

fn is_ws(byte: u8) -> bool {
	byte == b' ' || (9..=13).contains(&byte)
}

fn trim_end_len(line: &str) -> usize {
	let bytes = line.as_bytes();
	let mut end = bytes.len();
	while end > 0 && is_ws(bytes[end - 1]) {
		end -= 1;
	}
	end
}

fn skip_ws(bytes: &[u8], mut index: usize, end: usize) -> usize {
	while index < end && is_ws(bytes[index]) {
		index += 1;
	}
	index
}

fn marker_eq(line: &str, marker: &str) -> bool {
	let end = trim_end_len(line);
	end == marker.len() && line.starts_with(marker)
}

fn ellipsis_at(line: &str, index: usize) -> bool {
	line.get(index..).is_some_and(|rest| rest.starts_with('…'))
}

fn scan_line_number(bytes: &[u8], index: usize, end: usize) -> Option<(u32, usize)> {
	if index >= end || !(b'1'..=b'9').contains(&bytes[index]) {
		return None;
	}
	let mut value: u64 = 0;
	let mut probe = index;
	while probe < end && bytes[probe].is_ascii_digit() {
		value = value * 10 + u64::from(bytes[probe] - b'0');
		if value > u64::from(u32::MAX) {
			return None;
		}
		probe += 1;
	}
	Some((value as u32, probe))
}

fn scan_range_separator(line: &str, index: usize, end: usize) -> Option<usize> {
	let bytes = line.as_bytes();
	let mut cursor = index;
	let mut consumed = false;
	while cursor < end {
		let byte = bytes[cursor];
		if is_ws(byte) || matches!(byte, b'-' | b'.' | b'=') {
			cursor += 1;
			consumed = true;
			continue;
		}
		if ellipsis_at(line, cursor) {
			cursor += '…'.len_utf8();
			consumed = true;
			continue;
		}
		break;
	}
	if !consumed || cursor >= end || !(b'1'..=b'9').contains(&bytes[cursor]) {
		return None;
	}
	Some(cursor)
}

fn scan_dangling_separator(line: &str, index: usize, end: usize) -> Option<usize> {
	let bytes = line.as_bytes();
	let mut cursor = index;
	let mut saw_separator = false;
	while cursor < end {
		let byte = bytes[cursor];
		if matches!(byte, b'-' | b'.' | b'=') {
			saw_separator = true;
			cursor += 1;
			continue;
		}
		if ellipsis_at(line, cursor) {
			saw_separator = true;
			cursor += '…'.len_utf8();
			continue;
		}
		if is_ws(byte) {
			cursor += 1;
			continue;
		}
		break;
	}
	if !saw_separator {
		return None;
	}
	if cursor < end && !matches!(bytes[cursor], b':' | b'@') {
		return None;
	}
	Some(cursor)
}

fn scan_header_range(
	line: &str,
	index: usize,
	end: usize,
	allow_single: bool,
) -> Option<(Range, usize, bool)> {
	let bytes = line.as_bytes();
	let number_start = skip_ws(bytes, index, end);
	let (start, after_start) = scan_line_number(bytes, number_start, end)?;
	let Some(after_first) = scan_range_separator(line, after_start, end) else {
		if !allow_single {
			return None;
		}
		if let Some(dangling) = scan_dangling_separator(line, after_start, end) {
			return Some((Range { start, end: start }, dangling, true));
		}
		let next = skip_ws(bytes, after_start, end);
		return Some((Range { start, end: start }, next, false));
	};
	let (range_end, after_end) = scan_line_number(bytes, after_first, end)?;
	let next = skip_ws(bytes, after_end, end);
	Some((Range { start, end: range_end }, next, true))
}

fn is_register_name_byte(byte: u8) -> bool {
	byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

struct FinishScan {
	register: Option<String>,
	next: usize,
	had_colon: bool,
}

fn finish_target_scan(line: &str, index: usize, end: usize) -> Option<FinishScan> {
	let bytes = line.as_bytes();
	let mut cursor = skip_ws(bytes, index, end);
	let mut register = None;
	if cursor < end && bytes[cursor] == b'@' {
		let start = cursor + 1;
		let mut probe = start;
		while probe < end && is_register_name_byte(bytes[probe]) {
			probe += 1;
		}
		if probe == start || probe - start > REGISTER_NAME_MAX {
			return None;
		}
		register = Some(line[start..probe].to_string());
		cursor = probe;
	}
	cursor = skip_ws(bytes, cursor, end);
	let mut had_colon = false;
	if cursor < end && bytes[cursor] == b':' {
		had_colon = true;
		cursor = skip_ws(bytes, cursor + 1, end);
	}
	Some(FinishScan { register, next: cursor, had_colon })
}

fn scan_keyword(line: &str, index: usize, end: usize, keyword: &str) -> Option<usize> {
	let bytes = line.as_bytes();
	if !line.get(index..)?.starts_with(keyword) {
		return None;
	}
	let next = index + keyword.len();
	if next < end && !is_ws(bytes[next]) && bytes[next] != b':' {
		return None;
	}
	Some(next)
}

fn scan_put_target(line: &str, index: usize, end: usize) -> Option<(Target, FinishScan)> {
	let bytes = line.as_bytes();
	let cursor = skip_ws(bytes, index, end);
	if cursor >= end {
		return None;
	}
	match bytes[cursor] {
		b'<' | b'>' => {
			let is_after = bytes[cursor] == b'>';
			let probe = skip_ws(bytes, cursor + 1, end);
			if is_after && probe < end && bytes[probe] == b'$' {
				let finish = finish_target_scan(line, probe + 1, end)?;
				return Some((Target::Eof { register: None }, finish));
			}
			let (anchor, after_anchor) = scan_line_number(bytes, probe, end)?;
			let mut next = after_anchor;
			let mut block = false;
			if next < end && bytes[next] == b'*' {
				block = true;
				next += 1;
			}
			let finish = finish_target_scan(line, next, end)?;
			let target = if is_after {
				if block {
					Target::InsertAfterBlock { anchor, register: None }
				} else {
					Target::InsertAfter { anchor, register: None }
				}
			} else if anchor == 1 {
				// `<1` is head: position-stable and valid on empty files.
				Target::Bof { register: None }
			} else {
				Target::InsertBefore { anchor, register: None }
			};
			Some((target, finish))
		}
		_ => {
			let (range, next, had_separator) = scan_header_range(line, cursor, end, true)?;
			if next < end && bytes[next] == b'*' {
				if had_separator {
					return None;
				}
				let finish = finish_target_scan(line, next + 1, end)?;
				return Some((Target::Block { anchor: range.start, register: None }, finish));
			}
			let finish = finish_target_scan(line, next, end)?;
			Some((Target::Replace { range, register: None }, finish))
		}
	}
}

fn scan_cut_target(line: &str, index: usize, end: usize) -> Option<(Target, FinishScan)> {
	let bytes = line.as_bytes();
	let (range, next, had_separator) = scan_header_range(line, index, end, true)?;
	if next < end && bytes[next] == b'*' {
		if had_separator {
			return None;
		}
		let finish = finish_target_scan(line, next + 1, end)?;
		return Some((Target::CutBlock { anchor: range.start, register: None }, finish));
	}
	let finish = finish_target_scan(line, next, end)?;
	Some((Target::Cut { range, register: None }, finish))
}

fn unquote(text: &str) -> &str {
	let bytes = text.as_bytes();
	if bytes.len() >= 2 {
		let first = bytes[0];
		let last = bytes[bytes.len() - 1];
		if (first == b'"' || first == b'\'') && first == last {
			return &text[1..text.len() - 1];
		}
	}
	text
}

fn scan_move_dest(line: &str, index: usize, end: usize) -> Option<String> {
	let bytes = line.as_bytes();
	let cursor = skip_ws(bytes, index, end);
	if cursor >= end {
		return None;
	}
	let quote = bytes[cursor];
	if quote == b'"' || quote == b'\'' {
		let mut probe = cursor + 1;
		while probe < end {
			if bytes[probe] == b'\\' && probe + 1 < end {
				probe += 2;
				continue;
			}
			if bytes[probe] == quote {
				let after = skip_ws(bytes, probe + 1, end);
				if after != end {
					return None;
				}
				return Some(unquote(&line[cursor..=probe]).to_string());
			}
			probe += 1;
		}
		return None;
	}
	Some(unquote(&line[cursor..end]).trim().to_string())
}

fn scan_hunk_anchor(line: &str, start: usize, end: usize) -> Option<(Target, FinishScan)> {
	let bytes = line.as_bytes();
	let cursor = skip_ws(bytes, start, end);
	if let Some(next) = scan_keyword(line, cursor, end, "REM") {
		let after = skip_ws(bytes, next, end);
		if after != end {
			return None;
		}
		return Some((
			Target::Rem,
			FinishScan { register: None, next: after, had_colon: false },
		));
	}
	if let Some(next) = scan_keyword(line, cursor, end, "MV") {
		let dest = scan_move_dest(line, next, end)?;
		if dest.is_empty() {
			return None;
		}
		return Some((
			Target::Move(dest),
			FinishScan { register: None, next: end, had_colon: false },
		));
	}
	if let Some(next) = scan_keyword(line, cursor, end, "PUT") {
		return scan_put_target(line, next, end);
	}
	if let Some(next) = scan_keyword(line, cursor, end, "CUT") {
		return scan_cut_target(line, next, end);
	}
	None
}

/// Parse one op header line (`PUT …`, `CUT …`, `REM`, `MV …`).
fn try_parse_hunk_header(line: &str) -> Option<HunkHeader> {
	let end = trim_end_len(line);
	let bytes = line.as_bytes();
	let start = skip_ws(bytes, 0, end);
	if start >= end {
		return None;
	}
	let (target, finish) = scan_hunk_anchor(line, start, end)?;
	if finish.next != end {
		return None;
	}
	Some(HunkHeader { target: target.set_register(finish.register), had_colon: finish.had_colon })
}

/// Whether `text` would parse as an op header on its own.
pub fn is_hunk_header_text(text: &str) -> bool {
	try_parse_hunk_header(text).is_some()
}

/// Parse a `[PATH]` / `[PATH#TAG]` header line.
fn parse_section_header(line: &str) -> Option<SectionHeader> {
	if !line.starts_with('[') {
		return None;
	}
	let end = trim_end_len(line);
	let bytes = line.as_bytes();
	if end < 3 || bytes[end - 1] != b']' {
		return None;
	}
	let body_end = end - 1;
	let mut path_end = body_end;
	let mut file_hash = None;
	if body_end >= 6 {
		let tag_start = body_end - 5;
		if bytes[tag_start] == b'#'
			&& bytes[tag_start + 1..body_end].iter().all(u8::is_ascii_hexdigit)
		{
			path_end = tag_start;
			file_hash = Some(line[tag_start + 1..body_end].to_ascii_uppercase());
		}
	}
	if path_end <= 1 {
		return None;
	}
	if line[1..path_end].contains('#') {
		return None;
	}
	Some(SectionHeader { path: line[1..path_end].to_string(), file_hash })
}

// ── Section splitting ───────────────────────────────────────────────────────

fn lexical_absolute(path: &Path) -> PathBuf {
	let mut out = PathBuf::new();
	for component in path.components() {
		match component {
			Component::CurDir => {}
			Component::ParentDir => {
				out.pop();
			}
			other => out.push(other.as_os_str()),
		}
	}
	out
}

fn normalize_path(raw: &str, cwd: Option<&Path>) -> String {
	let unquoted = unquote(raw.trim());
	if let Some(cwd) = cwd {
		let path = Path::new(unquoted);
		if path.is_absolute() {
			let target = lexical_absolute(path);
			let base = lexical_absolute(cwd);
			if let Ok(relative) = target.strip_prefix(&base) {
				let text = relative.to_string_lossy().replace('\\', "/");
				return if text.is_empty() { ".".to_string() } else { text };
			}
		}
	}
	unquoted.to_string()
}

fn strip_leading_blank_lines(input: &str) -> String {
	let stripped = input.strip_prefix('\u{feff}').unwrap_or(input);
	let mut lines: Vec<&str> = stripped.split('\n').collect();
	while let Some(head) = lines.first() {
		let head = head.strip_suffix('\r').unwrap_or(head);
		if head.trim().is_empty() || marker_eq(head, BEGIN_PATCH_MARKER) {
			lines.remove(0);
		} else {
			break;
		}
	}
	lines.join("\n")
}

struct MergeEntry {
	path: String,
	file_path: PathBuf,
	file_hash: Option<String>,
	diffs: Vec<String>,
	interleaved: bool,
}

/// Split authored input into sections (merged by path) without parsing bodies.
pub fn split_patch_sections(input: &str, cwd: Option<&Path>) -> Result<Vec<PatchSection>, String> {
	let stripped = strip_leading_blank_lines(input);
	let lines: Vec<&str> = stripped
		.split('\n')
		.map(|line| line.strip_suffix('\r').unwrap_or(line))
		.collect();
	let first_line = lines.first().copied().unwrap_or("");
	if parse_section_header(first_line).is_none() {
		if is_unified_diff_header(first_line.trim_end()) {
			return Err(
				"unified-diff hunk header (`@@ -N,M +N,M @@`) is not valid in hashline. \
				 File sections start with `[path#HASH]`; use `PUT N.=M:`, `CUT N.=M`, or `PUT <N:`/`PUT >N:` ops."
					.to_string(),
			);
		}
		let preview: String = first_line.chars().take(120).collect();
		return Err(format!(
			"input must begin with \"[PATH#HASH]\" on the first non-blank line for anchored edits; got: {preview:?}. \
			 Example: \"[src/foo.ts#1A2B]\" then edit ops."
		));
	}

	let mut entries: Vec<MergeEntry> = Vec::new();
	let mut index_by_path: HashMap<String, usize> = HashMap::new();
	let mut current: Option<SectionHeader> = None;
	let mut current_lines: Vec<&str> = Vec::new();
	let mut previous_path: Option<String> = None;

	for line in &lines {
		if marker_eq(line, END_PATCH_MARKER) || marker_eq(line, ABORT_MARKER) {
			break;
		}
		if marker_eq(line, BEGIN_PATCH_MARKER) {
			continue;
		}
		if line.trim_end().starts_with('[') {
			let header = parse_section_header(line).ok_or_else(|| {
				format!(
					"Input header must be `[PATH]` or `[PATH#TAG]` with a 4-hex content-hash tag; got {:?}.",
					line.trim_end()
				)
			})?;
			if let Some(previous) = current.take() {
				push_section(
					&mut entries,
					&mut index_by_path,
					&mut previous_path,
					previous,
					std::mem::take(&mut current_lines),
					cwd,
				)?;
			}
			current = Some(header);
			continue;
		}
		current_lines.push(line);
	}
	if let Some(previous) = current.take() {
		push_section(&mut entries, &mut index_by_path, &mut previous_path, previous, current_lines, cwd)?;
	}

	Ok(entries
		.into_iter()
		.map(|entry| PatchSection {
			path: entry.path,
			file_path: entry.file_path,
			file_hash: entry.file_hash,
			diff: entry.diffs.join("\n"),
			interleaved: entry.interleaved,
		})
		.collect())
}

fn push_section(
	entries: &mut Vec<MergeEntry>,
	index_by_path: &mut HashMap<String, usize>,
	previous_path: &mut Option<String>,
	header: SectionHeader,
	lines: Vec<&str>,
	cwd: Option<&Path>,
) -> Result<(), String> {
	if !lines.iter().any(|line| !line.trim().is_empty()) {
		return Ok(());
	}
	let path = normalize_path(&header.path, cwd);
	if path.is_empty() {
		return Err("Input header `[]` is empty; provide a file path.".to_string());
	}
	let file_path = {
		let raw = PathBuf::from(&path);
		match cwd {
			Some(cwd) if !raw.is_absolute() => cwd.join(raw),
			_ => raw,
		}
	};
	let diff = lines.join("\n");
	if let Some(index) = index_by_path.get(&path).copied() {
		let entry = &mut entries[index];
		if let (Some(existing), Some(incoming)) = (&entry.file_hash, &header.file_hash) {
			if existing != incoming {
				return Err(format!(
					"Conflicting hashline snapshot tags for {path}: #{existing} and #{incoming}. Re-read the file and retry with one current header."
				));
			}
		}
		if entry.file_hash.is_none() {
			entry.file_hash = header.file_hash;
		}
		if previous_path.as_deref() != Some(path.as_str()) {
			entry.interleaved = true;
		}
		entry.diffs.push(diff);
	} else {
		index_by_path.insert(path.clone(), entries.len());
		entries.push(MergeEntry {
			path: path.clone(),
			file_path,
			file_hash: header.file_hash,
			diffs: vec![diff],
			interleaved: false,
		});
	}
	*previous_path = Some(path);
	Ok(())
}

fn is_unified_diff_header(line: &str) -> bool {
	let rest = line.strip_prefix("@@").unwrap_or("");
	rest.contains("@@") && rest.contains(',')
}

// ── Section body parsing ────────────────────────────────────────────────────

fn check_section_expansion(declared: u64, line_num: u32) -> Result<(), String> {
	if declared > MAX_SECTION_EXPANDED_LINES {
		return Err(format!(
			"line {line_num}: this section expands to more than {MAX_SECTION_EXPANDED_LINES} lines \
			 (ranges are counted before expansion). Split the edit into smaller ranges."
		));
	}
	Ok(())
}

fn check_section_rows(declared: u64, line_num: u32) -> Result<(), String> {
	if declared > MAX_SECTION_EXPANDED_LINES {
		return Err(format!(
			"line {line_num}: this section has more than {MAX_SECTION_EXPANDED_LINES} body rows. \
			 Split the edit into smaller writes."
		));
	}
	Ok(())
}

fn push_warning(warnings: &mut Vec<String>, warning: &str) {
	if !warnings.iter().any(|existing| existing == warning) {
		warnings.push(warning.to_string());
	}
}

fn invalid_absolute_range_message(line_num: u32, range: Range, op: &str) -> String {
	let Range { start, end } = range;
	let single = if op == "replace" {
		format!("PUT {start}:")
	} else {
		format!("CUT {start}")
	};
	let counted = start as u64 + end as u64 - 1;
	let mut message = format!(
		"line {line_num}: Invalid absolute range: start {start}, end {end}. \
		 The value after `.=` is an absolute source line, not a line count or replacement length. \
		 For one line use `{single}`."
	);
	if counted <= u64::from(u32::MAX) && counted >= u64::from(start) {
		let counted_form = if op == "replace" {
			format!("PUT {start}.={counted}:")
		} else {
			format!("CUT {start}.={counted}")
		};
		message.push_str(&format!(" For {end} lines starting at {start}, use `{counted_form}`."));
	}
	message
}

fn validate_range(range: Range, line_num: u32, op: &str) -> Result<(), String> {
	if range.end < range.start {
		return Err(invalid_absolute_range_message(line_num, range, op));
	}
	let span = u64::from(range.end) - u64::from(range.start) + 1;
	if span > MAX_EXPANDED_RANGE_LINES {
		return Err(format!(
			"line {line_num}: {op} range spans {span} lines; the maximum is {MAX_EXPANDED_RANGE_LINES}. \
			 Split it into smaller hunks."
		));
	}
	Ok(())
}

fn bodyless_message(target: &Target, had_colon: bool) -> Option<&'static str> {
	match target {
		Target::Cut { .. } | Target::CutBlock { .. } => Some(CUT_TAKES_NO_BODY),
		Target::Rem | Target::Move(_) => None,
		_ => {
			if target.register().is_some() {
				Some(REGISTER_PUT_TAKES_NO_BODY)
			} else if !had_colon {
				Some(COLONLESS_PUT_TAKES_NO_BODY)
			} else {
				None
			}
		}
	}
}

fn strip_one_leading_prefix(line: &str) -> Option<&str> {
	let bytes = line.as_bytes();
	let end = bytes.len();
	let mut index = skip_ws(bytes, 0, end);
	if line.get(index..).is_some_and(|rest| rest.starts_with(">>>")) {
		index += 3;
	} else if line.get(index..).is_some_and(|rest| rest.starts_with(">>")) {
		index += 2;
	}
	index = skip_ws(bytes, index, end);
	if index < end && matches!(bytes[index], b'+' | b'*' | b'-') {
		index = skip_ws(bytes, index + 1, end);
	}
	let digits_start = index;
	while index < end && bytes[index].is_ascii_digit() {
		index += 1;
	}
	if index == digits_start {
		return None;
	}
	if index < end && matches!(bytes[index], b':' | b'|') {
		Some(&line[index + 1..])
	} else {
		None
	}
}

fn is_literal_value(text: &str) -> bool {
	let trimmed = text.trim();
	let trimmed = trimmed.strip_suffix(',').unwrap_or(trimmed).trim();
	if trimmed.len() >= 2 {
		let first = trimmed.as_bytes()[0];
		let last = trimmed.as_bytes()[trimmed.len() - 1];
		if (first == b'"' || first == b'\'') && first == last {
			return true;
		}
	}
	trimmed.parse::<f64>().is_ok()
}

/// Strip one read-output line-number prefix from every bare row, but only when
/// *all* bare rows carry one and the remainders are not numeric-keyed literals.
fn strip_uniform_prefixes(payloads: &mut [PayloadRow]) {
	let mut saw_bare = false;
	let mut all_literal_values = true;
	for row in payloads.iter() {
		if !row.bare || row.text.trim().is_empty() {
			continue;
		}
		saw_bare = true;
		let Some(stripped) = strip_one_leading_prefix(&row.text) else { return };
		all_literal_values &= is_literal_value(stripped);
	}
	if !saw_bare || all_literal_values {
		return;
	}
	for row in payloads.iter_mut() {
		if !row.bare || row.text.trim().is_empty() {
			continue;
		}
		let stripped = strip_one_leading_prefix(&row.text).map(str::to_string);
		if let Some(stripped) = stripped {
			row.text = stripped;
		}
	}
}

fn is_read_metadata_line(line: &str) -> bool {
	let trimmed = line.trim();
	if trimmed == "…" || trimmed == "..." {
		return true;
	}
	if trimmed.starts_with("[Showing lines ") || (trimmed.starts_with('[') && trimmed.contains("ln elided;")) {
		return true;
	}
	// `N-M: …` collapsed summary row.
	let Some((prefix, _)) = trimmed.split_once(':') else { return false };
	let Some((start, end)) = prefix.split_once('-') else { return false };
	let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
	digits(start) && digits(end) && trimmed.contains('…')
}

fn detect_contamination(text: &str) -> Option<String> {
	let trimmed = text.trim_start();
	if trimmed.is_empty() {
		return None;
	}
	let sentinel = ["*** Update File:", "*** Add File:", "*** Delete File:", "*** Move to:"]
		.iter()
		.copied()
		.any(|marker| trimmed.starts_with(marker));
	if sentinel {
		let preview: String = trimmed.chars().take(48).collect();
		return Some(format!(
			"apply_patch sentinel {preview:?} is not valid in hashline. File sections start with `[path#HASH]` \
			 (no `Update File:` / `Add File:` keyword). Use `PUT N.=M:`, `CUT N.=M`, or `PUT <N:`/`PUT >N:` ops."
		));
	}
	if trimmed.starts_with("@@") {
		let preview: String = trimmed.chars().take(48).collect();
		return Some(format!(
			"`@@`-bracketed hunk header {preview:?} is not valid in hashline. \
			 Drop the `@@ ... @@` brackets and write a header such as `PUT N.=M:`."
		));
	}
	if trimmed.bytes().all(|byte| byte.is_ascii_digit()) {
		return Some(format!(
			"hunk headers need a verb and both endpoints. Use `PUT {trimmed}.={trimmed}:` to replace, \
			 or `CUT {trimmed}.={trimmed}` to delete."
		));
	}
	None
}

fn commit_deferred_blanks(pending: &mut Pending, warnings: &mut Vec<String>) {
	if pending.deferred_blanks.is_empty() {
		return;
	}
	push_warning(warnings, BARE_BODY_AUTO_PIPED_WARNING);
	let blanks = std::mem::take(&mut pending.deferred_blanks);
	pending.payloads.extend(blanks);
}

fn handle_blank(pending: &mut Option<Pending>, line_num: u32) {
	let Some(pending) = pending.as_mut() else { return };
	if bodyless_message(&pending.target, pending.had_colon).is_some() {
		return;
	}
	if pending.payloads.is_empty() {
		return;
	}
	pending
		.deferred_blanks
		.push(PayloadRow { text: String::new(), bare: true, line_num, minus: false });
}

/// Upstream `MD_BULLET_ROW_RE`: `-` followed by one space and a non-space.
fn is_md_bullet(text: &str) -> bool {
	text
		.trim_start()
		.strip_prefix("- ")
		.is_some_and(|rest| rest.chars().next().is_some_and(|ch| !ch.is_whitespace()))
}

/// Judge bare `-` body rows once the whole hunk body is known: Markdown bullets
/// are literal content, unified-diff `-old` rows paired with `+new` rows are
/// dropped, anything else is rejected so source cannot be silently corrupted.
fn resolve_minus_rows(payloads: &mut Vec<PayloadRow>, warnings: &mut Vec<String>) -> Result<(), String> {
	let mut first_minus: Option<u32> = None;
	let mut all_bullet = true;
	let mut has_explicit = false;
	let mut has_explicit_bullet = false;
	for row in payloads.iter() {
		if row.minus {
			if first_minus.is_none() {
				first_minus = Some(row.line_num);
			}
			all_bullet &= is_md_bullet(&row.text);
		} else if !row.bare {
			has_explicit = true;
			has_explicit_bullet |= is_md_bullet(&row.text);
		}
	}
	let Some(first_minus) = first_minus else { return Ok(()) };
	if all_bullet && (!has_explicit || has_explicit_bullet) {
		push_warning(warnings, MINUS_BULLET_AUTO_PIPED_WARNING);
		return Ok(());
	}
	if has_explicit && !all_bullet {
		payloads.retain(|row| !row.minus);
		push_warning(warnings, DIFF_OLD_ROWS_IGNORED_WARNING);
		return Ok(());
	}
	Err(format!("line {first_minus}: {MINUS_ROW_REJECTED}"))
}

fn cursor_for(target: &Target) -> Option<Cursor> {
	match target {
		Target::InsertBefore { anchor, .. } => Some(Cursor::Before(*anchor)),
		Target::InsertAfter { anchor, .. } => Some(Cursor::After(*anchor)),
		Target::Bof { .. } => Some(Cursor::Bof),
		Target::Eof { .. } => Some(Cursor::Eof),
		_ => None,
	}
}

fn push_deletes(edits: &mut Vec<Edit>, range: Range, line_num: u32) {
	for line in range.start..=range.end {
		edits.push(Edit::Delete { line, line_num });
	}
}

fn flush_pending(
	pending: Option<Pending>,
	edits: &mut Vec<Edit>,
	warnings: &mut Vec<String>,
) -> Result<(), String> {
	let Some(mut pending) = pending else { return Ok(()) };
	resolve_minus_rows(&mut pending.payloads, warnings)?;
	strip_uniform_prefixes(&mut pending.payloads);
	let payloads: Vec<String> = pending.payloads.into_iter().map(|row| row.text).collect();
	let line_num = pending.line_num;
	match pending.target {
		Target::Rem | Target::Move(_) => Ok(()),
		Target::Cut { range, register, .. } => {
			edits.push(Edit::Cut { range, register, line_num });
			push_deletes(edits, range, line_num);
			Ok(())
		}
		Target::CutBlock { anchor, register } => {
			edits.push(Edit::Block {
				anchor,
				payloads: Vec::new(),
				mode: BlockMode::Cut,
				register,
				line_num,
			});
			Ok(())
		}
		Target::Replace { range, register } => {
			if register.is_some() {
				edits.push(Edit::Paste { at: PasteAt::Span(range), register, line_num });
				return Ok(());
			}
			if payloads.is_empty() {
				if !pending.had_colon {
					return Err(format!("line {line_num}: {COLONLESS_SPAN_PUT}"));
				}
				push_deletes(edits, range, line_num);
				push_warning(warnings, EMPTY_PUT_AUTO_CUT_WARNING);
				return Ok(());
			}
			for text in payloads {
				edits.push(Edit::Insert {
					cursor: Cursor::Before(range.start),
					text,
					line_num,
					replacement: true,
				});
			}
			push_deletes(edits, range, line_num);
			Ok(())
		}
		Target::Block { anchor, register } => {
			if register.is_some() {
				edits.push(Edit::Block {
					anchor,
					payloads: Vec::new(),
					mode: BlockMode::Replace,
					register,
					line_num,
				});
				return Ok(());
			}
			if payloads.is_empty() {
				if !pending.had_colon {
					return Err(format!("line {line_num}: {COLONLESS_SPAN_PUT}"));
				}
				edits.push(Edit::Block {
					anchor,
					payloads: Vec::new(),
					mode: BlockMode::Replace,
					register: None,
					line_num,
				});
				push_warning(warnings, EMPTY_PUT_AUTO_CUT_WARNING);
				return Ok(());
			}
			edits.push(Edit::Block {
				anchor,
				payloads,
				mode: BlockMode::Replace,
				register: None,
				line_num,
			});
			Ok(())
		}
		Target::InsertAfterBlock { anchor, register } => {
			if register.is_some() || (!pending.had_colon && payloads.is_empty()) {
				edits.push(Edit::Block {
					anchor,
					payloads: Vec::new(),
					mode: BlockMode::PasteAfter,
					register,
					line_num,
				});
				return Ok(());
			}
			if payloads.is_empty() {
				return Err(format!("line {line_num}: {EMPTY_INSERT}"));
			}
			edits.push(Edit::Block {
				anchor,
				payloads,
				mode: BlockMode::InsertAfter,
				register: None,
				line_num,
			});
			Ok(())
		}
		target => {
			let Some(cursor) = cursor_for(&target) else {
				return Ok(());
			};
			if target.register().is_some() || (!pending.had_colon && payloads.is_empty()) {
				edits.push(Edit::Paste { at: PasteAt::Gap(cursor), register: target.register().map(str::to_string), line_num });
				return Ok(());
			}
			if payloads.is_empty() {
				return Err(format!("line {line_num}: {EMPTY_INSERT}"));
			}
			for text in payloads {
				edits.push(Edit::Insert { cursor, text, line_num, replacement: false });
			}
			Ok(())
		}
	}
}

#[derive(Debug, Default)]
struct OverlapHunk {
	line_num: u32,
	source_lines: BTreeSet<u32>,
	clipboard_dependent: bool,
}

fn normalize_overlapping_ranges(edits: &mut Vec<Edit>, warnings: &mut Vec<String>) -> Result<(), String> {
	let mut hunks: Vec<OverlapHunk> = Vec::new();
	let mut index_by_line: HashMap<u32, usize> = HashMap::new();
	for edit in edits.iter() {
		match edit {
			Edit::Cut { line_num, .. } => {
				let index = *index_by_line.entry(*line_num).or_insert_with(|| {
					hunks.push(OverlapHunk { line_num: *line_num, ..OverlapHunk::default() });
					hunks.len() - 1
				});
				hunks[index].clipboard_dependent = true;
			}
			Edit::Paste { at: PasteAt::Span(range), line_num, .. } => {
				let index = *index_by_line.entry(*line_num).or_insert_with(|| {
					hunks.push(OverlapHunk { line_num: *line_num, ..OverlapHunk::default() });
					hunks.len() - 1
				});
				hunks[index].clipboard_dependent = true;
				for line in range.start..=range.end {
					hunks[index].source_lines.insert(line);
				}
			}
			Edit::Delete { line, line_num } => {
				let index = *index_by_line.entry(*line_num).or_insert_with(|| {
					hunks.push(OverlapHunk { line_num: *line_num, ..OverlapHunk::default() });
					hunks.len() - 1
				});
				hunks[index].source_lines.insert(*line);
			}
			_ => {}
		}
	}

	let mut owner_by_line: HashMap<u32, usize> = HashMap::new();
	let mut dropped: BTreeSet<u32> = BTreeSet::new();
	for hunk_index in 0..hunks.len() {
		if hunks[hunk_index].source_lines.is_empty() {
			continue;
		}
		let mut overlaps: Vec<usize> = Vec::new();
		let mut first_overlap = None;
		for line in hunks[hunk_index].source_lines.iter() {
			if let Some(owner) = owner_by_line.get(line).copied() {
				if !overlaps.contains(&owner) {
					overlaps.push(owner);
				}
				first_overlap.get_or_insert(*line);
			}
		}
		if overlaps.is_empty() {
			for line in hunks[hunk_index].source_lines.iter() {
				owner_by_line.insert(*line, hunk_index);
			}
			continue;
		}
		let previous = if overlaps.len() == 1 { Some(overlaps[0]) } else { None };
		let exact = previous.is_some_and(|previous| {
			hunks[previous].source_lines.len() == hunks[hunk_index].source_lines.len()
				&& hunks[hunk_index]
					.source_lines
					.iter()
					.all(|line| hunks[previous].source_lines.contains(line))
		});
		if let (true, Some(previous)) = (exact, previous) {
			if !hunks[previous].clipboard_dependent {
				dropped.insert(hunks[previous].line_num);
				owner_by_line.retain(|_, owner| *owner != previous);
				for line in hunks[hunk_index].source_lines.iter() {
					owner_by_line.insert(*line, hunk_index);
				}
				push_warning(warnings, REPLACE_PAIR_COALESCED_WARNING);
				continue;
			}
		}
		let previous_label = previous
			.map(|previous| hunks[previous].line_num.to_string())
			.unwrap_or_else(|| "an earlier line".to_string());
		return Err(format!(
			"line {}: anchor line {} is already targeted by another hunk on line {previous_label}. \
			 Issue ONE hunk per range; payload is only the final desired content, never a before/after pair.",
			hunks[hunk_index].line_num,
			first_overlap.unwrap_or(0)
		));
	}

	if !dropped.is_empty() {
		edits.retain(|edit| !dropped.contains(&edit.line_num()));
	}
	Ok(())
}

/// Parse one section body into edits, an optional file op, and warnings.
pub fn parse_section_body(diff: &str) -> Result<ParsedPatch, String> {
	let mut edits: Vec<Edit> = Vec::new();
	let mut warnings: Vec<String> = Vec::new();
	let mut file_op: Option<FileOp> = None;
	let mut pending: Option<Pending> = None;
	// Declared expansion cost, charged before any edit is materialized so a
	// small input cannot ask for an unbounded number of deletes/inserts.
	let mut declared_lines: u64 = 0;
	let mut declared_rows: u64 = 0;

	for (index, raw) in diff.split('\n').enumerate() {
		let line_num = (index + 1) as u32;
		let line = raw.strip_suffix('\r').unwrap_or(raw);

		if line.is_empty() {
			handle_blank(&mut pending, line_num);
			continue;
		}
		if marker_eq(line, BEGIN_PATCH_MARKER) {
			continue;
		}
		if marker_eq(line, END_PATCH_MARKER) || marker_eq(line, ABORT_MARKER) {
			break;
		}

		if let Some(hunk) = try_parse_hunk_header(line) {
			let declared_range = match &hunk.target {
				Target::Replace { range, .. } => {
					validate_range(*range, line_num, "replace")?;
					Some(*range)
				}
				Target::Cut { range, .. } => {
					validate_range(*range, line_num, "cut")?;
					Some(*range)
				}
				_ => None,
			};
			if let Some(range) = declared_range {
				declared_lines = declared_lines.saturating_add((range.end - range.start + 1) as u64);
				check_section_expansion(declared_lines, line_num)?;
			}
			if hunk.had_colon && matches!(hunk.target, Target::Cut { .. } | Target::CutBlock { .. }) {
				push_warning(&mut warnings, CUT_COLON_IGNORED_WARNING);
			}
			if hunk.had_colon
				&& !matches!(hunk.target, Target::Rem | Target::Move(_))
				&& hunk.target.register().is_some()
			{
				return Err(format!("line {line_num}: {COLON_ON_REGISTER_PUT}"));
			}
			match hunk.target {
				Target::Rem => {
					flush_pending(pending.take(), &mut edits, &mut warnings)?;
					if file_op.is_some() {
						return Err(format!(
							"line {line_num}: only one file-level op (`REM` or `MV`) per section. \
							 Merge them under one header."
						));
					}
					if !edits.is_empty() {
						return Err(format!("line {line_num}: {REM_TAKES_NO_BODY}"));
					}
					file_op = Some(FileOp::Rem);
				}
				Target::Move(dest) => {
					flush_pending(pending.take(), &mut edits, &mut warnings)?;
					if file_op.is_some() {
						return Err(format!(
							"line {line_num}: only one file-level op (`REM` or `MV`) per section. \
							 Merge them under one header."
						));
					}
					file_op = Some(FileOp::Move(dest));
				}
				target => {
					flush_pending(pending.take(), &mut edits, &mut warnings)?;
					pending = Some(Pending {
						target,
						line_num,
						payloads: Vec::new(),
						had_colon: hunk.had_colon,
						deferred_blanks: Vec::new(),
					});
				}
			}
			continue;
		}

		if let Some(text) = line.strip_prefix('+') {
			if file_op.is_some() {
				return Err(format!("line {line_num}: {MOVE_TAKES_NO_BODY}"));
			}
			let Some(active) = pending.as_mut() else {
				return Err(format!(
					"line {line_num}: payload line has no preceding hunk header. Got {:?}. \
					 Use `PUT N.=M:`, `CUT N.=M`, or `PUT <N:`/`PUT >N:` above the body.",
					line
				));
			};
			if let Some(message) = bodyless_message(&active.target, active.had_colon) {
				return Err(format!("line {line_num}: {message}"));
			}
			commit_deferred_blanks(active, &mut warnings);
			if is_hunk_header_text(text) {
				push_warning(
					&mut warnings,
					&format!(
						"line {line_num}: body row `+{text}` is itself a valid hunk header, so it was inserted \
						 into the file as literal text rather than executed. Ops are never `+`-prefixed — drop the `+` \
						 to run it, and re-issue if this line landed in the file by mistake."
					),
				);
			}
			declared_rows += 1;
			check_section_rows(declared_rows, line_num)?;
			active
				.payloads
				.push(PayloadRow { text: text.to_string(), bare: false, line_num, minus: false });
			continue;
		}

		// Raw (unprefixed) row.
		if pending.is_none() && is_read_metadata_line(line) {
			push_warning(&mut warnings, READ_METADATA_IGNORED_WARNING);
			continue;
		}
		if let Some(message) = detect_contamination(line) {
			return Err(format!("line {line_num}: {message}"));
		}
		if file_op.is_some() {
			return Err(format!("line {line_num}: {MOVE_TAKES_NO_BODY}"));
		}
		if line.trim().is_empty() {
			handle_blank(&mut pending, line_num);
			continue;
		}
		let Some(active) = pending.as_mut() else {
			return Err(format!(
				"line {line_num}: payload line has no preceding hunk header. \
				 Use `PUT N.=M:`, `CUT N.=M`, or `PUT <N:`/`PUT >N:` above the body. Got {line:?}."
			));
		};
		if let Some(message) = bodyless_message(&active.target, active.had_colon) {
			return Err(format!("line {line_num}: {message}"));
		}
		let minus = line.trim_start().starts_with('-');
		if !minus {
			push_warning(&mut warnings, BARE_BODY_AUTO_PIPED_WARNING);
		}
		commit_deferred_blanks(active, &mut warnings);
		declared_rows += 1;
		check_section_rows(declared_rows, line_num)?;
		active
			.payloads
			.push(PayloadRow { text: line.to_string(), bare: true, line_num, minus });
	}

	flush_pending(pending.take(), &mut edits, &mut warnings)?;
	if matches!(file_op, Some(FileOp::Rem)) && !edits.is_empty() {
		return Err("`REM` deletes the whole file and cannot be combined with line ops.".to_string());
	}
	normalize_overlapping_ranges(&mut edits, &mut warnings)?;
	if edits.len() as u64 > MAX_SECTION_EXPANDED_LINES {
		return Err(format!(
			"section expands to {} edits, above the {MAX_SECTION_EXPANDED_LINES}-edit limit",
			edits.len()
		));
	}
	Ok(ParsedPatch { edits, file_op, warnings })
}

/// Lines targeted by anchored edits (1-indexed, sorted, deduplicated).
pub fn collect_anchor_lines(edits: &[Edit]) -> Vec<u32> {
	let mut lines = BTreeSet::new();
	for edit in edits {
		match edit {
			Edit::Delete { line, .. } => {
				lines.insert(*line);
			}
			Edit::Cut { range, .. } => {
				for line in range.start..=range.end {
					lines.insert(line);
				}
			}
			Edit::Paste { at, .. } => match at {
				PasteAt::Span(range) => {
					for line in range.start..=range.end {
						lines.insert(line);
					}
				}
				PasteAt::Gap(Cursor::Before(line) | Cursor::After(line)) => {
					lines.insert(*line);
				}
				PasteAt::Gap(_) => {}
			},
			Edit::Block { anchor, .. } => {
				lines.insert(*anchor);
			}
			Edit::Insert { cursor, .. } => match cursor {
				Cursor::Before(line) | Cursor::After(line) => {
					lines.insert(*line);
				}
				Cursor::Bof | Cursor::Eof => {}
			},
		}
	}
	lines.into_iter().collect()
}

/// Whether any edit anchors to concrete file content. Head/tail inserts do not:
/// they are safe to apply when the file drifted.
pub fn has_anchor_scoped_edit(edits: &[Edit]) -> bool {
	edits.iter().any(|edit| match edit {
		Edit::Delete { .. } | Edit::Block { .. } | Edit::Cut { .. } => true,
		Edit::Paste { at, .. } => match at {
			PasteAt::Span(_) => true,
			PasteAt::Gap(Cursor::Before(_) | Cursor::After(_)) => true,
			PasteAt::Gap(_) => false,
		},
		Edit::Insert { cursor, .. } => matches!(cursor, Cursor::Before(_) | Cursor::After(_)),
	})
}

/// Max 100_000-line cap test hook for callers building ranges themselves.
pub fn max_expanded_range_lines() -> u64 {
	MAX_EXPANDED_RANGE_LINES
}
