//! Codex `*** Begin Patch` envelope parser.
//!
//! Ports `packages/coding-agent/src/edit/apply-patch/parser.ts`. Each hunk is
//! lowered to a [`PatchInput`] so `apply_patch` shares the `patch` engine's
//! matching, validation, and atomic per-file commits. Add is strictly
//! non-overwriting, Move refuses an existing destination, and hunks apply in
//! order (not atomically across files), matching the Codex contract.

use serde_json::Value;

use super::{
	patch::{Operation, PatchInput},
	paths,
};
use crate::{
	state::{AgentContext, ToolResult},
	tools::ToolError,
};

const BEGIN_PATCH_MARKER: &str = "*** Begin Patch";
const END_PATCH_MARKER: &str = "*** End Patch";
const ADD_FILE_MARKER: &str = "*** Add File: ";
const DELETE_FILE_MARKER: &str = "*** Delete File: ";
const UPDATE_FILE_MARKER: &str = "*** Update File: ";
const MOVE_TO_MARKER: &str = "*** Move to: ";

pub fn schema() -> Value {
	serde_json::json!({
		"type": "object",
		"properties": {
			"input": {
				"type": "string",
				"description": "Full `*** Begin Patch` … `*** End Patch` envelope. File sections use `*** Add File:`, `*** Delete File:`, or `*** Update File:` (optionally followed by `*** Move to:` and `@@` hunks)."
			}
		},
		"required": ["input"]
	})
}

pub async fn execute(args: &Value, ctx: &AgentContext) -> Result<ToolResult, ToolError> {
	let input = super::require_str(args, "input")?;
	let hunks = parse_apply_patch(&input)?;
	if hunks.is_empty() {
		return Err(ToolError::ExecutionFailed("No files were modified.".to_string()));
	}
	for hunk in &hunks {
		// Reject internal URLs before any write, per authored path.
		paths::ensure_no_internal_scheme(&hunk.path)?;
		if let Some(rename) = &hunk.rename {
			paths::ensure_no_internal_scheme(rename)?;
		}
	}
	let changes = super::patch::apply_inputs(&hunks, &ctx.cwd, &ctx.cancellation_token, false).await?;
	Ok(super::patch::render_changes(changes, "Success. Updated the following files:"))
}

/// Parse a complete envelope; `*** Begin/End Patch` are required.
pub fn parse_apply_patch(input: &str) -> Result<Vec<PatchInput>, ToolError> {
	parse_with_options(input, false)
}

/// Best-effort parse for in-progress previews; never used to apply edits.
pub fn parse_apply_patch_streaming(input: &str) -> Vec<PatchInput> {
	parse_with_options(input, true).unwrap_or_default()
}

fn parse_with_options(input: &str, streaming: bool) -> Result<Vec<PatchInput>, ToolError> {
	let mut lines: Vec<String> = input.trim().split('\n').map(str::to_string).collect();
	// Lenient heredoc strip: <<EOF / <<'EOF' / <<"EOF" … EOF
	if lines.len() >= 2 {
		let first = lines[0].as_str();
		let last = lines[lines.len() - 1].trim();
		let valid_openers = ["<<EOF", "<<'EOF'", "<<\"EOF\""];
		if valid_openers.contains(&first) && last == "EOF" {
			lines = lines[1..lines.len() - 1].to_vec();
		}
	}
	if lines.is_empty() || lines[0].trim() != BEGIN_PATCH_MARKER {
		if streaming {
			return Ok(Vec::new());
		}
		return Err(ToolError::InvalidArguments(
			"The first line of the patch must be '*** Begin Patch'".to_string(),
		));
	}
	let has_end = lines.last().is_some_and(|line| line.trim() == END_PATCH_MARKER);
	if !has_end && !streaming {
		return Err(ToolError::InvalidArguments(
			"The last line of the patch must be '*** End Patch'".to_string(),
		));
	}
	let mut remaining: Vec<String> = if has_end {
		lines[1..lines.len() - 1].to_vec()
	} else {
		lines[1..].to_vec()
	};
	// 1-based, counting the `*** Begin Patch` line as 1.
	let mut line_number = 2usize;
	let mut hunks = Vec::new();

	while !remaining.is_empty() {
		if remaining[0].trim().is_empty() {
			remaining.remove(0);
			line_number += 1;
			continue;
		}
		let first = remaining[0].trim().to_string();
		if let Some(path) = first.strip_prefix(ADD_FILE_MARKER) {
			let path = path.to_string();
			let mut contents = String::new();
			let mut consumed = 1usize;
			for line in remaining.iter().skip(1) {
				if let Some(rest) = line.strip_prefix('+') {
					contents.push_str(rest);
					contents.push('\n');
					consumed += 1;
				} else {
					break;
				}
			}
			hunks.push(PatchInput { path, op: Operation::Create, rename: None, diff: contents });
			remaining.drain(0..consumed);
			line_number += consumed;
			continue;
		}
		if let Some(path) = first.strip_prefix(DELETE_FILE_MARKER) {
			hunks.push(PatchInput {
				path: path.to_string(),
				op: Operation::Delete,
				rename: None,
				diff: String::new(),
			});
			remaining.remove(0);
			line_number += 1;
			continue;
		}
		if let Some(path) = first.strip_prefix(UPDATE_FILE_MARKER) {
			let path = path.to_string();
			remaining.remove(0);
			line_number += 1;
			let mut move_path = None;
			if remaining.first().is_some_and(|line| line.starts_with(MOVE_TO_MARKER)) {
				move_path = Some(remaining[0][MOVE_TO_MARKER.len()..].to_string());
				remaining.remove(0);
				line_number += 1;
			}
			let mut diff_lines = Vec::new();
			loop {
				let Some(line) = remaining.first().cloned() else {
					break;
				};
				if line.starts_with("*** Add File:")
					|| line.starts_with("*** Delete File:")
					|| line.starts_with("*** Update File:")
				{
					break;
				}
				diff_lines.push(line);
				remaining.remove(0);
				line_number += 1;
			}
			if diff_lines.is_empty() {
				if streaming {
					hunks.push(PatchInput { path, op: Operation::Update, rename: move_path, diff: String::new() });
					continue;
				}
				return Err(ToolError::InvalidArguments(format!(
					"Line {line_number}: Update file hunk for path '{path}' is empty"
				)));
			}
			hunks.push(PatchInput {
				path,
				op: Operation::Update,
				rename: move_path,
				diff: diff_lines.join("\n"),
			});
			continue;
		}
		if streaming {
			break;
		}
		return Err(ToolError::InvalidArguments(format!(
			"Line {line_number}: '{first}' is not a valid hunk header. Valid hunk headers: '*** Add File: {{path}}', '*** Delete File: {{path}}', '*** Update File: {{path}}'"
		)));
	}
	Ok(hunks)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_add_update_move_delete() {
		let patch = "*** Begin Patch\n*** Add File: hello.txt\n+Hello world\n*** Update File: src/app.py\n*** Move to: src/main.py\n@@ def greet():\n-print(\"Hi\")\n+print(\"Hello, world!\")\n*** Delete File: obsolete.txt\n*** End Patch";
		let hunks = parse_apply_patch(patch).unwrap();
		assert_eq!(hunks.len(), 3);
		assert_eq!(hunks[0].op, Operation::Create);
		assert_eq!(hunks[0].diff, "Hello world\n");
		assert_eq!(hunks[1].op, Operation::Update);
		assert_eq!(hunks[1].rename.as_deref(), Some("src/main.py"));
		assert!(hunks[1].diff.contains("-print(\"Hi\")"));
		assert_eq!(hunks[2].op, Operation::Delete);
	}

	#[test]
	fn rejects_missing_envelope() {
		assert!(parse_apply_patch("*** Add File: x\n+y").is_err());
		assert!(parse_apply_patch("*** Begin Patch\n*** Add File: x\n+y").is_err());
		assert!(parse_apply_patch("*** Begin Patch\n*** Update File: x\n*** End Patch").is_err());
	}

	#[test]
	fn strips_heredoc_wrapper() {
		let patch = "<<'EOF'\n*** Begin Patch\n*** Add File: x\n+y\n*** End Patch\nEOF";
		let hunks = parse_apply_patch(patch).unwrap();
		assert_eq!(hunks.len(), 1);
		assert_eq!(hunks[0].path, "x");
	}
}
