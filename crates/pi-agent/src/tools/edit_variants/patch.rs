//! `patch` and `apply_patch` engines: unified-diff hunks applied to one file.
//!
//! Ports `packages/coding-agent/src/edit/diff.ts` (hunk parsing, sequence
//! matching, fuzzy fallbacks) and `modes/patch.ts` (replacement planning and
//! per-file create/update/delete/move). Every hunk is located before any
//! write; within one file the replacements form a single atomic commit.
//! Multi-entry calls apply in order and are not atomic across entries, which
//! matches upstream `apply_patch` semantics.

use regex::Regex;
use serde_json::Value;

use super::{
	fsops, paths,
	replace::{adjust_indentation, find_match},
	text::{self, DEFAULT_FUZZY_THRESHOLD},
};
use crate::{
	state::{AgentContext, ToolResult},
	tools::ToolError,
};

const EOF_MARKER: &str = "*** End of File";
const CHANGE_CONTEXT_MARKER: &str = "@@ ";
const EMPTY_CHANGE_CONTEXT_MARKER: &str = "@@";
const SEQUENCE_FUZZY_THRESHOLD: f64 = 0.92;
const CONTEXT_FUZZY_THRESHOLD: f64 = 0.8;
const PARTIAL_MATCH_MIN_LENGTH: usize = 6;
const PARTIAL_MATCH_MIN_RATIO: f64 = 0.3;
const DOMINANT_FUZZY_MIN_CONFIDENCE: f64 = 0.97;
const DOMINANT_FUZZY_DELTA: f64 = 0.08;
const MAX_RECORDED_MATCHES: usize = 5;
const AMBIGUITY_HINT_WINDOW: usize = 200;
const MATCH_PREVIEW_CONTEXT: usize = 2;
const MATCH_PREVIEW_MAX_LEN: usize = 80;

const DIFF_METADATA_PREFIXES: [&str; 15] = [
	"*** Update File:",
	"*** Add File:",
	"*** Delete File:",
	"diff --git ",
	"index ",
	"--- ",
	"+++ ",
	"new file mode ",
	"deleted file mode ",
	"rename from ",
	"rename to ",
	"similarity index ",
	"dissimilarity index ",
	"old mode ",
	"new mode ",
];
const MULTI_FILE_MARKERS: [&str; 4] = ["*** Update File:", "*** Add File:", "*** Delete File:", "diff --git "];
const PATCH_WRAPPER_PREFIXES: [&str; 2] = ["*** Begin Patch", "*** End Patch"];

pub const PATCH_DESCRIPTION: &str = "Patches files given diff hunks. Primary tool for existing-file edits.\n\n\
Input is `{ path, edits }`; `path` applies to every entry. Each entry is one of:\n\
- `{ op: \"update\", diff }` — one or more `@@` hunks with ` `, `+`, `-` body lines.\n\
- `{ op: \"create\", diff }` — full new file content, no prefixes.\n\
- `{ op: \"delete\" }` — delete the file.\n\
- `{ op: \"update\", rename, diff }` — update then move to `rename`.\n\n\
`@@` alone uses context lines; `@@ <anchor>` names a copied line/substring. \
Copy anchors/context verbatim; a non-unique match is refused. A rename \
destination MUST NOT exist. Only explicit `delete` entries and `rename` move \
files.";

pub const APPLY_PATCH_DESCRIPTION: &str = "Edit files: `apply_patch` shell command.\n\n\
`*** Begin Patch` / `*** End Patch` envelope with file sections:\n\
`*** Add File: <path>` (following `+` lines are contents), \
`*** Delete File: <path>`, or `*** Update File: <path>` with optional \
`*** Move to: <new path>` then `@@` hunks. Hunk lines start with space, `-`, \
or `+`. Add refuses an existing file; Move refuses an existing destination. \
Paths are workspace-relative.";

pub fn schema() -> Value {
	serde_json::json!({
		"type": "object",
		"properties": {
			"path": { "type": "string", "description": "File every entry applies to." },
			"edits": {
				"type": "array",
				"description": "Entries applied in order.",
				"items": {
					"type": "object",
					"properties": {
						"op": { "type": "string", "enum": ["create", "delete", "update"] },
						"rename": { "type": "string", "description": "New path for an update." },
						"diff": { "type": "string", "description": "Full content for create; `@@` hunks for update." }
					}
				}
			}
		},
		"required": ["path", "edits"]
	})
}

/// One file operation lowered from `patch` or `apply_patch`.
#[derive(Debug, Clone)]
pub struct PatchInput {
	pub path: String,
	pub op: Operation,
	pub rename: Option<String>,
	pub diff: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
	Create,
	Delete,
	Update,
}

impl Operation {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Create => "create",
			Self::Delete => "delete",
			Self::Update => "update",
		}
	}
}

#[derive(Debug, Clone, Default)]
pub struct DiffHunk {
	pub change_context: Option<String>,
	pub old_start_line: Option<usize>,
	pub new_start_line: Option<usize>,
	pub has_context_lines: bool,
	pub old_lines: Vec<String>,
	pub new_lines: Vec<String>,
	pub is_end_of_file: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
	Exact,
	TrimTrailing,
	Trim,
	CommentPrefix,
	Unicode,
	Prefix,
	Substring,
	Fuzzy,
	FuzzyDominant,
	Character,
}

#[derive(Debug, Clone, Default)]
pub struct SequenceResult {
	pub index: Option<usize>,
	pub confidence: f64,
	pub match_count: Option<usize>,
	pub match_indices: Vec<usize>,
	pub strategy: Option<Strategy>,
}

#[derive(Debug, Clone, Default)]
pub struct ContextResult {
	pub index: Option<usize>,
	pub confidence: f64,
	pub match_count: Option<usize>,
	pub match_indices: Vec<usize>,
	pub strategy: Option<Strategy>,
}

/// Result of one applied file operation.
#[derive(Debug, Clone)]
pub struct FileChange {
	pub path: String,
	pub new_path: Option<String>,
	pub old_content: Option<String>,
	pub new_content: Option<String>,
	pub warnings: Vec<String>,
	pub op: Operation,
}

/// `patch` mode entry point.
pub async fn execute_patch(args: &Value, ctx: &AgentContext) -> Result<ToolResult, ToolError> {
	let path = super::require_str(args, "path")?;
	let edits = args
		.get("edits")
		.and_then(Value::as_array)
		.ok_or_else(|| ToolError::InvalidArguments("edits must be an array".to_string()))?;
	if edits.is_empty() {
		return Err(ToolError::InvalidArguments("edits must not be empty".to_string()));
	}
	let mut inputs = Vec::with_capacity(edits.len());
	for entry in edits {
		let op = match entry.get("op").and_then(Value::as_str) {
			None | Some("update") => Operation::Update,
			Some("create") => Operation::Create,
			Some("delete") => Operation::Delete,
			Some(other) => {
				return Err(ToolError::InvalidArguments(format!(
					"op must be create, delete, or update (got '{other}')"
				)));
			},
		};
		inputs.push(PatchInput {
			path: path.clone(),
			op,
			rename: entry.get("rename").and_then(Value::as_str).map(str::to_string),
			diff: entry.get("diff").and_then(Value::as_str).unwrap_or("").to_string(),
		});
	}
	let changes = apply_inputs(&inputs, &ctx.cwd, &ctx.cancellation_token, true).await?;
	Ok(render_changes(changes, "Success. Updated the following files:"))
}

/// Apply a list of file operations in order; not atomic across entries.
pub async fn apply_inputs(
	inputs: &[PatchInput],
	cwd: &Path,
	token: &tokio_util::sync::CancellationToken,
	allow_create_overwrite: bool,
) -> Result<Vec<FileChange>, ToolError> {
	let mut changes = Vec::with_capacity(inputs.len());
	for input in inputs {
		let change = match apply_one(input, cwd, token, allow_create_overwrite).await {
			Ok(change) => change,
			Err(err) => return Err(decorate_partial(err, &changes)),
		};
		changes.push(change);
	}
	if changes.is_empty() {
		return Err(ToolError::ExecutionFailed("No files were modified.".to_string()));
	}
	Ok(changes)
}

fn decorate_partial(err: ToolError, landed: &[FileChange]) -> ToolError {
	if landed.is_empty() {
		return err;
	}
	let paths: Vec<&str> = landed.iter().map(|change| change.path.as_str()).collect();
	ToolError::ExecutionFailed(format!(
		"{err}\n\nThe earlier operations already landed for: {}. The remaining operations did not run.",
		paths.join(", ")
	))
}

pub async fn apply_one(
	input: &PatchInput,
	cwd: &Path,
	token: &tokio_util::sync::CancellationToken,
	allow_create_overwrite: bool,
) -> Result<FileChange, ToolError> {
	let must_exist = input.op != Operation::Create;
	let (absolute, display) = paths::resolve_edit_path(&input.path, cwd, must_exist, token).await?;
	let (dest_absolute, dest_display) = match &input.rename {
		Some(rename) => {
			let (dest, dest_display) = paths::resolve_edit_path(rename, cwd, false, token).await?;
			if dest == absolute {
				return Err(ToolError::ExecutionFailed("rename path is the same as source path".to_string()));
			}
			if tokio::fs::symlink_metadata(&dest).await.is_ok() {
				return Err(ToolError::ExecutionFailed(format!(
					"Cannot rename {display} to {dest_display}: destination already exists."
				)));
			}
			(Some(dest), Some(dest_display))
		},
		None => (None, None),
	};

	match input.op {
		Operation::Create => {
			if input.diff.is_empty() {
				return Err(ToolError::ExecutionFailed("Create operation requires diff (file content)".to_string()));
			}
			let normalized = normalize_create_content(&input.diff);
			let content = if normalized.ends_with('\n') { normalized } else { format!("{normalized}\n") };
			fsops::create_text(&absolute, &display, &content, token, !allow_create_overwrite).await?;
			Ok(FileChange {
				path: display,
				new_path: None,
				old_content: None,
				new_content: Some(content),
				warnings: Vec::new(),
				op: Operation::Create,
			})
		},
		Operation::Delete => {
			let old = fsops::read_text(&absolute, &display).await?;
			fsops::delete_file(&absolute, &display, token).await?;
			Ok(FileChange {
				path: display,
				new_path: None,
				old_content: Some(old),
				new_content: None,
				warnings: Vec::new(),
				op: Operation::Delete,
			})
		},
		Operation::Update => {
			if input.diff.is_empty() {
				return Err(ToolError::ExecutionFailed("Update operation requires diff (hunks)".to_string()));
			}
			let raw = fsops::read_text(&absolute, &display).await?;
			let (bom, stripped, ending) = text::normalize_file(&raw);
			let hunks = parse_diff_hunks(&input.diff)?;
			if hunks.is_empty() {
				return Err(ToolError::ExecutionFailed("Diff contains no hunks".to_string()));
			}
			let (new_content, warnings) =
				apply_hunks_to_content(&stripped, &display, &hunks, DEFAULT_FUZZY_THRESHOLD, true)?;
			let final_content = text::denormalize_file(&bom, &new_content, ending);
			match (&dest_absolute, &dest_display) {
				(Some(dest), Some(dest_display)) => {
					fsops::move_file(&absolute, &display, dest, dest_display, &raw, &final_content, token).await?;
				},
				_ => {
					fsops::commit_text(&absolute, &display, &raw, &final_content, token, None).await?;
				},
			}
			Ok(FileChange {
				path: display,
				new_path: dest_display,
				old_content: Some(raw),
				new_content: Some(final_content),
				warnings,
				op: Operation::Update,
			})
		},
	}
}

// ── Result shaping ────────────────────────────────────────────────────────

pub fn render_changes(changes: Vec<FileChange>, header: &str) -> ToolResult {
	let mut lines = vec![header.to_string()];
	let mut per_file = Vec::new();
	let mut all_warnings = Vec::new();
	for change in &changes {
		lines.push(format!("{} {}", marker(change.op), change.path));
		if let Some(new_path) = &change.new_path {
			lines.push(format!("M {} -> {new_path}", change.path));
		}
		if !change.warnings.is_empty() {
			lines.push(format!("Warnings for {}:", change.path));
			lines.extend(change.warnings.iter().cloned());
			all_warnings.extend(change.warnings.iter().cloned());
		}
		let diff = match (&change.old_content, &change.new_content) {
			(Some(old), Some(new)) => text::unified_diff(&change.path, old, new),
			(None, Some(new)) => text::unified_diff(&change.path, "", new),
			(Some(old), None) => text::unified_diff(&change.path, old, ""),
			(None, None) => String::new(),
		};
		per_file.push(serde_json::json!({
			"path": change.path,
			"newPath": change.new_path,
			"op": change.op.as_str(),
			"diff": diff,
		}));
	}
	super::result(
		lines.join("\n"),
		serde_json::json!({
			"files": per_file,
			"warnings": all_warnings,
			"diff": changes.iter().filter_map(|change| {
				if let (Some(old), Some(new)) = (&change.old_content, &change.new_content) {
					let path = change.new_path.clone().unwrap_or_else(|| change.path.clone());
					Some(text::unified_diff(&path, old, new))
				} else { None }
			}).collect::<Vec<_>>().join("\n"),
		}),
	)
}

fn marker(op: Operation) -> char {
	match op {
		Operation::Create => 'A',
		Operation::Delete => 'D',
		Operation::Update => 'M',
	}
}

fn normalize_create_content(content: &str) -> String {
	let lines: Vec<&str> = content.split('\n').collect();
	let non_empty: Vec<&&str> = lines.iter().filter(|line| !line.is_empty()).collect();
	if !non_empty.is_empty() && non_empty.iter().all(|line| line.starts_with('+')) {
		return lines
			.iter()
			.map(|line| line.strip_prefix("+ ").or_else(|| line.strip_prefix('+')).unwrap_or(line))
			.collect::<Vec<_>>()
			.join("\n");
	}
	content.to_string()
}

// ── Diff parsing ──────────────────────────────────────────────────────────

pub fn parse_diff_hunks(diff: &str) -> Result<Vec<DiffHunk>, ToolError> {
	let multi_file = count_multi_file_markers(diff);
	if multi_file > 1 {
		return Err(ToolError::InvalidArguments(format!(
			"Diff contains {multi_file} file markers. Single-file patches cannot contain multi-file markers."
		)));
	}
	let normalized = normalize_diff(diff);
	let lines: Vec<String> = normalized.split('\n').map(str::to_string).collect();
	let mut hunks = Vec::new();
	let mut index = 0;
	while index < lines.len() {
		let line = &lines[index];
		let trimmed = line.trim();
		if trimmed.is_empty() {
			index += 1;
			continue;
		}
		let first = line.chars().next();
		let is_diff_content = matches!(first, Some(' ') | Some('+') | Some('-'));
		if !is_diff_content && is_unified_metadata_line(trimmed) {
			index += 1;
			continue;
		}
		if trimmed.starts_with("@@") && lines[index + 1..].iter().all(|next| next.trim().is_empty()) {
			break;
		}
		let (hunk, consumed) = parse_one_hunk(&lines[index..], index + 1, true)?;
		hunks.push(hunk);
		index += consumed;
	}
	Ok(hunks)
}

fn is_diff_content_line(line: &str) -> bool {
	let mut chars = line.chars();
	match chars.next() {
		Some(' ') => true,
		Some('+') => !line.starts_with("+++ "),
		Some('-') => !line.starts_with("--- "),
		_ => false,
	}
}

fn is_patch_wrapper_line(line: &str) -> bool {
	line == "***" || PATCH_WRAPPER_PREFIXES.iter().any(|prefix| line.starts_with(*prefix))
}

fn is_unified_metadata_line(line: &str) -> bool {
	DIFF_METADATA_PREFIXES
		.iter()
		.filter(|prefix| !prefix.starts_with("*** "))
		.any(|prefix| line.starts_with(*prefix))
}

fn normalize_diff(diff: &str) -> String {
	let mut lines: Vec<String> = diff.split('\n').map(str::to_string).collect();
	while let Some(last) = lines.last() {
		if last.is_empty() || (last.trim().is_empty() && !is_diff_content_line(last)) {
			lines.pop();
		} else {
			break;
		}
	}
	if lines.first().is_some_and(|line| is_patch_wrapper_line(line.trim())) {
		lines.remove(0);
	}
	if lines.last().is_some_and(|line| is_patch_wrapper_line(line.trim())) {
		lines.pop();
	}
	lines.retain(|line| is_diff_content_line(line) || !DIFF_METADATA_PREFIXES.iter().any(|prefix| line.trim().starts_with(*prefix)));
	lines.join("\n")
}

fn count_multi_file_markers(diff: &str) -> usize {
	let mut counts: Vec<(String, usize)> = Vec::new();
	let mut paths = std::collections::HashSet::new();
	for line in diff.split('\n') {
		if is_diff_content_line(line) {
			continue;
		}
		let trimmed = line.trim();
		for marker in MULTI_FILE_MARKERS {
			if trimmed.starts_with(marker) {
				if let Some(path) = extract_marker_path(trimmed) {
					paths.insert(path);
				}
				match counts.iter_mut().find(|(name, _)| name.as_str() == marker) {
					Some((_, count)) => *count += 1,
					None => counts.push((marker.to_string(), 1)),
				}
				break;
			}
		}
	}
	if !paths.is_empty() {
		return paths.len();
	}
	counts.iter().map(|(_, count)| *count).max().unwrap_or(0)
}

fn extract_marker_path(line: &str) -> Option<String> {
	if let Some(rest) = line.strip_prefix("diff --git ") {
		let parts: Vec<&str> = rest.split_whitespace().collect();
		let candidate = parts.get(1).or_else(|| parts.first())?;
		return Some(candidate.trim_start_matches("a/").trim_start_matches("b/").to_string());
	}
	for marker in ["*** Update File:", "*** Add File:", "*** Delete File:"] {
		if let Some(rest) = line.strip_prefix(marker) {
			return Some(rest.trim().to_string());
		}
	}
	None
}

fn parse_one_hunk(lines: &[String], line_number: usize, allow_missing_context: bool) -> Result<(DiffHunk, usize), ToolError> {
	if lines.is_empty() {
		return Err(parse_error("Diff does not contain any lines", line_number));
	}
	let mut change_contexts: Vec<String> = Vec::new();
	let mut old_start_line = None;
	let mut new_start_line = None;
	let header_line = &lines[0];
	let header_trimmed = header_line.trim_end();
	let is_header = header_line.starts_with("@@");
	let unified = is_header.then(|| parse_unified_header(header_trimmed)).flatten();
	let empty_context_marker = is_empty_context_marker(header_trimmed);
	let mut start_index;
	if is_header && (header_trimmed == EMPTY_CHANGE_CONTEXT_MARKER || empty_context_marker) {
		start_index = 1;
	} else if let Some(unified) = unified {
		if unified.old_start < 1 || unified.new_start < 1 {
			return Err(parse_error("Line numbers in @@ header must be >= 1", line_number));
		}
		if let Some(context) = unified.context {
			change_contexts.push(context);
		}
		old_start_line = Some(unified.old_start);
		new_start_line = Some(unified.new_start);
		start_index = 1;
	} else if is_header && header_trimmed.starts_with(CHANGE_CONTEXT_MARKER) {
		let context_value = &header_trimmed[CHANGE_CONTEXT_MARKER.len()..];
		let trimmed_context = context_value.trim();
		let normalized_context = strip_leading_context_marker(trimmed_context);
		if let Some(hint) = parse_line_hint(normalized_context) {
			old_start_line = Some(hint);
			new_start_line = Some(hint);
			if hint < 1 {
				return Err(parse_error("Line hint must be >= 1", line_number));
			}
		} else if is_top_of_file(normalized_context) {
			old_start_line = Some(1);
			new_start_line = Some(1);
		} else if !trimmed_context.is_empty() {
			change_contexts.push(context_value.to_string());
		}
		start_index = 1;
	} else if is_header {
		let context_value = header_trimmed[2..].trim();
		if !context_value.is_empty() {
			change_contexts.push(context_value.to_string());
		}
		start_index = 1;
	} else {
		if !allow_missing_context {
			return Err(parse_error(
				&format!("Expected hunk to start with @@ context marker, got: '{}'", lines[0]),
				line_number,
			));
		}
		start_index = 0;
	}
	if old_start_line.is_some_and(|line| line < 1) {
		return Err(parse_error("Line numbers must be >= 1", line_number));
	}
	if new_start_line.is_some_and(|line| line < 1) {
		return Err(parse_error("Line numbers must be >= 1", line_number));
	}

	while start_index < lines.len() {
		let next = &lines[start_index];
		if !next.starts_with("@@") {
			break;
		}
		let trimmed = next.trim_end();
		if trimmed.starts_with(CHANGE_CONTEXT_MARKER) {
			let nested = &trimmed[CHANGE_CONTEXT_MARKER.len()..];
			if !nested.trim().is_empty() {
				change_contexts.push(nested.to_string());
			}
			start_index += 1;
		} else if trimmed == EMPTY_CHANGE_CONTEXT_MARKER {
			start_index += 1;
		} else {
			break;
		}
	}
	if start_index >= lines.len() {
		return Err(parse_error("Hunk does not contain any lines", line_number + 1));
	}

	let change_context = if change_contexts.is_empty() {
		None
	} else {
		Some(change_contexts.join("\n"))
	};
	let mut hunk = DiffHunk {
		change_context,
		old_start_line,
		new_start_line,
		..Default::default()
	};
	let mut parsed = 0usize;
	let mut index = start_index;
	while index < lines.len() {
		let line = &lines[index];
		let trimmed = line.trim();
		let next = lines.get(index + 1);
		if line.is_empty() && parsed > 0 && next.is_some_and(|next| next.trim_start().starts_with("@@")) {
			break;
		}
		if !is_diff_content_line(line) && line.trim_end() == EOF_MARKER && line.starts_with(EOF_MARKER) {
			if parsed == 0 {
				return Err(parse_error("Hunk does not contain any lines", line_number + 1));
			}
			hunk.is_end_of_file = true;
			parsed += 1;
			break;
		}
		if trimmed == "..." || trimmed == "…" {
			hunk.has_context_lines = true;
			parsed += 1;
			index += 1;
			continue;
		}
		match line.chars().next() {
			None => {
				hunk.has_context_lines = true;
				hunk.old_lines.push(String::new());
				hunk.new_lines.push(String::new());
			},
			Some(' ') => {
				hunk.has_context_lines = true;
				hunk.old_lines.push(line[1..].to_string());
				hunk.new_lines.push(line[1..].to_string());
			},
			Some('+') => hunk.new_lines.push(line[1..].to_string()),
			Some('-') => hunk.old_lines.push(line[1..].to_string()),
			Some(_) => {
				if line.starts_with("@@") {
					if parsed == 0 {
						return Err(parse_error(
							&format!(
								"Unexpected line in hunk: '{line}'. Lines must start with ' ' (context), '+' (add), or '-' (remove)"
							),
							line_number + 1,
						));
					}
					break;
				}
				hunk.has_context_lines = true;
				hunk.old_lines.push(line.clone());
				hunk.new_lines.push(line.clone());
			},
		}
		parsed += 1;
		index += 1;
	}
	if parsed == 0 {
		return Err(parse_error("Hunk does not contain any lines", line_number + start_index));
	}
	strip_line_number_prefixes(&mut hunk);
	Ok((hunk, parsed + start_index))
}

struct UnifiedHeader {
	old_start: usize,
	#[allow(dead_code)]
	old_count: usize,
	new_start: usize,
	#[allow(dead_code)]
	new_count: usize,
	context: Option<String>,
}

fn parse_unified_header(line: &str) -> Option<UnifiedHeader> {
	let regex = Regex::new(r"^@@\s*-(\d+)(?:,(\d+))?\s+\+(\d+)(?:,(\d+))?\s*@@(?:\s*(.*))?$").ok()?;
	let captures = regex.captures(line)?;
	let old_start = captures.get(1)?.as_str().parse().ok()?;
	let old_count = captures.get(2).and_then(|value| value.as_str().parse().ok()).unwrap_or(1);
	let new_start = captures.get(3)?.as_str().parse().ok()?;
	let new_count = captures.get(4).and_then(|value| value.as_str().parse().ok()).unwrap_or(1);
	let context = captures.get(5).map(|value| value.as_str().trim().to_string()).filter(|value| !value.is_empty());
	Some(UnifiedHeader { old_start, old_count, new_start, new_count, context })
}

fn is_empty_context_marker(line: &str) -> bool {
	let rest = line.trim_start_matches("@@").trim();
	rest == "@@" || line == "@@ @@"
}

fn strip_leading_context_marker(value: &str) -> String {
	value.strip_prefix("@@").unwrap_or(value).trim().to_string()
}

fn parse_line_hint(value: &str) -> Option<usize> {
	let regex = Regex::new(r"(?i)^lines?\s+(\d+)(?:\s*-\s*(\d+))?(?:\s*@@)?$").ok()?;
	let captures = regex.captures(value)?;
	captures.get(1)?.as_str().parse().ok()
}

fn is_top_of_file(value: &str) -> bool {
	let regex = Regex::new(r"(?i)^(top|start|beginning)\s+of\s+file$").ok();
	regex.is_some_and(|regex| regex.is_match(value))
}

fn strip_line_number_prefixes(hunk: &mut DiffHunk) {
	let all: Vec<&String> = hunk.old_lines.iter().chain(hunk.new_lines.iter()).filter(|line| !line.trim().is_empty()).collect();
	if all.len() < 2 {
		return;
	}
	let regex = Regex::new(r"^\s*(\d{1,6})\s+(.+)$").expect("static regex");
	let matches: Vec<(&String, usize)> = all
		.iter()
		.filter_map(|line| regex.captures(line).and_then(|captures| Some((*line, captures.get(1)?.as_str().parse().ok()?))))
		.collect();
	let required = 2usize.max((all.len() as f64 * 0.6).ceil() as usize);
	if matches.len() < required {
		return;
	}
	let numbers: Vec<usize> = matches.iter().map(|(_, number)| *number).collect();
	let mut sequential = 0usize;
	for pair in numbers.windows(2) {
		if pair[1] == pair[0] + 1 {
			sequential += 1;
		}
	}
	if numbers.len() >= 3 && sequential < numbers.len().saturating_sub(2).max(1) {
		return;
	}
	let strip = |line: &str| -> String {
		regex
			.captures(line)
			.and_then(|captures| captures.get(2).map(|value| value.as_str().to_string()))
			.unwrap_or_else(|| line.to_string())
	};
	hunk.old_lines = hunk.old_lines.iter().map(|line| strip(line)).collect();
	hunk.new_lines = hunk.new_lines.iter().map(|line| strip(line)).collect();
}

fn parse_error(message: &str, line_number: usize) -> ToolError {
	ToolError::InvalidArguments(format!("Line {line_number}: {message}"))
}

// ── Sequence matching ─────────────────────────────────────────────────────

fn collect_matches<F>(from: usize, to: usize, predicate: F) -> (Option<usize>, usize, Vec<usize>)
where
	F: Fn(usize) -> bool,
{
	let mut first = None;
	let mut count = 0;
	let mut indices = Vec::new();
	if from > to {
		return (None, 0, indices);
	}
	for index in from..=to {
		if !predicate(index) {
			continue;
		}
		first.get_or_insert(index);
		count += 1;
		if indices.len() < MAX_RECORDED_MATCHES {
			indices.push(index);
		}
	}
	(first, count, indices)
}

fn matches_at<F>(lines: &[String], pattern: &[String], index: usize, compare: &F) -> bool
where
	F: Fn(&str, &str) -> bool,
{
	if index + pattern.len() > lines.len() {
		return false;
	}
	(0..pattern.len()).all(|offset| compare(&lines[index + offset], &pattern[offset]))
}

fn fuzzy_score_at(
	lines_norm: &[String],
	pattern_norm: &[String],
	index: usize,
	min_score: f64,
) -> f64 {
	let count = pattern_norm.len();
	if count == 0 || index + count > lines_norm.len() {
		return 0.0;
	}
	let mut total = 0.0;
	for offset in 0..count {
		let line_norm = &lines_norm[index + offset];
		let pattern_norm = &pattern_norm[offset];
		if line_norm == pattern_norm {
			total += 1.0;
			continue;
		}
		let remaining = (count - offset - 1) as f64;
		let max_len = line_norm.chars().count().max(pattern_norm.chars().count()).max(1);
		let length_delta = line_norm.chars().count().abs_diff(pattern_norm.chars().count());
		let upper = 1.0 - length_delta as f64 / max_len as f64;
		if (total + upper + remaining) / count as f64 < min_score {
			return total / count as f64;
		}
		if upper > 0.0 {
			total += text::similarity(line_norm, pattern_norm);
		}
		if (total + remaining) / count as f64 < min_score {
			return total / count as f64;
		}
	}
	total / count as f64
}

fn norm_starts_with(line: &str, pattern: &str) -> bool {
	if pattern.is_empty() {
		return line.is_empty();
	}
	line.starts_with(pattern)
}

fn norm_includes(line: &str, pattern: &str) -> bool {
	if pattern.is_empty() {
		return line.is_empty();
	}
	if pattern.len() < PARTIAL_MATCH_MIN_LENGTH || !line.contains(pattern) {
		return false;
	}
	pattern.len() as f64 / line.len().max(1) as f64 >= PARTIAL_MATCH_MIN_RATIO
}

/// Find a sequence of pattern lines using progressively looser strategies.
pub fn seek_sequence(lines: &[String], pattern: &[String], start: usize, eof: bool, allow_fuzzy: bool) -> SequenceResult {
	if pattern.is_empty() {
		return SequenceResult { index: Some(start), confidence: 1.0, strategy: Some(Strategy::Exact), ..Default::default() };
	}
	if pattern.len() > lines.len() {
		return SequenceResult::default();
	}
	let search_start = if eof && lines.len() >= pattern.len() { lines.len() - pattern.len() } else { start };
	let max_start = lines.len() - pattern.len();

	let run_exact = |from: usize, to: usize| -> Option<SequenceResult> {
		let passes: [(fn(&str, &str) -> bool, f64, Strategy); 5] = [
			(|a, b| a == b, 1.0, Strategy::Exact),
			(|a, b| a.trim_end() == b.trim_end(), 0.99, Strategy::TrimTrailing),
			(|a, b| a.trim() == b.trim(), 0.98, Strategy::Trim),
			(|a, b| text::strip_comment_prefix(a) == text::strip_comment_prefix(b), 0.975, Strategy::CommentPrefix),
			(|a, b| text::normalize_unicode(a) == text::normalize_unicode(b), 0.97, Strategy::Unicode),
		];
		for (compare, confidence, strategy) in passes {
			let (first, count, indices) = collect_matches(from, to, |index| matches_at(lines, pattern, index, &compare));
			if let Some(index) = first {
				return Some(SequenceResult {
					index: Some(index),
					confidence,
					match_count: Some(count),
					match_indices: indices,
					strategy: Some(strategy),
				});
			}
		}
		if !allow_fuzzy {
			return None;
		}
		let lines_norm: Vec<String> = lines.iter().map(|line| text::normalize_for_fuzzy(line)).collect();
		let pattern_norm: Vec<String> = pattern.iter().map(|line| text::normalize_for_fuzzy(line)).collect();
		for (compare, confidence, strategy) in [
			(norm_starts_with as fn(&str, &str) -> bool, 0.965, Strategy::Prefix),
			(norm_includes as fn(&str, &str) -> bool, 0.94, Strategy::Substring),
		] {
			let (first, count, indices) = collect_matches(from, to, |index| matches_at(&lines_norm, &pattern_norm, index, &compare));
			if let Some(index) = first {
				return Some(SequenceResult {
					index: Some(index),
					confidence,
					match_count: Some(count),
					match_indices: indices,
					strategy: Some(strategy),
				});
			}
		}
		None
	};

	if let Some(result) = run_exact(search_start, max_start) {
		return result;
	}
	if eof && search_start > start
		&& let Some(result) = run_exact(start, max_start)
	{
		return result;
	}
	if !allow_fuzzy {
		return SequenceResult::default();
	}

	let lines_norm: Vec<String> = lines.iter().map(|line| text::normalize_for_fuzzy(line)).collect();
	let pattern_norm: Vec<String> = pattern.iter().map(|line| text::normalize_for_fuzzy(line)).collect();
	let bail = SEQUENCE_FUZZY_THRESHOLD - DOMINANT_FUZZY_DELTA;
	let mut best_score = 0.0f64;
	let mut second_best = 0.0f64;
	let mut best_index = None;
	let mut fuzzy_indices = Vec::new();
	let mut fuzzy_count = 0usize;
	let mut score_range = |from: usize, to: usize| {
		if from > to {
			return;
		}
		for index in from..=to {
			let score = fuzzy_score_at(&lines_norm, &pattern_norm, index, bail);
			if score >= SEQUENCE_FUZZY_THRESHOLD {
				fuzzy_count += 1;
				if fuzzy_indices.len() < MAX_RECORDED_MATCHES {
					fuzzy_indices.push(index);
				}
			}
			if score > best_score {
				second_best = best_score;
				best_score = score;
				best_index = Some(index);
			} else if score > second_best {
				second_best = score;
			}
		}
	};
	score_range(search_start, max_start);
	if eof && search_start > start {
		score_range(start, search_start.saturating_sub(1));
	}
	if let Some(index) = best_index
		&& best_score >= SEQUENCE_FUZZY_THRESHOLD
	{
		if fuzzy_count > 1
			&& best_score >= DOMINANT_FUZZY_MIN_CONFIDENCE
			&& best_score - second_best >= DOMINANT_FUZZY_DELTA
		{
			return SequenceResult {
				index: Some(index),
				confidence: best_score,
				match_count: Some(1),
				match_indices: fuzzy_indices,
				strategy: Some(Strategy::FuzzyDominant),
			};
		}
		return SequenceResult {
			index: Some(index),
			confidence: best_score,
			match_count: Some(fuzzy_count),
			match_indices: fuzzy_indices,
			strategy: Some(Strategy::Fuzzy),
		};
	}

	// Character fallback.
	const CHARACTER_MATCH_THRESHOLD: f64 = 0.92;
	let pattern_text = pattern.join("\n");
	let content_text = lines[start.min(lines.len())..].join("\n");
	let outcome = find_match(&content_text, &pattern_text, true, CHARACTER_MATCH_THRESHOLD, &[]);
	if let Some(found) = &outcome.matched {
		let matched_prefix = &content_text[..found.start_index];
		let line_index = start + matched_prefix.matches('\n').count();
		let count = outcome.occurrences.max(outcome.fuzzy_matches).max(1);
		return SequenceResult {
			index: Some(line_index),
			confidence: found.confidence,
			match_count: Some(count),
			strategy: Some(Strategy::Character),
			..Default::default()
		};
	}
	SequenceResult {
		index: None,
		confidence: best_score,
		match_count: (outcome.occurrences.max(outcome.fuzzy_matches) > 0).then_some(outcome.occurrences.max(outcome.fuzzy_matches)),
		..Default::default()
	}
}

pub fn find_context_line(lines: &[String], context: &str, start_from: usize, allow_fuzzy: bool) -> ContextResult {
	let trimmed_context = context.trim();
	let end = lines.len().saturating_sub(1);
	if lines.is_empty() {
		return ContextResult::default();
	}
	let (first, count, indices) = collect_matches(start_from.min(lines.len()), end, |index| lines[index] == context);
	if let Some(index) = first {
		return ContextResult {
			index: Some(index),
			confidence: 1.0,
			match_count: Some(count),
			match_indices: indices,
			strategy: Some(Strategy::Exact),
		};
	}
	let (first, count, indices) =
		collect_matches(start_from.min(lines.len()), end, |index| lines[index].trim() == trimmed_context);
	if let Some(index) = first {
		return ContextResult {
			index: Some(index),
			confidence: 0.99,
			match_count: Some(count),
			match_indices: indices,
			strategy: Some(Strategy::Trim),
		};
	}
	let normalized = text::normalize_unicode(context);
	let (first, count, indices) =
		collect_matches(start_from.min(lines.len()), end, |index| text::normalize_unicode(&lines[index]) == normalized);
	if let Some(index) = first {
		return ContextResult {
			index: Some(index),
			confidence: 0.98,
			match_count: Some(count),
			match_indices: indices,
			strategy: Some(Strategy::Unicode),
		};
	}
	if !allow_fuzzy {
		return ContextResult::default();
	}
	let context_norm = text::normalize_for_fuzzy(context);
	if !context_norm.is_empty() {
		let (first, count, indices) = collect_matches(start_from.min(lines.len()), end, |index| {
			text::normalize_for_fuzzy(&lines[index]).starts_with(&context_norm)
		});
		if let Some(index) = first {
			return ContextResult {
				index: Some(index),
				confidence: 0.96,
				match_count: Some(count),
				match_indices: indices,
				strategy: Some(Strategy::Prefix),
			};
		}
	}
	if context_norm.len() >= PARTIAL_MATCH_MIN_LENGTH {
		let mut substring: Vec<(usize, f64)> = Vec::new();
		for (index, line) in lines.iter().enumerate().skip(start_from.min(lines.len())) {
			let line_norm = text::normalize_for_fuzzy(line);
			if line_norm.contains(&context_norm) {
				let ratio = context_norm.len() as f64 / line_norm.len().max(1) as f64;
				substring.push((index, ratio));
			}
		}
		if substring.len() == 1 {
			return ContextResult {
				index: Some(substring[0].0),
				confidence: 0.94,
				match_count: Some(1),
				match_indices: vec![substring[0].0],
				strategy: Some(Strategy::Substring),
			};
		}
		if !substring.is_empty() {
			let indices: Vec<usize> = substring.iter().take(MAX_RECORDED_MATCHES).map(|(index, _)| *index).collect();
			let passing: Vec<usize> = substring
				.iter()
				.filter(|(_, ratio)| *ratio >= PARTIAL_MATCH_MIN_RATIO)
				.map(|(index, _)| *index)
				.collect();
			let first = passing.first().copied().unwrap_or(substring[0].0);
			return ContextResult {
				index: Some(first),
				confidence: 0.94,
				match_count: Some(if passing.is_empty() { substring.len() } else { passing.len() }),
				match_indices: indices,
				strategy: Some(Strategy::Substring),
			};
		}
	}
	let mut best_index = None;
	let mut best_score = 0.0f64;
	let mut fuzzy_indices = Vec::new();
	let mut fuzzy_count = 0usize;
	for (index, line) in lines.iter().enumerate().skip(start_from.min(lines.len())) {
		let score = text::similarity(&text::normalize_for_fuzzy(line), &context_norm);
		if score >= CONTEXT_FUZZY_THRESHOLD {
			fuzzy_count += 1;
			if fuzzy_indices.len() < MAX_RECORDED_MATCHES {
				fuzzy_indices.push(index);
			}
		}
		if score > best_score {
			best_score = score;
			best_index = Some(index);
		}
	}
	if let Some(index) = best_index
		&& best_score >= CONTEXT_FUZZY_THRESHOLD
	{
		return ContextResult {
			index: Some(index),
			confidence: best_score,
			match_count: Some(fuzzy_count),
			match_indices: fuzzy_indices,
			strategy: Some(Strategy::Fuzzy),
		};
	}
	ContextResult { confidence: best_score, ..Default::default() }
}

fn find_hierarchical_context(
	lines: &[String],
	context: &str,
	start_from: usize,
	line_hint: Option<usize>,
	allow_fuzzy: bool,
) -> ContextResult {
	if context.contains('\n') {
		let parts: Vec<&str> = context.split('\n').map(str::trim).filter(|part| !part.is_empty()).collect();
		let mut current_start = start_from;
		for (index, part) in parts.iter().enumerate() {
			let is_last = index == parts.len() - 1;
			let result = find_context_line(lines, *part, current_start, allow_fuzzy);
			if result.match_count.is_some_and(|count| count > 1) {
				if is_last
					&& let Some(hint) = line_hint
				{
					let hint_start = hint.saturating_sub(1);
					if hint_start >= current_start {
						let hinted = find_context_line(lines, *part, hint_start, allow_fuzzy);
						if let Some(index) = hinted.index {
							return ContextResult { index: Some(index), confidence: hinted.confidence, match_count: Some(1), match_indices: vec![index], strategy: hinted.strategy };
						}
					}
				}
				return result;
			}
			let Some(index) = result.index else {
				if is_last
					&& let Some(hint) = line_hint
				{
					let hint_start = hint.saturating_sub(1);
					if hint_start >= current_start {
						let hinted = find_context_line(lines, *part, hint_start, allow_fuzzy);
						if let Some(found) = hinted.index {
							return ContextResult { index: Some(found), confidence: hinted.confidence, match_count: Some(1), match_indices: vec![found], strategy: hinted.strategy };
						}
					}
				}
				return ContextResult { confidence: result.confidence, ..Default::default() };
			};
			if is_last {
				return result;
			}
			current_start = index + 1;
		}
		return ContextResult::default();
	}
	let space_parts: Vec<&str> = context.split_whitespace().filter(|part| !part.is_empty()).collect();
	let has_signature_chars = context.contains('(') || context.contains(')') || context.contains('{') || context.contains('[');
	if !has_signature_chars && space_parts.len() > 2 {
		let outer = space_parts[..space_parts.len() - 1].join(" ");
		let inner = space_parts[space_parts.len() - 1];
		let outer_result = find_context_line(lines, &outer, start_from, allow_fuzzy);
		if outer_result.match_count.is_some_and(|count| count > 1) {
			return outer_result;
		}
		if let Some(index) = outer_result.index {
			let inner_result = find_context_line(lines, inner, index + 1, allow_fuzzy);
			if let Some(found) = inner_result.index {
				return ContextResult { index: Some(found), confidence: inner_result.confidence, match_count: Some(1), match_indices: vec![found], strategy: inner_result.strategy };
			}
		}
	}
	let result = find_context_line(lines, context, start_from, allow_fuzzy);
	if (result.index.is_none() || result.match_count.is_some_and(|count| count > 1))
		&& let Some(hint) = line_hint
	{
		let hinted = find_context_line(lines, context, hint.saturating_sub(1), allow_fuzzy);
		if let Some(index) = hinted.index {
			return ContextResult { index: Some(index), confidence: hinted.confidence, match_count: Some(1), match_indices: vec![index], strategy: hinted.strategy };
		}
	}
	if result.index.is_some() && result.match_count.unwrap_or(0) <= 1 {
		return result;
	}
	if result.match_count.is_some_and(|count| count > 1) {
		return result;
	}
	if result.index.is_none() && start_from != 0 {
		let from_start = find_context_line(lines, context, 0, allow_fuzzy);
		if from_start.index.is_some() && from_start.match_count.unwrap_or(0) <= 1 {
			return from_start;
		}
		if from_start.match_count.is_some_and(|count| count > 1) {
			return from_start;
		}
	}
	if !has_signature_chars && space_parts.len() > 1 {
		let outer = space_parts[..space_parts.len() - 1].join(" ");
		let inner = space_parts[space_parts.len() - 1];
		let outer_result = find_context_line(lines, &outer, start_from, allow_fuzzy);
		if outer_result.match_count.is_some_and(|count| count > 1) {
			return outer_result;
		}
		let Some(outer_index) = outer_result.index else {
			return ContextResult { confidence: outer_result.confidence, ..Default::default() };
		};
		let inner_result = find_context_line(lines, inner, outer_index + 1, allow_fuzzy);
		if let Some(found) = inner_result.index {
			return ContextResult { index: Some(found), confidence: inner_result.confidence, match_count: Some(1), match_indices: vec![found], strategy: inner_result.strategy };
		}
	}
	result
}

fn hunk_hint_index(hunk: &DiffHunk, current_index: usize) -> Option<usize> {
	let hint = hunk.old_start_line?.saturating_sub(1);
	(hint >= current_index).then_some(hint)
}

fn find_sequence_with_hint(
	lines: &[String],
	pattern: &[String],
	current_index: usize,
	hint: Option<usize>,
	eof: bool,
	allow_fuzzy: bool,
) -> SequenceResult {
	let primary = seek_sequence(lines, pattern, current_index, eof, allow_fuzzy);
	if primary.match_count.is_some_and(|count| count > 1)
		&& let Some(hint) = hint
		&& hint != current_index
	{
		let hinted = seek_sequence(lines, pattern, hint, eof, allow_fuzzy);
		if hinted.index.is_some() && hinted.match_count.unwrap_or(1) <= 1 {
			return hinted;
		}
		if hinted.match_count.is_some_and(|count| count > 1) {
			return hinted;
		}
	}
	if primary.index.is_some() || primary.match_count.is_some_and(|count| count > 1) {
		return primary;
	}
	if let Some(hint) = hint
		&& hint != current_index
	{
		let hinted = seek_sequence(lines, pattern, hint, eof, allow_fuzzy);
		if hinted.index.is_some() || hinted.match_count.is_some_and(|count| count > 1) {
			return hinted;
		}
	}
	if current_index != 0 {
		let from_start = seek_sequence(lines, pattern, 0, eof, allow_fuzzy);
		if from_start.index.is_some() || from_start.match_count.is_some_and(|count| count > 1) {
			return from_start;
		}
	}
	primary
}

// ── Hunk application ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Replacement {
	start: usize,
	old_len: usize,
	new_lines: Vec<String>,
}

#[derive(Debug, Clone)]
struct HunkVariant {
	old_lines: Vec<String>,
	new_lines: Vec<String>,
	kind: VariantKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VariantKind {
	TrimCommon,
	DedupeShared,
	CollapseRepeated,
	SingleLine,
}

pub fn apply_hunks_to_content(
	original: &str,
	path: &str,
	hunks: &[DiffHunk],
	fuzzy_threshold: f64,
	allow_fuzzy: bool,
) -> Result<(String, Vec<String>), ToolError> {
	let had_final_newline = original.ends_with('\n');
	if hunks.len() == 1 {
		let hunk = &hunks[0];
		if hunk.change_context.is_none()
			&& !hunk.has_context_lines
			&& !hunk.old_lines.is_empty()
			&& hunk.old_start_line.is_none()
			&& !hunk.is_end_of_file
		{
			let (content, warnings) = apply_character_match(original, path, hunk, fuzzy_threshold, allow_fuzzy)?;
			return Ok((apply_trailing_newline_policy(&content, had_final_newline), warnings));
		}
	}
	let mut lines: Vec<String> = original.split('\n').map(str::to_string).collect();
	let mut stripped = false;
	if had_final_newline && lines.last().is_some_and(|line| line.is_empty()) {
		lines.pop();
		stripped = true;
	}
	let (replacements, warnings) = compute_replacements(&lines, path, hunks, allow_fuzzy)?;
	let mut new_lines = lines.clone();
	for replacement in replacements.iter().rev() {
		new_lines.splice(replacement.start..replacement.start + replacement.old_len, replacement.new_lines.iter().cloned());
	}
	if stripped {
		new_lines.push(String::new());
	}
	let content = apply_trailing_newline_policy(&new_lines.join("\n"), had_final_newline);
	Ok((content, warnings))
}

fn apply_trailing_newline_policy(content: &str, had_final_newline: bool) -> String {
	if had_final_newline {
		return if content.ends_with('\n') { content.to_string() } else { format!("{content}\n") };
	}
	content.trim_end_matches('\n').to_string()
}

fn apply_character_match(
	original: &str,
	path: &str,
	hunk: &DiffHunk,
	fuzzy_threshold: f64,
	allow_fuzzy: bool,
) -> Result<(String, Vec<String>), ToolError> {
	let old_text = hunk.old_lines.join("\n");
	let new_text = hunk.new_lines.join("\n");
	let content = text::to_lf(original);
	let old_normalized = text::to_lf(&old_text);
	let mut outcome = find_match(&content, &old_normalized, allow_fuzzy, fuzzy_threshold, &[]);
	if outcome.matched.is_none() && allow_fuzzy {
		let relaxed = fuzzy_threshold.min(0.92);
		if relaxed < fuzzy_threshold {
			let retry = find_match(&content, &old_normalized, true, relaxed, &[]);
			if retry.matched.is_some() {
				outcome = retry;
			}
		}
	}
	if outcome.occurrences > 1 {
		let previews = outcome.occurrence_previews.join("\n\n");
		let more = if outcome.occurrences > 5 {
			format!(" (showing first 5 of {})", outcome.occurrences)
		} else {
			String::new()
		};
		return Err(ToolError::ExecutionFailed(format!(
			"Found {} occurrences in {path}{more}:\n\n{previews}\n\nAdd more context lines to disambiguate.",
			outcome.occurrences
		)));
	}
	if outcome.fuzzy_matches > 1 {
		return Err(ToolError::ExecutionFailed(format!(
			"Found {} high-confidence matches in {path}. The text must be unique. Please provide more context to make it unique.",
			outcome.fuzzy_matches
		)));
	}
	let Some(found) = outcome.matched else {
		if let Some(closest) = &outcome.closest {
			return Err(ToolError::ExecutionFailed(format!(
				"Could not find a close enough match in {path}. Closest match ({}% similar) at line {}.",
				(closest.confidence * 100.0).round() as i64,
				closest.start_line
			)));
		}
		return Err(ToolError::ExecutionFailed(format!("Failed to find expected lines in {path}:\n{old_text}")));
	};
	let adjusted = adjust_indentation(&old_normalized, &found.actual_text, &new_text);
	let mut warnings = Vec::new();
	if outcome.dominant {
		warnings.push(format!(
			"Dominant fuzzy match selected in {path} near line {} ({}% similar).",
			found.start_line,
			(found.confidence * 100.0).round() as i64
		));
	}
	let before = &content[..found.start_index];
	let after = &content[found.start_index + found.actual_text.len()..];
	Ok((format!("{before}{adjusted}{after}"), warnings))
}

fn compute_replacements(
	original_lines: &[String],
	path: &str,
	hunks: &[DiffHunk],
	allow_fuzzy: bool,
) -> Result<(Vec<Replacement>, Vec<String>), ToolError> {
	let mut replacements: Vec<Replacement> = Vec::new();
	let mut warnings: Vec<String> = Vec::new();
	let mut line_index = 0usize;

	for hunk in hunks {
		if hunk.old_start_line.is_some_and(|line| line < 1) {
			return Err(ToolError::ExecutionFailed(format!(
				"Line hint {} is out of range for {path} (line numbers start at 1)",
				hunk.old_start_line.unwrap_or(0)
			)));
		}
		if hunk.new_start_line.is_some_and(|line| line < 1) {
			return Err(ToolError::ExecutionFailed(format!(
				"Line hint {} is out of range for {path} (line numbers start at 1)",
				hunk.new_start_line.unwrap_or(0)
			)));
		}
		let line_hint = hunk.old_start_line;
		let allow_aggressive = hunk.change_context.is_some() || line_hint.is_some() || hunk.is_end_of_file;
		let variants = filter_variants(build_variants(hunk), allow_aggressive);
		if let Some(hint) = line_hint
			&& hunk.change_context.is_none()
			&& !hunk.has_context_lines
		{
			line_index = hint.saturating_sub(1).min(original_lines.len().saturating_sub(1));
		}
		let mut context_index = None;
		if let Some(context) = &hunk.change_context {
			let result = find_hierarchical_context(original_lines, context, line_index, line_hint, allow_fuzzy);
			context_index = result.index;
			if result.index.is_none() || result.match_count.is_some_and(|count| count > 1) {
				let fallback = attempt_sequence_fallback(
					original_lines,
					hunk,
					line_index,
					line_hint,
					allow_fuzzy,
					allow_aggressive,
				);
				if let Some(index) = fallback {
					line_index = index;
				} else if result.match_count.is_some_and(|count| count > 1) {
					let display = context.split('\n').last().unwrap_or(context);
					let previews = format_match_previews(original_lines, &result.match_indices, result.match_count);
					let strategy = result.strategy.map(|strategy| format!(" Matching strategy: {strategy:?}.")).unwrap_or_default();
					let preview_text = previews.map(|value| format!("\n\n{value}")).unwrap_or_default();
					return Err(ToolError::ExecutionFailed(format!(
						"Found {} matches for context '{display}' in {path}.{strategy}{preview_text}\n\nAdd more surrounding context or additional @@ anchors to make it unique.",
						result.match_count.unwrap_or(0)
					)));
				} else {
					let display = context.split('\n').collect::<Vec<_>>().join(" > ");
					return Err(ToolError::ExecutionFailed(format!("Failed to find context '{display}' in {path}")));
				}
			} else if let Some(index) = result.index {
				let first_old = hunk.old_lines.first();
				let final_context = context.split('\n').last().map(str::trim).unwrap_or("");
				let hierarchical = context.contains('\n') || context.split_whitespace().count() > 2;
				line_index = if first_old.is_some_and(|line| line.trim() == final_context) || hierarchical {
					index
				} else {
					index + 1
				};
			}
		}

		if hunk.old_lines.is_empty() {
			let insertion = if hunk.change_context.is_some() {
				line_index
			} else if let Some(hint) = hunk.old_start_line.or(hunk.new_start_line) {
				if hint < 1 {
					return Err(ToolError::ExecutionFailed(format!(
						"Line hint {hint} is out of range for insertion in {path} (line numbers start at 1)"
					)));
				}
				if hint > original_lines.len() + 1 {
					return Err(ToolError::ExecutionFailed(format!(
						"Line hint {hint} is out of range for insertion in {path} (file has {} lines)",
						original_lines.len()
					)));
				}
				hint - 1
			} else if original_lines.last().is_some_and(|line| line.is_empty()) {
				original_lines.len() - 1
			} else {
				original_lines.len()
			};
			replacements.push(Replacement {
				start: insertion,
				old_len: 0,
				new_lines: hunk.new_lines.clone(),
			});
			continue;
		}

		let mut pattern = hunk.old_lines.clone();
		let match_hint = hunk_hint_index(hunk, line_index);
		let mut search = find_sequence_with_hint(original_lines, &pattern, line_index, match_hint, hunk.is_end_of_file, allow_fuzzy);
		let mut new_slice = hunk.new_lines.clone();
		if search.index.is_none() && pattern.last().is_some_and(|line| line.is_empty()) {
			pattern.pop();
			if new_slice.last().is_some_and(|line| line.is_empty()) {
				new_slice.pop();
			}
			search = find_sequence_with_hint(original_lines, &pattern, line_index, match_hint, hunk.is_end_of_file, allow_fuzzy);
		}
		if search.index.is_none() || search.match_count.unwrap_or(0) > 1 {
			for variant in &variants {
				if variant.old_lines.is_empty() {
					continue;
				}
				let result = find_sequence_with_hint(
					original_lines,
					&variant.old_lines,
					line_index,
					match_hint,
					hunk.is_end_of_file,
					allow_fuzzy,
				);
				if result.index.is_some() && result.match_count.unwrap_or(1) <= 1 {
					pattern = variant.old_lines.clone();
					new_slice = variant.new_lines.clone();
					search = result;
					break;
				}
			}
		}
		if search.index.is_none()
			&& let Some(context_index) = context_index
		{
			for variant in &variants {
				if variant.old_lines.len() != 1 || variant.new_lines.len() != 1 {
					continue;
				}
				let removed = &variant.old_lines[0];
				let shared_duplicate = hunk.new_lines.iter().any(|line| line.trim() == removed.trim());
				if let Some(index) = find_context_relative_match(original_lines, removed, context_index, shared_duplicate) {
					pattern = variant.old_lines.clone();
					new_slice = variant.new_lines.clone();
					search = SequenceResult {
						index: Some(index),
						confidence: 0.95,
						match_count: Some(1),
						..Default::default()
					};
					break;
				}
			}
		}
		if search.index.is_some()
			&& let Some(context_index) = context_index
			&& pattern.len() == 1
		{
			let trimmed = pattern[0].trim();
			let count = original_lines.iter().filter(|line| line.trim() == trimmed).count();
			if count > 1 {
				let shared = hunk.new_lines.iter().any(|line| line.trim() == trimmed);
				if let Some(index) = find_context_relative_match(original_lines, &pattern[0], context_index, shared) {
					search = SequenceResult { index: Some(index), confidence: search.confidence.max(0.95), match_count: Some(1), ..Default::default() };
				}
			}
		}
		if search.match_count.unwrap_or(0) > 1 {
			let hint_index = match_hint.or_else(|| line_hint.map(|hint| hint.saturating_sub(1)));
			if let Some(index) = choose_hinted_match(&search.match_indices, hint_index, AMBIGUITY_HINT_WINDOW) {
				search.index = Some(index);
				search.match_count = Some(1);
			}
		}
		let Some(found) = search.index else {
			if search.match_count.is_some_and(|count| count > 1) {
				let previews = format_match_previews(original_lines, &search.match_indices, search.match_count);
				let strategy = search.strategy.map(|strategy| format!(" Matching strategy: {strategy:?}.")).unwrap_or_default();
				let preview_text = previews.map(|value| format!("\n\n{value}")).unwrap_or_default();
				return Err(ToolError::ExecutionFailed(format!(
					"Found {} matches for the text in {path}.{strategy}{preview_text}\n\nAdd more surrounding context or additional @@ anchors to make it unique.",
					search.match_count.unwrap_or(0)
				)));
			}
			let closest = find_closest_sequence_match(original_lines, &pattern, line_index, hunk.is_end_of_file);
			if let (Some(index), confidence) = (closest.0, closest.1)
				&& confidence > 0.0
			{
				let similarity = (confidence * 100.0).round() as i64;
				let preview = format_match_preview(original_lines, index);
				return Err(ToolError::ExecutionFailed(format!(
					"Failed to find expected lines in {path}:\n{}\n\nClosest match ({similarity}% similar) near line {}:\n{preview}",
					hunk.old_lines.join("\n"),
					index + 1
				)));
			}
			return Err(ToolError::ExecutionFailed(format!(
				"Failed to find expected lines in {path}:\n{}",
				hunk.old_lines.join("\n")
			)));
		};
		if search.strategy == Some(Strategy::FuzzyDominant) {
			warnings.push(format!(
				"Dominant fuzzy match selected in {path} near line {} ({}% similar).",
				found + 1,
				(search.confidence * 100.0).round() as i64
			));
		} else if matches!(
			search.strategy,
			Some(Strategy::CommentPrefix | Strategy::Prefix | Strategy::Substring | Strategy::Fuzzy | Strategy::Character)
		) {
			warnings.push(format!(
				"Inexact match in {path} near line {}: matched via {:?} strategy ({}% similar). Re-read the file if the result is not what you intended.",
				found + 1,
				search.strategy.unwrap_or(Strategy::Fuzzy),
				(search.confidence * 100.0).round() as i64
			));
		}
		if search.match_count.is_some_and(|count| count > 1) {
			let previews = format_match_previews(original_lines, &search.match_indices, search.match_count);
			let strategy = search.strategy.map(|strategy| format!(" Matching strategy: {strategy:?}.")).unwrap_or_default();
			let preview_text = previews.map(|value| format!("\n\n{value}")).unwrap_or_default();
			return Err(ToolError::ExecutionFailed(format!(
				"Found {} matches for the text in {path}.{strategy}{preview_text}\n\nAdd more surrounding context or additional @@ anchors to make it unique.",
				search.match_count.unwrap_or(0)
			)));
		}
		if hunk.change_context.is_none() && !hunk.has_context_lines && !hunk.is_end_of_file && line_hint.is_none() {
			let second = seek_sequence(original_lines, &pattern, found + 1, false, allow_fuzzy);
			if let Some(second_index) = second.index {
				let preview1 = format_match_preview(original_lines, found);
				let preview2 = format_match_preview(original_lines, second_index);
				return Err(ToolError::ExecutionFailed(format!(
					"Found 2 occurrences in {path}:\n\n{preview1}\n\n{preview2}\n\nAdd more context lines to disambiguate."
				)));
			}
		}
		let actual: Vec<String> = original_lines[found..found + pattern.len()].to_vec();
		let is_noop = pattern.len() == new_slice.len() && pattern.iter().zip(new_slice.iter()).all(|(a, b)| a == b);
		if is_noop {
			line_index = found + pattern.len();
			continue;
		}
		if matches!(search.strategy, Some(Strategy::Prefix | Strategy::Substring)) {
			assert_partial_preserves_discarded(path, &pattern, &actual, &new_slice, found)?;
		}
		let adjusted = adjust_lines_indentation(&pattern, &actual, &new_slice);
		replacements.push(Replacement {
			start: found,
			old_len: pattern.len(),
			new_lines: adjusted,
		});
		line_index = found + pattern.len();
	}

	replacements.sort_by_key(|replacement| replacement.start);
	for pair in replacements.windows(2) {
		let previous = &pair[0];
		let next = &pair[1];
		let previous_end = previous.start + previous.old_len;
		if next.start < previous_end {
			let format_range = |replacement: &Replacement| {
				if replacement.old_len == 0 {
					format!("{} (insertion)", replacement.start + 1)
				} else {
					format!("{}-{}", replacement.start + 1, replacement.start + replacement.old_len)
				}
			};
			return Err(ToolError::ExecutionFailed(format!(
				"Overlapping hunks detected in {path} at lines {} and {}. Split hunks or add more context to avoid overlap.",
				format_range(previous),
				format_range(next)
			)));
		}
	}
	Ok((replacements, warnings))
}

fn find_context_relative_match(
	lines: &[String],
	pattern_line: &str,
	context_index: usize,
	prefer_second_forward: bool,
) -> Option<usize> {
	let trimmed = pattern_line.trim();
	let forward: Vec<usize> = (context_index + 1..lines.len()).filter(|index| lines[*index].trim() == trimmed).collect();
	if !forward.is_empty() {
		if prefer_second_forward && forward.len() > 1 {
			return Some(forward[1]);
		}
		return Some(forward[0]);
	}
	(0..context_index).rev().find(|index| lines[*index].trim() == trimmed)
}

fn format_match_preview(lines: &[String], start_index: usize) -> String {
	let start = start_index.saturating_sub(MATCH_PREVIEW_CONTEXT);
	let end = (start_index + MATCH_PREVIEW_CONTEXT + 1).min(lines.len());
	lines[start..end]
		.iter()
		.enumerate()
		.map(|(index, line)| {
			let number = start + index + 1;
			let truncated: String = if line.chars().count() > MATCH_PREVIEW_MAX_LEN {
				let mut text: String = line.chars().take(MATCH_PREVIEW_MAX_LEN - 1).collect();
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

fn format_match_previews(lines: &[String], indices: &[usize], count: Option<usize>) -> Option<String> {
	if indices.is_empty() {
		return None;
	}
	let previews: Vec<String> = indices.iter().map(|index| format_match_preview(lines, *index)).collect();
	let more = match count {
		Some(count) if count > indices.len() => format!(" (showing first {} of {count})", indices.len()),
		_ => String::new(),
	};
	Some(format!("{}{more}", previews.join("\n\n")))
}

fn choose_hinted_match(indices: &[usize], hint: Option<usize>, window: usize) -> Option<usize> {
	let hint = hint?;
	let candidates: Vec<usize> = indices.iter().copied().filter(|index| index.abs_diff(hint) <= window).collect();
	(candidates.len() == 1).then_some(candidates[0])
}

fn find_closest_sequence_match(lines: &[String], pattern: &[String], start: usize, eof: bool) -> (Option<usize>, f64) {
	if pattern.is_empty() {
		return (Some(start), 1.0);
	}
	if pattern.len() > lines.len() {
		return (None, 0.0);
	}
	let max_start = lines.len() - pattern.len();
	let search_start = if eof { max_start } else { start.min(max_start) };
	let lines_norm: Vec<String> = lines.iter().map(|line| text::normalize_for_fuzzy(line)).collect();
	let pattern_norm: Vec<String> = pattern.iter().map(|line| text::normalize_for_fuzzy(line)).collect();
	let mut best_index = None;
	let mut best_score = 0.0f64;
	for index in search_start..=max_start {
		let score = fuzzy_score_at(&lines_norm, &pattern_norm, index, best_score);
		if score > best_score {
			best_score = score;
			best_index = Some(index);
		}
	}
	if eof && search_start > start {
		for index in start..search_start {
			let score = fuzzy_score_at(&lines_norm, &pattern_norm, index, best_score);
			if score > best_score {
				best_score = score;
				best_index = Some(index);
			}
		}
	}
	(best_index, best_score)
}

fn attempt_sequence_fallback(
	lines: &[String],
	hunk: &DiffHunk,
	current_index: usize,
	line_hint: Option<usize>,
	allow_fuzzy: bool,
	allow_aggressive: bool,
) -> Option<usize> {
	if hunk.old_lines.is_empty() {
		return None;
	}
	let hint = hunk_hint_index(hunk, current_index).or(line_hint);
	let result = find_sequence_with_hint(lines, &hunk.old_lines, current_index, hint, false, allow_fuzzy);
	if result.index.is_some() && result.match_count.unwrap_or(1) <= 1 {
		let index = result.index?;
		let next = index + 1;
		if next <= lines.len().saturating_sub(hunk.old_lines.len()) {
			let second = seek_sequence(lines, &hunk.old_lines, next, false, allow_fuzzy);
			if second.index.is_some() {
				return None;
			}
		}
		return Some(index);
	}
	for variant in filter_variants(build_variants(hunk), allow_aggressive) {
		if variant.old_lines.is_empty() {
			continue;
		}
		let result = find_sequence_with_hint(lines, &variant.old_lines, current_index, hint, false, allow_fuzzy);
		if result.index.is_some() && result.match_count.unwrap_or(1) <= 1 {
			return result.index;
		}
	}
	None
}

fn assert_partial_preserves_discarded(
	path: &str,
	pattern: &[String],
	matched: &[String],
	new_lines: &[String],
	match_start: usize,
) -> Result<(), ToolError> {
	let mut new_lines_norm: Option<String> = None;
	for (index, (pattern_line, matched_line)) in pattern.iter().zip(matched.iter()).enumerate() {
		let line_norm = text::normalize_for_fuzzy(matched_line);
		let pattern_norm = text::normalize_for_fuzzy(pattern_line);
		if line_norm == pattern_norm {
			continue;
		}
		let Some(at) = line_norm.find(&pattern_norm) else { continue };
		let discarded = [line_norm[..at].trim(), line_norm[at + pattern_norm.len()..].trim()];
		for part in discarded {
			if part.is_empty() {
				continue;
			}
			let normalized = new_lines_norm.get_or_insert_with(|| {
				new_lines.iter().map(|line| text::normalize_for_fuzzy(line)).collect::<Vec<_>>().join("\n")
			});
			if !normalized.contains(part) {
				return Err(ToolError::ExecutionFailed(format!(
					"Refusing partial-line match in {path} at line {}: the file line also contains {part:?}, which the replacement would silently drop. Provide the complete line in the hunk.",
					match_start + index + 1
				)));
			}
		}
	}
	Ok(())
}

// ── Fallback variants ─────────────────────────────────────────────────────

fn trim_common_context(old_lines: &[String], new_lines: &[String]) -> Option<HunkVariant> {
	let mut start = 0;
	let mut end_old = old_lines.len();
	let mut end_new = new_lines.len();
	while start < end_old && start < end_new && old_lines[start] == new_lines[start] {
		start += 1;
	}
	while end_old > start && end_new > start && old_lines[end_old - 1] == new_lines[end_new - 1] {
		end_old -= 1;
		end_new -= 1;
	}
	if start == 0 && end_old == old_lines.len() && end_new == new_lines.len() {
		return None;
	}
	let trimmed_old = old_lines[start..end_old].to_vec();
	let trimmed_new = new_lines[start..end_new].to_vec();
	if trimmed_old.is_empty() && trimmed_new.is_empty() {
		return None;
	}
	Some(HunkVariant { old_lines: trimmed_old, new_lines: trimmed_new, kind: VariantKind::TrimCommon })
}

fn collapse_consecutive_shared(old_lines: &[String], new_lines: &[String]) -> Option<HunkVariant> {
	let new_set: std::collections::HashSet<&String> = new_lines.iter().collect();
	let shared: std::collections::HashSet<&String> = old_lines.iter().filter(|line| new_set.contains(line)).collect();
	let collapse = |lines: &[String]| -> Vec<String> {
		let mut out = Vec::new();
		let mut index = 0;
		while index < lines.len() {
			let line = &lines[index];
			out.push(line.clone());
			let mut next = index + 1;
			while next < lines.len() && lines[next] == *line && shared.contains(line) {
				next += 1;
			}
			index = next;
		}
		out
	};
	let collapsed_old = collapse(old_lines);
	let collapsed_new = collapse(new_lines);
	if collapsed_old.len() == old_lines.len() && collapsed_new.len() == new_lines.len() {
		return None;
	}
	Some(HunkVariant { old_lines: collapsed_old, new_lines: collapsed_new, kind: VariantKind::DedupeShared })
}

fn reduce_to_single_line_change(old_lines: &[String], new_lines: &[String]) -> Option<HunkVariant> {
	if old_lines.len() != new_lines.len() || old_lines.is_empty() {
		return None;
	}
	let mut changed = None;
	for index in 0..old_lines.len() {
		if old_lines[index] != new_lines[index] {
			if changed.is_some() {
				return None;
			}
			changed = Some(index);
		}
	}
	let index = changed?;
	Some(HunkVariant {
		old_lines: vec![old_lines[index].clone()],
		new_lines: vec![new_lines[index].clone()],
		kind: VariantKind::SingleLine,
	})
}

fn build_variants(hunk: &DiffHunk) -> Vec<HunkVariant> {
	let mut variants = Vec::new();
	if let Some(trimmed) = trim_common_context(&hunk.old_lines, &hunk.new_lines) {
		variants.push(trimmed);
	}
	let base_old = variants.last().map(|variant| &variant.old_lines).unwrap_or(&hunk.old_lines);
	let base_new = variants.last().map(|variant| &variant.new_lines).unwrap_or(&hunk.new_lines);
	if let Some(deduped) = collapse_consecutive_shared(base_old, base_new) {
		variants.push(deduped);
	}
	let deduped_old = variants.last().map(|variant| &variant.old_lines).unwrap_or(&hunk.old_lines);
	let deduped_new = variants.last().map(|variant| &variant.new_lines).unwrap_or(&hunk.new_lines);
	if let Some((collapsed_old, collapsed_new)) = text::collapse_repeated_blocks(deduped_old, deduped_new) {
		variants.push(HunkVariant { old_lines: collapsed_old, new_lines: collapsed_new, kind: VariantKind::CollapseRepeated });
	}
	if let Some(single) = reduce_to_single_line_change(
		variants.first().map(|variant| &variant.old_lines).unwrap_or(&hunk.old_lines),
		variants.first().map(|variant| &variant.new_lines).unwrap_or(&hunk.new_lines),
	) {
		variants.push(single);
	}
	let mut seen = std::collections::HashSet::new();
	variants
		.into_iter()
		.filter(|variant| {
			if variant.old_lines.is_empty() && variant.new_lines.is_empty() {
				return false;
			}
			let key = format!("{}||{}", variant.old_lines.join("\n"), variant.new_lines.join("\n"));
			seen.insert(key)
		})
		.collect()
}

fn filter_variants(variants: Vec<HunkVariant>, allow_aggressive: bool) -> Vec<HunkVariant> {
	if allow_aggressive {
		return variants;
	}
	variants
		.into_iter()
		.filter(|variant| !matches!(variant.kind, VariantKind::CollapseRepeated | VariantKind::SingleLine))
		.collect()
}

fn adjust_lines_indentation(pattern: &[String], actual: &[String], new_lines: &[String]) -> Vec<String> {
	if pattern.is_empty() || actual.is_empty() || new_lines.is_empty() {
		return new_lines.to_vec();
	}
	if pattern == actual {
		return new_lines.to_vec();
	}
	let trimmed_equal = pattern.len() == new_lines.len()
		&& pattern.iter().zip(new_lines.iter()).all(|(a, b)| a.trim() == b.trim());
	if trimmed_equal {
		return new_lines.to_vec();
	}
	let indent_char = actual
		.iter()
		.find_map(|line| text::leading_whitespace(line).chars().next())
		.unwrap_or(' ');
	let pattern_tab_only = pattern.iter().all(|line| line.trim().is_empty() || !text::leading_whitespace(line).contains(' '));
	let actual_space_only = actual.iter().all(|line| line.trim().is_empty() || !text::leading_whitespace(line).contains('\t'));
	let pattern_space_only = pattern.iter().all(|line| line.trim().is_empty() || !text::leading_whitespace(line).contains('\t'));
	let actual_tab_only = actual.iter().all(|line| line.trim().is_empty() || !text::leading_whitespace(line).contains(' '));
	if pattern_tab_only && actual_space_only {
		let mut ratio = None;
		let mut consistent = true;
		for (pattern_line, actual_line) in pattern.iter().zip(actual.iter()) {
			if pattern_line.trim().is_empty() || actual_line.trim().is_empty() {
				continue;
			}
			let pattern_indent = text::leading_whitespace_len(pattern_line);
			let actual_indent = text::leading_whitespace_len(actual_line);
			if pattern_indent == 0 {
				continue;
			}
			if actual_indent % pattern_indent != 0 {
				consistent = false;
				break;
			}
			let next_ratio = actual_indent / pattern_indent;
			if let Some(existing) = ratio {
				if existing != next_ratio {
					consistent = false;
					break;
				}
			} else {
				ratio = Some(next_ratio);
			}
		}
		if consistent
			&& let Some(ratio) = ratio
		{
			let joined = new_lines.join("\n");
			let converted = joined
				.split('\n')
				.map(|line| {
					let trimmed = line.trim_start();
					if trimmed.is_empty() {
						return line.to_string();
					}
					let indent = text::leading_whitespace(line);
					if !indent.contains('\t') || indent.contains(' ') {
						return line.to_string();
					}
					format!("{}{trimmed}", " ".repeat(indent.len() * ratio))
				})
				.collect::<Vec<_>>()
				.join("\n");
			return converted.split('\n').map(str::to_string).collect();
		}
	}
	// Tabs in the file where the patch used spaces: invert using the first
	// whole-tab sample. Multi-level offset solving is out of scope.
	if pattern_space_only && actual_tab_only {
		let sample = pattern
			.iter()
			.zip(actual.iter())
			.find_map(|(pattern_line, actual_line)| {
				let spaces = text::leading_whitespace_len(pattern_line);
				let tabs = text::leading_whitespace_len(actual_line);
				(tabs > 0 && spaces % tabs == 0 && spaces > 0).then_some(spaces / tabs)
			});
		if let Some(width) = sample {
			return new_lines
				.iter()
				.map(|line| {
					if line.trim().is_empty() {
						return line.clone();
					}
					let spaces = text::leading_whitespace_len(line);
					if spaces == 0 || spaces % width != 0 {
						return line.clone();
					}
					format!("{}{}", "\t".repeat(spaces / width), line[spaces..].to_string())
				})
				.collect();
		}
	}
	let pattern_min = pattern
		.iter()
		.filter(|line| !line.trim().is_empty())
		.map(|line| text::leading_whitespace_len(line))
		.min()
		.unwrap_or(0);
	let mut deltas = Vec::new();
	for (pattern_line, actual_line) in pattern.iter().zip(actual.iter()) {
		if pattern_line.trim().is_empty() || actual_line.trim().is_empty() {
			continue;
		}
		deltas.push(text::leading_whitespace_len(actual_line) as i32 - text::leading_whitespace_len(pattern_line) as i32);
	}
	let delta = deltas.first().copied().filter(|first| deltas.iter().all(|value| value == first));
	text::realign_context_lines(pattern, actual, new_lines, pattern_min, delta, indent_char)
}


