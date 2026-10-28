//! Headless `omp read` / `omp grep` command bodies.
//!
//! `read` delegates to the same [`pi_agent::ReadTool`] the agent loop uses, so
//! selectors, caps, directory listings, and unsupported-format errors are the
//! tool's own. `grep` drives the same `grep-searcher`/`grep-regex` engine and
//! `pi-walker` traversal policy that `pi-natives`' N-API `grep` wraps; those
//! N-API entry points are not callable from Rust, so the engine is linked
//! directly instead of reaching through the installed JS CLI.
//!
//! Output mirrors `packages/coding-agent/src/cli/grep-cli.ts`: a header, result
//! totals, then content matches with context, per-file counts, or file names.
//! Broken stdout pipes end the command quietly; every other read/search/write
//! failure is reported by the caller as exit 1.

use std::{
	io::{self, Write},
	path::{Path, PathBuf},
};

use grep_searcher::{
	BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkContextKind, SinkMatch,
};
use pi_agent::Tool as _;
use pi_walker::{CompiledWalkGlob, FileCandidate, WalkFilter, WalkRequest};
use serde_json::json;

/// Native grep partitions files above this size behind normal files so a big
/// first path cannot consume the whole first-page budget. We stream every file
/// in full, but keep the same search order.
const OVERSIZED_FILE_BYTES: f64 = (4 * 1024 * 1024) as f64;

/// Search output mode; `count` wins when both CLI flags are present.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrepMode {
	Content,
	Count,
	FilesWithMatches,
}

impl GrepMode {
	const fn label(self) -> &'static str {
		match self {
			Self::Content => "content",
			Self::Count => "count",
			Self::FilesWithMatches => "filesWithMatches",
		}
	}
}

impl Default for GrepMode {
	fn default() -> Self {
		Self::Content
	}
}

/// Resolve `--count`/`--files`; count wins when both are present.
#[must_use]
pub const fn cli_mode(count: bool, files: bool) -> GrepMode {
	if count {
		GrepMode::Count
	} else if files {
		GrepMode::FilesWithMatches
	} else {
		GrepMode::Content
	}
}

/// Parsed `omp grep` request.
#[derive(Debug, Clone)]
pub struct GrepRequest {
	pub pattern:  String,
	pub path:     String,
	pub glob:     Option<String>,
	pub limit:    u32,
	pub context:  u32,
	pub mode:     GrepMode,
	pub gitignore: bool,
}

#[derive(Debug)]
struct ContextLine {
	line_number: u32,
	line:        String,
}

#[derive(Debug)]
struct MatchLine {
	line_number: u32,
	line:        String,
	before:      Vec<ContextLine>,
	after:       Vec<ContextLine>,
}

struct FileMatches {
	path:          String,
	matches:       Vec<MatchLine>,
	match_count:   u64,
	limit_reached: bool,
}

struct Collector {
	collect:       bool,
	max:           Option<u64>,
	match_count:   u64,
	collected:     u64,
	limit_reached: bool,
	matches:       Vec<MatchLine>,
	context:       Vec<ContextLine>,
}

impl Collector {
	fn new(collect: bool, max: Option<u64>) -> Self {
		Self {
			collect,
			max,
			match_count: 0,
			collected: 0,
			limit_reached: false,
			matches: Vec::new(),
			context: Vec::new(),
		}
	}
}

impl Sink for Collector {
	type Error = io::Error;

	fn matched(
		&mut self,
		_searcher: &Searcher,
		mat: &SinkMatch<'_>,
	) -> io::Result<bool> {
		self.match_count += 1;
		if self.limit_reached {
			return Ok(false);
		}
		if self.collect {
			self.matches.push(MatchLine {
				line_number: clamp_line(mat.line_number()),
				line: trimmed(mat.bytes()),
				before: std::mem::take(&mut self.context),
				after: Vec::new(),
			});
		} else {
			self.context.clear();
		}
		self.collected += 1;
		if let Some(max) = self.max
			&& self.collected >= max
		{
			self.limit_reached = true;
		}
		Ok(true)
	}

	fn context(
		&mut self,
		_searcher: &Searcher,
		ctx: &SinkContext<'_>,
	) -> io::Result<bool> {
		if !self.collect {
			return Ok(true);
		}
		let line = trimmed(ctx.bytes());
		match ctx.kind() {
			SinkContextKind::Before => self.context.push(ContextLine {
				line_number: clamp_line(ctx.line_number()),
				line,
			}),
			SinkContextKind::After => {
				if let Some(last) = self.matches.last_mut() {
					last.after.push(ContextLine {
						line_number: clamp_line(ctx.line_number()),
						line,
					});
				}
			},
			SinkContextKind::Other => {},
		}
		Ok(true)
	}
}

fn trimmed(bytes: &[u8]) -> String {
	match std::str::from_utf8(bytes) {
		Ok(text) => text.trim_end().to_string(),
		Err(_) => String::from_utf8_lossy(bytes).trim_end().to_string(),
	}
}

fn clamp_line(line: Option<u64>) -> u32 {
	line.unwrap_or(0).min(u64::from(u32::MAX)) as u32
}

/// Run `omp read` against `cwd`, writing the tool's exact text response.
pub fn run_read(
	path: &str,
	cwd: &Path,
	out: &mut dyn Write,
) -> Result<(), String> {
	if path.is_empty() {
		return Err("error: path is required".to_string());
	}
	let runtime = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.map_err(|err| format!("Failed to start runtime: {err}"))?;
	let ctx = pi_agent::AgentContext::new(cwd.to_path_buf());
	let result = runtime
		.block_on(pi_agent::ReadTool::new().execute(&json!({ "path": path }), &ctx))
		.map_err(|err| format!("Error: {err}"))?;
	if result.is_error {
		return Err(format!("Error: {}", result.content));
	}
	write_text(out, result.content.as_bytes())?;
	if !result.content.is_empty() && !result.content.ends_with('\n') {
		write_text(out, b"\n")?;
	}
	flush(out)
}

/// Run `omp grep`, writing the TS CLI's header/totals/matches layout.
pub fn run_grep(
	request: &GrepRequest,
	cwd: &Path,
	out: &mut dyn Write,
) -> Result<(), String> {
	if request.pattern.is_empty() {
		return Err("Error: Pattern is required".to_string());
	}

	let search_path = absolute_path(cwd, &request.path);
	let matcher = pi_grep_pattern::build_matcher(&request.pattern, false, false)
		.map_err(|message| format!("Error: {message}"))?;

	let metadata = std::fs::metadata(&search_path)
		.map_err(|err| format!("Error: Path not found: {err}"))?;
	let compiled_glob = compile_glob(request.glob.as_deref())?;

	write_text(
		out,
		format!(
			"Searching in: {}\nPattern: {}\nMode: {}, Limit: {}, Context: {}, Gitignore: {}\n\n",
			search_path.display(),
			request.pattern,
			request.mode.label(),
			request.limit,
			request.context,
			request.gitignore,
		)
		.as_bytes(),
	)?;

	// FIFOs, sockets, and devices are not searched; native grep reports zero
	// totals instead of an error (and never blocks opening the target).
	if !metadata.is_file() && !metadata.is_dir() {
		write_text(
			out,
			b"Total matches: 0\nFiles with matches: 0\nFiles searched: 0\n\n",
		)?;
		return flush(out);
	}

	let mut searcher = SearcherBuilder::new()
		.binary_detection(BinaryDetection::quit(b'\x00'))
		.line_number(true)
		.before_context(if request.mode == GrepMode::Content {
			request.context as usize
		} else {
			0
		})
		.after_context(if request.mode == GrepMode::Content {
			request.context as usize
		} else {
			0
		})
		.build();

	let single_file = metadata.is_file();
	let mut searched = 0u32;
	let mut results = Vec::new();

	if single_file {
		let display = search_path.to_string_lossy().into_owned();
		searched = 1;
		if let Some(result) = search_file(
			&mut searcher,
			&matcher,
			&display,
			&search_path,
			request,
			true,
		)
		.map_err(|err| {
			format!("Error: Failed to read {}: {err}", search_path.display())
		})? {
			results.push(result);
		}
	} else {
		let candidates = collect_candidates(&search_path, request, compiled_glob)?;
		for candidate in candidates {
			if let Ok(result) = search_file(
				&mut searcher,
				&matcher,
				&candidate.relative,
				&candidate.path,
				request,
				false,
			) {
				searched = searched.saturating_add(1);
				if let Some(result) = result {
					results.push(result);
				}
			}
			if request.mode == GrepMode::Content
				&& request.limit > 0
				&& emitted(&results) >= u64::from(request.limit)
			{
				break;
			}
		}
	}

	let limit = u64::from(request.limit);
	let mut total_matches = 0u64;
	let mut files_with_matches = 0u32;

	for result in &results {
		if result.match_count == 0 {
			continue;
		}
		files_with_matches = files_with_matches.saturating_add(1);
		total_matches = total_matches.saturating_add(result.match_count);
	}

	// Aggregate paging exactly like pi-natives' `aggregate_parallel_results`:
	// content counts emitted lines, count counts emitted matches, files counts
	// emitted rows.
	let mut emitted = 0u64;
	let mut limit_reached = false;
	match request.mode {
		GrepMode::Content => {
			for result in &results {
				for _matched in &result.matches {
					if emitted >= limit {
						limit_reached = true;
						break;
					}
					emitted += 1;
				}
				if emitted >= limit {
					limit_reached = true;
				}
			}
		},
		GrepMode::Count => {
			for result in &results {
				if emitted >= limit {
					limit_reached = true;
					continue;
				}
				let remaining = limit.saturating_sub(emitted);
				let selected = result.match_count.min(remaining);
				emitted = emitted.saturating_add(selected);
				if selected < result.match_count {
					limit_reached = true;
				}
			}
		},
		GrepMode::FilesWithMatches => {
			for _result in &results {
				if emitted >= limit {
					limit_reached = true;
					continue;
				}
				emitted += 1;
			}
		},
	}
	if emitted >= limit {
		limit_reached = true;
	}
	if single_file && results.iter().any(|result| result.limit_reached) {
		limit_reached = true;
	}
	if request.limit == 0 {
		limit_reached = files_with_matches > 0;
	}

	write_text(
		out,
		format!(
			"Total matches: {total_matches}\nFiles with matches: {files_with_matches}\nFiles searched: {searched}\n"
		)
		.as_bytes(),
	)?;
	if limit_reached {
		write_text(out, b"Limit reached: true\n")?;
	}
	write_text(out, b"\n")?;

	let mut written = 0u64;
	// Native's single-file branch bypasses aggregate paging: `-l 0` still emits
	// the one row its collector captured, while walk mode emits nothing.
	let write_limit = if single_file { limit.max(1) } else { limit };
	match request.mode {
		GrepMode::Content => {
			for result in &results {
				for matched in &result.matches {
					if written >= write_limit {
						break;
					}
					write_content_match(out, &result.path, matched)?;
					written += 1;
				}
				if written >= write_limit {
					break;
				}
			}
		},
		GrepMode::Count => {
			for result in &results {
				if !single_file && written >= limit {
					break;
				}
				write_text(
					out,
					format!("{}: {} matches\n", display_path(&result.path), result.match_count)
						.as_bytes(),
				)?;
				if !single_file {
					let remaining = limit.saturating_sub(written);
					written = written.saturating_add(result.match_count.min(remaining));
				}
			}
		},
		GrepMode::FilesWithMatches => {
			for result in &results {
				if !single_file && written >= limit {
					break;
				}
				write_text(out, format!("{}\n", display_path(&result.path)).as_bytes())?;
				written += 1;
			}
		},
	}
	flush(out)
}

fn emitted(results: &[FileMatches]) -> u64 {
	results.iter().map(|result| result.match_count).sum()
}

fn write_content_match(
	out: &mut dyn Write,
	path: &str,
	matched: &MatchLine,
) -> Result<(), String> {
	let path = display_path(path);
	for context in &matched.before {
		write_text(
			out,
			format!("{path}-{}- {}\n", context.line_number, context.line).as_bytes(),
		)?;
	}
	write_text(
		out,
		format!("{path}:{}: {}\n", matched.line_number, matched.line).as_bytes(),
	)?;
	for context in &matched.after {
		write_text(
			out,
			format!("{path}-{}- {}\n", context.line_number, context.line).as_bytes(),
		)?;
	}
	write_text(out, b"\n")
}

fn display_path(path: &str) -> String {
	path.replace('\\', "/")
}

fn search_file(
	searcher: &mut Searcher,
	matcher: &pi_grep_pattern::CompiledMatcher,
	display_path: &str,
	fs_path: &Path,
	request: &GrepRequest,
	single_file: bool,
) -> io::Result<Option<FileMatches>> {
	let file_limit = Some(u64::from(request.limit));
	// A direct file is searched through native's single-file branch, which
	// passes `maxCount` to every mode. Walk mode keeps per-file limits: count
	// is unbounded and files-with-matches stops at one.
	let (collect, max) = match (request.mode, single_file) {
		(GrepMode::Content, _) => (true, file_limit),
		(GrepMode::Count, true) => (false, file_limit),
		(GrepMode::Count, false) => (false, None),
		(GrepMode::FilesWithMatches, true) => (false, file_limit),
		(GrepMode::FilesWithMatches, false) => (false, Some(1)),
	};
	let mut collector = Collector::new(collect, max);
	searcher.search_path(matcher, fs_path, &mut collector)?;
	if collector.match_count == 0 {
		return Ok(None);
	}
	// Walk files mode counts one row per matching file, not matching lines
	// (`search_file_bytes` short-circuits with `is_match` upstream). The
	// single-file branch reports native's full match_count in every mode.
	let match_count = if request.mode == GrepMode::FilesWithMatches && !single_file {
		1
	} else {
		collector.match_count
	};
	Ok(Some(FileMatches {
		path: display_path.to_string(),
		matches: collector.matches,
		match_count,
		limit_reached: collector.limit_reached,
	}))
}

fn collect_candidates(
	root: &Path,
	request: &GrepRequest,
	compiled_glob: Option<CompiledWalkGlob>,
) -> Result<Vec<FileCandidate>, String> {
	let mut filter = WalkFilter::files_only();
	let mentions_node_modules = request
		.glob
		.as_deref()
		.is_some_and(|glob| glob.contains("node_modules"));
	if let Some(compiled) = compiled_glob {
		filter = filter.glob(compiled);
	}
	let request = WalkRequest::new(root)
		.hidden(true)
		.gitignore(request.gitignore)
		.skip_git(true)
		.skip_node_modules(!mentions_node_modules)
		.follow_links(pi_walker::FollowLinks::Never)
		.detail(pi_walker::WalkDetail::Minimal)
		.size_hints(pi_walker::SizeHintPolicy::WhenCheap)
		.order(pi_walker::WalkOrder::Path)
		.emit_root(false)
		.depth(1, usize::MAX)
		.directory_errors(pi_walker::DirectoryErrorMode::SkipSkippable)
		.cache(false)
		.filter(filter);
	let candidates = request
		.collect_file_candidates()
		.map_err(|err| format!("Error: Walk error: {err}"))?;
	// `WhenCheap` may omit sizes (e.g. fresh large files); stat only those so
	// oversized files still sort after normal ones.
	let mut classified: Vec<(bool, FileCandidate)> = candidates
		.into_iter()
		.map(|candidate| {
			let size = candidate.size.or_else(|| {
				std::fs::metadata(&candidate.path).ok().map(|metadata| metadata.len() as f64)
			});
			(size.is_some_and(|size| size > OVERSIZED_FILE_BYTES), candidate)
		})
		.collect();
	classified.sort_by(|(left_oversized, left), (right_oversized, right)| {
		left_oversized
			.cmp(right_oversized)
			.then_with(|| left.relative.cmp(&right.relative))
	});
	Ok(classified.into_iter().map(|(_, candidate)| candidate).collect())
}

fn compile_glob(glob: Option<&str>) -> Result<Option<CompiledWalkGlob>, String> {
	let Some(glob) = glob.map(str::trim).filter(|glob| !glob.is_empty()) else {
		return Ok(None);
	};
	CompiledWalkGlob::new([build_glob_pattern(glob)])
		.map(Some)
		.map_err(|err| format!("Error: Invalid glob pattern: {err}"))
}

/// Exact root-level `{a,b}` union: native keeps these non-recursive instead of
/// prefixing `**/`.
fn is_exact_brace_union(pattern: &str) -> bool {
	if !(pattern.starts_with('{') && pattern.ends_with('}')) {
		return false;
	}
	let inner = &pattern[1..pattern.len() - 1];
	!inner.is_empty()
		&& !inner
			.chars()
			.any(|ch| matches!(ch, '*' | '?' | '[' | ']' | '{' | '}'))
}

/// `pi-natives`' glob builder: normalize separators, prefix simple patterns
/// with `**/`, and close unclosed brace unions.
fn build_glob_pattern(glob: &str) -> String {
	let normalized = glob.replace('\\', "/");
	let mut pattern = if !normalized.contains('/')
		&& !normalized.starts_with("**")
		&& !is_exact_brace_union(&normalized)
	{
		format!("**/{normalized}")
	} else {
		normalized
	};
	let opens = pattern.chars().filter(|&ch| ch == '{').count();
	let closes = pattern.chars().filter(|&ch| ch == '}').count();
	for _ in closes..opens {
		pattern.push('}');
	}
	pattern
}

/// Resolve a CLI path against `cwd`, matching `expandPath` + `path.resolve`:
/// strip a stray leading colon, an `@` prefix, and expand `~`.
fn absolute_path(cwd: &Path, raw: &str) -> PathBuf {
	let trimmed = raw.trim();
	let de_coloned = match trimmed.strip_prefix(':') {
		Some(rest)
			if rest.starts_with('/')
				|| rest.starts_with('\\')
				|| rest.starts_with('~')
				|| rest.starts_with("./")
				|| rest.starts_with("../")
				|| rest.starts_with(".\\")
				|| rest.starts_with("..\\") =>
		{
			rest
		},
		_ => trimmed,
	};
	let raw = de_coloned.strip_prefix('@').unwrap_or(de_coloned);
	let expanded = if let Some(rest) = raw.strip_prefix("~/") {
		match std::env::var_os("HOME") {
			Some(home) => PathBuf::from(home).join(rest),
			None => PathBuf::from(raw),
		}
	} else if raw == "~" {
		std::env::var_os("HOME").map_or_else(|| PathBuf::from(raw), PathBuf::from)
	} else {
		PathBuf::from(raw)
	};
	let joined = if expanded.is_absolute() {
		expanded
	} else {
		cwd.join(expanded)
	};
	joined.components().collect()
}

fn write_text(out: &mut dyn Write, bytes: &[u8]) -> Result<(), String> {
	match out.write_all(bytes) {
		Ok(()) => Ok(()),
		Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(()),
		Err(err) => Err(format!("Failed to write output: {err}")),
	}
}

fn flush(out: &mut dyn Write) -> Result<(), String> {
	match out.flush() {
		Ok(()) => Ok(()),
		Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(()),
		Err(err) => Err(format!("Failed to write output: {err}")),
	}
}
