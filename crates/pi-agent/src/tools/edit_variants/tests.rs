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

#[tokio::test]
async fn sloppy_infers_gaps_between_non_adjacent_match_lines() {
	let dir = temp_dir("sloppy-non-consecutive");
	let file = dir.join("demo.txt");
	std::fs::write(
		&file,
		"fn main() {\n    let a = 1;\n    let middle = 0;\n    let b = 2;\n}\n",
	)
	.unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«\n    let a = 1;\n    let b = 2;\n»\n    let a = 10;\n    let b = 20;";
	super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap();
	assert_eq!(
		read(&file),
		"fn main() {\n    let a = 10;\n    let middle = 0;\n    let b = 20;\n}\n"
	);
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_trims_boundary_text_double_typed_on_selection_and_literal() {
	let dir = temp_dir("sloppy-overlap-trim");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "const v = crypto.get(name);\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«const v = crypto.⟪.get│set⟫(name);";
	super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap();
	assert_eq!(read(&file), "const v = crypto.set(name);\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_frames_whole_line_insertion() {
	let dir = temp_dir("sloppy-line-insert");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "fn main() {\n    let x = 1;\n}\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«fn main() {\n⟪│    let y = 2;⟫";
	super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap();
	assert_eq!(
		read(&file),
		"fn main() {\n    let y = 2;\n    let x = 1;\n}\n"
	);
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_expands_whole_line_deletion_over_separator_blank() {
	let dir = temp_dir("sloppy-full-line-delete");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "head\n\ntarget\n\ntail\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«head\n\n⟪target│⟫\n\ntail";
	super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap();
	assert_eq!(read(&file), "head\n\ntail\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_aligns_explicit_rewrite_that_echoes_surrounding_boundaries() {
	let dir = temp_dir("sloppy-boundary-echo");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "const x = 1;\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«\nconst x = ⟪1⟫;\n»\nconst x = 2";
	super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap();
	assert_eq!(read(&file), "const x = 2;\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_merges_overlapping_consistent_edits() {
	let dir = temp_dir("sloppy-overlap-merge");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "let a = 1;\nlet a = 2;\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«*let a\n»let b\n«let a = 2;\n»let b = 2;";
	super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap();
	assert_eq!(read(&file), "let b = 1;\nlet b = 2;\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_refuses_conflicting_overlapping_edits() {
	let dir = temp_dir("sloppy-overlap-conflict");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "let a = 1;\nlet a = 2;\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«*let a\n»let b\n«let a = 2;\n»let c = 2;";
	let err = super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap_err();
	let message = err.to_string();
	assert!(message.contains("overlapping original spans"), "{message}");
	assert_eq!(read(&file), "let a = 1;\nlet a = 2;\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_substitutes_multiple_bare_selections_positionally() {
	let dir = temp_dir("sloppy-positional");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "const a = 1 + 2;\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«const a = ⟪1⟫ + ⟪2⟫;\n»\n3\n4";
	super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap();
	assert_eq!(read(&file), "const a = 3 + 4;\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_rejects_multi_selection_rewrite_that_proves_neither_shape() {
	let dir = temp_dir("sloppy-positional-proof");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "const a = 1 + 2;\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«const a = ⟪1⟫ + ⟪2⟫;\n»\n9";
	let err = super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap_err();
	let message = err.to_string();
	assert!(
		message.contains("proves neither positional substitution nor whole-span replacement"),
		"{message}"
	);
	assert_eq!(read(&file), "const a = 1 + 2;\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_reemits_lone_deletion_reference_above_its_anchor() {
	let dir = temp_dir("sloppy-lone-reference");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "keep();\na();\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«\na();\n»\n«\nkeep();\n»\n»1";
	super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap();
	assert_eq!(read(&file), "a();\n\nkeep();\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_reuses_inline_deletion_reference_in_later_insertion() {
	let dir = temp_dir("sloppy-inline-reference");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "const moved = createMoved();\nbefore();\nafter();\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«\n⟪const moved = createMoved();│⟫\n«\nbefore();\n⟪│\n»1\n⟫\nafter();";
	super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap();
	assert_eq!(read(&file), "before();\nconst moved = createMoved();\nafter();\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_reemits_register_with_glued_markers() {
	let dir = temp_dir("sloppy-glued-reference");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "keep();\na();\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«a();\n»\n«keep();\n»\n»1";
	super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap();
	assert_eq!(read(&file), "a();\n\nkeep();\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_reemits_register_with_trailing_space() {
	let dir = temp_dir("sloppy-reference-trailing-space");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "keep();\na();\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«\na();\n»\n«\nkeep();\n»\n»1 ";
	super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap();
	assert_eq!(read(&file), "a();\n\nkeep();\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_rejects_ordinal_opener_instead_of_splitting_it() {
	let dir = temp_dir("sloppy-ordinal-opener");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "a();\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«1\na();";
	let err = super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap_err();
	assert!(err.to_string().contains("not a valid opener"), "{err}");
	assert_eq!(read(&file), "a();\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_collapses_duplicate_markerless_desired_text() {
	let dir = temp_dir("sloppy-desired-collapse");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "alpha();\nalpha();\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«alpha();";
	super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap();
	assert_eq!(read(&file), "alpha();\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_keeps_ambiguity_when_every_candidate_is_sibling_claimed() {
	let dir = temp_dir("sloppy-sibling-all-claimed");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "let a = 1;\nlet a = 2;\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«*let a\n»let z\n«let a\n»let b";
	let err = super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap_err();
	let message = err.to_string();
	assert!(message.contains("ambiguous"), "{message}");
	assert_eq!(read(&file), "let a = 1;\nlet a = 2;\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_excludes_sibling_claimed_span_from_ambiguous_match() {
	let dir = temp_dir("sloppy-sibling-exclusion");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "let value = 1;\nif value {\n    let value = 1;\n}\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«if value {\n    let value = 1;\n}\n»\nif value {\n    let value = 2;\n}\n«let value = 1;\n»\nlet value = 3;";
	super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap();
	assert_eq!(
		read(&file),
		"let value = 3;\nif value {\n    let value = 2;\n}\n"
	);
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_reemits_gap_capture_in_rewrite() {
	let dir = temp_dir("sloppy-gap-reemit");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "const a = 1;\nconst b = 2;\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«const … = 1;\n»let … = 3;";
	super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap();
	assert_eq!(read(&file), "let a = 3;\nconst b = 2;\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_rejects_selection_markers_in_rewrite() {
	let dir = temp_dir("sloppy-rewrite-marker");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "const x = 1;\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«const x = 1;\n»const x = ⟪2⟫;";
	let err = super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap_err();
	let message = err.to_string();
	assert!(message.contains("selection markers in REWRITE"), "{message}");
	assert_eq!(read(&file), "const x = 1;\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_rejects_unclaimed_whole_line_gap_in_rewrite() {
	let dir = temp_dir("sloppy-rewrite-gap");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "head\nconst x = 1;\ntail\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«const x = 1;\n»…";
	let err = super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap_err();
	let message = err.to_string();
	assert!(message.contains("whole-line … with no MATCH gap"), "{message}");
	assert_eq!(read(&file), "head\nconst x = 1;\ntail\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_escalates_identical_no_op_after_three_attempts() {
	let dir = temp_dir("sloppy-no-op-stop");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "const x = 1;\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«const x = 1;\n»const x = 1;";
	for attempt in 1..=3 {
		let err = super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
			.await
			.unwrap_err();
		let message = err.to_string();
		if attempt < 3 {
			assert!(message.contains("makes no change"), "{message}");
		} else {
			assert!(message.contains("STOP: identical no-op repeated 3 times"), "{message}");
		}
	}
	assert_eq!(read(&file), "const x = 1;\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_add_lines_preserve_selection_glyphs() {
	let dir = temp_dir("sloppy-add-literal");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "run();\ndone();\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«run();\n＋const sel = '⟪a│b⟫';";
	super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap();
	assert_eq!(read(&file), "run();\nconst sel = '⟪a│b⟫';\ndone();\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_keeps_mid_line_gap_literal_for_multiline_capture() {
	let dir = temp_dir("sloppy-gap-literal");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "function f() {\n  a();\n  b();\n}\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«function f() {\n…\n}\n»function f() {\n  return `${x}[… ]${y}`;\n}";
	super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap();
	assert_eq!(read(&file), "function f() {\n  return `${x}[… ]${y}`;\n}\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_deletes_whole_first_line_starting_with_non_ascii() {
	let dir = temp_dir("sloppy-utf8-first-line");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "// 🚀 header\nkeep();\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«// 🚀 header\n»";
	super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap();
	assert_eq!(read(&file), "keep();\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn sloppy_relocates_non_ascii_selection_line_without_panicking() {
	assert_eq!(
		super::sloppy::relocate_selection_lines("const label = \"日本語\";\n⟪日本語⟫"),
		"const label = \"⟪日本語⟫\";"
	);
}

#[tokio::test]
async fn sloppy_applies_diverging_rewrite_over_inline_sides() {
	let dir = temp_dir("sloppy-mixed-rewrite");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "const value = oldValue;\nconst other = keep;\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«const value = ⟪oldValue│newValue⟫;\n»\nconst value = newValue; // updated\nconst added = 1;";
	super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap();
	assert_eq!(read(&file), "const value = newValue; // updated\nconst added = 1;\nconst other = keep;\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_deletes_whole_match_for_trailing_empty_inline_selection() {
	let dir = temp_dir("sloppy-whole-match-delete");
	let file = dir.join("demo.txt");
	std::fs::write(
		&file,
		"  prepare();\n\n  const parseAnsi = () => {\n    return 1;\n  };\n\n  run();\n",
	)
	.unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«  const parseAnsi = () => {\n    return 1;\n  };\n⟪│⟫";
	super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap();
	assert_eq!(read(&file), "  prepare();\n\n  run();\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_deletes_every_match_for_star_empty_rewrite() {
	let dir = temp_dir("sloppy-star-delete");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "remove();\nkeep();\nremove();\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«*remove();\n»";
	super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap();
	assert_eq!(read(&file), "keep();\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_recovers_unique_non_bracket_punctuation_insertion() {
	let dir = temp_dir("sloppy-punctuation");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "runTask:alpha);\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«runTask alpha);\n»runTask(beta);";
	super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap();
	assert_eq!(read(&file), "runTask(beta);\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_refuses_bracket_insertion_punctuation_recovery() {
	let dir = temp_dir("sloppy-punctuation-bracket");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "runTask(alpha);\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«runTask alpha);\n»runTask(beta);";
	let result = super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx).await;
	assert!(result.is_err());
	assert_eq!(read(&file), "runTask(alpha);\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_recovers_multiline_gap_through_normalized_matching() {
	let dir = temp_dir("sloppy-normalized-gap");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "if (x) {\n    return 1;\n    return 2;\n}\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«if (x) {\n  return 1;\n…\n}\n»if (x) {\n    return 3;\n}";
	super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap();
	assert_eq!(read(&file), "if (x) {\n    return 3;\n}\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_maps_non_ascii_prefix_through_normalized_selection() {
	let dir = temp_dir("sloppy-nonascii-prefix");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "const icon = \"📁\";\n    let π = 3;\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«const icon = \"📁\";\nlet π = ⟪3│4⟫;";
	super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap();
	assert_eq!(read(&file), "const icon = \"📁\";\n    let π = 4;\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_recovers_multiline_gap_and_selection_normalized() {
	let dir = temp_dir("sloppy-normalized-gap-selection");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "if (x) {\n    return 1;\n    return 2;\n}\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«if (x) {\n  return ⟪1│3⟫;\n…\n}";
	super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap();
	assert_eq!(read(&file), "if (x) {\n    return 3;\n    return 2;\n}\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_recovers_multi_literal_fuzzy_with_gap() {
	let dir = temp_dir("sloppy-multi-fuzzy");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "const alpha = one;\nconst beta = two;\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«const alpha = one;\n…\nconst beta = tw0;\n»const alpha = one;\nconst beta = fixed;";
	super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap();
	assert_eq!(read(&file), "const alpha = one;\nconst beta = fixed;\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_refuses_overlapping_ambiguous_raw_match() {
	let dir = temp_dir("sloppy-overlapping-ambiguous");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "abcabcabc\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«\nabcabc\n»\nxyz";
	let result = super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx).await;
	assert!(result.is_err());
	assert_eq!(read(&file), "abcabcabc\n");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn sloppy_matches_unique_multibyte_literal_without_panicking() {
	let dir = temp_dir("sloppy-unique-multibyte");
	let file = dir.join("demo.txt");
	std::fs::write(&file, "éé!\n").unwrap();
	let ctx = context(&dir);
	let payload = "§demo.txt\n«\néé\n»\nok";
	super::execute(super::EditMode::Sloppy, &json!({ "input": payload }), &ctx)
		.await
		.unwrap();
	assert_eq!(read(&file), "ok!\n");
	std::fs::remove_dir_all(&dir).unwrap();
}
