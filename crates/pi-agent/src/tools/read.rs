//! The `read` tool: local text files (selectors, ranges, caps), directory
//! listings, and honest errors for the source kinds that are not ported yet.
//!
//! Plain-file reads mirror `packages/coding-agent/src/tools/read.ts` /
//! `read-selector.ts` / `path-utils.ts` / `read-format.ts`, including the
//! trailing-selector grammar, literal-colon filenames, context expansion,
//! line numbering, and 3000-line / 50 KiB / 768-column caps.
//!
//! Remaining read slices (planned, not yet ported): archives, SQLite,
//! markit documents, images/`:img`, and the internal-URL router. `:conflicts`
//! is blocked on the conflict-history store that
//! `write({path:"conflict://<N>"})` resolves against; until that store lands
//! the selector errors rather than listing ids nothing can act on.
//!
//! Hashline display mode is injected, not implemented here: a
//! [`ReadSnapshotSink`] (the hashline module owns the concrete store) records
//! the whole normalized file plus the displayed line numbers and returns the
//! `[path#TAG]` header. With no sink the tool emits plain `N|TEXT` bodies and
//! never fabricates a header.
//!
//! Cancellation is checked at entry and between scanner chunks for Rust
//! interrupt safety. TS deliberately makes deterministic local reads
//! non-abortable (`read-fs-not-abortable.test.ts`); keeping these checks is an
//! intentional Rust divergence, not an unnoticed gap.

use std::{
	io,
	path::{Path, PathBuf},
	sync::Arc,
	time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use pi_ast::summary::{SummaryOptions, SummaryResult, summarize_code};
use pi_walker::{CompiledWalkGlob, WalkFilter, WalkRequest};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::sync::CancellationToken;

use super::{Tool, ToolError, tool_result};
use crate::state::{AgentContext, ToolResult};

/// Shared read caps (`packages/coding-agent/src/session/streaming-output.ts`).
const DEFAULT_MAX_LINES: u64 = 3000;
const DEFAULT_MAX_BYTES: u64 = 50 * 1024;
/// Open-ended reads stop after this many lines (`read.defaultLimit`).
const DEFAULT_READ_LIMIT: u64 = 300;
/// Per-line display cap (`tools.outputMaxColumns` default).
///
/// ponytail: no settings seam yet; wire this to config when the settings
/// store lands.
const MAX_COLUMNS: usize = 768;
/// Files at or below this size are scanned to EOF so `totalLines` is exact.
const SNAPSHOT_MAX_BYTES: u64 = 4 * 1024 * 1024;
const READ_CHUNK_SIZE: usize = 8 * 1024;
const RANGE_LEADING_CONTEXT: u64 = 1;
const RANGE_TRAILING_CONTEXT: u64 = 3;
const BINARY_SNIFF_BYTES: usize = 8 * 1024;
/// Structural summary caps (`tools/read-summary.ts`).
const MAX_SUMMARY_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SUMMARY_LINES: u64 = 20_000;
/// `read.summarize.minTotalLines` default.
const SUMMARY_MIN_TOTAL_LINES: u64 = 100;
/// `read.summarize.minBodyLines` / `minCommentLines` defaults.
const SUMMARY_MIN_BODY_LINES: u32 = 4;
const SUMMARY_MIN_COMMENT_LINES: u32 = 6;
/// `read.summarize.unfoldUntil` / `unfoldLimit` defaults.
const SUMMARY_UNFOLD_UNTIL: u32 = 50;
const SUMMARY_UNFOLD_LIMIT: u32 = 100;
/// Footer shows at most this many elided ranges as re-read examples.
const FOOTER_RANGE_SAMPLES: usize = 2;
/// Unique-suffix workspace scan budget (`WORKSPACE_SUFFIX_TIMEOUT_MS`).
const SUFFIX_SCAN_TIMEOUT: Duration = Duration::from_secs(5);

/// Linux/Android `O_NONBLOCK`; opening a raced-in FIFO must not hang.
#[cfg(any(target_os = "linux", target_os = "android"))]
const O_NONBLOCK: i32 = 0o4000;

const CONVERTIBLE_EXTENSIONS: &[&str] =
	&[".pdf", ".doc", ".docx", ".ppt", ".pptx", ".xls", ".xlsx", ".rtf", ".epub"];
const IMAGE_EXTENSIONS: &[&str] = &[
	".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".ico", ".tif", ".tiff", ".avif", ".heic", ".svgz",
];
const SQLITE_EXTENSIONS: &[&str] = &[".sqlite", ".sqlite3", ".db", ".db3"];
const ARCHIVE_SUFFIXES: &[&str] = &[
	".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst", ".tgz", ".zip", ".jar", ".war", ".ear", ".apk", ".whl", ".tar",
	".rar", ".7z", ".iso", ".cab", ".deb", ".rpm", ".cpio", ".ar", ".a", ".lzh", ".arj", ".asar", ".gz", ".bz2",
	".xz", ".zst",
];

/// Hashline snapshot seam. The hashline module owns the concrete store; this
/// trait only lets `read` record what it displayed. Implementations receive the
/// absolute display path and canonicalize it themselves.
pub trait ReadSnapshotSink: Send + Sync {
	/// Whole file text (BOM stripped, LF-normalized), at most the 4 MiB
	/// snapshot cap. Returns the 4-hex uppercase tag, or `None` when the file
	/// cannot be snapshotted.
	fn record_file(&self, absolute_path: &Path, full_text: &str) -> Option<String>;

	/// Source line numbers (1-indexed) whose text this read displayed.
	fn record_seen_lines(&self, absolute_path: &Path, tag: &str, lines: &[u32]);

	/// Exact `[path#TAG]` header line, no trailing newline.
	fn header(&self, absolute_path: &Path, tag: &str, cwd: &Path) -> String;
}

/// Builtin file reading tool.
pub struct ReadTool {
	sink: Option<Arc<dyn ReadSnapshotSink>>,
}

impl Default for ReadTool {
	fn default() -> Self {
		Self::new()
	}
}

impl ReadTool {
	pub fn new() -> Self {
		Self { sink: None }
	}

	/// Read with hashline headers and snapshot recording enabled.
	pub fn with_snapshot_sink(sink: Arc<dyn ReadSnapshotSink>) -> Self {
		Self { sink: Some(sink) }
	}
}

#[async_trait]
impl Tool for ReadTool {
	fn name(&self) -> &str {
		"read"
	}

	fn description(&self) -> &str {
		"Read a file, directory listing, or a line range with an optional :selector suffix"
	}

	fn schema(&self) -> Value {
		json!({
			"type": "object",
			"properties": {
				"path": {
					"type": "string",
					"description": "Path to read, optionally ending in a selector such as :50-100, :raw, or :raw:50-100"
				}
			},
			"required": ["path"]
		})
	}

	async fn execute(&self, args: &Value, ctx: &AgentContext) -> Result<ToolResult, ToolError> {
		let raw_path = args
			.get("path")
			.and_then(Value::as_str)
			.ok_or_else(|| ToolError::InvalidArguments("missing 'path' parameter".to_string()))?;
		if raw_path.is_empty() {
			return Ok(tool_result("Path must not be empty", true));
		}
		// Resolution devices are virtual: `read xd://resolve` returns their
		// one-line usage instead of probing a filesystem path.
		match super::pending::parse_xd_target(raw_path) {
			Some(super::pending::XdTarget::Device(device)) => {
				return match super::pending::resolution_device_usage(device) {
					Some(usage) => Ok(tool_result(usage, false)),
					None => Err(ToolError::InvalidArguments(format!(
						"Unknown xd:// device '{device}'. Mounted devices: xd://resolve, xd://reject."
					))),
				};
			},
			Some(super::pending::XdTarget::Invalid) => {
				return Err(ToolError::InvalidArguments(format!(
					"Invalid xd:// URL: '{raw_path}'. Use xd://resolve or xd://reject."
				)));
			},
			None => {},
		}
		if is_cancelled(ctx) {
			return Ok(tool_result("Read cancelled", true));
		}

		// `~` expands before selector peeling, so `~/notes:12` reads the file.
		let raw_path = expand_tilde(raw_path, ctx);

		// Literal filesystem names win over selector interpretation when the
		// whole path exists (or the probe is inconclusive), per
		// `splitPathAndSelPreferringLiteral`.
		let (path_part, sel_part) = match split_path_and_sel(&raw_path) {
			Some(strict) => {
				let literal = resolve_path(ctx, &raw_path);
				match probe_literal(&literal).await {
					LiteralState::Missing => (strict.path, Some(strict.sel)),
					LiteralState::Exists | LiteralState::Unknown => (raw_path.clone(), None),
				}
			},
			None => (raw_path.clone(), None),
		};

		let parsed = match parse_sel(sel_part.as_deref()) {
			Ok(parsed) => parsed,
			Err(message) => return Ok(tool_result(message, true)),
		};

		let mut target = resolve_path(ctx, &path_part);

		// An archive member (`archive.ext:inner`) never stats as one path.
		if let Some(archive) = existing_archive_prefix(&raw_path, ctx) {
			return Ok(unsupported(
				format!("Archive reads are not supported in this build: '{archive}'. Extract the member with a shell tool."),
				&target,
			));
		}

		let mut suffix_resolution: Option<(String, String)> = None;
		let meta = match tokio::fs::metadata(&target).await {
			Ok(meta) => meta,
			Err(err) => {
				if let Some(scheme) = internal_scheme(&raw_path) {
					return Ok(unsupported(
						format!(
							"Internal resource reads are not supported in this build: '{scheme}://'. Use a local filesystem path."
						),
						&target,
					));
				}
				let recovered = if is_not_found(&err) {
					match find_unique_workspace_suffix(&path_part, ctx).await {
						Some((candidate, display)) => match tokio::fs::metadata(&candidate).await {
							Ok(candidate_meta) => {
								target = candidate;
								suffix_resolution = Some((path_part.clone(), display));
								Some(candidate_meta)
							},
							Err(_) => None,
						},
						None => None,
					}
				} else {
					None
				};
				match recovered {
					Some(meta) => meta,
					None => {
						return Ok(tool_result(format!("Failed to read {}: {err}", target.display()), true));
					},
				}
			},
		};

		if meta.is_dir() {
			let mut result = read_directory(&target, &parsed, ctx).await;
			attach_suffix_details(&mut result.details, suffix_resolution.as_ref());
			return Ok(result);
		}
		if !meta.is_file() {
			return Ok(tool_result(format!("{} is not a regular file", target.display()), true));
		}

		if let Some(kind) = unsupported_extension(&target) {
			return Ok(unsupported(
				format!("{kind} reads are not supported in this build: '{}'.", target.display()),
				&target,
			));
		}
		if matches!(parsed, Sel::Image) {
			return Ok(unsupported(
				format!("Image/SVG rendering is not supported in this build: '{}'.", target.display()),
				&target,
			));
		}
		if matches!(parsed, Sel::Conflicts) {
			return Ok(unsupported(
				format!(
					"Conflict indexing is not supported in this build: '{}'. Inspect merge markers with the read tool directly.",
					target.display()
				),
				&target,
			));
		}

		let image_ext = has_extension(&target, IMAGE_EXTENSIONS);
		let raw_selector = is_raw(&parsed);

		let mut file = match open_regular(&target).await {
			Ok(file) => file,
			Err(err) => {
				return Ok(tool_result(format!("Failed to read {}: {err}", target.display()), true));
			},
		};
		let mut head = vec![0u8; BINARY_SNIFF_BYTES];
		let read = match file.read(&mut head).await {
			Ok(read) => read,
			Err(err) => return Ok(tool_result(format!("Failed to read {}: {err}", target.display()), true)),
		};
		head.truncate(read);
		if file.seek(io::SeekFrom::Start(0)).await.is_err() {
			return Ok(tool_result(format!("Failed to read {}: seek failed", target.display()), true));
		}
		drop(file);

		if has_extension(&target, SQLITE_EXTENSIONS) && head.starts_with(b"SQLite format 3\0") {
			return Ok(unsupported(
				format!(
					"SQLite reads are not supported in this build: '{}'. Query it with the sqlite3 CLI.",
					target.display()
				),
				&target,
			));
		}
		if image_ext {
			return Ok(unsupported(
				format!(
					"Image reads are not supported in this build: '{}'. Use inspect_image or a shell tool.",
					target.display()
				),
				&target,
			));
		}
		if !raw_selector && looks_binary(&head) {
			let details = json!({
				"resolvedPath": target.display().to_string(),
				"fileSize": meta.len(),
				"meta": { "source": { "type": "path", "value": target.display().to_string() } }
			});
			let mut result = result_with_details(
				prepend_suffix_notice(
					format!(
						"[Cannot read binary file '{}' ({}); not valid UTF-8 text. Use ':raw' to read bytes verbatim.]",
						target.display(),
						format_bytes(meta.len())
					),
					suffix_resolution.as_ref(),
				),
				details,
				false,
			);
			attach_suffix_details(&mut result.details, suffix_resolution.as_ref());
			return Ok(result);
		}

		let sink = self.sink.as_deref();
		if matches!(parsed, Sel::None) && !is_prose_summary_path(&target) {
			if let Some(summary) = try_summarize(&target, meta.len(), ctx).await {
				return Ok(
					read_summary_result(&target, meta.len(), &summary, sink, suffix_resolution.as_ref(), ctx)
						.await,
				);
			}
		}
		let outcome = match &parsed {
			Sel::Lines { ranges, raw } if ranges.len() > 1 => {
				read_multi_range(&target, meta.len(), ranges, *raw, sink, ctx).await
			},
			Sel::Lines { ranges, raw } => {
				let range = ranges[0];
				read_single_range(&target, meta.len(), range, *raw, sink, ctx).await
			},
			Sel::Raw | Sel::None => {
				read_single_range(
					&target,
					meta.len(),
					LineRange { start_line: 1, end_line: None },
					raw_selector,
					sink,
					ctx,
				)
				.await
			},
			Sel::Conflicts | Sel::Image => unreachable!("handled above"),
		};

		match outcome {
			Ok(mut result) => {
				result.content = prepend_suffix_notice(result.content, suffix_resolution.as_ref());
				attach_suffix_details(&mut result.details, suffix_resolution.as_ref());
				Ok(result)
			},
			Err(message) => Ok(tool_result(message, true)),
		}
	}
}

// ---------------------------------------------------------------------------
// Selector grammar
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LineRange {
	start_line: u64,
	/// `None` means open-ended (`:N-`).
	end_line: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Sel {
	None,
	Raw,
	Conflicts,
	Image,
	Lines { ranges: Vec<LineRange>, raw: bool },
}

fn is_raw(sel: &Sel) -> bool {
	matches!(sel, Sel::Raw) || matches!(sel, Sel::Lines { raw: true, .. })
}

fn invalid_selector(sel: &str) -> String {
	format!(
		"Invalid selector ':{sel}'. Use :N, :N-M, :N+K, :N- (open-ended), a comma-separated list of ranges, :raw, :img for SVG rendering, or a range combined with raw (e.g. :raw:50-100)."
	)
}

fn parse_sel(sel: Option<&str>) -> Result<Sel, String> {
	let Some(sel) = sel.filter(|sel| !sel.is_empty()) else {
		return Ok(Sel::None);
	};

	if sel.contains(':') {
		let chunks: Vec<&str> = sel.split(':').collect();
		if chunks.len() == 2 {
			let (a, b) = (chunks[0], chunks[1]);
			let a_raw = a.eq_ignore_ascii_case("raw");
			let b_raw = b.eq_ignore_ascii_case("raw");
			let range_chunk = if a_raw {
				Some(b)
			} else if b_raw {
				Some(a)
			} else {
				None
			};
			let raw_chunk = if a_raw {
				Some(a)
			} else if b_raw {
				Some(b)
			} else {
				None
			};
			if let (Some(range_chunk), Some(_)) = (range_chunk, raw_chunk) {
				if let Some(ranges) = parse_line_ranges(range_chunk)? {
					return Ok(Sel::Lines { ranges, raw: true });
				}
			}
		}
		let mut all_read_like = true;
		for chunk in &chunks {
			match chunk_looks_read_like(chunk) {
				Ok(true) => {},
				Ok(false) => {
					all_read_like = false;
					break;
				},
				Err(message) => return Err(message),
			}
		}
		if all_read_like {
			return Err(invalid_selector(sel));
		}
		// Unrecognized compound: sqlite/archive/url readers own their colon syntax.
		return Ok(Sel::None);
	}

	if sel.eq_ignore_ascii_case("raw") {
		return Ok(Sel::Raw);
	}
	if sel.eq_ignore_ascii_case("conflicts") {
		return Ok(Sel::Conflicts);
	}
	if sel.eq_ignore_ascii_case("img") {
		return Ok(Sel::Image);
	}
	match parse_line_ranges(sel)? {
		Some(ranges) => Ok(Sel::Lines { ranges, raw: false }),
		None => Ok(Sel::None),
	}
}

fn chunk_looks_read_like(chunk: &str) -> Result<bool, String> {
	let lower = chunk.to_ascii_lowercase();
	if lower == "raw" || lower == "conflicts" || lower == "img" {
		return Ok(true);
	}
	// Malformed open bounds (`-5`, `-5+2`) are selector-shaped, not ranges.
	if lower.starts_with('-') && lower[1..].chars().all(|c| c.is_ascii_digit() || c == '-' || c == '+') {
		return Ok(true);
	}
	Ok(parse_line_ranges(chunk)?.is_some())
}

fn parse_line_ranges(sel: &str) -> Result<Option<Vec<LineRange>>, String> {
	let mut parsed = Vec::new();
	for chunk in sel.split(',') {
		match parse_line_range_chunk(chunk)? {
			Some(range) => parsed.push(range),
			None => return Ok(None),
		}
	}
	if parsed.is_empty() {
		return Ok(None);
	}
	parsed.sort_by_key(|range| range.start_line);
	let mut merged: Vec<LineRange> = Vec::with_capacity(parsed.len());
	for current in parsed {
		match merged.last_mut() {
			None => merged.push(current),
			Some(last) => {
				let Some(last_end) = last.end_line else {
					continue; // open-ended absorbs everything after it
				};
				if current.start_line <= last_end.saturating_add(1) {
					if current.end_line.is_none() || current.end_line > Some(last_end) {
						last.end_line = current.end_line;
					}
				} else {
					merged.push(current);
				}
			},
		}
	}
	Ok(Some(merged))
}

fn parse_line_range_chunk(chunk: &str) -> Result<Option<LineRange>, String> {
	let stripped = chunk.strip_prefix('L').or_else(|| chunk.strip_prefix('l')).unwrap_or(chunk);
	let digits = stripped.find(|c: char| !c.is_ascii_digit()).unwrap_or(stripped.len());
	if digits == 0 {
		return Ok(None);
	}
	let Ok(start) = stripped[..digits].parse::<u64>() else {
		return Ok(None);
	};
	if start < 1 {
		return Err("Line selector 0 is invalid; lines are 1-indexed. Use :1.".to_string());
	}
	let rest = &stripped[digits..];
	let (separator, rhs) = if let Some(rhs) = rest.strip_prefix("..") {
		(Some(".."), rhs)
	} else if let Some(rhs) = rest.strip_prefix('-') {
		(Some("-"), rhs)
	} else if let Some(rhs) = rest.strip_prefix('+') {
		(Some("+"), rhs)
	} else if rest.is_empty() {
		(None, "")
	} else {
		return Ok(None);
	};

	let rhs = rhs.strip_prefix('L').or_else(|| rhs.strip_prefix('l')).unwrap_or(rhs);
	if !rhs.is_empty() && !rhs.chars().all(|c| c.is_ascii_digit()) {
		return Ok(None);
	}
	let rhs_number = if rhs.is_empty() { None } else { rhs.parse::<u64>().ok() };

	let end_line = match separator {
		None => None,
		Some("+") => {
			// The candidate grammar never peels a bare `N+`, so a missing count
			// here is a malformed literal path, not a selector.
			let Some(count) = rhs_number else {
				return Ok(None);
			};
			if count < 1 {
				return Err(format!("Invalid range {start}+0: count must be >= 1."));
			}
			start.checked_add(count - 1)
		},
		Some(_) => match rhs_number {
			None => None,
			Some(end) => {
				if end < start {
					return Err(format!("Invalid range {start}-{end}: end must be >= start."));
				}
				Some(end)
			},
		},
	};
	Ok(Some(LineRange { start_line: start, end_line }))
}

/// Permissive validation used only while splitting a path from its selector.
fn is_selector_tail(chunk: &str) -> bool {
	chunk.eq_ignore_ascii_case("raw")
		|| chunk.eq_ignore_ascii_case("conflicts")
		|| chunk.eq_ignore_ascii_case("img")
		|| is_range_list(chunk)
}

fn is_range_list(chunk: &str) -> bool {
	if chunk.is_empty() {
		return false;
	}
	chunk.split(',').all(is_range_candidate)
}

fn is_range_candidate(chunk: &str) -> bool {
	let stripped = chunk.strip_prefix('L').or_else(|| chunk.strip_prefix('l')).unwrap_or(chunk);
	let digits = stripped.find(|c: char| !c.is_ascii_digit()).unwrap_or(stripped.len());
	if digits == 0 {
		return false;
	}
	let rest = &stripped[digits..];
	if rest.is_empty() || rest == "-" || rest == ".." {
		return true;
	}
	let rhs = if let Some(rhs) = rest.strip_prefix("..") {
		rhs
	} else if let Some(rhs) = rest.strip_prefix('-') {
		rhs
	} else if let Some(rhs) = rest.strip_prefix('+') {
		rhs
	} else {
		return false;
	};
	let rhs = rhs.strip_prefix('L').or_else(|| rhs.strip_prefix('l')).unwrap_or(rhs);
	!rhs.is_empty() && rhs.chars().all(|c| c.is_ascii_digit())
}

struct StrictSplit {
	path: String,
	sel: String,
}

fn split_path_and_sel(raw_path: &str) -> Option<StrictSplit> {
	let colon = raw_path.rfind(':')?;
	if colon == 0 {
		return None;
	}
	let candidate = &raw_path[colon + 1..];
	if !is_selector_tail(candidate) {
		return None;
	}

	let mut base = &raw_path[..colon];
	let mut sel = candidate.to_string();
	if let Some(inner_colon) = base.rfind(':') {
		if inner_colon > 0 {
			let inner = &base[inner_colon + 1..];
			let inner_raw = inner.eq_ignore_ascii_case("raw");
			let outer_raw = candidate.eq_ignore_ascii_case("raw");
			let inner_range = is_range_list(inner);
			let outer_range = is_range_list(candidate);
			if (inner_raw && outer_range) || (inner_range && outer_raw) {
				sel = format!("{inner}:{candidate}");
				base = &base[..inner_colon];
			}
		}
	}
	Some(StrictSplit { path: base.to_string(), sel })
}

// ---------------------------------------------------------------------------
// Path handling and source walls
// ---------------------------------------------------------------------------

fn resolve_path(ctx: &AgentContext, raw: &str) -> PathBuf {
	// A bare `/` means the session cwd, not the filesystem root.
	if raw == "/" {
		return ctx.cwd.clone();
	}
	let path = Path::new(raw);
	if path.is_absolute() {
		path.to_path_buf()
	} else {
		ctx.cwd.join(path)
	}
}

/// `expandTilde` from `path-utils.ts`: `~` and `~/...` map to `$HOME`, and a
/// `~suffix` form joins onto home. Without a home directory the input is kept.
fn expand_tilde(raw: &str, ctx: &AgentContext) -> String {
	let Some(home) = ctx.env.get("HOME").filter(|home| !home.is_empty()) else {
		return raw.to_string();
	};
	if raw == "~" {
		return home.clone();
	}
	if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
		return format!("{home}/{rest}");
	}
	if let Some(rest) = raw.strip_prefix('~') {
		return Path::new(home).join(rest).to_string_lossy().into_owned();
	}
	raw.to_string()
}

fn is_not_found(err: &io::Error) -> bool {
	matches!(err.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory)
}

/// `findUniqueWorkspaceSuffix` normalization: `/` separators, one leading
/// `./`, no trailing slashes.
fn normalize_suffix(raw: &str) -> String {
	let mut normalized = raw.replace('\\', "/");
	if let Some(rest) = normalized.strip_prefix("./") {
		normalized = rest.to_string();
	}
	while normalized.ends_with('/') {
		normalized.pop();
	}
	normalized
}

/// `escapeGlobMetachars`: wrap glob metacharacters in a single-char class.
fn escape_glob_metachars(value: &str) -> String {
	let mut escaped = String::with_capacity(value.len());
	for ch in value.chars() {
		if matches!(ch, '*' | '?' | '[' | '{') {
			escaped.push('[');
			escaped.push(ch);
			escaped.push(']');
		} else {
			escaped.push(ch);
		}
	}
	escaped
}

/// Unique workspace entry whose trailing path matches a missing authored path.
/// `None` for no match, ambiguity, timeout, cancellation, or scan failure.
async fn find_unique_workspace_suffix(
	raw: &str,
	ctx: &AgentContext,
) -> Option<(PathBuf, String)> {
	let normalized = normalize_suffix(raw);
	if normalized.is_empty() || normalized.starts_with('/') {
		return None;
	}
	let pattern = format!("**/{}", escape_glob_metachars(&normalized));
	let glob = CompiledWalkGlob::new([pattern]).ok()?;
	let deadline = Instant::now() + SUFFIX_SCAN_TIMEOUT;
	let request = WalkRequest::new(ctx.cwd.clone())
		.hidden(true)
		.gitignore(false)
		.skip_git(true)
		.filter(WalkFilter::all().glob(glob))
		.limit(2);
	let outcome = request
		.collect_with_heartbeat(|| {
			if is_cancelled(ctx) {
				return Err("cancelled");
			}
			if Instant::now() >= deadline {
				return Err("timeout");
			}
			Ok::<(), &'static str>(())
		})
		.ok()?;
	if outcome.entries.len() != 1 {
		return None;
	}
	let entry = &outcome.entries[0];
	Some((entry.absolute_path(&ctx.cwd), entry.path.clone()))
}

/// Inline the suffix-resolution notice so the model sees the real path.
fn prepend_suffix_notice(content: String, suffix: Option<&(String, String)>) -> String {
	match suffix {
		Some((from, to)) => format!(
			"[Path '{from}' not found; resolved to '{to}' via suffix match]\n{content}"
		),
		None => content,
	}
}

fn attach_suffix_details(details: &mut Option<Value>, suffix: Option<&(String, String)>) {
	let Some((from, to)) = suffix else {
		return;
	};
	let Some(Value::Object(map)) = details.as_mut() else {
		return;
	};
	map.insert("suffixResolution".to_string(), json!({ "from": from, "to": to }));
}

enum LiteralState {
	Exists,
	Missing,
	Unknown,
}

async fn probe_literal(path: &Path) -> LiteralState {
	match tokio::fs::symlink_metadata(path).await {
		Ok(_) => LiteralState::Exists,
		Err(err) => match err.kind() {
			io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => LiteralState::Missing,
			// ENAMETOOLONG: no single entry can be that long, so it is missing.
			_ if err.raw_os_error() == Some(36) => LiteralState::Missing,
			_ => LiteralState::Unknown,
		},
	}
}

async fn open_regular(path: &Path) -> io::Result<tokio::fs::File> {
	#[cfg(any(target_os = "linux", target_os = "android"))]
	let file = {
		let mut options = tokio::fs::OpenOptions::new();
		options.read(true).custom_flags(O_NONBLOCK);
		options.open(path).await?
	};
	#[cfg(not(any(target_os = "linux", target_os = "android")))]
	let file = tokio::fs::File::open(path).await?;

	// Re-check after open so a file swapped for a FIFO/device is rejected.
	if !file.metadata().await?.is_file() {
		return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
	}
	Ok(file)
}

/// Read at most `cap` bytes through one already-validated regular handle.
/// `None` when the file is not a regular file or grew past `cap` while open,
/// so callers can hash exactly the bytes they displayed without reopening.
async fn read_bounded_regular(path: &Path, cap: u64) -> Option<Vec<u8>> {
	let file = open_regular(path).await.ok()?;
	let mut limited = file.take(cap + 1);
	let mut bytes = Vec::new();
	limited.read_to_end(&mut bytes).await.ok()?;
	((bytes.len() as u64) <= cap).then_some(bytes)
}

fn internal_scheme(raw: &str) -> Option<String> {
	let idx = raw.find("://")?;
	let scheme = &raw[..idx];
	if scheme.is_empty() || !scheme.as_bytes()[0].is_ascii_alphabetic() {
		return None;
	}
	if !scheme
		.bytes()
		.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
	{
		return None;
	}
	Some(scheme.to_ascii_lowercase())
}

fn lower_path(path: &Path) -> String {
	path.to_string_lossy().to_ascii_lowercase()
}

fn has_extension(path: &Path, extensions: &[&str]) -> bool {
	let lower = lower_path(path);
	extensions.iter().any(|extension| lower.ends_with(extension))
}

fn unsupported_extension(path: &Path) -> Option<&'static str> {
	if has_extension(path, CONVERTIBLE_EXTENSIONS) {
		return Some("Document");
	}
	if has_extension(path, ARCHIVE_SUFFIXES) {
		return Some("Archive");
	}
	None
}

fn existing_archive_prefix(raw: &str, ctx: &AgentContext) -> Option<String> {
	let lower = raw.to_ascii_lowercase();
	let mut cuts = Vec::new();
	for suffix in ARCHIVE_SUFFIXES {
		if lower.ends_with(suffix) {
			cuts.push(raw.len());
		}
		let needle = format!("{suffix}:");
		let mut from = 0;
		while let Some(offset) = lower[from..].find(&needle) {
			let end = from + offset + suffix.len();
			cuts.push(end);
			from = end;
		}
	}
	for end in cuts {
		let candidate = &raw[..end];
		let target = resolve_path(ctx, candidate);
		if std::fs::symlink_metadata(&target).is_ok() {
			return Some(target.display().to_string());
		}
	}
	None
}

fn looks_binary(head: &[u8]) -> bool {
	if head.contains(&0) {
		return true;
	}
	match std::str::from_utf8(head) {
		Ok(_) => false,
		// An incomplete multi-byte sequence at the sniff boundary is not binary.
		Err(err) => err.error_len().is_some(),
	}
}

fn unsupported(message: String, target: &Path) -> ToolResult {
	let details = json!({
		"resolvedPath": target.display().to_string(),
		"meta": { "source": { "type": "path", "value": target.display().to_string() } }
	});
	result_with_details(message, details, true)
}

// ---------------------------------------------------------------------------
// Local text reads
// ---------------------------------------------------------------------------

struct Window {
	lines: Vec<String>,
	total_lines: u64,
	collected_bytes: u64,
	stopped_by_bytes: bool,
	first_line_bytes: Option<u64>,
	first_line_preview: Vec<u8>,
	reached_eof: bool,
}

struct ScanState {
	start_line: u64,
	max_lines: u64,
	max_bytes: u64,
	selected_limit: u64,
	lines: Vec<String>,
	total_lines: u64,
	collected_bytes: u64,
	stopped_by_bytes: bool,
	first_line_bytes: Option<u64>,
	preview: Vec<u8>,
	current_len: u64,
	buffer: Vec<u8>,
	capture: bool,
	discard: bool,
	capture_limit: u64,
	done: bool,
	selected_seen: u64,
}

impl ScanState {
	fn new(start_line: u64, max_lines: u64, max_bytes: u64, selected_limit: u64) -> Self {
		let mut state = Self {
			start_line,
			max_lines,
			max_bytes,
			selected_limit,
			lines: Vec::new(),
			total_lines: 0,
			collected_bytes: 0,
			stopped_by_bytes: false,
			first_line_bytes: None,
			preview: Vec::new(),
			current_len: 0,
			buffer: Vec::new(),
			capture: false,
			discard: false,
			capture_limit: 0,
			done: false,
			selected_seen: 0,
		};
		state.setup();
		state
	}

	fn setup(&mut self) {
		self.capture = !self.done && self.total_lines >= self.start_line;
		self.discard = !self.capture;
		self.buffer.clear();
		if self.capture {
			let separator = if self.lines.is_empty() { 0 } else { 1 };
			self.capture_limit = self.max_bytes.saturating_sub(self.collected_bytes + separator);
			if self.capture_limit == 0 {
				self.discard = true;
			}
		} else {
			self.capture_limit = 0;
		}
	}

	fn push_byte(&mut self, byte: u8) {
		self.current_len += 1;
		if !self.done && self.total_lines >= self.start_line && self.lines.is_empty() && self.preview.len() < self.max_bytes as usize {
			self.preview.push(byte);
		}
		if self.capture && !self.discard {
			if self.current_len <= self.capture_limit {
				self.buffer.push(byte);
			} else {
				self.discard = true;
				self.buffer.clear();
			}
		}
	}

	fn finish_line(&mut self) {
		if self.total_lines >= self.start_line && self.selected_seen < self.selected_limit {
			self.selected_seen += 1;
		}
		if !self.done && self.total_lines >= self.start_line {
			let separator = if self.lines.is_empty() { 0 } else { 1 };
			if self.lines.len() as u64 >= self.max_lines {
				self.done = true;
			} else if self.lines.is_empty() && self.current_len > self.max_bytes {
				self.stopped_by_bytes = true;
				self.done = true;
				self.first_line_bytes.get_or_insert(self.current_len);
			} else if !self.lines.is_empty() && self.collected_bytes + separator + self.current_len > self.max_bytes {
				self.stopped_by_bytes = true;
				self.done = true;
			} else {
				self.lines.push(String::from_utf8_lossy(&self.buffer).into_owned());
				self.collected_bytes += separator + self.current_len;
				self.first_line_bytes.get_or_insert(self.current_len);
				if self.collected_bytes > self.max_bytes {
					self.stopped_by_bytes = true;
					self.done = true;
				} else if self.lines.len() as u64 >= self.max_lines {
					self.done = true;
				}
			}
		} else if self.total_lines >= self.start_line {
			self.first_line_bytes.get_or_insert(self.current_len);
		}
		self.total_lines += 1;
		self.current_len = 0;
		self.buffer.clear();
		self.setup();
	}
}

/// Scan an in-memory buffer with the same line model and caps as
/// `scan_window`; used for files at or below the snapshot cap so the tag is
/// derived from exactly the scanned bytes.
fn scan_bytes(
	bytes: &[u8],
	start_line: u64,
	max_lines: u64,
	max_bytes: u64,
	selected_limit: u64,
	raw: bool,
) -> Window {
	let mut state = ScanState::new(start_line, max_lines, max_bytes, selected_limit);
	let mut saw_any = false;
	let mut ended_with_newline = false;
	for &byte in bytes {
		saw_any = true;
		ended_with_newline = byte == b'\n';
		if byte == b'\n' {
			state.finish_line();
		} else {
			state.push_byte(byte);
		}
	}
	if state.current_len > 0 || !saw_any || (raw && ended_with_newline) {
		state.finish_line();
	}
	Window {
		lines: state.lines,
		total_lines: state.total_lines,
		collected_bytes: state.collected_bytes,
		stopped_by_bytes: state.stopped_by_bytes,
		first_line_bytes: state.first_line_bytes,
		first_line_preview: state.preview,
		reached_eof: true,
	}
}

async fn scan_window(
	path: &Path,
	file_size: u64,
	start_line: u64,
	max_lines: u64,
	max_bytes: u64,
	selected_limit: u64,
	raw: bool,
	cancel: &CancellationToken,
) -> Result<Window, String> {
	let mut file = open_regular(path)
		.await
		.map_err(|err| format!("Failed to read {}: {err}", path.display()))?;
	let mut state = ScanState::new(start_line, max_lines, max_bytes, selected_limit);
	let stop_after_collect = file_size > SNAPSHOT_MAX_BYTES;
	let mut chunk = [0u8; READ_CHUNK_SIZE];
	let mut saw_any = false;
	let mut ended_with_newline = false;
	let mut reached_eof = true;

	'outer: loop {
		if cancel.is_cancelled() {
			return Err("Read cancelled".to_string());
		}
		let read = file
			.read(&mut chunk)
			.await
			.map_err(|err| format!("Failed to read {}: {err}", path.display()))?;
		if read == 0 {
			break;
		}
		saw_any = true;
		ended_with_newline = chunk[read - 1] == b'\n';
		for &byte in &chunk[..read] {
			if byte == b'\n' {
				state.finish_line();
				if state.done && state.selected_seen >= state.selected_limit && stop_after_collect {
					reached_eof = false;
					break 'outer;
				}
			} else {
				state.push_byte(byte);
			}
		}
	}

	if reached_eof && (state.current_len > 0 || !saw_any || (raw && ended_with_newline)) {
		state.finish_line();
	}

	Ok(Window {
		lines: state.lines,
		total_lines: state.total_lines,
		collected_bytes: state.collected_bytes,
		stopped_by_bytes: state.stopped_by_bytes,
		first_line_bytes: state.first_line_bytes,
		first_line_preview: state.preview,
		reached_eof,
	})
}

async fn read_single_range(
	path: &Path,
	file_size: u64,
	range: LineRange,
	raw: bool,
	sink: Option<&dyn ReadSnapshotSink>,
	ctx: &AgentContext,
) -> Result<ToolResult, String> {
	let requested_start = range.start_line - 1;
	let limit = range.end_line.map(|end| end - range.start_line + 1);
	let effective_limit = limit.unwrap_or(DEFAULT_READ_LIMIT);
	let expand_start = !raw && requested_start > 0;
	let expand_end = !raw && limit.is_some();
	let leading_context = if expand_start {
		requested_start.min(RANGE_LEADING_CONTEXT)
	} else {
		0
	};
	let trailing_context = if expand_end { RANGE_TRAILING_CONTEXT } else { 0 };
	let start_line = requested_start - leading_context;
	let selected_line_limit = effective_limit + leading_context + trailing_context;
	let max_lines_to_collect = selected_line_limit.min(DEFAULT_MAX_LINES);
	let max_bytes = DEFAULT_MAX_BYTES.max(max_lines_to_collect * 512);

	let buffered = if file_size <= SNAPSHOT_MAX_BYTES {
		read_bounded_regular(path, SNAPSHOT_MAX_BYTES).await
	} else {
		None
	};
	let (window, snapshot_text) = match buffered {
		Some(bytes) => {
			let text = normalize_snapshot_text(&bytes);
			// Raw display stays byte-exact; non-raw uses the normalized text
			// whose hash the snapshot records (BOM stripped, CRLF → LF).
			let source: &[u8] = if raw { &bytes } else { text.as_bytes() };
			let window = scan_bytes(
				source,
				start_line,
				max_lines_to_collect,
				max_bytes,
				selected_line_limit,
				raw,
			);
			(window, Some(text))
		},
		None => (
			scan_window(
				path,
				file_size,
				start_line,
				max_lines_to_collect,
				max_bytes,
				selected_line_limit,
				raw,
				&ctx.cancellation_token,
			)
			.await?,
			None,
		),
	};

	if window.reached_eof && requested_start >= window.total_lines {
		let suggestion = if window.total_lines == 0 {
			"The file is empty.".to_string()
		} else {
			format!(
				"Use :1 to read from the start, or :{} to read the last line.",
				window.total_lines
			)
		};
		let details = json!({
			"resolvedPath": path.display().to_string(),
			"fileSize": file_size,
			"meta": { "source": { "type": "path", "value": path.display().to_string() } }
		});
		return Ok(result_with_details(
			format!(
				"Line {} is beyond end of file ({} lines total). {suggestion}",
				requested_start + 1,
				window.total_lines
			),
			details,
			false,
		));
	}

	let display_start = start_line + 1;
	let total_selected_lines = window.total_lines.saturating_sub(start_line);
	let user_limited_lines = window.lines.len() as u64;
	let truncated = (window.lines.len() as u64) < total_selected_lines || window.stopped_by_bytes;
	let first_line_exceeds = window
		.first_line_bytes
		.is_some_and(|bytes| bytes > max_bytes);
	let mut column_truncated = false;
	let display_lines: Vec<String> = if raw {
		window.lines.clone()
	} else {
		window
			.lines
			.iter()
			.map(|line| {
				let (text, was_truncated) = truncate_line(line, MAX_COLUMNS);
				column_truncated |= was_truncated;
				text
			})
			.collect()
	};
	let unnumbered = display_lines.join("\n");
	let line_separator = if sink.is_some() { ':' } else { '|' };
	let format_body = |lines: &[String]| -> String {
		if raw {
			lines.join("\n")
		} else {
			format_numbered(lines, display_start, line_separator)
		}
	};

	let mut output: String;
	let mut truncation: Option<Value> = None;
	let mut meta_truncation: Option<Value> = None;

	if first_line_exceeds {
		let first_line_bytes = window.first_line_bytes.unwrap_or(0);
		let preview = String::from_utf8_lossy(&window.first_line_preview).into_owned();
		if sink.is_some() && !raw {
			output = format!(
				"[Line {display_start} is {}, exceeds {} limit. Hashline output requires full lines; cannot emit an editable numbered preview for a truncated line.]",
				format_bytes(first_line_bytes),
				format_bytes(max_bytes)
			);
		} else if preview.is_empty() {
			output = format!(
				"[Line {display_start} is {}, exceeds {} limit. Unable to display a valid UTF-8 snippet.]",
				format_bytes(first_line_bytes),
				format_bytes(max_bytes)
			);
		} else {
			output = if raw {
				preview
			} else {
				format_numbered(&[preview], display_start, line_separator)
			};
		}
		truncation = Some(truncation_json(
			"",
			true,
			Some(if window.stopped_by_bytes { "bytes" } else { "lines" }),
			total_selected_lines,
			window.collected_bytes,
			0,
			0,
			true,
		));
		meta_truncation = Some(meta_truncation_json(
			if window.stopped_by_bytes { "bytes" } else { "lines" },
			window.total_lines,
			window.collected_bytes,
			0,
			window.collected_bytes,
			display_start,
			display_start.saturating_sub(1),
		));
	} else if truncated {
		output = format_body(&display_lines);
		truncation = Some(truncation_json(
			&unnumbered,
			true,
			Some(if window.stopped_by_bytes { "bytes" } else { "lines" }),
			total_selected_lines,
			window.collected_bytes,
			window.lines.len() as u64,
			window.collected_bytes,
			false,
		));
		meta_truncation = Some(meta_truncation_json(
			if window.stopped_by_bytes { "bytes" } else { "lines" },
			window.total_lines,
			window.collected_bytes,
			window.lines.len() as u64,
			window.collected_bytes,
			display_start,
			display_start + window.lines.len().saturating_sub(1) as u64,
		));
	} else if start_line + user_limited_lines < window.total_lines || !window.reached_eof {
		output = format_body(&display_lines);
		let next_offset = start_line + user_limited_lines + 1;
		if window.reached_eof {
			let remaining = window.total_lines - (start_line + user_limited_lines);
			output.push_str(&format!(
				"\n\n[{remaining} more lines in file. Use :{next_offset} to continue]"
			));
		} else {
			output.push_str(&format!(
				"\n\n[More lines in file ({} total; not scanned to EOF). Use :{next_offset} to continue]",
				format_bytes(file_size)
			));
		}
	} else {
		output = format_body(&display_lines);
	}

	if let (Some(sink), Some(text)) = (sink, snapshot_text.as_deref()) {
		if !first_line_exceeds && !window.lines.is_empty() {
			let seen: Vec<u64> = (0..window.lines.len() as u64)
				.map(|offset| display_start + offset)
				.collect();
			if let Some(header) = record_snapshot_text(sink, path, &ctx.cwd, text, &seen) {
				if !raw {
					output = format!("{header}\n{output}");
				}
			}
		}
	}

	let mut details = serde_json::Map::new();
	details.insert("resolvedPath".into(), json!(path.display().to_string()));
	details.insert("fileSize".into(), json!(file_size));
	if window.reached_eof {
		details.insert("totalLines".into(), json!(window.total_lines));
	}
	if !display_lines.is_empty() {
		let line_numbers: Vec<Value> = (0..display_lines.len() as u64)
			.map(|offset| json!(display_start + offset))
			.collect();
		details.insert(
			"displayContent".into(),
			json!({ "text": unnumbered, "startLine": display_start, "lineNumbers": line_numbers }),
		);
	}
	if let Some(truncation) = truncation {
		details.insert("truncation".into(), truncation);
	}
	let mut meta = serde_json::Map::new();
	meta.insert(
		"source".into(),
		json!({ "type": "path", "value": path.display().to_string() }),
	);
	if let Some(truncation) = meta_truncation {
		meta.insert("truncation".into(), truncation);
	}
	if column_truncated {
		meta.insert("limits".into(), json!({ "columnTruncated": { "maxColumn": MAX_COLUMNS } }));
	}
	details.insert("meta".into(), Value::Object(meta));

	Ok(result_with_details(output, Value::Object(details), false))
}

async fn read_multi_range(
	path: &Path,
	file_size: u64,
	ranges: &[LineRange],
	raw: bool,
	sink: Option<&dyn ReadSnapshotSink>,
	ctx: &AgentContext,
) -> Result<ToolResult, String> {
	let mut blocks: Vec<(LineRange, Vec<String>)> = Vec::new();
	let mut skipped: Vec<LineRange> = Vec::new();
	let mut byte_capped: Vec<LineRange> = Vec::new();
	let mut total_lines: Option<u64> = None;

	let buffered = if file_size <= SNAPSHOT_MAX_BYTES {
		read_bounded_regular(path, SNAPSHOT_MAX_BYTES).await
	} else {
		None
	};
	let snapshot_text = buffered.as_ref().map(|bytes| normalize_snapshot_text(bytes));

	for &range in ranges {
		let requested_length = range
			.end_line
			.map(|end| end - range.start_line + 1)
			.unwrap_or(DEFAULT_READ_LIMIT);
		let max_lines = requested_length.min(DEFAULT_MAX_LINES);
		// Buffered files (≤4 MiB) slice addressable lines with no byte budget,
		// matching TS; only the streaming fallback streams under the cap.
		let max_bytes = if buffered.is_some() {
			u64::MAX
		} else {
			DEFAULT_MAX_BYTES.max(max_lines * 512)
		};
		let window = match (buffered.as_deref(), snapshot_text.as_deref()) {
			(Some(bytes), Some(text)) => {
				let source: &[u8] = if raw { bytes } else { text.as_bytes() };
				scan_bytes(source, range.start_line - 1, max_lines, max_bytes, max_lines, raw)
			},
			_ => {
				scan_window(
					path,
					file_size,
					range.start_line - 1,
					max_lines,
					max_bytes,
					max_lines,
					raw,
					&ctx.cancellation_token,
				)
				.await?
			},
		};
		if window.stopped_by_bytes {
			byte_capped.push(range);
		}
		if window.reached_eof {
			total_lines = Some(total_lines.map_or(window.total_lines, |total| total.max(window.total_lines)));
		}
		let known_total = window.total_lines;
		if range.start_line > known_total && window.reached_eof {
			skipped.push(range);
			continue;
		}
		blocks.push((range, window.lines));
	}

	let mut notices = Vec::new();
	for range in &skipped {
		let bound = match range.end_line {
			Some(end) => format!("{}-{end}", range.start_line),
			None => range.start_line.to_string(),
		};
		let total = total_lines.unwrap_or(0);
		notices.push(format!(
			"[Range {bound} is beyond end of file ({total} lines total); skipped]"
		));
	}
	for range in &byte_capped {
		let bound = match range.end_line {
			Some(end) => format!("{}-{end}", range.start_line),
			None => range.start_line.to_string(),
		};
		notices.push(format!(
			"[Range {bound} stopped at the read byte budget; use a narrower selector to read the rest]"
		));
	}

	let mut column_truncated = false;
	let mut block_texts: Vec<String> = Vec::new();
	let mut display_lines: Vec<String> = Vec::new();
	let mut display_numbers: Vec<Value> = Vec::new();
	let mut seen_lines: Vec<u64> = Vec::new();
	let mut first_display: Option<u64> = None;
	let mut previous_end: Option<u64> = None;
	let line_separator = if sink.is_some() { ':' } else { '|' };

	for (range, lines) in &blocks {
		if lines.is_empty() {
			continue;
		}
		if previous_end.is_some_and(|end| range.start_line > end + 1) {
			display_lines.push("…".to_string());
			display_numbers.push(Value::Null);
		}
		let mut formatted = Vec::with_capacity(lines.len());
		for (index, line) in lines.iter().enumerate() {
			let number = range.start_line + index as u64;
			let text = if raw {
				line.clone()
			} else {
				let (text, was_truncated) = truncate_line(line, MAX_COLUMNS);
				column_truncated |= was_truncated;
				text
			};
			formatted.push(if raw {
				text.clone()
			} else {
				format!("{number}{line_separator}{text}")
			});
			seen_lines.push(number);
			display_lines.push(text);
			display_numbers.push(json!(number));
			if first_display.is_none() {
				first_display = Some(number);
			}
		}
		block_texts.push(formatted.join("\n"));
		previous_end = Some(range.start_line + lines.len() as u64 - 1);
	}

	let mut output = block_texts.join("\n\n…\n\n");
	if !notices.is_empty() {
		if !output.is_empty() {
			output.push('\n');
		}
		output.push_str(&notices.join("\n"));
	}

	if let (Some(sink), Some(text)) = (sink, snapshot_text.as_deref()) {
		if !seen_lines.is_empty() {
			if let Some(header) = record_snapshot_text(sink, path, &ctx.cwd, text, &seen_lines) {
				if !raw {
					output = format!("{header}\n{output}");
				}
			}
		}
	}

	let mut details = serde_json::Map::new();
	details.insert("resolvedPath".into(), json!(path.display().to_string()));
	details.insert("fileSize".into(), json!(file_size));
	if let Some(total) = total_lines {
		details.insert("totalLines".into(), json!(total));
	}
	if !raw && !display_lines.is_empty() {
		details.insert(
			"displayContent".into(),
			json!({
				"text": display_lines.join("\n"),
				"startLine": first_display.unwrap_or(1),
				"lineNumbers": display_numbers,
			}),
		);
	}
	let mut meta = serde_json::Map::new();
	meta.insert(
		"source".into(),
		json!({ "type": "path", "value": path.display().to_string() }),
	);
	if column_truncated {
		meta.insert("limits".into(), json!({ "columnTruncated": { "maxColumn": MAX_COLUMNS } }));
	}
	details.insert("meta".into(), Value::Object(meta));

	Ok(result_with_details(output, Value::Object(details), false))
}

#[allow(clippy::too_many_arguments)]
fn truncation_json(
	content: &str,
	truncated: bool,
	truncated_by: Option<&str>,
	total_lines: u64,
	total_bytes: u64,
	output_lines: u64,
	output_bytes: u64,
	first_line_exceeds_limit: bool,
) -> Value {
	let mut value = serde_json::Map::new();
	value.insert("content".into(), json!(content));
	value.insert("truncated".into(), json!(truncated));
	if let Some(truncated_by) = truncated_by {
		value.insert("truncatedBy".into(), json!(truncated_by));
	}
	value.insert("totalLines".into(), json!(total_lines));
	value.insert("totalBytes".into(), json!(total_bytes));
	value.insert("outputLines".into(), json!(output_lines));
	value.insert("outputBytes".into(), json!(output_bytes));
	value.insert("lastLinePartial".into(), json!(false));
	value.insert("firstLineExceedsLimit".into(), json!(first_line_exceeds_limit));
	Value::Object(value)
}

#[allow(clippy::too_many_arguments)]
fn meta_truncation_json(
	truncated_by: &str,
	total_lines: u64,
	total_bytes: u64,
	output_lines: u64,
	output_bytes: u64,
	shown_start: u64,
	shown_end: u64,
) -> Value {
	json!({
		"direction": "head",
		"truncatedBy": truncated_by,
		"totalLines": total_lines,
		"totalBytes": total_bytes,
		"outputLines": output_lines,
		"outputBytes": output_bytes,
		"shownRange": { "start": shown_start, "end": shown_end },
		"nextOffset": shown_end + 1,
	})
}

/// `truncateHead` for listing text: exact UTF-8 byte budget, never a partial
/// line, with the TS `content`/top-level and `meta.truncation` shapes.
struct HeadTruncation {
	content:            String,
	truncated_by:       &'static str,
	total_lines:        u64,
	total_bytes:        u64,
	output_lines:       u64,
	output_bytes:       u64,
	first_line_exceeds: bool,
}

fn truncate_head(content: &str, max_bytes: u64) -> HeadTruncation {
	let total_bytes = content.len() as u64;
	let total_lines = content.bytes().filter(|byte| *byte == b'\n').count() as u64 + 1;
	if total_bytes <= max_bytes {
		return HeadTruncation {
			content: content.to_string(),
			truncated_by: "lines",
			total_lines,
			total_bytes,
			output_lines: total_lines,
			output_bytes: total_bytes,
			first_line_exceeds: false,
		};
	}
	let mut included = 0u64;
	let mut used = 0u64;
	let mut cut = 0usize;
	let mut cursor = 0usize;
	for line in content.split('\n') {
		let separator = u64::from(included > 0);
		let Some(remaining) = max_bytes.checked_sub(used + separator) else {
			break;
		};
		let line_bytes = line.len() as u64;
		if line_bytes > remaining {
			if included == 0 {
				return HeadTruncation {
					content: String::new(),
					truncated_by: "bytes",
					total_lines,
					total_bytes,
					output_lines: 0,
					output_bytes: 0,
					first_line_exceeds: true,
				};
			}
			break;
		}
		used += separator + line_bytes;
		included += 1;
		cursor += line.len();
		cut = cursor;
		if cursor < content.len() {
			cursor += 1;
		}
	}
	HeadTruncation {
		content: content[..cut].to_string(),
		truncated_by: "bytes",
		total_lines,
		total_bytes,
		output_lines: included,
		output_bytes: used,
		first_line_exceeds: false,
	}
}

fn format_numbered(lines: &[String], start_line: u64, separator: char) -> String {
	lines
		.iter()
		.enumerate()
		.map(|(index, line)| format!("{}{separator}{line}", start_line + index as u64))
		.collect::<Vec<_>>()
		.join("\n")
}

fn truncate_line(line: &str, max_chars: usize) -> (String, bool) {
	let mut chars = line.chars();
	let prefix: String = chars.by_ref().take(max_chars).collect();
	if chars.next().is_some() {
		(format!("{prefix}…"), true)
	} else {
		(prefix, false)
	}
}

/// LF-normalize and strip a UTF-8 BOM before handing text to the hashline
/// store, whose tag hash expects canonical line endings.
fn normalize_snapshot_text(bytes: &[u8]) -> String {
	let text = String::from_utf8_lossy(bytes);
	let borrowed: &str = &text;
	let stripped = borrowed.strip_prefix('\u{feff}').unwrap_or(borrowed);
	stripped.replace("\r\n", "\n").replace('\r', "\n")
}

/// Record an already-displayed, already-normalized text in the sink and
/// return the header. Skipped (no header, no fabrication) when the sink cannot
/// tag the text.
fn record_snapshot_text(
	sink: &dyn ReadSnapshotSink,
	path: &Path,
	cwd: &Path,
	full_text: &str,
	seen: &[u64],
) -> Option<String> {
	let tag = sink.record_file(path, full_text)?;
	if !seen.is_empty() {
		// The store keys seen lines as u32; a window past that bound is not
		// representable, so record the snapshot without provenance.
		let seen_32: Option<Vec<u32>> = seen.iter().map(|line| u32::try_from(*line).ok()).collect();
		if let Some(seen_32) = seen_32 {
			sink.record_seen_lines(path, &tag, &seen_32);
		}
	}
	Some(sink.header(path, &tag, cwd))
}

/// Bounded fallback for callers that did not buffer the file themselves
/// (structural summaries): one validated regular handle, never a path reopen.
async fn record_read_snapshot(
	sink: &dyn ReadSnapshotSink,
	path: &Path,
	cwd: &Path,
	file_size: u64,
	seen: &[u64],
) -> Option<String> {
	if file_size > SNAPSHOT_MAX_BYTES {
		return None;
	}
	let bytes = read_bounded_regular(path, SNAPSHOT_MAX_BYTES).await?;
	let full_text = normalize_snapshot_text(&bytes);
	record_snapshot_text(sink, path, cwd, &full_text, seen)
}

// ---------------------------------------------------------------------------
// Structural summaries (`read-summary.ts` over native `pi-ast`)
// ---------------------------------------------------------------------------

/// Prose files skip summarization unless `read.summarize.prose` opts in; that
/// setting defaults to false, so they are read verbatim here.
fn is_prose_summary_path(path: &Path) -> bool {
	matches!(
		path.extension().and_then(|ext| ext.to_str()).map(|ext| ext.to_ascii_lowercase()).as_deref(),
		Some("md" | "markdown" | "mdx" | "mdc" | "mkd" | "mdown" | "txt")
	)
}

/// `countTextLines`: newline count + 1 for non-empty text.
fn count_text_lines(text: &str) -> u64 {
	if text.is_empty() {
		return 0;
	}
	text.bytes().filter(|byte| *byte == b'\n').count() as u64 + 1
}

/// `trySummarize` with the shipped defaults: bounded read, 100-line floor,
/// 20k-line and 2 MiB ceilings, tree-sitter parse must succeed and elide.
async fn try_summarize(path: &Path, file_size: u64, ctx: &AgentContext) -> Option<SummaryResult> {
	if file_size > MAX_SUMMARY_BYTES || is_cancelled(ctx) {
		return None;
	}
	let bytes = tokio::fs::read(path).await.ok()?;
	if bytes.len() as u64 > MAX_SUMMARY_BYTES || is_cancelled(ctx) {
		return None;
	}
	let code = String::from_utf8_lossy(&bytes).into_owned();
	let line_count = count_text_lines(&code);
	if line_count > MAX_SUMMARY_LINES || line_count < SUMMARY_MIN_TOTAL_LINES {
		return None;
	}
	let result = summarize_code(SummaryOptions {
		code,
		lang: None,
		path: Some(path.to_string_lossy().into_owned()),
		min_body_lines: Some(SUMMARY_MIN_BODY_LINES),
		min_comment_lines: Some(SUMMARY_MIN_COMMENT_LINES),
		unfold_until_lines: Some(SUMMARY_UNFOLD_UNTIL),
		unfold_limit_lines: Some(SUMMARY_UNFOLD_LIMIT),
	})
	.ok()?;
	(result.parsed && result.elided).then_some(result)
}

#[derive(Clone)]
enum SummaryUnit {
	Line { line: u32, text: String },
	Elided { start: u32, end: u32 },
	Merged { start: u32, end: u32, merged: String },
}

struct RenderedSummary {
	text:             String,
	display_text:     String,
	elided_ranges:    Vec<(u32, u32)>,
	elided_lines:     u64,
	seen:             Vec<u64>,
}

/// `canMergeBracePair`: head ends with an opener and tail is only its closer
/// plus terminating punctuation.
fn can_merge_brace_pair(head_line: &str, tail_line: &str) -> bool {
	let head = head_line.trim_end();
	let Some(opener) = head.chars().next_back() else {
		return false;
	};
	let closer = match opener {
		'{' => '}',
		'(' => ')',
		'[' => ']',
		_ => return false,
	};
	let tail = tail_line.trim();
	let mut chars = tail.chars();
	if chars.next() != Some(closer) {
		return false;
	}
	chars.all(|ch| matches!(ch, ';' | ',' | ')' | ']' | '}'))
}

/// `renderSummary`: kept lines numbered in the active display mode, elided
/// spans as `…`, and kept-head/elided/kept-tail sandwiches collapsed to one
/// `S-E` brace-pair row.
fn render_summary(result: &SummaryResult, hash_lines: bool) -> RenderedSummary {
	let mut raw: Vec<SummaryUnit> = Vec::new();
	for segment in &result.segments {
		if segment.kind == "elided" {
			raw.push(SummaryUnit::Elided { start: segment.start_line, end: segment.end_line });
			continue;
		}
		let Some(text) = segment.text.as_deref() else {
			continue;
		};
		if text.is_empty() {
			continue;
		}
		for (index, line) in text.split('\n').enumerate() {
			raw.push(SummaryUnit::Line { line: segment.start_line + index as u32, text: line.to_string() });
		}
	}

	let mut units: Vec<SummaryUnit> = Vec::with_capacity(raw.len());
	let mut index = 0;
	while index < raw.len() {
		if let SummaryUnit::Elided { .. } = raw[index] {
			if let Some(SummaryUnit::Line { line: head_line, text: head }) = units.last().cloned() {
				if let Some(SummaryUnit::Line { line: tail_line, text: tail }) = raw.get(index + 1) {
					if can_merge_brace_pair(&head, tail) {
						let merged = format!("{} … {}", head.trim_end(), tail.trim());
						units.pop();
						units.push(SummaryUnit::Merged { start: head_line, end: *tail_line, merged });
						index += 2;
						continue;
					}
				}
			}
		}
		units.push(raw[index].clone());
		index += 1;
	}

	let separator = if hash_lines { ':' } else { '|' };
	let mut model_parts: Vec<String> = Vec::new();
	let mut display_parts: Vec<String> = Vec::new();
	let mut elided_ranges: Vec<(u32, u32)> = Vec::new();
	let mut elided_lines = 0u64;
	let mut seen: Vec<u64> = Vec::new();
	for unit in units {
		match unit {
			SummaryUnit::Line { line, text } => {
				model_parts.push(if hash_lines {
					format!("{line}:{text}")
				} else {
					format!("{line}|{text}")
				});
				display_parts.push(text);
				seen.push(u64::from(line));
			},
			SummaryUnit::Merged { start, end, merged } => {
				model_parts.push(format!("{start}-{end}{separator}{merged}"));
				display_parts.push(merged);
				elided_ranges.push((start, end));
				elided_lines += u64::from(end.saturating_sub(start).saturating_sub(1));
				seen.push(u64::from(start));
				seen.push(u64::from(end));
			},
			SummaryUnit::Elided { start, end } => {
				model_parts.push("…".to_string());
				display_parts.push("…".to_string());
				elided_ranges.push((start, end));
				elided_lines += u64::from(end.saturating_sub(start).saturating_add(1));
			},
		}
	}

	RenderedSummary {
		text: model_parts.join("\n"),
		display_text: display_parts.join("\n"),
		elided_ranges,
		elided_lines,
		seen,
	}
}

fn summary_footer(display_path: &str, elided_ranges: &[(u32, u32)], elided_lines: u64) -> String {
	if elided_ranges.is_empty() {
		return String::new();
	}
	let sample_count = elided_ranges.len().min(FOOTER_RANGE_SAMPLES);
	let selector = elided_ranges[..sample_count]
		.iter()
		.map(|(start, end)| format!("{start}-{end}"))
		.collect::<Vec<_>>()
		.join(",");
	let example = format!("{display_path}:{selector}");
	let tail = if elided_ranges.len() > sample_count {
		format!(", e.g. {example}")
	} else {
		format!(" with {example}")
	};
	format!("[…{elided_lines}ln elided; re-read needed ranges{tail}]")
}

/// Path as the model sees it: cwd-relative when possible, `/` separators.
fn display_path(path: &Path, cwd: &Path) -> String {
	match path.strip_prefix(cwd) {
		Ok(relative) if !relative.as_os_str().is_empty() => {
			relative.to_string_lossy().replace('\\', "/")
		},
		_ => path.to_string_lossy().replace('\\', "/"),
	}
}

async fn read_summary_result(
	path: &Path,
	file_size: u64,
	summary: &SummaryResult,
	sink: Option<&dyn ReadSnapshotSink>,
	suffix: Option<&(String, String)>,
	ctx: &AgentContext,
) -> ToolResult {
	let rendered = render_summary(summary, sink.is_some());
	let footer = summary_footer(&display_path(path, &ctx.cwd), &rendered.elided_ranges, rendered.elided_lines);
	let mut text = rendered.text.clone();
	if !footer.is_empty() {
		text = format!("{text}\n\n{footer}");
	}
	if let Some(sink) = sink {
		if let Some(header) = record_read_snapshot(sink, path, &ctx.cwd, file_size, &rendered.seen).await {
			text = format!("{header}\n{text}");
		}
	}

	let details = json!({
		"resolvedPath": path.to_string_lossy(),
		"fileSize": file_size,
		"displayContent": { "text": rendered.display_text, "startLine": 1 },
		"summary": {
			"lines": count_text_lines(&rendered.text),
			"elidedSpans": rendered.elided_ranges.len(),
			"elidedLines": rendered.elided_lines,
		},
		"meta": { "source": { "type": "path", "value": path.to_string_lossy() } },
	});
	let mut result = result_with_details(prepend_suffix_notice(text, suffix), details, false);
	attach_suffix_details(&mut result.details, suffix);
	result
}

// ---------------------------------------------------------------------------
// Directory listings
// ---------------------------------------------------------------------------

async fn read_directory(target: &Path, parsed: &Sel, ctx: &AgentContext) -> ToolResult {
	if is_cancelled(ctx) {
		return tool_result("Read cancelled", true);
	}
	if let Sel::Lines { ranges, .. } = parsed {
		if ranges.len() > 1 {
			return tool_result("Multi-range line selectors are not supported for directory listings.", true);
		}
	}
	let root = target.to_path_buf();
	let built = match tokio::task::spawn_blocking(move || build_directory_tree(&root)).await {
		Ok(Ok(built)) => built,
		Ok(Err(err)) => return tool_result(format!("Cannot read directory: {err}"), true),
		Err(err) => return tool_result(format!("Cannot read directory: {err}"), true),
	};
	if is_cancelled(ctx) {
		return tool_result("Read cancelled", true);
	}

	let (offset, limit) = selector_offset_limit(parsed);
	let total_lines = built.total_lines;
	let mut output = if total_lines <= 1 {
		"(empty directory)".to_string()
	} else {
		built.rendered
	};
	let mut slice_notice: Option<String> = None;
	if offset.is_some() || limit.is_some() {
		let lines: Vec<String> = output.split('\n').map(str::to_string).collect();
		let start = offset.unwrap_or(1).saturating_sub(1) as usize;
		if start >= lines.len() {
			let suggestion = if lines.is_empty() {
				"The listing is empty.".to_string()
			} else {
				format!(
					"Use :1 to read from the start, or :{} to read the last line.",
					lines.len()
				)
			};
			return tool_result(
				format!(
					"Line {} is beyond end of listing ({} lines total). {suggestion}",
					start + 1,
					lines.len()
				),
				false,
			);
		}
		let end = match limit {
			Some(limit) => (start as u64 + limit).min(lines.len() as u64) as usize,
			None => lines.len(),
		};
		output = lines[start..end].join("\n");
		if end < lines.len() {
			let remaining = lines.len() - end;
			slice_notice = Some(format!(
				"[{remaining} more lines in listing. Use :{} to continue]",
				end + 1
			));
		}
	}
	if let Some(notice) = slice_notice {
		output.push_str("\n\n");
		output.push_str(&notice);
	}
	// TS runs every no-selector listing through `truncateHead(output, {
	// maxLines: MAX_SAFE_INTEGER })`, so the 50 KiB byte cap still applies.
	// The `:N-M` slice path is intentionally user-bounded.
	let head_truncation = if offset.is_none() && limit.is_none() && output.len() as u64 > DEFAULT_MAX_BYTES
	{
		Some(truncate_head(&output, DEFAULT_MAX_BYTES))
	} else {
		None
	};
	if let Some(truncation) = &head_truncation {
		output = truncation.content.clone();
	}

	let mut details = serde_json::Map::new();
	details.insert("isDirectory".into(), json!(true));
	details.insert("resolvedPath".into(), json!(target.display().to_string()));
	let mut meta = serde_json::Map::new();
	meta.insert(
		"source".into(),
		json!({ "type": "path", "value": target.display().to_string() }),
	);
	if built.truncated {
		meta.insert("limits".into(), json!({ "resultLimit": { "reached": 1, "suggestion": 2 } }));
	}
	if let Some(truncation) = &head_truncation {
		details.insert(
			"truncation".into(),
			truncation_json(
				&truncation.content,
				true,
				Some(truncation.truncated_by),
				truncation.total_lines,
				truncation.total_bytes,
				truncation.output_lines,
				truncation.output_bytes,
				truncation.first_line_exceeds,
			),
		);
		meta.insert(
			"truncation".into(),
			meta_truncation_json(
				truncation.truncated_by,
				truncation.total_lines,
				truncation.total_bytes,
				truncation.output_lines,
				truncation.output_bytes,
				1,
				truncation.output_lines,
			),
		);
	}
	details.insert("meta".into(), Value::Object(meta));

	result_with_details(output, Value::Object(details), false)
}

fn selector_offset_limit(parsed: &Sel) -> (Option<u64>, Option<u64>) {
	match parsed {
		Sel::Lines { ranges, raw: false } => {
			let first = ranges[0];
			let limit = first.end_line.map(|end| end - first.start_line + 1);
			(Some(first.start_line), limit)
		},
		_ => (None, None),
	}
}

struct BuiltDirectory {
	rendered: String,
	total_lines: u64,
	truncated: bool,
}

struct DirectoryNode {
	name: String,
	is_dir: bool,
	mtime: u64,
	size: u64,
	depth: usize,
	children: Vec<DirectoryNode>,
	dropped: usize,
}

const READ_DIRECTORY_MAX_DEPTH: usize = 2;
const READ_DIRECTORY_CHILD_LIMIT: usize = 12;

fn build_directory_tree(root: &Path) -> io::Result<BuiltDirectory> {
	// Fail closed on an unreadable root, but tolerate unreadable children.
	std::fs::read_dir(root)?;
	let mut root_node = DirectoryNode {
		name: ".".to_string(),
		is_dir: true,
		mtime: 0,
		size: 0,
		depth: 0,
		children: Vec::new(),
		dropped: 0,
	};
	fill_children(&mut root_node, root, 0);
	let mut rendered = Vec::new();
	render_node(&root_node, &mut rendered);
	let rendered = format_lines(&rendered);
	let truncated = count_dropped(&root_node) > 0;
	Ok(BuiltDirectory {
		total_lines: rendered.lines().count().max(1) as u64,
		truncated,
		rendered,
	})
}

fn count_dropped(node: &DirectoryNode) -> usize {
	node.dropped + node.children.iter().map(count_dropped).sum::<usize>()
}

fn fill_children(node: &mut DirectoryNode, path: &Path, depth: usize) {
	if depth >= READ_DIRECTORY_MAX_DEPTH {
		return;
	}
	let Ok(entries) = std::fs::read_dir(path) else {
		return;
	};
	let mut children = Vec::new();
	for entry in entries.flatten() {
		let Ok(metadata) = entry.metadata() else {
			continue;
		};
		let name = entry.file_name().to_string_lossy().into_owned();
		let is_dir = metadata.is_dir();
		let mut child = DirectoryNode {
			name,
			is_dir,
			mtime: metadata
				.modified()
				.ok()
				.and_then(|time| time.duration_since(UNIX_EPOCH).ok())
				.map(|duration| duration.as_secs())
				.unwrap_or(0),
			size: if is_dir { 0 } else { metadata.len() },
			depth: depth + 1,
			children: Vec::new(),
			dropped: 0,
		};
		if is_dir {
			fill_children(&mut child, &entry.path(), depth + 1);
		}
		children.push(child);
	}
	children.sort_by(|left, right| right.mtime.cmp(&left.mtime).then_with(|| left.name.cmp(&right.name)));

	let limit = if depth == 0 { None } else { Some(READ_DIRECTORY_CHILD_LIMIT) };
	if let Some(limit) = limit {
		if children.len() > limit {
			node.dropped = children.len() - limit;
			if limit <= 1 {
				children.truncate(limit);
			} else {
				let oldest = children.pop().expect("non-empty");
				children.truncate(limit - 1);
				children.push(oldest);
			}
		}
	}
	node.children = children;
}

struct RenderedLine {
	label: String,
	size: Option<String>,
	age: String,
}

fn render_node(node: &DirectoryNode, out: &mut Vec<RenderedLine>) {
	if node.depth == 0 {
		out.push(RenderedLine {
			label: node.name.clone(),
			size: None,
			age: String::new(),
		});
	} else {
		let indent = "  ".repeat(node.depth);
		let suffix = if node.is_dir { "/" } else { "" };
		out.push(RenderedLine {
			label: format!("{indent}- {}{suffix}", node.name),
			size: if node.is_dir { None } else { Some(format_bytes(node.size)) },
			age: format_age(node.mtime),
		});
	}

	if node.dropped == 0 {
		for child in &node.children {
			render_node(child, out);
		}
		return;
	}

	let (oldest, recent) = match node.children.split_last() {
		Some((oldest, recent)) => (oldest, recent),
		None => return,
	};
	for child in recent {
		render_node(child, out);
	}
	let marker_depth = node.depth + 1;
	out.push(RenderedLine {
		label: format!("{}- … {} more", "  ".repeat(marker_depth), node.dropped),
		size: None,
		age: String::new(),
	});
	render_node(oldest, out);
}

fn format_lines(lines: &[RenderedLine]) -> String {
	let max_label = lines.iter().map(|line| line.label.chars().count()).max().unwrap_or(0);
	lines
		.iter()
		.map(|line| {
			if line.age.is_empty() {
				return line.label.clone();
			}
			format!(
				"{}{}  {}",
				pad_end(&line.label, max_label + 2),
				pad_end(line.size.as_deref().unwrap_or(""), 8),
				line.age
			)
			.trim_end()
			.to_string()
		})
		.collect::<Vec<_>>()
		.join("\n")
}

fn pad_end(text: &str, width: usize) -> String {
	let length = text.chars().count();
	if length >= width {
		text.to_string()
	} else {
		format!("{text}{}", " ".repeat(width - length))
	}
}

fn format_bytes(bytes: u64) -> String {
	if bytes < 1024 {
		return format!("{bytes}B");
	}
	if bytes < 1024 * 1024 {
		return format!("{:.1}KB", bytes as f64 / 1024.0);
	}
	if bytes < 1024 * 1024 * 1024 {
		return format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0));
	}
	format!("{:.1}GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

fn format_age(mtime: u64) -> String {
	if mtime == 0 {
		return String::new();
	}
	let now = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|duration| duration.as_secs())
		.unwrap_or(0);
	let age_seconds = now.saturating_sub(mtime);
	if age_seconds == 0 {
		return String::new();
	}
	let mins = age_seconds / 60;
	let hours = mins / 60;
	let days = hours / 24;
	let weeks = days / 7;
	let months = days / 30;
	if months > 0 {
		return format!("{months}mo ago");
	}
	if weeks > 0 {
		return format!("{weeks}w ago");
	}
	if days > 0 {
		return format!("{days}d ago");
	}
	if hours > 0 {
		return format!("{hours}h ago");
	}
	if mins > 0 {
		return format!("{mins}m ago");
	}
	"just now".to_string()
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn is_cancelled(ctx: &AgentContext) -> bool {
	ctx.cancellation_token.is_cancelled()
}

fn result_with_details(content: String, details: Value, is_error: bool) -> ToolResult {
	ToolResult {
		tool_call_id: String::new(),
		content,
		is_error,
		terminate: false,
		details: Some(details),
	}
}
