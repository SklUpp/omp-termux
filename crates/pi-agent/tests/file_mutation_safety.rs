//! Safety boundaries for the file-mutating tools that are reachable today:
//! the non-hashline `edit` engines (internal-URL refusal, UTF-8 refusal,
//! symlink/mode preservation, BOM+CRLF round-trip, hashline dispatch refusal)
//! and the staged AST preview gate (stale requeue, partial-apply retry).
//!
//! Every case drives a real tool entry point against a task-owned temp
//! directory and asserts the on-disk bytes afterwards.

use std::{
	fs,
	path::{Path, PathBuf},
	sync::{
		Arc,
		atomic::{AtomicUsize, Ordering},
	},
};

use pi_agent::{
	AgentContext, ToolError,
	tools::{
		AstEditTool, PendingStore,
		edit_variants::{EditMode, execute},
		hashline::SnapshotStore,
	},
};
use serde_json::json;

struct TempDir {
	path: PathBuf,
}

impl TempDir {
	fn new(label: &str) -> Self {
		static NEXT: AtomicUsize = AtomicUsize::new(0);
		let unique = format!(
			"pi-mutation-{label}-{}-{}",
			std::process::id(),
			NEXT.fetch_add(1, Ordering::SeqCst)
		);
		let path = std::env::temp_dir().join(unique);
		fs::create_dir_all(&path).expect("create temp dir");
		Self { path }
	}

	fn write(&self, name: &str, content: &str) -> PathBuf {
		let path = self.path.join(name);
		if let Some(parent) = path.parent() {
			fs::create_dir_all(parent).expect("create parent");
		}
		fs::write(&path, content).expect("write fixture");
		path
	}

	fn read(&self, name: &str) -> String {
		String::from_utf8(fs::read(self.path.join(name)).expect("read fixture")).expect("utf-8")
	}
}

impl Drop for TempDir {
	fn drop(&mut self) {
		let _ = fs::remove_dir_all(&self.path);
	}
}

fn ctx(dir: &TempDir) -> AgentContext {
	AgentContext::new(dir.path.clone())
}

fn ast_args(paths: &[&str]) -> serde_json::Value {
	json!({
		"ops": [{ "pat": "foo($A)", "out": "bar($A)" }],
		"paths": paths,
	})
}

#[tokio::test]
async fn internal_url_paths_are_refused_without_a_filesystem_fallback() {
	let dir = TempDir::new("internal-urls");
	let ctx = ctx(&dir);

	let cases = [
		(
			EditMode::Replace,
			json!({ "path": "artifact://note", "old_string": "a", "new_string": "b" }),
		),
		(
			EditMode::Patch,
			json!({ "path": "agent://child", "edits": [{ "op": "create", "diff": "x\n" }] }),
		),
		(
			EditMode::ApplyPatch,
			json!({ "input": "*** Begin Patch\n*** Add File: skill://rule\n+x\n*** End Patch" }),
		),
		(
			EditMode::Sloppy,
			json!({ "input": "§local://notes\n«a\n»\nb" }),
		),
	];
	for (mode, args) in cases {
		let error = execute(mode, &args, &ctx)
			.await
			.expect_err(&format!("{mode:?} must refuse an internal URL"));
		let message = error.to_string();
		assert!(
			message.contains("must be resolved through the proper protocol handler"),
			"{mode:?}: {message}"
		);
	}
	assert_eq!(
		fs::read_dir(&dir.path).expect("dir").count(),
		0,
		"no authored URL may degrade into a local file"
	);
}

#[tokio::test]
async fn non_utf8_targets_are_refused_without_a_lossy_rewrite() {
	let dir = TempDir::new("non-utf8");
	let file = dir.write("bad.txt", "");
	fs::write(&file, [b'f', b'o', b'o', 0xff, b'\n']).expect("write bytes");
	let ctx = ctx(&dir);

	let error = execute(
		EditMode::Replace,
		&json!({ "path": "bad.txt", "old_string": "foo", "new_string": "bar" }),
		&ctx,
	)
	.await
	.expect_err("non-UTF-8 must be refused");
	assert!(error.to_string().contains("not valid UTF-8"), "{error}");
	assert_eq!(fs::read(&file).expect("read"), vec![b'f', b'o', b'o', 0xff, b'\n']);
}

#[cfg(unix)]
#[tokio::test]
async fn replace_through_a_symlink_updates_the_target_and_keeps_the_link() {
	use std::os::unix::fs::PermissionsExt;

	let dir = TempDir::new("symlink");
	let target = dir.write("real.txt", "alpha\n");
	fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).expect("chmod");
	let link = dir.path.join("link.txt");
	std::os::unix::fs::symlink("real.txt", &link).expect("symlink");
	let ctx = ctx(&dir);

	execute(
		EditMode::Replace,
		&json!({ "path": "link.txt", "old_string": "alpha", "new_string": "beta" }),
		&ctx,
	)
	.await
	.expect("replace through symlink");

	assert!(
		fs::symlink_metadata(&link).expect("link meta").file_type().is_symlink(),
		"the authored symlink must survive the write"
	);
	assert_eq!(fs::read_link(&link).expect("link target"), Path::new("real.txt"));
	assert_eq!(dir.read("real.txt"), "beta\n");
	assert_eq!(
		fs::metadata(&target).expect("meta").permissions().mode() & 0o777,
		0o640,
		"the destination mode must be preserved"
	);
}

#[tokio::test]
async fn apply_patch_update_round_trips_bom_and_crlf() {
	let dir = TempDir::new("apply-patch-bom");
	dir.write("win.txt", "\u{feff}alpha\r\nbeta\r\n");
	let ctx = ctx(&dir);

	execute(
		EditMode::ApplyPatch,
		&json!({ "input": "*** Begin Patch\n*** Update File: win.txt\n@@\n-alpha\n+ALPHA\n*** End Patch" }),
		&ctx,
	)
	.await
	.expect("apply patch update");
	assert_eq!(dir.read("win.txt"), "\u{feff}ALPHA\r\nbeta\r\n");
}

#[tokio::test]
async fn hashline_mode_is_not_served_by_the_plain_engines() {
	let dir = TempDir::new("hashline-dispatch");
	let file = dir.write("a.txt", "keep\n");
	let ctx = ctx(&dir);

	let error = execute(
		EditMode::Hashline,
		&json!({ "input": "[a.txt#0000]\nPUT 1.=1:\n+changed\n" }),
		&ctx,
	)
	.await
	.expect_err("hashline must be dispatched to the native engine");
	assert!(
		error.to_string().contains("native hashline engine"),
		"{error}"
	);
	assert_eq!(fs::read_to_string(&file).expect("read"), "keep\n");
}

#[tokio::test]
async fn stale_ast_edit_preview_writes_nothing_and_a_fresh_preview_applies() {
	let dir = TempDir::new("ast-stale");
	let first = dir.write("a.ts", "const a = foo(1);\n");
	let second = dir.write("b.ts", "const b = foo(1);\n");
	let pending = Arc::new(PendingStore::new());
	let tool = AstEditTool::new(Arc::new(SnapshotStore::new()), Arc::clone(&pending));
	let ctx = ctx(&dir);

	tool.execute(&ast_args(&["a.ts", "b.ts"]), &ctx)
		.await
		.expect("preview");
	assert_eq!(pending.len(), 1);

	// Same replacement count, different bytes: a counts-only gate would apply.
	dir.write("b.ts", "const b = foo(2);\n");
	let stale = pending.resolve(&ctx, "apply").await.expect("stale resolve");
	assert!(stale.is_error, "{}", stale.content);
	assert!(stale.content.contains("stale / no longer matches"), "{}", stale.content);
	assert_eq!(fs::read_to_string(&first).expect("a"), "const a = foo(1);\n");
	assert_eq!(fs::read_to_string(&second).expect("b"), "const b = foo(2);\n");
	let details = stale.details.as_ref().expect("details");
	// `PendingStore::resolve` decorates the tool result and moves its details
	// under `sourceResultDetails`.
	let source = &details["sourceResultDetails"];
	assert_eq!(source["stale"], json!(true));
	assert_eq!(source["partial"], json!(false));
	assert_eq!(source["writtenFiles"], json!([]));
	assert_eq!(source["attemptWrittenFiles"], json!([]));
	assert_eq!(pending.len(), 1, "a failed apply keeps the preview for retry");

	// A fresh preview reads the new bytes; resolving it applies the current state.
	tool.execute(&ast_args(&["a.ts", "b.ts"]), &ctx)
		.await
		.expect("fresh preview");
	assert_eq!(pending.len(), 2);
	let applied = pending.resolve(&ctx, "apply current").await.expect("fresh resolve");
	assert!(!applied.is_error, "{}", applied.content);
	assert_eq!(fs::read_to_string(&first).expect("a"), "const a = bar(1);\n");
	assert_eq!(fs::read_to_string(&second).expect("b"), "const b = bar(2);\n");
	assert_eq!(pending.len(), 1, "the superseded stale preview stays staged");
	assert!(!pending.reject(&ctx, "discard stale").await.expect("reject").is_error);
	assert!(pending.is_empty());
}

/// Deterministic partial apply without permission tricks: the second staged
/// file's name is readable but long enough that the engine's
/// `.<name>.tmp-<16 hex>` sibling exceeds `NAME_MAX`, so that one write fails
/// after the first file already landed.
#[cfg(unix)]
#[tokio::test]
async fn partial_ast_apply_retry_reports_prior_writes_and_rewrites_nothing() {
	let dir = TempDir::new("ast-partial");
	let first = dir.write("a.ts", "const a = foo(1);\n");
	let blocked_name = format!("{}.ts", "x".repeat(237));
	assert_eq!(blocked_name.len(), 240);
	assert!(
		blocked_name.len() + "..tmp-0123456789abcdef".len() > 255,
		"the temp sibling must exceed NAME_MAX"
	);
	let blocked = dir.write(&format!("blocked/{blocked_name}"), "const b = foo(1);\n");
	let blocked_display = format!("blocked/{blocked_name}");
	let pending = Arc::new(PendingStore::new());
	let tool = AstEditTool::new(Arc::new(SnapshotStore::new()), Arc::clone(&pending));
	let ctx = ctx(&dir);

	tool.execute(&ast_args(&["a.ts", &blocked_display]), &ctx)
		.await
		.expect("preview");

	let partial = pending.resolve(&ctx, "apply").await.expect("partial resolve");
	assert!(partial.is_error, "{}", partial.content);
	let details = partial.details.as_ref().expect("details");
	let source = &details["sourceResultDetails"];
	assert_eq!(source["partial"], json!(true), "{}", partial.content);
	assert_eq!(source["writtenFiles"], json!(["a.ts"]));
	assert_eq!(source["snapshotInvalidations"], json!(["a.ts"]));
	assert_eq!(source["failedFile"], json!(blocked_display));
	assert_eq!(fs::read_to_string(&first).expect("a"), "const a = bar(1);\n");
	assert_eq!(fs::read_to_string(&blocked).expect("b"), "const b = foo(1);\n");
	assert_eq!(pending.len(), 1, "a partial apply keeps the preview for retry");

	let retry = pending.resolve(&ctx, "retry").await.expect("retry");
	assert!(retry.is_error, "{}", retry.content);
	assert!(retry.content.contains("this attempt applied nothing"), "{}", retry.content);
	assert!(
		retry.content.contains("Previously written by this staged action: a.ts"),
		"{}",
		retry.content
	);
	let details = retry.details.as_ref().expect("details");
	let source = &details["sourceResultDetails"];
	assert_eq!(source["stale"], json!(true));
	assert_eq!(source["partial"], json!(true));
	assert_eq!(source["writtenFiles"], json!(["a.ts"]));
	assert_eq!(source["attemptWrittenFiles"], json!([]));
	assert_eq!(fs::read_to_string(&first).expect("a"), "const a = bar(1);\n");
	assert_eq!(fs::read_to_string(&blocked).expect("b"), "const b = foo(1);\n");
}

// ── Sloppy recovery regressions (EditEngineReview 1, 3, 5, 6) ─────────────
//
// Consumer-level acceptance through the public dispatch. They encode the
// intended post-fix behavior, so on a snapshot without the engine fixes they
// fail: finding 1 panics at the UTF-8 boundary, finding 3 writes the inline
// value instead of the authored REWRITE, and findings 5/6 do not refuse or
// delete as asserted. They are kept red on purpose.

fn refusal_message(error: &ToolError) -> &str {
	match error {
		ToolError::ExecutionFailed(message) | ToolError::InvalidArguments(message) => message,
		other => panic!("expected an input or execution refusal, got {other:?}"),
	}
}

#[tokio::test]
async fn sloppy_deletes_a_whole_first_line_that_starts_with_multibyte_text() {
	let dir = TempDir::new("sloppy-cjk-delete");
	let file = dir.write("cjk.txt", "日本語\nsecond\n");
	let ctx = ctx(&dir);

	execute(EditMode::Sloppy, &json!({ "input": "§cjk.txt\n«日本語\n»" }), &ctx)
		.await
		.expect("whole-line deletion at offset 0");
	assert_eq!(fs::read_to_string(&file).expect("read"), "second\n");
}

#[tokio::test]
async fn sloppy_mixed_inline_selection_and_rewrite_keeps_the_authored_rewrite() {
	let dir = TempDir::new("sloppy-mixed-rewrite");
	let file = dir.write("mixed.txt", "let a = 1;\n");
	let ctx = ctx(&dir);

	execute(
		EditMode::Sloppy,
		&json!({ "input": "§mixed.txt\n«let a = ⟪1│2⟫;\n»\nlet a = 3;" }),
		&ctx,
	)
	.await
	.expect("mixed inline + rewrite");
	assert_eq!(
		fs::read_to_string(&file).expect("read"),
		"let a = 3;\n",
		"a non-redundant REWRITE is the final text, not the inline selection"
	);
}

#[tokio::test]
async fn sloppy_rejects_a_rewrite_carrying_selection_markers_and_keeps_the_file() {
	let dir = TempDir::new("sloppy-marker-guard");
	let file = dir.write("markers.txt", "x\n");
	let ctx = ctx(&dir);

	let error = execute(
		EditMode::Sloppy,
		&json!({ "input": "§markers.txt\n«x\n»\n⟪y⟫" }),
		&ctx,
	)
	.await
	.expect_err("REWRITE markers must be refused");
	let message = refusal_message(&error).to_lowercase();
	assert!(
		message.contains("marker") || message.contains('⟪'),
		"the refusal must name the rejected marker: {message}"
	);
	assert_eq!(fs::read_to_string(&file).expect("read"), "x\n", "a refused payload writes nothing");
}

#[tokio::test]
async fn sloppy_rejects_an_unclaimed_ellipsis_in_the_rewrite_and_keeps_the_file() {
	let dir = TempDir::new("sloppy-ellipsis-guard");
	let file = dir.write("dots.txt", "a\nb\n");
	let ctx = ctx(&dir);

	let error = execute(
		EditMode::Sloppy,
		&json!({ "input": "§dots.txt\n«a\n»\n…" }),
		&ctx,
	)
	.await
	.expect_err("an unclaimed whole-line … must be refused");
	let message = refusal_message(&error).to_lowercase();
	assert!(
		message.contains('…') || message.contains("ellipsis") || message.contains("gap"),
		"the refusal must name the unclaimed ellipsis: {message}"
	);
	assert_eq!(
		fs::read_to_string(&file).expect("read"),
		"a\nb\n",
		"an ellipsis must not silently delete a line"
	);
}

#[tokio::test]
async fn sloppy_whole_match_deletion_form_removes_the_line() {
	let dir = TempDir::new("sloppy-whole-match-delete");
	let file = dir.write("delete.txt", "keep\nremove me\nkeep2\n");
	let ctx = ctx(&dir);

	execute(
		EditMode::Sloppy,
		&json!({ "input": "§delete.txt\n«remove me\n⟪│⟫" }),
		&ctx,
	)
	.await
	.expect("sole ⟪│⟫ at the match end deletes the match");
	assert_eq!(fs::read_to_string(&file).expect("read"), "keep\nkeep2\n");
}

#[tokio::test]
async fn ambiguous_replace_refuses_and_replace_all_is_explicit() {
	let dir = TempDir::new("replace-ambiguous");
	dir.write("demo.txt", "let x = 1;\nlet x = 1;\n");
	let ctx = ctx(&dir);

	let error = execute(
		EditMode::Replace,
		&json!({ "path": "demo.txt", "old_string": "let x = 1;", "new_string": "let x = 2;" }),
		&ctx,
	)
	.await
	.expect_err("ambiguous replace must fail");
	assert!(error.to_string().contains("Found 2 occurrences"), "{error}");
	assert_eq!(
		dir.read("demo.txt"),
		"let x = 1;\nlet x = 1;\n",
		"a refusal must not write"
	);

	execute(
		EditMode::Replace,
		&json!({
			"path": "demo.txt",
			"old_string": "let x = 1;",
			"new_string": "let x = 2;",
			"replace_all": true,
		}),
		&ctx,
	)
	.await
	.expect("explicit replace_all");
	assert_eq!(dir.read("demo.txt"), "let x = 2;\nlet x = 2;\n");
}

#[tokio::test]
async fn sloppy_ambiguity_leaves_every_section_untouched() {
	let dir = TempDir::new("sloppy-ambiguous");
	dir.write("a.txt", "let x = 1;\nlet x = 1;\n");
	dir.write("b.txt", "b\n");
	let ctx = ctx(&dir);

	let error = execute(
		EditMode::Sloppy,
		&json!({ "input": "§a.txt\n«let x = 1;\n»\nlet x = 2;\n§b.txt\n«b\n»\nB" }),
		&ctx,
	)
	.await
	.expect_err("non-unique match must fail");
	let message = error.to_string();
	assert!(message.contains("No files were modified"), "{message}");
	assert_eq!(dir.read("a.txt"), "let x = 1;\nlet x = 1;\n");
	assert_eq!(dir.read("b.txt"), "b\n", "later sections must not land");
}

#[tokio::test]
async fn patch_move_and_delete_refuse_non_file_targets() {
	let dir = TempDir::new("patch-refusals");
	dir.write("folder/inner.txt", "inner\n");
	dir.write("note.txt", "note\n");
	dir.write("existing.txt", "existing\n");
	let ctx = ctx(&dir);

	let error = execute(
		EditMode::Patch,
		&json!({ "path": "folder", "edits": [{ "op": "delete" }] }),
		&ctx,
	)
	.await
	.expect_err("deleting a directory must fail");
	assert!(error.to_string().contains("is not a regular file"), "{error}");
	assert_eq!(dir.read("folder/inner.txt"), "inner\n", "directory target stays intact");

	let error = execute(
		EditMode::Patch,
		&json!({ "path": "note.txt", "edits": [{ "op": "update", "rename": "note.txt", "diff": "@@\n-note\n+changed" }] }),
		&ctx,
	)
	.await
	.expect_err("rename onto itself must fail");
	assert!(error.to_string().contains("same as source path"), "{error}");
	assert_eq!(dir.read("note.txt"), "note\n");

	let error = execute(
		EditMode::Patch,
		&json!({ "path": "note.txt", "edits": [{ "op": "update", "rename": "existing.txt", "diff": "@@\n-note\n+changed" }] }),
		&ctx,
	)
	.await
	.expect_err("rename onto an existing file must fail");
	assert!(error.to_string().contains("destination already exists"), "{error}");
	assert_eq!(dir.read("note.txt"), "note\n");
	assert_eq!(dir.read("existing.txt"), "existing\n");
}

#[tokio::test]
async fn apply_patch_never_overwrites_an_existing_destination() {
	let dir = TempDir::new("apply-patch-trust");
	dir.write("existing.txt", "existing\n");
	dir.write("source.txt", "source\n");
	let ctx = ctx(&dir);

	let error = execute(
		EditMode::ApplyPatch,
		&json!({ "input": "*** Begin Patch\n*** Add File: existing.txt\n+overwrite\n*** End Patch" }),
		&ctx,
	)
	.await
	.expect_err("Add File must not overwrite");
	assert!(error.to_string().contains("already exists"), "{error}");
	assert_eq!(dir.read("existing.txt"), "existing\n");

	let error = execute(
		EditMode::ApplyPatch,
		&json!({
			"input": "*** Begin Patch\n*** Update File: source.txt\n*** Move to: existing.txt\n@@\n-source\n+moved\n*** End Patch",
		}),
		&ctx,
	)
	.await
	.expect_err("Move to an existing path must fail");
	assert!(error.to_string().contains("destination already exists"), "{error}");
	assert_eq!(dir.read("source.txt"), "source\n");
	assert_eq!(dir.read("existing.txt"), "existing\n");
}

#[tokio::test]
async fn replace_preserves_bom_crlf_and_multibyte_text() {
	let dir = TempDir::new("replace-encoding");
	let file = dir.write("win.txt", "\u{feff}α\r\nβ\r\nγ\r\n");
	let ctx = ctx(&dir);

	execute(
		EditMode::Replace,
		&json!({ "path": "win.txt", "old_string": "β", "new_string": "δ" }),
		&ctx,
	)
	.await
	.expect("replace multibyte");
	let bytes = fs::read(&file).expect("read");
	assert_eq!(
		String::from_utf8(bytes).expect("utf8"),
		"\u{feff}α\r\nδ\r\nγ\r\n",
		"BOM and CRLF endings survive the edit"
	);
}
