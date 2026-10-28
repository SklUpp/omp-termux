//! Behavioural contract for the native hashline edit path.
//!
//! Fixture expectations were captured from the TypeScript implementation with
//! `bun tmp/hl-fixtures.ts` (same patch text, same post-edit text). Nothing here
//! asserts source text: every check drives a real parse/apply/write.

use std::{
	collections::BTreeSet,
	path::{Path, PathBuf},
	sync::{
		Arc,
		atomic::{AtomicUsize, Ordering},
	},
};

use pi_agent::{
	AgentContext, CancellationToken, EditTool, Tool, ToolResult,
	tools::hashline::{
		apply, block,
		clipboard::Clipboard,
		hash::{self, LineEnding},
		parser, snapshot,
	},
};
use serde_json::json;

const SAMPLE: &str = "line1\nline2\nline3\nline4\nline5\n";

struct TempDir {
	path: PathBuf,
}

impl TempDir {
	fn new(label: &str) -> Self {
		static NEXT: AtomicUsize = AtomicUsize::new(0);
		let unique = format!(
			"hashline-{label}-{}-{}",
			std::process::id(),
			NEXT.fetch_add(1, Ordering::SeqCst)
		);
		let path = std::env::temp_dir().join(unique);
		std::fs::create_dir_all(&path).expect("create temp dir");
		Self { path }
	}

	fn write(&self, name: &str, content: &str) -> PathBuf {
		let path = self.path.join(name);
		if let Some(parent) = path.parent() {
			std::fs::create_dir_all(parent).expect("create parent");
		}
		std::fs::write(&path, content).expect("write fixture");
		path
	}

	fn read(&self, name: &str) -> String {
		std::fs::read_to_string(self.path.join(name)).expect("read fixture")
	}
}

impl Drop for TempDir {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.path);
	}
}

fn normalized(content: &str) -> String {
	let (_, text) = hash::strip_bom(content);
	hash::normalize_to_lf(text)
}

/// Record a file the way `read` does: full normalized text + displayed lines,
/// into the session store the edit tool will validate against.
fn record(store: &snapshot::SnapshotStore, path: &Path, seen: &[u32]) -> String {
	let content = std::fs::read_to_string(path).expect("read for record");
	let text = normalized(&content);
	let key = snapshot::canonical_key(path);
	let tag = store.record(&key, &text, None).expect("record snapshot");
	store.record_seen_lines(&key, &tag, seen);
	tag
}

fn tag_of(path: &Path) -> String {
	let content = std::fs::read_to_string(path).expect("read for tag");
	hash::compute_file_hash(&normalized(&content))
}

fn apply_diff(text: &str, diff: &str, path: &str) -> Result<apply::ApplyResult, String> {
	let parsed = parser::parse_section_body(diff)?;
	let mut warnings = parsed.warnings.clone();
	let resolved = block::resolve_block_edits(parsed.edits, text, path, &mut warnings)?;
	let mut clipboard = Clipboard::default();
	let mut result = apply::apply_edits(text, &resolved, &mut clipboard)?;
	warnings.extend(result.warnings);
	result.warnings = warnings;
	Ok(result)
}

fn apply_text(text: &str, diff: &str, path: &str) -> String {
	apply_diff(text, diff, path).expect("apply").text
}

async fn run_edit(cwd: &Path, store: &Arc<snapshot::SnapshotStore>, input: &str) -> ToolResult {
	run_edit_with_options(cwd, store, input, Default::default()).await
}

async fn run_edit_with_options(
	cwd: &Path,
	store: &Arc<snapshot::SnapshotStore>,
	input: &str,
	options: pi_agent::tools::hashline::edit::EditToolOptions,
) -> ToolResult {
	let ctx = AgentContext::new(cwd.to_path_buf());
	EditTool::with_options(store.clone(), options)
		.execute(&json!({ "input": input }), &ctx)
		.await
		.expect("edit tool")
}

fn assert_failed(result: &ToolResult, needle: &str) {
	assert!(result.is_error, "expected failure, got: {}", result.content);
	assert!(result.content.contains(needle), "missing {needle:?} in: {}", result.content);
}

// ── Tags ────────────────────────────────────────────────────────────────────

#[test]
fn tags_match_upstream_xxh32_fixtures() {
	assert_eq!(hash::compute_file_hash(""), "5D05");
	assert_eq!(hash::compute_file_hash("a"), "7456");
	assert_eq!(hash::compute_file_hash("a\n"), "9585");
	assert_eq!(hash::compute_file_hash("a\nb\n"), "9A46");
	assert_eq!(hash::compute_file_hash("a \nb\t\n"), "9A46");
	assert_eq!(hash::compute_file_hash("a\r\nb\r\n"), "9A46");
	assert_eq!(hash::compute_file_hash("\u{feff}a\nb\n"), "AC6B");
	assert_eq!(hash::compute_file_hash("  \n\n"), "AF96");
}

#[test]
fn line_endings_round_trip_and_bom_is_preserved_separately() {
	assert_eq!(hash::detect_line_ending("a\r\nb\n"), LineEnding::Crlf);
	assert_eq!(hash::detect_line_ending("a\nb\r\n"), LineEnding::Lf);
	assert_eq!(hash::normalize_to_lf("a\r\nb\rc\n"), "a\nb\nc\n");
	assert_eq!(hash::restore_line_endings("a\nb\n", LineEnding::Crlf), "a\r\nb\r\n");
	assert_eq!(hash::strip_bom("\u{feff}a"), ("\u{feff}", "a"));
}

// ── Parser + apply fixtures ─────────────────────────────────────────────────

#[test]
fn range_and_gap_inserts_match_upstream_text() {
	assert_eq!(apply_text(SAMPLE, "PUT 2.=3:\n+X\n+Y\n", "x.rs"), "line1\nX\nY\nline4\nline5\n");
	assert_eq!(
		apply_text(SAMPLE, "PUT <3:\n+X\n", "x.rs"),
		"line1\nline2\nX\nline3\nline4\nline5\n"
	);
	assert_eq!(
		apply_text(SAMPLE, "PUT >3:\n+X\n", "x.rs"),
		"line1\nline2\nline3\nX\nline4\nline5\n"
	);
	assert_eq!(
		apply_text(SAMPLE, "PUT <1:\n+H\n", "x.rs"),
		"H\nline1\nline2\nline3\nline4\nline5\n"
	);
	assert_eq!(
		apply_text(SAMPLE, "PUT >$:\n+T\n", "x.rs"),
		"line1\nline2\nline3\nline4\nline5\nT\n"
	);
	assert_eq!(apply_text("", "PUT <1:\n+H\n", "x.rs"), "H");
	assert_eq!(apply_text("", "PUT >$:\n+T\n", "x.rs"), "T");
	assert_eq!(apply_text("a\nb", "PUT 2.=2:\n+X\n", "x.rs"), "a\nX");
	assert_eq!(apply_text("a\nb", "PUT >$:\n+X\n", "x.rs"), "a\nb\nX");
	assert_eq!(apply_text("a\nb\n", "PUT >$:\n+X\n", "x.rs"), "a\nb\nX\n");
	assert_eq!(apply_text("a\nb\n", "PUT <1:\n+X\n", "x.rs"), "X\na\nb\n");
}

#[test]
fn first_changed_line_tracks_the_earliest_hunk() {
	let result = apply_diff(SAMPLE, "PUT 4.=4:\n+D\nPUT <2:\n+B\nPUT >5:\n+E\n", "x.rs").expect("apply");
	assert_eq!(result.text, "line1\nB\nline2\nline3\nD\nline5\nE\n");
	assert_eq!(result.first_changed_line, Some(2));
	assert_eq!(result.warnings, Vec::<String>::new());
}

#[test]
fn cut_and_register_pastes_follow_authored_order() {
	let mut clipboard = Clipboard::default();
	let parsed = parser::parse_section_body("CUT 2.=3 @r\nPUT >$ @r\n").expect("parse");
	let result = apply::apply_edits(SAMPLE, &parsed.edits, &mut clipboard).expect("apply");
	assert_eq!(result.text, "line1\nline4\nline5\nline2\nline3\n");
	assert_eq!(clipboard.named["r"], vec!["line2".to_string(), "line3".to_string()]);

	let mut clipboard = Clipboard::default();
	let parsed = parser::parse_section_body("CUT 2.=2\nPUT >5\n").expect("parse");
	let result = apply::apply_edits(SAMPLE, &parsed.edits, &mut clipboard).expect("apply");
	assert_eq!(result.text, "line1\nline3\nline4\nline5\nline2\n");
}

#[test]
fn span_paste_replaces_the_target_range() {
	let mut clipboard = Clipboard::default();
	let parsed = parser::parse_section_body("CUT 2.=2 @r\nPUT 4.=5 @r\n").expect("parse");
	let result = apply::apply_edits(SAMPLE, &parsed.edits, &mut clipboard).expect("apply");
	assert_eq!(result.text, "line1\nline3\nline2\n");
}

#[test]
fn block_operations_resolve_through_pi_ast() {
	let file = "fn outer() {\n    one();\n}\ntail();\n";
	assert_eq!(
		apply_text(file, "PUT 1*:\n+fn outer() {\n+    two();\n+}\n", "x.rs"),
		"fn outer() {\n    two();\n}\ntail();\n"
	);
	assert_eq!(apply_text(file, "CUT 1*\n", "x.rs"), "tail();\n");
	assert_eq!(
		apply_text(file, "PUT >1*:\n+    extra();\n", "x.rs"),
		"fn outer() {\n    one();\n}\n    extra();\ntail();\n"
	);
	let err = apply_diff(file, "CUT 2*\n", "x.rs").expect_err("single-line block rejected");
	assert!(err.contains("single-line block"), "got: {err}");
}

#[test]
fn overlapping_hunks_are_rejected() {
	let err = apply_diff(SAMPLE, "PUT 2.=3:\n+X\nCUT 3.=4\n", "x.rs").expect_err("overlap");
	assert!(err.contains("already targeted"), "got: {err}");
}

#[test]
fn empty_body_and_warnings_match_upstream() {
	let result = apply_diff(SAMPLE, "PUT 2.=3:\n", "x.rs").expect("apply");
	assert_eq!(result.text, "line1\nline4\nline5\n");
	assert!(result.warnings.iter().any(|w| w.contains("Interpreted an empty `PUT` body as deletion")));

	let result = apply_diff(SAMPLE, "CUT 2.=2:\n", "x.rs").expect("apply");
	assert_eq!(result.text, "line1\nline3\nline4\nline5\n");
	assert!(result.warnings.iter().any(|w| w.contains("Ignored a trailing `:`")));

	let result = apply_diff(SAMPLE, "PUT 2.=2:\n+X\n\n+Y\n", "x.rs").expect("apply");
	assert_eq!(result.text, "line1\nX\n\nY\nline3\nline4\nline5\n");
	assert!(result.warnings.iter().any(|w| w.contains("Auto-prefixed bare body row(s)")));
}

#[test]
fn literal_plus_and_bodyless_errors_are_explicit() {
	assert_eq!(
		apply_text(SAMPLE, "PUT 2.=2:\n+++weird\n", "x.rs"),
		"line1\n++weird\nline3\nline4\nline5\n"
	);

	let err = apply_diff(SAMPLE, "PUT 2.=3\n+X\n", "x.rs").expect_err("colonless body");
	assert!(err.contains("clipboard-backed"), "got: {err}");

	let err = apply_diff(SAMPLE, "PUT 2.=3\n", "x.rs").expect_err("colonless span");
	assert!(err.contains("span targets need a named register"), "got: {err}");

	let err = apply_diff(SAMPLE, "PUT >3\n", "x.rs").expect_err("empty anonymous paste");
	assert!(err.contains("Nothing to paste"), "got: {err}");

	let err = apply_diff(SAMPLE, "CUT 1*= @r\n", "x.rs").expect_err("bad header");
	assert!(err.contains("no preceding hunk header"), "got: {err}");

	let err = apply_diff(SAMPLE, "PUT 9.=9:\n+X\n", "x.rs").expect_err("out of range");
	assert!(err.contains("does not exist"), "got: {err}");
}

#[test]
fn minus_rows_follow_the_upstream_policy() {
	// Bare `-` rows are auto-prefixed and survive as literal content.
	let result = apply_diff(SAMPLE, "PUT 2.=3:\n- a\n- b\n", "x.md").expect("bullets");
	assert_eq!(result.text, "line1\n- a\n- b\nline4\nline5\n");
	assert!(!result.warnings.is_empty(), "bare `-` rows must surface the auto-prefix warning");

	// Unified-diff old rows paired with `+new` rows are dropped.
	let result = apply_diff(SAMPLE, "PUT 2.=3:\n-old1\n-old2\n+new1\n", "x.rs").expect("diff rows");
	assert_eq!(result.text, "line1\nnew1\nline4\nline5\n");
	assert!(result.warnings.iter().any(|w| w.contains("Ignored unified-diff `-old` row(s)")));

	// Anything else is rejected with the canonical fix.
	let err = apply_diff(SAMPLE, "PUT 2.=3:\n-old1\n", "x.rs").expect_err("minus-only");
	assert!(err.contains("`-` rows are not valid"), "got: {err}");
}

#[test]
fn rem_and_mv_are_file_ops() {
	let parsed = parser::parse_section_body("REM\n").expect("rem");
	assert_eq!(parsed.file_op, Some(parser::FileOp::Rem));

	let parsed = parser::parse_section_body("MV \"new dir/file.txt\"\n").expect("mv");
	assert_eq!(parsed.file_op, Some(parser::FileOp::Move("new dir/file.txt".to_string())));

	let parsed = parser::parse_section_body("MV \"a\\\"b.txt\"\n").expect("mv escaped");
	assert_eq!(parsed.file_op, Some(parser::FileOp::Move("a\\\"b.txt".to_string())));
}

// ── Snapshot store ──────────────────────────────────────────────────────────

#[test]
fn snapshot_store_dedups_versions_and_resolves_tags() {
	let store = snapshot::SnapshotStore::new();
	let path = canonical("/snapshot/store/basic.txt");
	let first = store.record(&path, "one\n", Some(&[1])).expect("record");
	assert_eq!(first, hash::compute_file_hash("one\n"));
	// Same content again fuses onto one version and widens provenance.
	assert_eq!(store.record(&path, "one\n", Some(&[2])).expect("record"), first);
	let head = store.head(&path).expect("head");
	assert_eq!(head.seen_lines, Some(BTreeSet::from([1u32, 2])));
	assert_eq!(store.by_hash(&path, &first).expect("by hash").text, "one\n");
	assert_eq!(store.by_content(&path, "one\n").expect("by content").text, "one\n");

	for index in 0..6 {
		store.record(&path, &format!("version {index}\n"), None).expect("record");
	}
	assert_eq!(store.head(&path).expect("head").text, "version 5\n");
	assert!(store.by_hash(&path, &first).is_none(), "oldest version evicted");

	store.relocate(&path, &canonical("/snapshot/store/moved.txt"));
	assert!(store.head(&path).is_none());
	assert!(store.head(&canonical("/snapshot/store/moved.txt")).is_some());
	store.invalidate(&canonical("/snapshot/store/moved.txt"));
	assert!(store.head(&canonical("/snapshot/store/moved.txt")).is_none());
}

#[test]
fn snapshot_store_refuses_files_above_the_cap() {
	let store = snapshot::SnapshotStore::new();
	let path = canonical("/snapshot/store/large.txt");
	assert!(
		store
			.record(&path, &"a".repeat(snapshot::SNAPSHOT_MAX_BYTES + 1), None)
			.is_none()
	);
	assert!(store.head(&path).is_none());
}

fn canonical(path: &str) -> String {
	let dir = std::env::temp_dir().to_string_lossy().into_owned();
	format!("{dir}{path}")
}

// ── edit tool: tags, seen lines, transactions ───────────────────────────────

#[tokio::test]
async fn edit_applies_a_tagged_range_and_returns_the_new_header() {
	let dir = TempDir::new("apply");
	let store = Arc::new(snapshot::SnapshotStore::new());
	let path = dir.write("note.txt", "alpha\nbeta\ngamma\n");
	let tag = record(&store, &path, &[1, 2, 3]);

	let input = format!("[note.txt#{tag}]\nPUT 2.=2:\n+BETA\n");
	let result = run_edit(&dir.path, &store, &input).await;
	assert!(!result.is_error, "{}", result.content);
	assert!(result.content.contains("Updated note.txt (first changed line 2)"), "{}", result.content);
	assert_eq!(dir.read("note.txt"), "alpha\nBETA\ngamma\n");
	let fresh = tag_of(&path);
	assert!(result.content.contains(&format!("[note.txt#{fresh}]")), "{}", result.content);
}

#[tokio::test]
async fn stale_tag_is_rejected_without_touching_the_file() {
	let dir = TempDir::new("stale");
	let store = Arc::new(snapshot::SnapshotStore::new());
	let path = dir.write("note.txt", "alpha\nbeta\n");
	let stale = record(&store, &path, &[1, 2]);
	dir.write("note.txt", "alpha\nBETA\n");

	let input = format!("[note.txt#{stale}]\nPUT 2.=2:\n+beta\n");
	let result = run_edit(&dir.path, &store, &input).await;
	let fresh = hash::compute_file_hash("alpha\nBETA\n");
	assert!(result.is_error, "{}", result.content);
	assert!(result.content.contains(&format!("#{fresh}")), "{}", result.content);
	assert_eq!(dir.read("note.txt"), "alpha\nBETA\n");
}

#[tokio::test]
async fn unseen_anchor_is_rejected_then_a_retry_succeeds() {
	let dir = TempDir::new("unseen");
	let store = Arc::new(snapshot::SnapshotStore::new());
	let path = dir.write("note.txt", "alpha\nbeta\ngamma\n");
	let tag = record(&store, &path, &[1, 2]);

	let input = format!("[note.txt#{tag}]\nPUT 3.=3:\n+GAMMA\n");
	let enforced = || pi_agent::tools::hashline::edit::EditToolOptions { enforce_seen_lines: true };
	let rejected = run_edit_with_options(&dir.path, &store, &input, enforced()).await;
	assert_failed(&rejected, "never displayed");
	assert!(rejected.content.contains("3:gamma"), "{}", rejected.content);
	assert_eq!(dir.read("note.txt"), "alpha\nbeta\ngamma\n");

	let retried = run_edit_with_options(&dir.path, &store, &input, enforced()).await;
	assert!(!retried.is_error, "{}", retried.content);
	assert_eq!(dir.read("note.txt"), "alpha\nbeta\nGAMMA\n");
}

#[tokio::test]
async fn missing_tag_and_noop_are_reported_without_writes() {
	let dir = TempDir::new("missing-tag");
	let store = Arc::new(snapshot::SnapshotStore::new());
	let path = dir.write("note.txt", "alpha\n");
	record(&store, &path, &[1]);

	let missing = run_edit(&dir.path, &store, "[note.txt]\nPUT 1.=1:\n+beta\n").await;
	assert_failed(&missing, "Missing hashline snapshot tag");

	let tag = record(&store, &path, &[1]);
	let noop = run_edit(&dir.path, &store, &format!("[note.txt#{tag}]\nPUT 1.=1:\n+alpha\n")).await;
	assert!(noop.is_error, "{}", noop.content);
	assert!(noop.content.contains("produced no change"), "{}", noop.content);
	assert_eq!(dir.read("note.txt"), "alpha\n");
}

#[tokio::test]
async fn stale_tag_recovers_shifted_anchors() {
	let dir = TempDir::new("recover-shift");
	let store = Arc::new(snapshot::SnapshotStore::new());
	let path = dir.write("note.txt", "alpha\nbeta\ngamma\n");
	let tag = record(&store, &path, &[1, 2, 3]);
	// External drift prepends one line: every anchor shifts by +1 uniformly.
	dir.write("note.txt", "header\nalpha\nbeta\ngamma\n");

	let input = format!("[note.txt#{tag}]\nPUT 2.=2:\n+BETA\n");
	let result = run_edit(&dir.path, &store, &input).await;
	assert!(!result.is_error, "{}", result.content);
	assert!(
		result.content.contains("Recovered by remapping stale line anchors"),
		"{}",
		result.content
	);
	assert!(result.content.contains("first changed line 3"), "{}", result.content);
	assert_eq!(dir.read("note.txt"), "header\nalpha\nBETA\ngamma\n");
}

#[tokio::test]
async fn unrecoverable_drift_reveals_current_tag_and_context() {
	let dir = TempDir::new("mismatch-reveal");
	let store = Arc::new(snapshot::SnapshotStore::new());
	let path = dir.write("note.txt", "alpha\nbeta\ngamma\n");
	let tag = record(&store, &path, &[1, 2, 3]);
	// The anchored line itself changed: no safe remap exists.
	dir.write("note.txt", "alpha\nBETA\ngamma\n");

	let input = format!("[note.txt#{tag}]\nPUT 2.=2:\n+beta\n");
	let result = run_edit(&dir.path, &store, &input).await;
	assert_failed(&result, "Edit rejected for note.txt: file changed between read and edit.");
	let current = tag_of(&path);
	assert!(result.content.contains(&format!("#{current}")), "{}", result.content);
	assert!(result.content.contains("*2:BETA"), "{}", result.content);
	assert!(result.content.contains("1:alpha"), "{}", result.content);
	assert_eq!(dir.read("note.txt"), "alpha\nBETA\ngamma\n");
}

#[tokio::test]
async fn syntax_breaking_edit_warns_but_still_lands() {
	let dir = TempDir::new("syntax");
	let store = Arc::new(snapshot::SnapshotStore::new());
	let path = dir.write("main.rs", "fn main() {\n    let x = 1;\n}\n");
	let tag = record(&store, &path, &[1, 2, 3]);

	let input = format!("[main.rs#{tag}]\nPUT 2.=2:\n+    let x = ;\n");
	let result = run_edit(&dir.path, &store, &input).await;
	assert!(!result.is_error, "{}", result.content);
	assert!(
		result.content.contains("This edit introduced a syntax error near line 2"),
		"{}",
		result.content
	);
	assert_eq!(dir.read("main.rs"), "fn main() {\n    let x = ;\n}\n");
}

#[tokio::test]
async fn seen_line_enforcement_can_be_disabled() {
	use pi_agent::tools::hashline::edit::EditToolOptions;

	let dir = TempDir::new("seen-config");
	let store = Arc::new(snapshot::SnapshotStore::new());
	let path = dir.write("note.txt", "alpha\nbeta\ngamma\n");
	let tag = record(&store, &path, &[1, 2]);
	let ctx = AgentContext::new(dir.path.clone());
	let input = format!("[note.txt#{tag}]\nPUT 3.=3:\n+GAMMA\n");

	let strict = EditTool::with_options(store.clone(), EditToolOptions { enforce_seen_lines: true });
	let rejected = strict.execute(&json!({ "input": input }), &ctx).await.expect("strict");
	assert!(rejected.is_error, "{}", rejected.content);
	assert!(rejected.content.contains("never displayed"), "{}", rejected.content);
	assert_eq!(dir.read("note.txt"), "alpha\nbeta\ngamma\n");

	let lax = EditTool::with_options(store.clone(), EditToolOptions { enforce_seen_lines: false });
	let accepted = lax.execute(&json!({ "input": input }), &ctx).await.expect("lax");
	assert!(!accepted.is_error, "{}", accepted.content);
	assert_eq!(dir.read("note.txt"), "alpha\nbeta\nGAMMA\n");
}

#[tokio::test]
async fn default_options_skip_seen_line_enforcement() {
	// Upstream `edit.enforceSeenLines` schema default is false; an unseen anchor
	// edit therefore applies unless the guard is switched on.
	let dir = TempDir::new("seen-default");
	let store = Arc::new(snapshot::SnapshotStore::new());
	let path = dir.write("note.txt", "alpha\nbeta\ngamma\n");
	let tag = record(&store, &path, &[1, 2]);

	let input = format!("[note.txt#{tag}]\nPUT 3.=3:\n+GAMMA\n");
	let result = run_edit(&dir.path, &store, &input).await;
	assert!(!result.is_error, "{}", result.content);
	assert_eq!(dir.read("note.txt"), "alpha\nbeta\nGAMMA\n");
}

#[tokio::test]
async fn repeated_identical_noops_escalate_to_a_hard_stop() {
	let dir = TempDir::new("noop-guard");
	let store = Arc::new(snapshot::SnapshotStore::new());
	let path = dir.write("note.txt", "alpha\n");
	let tag = record(&store, &path, &[1]);
	let ctx = AgentContext::new(dir.path.clone());
	let tool = EditTool::new(store.clone());
	let input = format!("[note.txt#{tag}]\nPUT 1.=1:\n+alpha\n");

	for attempt in 1..=3 {
		let result = tool.execute(&json!({ "input": input }), &ctx).await.expect("noop");
		assert!(result.is_error, "attempt {attempt}: {}", result.content);
		if attempt < 3 {
			assert!(result.content.contains("produced no change"), "{}", result.content);
		} else {
			assert!(
				result.content.contains("byte-identical no-op 3 times in a row"),
				"{}",
				result.content
			);
			assert!(result.content.starts_with("STOP."), "{}", result.content);
		}
	}
	assert_eq!(dir.read("note.txt"), "alpha\n");
}

#[tokio::test]
async fn multi_file_preflight_prevents_partial_writes() {
	let dir = TempDir::new("preflight");
	let store = Arc::new(snapshot::SnapshotStore::new());
	let first = dir.write("a.txt", "one\ntwo\n");
	let second = dir.write("b.txt", "uno\ndue\n");
	let first_tag = record(&store, &first, &[1, 2]);
	let stale_second = record(&store, &second, &[1, 2]);
	dir.write("b.txt", "uno\nDUE\n");

	let input = format!(
		"[a.txt#{first_tag}]\nPUT 2.=2:\n+TWO\n[b.txt#{stale_second}]\nPUT 2.=2:\n+due\n"
	);
	let result = run_edit(&dir.path, &store, &input).await;
	assert!(result.is_error);
	assert_eq!(dir.read("a.txt"), "one\ntwo\n", "validated prefix must not land");
	assert_eq!(dir.read("b.txt"), "uno\nDUE\n");
}

#[tokio::test]
async fn multi_file_batch_moves_content_between_files() {
	let dir = TempDir::new("cross-file");
	let store = Arc::new(snapshot::SnapshotStore::new());
	let source = dir.write("source.txt", "keep\nmove me\n");
	let dest = dir.write("dest.txt", "head\n");
	let source_tag = record(&store, &source, &[1, 2]);
	let dest_tag = record(&store, &dest, &[1]);

	let input = format!(
		"[source.txt#{source_tag}]\nCUT 2.=2 @payload\n[dest.txt#{dest_tag}]\nPUT >$ @payload\n"
	);
	let result = run_edit(&dir.path, &store, &input).await;
	assert!(!result.is_error, "{}", result.content);
	assert_eq!(dir.read("source.txt"), "keep\n");
	assert_eq!(dir.read("dest.txt"), "head\nmove me\n");
}

#[tokio::test]
async fn bom_crlf_and_mode_survive_the_write() {
	let dir = TempDir::new("bytes");
	let store = Arc::new(snapshot::SnapshotStore::new());
	let path = dir.path.join("bytes.txt");
	std::fs::write(&path, b"\xEF\xBB\xBFone\r\ntwo\r\nthree\r\n").expect("write bom fixture");
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).expect("chmod");
	}
	let tag = record(&store, &path, &[1, 2, 3]);

	let input = format!("[bytes.txt#{tag}]\nPUT 2.=2:\n+TWO\n");
	let result = run_edit(&dir.path, &store, &input).await;
	assert!(!result.is_error, "{}", result.content);
	let bytes = std::fs::read(&path).expect("read bytes");
	assert_eq!(bytes, b"\xEF\xBB\xBFone\r\nTWO\r\nthree\r\n");
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
		assert_eq!(mode, 0o640);
	}
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_target_is_edited_through_the_link() {
	let dir = TempDir::new("symlink");
	let store = Arc::new(snapshot::SnapshotStore::new());
	let real = dir.write("real/target.txt", "alpha\nbeta\n");
	let link = dir.path.join("link.txt");
	std::os::unix::fs::symlink(&real, &link).expect("symlink");
	let tag = record(&store, &real, &[1, 2]);

	let input = format!("[link.txt#{tag}]\nPUT 2.=2:\n+BETA\n");
	let result = run_edit(&dir.path, &store, &input).await;
	assert!(!result.is_error, "{}", result.content);
	assert!(std::fs::symlink_metadata(&link).expect("lstat").file_type().is_symlink());
	assert_eq!(std::fs::read_to_string(&real).expect("target"), "alpha\nBETA\n");
}

#[tokio::test]
async fn rem_deletes_and_mv_moves_with_the_original_bytes() {
	let dir = TempDir::new("file-ops");
	let store = Arc::new(snapshot::SnapshotStore::new());
	let path = dir.write("gone.txt", "alpha\n");
	let tag = record(&store, &path, &[1]);
	let result = run_edit(&dir.path, &store, &format!("[gone.txt#{tag}]\nREM\n")).await;
	assert!(!result.is_error, "{}", result.content);
	assert!(result.content.contains("Deleted gone.txt"));
	assert!(!path.exists());

	let path = dir.write("old.txt", "\u{feff}alpha\r\nbeta\r\n");
	let tag = record(&store, &path, &[1, 2]);
	let input = format!("[old.txt#{tag}]\nPUT 2.=2:\n+BETA\nMV \"sub/new.txt\"\n");
	let result = run_edit(&dir.path, &store, &input).await;
	assert!(!result.is_error, "{}", result.content);
	assert!(result.content.contains("Moved old.txt -> sub/new.txt"));
	assert!(!path.exists());
	let moved = std::fs::read(dir.path.join("sub/new.txt")).expect("moved bytes");
	assert_eq!(moved, b"\xEF\xBB\xBFalpha\r\nBETA\r\n");
}

#[tokio::test]
async fn clipboard_paste_amplification_is_bounded_before_cloning() {
	let dir = TempDir::new("paste-amplification");
	let store = Arc::new(snapshot::SnapshotStore::new());
	// One ~3 MiB source line captured into @r, then pasted 1000 times into a
	// tiny destination: the budget must stop it before cloning the register.
	let big = "x".repeat(3 * 1024 * 1024);
	let source = dir.write("source.txt", &format!("{big}\n"));
	let dest = dir.write("dest.txt", "keep\n");
	let tag_source = record(&store, &source, &[1]);
	let tag_dest = record(&store, &dest, &[1]);

	let mut pastes = String::new();
	for _ in 0..1000 {
		pastes.push_str("PUT >$ @r\n");
	}
	let input = format!(
		"[source.txt#{tag_source}]\nCUT 1.=1 @r\n[dest.txt#{tag_dest}]\n{pastes}"
	);
	let result = run_edit(&dir.path, &store, &input).await;
	assert!(result.is_error, "{}", result.content);
	assert!(result.content.contains("MiB"), "{}", result.content);
	assert_eq!(dir.read("dest.txt"), "keep\n");
	assert_eq!(dir.read("source.txt").len(), big.len() + 1);
}

#[tokio::test]
async fn blank_line_captures_charge_vector_slots_before_cloning() {
	let dir = TempDir::new("blank-line-slots");
	let store = Arc::new(snapshot::SnapshotStore::new());
	// 99,999 empty lines: ~100 KiB of text, but 99,999 `String` slots per
	// register copy (~2.4 MiB) that a byte-only accounting would miss.
	let source = dir.write("source.txt", &"\n".repeat(99_999));
	let dest = dir.write("dest.txt", "keep\n");
	let tag_source = record(&store, &source, &[1]);
	let tag_dest = record(&store, &dest, &[1]);

	let mut pastes = String::new();
	for _ in 0..10 {
		pastes.push_str("PUT >$ @r\n");
	}
	let input = format!(
		"[source.txt#{tag_source}]\nCUT 1.=99999 @r\n[dest.txt#{tag_dest}]\n{pastes}"
	);
	let result = run_edit(&dir.path, &store, &input).await;
	assert!(result.is_error, "{}", result.content);
	// Blank-line slot accounting is cumulative, so the call stops on the
	// aggregate edit/byte budget long before ten 99,999-line expansions exist.
	assert!(
		result.content.contains("more than"),
		"{}",
		result.content
	);
	assert_eq!(dir.read("dest.txt"), "keep\n");
	assert_eq!(dir.read("source.txt").len(), 99_999);
}

#[tokio::test]
async fn batch_expansion_is_rejected_before_materializing() {
	let dir = TempDir::new("expansion-cap");
	let store = Arc::new(snapshot::SnapshotStore::new());
	let path = dir.write("note.txt", "alpha\nbeta\n");
	let tag = record(&store, &path, &[1, 2]);

	// 2 * 60k declared lines: rejected while parsing the second hunk, long
	// before 120k deletes/inserts could be materialized.
	let input = format!(
		"[note.txt#{tag}]\nPUT 1.=60000:\n+x\nPUT 1.=60000:\n+x\n"
	);
	let result = run_edit(&dir.path, &store, &input).await;
	assert_failed(&result, "expands to more than 100000 lines");
	assert_eq!(dir.read("note.txt"), "alpha\nbeta\n");
}

#[tokio::test]
async fn move_destination_collisions_are_rejected_before_any_write() {
	let dir = TempDir::new("mv-collision");
	let store = Arc::new(snapshot::SnapshotStore::new());
	let a = dir.write("a.txt", "A\n");
	let b = dir.write("b.txt", "B\n");
	let tag_a = record(&store, &a, &[1]);
	let tag_b = record(&store, &b, &[1]);

	let input = format!(
		"[a.txt#{tag_a}]\nMV b.txt\n[b.txt#{tag_b}]\nPUT 1.=1:\n+B2\n"
	);
	let result = run_edit(&dir.path, &store, &input).await;
	assert_failed(&result, "Split the move into its own edit call");
	assert_eq!(dir.read("a.txt"), "A\n");
	assert_eq!(dir.read("b.txt"), "B\n");
}

#[tokio::test]
async fn later_commit_failure_reports_landed_sections() {
	let dir = TempDir::new("landed-prefix");
	let store = Arc::new(snapshot::SnapshotStore::new());
	let a = dir.write("a.txt", "A\n");
	let b = dir.write("b.txt", "B\n");
	let tag_a = record(&store, &a, &[1]);
	let tag_b = record(&store, &b, &[1]);
	std::fs::create_dir(dir.path.join("d")).unwrap();

	// The second section's move destination is a directory: prepare accepts it,
	// the write fails, and the first section must still be reported as written.
	let input = format!(
		"[a.txt#{tag_a}]\nPUT 1.=1:\n+A2\n[b.txt#{tag_b}]\nMV d\n"
	);
	let result = run_edit(&dir.path, &store, &input).await;
	assert!(result.is_error, "{}", result.content);
	assert!(result.content.contains("Already written by this call"), "{}", result.content);
	assert!(result.content.contains("Updated a.txt"), "{}", result.content);
	assert!(
		result.content.contains("did not complete; any partial mutation the message above reports"),
		"{}",
		result.content
	);
	assert_eq!(dir.read("a.txt"), "A2\n");
	assert_eq!(dir.read("b.txt"), "B\n");
}

#[cfg(unix)]
#[tokio::test]
async fn move_preserves_source_permissions() {
	use std::os::unix::fs::{MetadataExt, PermissionsExt};

	let dir = TempDir::new("mv-mode");
	let store = Arc::new(snapshot::SnapshotStore::new());
	let source = dir.write("private.txt", "secret\n");
	std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600)).unwrap();
	let tag = record(&store, &source, &[1]);

	let input = format!("[private.txt#{tag}]\nMV moved.txt\n");
	let result = run_edit(&dir.path, &store, &input).await;
	assert!(!result.is_error, "{}", result.content);
	assert!(!source.exists(), "source removed after a complete move");
	let moved = dir.path.join("moved.txt");
	assert_eq!(std::fs::metadata(&moved).unwrap().mode() & 0o777, 0o600);
	assert_eq!(dir.read("moved.txt"), "secret\n");
}

#[tokio::test]
async fn named_registers_do_not_leak_across_tool_instances() {
	let dir = TempDir::new("register-isolation");
	let store = Arc::new(snapshot::SnapshotStore::new());
	let path = dir.write("shared.txt", "keep\ntake\n");
	let tag = record(&store, &path, &[1, 2]);
	let ctx = AgentContext::new(dir.path.clone());

	// Session A captures a register.
	let session_a = EditTool::new(store.clone());
	let cut = session_a
		.execute(&json!({ "input": format!("[shared.txt#{tag}]\nCUT 2.=2 @saved\n") }), &ctx)
		.await
		.expect("cut");
	assert!(!cut.is_error, "{}", cut.content);
	assert_eq!(dir.read("shared.txt"), "keep\n");

	// Session B shares the snapshot store but must not see A's registers.
	let tag = record(&store, &path, &[1]);
	let session_b = EditTool::new(store.clone());
	let paste = session_b
		.execute(&json!({ "input": format!("[shared.txt#{tag}]\nPUT 2.=2 @saved\n") }), &ctx)
		.await
		.expect("paste");
	assert!(paste.is_error, "{}", paste.content);
	assert_eq!(dir.read("shared.txt"), "keep\n", "nothing was pasted");

	// A's own register still resolves on the same instance.
	let paste = session_a
		.execute(&json!({ "input": format!("[shared.txt#{tag}]\nPUT >$ @saved\n") }), &ctx)
		.await
		.expect("paste a");
	assert!(!paste.is_error, "{}", paste.content);
	assert_eq!(dir.read("shared.txt"), "keep\ntake\n");
}

#[tokio::test]
async fn cancelled_edit_never_writes() {
	let dir = TempDir::new("cancel");
	let store = Arc::new(snapshot::SnapshotStore::new());
	let path = dir.write("note.txt", "alpha\n");
	let tag = record(&store, &path, &[1]);

	let token = CancellationToken::new();
	token.cancel();
	let ctx = AgentContext::with_cancellation(dir.path.clone(), token);
	let tool = EditTool::new(store.clone());
	let input = format!("[note.txt#{tag}]\nPUT 1.=1:\n+beta\n");
	let result = tool
		.execute(&json!({ "input": input }), &ctx)
		.await
		.expect("edit");
	assert!(result.is_error);
	assert!(result.content.contains("cancelled"), "{}", result.content);
	assert_eq!(dir.read("note.txt"), "alpha\n");
}

#[tokio::test]
async fn named_registers_persist_across_calls_on_one_tool_instance() {
	let dir = TempDir::new("registers");
	let store = Arc::new(snapshot::SnapshotStore::new());
	let path = dir.write("registers.txt", "keep\ntake\n");
	let tag = record(&store, &path, &[1, 2]);
	let tool = EditTool::new(store.clone());
	let ctx = AgentContext::new(dir.path.clone());

	let cut_input = format!("[registers.txt#{tag}]\nCUT 2.=2 @saved\n");
	let cut = tool
		.execute(&json!({ "input": cut_input }), &ctx)
		.await
		.expect("cut");
	assert!(!cut.is_error, "{}", cut.content);
	assert_eq!(dir.read("registers.txt"), "keep\n");

	let tag = record(&store, &path, &[1]);
	let paste_input = format!("[registers.txt#{tag}]\nPUT >$ @saved\n");
	let paste = tool
		.execute(&json!({ "input": paste_input }), &ctx)
		.await
		.expect("paste");
	assert!(!paste.is_error, "{}", paste.content);
	assert_eq!(dir.read("registers.txt"), "keep\ntake\n");
}
