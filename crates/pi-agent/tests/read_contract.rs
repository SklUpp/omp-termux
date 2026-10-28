//! Behavior contract for the `read` tool: selector grammar, literal-colon
//! paths, context expansion, caps, directory listings, snapshot sink seam, and
//! the safety walls (non-regular files, cancellation, binary sniffing).

use std::{
	path::PathBuf,
	sync::{
		Arc, Mutex,
		atomic::{AtomicUsize, Ordering},
	},
};

use pi_agent::{AgentContext, CancellationToken, ReadTool, Tool, tools::read::ReadSnapshotSink};
use serde_json::json;

struct TempDir {
	path: PathBuf,
}

impl TempDir {
	fn new(label: &str) -> Self {
		static NEXT: AtomicUsize = AtomicUsize::new(0);
		let unique =
			format!("pi-agent-read-{label}-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::SeqCst));
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
}

impl Drop for TempDir {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.path);
	}
}

fn numbered_lines(count: usize) -> String {
	(1..=count).map(|n| format!("line{n}\n")).collect()
}

fn ctx(dir: &TempDir) -> AgentContext {
	AgentContext::new(dir.path.clone())
}

async fn read(ctx: &AgentContext, path: &str) -> pi_agent::ToolResult {
	ReadTool::new()
		.execute(&json!({ "path": path }), ctx)
		.await
		.expect("read call")
}

#[tokio::test]
async fn literal_colon_filename_wins_over_selector() {
	let dir = TempDir::new("literal-colon");
	dir.write("weird:1-2", "literal\n");
	let ctx = ctx(&dir);

	let result = read(&ctx, "weird:1-2").await;
	assert!(!result.is_error, "{}", result.content);
	assert_eq!(result.content, "1|literal");
	assert!(result.details.unwrap()["resolvedPath"].as_str().unwrap().ends_with("weird:1-2"));
}

#[tokio::test]
async fn line_range_adds_one_leading_and_three_trailing_context_lines() {
	let dir = TempDir::new("range-context");
	dir.write("f.txt", &numbered_lines(100));
	let ctx = ctx(&dir);

	let result = read(&ctx, "f.txt:10-12").await;
	assert!(!result.is_error, "{}", result.content);
	assert_eq!(
		result.content,
		"9|line9\n10|line10\n11|line11\n12|line12\n13|line13\n14|line14\n15|line15"
	);
	let details = result.details.expect("details");
	assert_eq!(details["totalLines"], json!(100));
	assert_eq!(details["truncation"]["truncated"], json!(true));
}

#[tokio::test]
async fn raw_ranges_are_exact_and_skip_context() {
	let dir = TempDir::new("raw-range");
	dir.write("f.txt", &numbered_lines(5));
	let ctx = ctx(&dir);

	let trailing = read(&ctx, "f.txt:2-2:raw").await;
	let leading = read(&ctx, "f.txt:raw:2-2").await;
	assert!(!trailing.is_error, "{}", trailing.content);
	assert_eq!(trailing.content, "line2");
	assert_eq!(trailing.content, leading.content);

	let whole = read(&ctx, "f.txt:raw").await;
	assert_eq!(whole.content, numbered_lines(5));
}

#[tokio::test]
async fn raw_multi_range_keeps_verbatim_blocks() {
	let dir = TempDir::new("raw-multi");
	dir.write("f.txt", &numbered_lines(10));
	let ctx = ctx(&dir);

	let result = read(&ctx, "f.txt:raw:1-2,5-6").await;
	assert!(!result.is_error, "{}", result.content);
	assert_eq!(result.content, "line1\nline2\n\n…\n\nline5\nline6");
}

#[tokio::test]
async fn multi_range_merges_sorts_and_marks_the_gap() {
	let dir = TempDir::new("multi-range");
	dir.write("f.txt", &numbered_lines(10));
	let ctx = ctx(&dir);

	let result = read(&ctx, "f.txt:5-6,1-2").await;
	assert!(!result.is_error, "{}", result.content);
	assert_eq!(result.content, "1|line1\n2|line2\n\n…\n\n5|line5\n6|line6");

	let merged = read(&ctx, "f.txt:1-2,2-3").await;
	assert!(
		merged.content.starts_with("1|line1\n2|line2\n3|line3"),
		"merged range is a single window: {}",
		merged.content
	);
	assert!(!merged.content.contains("\n\n…\n\n"), "adjacent ranges must merge: {}", merged.content);
}

#[tokio::test]
async fn open_ended_and_count_selectors() {
	let dir = TempDir::new("open-ended");
	dir.write("f.txt", &numbered_lines(100));
	let ctx = ctx(&dir);

	let tail = read(&ctx, "f.txt:98-").await;
	assert!(!tail.is_error, "{}", tail.content);
	assert!(tail.content.starts_with("97|line97\n98|line98"), "{}", tail.content);
	assert!(tail.content.ends_with("100|line100"), "{}", tail.content);

	let counted = read(&ctx, "f.txt:50+2").await;
	assert!(counted.content.contains("50|line50\n51|line51"), "{}", counted.content);
	assert!(counted.content.contains("49|line49"), "context: {}", counted.content);
}

#[tokio::test]
async fn invalid_selectors_are_errors_not_silent_widening() {
	let dir = TempDir::new("invalid-selector");
	dir.write("f.txt", &numbered_lines(5));
	let ctx = ctx(&dir);

	let zero = read(&ctx, "f.txt:0").await;
	assert!(zero.is_error);
	assert!(zero.content.contains("Line selector 0 is invalid"), "{}", zero.content);

	let reversed = read(&ctx, "f.txt:4-2").await;
	assert!(reversed.is_error);
	assert!(reversed.content.contains("end must be >= start"), "{}", reversed.content);

	let zero_count = read(&ctx, "f.txt:2+0").await;
	assert!(zero_count.is_error);
	assert!(zero_count.content.contains("count must be >= 1"), "{}", zero_count.content);
}

#[tokio::test]
async fn off_end_range_reports_total_without_error() {
	let dir = TempDir::new("off-end");
	dir.write("f.txt", &numbered_lines(10));
	let ctx = ctx(&dir);

	let result = read(&ctx, "f.txt:999").await;
	assert!(!result.is_error, "{}", result.content);
	assert_eq!(
		result.content,
		"Line 999 is beyond end of file (10 lines total). Use :1 to read from the start, or :10 to read the last line."
	);
}

#[tokio::test]
async fn empty_file_reads_as_a_single_empty_line() {
	let dir = TempDir::new("empty");
	dir.write("empty.txt", "");
	let ctx = ctx(&dir);

	// TS treats an empty file as one addressable empty line, not zero lines.
	let result = read(&ctx, "empty.txt").await;
	assert!(!result.is_error, "{}", result.content);
	assert_eq!(result.content, "1|");
	assert_eq!(result.details.expect("details")["totalLines"], json!(1));
}

#[tokio::test]
async fn default_open_ended_read_stops_at_the_line_cap() {
	let dir = TempDir::new("line-cap");
	dir.write("f.txt", &numbered_lines(4000));
	let ctx = ctx(&dir);

	let result = read(&ctx, "f.txt").await;
	assert!(!result.is_error, "{}", result.content);
	let lines: Vec<&str> = result.content.lines().collect();
	assert_eq!(lines.len(), 300);
	assert_eq!(lines[0], "1|line1");
	assert_eq!(lines[299], "300|line300");
	let details = result.details.expect("details");
	assert_eq!(details["truncation"]["truncatedBy"], json!("lines"));
	assert_eq!(details["totalLines"], json!(4000));
}

#[tokio::test]
async fn oversized_single_line_is_bounded_and_flagged() {
	let dir = TempDir::new("byte-cap");
	dir.write("f.txt", &"a".repeat(200 * 1024));
	let ctx = ctx(&dir);

	let result = read(&ctx, "f.txt").await;
	assert!(!result.is_error, "{}", result.content);
	// No sink: the preview is the numbered first `maxBytes` bytes, never the
	// whole 200 KiB line.
	assert!(result.content.starts_with("1|aaaa"), "{}", result.content);
	assert!(result.content.len() <= 150 * 1024 + 2, "{}", result.content.len());
	let details = result.details.expect("details");
	assert_eq!(details["truncation"]["firstLineExceedsLimit"], json!(true));
}

#[tokio::test]
async fn hashline_mode_refuses_to_number_a_truncated_first_line() {
	let dir = TempDir::new("byte-cap-sink");
	dir.write("f.txt", &"a".repeat(200 * 1024));
	let sink = std::sync::Arc::new(TestSink::default());
	let tool = ReadTool::with_snapshot_sink(sink.clone());
	let ctx = ctx(&dir);

	let result = tool
		.execute(&json!({ "path": "f.txt" }), &ctx)
		.await
		.expect("read");
	assert!(!result.is_error, "{}", result.content);
	assert!(
		result
			.content
			.starts_with("[Line 1 is 200.0KB, exceeds 150.0KB limit. Hashline output requires full lines"),
		"{}",
		result.content
	);
	assert!(sink.files.lock().expect("files").is_empty());
}

#[tokio::test]
async fn late_range_in_a_large_file_stays_bounded_and_correct() {
	let dir = TempDir::new("large-late-range");
	let path = dir.path.join("big.txt");
	let line = "0123456789abcdef\n";
	let count = (5 * 1024 * 1024) / line.len() + 10;
	std::fs::write(&path, line.repeat(count)).expect("write big fixture");
	let ctx = ctx(&dir);

	let target = count - 2;
	let result = read(&ctx, &format!("big.txt:{target}-{target}")).await;
	assert!(!result.is_error, "{}", result.content);
	assert!(
		result.content.contains(&format!("{target}|0123456789abcdef")),
		"{}",
		result.content
	);
	assert!(result.content.len() < 4096, "window escaped the cap: {}", result.content.len());
}

#[tokio::test]
async fn directory_listing_renders_entries_and_slices() {
	let dir = TempDir::new("dir-listing");
	dir.write("a.txt", "a");
	dir.write("b.txt", "b");
	dir.write("sub/c.txt", "c");
	let ctx = ctx(&dir);

	let listing = read(&ctx, ".").await;
	assert!(!listing.is_error, "{}", listing.content);
	assert!(listing.content.contains("- a.txt"), "{}", listing.content);
	assert!(listing.content.contains("- sub/"), "{}", listing.content);
	assert!(listing.content.contains("    - c.txt"), "{}", listing.content);
	let details = listing.details.clone().expect("details");
	assert_eq!(details["isDirectory"], json!(true));

	let second = read(&ctx, ".:2-").await;
	assert!(!second.is_error, "{}", second.content);
	let remaining: Vec<&str> = listing.content.lines().skip(1).collect();
	assert!(second.content.starts_with(remaining[0]), "{}", second.content);
	assert_eq!(second.content.lines().count(), remaining.len());

	let root_alias = read(&ctx, "/").await;
	assert_eq!(
		root_alias.content.lines().count(),
		listing.content.lines().count()
	);

	let multi = read(&ctx, ".:1-2,4-5").await;
	assert!(multi.is_error);
	assert!(multi.content.contains("Multi-range line selectors"));
}

#[tokio::test]
async fn symlinks_resolve_and_dangling_links_error() {
	let dir = TempDir::new("symlink");
	let target = dir.write("f.txt", "target\n");
	std::os::unix::fs::symlink(&target, dir.path.join("link.txt")).expect("symlink");
	std::os::unix::fs::symlink(dir.path.join("missing.txt"), dir.path.join("dangling.txt"))
		.expect("dangling symlink");
	let ctx = ctx(&dir);

	let linked = read(&ctx, "link.txt").await;
	assert!(!linked.is_error, "{}", linked.content);
	assert_eq!(linked.content, "1|target");

	let dangling = read(&ctx, "dangling.txt").await;
	assert!(dangling.is_error);
}

#[tokio::test]
async fn non_regular_files_are_rejected() {
	let dir = TempDir::new("non-regular");
	let fifo = dir.path.join("pipe");
	let made = std::process::Command::new("mkfifo")
		.arg(&fifo)
		.status()
		.map(|status| status.success())
		.unwrap_or(false);
	let ctx = ctx(&dir);

	if made {
		let result = read(&ctx, "pipe").await;
		assert!(result.is_error);
		assert!(result.content.contains("not a regular file"), "{}", result.content);
	}

	let missing = read(&ctx, "does-not-exist.txt").await;
	assert!(missing.is_error);
}

#[tokio::test]
async fn pre_cancelled_context_aborts_before_reading() {
	let dir = TempDir::new("cancel");
	dir.write("f.txt", &numbered_lines(10));
	let token = CancellationToken::new();
	token.cancel();
	let ctx = AgentContext::with_cancellation(dir.path.clone(), token);

	let result = read(&ctx, "f.txt").await;
	assert!(result.is_error);
	assert_eq!(result.content, "Read cancelled");
}

#[tokio::test]
async fn binary_files_need_explicit_raw() {
	let dir = TempDir::new("binary");
	dir.write("bin.dat", "\0\u{1}abc");
	let ctx = ctx(&dir);

	let notice = read(&ctx, "bin.dat").await;
	assert!(!notice.is_error, "{}", notice.content);
	assert!(notice.content.contains("Cannot read binary file"), "{}", notice.content);

	let raw = read(&ctx, "bin.dat:raw").await;
	assert!(!raw.is_error, "{}", raw.content);
	assert!(raw.content.starts_with('\u{0}'), "{:?}", raw.content);
}

#[derive(Default)]
struct TestSink {
	files: Mutex<Vec<(PathBuf, String)>>,
	seen: Mutex<Vec<(PathBuf, String, Vec<u32>)>>,
}

impl ReadSnapshotSink for TestSink {
	fn record_file(&self, absolute_path: &std::path::Path, full_text: &str) -> Option<String> {
		self.files
			.lock()
			.expect("files")
			.push((absolute_path.to_path_buf(), full_text.to_string()));
		Some("0A1B".to_string())
	}

	fn record_seen_lines(&self, absolute_path: &std::path::Path, tag: &str, lines: &[u32]) {
		self.seen
			.lock()
			.expect("seen")
			.push((absolute_path.to_path_buf(), tag.to_string(), lines.to_vec()));
	}

	fn header(&self, absolute_path: &std::path::Path, tag: &str, _cwd: &std::path::Path) -> String {
		let name = absolute_path
			.file_name()
			.map(|name| name.to_string_lossy().into_owned())
			.unwrap_or_default();
		format!("[{name}#{tag}]")
	}
}

#[tokio::test]
async fn snapshot_sink_adds_header_and_records_displayed_lines() {
	let dir = TempDir::new("sink");
	dir.write("f.txt", "one\ntwo\nthree\n");
	let sink = std::sync::Arc::new(TestSink::default());
	let tool = ReadTool::with_snapshot_sink(sink.clone());
	let ctx = ctx(&dir);

	let result = tool
		.execute(&json!({ "path": "f.txt:2-2" }), &ctx)
		.await
		.expect("read");
	assert!(!result.is_error, "{}", result.content);
	let lines: Vec<&str> = result.content.lines().collect();
	assert_eq!(lines[0], "[f.txt#0A1B]");
	assert_eq!(lines[1], "1:one");
	assert!(lines.contains(&"2:two"), "{}", result.content);
	assert!(lines.contains(&"3:three"), "{}", result.content);

	let files = sink.files.lock().expect("files");
	assert_eq!(files.len(), 1);
	assert_eq!(files[0].1, "one\ntwo\nthree\n");
	let seen = sink.seen.lock().expect("seen");
	assert_eq!(seen.len(), 1);
	assert_eq!(seen[0].1, "0A1B");
	assert_eq!(seen[0].2, vec![1, 2, 3]);
}

#[tokio::test]
async fn raw_read_with_sink_records_without_a_header() {
	let dir = TempDir::new("sink-raw");
	dir.write("f.txt", "one\ntwo\n");
	let sink = std::sync::Arc::new(TestSink::default());
	let tool = ReadTool::with_snapshot_sink(sink.clone());
	let ctx = ctx(&dir);

	let result = tool
		.execute(&json!({ "path": "f.txt:raw:2-2" }), &ctx)
		.await
		.expect("read");
	assert_eq!(result.content, "two");
	let files = sink.files.lock().expect("files");
	assert_eq!(files.len(), 1);
	let seen = sink.seen.lock().expect("seen");
	assert_eq!(seen[0].2, vec![2]);
}

#[tokio::test]
async fn sink_skips_files_above_the_snapshot_cap() {
	let dir = TempDir::new("sink-cap");
	let path = dir.path.join("big.txt");
	let line = "0123456789abcdef\n";
	let count = (5 * 1024 * 1024) / line.len() + 10;
	std::fs::write(&path, line.repeat(count)).expect("write big fixture");
	let sink = std::sync::Arc::new(TestSink::default());
	let tool = ReadTool::with_snapshot_sink(sink.clone());
	let ctx = ctx(&dir);

	let target = count - 2;
	let result = tool
		.execute(&json!({ "path": format!("big.txt:{target}-{target}") }), &ctx)
		.await
		.expect("read");
	assert!(!result.content.starts_with('['), "{}", result.content);
	assert!(sink.files.lock().expect("files").is_empty());
	assert!(sink.seen.lock().expect("seen").is_empty());
}

#[tokio::test]
async fn tilde_expands_against_home_before_selectors() {
	let dir = TempDir::new("tilde");
	dir.write("tilde.txt", "home\n");
	let mut ctx = ctx(&dir);
	ctx.env.insert("HOME".to_string(), dir.path.to_string_lossy().into_owned());

	let whole = read(&ctx, "~/tilde.txt").await;
	assert!(!whole.is_error, "{}", whole.content);
	assert_eq!(whole.content, "1|home");

	let ranged = read(&ctx, "~/tilde.txt:1").await;
	assert!(!ranged.is_error, "{}", ranged.content);
	assert_eq!(ranged.content, "1|home");

	let home_root = read(&ctx, "~").await;
	assert!(!home_root.is_error, "{}", home_root.content);
	assert_eq!(home_root.details.expect("details")["isDirectory"], json!(true));
}

#[tokio::test]
async fn unique_workspace_suffix_resolves_missing_paths_once() {
	let dir = TempDir::new("suffix");
	dir.write("src/deep/unique_name.txt", "found\n");
	let ctx = ctx(&dir);

	let resolved = read(&ctx, "unique_name.txt").await;
	assert!(!resolved.is_error, "{}", resolved.content);
	assert!(
		resolved
			.content
			.starts_with("[Path 'unique_name.txt' not found; resolved to 'src/deep/unique_name.txt' via suffix match]\n1|found"),
		"{}",
		resolved.content
	);
	let details = resolved.details.expect("details");
	assert_eq!(details["suffixResolution"]["from"], json!("unique_name.txt"));
	assert_eq!(details["suffixResolution"]["to"], json!("src/deep/unique_name.txt"));

	let ranged = read(&ctx, "unique_name.txt:1").await;
	assert!(!ranged.is_error, "{}", ranged.content);
	assert!(ranged.content.contains("1|found"), "{}", ranged.content);
	assert!(ranged.content.contains("via suffix match"), "{}", ranged.content);

	let absolute = read(&ctx, "/definitely/not/here.txt").await;
	assert!(absolute.is_error);
	assert!(!absolute.content.contains("via suffix match"), "{}", absolute.content);
}

#[tokio::test]
async fn ambiguous_suffix_match_is_not_guessed() {
	let dir = TempDir::new("suffix-ambiguous");
	dir.write("a/same.txt", "a\n");
	dir.write("b/same.txt", "b\n");
	let ctx = ctx(&dir);

	let result = read(&ctx, "same.txt").await;
	assert!(result.is_error);
	assert!(result.content.contains("Failed to read"), "{}", result.content);
}

#[tokio::test]
async fn large_code_file_is_structurally_summarized() {
	let dir = TempDir::new("summary");
	let mut code = String::from("fn big() {\n");
	for index in 0..110 {
		code.push_str(&format!("    let value_{index} = {index};\n"));
	}
	code.push_str("}\n");
	dir.write("big.rs", &code);
	let ctx = ctx(&dir);

	let result = read(&ctx, "big.rs").await;
	assert!(!result.is_error, "{}", result.content);
	assert!(result.content.contains('…'), "{}", result.content);
	assert!(result.content.contains("ln elided; re-read needed ranges with big.rs:"), "{}", result.content);
	let details = result.details.expect("details");
	assert!(details["summary"]["elidedSpans"].as_u64().unwrap() >= 1, "{}", details);
	assert_eq!(details["summary"]["elidedLines"], json!(110));
}

#[tokio::test]
async fn prose_files_skip_summaries_by_default() {
	let dir = TempDir::new("prose");
	let text: String = (1..=120).map(|n| format!("line {n}\n")).collect();
	dir.write("notes.txt", &text);
	let ctx = ctx(&dir);

	let result = read(&ctx, "notes.txt").await;
	assert!(!result.is_error, "{}", result.content);
	assert!(!result.content.contains("…"), "{}", result.content);
	assert_eq!(result.content.lines().count(), 120);
	let details = result.details.expect("details");
	assert!(details.get("summary").is_none(), "{}", details);
}

#[tokio::test]
async fn explicit_selector_bypasses_summarization() {
	let dir = TempDir::new("summary-selector");
	let mut code = String::from("fn big() {\n");
	for index in 0..110 {
		code.push_str(&format!("    let value_{index} = {index};\n"));
	}
	code.push_str("}\n");
	dir.write("big.rs", &code);
	let ctx = ctx(&dir);

	let result = read(&ctx, "big.rs:1-2").await;
	assert!(!result.is_error, "{}", result.content);
	assert!(result.content.starts_with("1|fn big() {"), "{}", result.content);
	assert!(result.details.expect("details").get("summary").is_none());
}

#[tokio::test]
async fn non_raw_buffered_reads_strip_bom_and_normalize_crlf() {
	let dir = TempDir::new("normalize-buffered");
	dir.write("crlf.txt", "\u{feff}one\r\ntwo\r\n");
	let ctx = ctx(&dir);

	let result = read(&ctx, "crlf.txt").await;
	assert!(!result.is_error, "{}", result.content);
	assert_eq!(result.content, "1|one\n2|two");

	let raw = read(&ctx, "crlf.txt:raw").await;
	assert_eq!(raw.content, "\u{feff}one\r\ntwo\r\n");
}

#[derive(Default)]
struct RecordingSink {
	files: Mutex<Vec<String>>,
	seen:  Mutex<Vec<Vec<u32>>>,
}

impl ReadSnapshotSink for RecordingSink {
	fn record_file(&self, _: &std::path::Path, full_text: &str) -> Option<String> {
		self.files.lock().expect("files").push(full_text.to_string());
		Some("ABCD".to_string())
	}

	fn record_seen_lines(&self, _: &std::path::Path, _: &str, lines: &[u32]) {
		self.seen.lock().expect("seen").push(lines.to_vec());
	}

	fn header(&self, path: &std::path::Path, tag: &str, _: &std::path::Path) -> String {
		format!("[{}#{tag}]", path.display())
	}
}

#[tokio::test]
async fn snapshot_records_exactly_the_bytes_that_were_scanned() {
	let dir = TempDir::new("snapshot-bytes");
	dir.write("win.txt", "\u{feff}alpha\r\nbeta\r\n");
	let sink = Arc::new(RecordingSink::default());
	let tool = ReadTool::with_snapshot_sink(sink.clone());

	let result = tool
		.execute(&json!({ "path": "win.txt:1-1" }), &ctx(&dir))
		.await
		.expect("read");
	assert!(result.content.ends_with("\n1:alpha\n2:beta"), "{}", result.content);
	assert_eq!(sink.files.lock().expect("files")[0], "alpha\nbeta\n");
	assert_eq!(sink.seen.lock().expect("seen")[0], vec![1, 2]);
}

#[tokio::test]
async fn directory_listing_without_selector_is_byte_capped() {
	let dir = TempDir::new("dir-byte-cap");
	for index in 0..900 {
		dir.write(&format!("file-{index:04}-{}.txt", "x".repeat(60)), "x");
	}
	let ctx = ctx(&dir);

	let result = read(&ctx, ".").await;
	assert!(!result.is_error, "{}", result.content);
	assert!(
		result.content.len() <= 50 * 1024,
		"listing escaped the cap: {}",
		result.content.len()
	);
	let details = result.details.expect("details");
	assert_eq!(details["truncation"]["truncated"], json!(true));
	assert_eq!(details["truncation"]["truncatedBy"], json!("bytes"));
	assert_eq!(details["meta"]["truncation"]["direction"], json!("head"));
	assert_eq!(details["meta"]["truncation"]["shownRange"]["start"], json!(1));
}

#[tokio::test]
async fn buffered_multi_range_keeps_every_requested_line() {
	let dir = TempDir::new("multi-byte-budget");
	let filler = "y".repeat(696);
	let mut content = String::new();
	for index in 0..3500 {
		content.push_str(&format!("{index:04}-{filler}\n"));
	}
	dir.write("wide.txt", &content);
	let ctx = ctx(&dir);

	let result = read(&ctx, "wide.txt:1-1500,2001-3500").await;
	assert!(!result.is_error, "{}", result.content);
	assert!(!result.content.contains("byte budget"), "requested lines were dropped");
	// 1500 + 1500 requested lines, exact endpoints, and no gap rows.
	for expected in ["1|0000-", "1500|1499-", "2001|2000-", "3500|3499-"] {
		assert!(result.content.contains(expected), "missing {expected}");
	}
	for absent in ["1501|1500-", "2000|1999-"] {
		assert!(!result.content.contains(absent), "unrequested {absent} was displayed");
	}
	let rows = result.content.lines().filter(|line| line.contains('|')).count();
	assert_eq!(rows, 3000, "each requested line must be displayed");
}
