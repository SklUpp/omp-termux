//! Behavioural tests for the non-hashline edit engines. Every test owns a
//! fresh temp directory and only removes that directory.

use std::{
	collections::HashMap,
	path::{Path, PathBuf},
};

use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::state::AgentContext;

fn temp_dir(label: &str) -> PathBuf {
	let path = std::env::temp_dir().join(format!(
		"pi-edit-variants-{label}-{}-{}",
		std::process::id(),
		std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
	));
	std::fs::create_dir_all(&path).unwrap();
	path
}

fn context(cwd: &Path) -> AgentContext {
	AgentContext {
		cwd: cwd.to_path_buf(),
		env: HashMap::new(),
		cancellation_token: CancellationToken::new(),
		approver: None,
		ask: None,
	}
}

fn read(path: &Path) -> String {
	String::from_utf8(std::fs::read(path).unwrap()).unwrap()
}

#[tokio::test]
async fn replace_refuses_ambiguous_old_string() {
	let dir = temp_dir("replace-ambiguous");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "let x = 1;\nlet x = 1;\n").unwrap();
	let ctx = context(&dir);
	let err = super::execute(
		super::EditMode::Replace,
		&json!({ "path": "demo.txt", "old_string": "let x = 1;", "new_string": "let x = 2;" }),
		&ctx,
	)
	.await
	.unwrap_err();
	let message = err.to_string();
	assert!(message.contains("Found 2 occurrences"), "{message}");
	assert!(message.contains("Add more context lines to disambiguate"), "{message}");
	assert_eq!(read(&file), "let x = 1;\nlet x = 1;\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn replace_preserves_crlf_and_bom() {
	let dir = temp_dir("replace-crlf");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "\u{feff}a\r\nb\r\nc\r\n").unwrap();
	let ctx = context(&dir);
	let result = super::execute(
		super::EditMode::Replace,
		&json!({ "path": "demo.txt", "old_string": "b", "new_string": "B", "replace_all": true }),
		&ctx,
	)
	.await
	.unwrap();
	assert!(result.content.contains("demo.txt"));
	assert_eq!(read(&file), "\u{feff}a\r\nB\r\nc\r\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn patch_create_overwrites_and_update_delete_move_are_explicit() {
	let dir = temp_dir("patch-ops");
	let ctx = context(&dir);
	let target = dir.join("note.txt");
	let sentinel = dir.join("keep.txt");
	std::fs::write(&sentinel, "keep\n").unwrap();

	super::execute(
		super::EditMode::Patch,
		&json!({ "path": "note.txt", "edits": [{ "op": "create", "diff": "one\ntwo\n" }] }),
		&ctx,
	)
	.await
	.unwrap();
	assert_eq!(read(&target), "one\ntwo\n");

	// Patch-mode create is documented as a full overwrite.
	super::execute(
		super::EditMode::Patch,
		&json!({ "path": "note.txt", "edits": [{ "op": "create", "diff": "replaced\n" }] }),
		&ctx,
	)
	.await
	.unwrap();
	assert_eq!(read(&target), "replaced\n");

	// Update one line, then move; then delete, leaving the sibling untouched.
	super::execute(
		super::EditMode::Patch,
		&json!({ "path": "note.txt", "edits": [{ "op": "update", "rename": "moved.txt", "diff": "@@\n-replaced\n+updated" }] }),
		&ctx,
	)
	.await
	.unwrap();
	assert!(!target.exists());
	assert_eq!(read(&dir.join("moved.txt")), "updated\n");
	assert_eq!(read(&sentinel), "keep\n");

	super::execute(
		super::EditMode::Patch,
		&json!({ "path": "moved.txt", "edits": [{ "op": "delete" }] }),
		&ctx,
	)
	.await
	.unwrap();
	assert!(!dir.join("moved.txt").exists());
	assert_eq!(read(&sentinel), "keep\n");

	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn patch_update_preserves_crlf_newlines() {
	let dir = temp_dir("patch-crlf");
	let file = dir.join("win.txt");
	std::fs::write(&file, "alpha\r\nbeta\r\ngamma\r\n").unwrap();
	let ctx = context(&dir);
	super::execute(
		super::EditMode::Patch,
		&json!({ "path": "win.txt", "edits": [{ "op": "update", "diff": "@@\n-beta\n+BETA" }] }),
		&ctx,
	)
	.await
	.unwrap();
	assert_eq!(read(&file), "alpha\r\nBETA\r\ngamma\r\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn apply_patch_create_and_move_refuse_existing_destinations() {
	let dir = temp_dir("apply-patch-trust");
	let ctx = context(&dir);
	let existing = dir.join("exists.txt");
	let source = dir.join("source.txt");
	std::fs::write(&existing, "existing\n").unwrap();
	std::fs::write(&source, "source\n").unwrap();

	let err = super::execute(
		super::EditMode::ApplyPatch,
		&json!({ "input": "*** Begin Patch\n*** Add File: exists.txt\n+overwrite\n*** End Patch" }),
		&ctx,
	)
	.await
	.unwrap_err();
	assert!(err.to_string().contains("already exists"), "{err}");
	assert_eq!(read(&existing), "existing\n");

	let err = super::execute(
		super::EditMode::ApplyPatch,
		&json!({ "input": "*** Begin Patch\n*** Update File: source.txt\n*** Move to: exists.txt\n@@\n-source\n+moved\n*** End Patch" }),
		&ctx,
	)
	.await
	.unwrap_err();
	assert!(err.to_string().contains("destination already exists"), "{err}");
	assert_eq!(read(&source), "source\n");
	assert_eq!(read(&existing), "existing\n");

	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn apply_patch_delete_removes_only_named_file() {
	let dir = temp_dir("apply-patch-delete");
	let doomed = dir.join("doomed.txt");
	let keep = dir.join("keep.txt");
	std::fs::write(&doomed, "bye\n").unwrap();
	std::fs::write(&keep, "stay\n").unwrap();
	let ctx = context(&dir);
	super::execute(
		super::EditMode::ApplyPatch,
		&json!({ "input": "*** Begin Patch\n*** Delete File: doomed.txt\n*** End Patch" }),
		&ctx,
	)
	.await
	.unwrap();
	assert!(!doomed.exists());
	assert_eq!(read(&keep), "stay\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_applies_unambiguous_rewrite_and_normalized_fallback() {
	let dir = temp_dir("sloppy-basic");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "fn main() {\n    let avlue = 1;\n}\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«fn main() {\n    let avlue = 1;\n}\n»\nfn main() {\n    let value = 2;\n}";
	super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap();
	assert_eq!(read(&file), "fn main() {\n    let value = 2;\n}\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_normalized_match_tolerates_indentation_drift() {
	let dir = temp_dir("sloppy-normalized");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "fn main() {\n    let x = 1;\n}\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«fn main() {\nlet x = 1;\n}\n»\nfn main() {\n    let x = 2;\n}";
	super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap();
	assert_eq!(read(&file), "fn main() {\n    let x = 2;\n}\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_refuses_ambiguous_match_atomically() {
	let dir = temp_dir("sloppy-ambiguous");
	let file = dir.join("demo.txt");
	let other = dir.join("other.txt");
	std::fs::write(&file, "let a = 1;\nlet a = 1;\n").unwrap();
	std::fs::write(&other, "untouched\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«let a = 1;\n»\nlet a = 2;\n§other.txt\n«untouched\n»\nchanged";
	let err = super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap_err();
	let message = err.to_string();
	assert!(message.contains("ambiguous"), "{message}");
	assert!(message.contains("No files were modified"), "{message}");
	assert_eq!(read(&file), "let a = 1;\nlet a = 1;\n");
	assert_eq!(read(&other), "untouched\n");
	std::fs::remove_dir_all(&dir).unwrap();
}
