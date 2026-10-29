//! Text-edit and `WorkspaceEdit` application.
//!
//! Port of `packages/coding-agent/src/lsp/edits.ts` with two hardening changes:
//! every mutation must target a local `file://` URI, and each file write is
//! atomic (temp file in the same directory + rename). Batches are fully
//! validated before the first byte is written, so an overlapping edit in a
//! later URI never leaves earlier files half-applied.

use std::{
	collections::HashMap,
	path::{Path, PathBuf},
	sync::atomic::{AtomicU64, Ordering},
};

use tokio_util::sync::CancellationToken;

use super::types::{
	CreateFileOptions, DeleteFileOptions, Position, Range, RenameFileOptions, ResourceOperation,
	TextEdit, WorkspaceDocumentChange, WorkspaceEdit,
};
use super::utils::{file_uri_path, format_path_relative_to_cwd};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

// =============================================================================
// Text edit application
// =============================================================================

/// Apply edits to an in-memory string. Edits are sorted bottom-to-top and
/// overlap-validated first, preserving all original line/character indices.
pub fn apply_text_edits_to_string(content: &str, edits: &[TextEdit]) -> Result<String, String> {
	let mut lines: Vec<String> = content.split('\n').map(str::to_string).collect();
	let sorted = sort_and_validate_text_edits(edits)?;
	for edit in sorted {
		let start = edit.range.start;
		let end = edit.range.end;
		if start.line as usize >= lines.len() || end.line as usize >= lines.len() {
			return Err(format!(
				"LSP edit range {} is outside the document ({} lines)",
				format_range(edit.range),
				lines.len()
			));
		}
		if start.line == end.line {
			let line = lines[start.line as usize].clone();
			let start_byte = char_to_byte_index(&line, start.character as usize);
			let end_byte = char_to_byte_index(&line, end.character as usize);
			lines[start.line as usize] = format!(
				"{}{}{}",
				&line[..start_byte],
				edit.new_text,
				&line[end_byte..]
			);
		} else {
			let start_line = lines[start.line as usize].clone();
			let end_line = lines[end.line as usize].clone();
			let start_byte = char_to_byte_index(&start_line, start.character as usize);
			let end_byte = char_to_byte_index(&end_line, end.character as usize);
			let new_content = format!(
				"{}{}{}",
				&start_line[..start_byte],
				edit.new_text,
				&end_line[end_byte..]
			);
			let replacement: Vec<String> = new_content.split('\n').map(str::to_string).collect();
			let from = start.line as usize;
			let to = end.line as usize + 1;
			lines.splice(from..to, replacement);
		}
	}
	Ok(lines.join("\n"))
}

fn char_to_byte_index(text: &str, utf16_units: usize) -> usize {
	if utf16_units == 0 {
		return 0;
	}
	let mut units = 0;
	for (index, ch) in text.char_indices() {
		if units >= utf16_units {
			return index;
		}
		units += ch.len_utf16();
	}
	text.len()
}

pub fn compare_position(a: Position, b: Position) -> std::cmp::Ordering {
	a.line.cmp(&b.line).then(a.character.cmp(&b.character))
}

fn positions_equal(a: Position, b: Position) -> bool {
	a.line == b.line && a.character == b.character
}

fn ranges_equal(a: Range, b: Range) -> bool {
	positions_equal(a.start, b.start) && positions_equal(a.end, b.end)
}

fn is_empty_range(range: Range) -> bool {
	positions_equal(range.start, range.end)
}

fn format_range(range: Range) -> String {
	format!(
		"{}:{}-{}:{}",
		range.start.line + 1,
		range.start.character + 1,
		range.end.line + 1,
		range.end.character + 1
	)
}

/// True when two ranges share any position other than a touching boundary.
#[must_use]
pub fn ranges_overlap(a: Range, b: Range) -> bool {
	compare_position(a.start, b.end).is_lt() && compare_position(b.start, a.end).is_lt()
}

fn reject_snippet_text_edits(edits: &[TextEdit]) -> Result<(), String> {
	if edits.iter().any(|edit| edit.insert_text_format == Some(2)) {
		return Err("snippet-formatted LSP edits are unsupported".to_string());
	}
	Ok(())
}

/// Sort edits bottom-to-top, reject snippet edits and overlaps, and collapse
/// byte-identical duplicate ranges.
pub fn sort_and_validate_text_edits(edits: &[TextEdit]) -> Result<Vec<TextEdit>, String> {
	reject_snippet_text_edits(edits)?;
	let mut indexed: Vec<(usize, &TextEdit)> = edits.iter().enumerate().collect();
	indexed.sort_by(|(left_index, left), (right_index, right)| {
		right
			.range
			.start
			.line
			.cmp(&left.range.start.line)
			.then(right.range.start.character.cmp(&left.range.start.character))
			.then(right_index.cmp(left_index))
	});
	let mut unique: Vec<TextEdit> = Vec::new();
	for (_, edit) in indexed {
		if let Some(previous) = unique.last() {
			if !is_empty_range(edit.range)
				&& ranges_equal(previous.range, edit.range)
				&& previous.new_text == edit.new_text
			{
				continue;
			}
		}
		unique.push(edit.clone());
	}
	for window in unique.windows(2) {
		let later = window[0].range;
		let earlier = window[1].range;
		if compare_position(earlier.end, later.start).is_gt() {
			return Err(format!(
				"overlapping LSP edits: {} conflicts with {}; LSP produced inconsistent edits",
				format_range(earlier),
				format_range(later)
			));
		}
	}
	Ok(unique)
}

/// Flatten `WorkspaceEdit` text edits to `uri -> edits`. Resource operations are
/// ignored; callers handle them through ordered document changes.
#[must_use]
pub fn flatten_workspace_text_edits(edit: &WorkspaceEdit) -> HashMap<String, Vec<TextEdit>> {
	let mut out: HashMap<String, Vec<TextEdit>> = HashMap::new();
	if let Some(changes) = &edit.changes {
		for (uri, edits) in changes {
			if edits.is_empty() {
				continue;
			}
			out.entry(uri.clone()).or_default().extend(edits.clone());
		}
	}
	if let Some(changes) = &edit.document_changes {
		for change in changes {
			if let WorkspaceDocumentChange::Text(text) = change {
				if text.edits.is_empty() {
					continue;
				}
				out.entry(text.text_document.uri.clone()).or_default().extend(text.edits.clone());
			}
		}
	}
	out
}

// =============================================================================
// Workspace edits
// =============================================================================

#[derive(Debug, Clone)]
pub enum ExecutedWorkspaceChange {
	Edit { uri: String },
	Create { uri: String },
	Rename { old_uri: String, new_uri: String },
	Delete { uri: String },
}

#[derive(Debug, Default)]
pub struct WorkspaceEditResult {
	pub applied: Vec<String>,
	pub executed: Vec<ExecutedWorkspaceChange>,
}

#[derive(Debug)]
enum WorkspaceEditOp {
	Text { uri: String, edits: Vec<TextEdit> },
	Create { uri: String, options: CreateFileOptions },
	Rename { old_uri: String, new_uri: String, options: RenameFileOptions },
	Delete { uri: String, options: DeleteFileOptions },
}

/// Flatten `documentChanges` into ordered operations. Pending text edits are
/// flushed before any resource op touching the same URI (or descendant), so
/// renames/creates/deletes always observe the correct prior file state.
fn plan_document_changes(document_changes: &[WorkspaceDocumentChange]) -> Vec<WorkspaceEditOp> {
	let mut ops = Vec::new();
	let mut pending: Vec<(String, Vec<TextEdit>)> = Vec::new();

	fn flush_uri(pending: &mut Vec<(String, Vec<TextEdit>)>, ops: &mut Vec<WorkspaceEditOp>, uri: &str) {
		if let Some(index) = pending.iter().position(|(candidate, _)| candidate == uri) {
			let (uri, edits) = pending.remove(index);
			ops.push(WorkspaceEditOp::Text { uri, edits });
		}
	}

	fn flush_subtree(
		pending: &mut Vec<(String, Vec<TextEdit>)>,
		ops: &mut Vec<WorkspaceEditOp>,
		uri: &str,
	) {
		let prefix = if uri.ends_with('/') { uri.to_string() } else { format!("{uri}/") };
		let matches: Vec<String> = pending
			.iter()
			.filter(|(candidate, _)| candidate == uri || candidate.starts_with(&prefix))
			.map(|(candidate, _)| candidate.clone())
			.collect();
		for target in matches {
			flush_uri(pending, ops, &target);
		}
	}

	for change in document_changes {
		match change {
			WorkspaceDocumentChange::Text(text) => {
				if text.edits.is_empty() {
					continue;
				}
				if let Some((_, edits)) = pending
					.iter_mut()
					.find(|(candidate, _)| candidate == &text.text_document.uri)
				{
					edits.extend(text.edits.clone());
				} else {
					pending.push((text.text_document.uri.clone(), text.edits.clone()));
				}
			}
			WorkspaceDocumentChange::Resource(resource) => match resource {
				ResourceOperation::Create { uri, options } => {
					flush_uri(&mut pending, &mut ops, uri);
					ops.push(WorkspaceEditOp::Create { uri: uri.clone(), options: options.clone() });
				}
				ResourceOperation::Rename { old_uri, new_uri, options } => {
					flush_subtree(&mut pending, &mut ops, old_uri);
					flush_subtree(&mut pending, &mut ops, new_uri);
					ops.push(WorkspaceEditOp::Rename {
						old_uri: old_uri.clone(),
						new_uri: new_uri.clone(),
						options: options.clone(),
					});
				}
				ResourceOperation::Delete { uri, options } => {
					flush_subtree(&mut pending, &mut ops, uri);
					ops.push(WorkspaceEditOp::Delete { uri: uri.clone(), options: options.clone() });
				}
			},
		}
	}
	let remaining: Vec<String> = pending.iter().map(|(uri, _)| uri.clone()).collect();
	for uri in remaining {
		flush_uri(&mut pending, &mut ops, &uri);
	}
	ops
}

fn uri_file(uri: &str) -> Result<PathBuf, String> {
	file_uri_path(uri).ok_or_else(|| format!("unsupported non-file LSP edit URI: {uri}"))
}

fn record_change(
	result: &mut WorkspaceEditResult,
	on_executed: &mut dyn FnMut(&ExecutedWorkspaceChange),
	change: ExecutedWorkspaceChange,
) {
	on_executed(&change);
	result.executed.push(change);
}

/// Apply a workspace edit. `on_executed` fires after each committed filesystem
/// mutation so callers can reconcile overlays even when a later op fails.
pub async fn apply_workspace_edit(
	edit: &WorkspaceEdit,
	cwd: &Path,
	mut on_executed: impl FnMut(&ExecutedWorkspaceChange),
) -> Result<WorkspaceEditResult, String> {
	let mut result = WorkspaceEditResult::default();

	if let Some(document_changes) = &edit.document_changes {
		let ops = plan_document_changes(document_changes);
		for op in &ops {
			match op {
				WorkspaceEditOp::Text { uri, edits } => {
					uri_file(uri)?;
					sort_and_validate_text_edits(edits)?;
				}
				WorkspaceEditOp::Create { uri, .. } | WorkspaceEditOp::Delete { uri, .. } => {
					uri_file(uri)?;
				}
				WorkspaceEditOp::Rename { old_uri, new_uri, .. } => {
					uri_file(old_uri)?;
					uri_file(new_uri)?;
				}
			}
		}
		for op in ops {
			match op {
				WorkspaceEditOp::Text { uri, edits } => {
					let path = uri_file(&uri)?;
					apply_text_edits(&path, &edits).await?;
					result.applied.push(format!(
						"Applied {} edit(s) to {}",
						edits.len(),
						format_path_relative_to_cwd(&path, cwd)
					));
					record_change(
						&mut result,
						&mut on_executed,
						ExecutedWorkspaceChange::Edit { uri },
					);
				}
				WorkspaceEditOp::Create { uri, options } => {
					let path = uri_file(&uri)?;
					if let Some(parent) = path.parent() {
						tokio::fs::create_dir_all(parent).await.map_err(|error| error.to_string())?;
					}
					let create_result: Result<(), std::io::Error> = if options.overwrite.unwrap_or(false) {
						tokio::fs::write(&path, "").await
					} else {
						tokio::fs::OpenOptions::new()
							.write(true)
							.create_new(true)
							.open(&path)
							.await
							.map(|_| ())
					};
					if let Err(error) = create_result {
						if options.ignore_if_exists.unwrap_or(false)
							&& !options.overwrite.unwrap_or(false)
							&& error.kind() == std::io::ErrorKind::AlreadyExists
						{
							continue;
						}
						return Err(error.to_string());
					}
					result.applied.push(format!("Created {}", format_path_relative_to_cwd(&path, cwd)));
					record_change(
						&mut result,
						&mut on_executed,
						ExecutedWorkspaceChange::Create { uri },
					);
				}
				WorkspaceEditOp::Rename { old_uri, new_uri, options } => {
					let old_path = uri_file(&old_uri)?;
					let new_path = uri_file(&new_uri)?;
					if let Some(parent) = new_path.parent() {
						tokio::fs::create_dir_all(parent).await.map_err(|error| error.to_string())?;
					}
					if old_path != new_path {
						let executed = rename_resource(
							&old_path,
							&new_path,
							&options,
							cwd,
							&mut result,
							&mut on_executed,
							&old_uri,
							&new_uri,
						)
						.await?;
						if !executed {
							continue;
						}
					}
					result.applied.push(format!(
						"Renamed {} -> {}",
						format_path_relative_to_cwd(&old_path, cwd),
						format_path_relative_to_cwd(&new_path, cwd)
					));
					record_change(
						&mut result,
						&mut on_executed,
						ExecutedWorkspaceChange::Rename { old_uri, new_uri },
					);
				}
				WorkspaceEditOp::Delete { uri, options } => {
					let path = uri_file(&uri)?;
					match delete_resource(&path, &options).await {
						Ok(true) => {}
						Ok(false) => continue,
						Err(error) => return Err(error),
					}
					result.applied.push(format!("Deleted {}", format_path_relative_to_cwd(&path, cwd)));
					record_change(
						&mut result,
						&mut on_executed,
						ExecutedWorkspaceChange::Delete { uri },
					);
				}
			}
		}
	} else if let Some(changes) = &edit.changes {
		for edits in changes.values() {
			sort_and_validate_text_edits(edits)?;
		}
		for (uri, edits) in changes {
			if edits.is_empty() {
				continue;
			}
			let path = uri_file(uri)?;
			apply_text_edits(&path, edits).await?;
			result.applied.push(format!(
				"Applied {} edit(s) to {}",
				edits.len(),
				format_path_relative_to_cwd(&path, cwd)
			));
			record_change(
				&mut result,
				&mut on_executed,
				ExecutedWorkspaceChange::Edit { uri: uri.clone() },
			);
		}
	}

	Ok(result)
}

#[allow(clippy::too_many_arguments)]
async fn rename_resource(
	old_path: &Path,
	new_path: &Path,
	options: &RenameFileOptions,
	cwd: &Path,
	result: &mut WorkspaceEditResult,
	on_executed: &mut dyn FnMut(&ExecutedWorkspaceChange),
	old_uri: &str,
	new_uri: &str,
) -> Result<bool, String> {
	let target_exists = tokio::fs::symlink_metadata(new_path).await.is_ok();
	if target_exists {
		if !options.overwrite.unwrap_or(false) {
			if options.ignore_if_exists.unwrap_or(false) {
				return Ok(false);
			}
			return Err(format!(
				"rename target already exists: {}",
				format_path_relative_to_cwd(new_path, cwd)
			));
		}
	}

	let mut displaced: Option<(PathBuf, PathBuf)> = None;
	if target_exists {
		let same_file = match (tokio::fs::symlink_metadata(old_path).await, tokio::fs::symlink_metadata(new_path).await) {
			(Ok(source), Ok(target)) => {
				#[cfg(unix)]
				{
					use std::os::unix::fs::MetadataExt;
					source.dev() == target.dev() && source.ino() == target.ino()
				}
				#[cfg(not(unix))]
				{
					let _ = (source, target);
					false
				}
			}
			_ => false,
		};
		if !same_file {
			let hold_dir = create_hold_dir(new_path).await?;
			let hold_file = hold_dir.join(
				new_path
					.file_name()
					.map(std::ffi::OsStr::to_os_string)
					.unwrap_or_else(|| "displaced".into()),
			);
			if let Err(error) = tokio::fs::rename(new_path, &hold_file).await {
				let _ = tokio::fs::remove_dir_all(&hold_dir).await;
				return Err(error.to_string());
			}
			displaced = Some((hold_dir, hold_file));
		}
	}

	if let Err(error) = tokio::fs::rename(old_path, new_path).await {
		if let Some((hold_dir, hold_file)) = &displaced {
			if tokio::fs::rename(hold_file, new_path).await.is_err() {
				record_change(
					result,
					on_executed,
					ExecutedWorkspaceChange::Delete { uri: new_uri.to_string() },
				);
			}
			let _ = tokio::fs::remove_dir_all(hold_dir).await;
		}
		let _ = old_uri;
		return Err(error.to_string());
	}
	if let Some((hold_dir, _)) = displaced {
		let _ = tokio::fs::remove_dir_all(&hold_dir).await;
	}
	Ok(true)
}

async fn create_hold_dir(new_path: &Path) -> Result<PathBuf, String> {
	let parent = new_path.parent().unwrap_or_else(|| Path::new("."));
	for _ in 0..100 {
		let candidate = parent.join(format!(
			".omp-displaced-{}-{}",
			std::process::id(),
			TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
		));
		match tokio::fs::create_dir(&candidate).await {
			Ok(()) => return Ok(candidate),
			Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
			Err(error) => return Err(error.to_string()),
		}
	}
	Err("unable to allocate a displacement directory for LSP rename".to_string())
}

async fn delete_resource(path: &Path, options: &DeleteFileOptions) -> Result<bool, String> {
	let metadata = match tokio::fs::symlink_metadata(path).await {
		Ok(metadata) => metadata,
		Err(error) => {
			if options.ignore_if_not_exists.unwrap_or(false)
				&& error.kind() == std::io::ErrorKind::NotFound
			{
				return Ok(false);
			}
			return Err(error.to_string());
		}
	};
	let result = if metadata.is_dir() && !metadata.file_type().is_symlink() {
		if options.recursive.unwrap_or(false) {
			tokio::fs::remove_dir_all(path).await
		} else {
			tokio::fs::remove_dir(path).await
		}
	} else {
		tokio::fs::remove_file(path).await
	};
	result.map(|()| true).map_err(|error| error.to_string())
}

// =============================================================================
// Text files
// =============================================================================

/// Apply text edits to one file with an atomic replace.
pub async fn apply_text_edits(file_path: &Path, edits: &[TextEdit]) -> Result<(), String> {
	let content = tokio::fs::read_to_string(file_path)
		.await
		.map_err(|error| format!("{}: {error}", file_path.display()))?;
	let updated = apply_text_edits_to_string(&content, edits)?;
	write_text_atomic(file_path, &updated).await
}

pub async fn write_text_atomic(path: &Path, content: &str) -> Result<(), String> {
	// Shared with builtin edit/write tools: resolves a final symlink to its
	// target, writes an unpublished temp file beside it, then renames. This
	// preserves a symlinked destination instead of replacing the link.
	crate::tools::write_atomic(path, content.as_bytes(), &CancellationToken::new())
		.await
		.map_err(|error| error.to_string())
}

// =============================================================================
// Rename references + move as one unit
// =============================================================================

#[derive(Debug, Clone)]
pub struct RenameReferenceEdit {
	pub file_path: PathBuf,
	pub edits: Vec<TextEdit>,
}

/// Apply a rename's reference edits and then move `source` -> `dest` as one
/// unit. If the move fails, every edited reference file is restored from its
/// pre-edit snapshot before the original error is returned.
pub async fn apply_edits_then_rename(
	references: &[RenameReferenceEdit],
	source: &Path,
	dest: &Path,
) -> Result<(), String> {
	let mut backups = Vec::new();
	for reference in references {
		let original = tokio::fs::read_to_string(&reference.file_path)
			.await
			.map_err(|error| format!("{}: {error}", reference.file_path.display()))?;
		backups.push((reference.file_path.clone(), original));
		if let Err(error) = apply_text_edits(&reference.file_path, &reference.edits).await {
			for (file_path, original) in backups {
				let _ = write_text_atomic(&file_path, &original).await;
			}
			return Err(error);
		}
	}
	let move_result = async {
		if let Some(parent) = dest.parent() {
			tokio::fs::create_dir_all(parent).await.map_err(|error| error.to_string())?;
		}
		tokio::fs::rename(source, dest).await.map_err(|error| error.to_string())
	}
	.await;
	if let Err(error) = move_result {
		for (file_path, original) in backups {
			let _ = write_text_atomic(&file_path, &original).await;
		}
		return Err(error);
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::sync::atomic::AtomicUsize;

	#[test]
	fn applies_multiline_edits_bottom_up() {
		let content = "one\ntwo\nthree\n";
		let edits = vec![
			TextEdit {
				range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: 3 } },
				new_text: "ONE".into(),
				insert_text_format: None,
			},
			TextEdit {
				range: Range { start: Position { line: 1, character: 0 }, end: Position { line: 2, character: 5 } },
				new_text: "TWO-THREE".into(),
				insert_text_format: None,
			},
		];
		assert_eq!(apply_text_edits_to_string(content, &edits).unwrap(), "ONE\nTWO-THREE\n");
	}

	#[test]
	fn rejects_overlapping_edits() {
		let range = Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: 2 } };
		let edits = vec![
			TextEdit { range, new_text: "a".into(), insert_text_format: None },
			TextEdit { range, new_text: "b".into(), insert_text_format: None },
		];
		assert!(sort_and_validate_text_edits(&edits).is_err());
	}

	#[test]
	fn snippet_edits_are_rejected() {
		let edits = vec![TextEdit {
			range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: 0 } },
			new_text: "x".into(),
			insert_text_format: Some(2),
		}];
		assert!(sort_and_validate_text_edits(&edits).is_err());
	}

	#[test]
	fn document_changes_flush_text_before_rename() {
		let uri = "file:///tmp/a.rs".to_string();
		let changes = vec![
			WorkspaceDocumentChange::Text(TextDocumentEdit {
				text_document: super::super::types::OptionalVersionedTextDocumentIdentifier {
					uri: uri.clone(),
					version: None,
				},
				edits: vec![TextEdit {
					range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: 0 } },
					new_text: "x".into(),
					insert_text_format: None,
				}],
			}),
			WorkspaceDocumentChange::Resource(ResourceOperation::Rename {
				old_uri: uri,
				new_uri: "file:///tmp/b.rs".to_string(),
				options: RenameFileOptions::default(),
			}),
		];
		let ops = plan_document_changes(&changes);
		assert!(matches!(ops[0], WorkspaceEditOp::Text { .. }));
		assert!(matches!(ops[1], WorkspaceEditOp::Rename { .. }));
	}

	fn test_dir(label: &str) -> PathBuf {
		static COUNTER: AtomicUsize = AtomicUsize::new(0);
		let dir = std::env::temp_dir().join(format!(
			"pi-lsp-edits-{label}-{}-{}",
			std::process::id(),
			COUNTER.fetch_add(1, Ordering::Relaxed)
		));
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).expect("temp dir");
		dir
	}

	#[tokio::test]
	async fn stale_batch_is_rejected_before_first_write() {
		let dir = test_dir("stale");
		let first = dir.join("first.txt");
		let second = dir.join("second.txt");
		std::fs::write(&first, "abc").unwrap();
		std::fs::write(&second, "xyz").unwrap();

		let range = Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: 2 } };
		let overlapping = vec![
			TextEdit { range, new_text: "1".into(), insert_text_format: None },
			TextEdit { range, new_text: "2".into(), insert_text_format: None },
		];
		let mut changes = std::collections::BTreeMap::new();
		changes.insert(
			super::super::utils::file_to_uri(&first),
			vec![TextEdit { range, new_text: "OK".into(), insert_text_format: None }],
		);
		changes.insert(super::super::utils::file_to_uri(&second), overlapping);
		let edit = WorkspaceEdit { changes: Some(changes), document_changes: None };
		let result = apply_workspace_edit(&edit, &dir, |_| {}).await;
		assert!(result.is_err(), "overlap in the second URI must fail the batch");
		assert_eq!(std::fs::read_to_string(&first).unwrap(), "abc");
		let _ = std::fs::remove_dir_all(dir);
	}

	#[tokio::test]
	async fn failed_move_restores_reference_edits() {
		let dir = test_dir("rollback");
		let reference = dir.join("ref.txt");
		let source = dir.join("source.txt");
		std::fs::write(&reference, "old").unwrap();
		std::fs::write(&source, "body").unwrap();
		let blocker = dir.join("blocker");
		std::fs::write(&blocker, "file").unwrap();
		let dest = blocker.join("nested").join("dest.txt");

		let reference_edits = vec![RenameReferenceEdit {
			file_path: reference.clone(),
			edits: vec![TextEdit {
				range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: 3 } },
				new_text: "new".into(),
				insert_text_format: None,
			}],
		}];
		let result = apply_edits_then_rename(&reference_edits, &source, &dest).await;
		assert!(result.is_err());
		assert_eq!(std::fs::read_to_string(&reference).unwrap(), "old");
		assert!(source.exists());
		let _ = std::fs::remove_dir_all(dir);
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn atomic_write_preserves_final_symlink() {
		let dir = test_dir("symlink");
		let target = dir.join("real.txt");
		let link = dir.join("link.txt");
		std::fs::write(&target, "old").unwrap();
		std::os::unix::fs::symlink(&target, &link).unwrap();

		super::write_text_atomic(&link, "new").await.unwrap();

		assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
		assert_eq!(std::fs::read_link(&link).unwrap(), target);
		assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
		let _ = std::fs::remove_dir_all(dir);
	}
}
