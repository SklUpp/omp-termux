//! Behavioral checks for the headless `read` / `grep` command bodies: modes,
//! context, limits, path resolution, and error exits.

use std::{
	fs,
	path::PathBuf,
	time::{SystemTime, UNIX_EPOCH},
};

use omp_cli::tool_commands::{cli_mode, run_grep, run_read, GrepMode, GrepRequest};

struct TempDir(PathBuf);

impl TempDir {
	fn new(tag: &str) -> Self {
		let nanos = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.expect("clock")
			.as_nanos();
		let path = std::env::temp_dir().join(format!("omp-tool-{tag}-{nanos}"));
		fs::create_dir_all(&path).expect("temp dir");
		Self(path)
	}

	fn write(&self, name: &str, contents: &str) {
		let path = self.0.join(name);
		if let Some(parent) = path.parent() {
			fs::create_dir_all(parent).expect("fixture dir");
		}
		fs::write(path, contents).expect("write fixture");
	}
}

impl Drop for TempDir {
	fn drop(&mut self) {
		let _ = fs::remove_dir_all(&self.0);
	}
}

fn request(pattern: &str, mode: GrepMode) -> GrepRequest {
	GrepRequest {
		pattern: pattern.to_string(),
		path: ".".to_string(),
		glob: None,
		limit: 20,
		context: 2,
		mode,
		gitignore: true,
	}
}

fn grep(dir: &TempDir, request: &GrepRequest) -> Result<String, String> {
	let mut out = Vec::new();
	run_grep(request, &dir.0, &mut out)?;
	Ok(String::from_utf8(out).expect("utf8 output"))
}

#[test]
fn content_mode_emits_context_and_totals() {
	let dir = TempDir::new("context");
	dir.write("f.txt", "before\nneedle\nafter\n");

	let request = GrepRequest { context: 1, ..request("needle", GrepMode::Content) };
	let output = grep(&dir, &request).expect("grep");

	assert!(output.contains("Mode: content, Limit: 20, Context: 1, Gitignore: true"));
	assert!(output.contains("Total matches: 1\nFiles with matches: 1\nFiles searched: 1"));
	assert!(output.contains("f.txt-1- before"));
	assert!(output.contains("f.txt:2: needle"));
	assert!(output.contains("f.txt-3- after"));
}

#[test]
fn count_wins_over_files_and_honors_gitignore_and_hidden() {
	assert_eq!(cli_mode(true, true), GrepMode::Count);
	assert_eq!(cli_mode(false, true), GrepMode::FilesWithMatches);
	assert_eq!(cli_mode(false, false), GrepMode::Content);

	let dir = TempDir::new("modes");
	dir.write("a.txt", "needle\nneedle\n");
	dir.write(".hidden.txt", "needle\n");
	dir.write("ignored.txt", "needle\n");
	dir.write(".git/HEAD", "ref: refs/heads/main\n");
	dir.write(".gitignore", "ignored.txt\n");

	let counted = request("needle", GrepMode::Count);
	let output = grep(&dir, &counted).expect("count");
	assert!(output.contains("Mode: count"), "{output}");
	assert!(output.contains("a.txt: 2 matches"), "{output}");
	assert!(output.contains(".hidden.txt: 1 matches"), "{output}");
	assert!(!output.contains("ignored.txt"), "{output}");

	let mut with_ignored = counted.clone();
	with_ignored.gitignore = false;
	let output = grep(&dir, &with_ignored).expect("no-gitignore");
	assert!(output.contains("ignored.txt: 1 matches"), "{output}");
}

#[test]
fn glob_files_mode_and_limit_reached() {
	let dir = TempDir::new("glob");
	dir.write("a.txt", "needle\n");
	dir.write("b.rs", "needle\n");

	let glob_request = GrepRequest {
		glob: Some("*.rs".to_string()),
		mode: GrepMode::FilesWithMatches,
		..request("needle", GrepMode::FilesWithMatches)
	};
	let output = grep(&dir, &glob_request).expect("glob files");
	assert!(output.contains("b.rs"), "{output}");
	assert!(!output.contains("a.txt"), "{output}");

	let limited = GrepRequest {
		path: dir.0.to_string_lossy().into_owned(),
		limit: 0,
		mode: GrepMode::FilesWithMatches,
		..request("needle", GrepMode::FilesWithMatches)
	};
	let output = grep(&dir, &limited).expect("zero limit");
	assert!(output.contains("Limit reached: true"), "{output}");
	assert!(!output.contains("a.txt"), "{output}");
	assert!(!output.contains("b.rs"), "{output}");
}

#[test]
fn matcher_fallbacks_match_native_contract() {
	let dir = TempDir::new("matcher");
	dir.write(
		"t.txt",
		"foo[bar\nfetch(\nfoobar\nfoooo\na{b}\n(1 fail)\nab{c}xxde\n",
	);

	// Unclosed class: literal fallback, not an error.
	let output = grep(&dir, &request("foo[bar", GrepMode::Content)).expect("literal class");
	assert!(output.contains("t.txt:1: foo[bar"), "{output}");

	// Unclosed group: escaped-parenthesis retry.
	let output = grep(&dir, &request("fetch(", GrepMode::Content)).expect("literal paren");
	assert!(output.contains("t.txt:2: fetch("), "{output}");

	// Unopened group: literal fallback.
	let output = grep(&dir, &request("fail)", GrepMode::Content)).expect("stray close");
	assert!(output.contains("t.txt:6: (1 fail)"), "{output}");

	// Non-quantifier braces are literal, not malformed repetitions.
	let output = grep(&dir, &request("a{b}", GrepMode::Content)).expect("literal braces");
	assert!(output.contains("t.txt:5: a{b}"), "{output}");

	// Lookahead needs PCRE2.
	let output = grep(&dir, &request("foo(?=bar)", GrepMode::Content)).expect("lookahead");
	assert!(output.contains("Total matches: 1"), "{output}");
	assert!(output.contains("t.txt:3: foobar"), "{output}");

	// Valid regex keeps regex semantics rather than becoming a literal.
	let output = grep(&dir, &request("fo+", GrepMode::Content)).expect("regex precedence");
	// `fo+` matches the "foo" prefix on lines 1 and 3, and all of line 4.
	assert!(output.contains("Total matches: 3"), "{output}");
	assert!(output.contains("t.txt:1: foo[bar"), "{output}");
	assert!(output.contains("t.txt:4: foooo"), "{output}");

	// Brace sanitization escapes the literal braces but keeps the rest a
	// regex; a pure literal fallback would miss this line.
	let output = grep(&dir, &request("ab{c}.*de", GrepMode::Content)).expect("brace regex");
	assert!(output.contains("Total matches: 1"), "{output}");
	assert!(output.contains("t.txt:7: ab{c}xxde"), "{output}");
}

#[test]
fn single_file_modes_and_glob_edges() {
	let dir = TempDir::new("edges");
	dir.write("three.txt", "needle\nneedle\nneedle\n");
	dir.write("many.txt", &"needle\n".repeat(25));

	// Native's single-file branch passes maxCount through for every mode.
	let files = GrepRequest {
		path: "three.txt".to_string(),
		mode: GrepMode::FilesWithMatches,
		..request("needle", GrepMode::FilesWithMatches)
	};
	let output = grep(&dir, &files).expect("single files");
	assert!(output.contains("Total matches: 3"), "{output}");
	assert!(!output.contains("Limit reached"), "{output}");

	let many_files = GrepRequest { path: "many.txt".to_string(), ..files.clone() };
	let output = grep(&dir, &many_files).expect("single files limited");
	assert!(output.contains("Total matches: 21"), "{output}");
	assert!(output.contains("Limit reached: true"), "{output}");

	let many_count = GrepRequest {
		path: "many.txt".to_string(),
		mode: GrepMode::Count,
		..request("needle", GrepMode::Count)
	};
	let output = grep(&dir, &many_count).expect("single count");
	assert!(output.contains("many.txt: 21 matches"), "{output}");

	// Zero limit on a direct file still emits the single captured row, matching
	// native's single-file branch (collector sees the following match too).
	let three_path = dir.0.join("three.txt").to_string_lossy().into_owned();
	let zero_content = GrepRequest {
		path: "three.txt".to_string(),
		limit: 0,
		mode: GrepMode::Content,
		..request("needle", GrepMode::Content)
	};
	let output = grep(&dir, &zero_content).expect("zero content");
	assert!(output.contains("Total matches: 2"), "{output}");
	assert!(output.contains(&format!("{three_path}:1: needle")), "{output}");
	assert!(output.contains("Limit reached: true"), "{output}");

	let zero_count = GrepRequest { mode: GrepMode::Count, ..zero_content.clone() };
	let output = grep(&dir, &zero_count).expect("zero count");
	assert!(output.contains(&format!("{three_path}: 2 matches")), "{output}");

	// Glob validation runs before the single-file branch.
	let bad_glob = GrepRequest {
		path: "three.txt".to_string(),
		glob: Some("[".to_string()),
		..request("needle", GrepMode::Content)
	};
	let error = grep(&dir, &bad_glob).expect_err("invalid glob");
	assert!(error.contains("Invalid glob pattern"), "{error}");

	// Exact brace unions stay root-level rather than becoming `**/{a,b}`.
	dir.write("a", "needle\n");
	dir.write("b", "needle\n");
	dir.write("sub/a", "needle\n");
	let union = GrepRequest {
		glob: Some("{a,b}".to_string()),
		mode: GrepMode::FilesWithMatches,
		..request("needle", GrepMode::FilesWithMatches)
	};
	let output = grep(&dir, &union).expect("brace union");
	assert!(output.contains("\na\nb\n"), "{output}");
	assert!(!output.contains("sub/a"), "{output}");

	// Oversized files are searched after normal files, not in path order.
	let mut big = vec![b'a'; 4 * 1024 * 1024 + 1];
	big.extend_from_slice(b"\nneedle\n");
	fs::write(dir.0.join("a_big.txt"), big).expect("big fixture");
	dir.write("z_small.txt", "needle\n");
	let order = GrepRequest {
		glob: Some("*.txt".to_string()),
		mode: GrepMode::FilesWithMatches,
		..request("needle", GrepMode::FilesWithMatches)
	};
	let output = grep(&dir, &order).expect("oversized order");
	let small = output.find("z_small.txt").expect("small row");
	let big = output.find("a_big.txt").expect("big row");
	assert!(small < big, "{output}");
}

#[test]
fn errors_are_reported_not_empty_results() {
	let dir = TempDir::new("errors");
	let missing_pattern = request("", GrepMode::Content);
	assert_eq!(grep(&dir, &missing_pattern), Err("Error: Pattern is required".to_string()));

	let missing_path = GrepRequest {
		path: "does-not-exist".to_string(),
		..request("needle", GrepMode::Content)
	};
	let error = grep(&dir, &missing_path).expect_err("missing path");
	assert!(error.contains("Path not found"), "{error}");
}

#[test]
fn read_selects_ranges_and_reports_unsupported_and_missing_paths() {
	let dir = TempDir::new("read");
	dir.write("lines.txt", "one\ntwo\nthree\n");
	dir.write("data.zip", "PK");

	let mut out = Vec::new();
	run_read("lines.txt:2-2", &dir.0, &mut out).expect("read range");
	let output = String::from_utf8(out).expect("utf8");
	assert!(output.contains("2|two"), "{output}");

	let mut out = Vec::new();
	let error = run_read("data.zip", &dir.0, &mut out).expect_err("archive unsupported");
	assert!(error.contains("not supported"), "{error}");

	let mut out = Vec::new();
	let error = run_read("missing.txt", &dir.0, &mut out).expect_err("missing");
	assert!(error.contains("Failed to read"), "{error}");

	let mut out = Vec::new();
	let error = run_read("", &dir.0, &mut out).expect_err("empty path");
	assert!(error.contains("path is required"), "{error}");
}
