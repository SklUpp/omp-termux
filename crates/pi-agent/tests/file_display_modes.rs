//! Consumer contract for resolver-driven file display: one `FileDisplayInputs`
//! (edit mode, mounted edit tool, `readLineNumbers`) plus the per-call
//! `raw`/`immutable` flags decides whether `read`, `ast_grep`, and `ast_edit`
//! emit `[path#TAG]` headers, `:`/`|` numbers, or bare text.
//!
//! Every assertion goes through a real tool call against a task-owned temp
//! directory; snapshot tags and seen lines are read back from the same
//! `SnapshotStore` the tools recorded into, never from source text.

use std::{
	collections::BTreeSet,
	fs,
	path::PathBuf,
	sync::{
		Arc,
		atomic::{AtomicUsize, Ordering},
	},
};

use pi_agent::{
	AgentContext, Tool,
	tools::{
		AstEditTool, AstGrepTool, PendingStore, ReadTool,
		edit_variants::{EditMode, EditModeSettings, match_model_variant, resolve_edit_mode},
		hashline::{NativeSnapshotSink, SnapshotStore, canonical_key},
		read::FileDisplayInputs,
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
			"pi-display-{label}-{}-{}",
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
}

impl Drop for TempDir {
	fn drop(&mut self) {
		let _ = fs::remove_dir_all(&self.path);
	}
}

fn ctx(dir: &TempDir) -> AgentContext {
	AgentContext::new(dir.path.clone())
}

fn numbered_lines(count: usize) -> String {
	(1..=count).map(|n| format!("line{n}\n")).collect()
}

/// 4-hex tag from the `[path#TAG]` header line.
fn header_tag(content: &str) -> String {
	let header = content.lines().next().expect("header line");
	assert!(header.starts_with('[') && header.ends_with(']'), "{header}");
	header
		.rsplit_once('#')
		.map(|(_, tag)| tag.trim_end_matches(']').to_string())
		.unwrap_or_else(|| panic!("header has no tag: {header}"))
}

fn seen(lines: &[u32]) -> Option<BTreeSet<u32>> {
	Some(lines.iter().copied().collect())
}

/// `MARKER LINE SEP text` line whose payload contains `needle`. Keeps the
/// consumer contract (marker, line number, separator) without pinning how the
/// AST match text itself is trimmed.
fn has_marked_line(content: &str, marker: &str, needle: &str) -> bool {
	content
		.lines()
		.any(|line| line.starts_with(marker) && line.contains(needle))
}

fn hash_read_tool(store: &Arc<SnapshotStore>) -> ReadTool {
	ReadTool::with_snapshot_sink(Arc::new(NativeSnapshotSink::new(Arc::clone(store))))
		.with_display_inputs(FileDisplayInputs::new(EditMode::Hashline, true, false))
}

fn big_code_fixture() -> String {
	let mut code = String::from("fn big() {\n");
	for index in 0..110 {
		code.push_str(&format!("    let value_{index} = {index};\n"));
	}
	code.push_str("}\n");
	code
}

#[tokio::test]
async fn resolved_hashline_multi_range_emits_one_header_and_colon_numbers() {
	let dir = TempDir::new("multi-hash");
	let file = dir.write("f.txt", &numbered_lines(10));
	let store = Arc::new(SnapshotStore::new());
	let tool = hash_read_tool(&store);
	let ctx = AgentContext::new(dir.path.clone());

	let result = tool
		.execute(&json!({ "path": "f.txt:5-6,1-2" }), &ctx)
		.await
		.expect("read");
	assert!(!result.is_error, "{}", result.content);
	let tag = header_tag(&result.content);
	assert_eq!(
		result.content,
		format!("[f.txt#{tag}]\n1:line1\n2:line2\n\n…\n\n5:line5\n6:line6")
	);

	let snapshot = store.head(&canonical_key(&file)).expect("snapshot");
	assert_eq!(snapshot.hash, tag, "header tag must be the stored content tag");
	assert_eq!(
		snapshot.seen_lines,
		seen(&[1, 2, 5, 6]),
		"only the displayed ranges are seen provenance"
	);
}

#[tokio::test]
async fn raw_multi_range_suppresses_numbers_and_snapshot_recording() {
	let dir = TempDir::new("multi-raw");
	let file = dir.write("f.txt", &numbered_lines(10));
	let store = Arc::new(SnapshotStore::new());
	let tool = ReadTool::with_snapshot_sink(Arc::new(NativeSnapshotSink::new(Arc::clone(&store))))
		.with_display_inputs(FileDisplayInputs::new(EditMode::Hashline, true, true));
	let ctx = AgentContext::new(dir.path.clone());

	let result = tool
		.execute(&json!({ "path": "f.txt:raw:5-6,1-2" }), &ctx)
		.await
		.expect("read");
	assert!(!result.is_error, "{}", result.content);
	assert_eq!(result.content, "line1\nline2\n\n…\n\nline5\nline6");
	assert!(
		store.head(&canonical_key(&file)).is_none(),
		"a raw read must not mint a snapshot"
	);
}

#[tokio::test]
async fn resolved_summary_honours_hash_number_plain_and_no_edit_inputs() {
	let dir = TempDir::new("summary-modes");
	let file = dir.write("big.rs", &big_code_fixture());
	let ctx = AgentContext::new(dir.path.clone());
	let store = Arc::new(SnapshotStore::new());

	let hashed = hash_read_tool(&store)
		.execute(&json!({ "path": "big.rs" }), &ctx)
		.await
		.expect("hash summary");
	assert!(!hashed.is_error, "{}", hashed.content);
	assert!(hashed.content.contains("ln elided; re-read needed ranges"), "{}", hashed.content);
	assert!(
		hashed.content.contains("1-112:"),
		"a brace-collapsed summary stays one numbered range row: {}",
		hashed.content
	);
	let tag = header_tag(&hashed.content);
	let snapshot = store.head(&canonical_key(&file)).expect("summary snapshot");
	assert_eq!(snapshot.hash, tag);
	let seen_lines = snapshot.seen_lines.expect("summary records seen lines");
	assert!(seen_lines.contains(&1), "{seen_lines:?}");
	assert!(seen_lines.contains(&112), "the kept tail is seen: {seen_lines:?}");
	assert!(!seen_lines.contains(&50), "elided middle must not be seen: {seen_lines:?}");
	assert!(seen_lines.len() < 112, "summary must not claim every line: {seen_lines:?}");

	let numbered = ReadTool::new()
		.with_display_inputs(FileDisplayInputs::new(EditMode::Hashline, false, true))
		.execute(&json!({ "path": "big.rs" }), &ctx)
		.await
		.expect("numbered summary");
	assert!(!numbered.is_error, "{}", numbered.content);
	assert!(numbered.content.starts_with("1-112|"), "{}", numbered.content);

	let plain = ReadTool::new()
		.with_display_inputs(FileDisplayInputs::new(EditMode::Patch, true, false))
		.execute(&json!({ "path": "big.rs" }), &ctx)
		.await
		.expect("plain summary");
	assert!(!plain.is_error, "{}", plain.content);
	assert!(plain.content.starts_with("fn big() {"), "{}", plain.content);
	assert!(!plain.content.lines().next().unwrap().contains('|'), "{}", plain.content);
}

#[tokio::test]
async fn ast_grep_hashline_tags_files_and_records_only_match_lines() {
	let dir = TempDir::new("grep-hash");
	let file = dir.write(
		"one.ts",
		"fn main() {\n    foo(a0);\n    let x = 1;\n    foo(a1);\n}\n",
	);
	let store = Arc::new(SnapshotStore::new());
	let tool = AstGrepTool::with_snapshot_sink(Arc::new(NativeSnapshotSink::new(Arc::clone(&store))))
		.with_display_inputs(FileDisplayInputs::new(EditMode::Hashline, true, false));
	let ctx = AgentContext::new(dir.path.clone());

	let result = tool
		.execute(&json!({ "pat": "foo($A)", "path": "one.ts" }), &ctx)
		.await
		.expect("ast_grep");
	assert!(!result.is_error, "{}", result.content);
	assert!(result.content.starts_with("[one.ts#"), "{}", result.content);
	assert!(has_marked_line(&result.content, "*2:", "foo(a0)"), "{}", result.content);
	assert!(has_marked_line(&result.content, "*4:", "foo(a1)"), "{}", result.content);
	assert!(!result.content.contains("*2|"), "{}", result.content);

	let tag = header_tag(&result.content);
	let snapshot = store.head(&canonical_key(&file)).expect("grep snapshot");
	assert_eq!(snapshot.hash, tag);
	assert_eq!(snapshot.seen_lines, seen(&[2, 4]), "match lines are seen provenance");
	assert!(
		result.details.as_ref().is_some_and(|details| details["displayContent"]
			.as_str()
			.is_some_and(|display| display.contains('│') && display.contains("foo(a0)"))),
		"{}",
		result.content
	);

	let plain_store = Arc::new(SnapshotStore::new());
	let plain =
		AstGrepTool::with_snapshot_sink(Arc::new(NativeSnapshotSink::new(Arc::clone(&plain_store))))
			.with_display_inputs(FileDisplayInputs::new(EditMode::Hashline, false, false));
	let result = plain
		.execute(&json!({ "pat": "foo($A)", "path": "one.ts" }), &ctx)
		.await
		.expect("plain ast_grep");
	assert!(!result.is_error, "{}", result.content);
	assert!(!result.content.contains("[one.ts#"), "{}", result.content);
	assert!(has_marked_line(&result.content, "*2|", "foo(a0)"), "{}", result.content);
	assert!(
		plain_store.head(&canonical_key(&file)).is_none(),
		"plain output minted a snapshot"
	);
}

#[tokio::test]
async fn ast_grep_dedups_canonical_targets_and_keeps_single_entry_globs_scoped() {
	let dir = TempDir::new("grep-scope");
	dir.write("a.ts", "const a = foo(1);\n");
	dir.write("src/top.ts", "const top = foo(1);\n");
	dir.write("src/lib/nested.ts", "const nested = foo(1);\n");
	let ctx = AgentContext::new(dir.path.clone());
	let tool = AstGrepTool::new();

	let duplicate = tool
		.execute(&json!({ "pat": "foo($A)", "path": "a.ts;./a.ts" }), &ctx)
		.await
		.expect("duplicate targets");
	assert!(!duplicate.is_error, "{}", duplicate.content);
	let details = duplicate.details.as_ref().expect("details");
	assert_eq!(details["matchCount"].as_u64(), Some(1), "{}", duplicate.content);
	assert_eq!(details["filesSearched"].as_u64(), Some(1));
	assert_eq!(details["files"], json!(["a.ts"]));

	let glob = tool
		.execute(&json!({ "pat": "foo($A)", "path": "src/*.ts" }), &ctx)
		.await
		.expect("single glob");
	assert!(!glob.is_error, "{}", glob.content);
	let details = glob.details.as_ref().expect("details");
	assert_eq!(details["filesSearched"].as_u64(), Some(1), "{}", glob.content);
	assert_eq!(details["files"], json!(["src/top.ts"]));
	assert!(!glob.content.contains("nested.ts"), "{}", glob.content);

	#[cfg(unix)]
	{
		std::os::unix::fs::symlink(dir.path.join("a.ts"), dir.path.join("link.ts")).expect("symlink");
		let linked = tool
			.execute(&json!({ "pat": "foo($A)", "path": "a.ts;link.ts" }), &ctx)
			.await
			.expect("symlink target");
		assert!(!linked.is_error, "{}", linked.content);
		let details = linked.details.as_ref().expect("details");
		assert_eq!(details["matchCount"].as_u64(), Some(1), "{}", linked.content);
		assert_eq!(details["filesSearched"].as_u64(), Some(1));
	}
}

#[tokio::test]
async fn ast_edit_resolved_inputs_choose_hash_or_column_frames() {
	let dir = TempDir::new("ast-edit-display");
	let file = dir.write("a.ts", "const a = foo(1);\n");
	let ctx = AgentContext::new(dir.path.clone());
	let ops = json!({ "ops": [{ "pat": "foo($A)", "out": "bar($A)" }], "paths": ["a.ts"] });

	let snapshots = Arc::new(SnapshotStore::new());
	let hashed = AstEditTool::with_display_inputs(
		Arc::clone(&snapshots),
		Arc::new(PendingStore::new()),
		FileDisplayInputs::new(EditMode::Hashline, true, false),
	);
	let result = hashed.execute(&ops, &ctx).await.expect("hash preview");
	assert!(!result.is_error, "{}", result.content);
	assert!(result.content.contains("[a.ts#"), "{}", result.content);
	assert!(!result.content.contains("-1:11 "), "{}", result.content);
	assert!(
		result.details.as_ref().is_some_and(|details| details["displayContent"]
			.as_str()
			.is_some_and(|display| display.contains('│'))),
		"{}",
		result.content
	);
	assert_eq!(
		fs::read_to_string(&file).expect("read"),
		"const a = foo(1);\n",
		"preview must not write"
	);

	let plain = AstEditTool::with_display_inputs(
		snapshots,
		Arc::new(PendingStore::new()),
		FileDisplayInputs::new(EditMode::Hashline, false, true),
	);
	let result = plain.execute(&ops, &ctx).await.expect("plain preview");
	assert!(!result.is_error, "{}", result.content);
	assert!(!result.content.contains("[a.ts#"), "{}", result.content);
	assert!(
		result.content.contains("-1:11 foo(1)"),
		"without an edit tool the preview keeps column frames even when readLineNumbers is set: {}",
		result.content
	);
}

#[tokio::test]
async fn missing_edit_tool_keeps_plain_numbers_without_hashlines() {
	let dir = TempDir::new("no-edit-tool");
	let file = dir.write("f.txt", "one\ntwo\nthree\n");
	let store = Arc::new(SnapshotStore::new());
	// `readLineNumbers` is on, but no `edit` tool consumes anchors: digits only.
	let tool = ReadTool::with_snapshot_sink(Arc::new(NativeSnapshotSink::new(Arc::clone(&store))))
		.with_display_inputs(FileDisplayInputs::new(EditMode::Hashline, false, true));

	let result = tool
		.execute(&json!({ "path": "f.txt:2-2" }), &ctx(&dir))
		.await
		.expect("read");
	assert!(!result.is_error, "{}", result.content);
	assert_eq!(result.content, "1|one\n2|two\n3|three");
	assert!(store.head(&canonical_key(&file)).is_none(), "no anchors, no snapshot");
}

#[tokio::test]
async fn non_hashline_mode_renders_plain_text_and_records_nothing() {
	let dir = TempDir::new("non-hashline");
	let file = dir.write("f.txt", "one\ntwo\nthree\n");
	let store = Arc::new(SnapshotStore::new());
	let tool = ReadTool::with_snapshot_sink(Arc::new(NativeSnapshotSink::new(Arc::clone(&store))))
		.with_display_inputs(FileDisplayInputs::new(EditMode::Patch, true, false));

	let result = tool
		.execute(&json!({ "path": "f.txt:2-2" }), &ctx(&dir))
		.await
		.expect("read");
	assert!(!result.is_error, "{}", result.content);
	assert_eq!(result.content, "one\ntwo\nthree");
	assert!(store.head(&canonical_key(&file)).is_none());
}

#[tokio::test]
async fn raw_selector_suppresses_hashlines_and_provenance() {
	let dir = TempDir::new("raw-suppression");
	let file = dir.write("f.txt", &numbered_lines(3));
	let store = Arc::new(SnapshotStore::new());
	let tool = ReadTool::with_snapshot_sink(Arc::new(NativeSnapshotSink::new(Arc::clone(&store))))
		.with_display_inputs(FileDisplayInputs::new(EditMode::Hashline, true, true));

	let result = tool
		.execute(&json!({ "path": "f.txt:raw" }), &ctx(&dir))
		.await
		.expect("read");
	assert!(!result.is_error, "{}", result.content);
	assert_eq!(result.content, numbered_lines(3));
	assert!(!result.content.contains('['), "raw output must stay verbatim");
	assert!(store.head(&canonical_key(&file)).is_none(), "raw reads mint no anchors");
}

#[tokio::test]
async fn bom_crlf_text_is_normalized_before_hashing() {
	let dir = TempDir::new("bom-crlf");
	let file = dir.write("f.txt", "\u{feff}a\r\nb\r\nc\r\n");
	let store = Arc::new(SnapshotStore::new());
	let tool = hash_read_tool(&store);

	let result = tool
		.execute(&json!({ "path": "f.txt:2-2" }), &ctx(&dir))
		.await
		.expect("read");
	assert!(!result.is_error, "{}", result.content);
	let snapshot = store.head(&canonical_key(&file)).expect("snapshot recorded");
	// The tag and the snapshot text come from the same normalized bytes, so a
	// later `edit` validates against LF content without a BOM.
	assert_eq!(snapshot.text, "a\nb\nc\n");
	assert_eq!(header_tag(&result.content), snapshot.hash);
	assert!(result.content.ends_with("1:a\n2:b\n3:c"), "{}", result.content);
	assert_eq!(snapshot.seen_lines, seen(&[1, 2, 3]));
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_read_records_the_canonical_target() {
	let dir = TempDir::new("symlink-read");
	let real = dir.write("real.txt", "one\ntwo\n");
	std::os::unix::fs::symlink("real.txt", dir.path.join("link.txt")).expect("symlink");
	let store = Arc::new(SnapshotStore::new());
	let tool = hash_read_tool(&store);

	let result = tool
		.execute(&json!({ "path": "link.txt" }), &ctx(&dir))
		.await
		.expect("read");
	assert!(!result.is_error, "{}", result.content);
	assert!(result.content.starts_with("[link.txt#"), "{}", result.content);

	// The store keys by canonical path, so the tag follows the real file even
	// though the header shows the authored link.
	let snapshot = store.head(&canonical_key(&real)).expect("canonical snapshot");
	assert_eq!(header_tag(&result.content), snapshot.hash);
	assert_eq!(snapshot.seen_lines, seen(&[1, 2]));
}

struct Settings {
	mode:    Option<&'static str>,
	variant: Option<EditMode>,
}

impl EditModeSettings for Settings {
	fn get_edit_mode(&self) -> Option<&str> {
		self.mode
	}

	fn get_edit_variant_for_model(&self, _model: Option<&str>) -> Option<EditMode> {
		self.variant
	}
}

#[test]
fn edit_mode_resolution_prefers_variant_then_env_then_configured_mode() {
	let variant = Settings { mode: Some("patch"), variant: Some(EditMode::Replace) };
	assert_eq!(
		resolve_edit_mode(&variant, Some("any/model"), Some("sloppy"), false),
		EditMode::Replace
	);

	let configured = Settings { mode: Some("patch"), variant: None };
	// A known env override outranks `edit.mode` …
	assert_eq!(
		resolve_edit_mode(&configured, Some("any/model"), Some("sloppy"), false),
		EditMode::Sloppy
	);
	// … while an unknown one is skipped instead of defaulting.
	assert_eq!(
		resolve_edit_mode(&configured, Some("any/model"), Some("bogus"), false),
		EditMode::Patch
	);
}

#[test]
fn bad_hashline_model_falls_back_to_sloppy_unless_strict() {
	let settings = Settings { mode: Some("hashline"), variant: None };
	assert_eq!(
		resolve_edit_mode(&settings, Some("moonshotai/kimi-k2"), None, false),
		EditMode::Sloppy
	);
	assert_eq!(
		resolve_edit_mode(&settings, Some("moonshotai/kimi-k2"), None, true),
		EditMode::Hashline
	);
	// Unknown model: the configured hashline mode is kept.
	assert_eq!(
		resolve_edit_mode(&settings, Some("local/any"), None, false),
		EditMode::Hashline
	);
}

#[test]
fn model_variant_matching_is_first_match_case_insensitive() {
	let entries = [("Gemini", EditMode::Sloppy), ("gemini-2", EditMode::Replace)];
	assert_eq!(
		match_model_variant(entries, Some("google/GEMINI-2-pro")),
		Some(EditMode::Sloppy)
	);
	assert_eq!(match_model_variant(entries, None), None);
}

#[tokio::test]
async fn ast_grep_records_seen_lines_per_file() {
	let dir = TempDir::new("grep-seen");
	let a = dir.write("a.ts", "const a = foo(1);\nconst b = bar(2);\nconst c = foo(3);\n");
	let b = dir.write("b.ts", "const d = foo(4);\n");
	let store = Arc::new(SnapshotStore::new());
	let tool = AstGrepTool::with_snapshot_sink(Arc::new(NativeSnapshotSink::new(Arc::clone(&store))))
		.with_display_inputs(FileDisplayInputs::new(EditMode::Hashline, true, false));

	let result = tool
		.execute(&json!({ "pat": "foo($A)", "path": ["a.ts", "b.ts"] }), &ctx(&dir))
		.await
		.expect("ast grep");
	assert!(!result.is_error, "{}", result.content);
	assert!(has_marked_line(&result.content, "*1:", "foo(1)"), "{}", result.content);
	assert!(has_marked_line(&result.content, "*3:", "foo(3)"), "{}", result.content);
	assert!(!result.content.contains("*1|"), "hashline mode must not use plain pipes");

	// Provenance is per file and covers exactly the emitted match lines.
	let snapshot_a = store.head(&canonical_key(&a)).expect("a snapshot");
	assert_eq!(snapshot_a.seen_lines, seen(&[1, 3]));
	assert_eq!(store.head(&canonical_key(&b)).expect("b snapshot").seen_lines, seen(&[1]));
	assert!(
		result.content.contains(&format!("a.ts#{}", snapshot_a.hash)),
		"{}",
		result.content
	);
}

#[tokio::test]
async fn ast_grep_falls_back_to_plain_lines_above_the_snapshot_cap() {
	let dir = TempDir::new("grep-cap");
	let pad = format!("//{}\n", "x".repeat(4 * 1024 * 1024 + 32));
	let file = dir.write("big.ts", &format!("{pad}const a = foo(1);\n"));
	let store = Arc::new(SnapshotStore::new());
	let tool = AstGrepTool::with_snapshot_sink(Arc::new(NativeSnapshotSink::new(Arc::clone(&store))))
		.with_display_inputs(FileDisplayInputs::new(EditMode::Hashline, true, false));

	let result = tool
		.execute(&json!({ "pat": "foo($A)", "path": "big.ts" }), &ctx(&dir))
		.await
		.expect("ast grep");
	assert!(!result.is_error, "{}", result.content);
	assert!(has_marked_line(&result.content, "*2|", "foo(1)"), "{}", result.content);
	assert!(!result.content.contains("[big.ts#"), "{}", result.content);
	assert!(store.head(&canonical_key(&file)).is_none(), "above the cap mints no tag");
}

#[cfg(unix)]
#[tokio::test]
async fn overlapping_directory_symlink_and_glob_entries_match_each_file_once() {
	let dir = TempDir::new("grep-dedup-overlap");
	dir.write("src/a.ts", "const a = foo(1);\n");
	std::os::unix::fs::symlink("src/a.ts", dir.path.join("link.ts")).expect("symlink");

	// The same canonical file is reachable through the directory walk, the
	// symlink literal, and the recursive glob; it must be searched once.
	let result = AstGrepTool::new()
		.execute(
			&json!({ "pat": "foo($A)", "path": ["src", "link.ts", "**/*.ts"] }),
			&ctx(&dir),
		)
		.await
		.expect("ast grep");
	assert!(!result.is_error, "{}", result.content);
	let details = result.details.as_ref().expect("details");
	assert_eq!(details["matchCount"], json!(1), "{}", result.content);
	assert_eq!(details["fileCount"], json!(1));
	assert_eq!(details["files"], json!(["src/a.ts"]));
	assert_eq!(result.content.matches("foo(1)").count(), 1, "{}", result.content);
}
