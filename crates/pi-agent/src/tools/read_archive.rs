//! Archive reads through the Python 3 standard-library backend.
//!
//! Mirrors `packages/coding-agent/src/tools/read-archive.ts` and the format
//! table in `packages/utils/src/ar/registry.ts` for the formats this build can
//! serve without a new Rust dependency: ZIP containers (and their package
//! aliases), tar, `tar.gz`/`tgz`, `tar.bz2`/`tbz2`/`tbz`, `tar.xz`/`txz`, and
//! single-stream `gz`/`bz2`/`xz` members. Every other advertised suffix keeps
//! an explicit missing-backend refusal; nothing is silently treated as text.
//!
//! The helper (`read_archive_helper.py`, compiled in with `include_str!`) is
//! spawned as `python3 -I -B -c <source> <json>` in its own process group. It
//! returns one framed JSON header line plus, for a member read, the raw bytes.
//! Member contents never touch disk, symlinks/hardlinks resolve only inside the
//! in-memory index, and path traversal is rejected before any lookup. Rust
//! re-validates every metadata field and caps actual stdout bytes, so a lying
//! declared size cannot allocate past the contract limits (1M entries, 64 MiB
//! index/member, 256 MiB in-memory stream, 4096-byte paths, 40 link rewrites).
//!
//! Cancellation and the wall-clock bound kill the whole helper group and reap
//! it. Rendering reuses the ordinary read pipeline: archive members are
//! immutable (no hashline tag), and directory listings share the 50 KiB head
//! truncation and result-limit metadata of filesystem listings.
//!
//! `rar`, `7z`, `iso`, `cab`, `deb`, `rpm`, `cpio`, `ar`, `lzh`, `arj`,
//! `asar`, `zst`, `Z` and `lzma` remain open slices with a named prerequisite;
//! PDF/DOCX conversion is a separate backend and stays out of this path.

use std::{
	collections::HashSet,
	path::{Path, PathBuf},
	process::Stdio,
	time::Duration,
};

use serde_json::{Map, Value, json};
use tokio::{
	io::{AsyncRead, AsyncReadExt},
	process::{Child, Command},
	task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use super::{
	read::{
		DEFAULT_MAX_BYTES, LineRange, ReadTool, Sel, TextSource, find_unique_workspace_suffix,
		format_bytes, is_not_found, is_raw, meta_truncation_json, parse_sel, prepend_suffix_notice,
		read_multi_range, read_single_range, resolve_path, selector_offset_limit,
		split_path_and_sel, truncate_head, truncation_json,
	},
	tool_result,
};
use crate::state::{AgentContext, ToolResult};

const HELPER_SCRIPT: &str = include_str!("read_archive_helper.py");
const HELPER_TIMEOUT: Duration = Duration::from_secs(120);
const STDERR_CAP: usize = 64 * 1024;
/// Framed metadata ceiling; a 5000-entry listing of 4096-byte names fits.
const HEADER_CAP: usize = 32 * 1024 * 1024;
/// Declared payload ceiling mirrored from the helper contract.
const MAX_MEMBER_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 1_000_000;
const MAX_PATH_BYTES: usize = 4096;
const STDOUT_CAP: usize = HEADER_CAP + MAX_MEMBER_BYTES as usize;

/// Suffixes this build can read, mapped to their helper format. Longest
/// extension first within a path so `.tar.gz` never degrades to bare `gz`.
const SUPPORTED_EXTENSIONS: &[(&str, &str)] = &[
	(".tar.bz2", "tar.bz2"),
	(".tbz2", "tar.bz2"),
	(".tbz", "tar.bz2"),
	(".tar.gz", "tar.gz"),
	(".tgz", "tar.gz"),
	(".tar.xz", "tar.xz"),
	(".txz", "tar.xz"),
	(".nupkg", "zip"),
	(".vsix", "zip"),
	(".apk", "zip"),
	(".whl", "zip"),
	(".ipa", "zip"),
	(".xpi", "zip"),
	(".cbz", "zip"),
	(".jar", "zip"),
	(".war", "zip"),
	(".ear", "zip"),
	(".zip", "zip"),
	(".tar", "tar"),
	(".bz2", "bz2"),
	(".gz", "gz"),
	(".xz", "xz"),
];

/// Recognised but not bundled here; the refusal names the missing backend.
const UNSUPPORTED_EXTENSIONS: &[(&str, &str)] = &[
	(".tar.zst", "zstd"),
	(".tzst", "zstd"),
	(".tar.z", "compress"),
	(".rar", "rar"),
	(".cbr", "rar"),
	(".7z", "7z"),
	(".iso", "iso"),
	(".cab", "cab"),
	(".deb", "deb"),
	(".rpm", "rpm"),
	(".cpio", "cpio"),
	(".lzh", "lzh"),
	(".lha", "lzh"),
	(".arj", "arj"),
	(".asar", "asar"),
	(".lib", "ar"),
	(".lzma", "lzma"),
	(".zst", "zstd"),
	(".z", "compress"),
	(".ar", "ar"),
	(".a", "ar"),
];

struct ArchiveCandidate {
	archive_path:          String,
	sub_path:              String,
	format:                Option<&'static str>,
	unsupported_backend:   Option<&'static str>,
}

/// Resolved archive target and member selector remainder.
pub(super) struct ResolvedArchivePath {
	pub absolute_path:     PathBuf,
	pub archive_sub_path:  String,
	pub format:            String,
	pub suffix_resolution: Option<(String, String)>,
}

pub(super) enum ArchiveRoute {
	Read(ResolvedArchivePath),
	UnsupportedBackend { target: PathBuf, backend: String },
}

/// Split `archive.ext:inner/path:selector` into every plausible archive path,
/// longest first, exactly like `parseArchivePathCandidates`.
fn parse_candidates(raw: &str) -> Vec<ArchiveCandidate> {
	let normalized = raw.replace('\\', "/");
	let lower = normalized.to_ascii_lowercase();
	let bytes = lower.as_bytes();
	let mut candidates = Vec::new();
	let mut seen = HashSet::new();
	let mut index = 0usize;
	while index < bytes.len() {
		if bytes[index] != b'.' {
			index += 1;
			continue;
		}
		for (extension, format) in SUPPORTED_EXTENSIONS {
			let end = index + extension.len();
			if end > bytes.len() || &bytes[index..end] != extension.as_bytes() {
				continue;
			}
			if end < bytes.len() && bytes[end] != b':' {
				continue;
			}
			push_candidate(
				&mut candidates,
				&mut seen,
				raw,
				&normalized,
				end,
				Some(*format),
				None,
			);
		}
		for (extension, backend) in UNSUPPORTED_EXTENSIONS {
			let end = index + extension.len();
			if end > bytes.len() || &bytes[index..end] != extension.as_bytes() {
				continue;
			}
			if end < bytes.len() && bytes[end] != b':' {
				continue;
			}
			push_candidate(
				&mut candidates,
				&mut seen,
				raw,
				&normalized,
				end,
				None,
				Some(*backend),
			);
		}
		index += 1;
	}
	candidates.sort_by(|left, right| {
		right
			.archive_path
			.len()
			.cmp(&left.archive_path.len())
			.then_with(|| right.sub_path.len().cmp(&left.sub_path.len()))
	});
	candidates
}

#[allow(clippy::too_many_arguments)]
fn push_candidate(
	candidates: &mut Vec<ArchiveCandidate>,
	seen: &mut HashSet<(String, String)>,
	raw: &str,
	normalized: &str,
	end: usize,
	format: Option<&'static str>,
	unsupported_backend: Option<&'static str>,
) {
	let archive_path = raw[..end].to_string();
	let sub_path = normalized[end..].trim_start_matches(':').to_string();
	if !seen.insert((archive_path.clone(), sub_path.clone())) {
		return;
	}
	candidates.push(ArchiveCandidate {
		archive_path,
		sub_path,
		format,
		unsupported_backend,
	});
}

/// Resolve the authored path to an existing archive target. Literal whole-path
/// precedence is enforced by the caller before this runs.
pub(super) async fn resolve_archive_read_path(
	raw: &str,
	ctx: &AgentContext,
) -> Result<Option<ArchiveRoute>, String> {
	for candidate in parse_candidates(raw) {
		let mut target = resolve_path(ctx, &candidate.archive_path);
		let mut suffix_resolution = None;
		let meta = match tokio::fs::metadata(&target).await {
			Ok(meta) => meta,
			Err(error) => {
				if !is_not_found(&error) || is_remote_mount(&target, ctx) {
					continue;
				}
				let Some((found, display)) =
					find_unique_workspace_suffix(&candidate.archive_path, ctx).await
				else {
					continue;
				};
				match tokio::fs::metadata(&found).await {
					Ok(meta) => {
						target = found;
						suffix_resolution = Some((candidate.archive_path.clone(), display));
						meta
					},
					Err(retry) => {
						if !is_not_found(&retry) {
							return Err(retry.to_string());
						}
						continue;
					},
				}
			},
		};
		// Only regular files are archives; a FIFO/device must fall through to
		// the plain read path, which refuses it without blocking on open.
		if !meta.is_file() {
			continue;
		}
		if let Some(format) = candidate.format {
			return Ok(Some(ArchiveRoute::Read(ResolvedArchivePath {
				absolute_path: target,
				archive_sub_path: candidate.sub_path,
				format: format.to_string(),
				suffix_resolution,
			})));
		}
		if let Some(backend) = candidate.unsupported_backend {
			return Ok(Some(ArchiveRoute::UnsupportedBackend {
				target,
				backend: backend.to_string(),
			}));
		}
	}
	Ok(None)
}

/// Upstream skips suffix matching under the sshfs remote mount to avoid hangs.
/// This crate has no XDG/profile-aware dirs layer, so the default
/// `$HOME/.omp/remote` root is approximated here; an XDG-relocated root only
/// costs the bounded suffix-scan timeout, never a hang.
fn is_remote_mount(path: &Path, ctx: &AgentContext) -> bool {
	let Some(home) = ctx.env.get("HOME").filter(|home| !home.is_empty()) else {
		return false;
	};
	path.starts_with(Path::new(home).join(".omp").join("remote"))
}

// ---------------------------------------------------------------------------
// Tool entry point
// ---------------------------------------------------------------------------

/// Read an archive directory or member using the caller's selector.
pub(super) async fn read_archive(
	resolved: ResolvedArchivePath,
	authored: &str,
	tool: &ReadTool,
	ctx: &AgentContext,
) -> ToolResult {
	if ctx.cancellation_token.is_cancelled() {
		return tool_result("Read cancelled", true);
	}
	let archive_display = resolved.absolute_path.display().to_string();
	let format = resolved.format.clone();
	let suffix = resolved.suffix_resolution.clone();
	let sub_path = resolved.archive_sub_path.clone();

	let (mut member, mut parsed) = match split_path_and_sel(&sub_path) {
		Some(split) => match parse_sel(Some(&split.sel)) {
			Ok(sel) => (split.path, sel),
			Err(message) => return tool_result(message, true),
		},
		None => (sub_path.clone(), Sel::None),
	};

	let mut reply = match fetch(&format, &archive_display, &member, &parsed, ctx).await {
		Ok(reply) => reply,
		Err(HelperError::Cancelled) => return tool_result("Read cancelled", true),
		Err(HelperError::Message(message)) => return tool_result(message, true),
	};
	if matches!(reply, ArchiveReply::NotFound) && !sub_path.is_empty() {
		// `archive.zip:500` / `archive.zip:raw`: the whole remainder is a root
		// selector, not a member name. A real member keeps precedence above.
		if let Ok(whole) = parse_sel(Some(&sub_path))
			&& whole != Sel::None
		{
			member.clear();
			parsed = whole;
			reply = match fetch(&format, &archive_display, &member, &parsed, ctx).await {
				Ok(reply) => reply,
				Err(HelperError::Cancelled) => return tool_result("Read cancelled", true),
				Err(HelperError::Message(message)) => return tool_result(message, true),
			};
		}
	}

	match reply {
		ArchiveReply::NotFound => {
			tool_result(format!("Path '{authored}' not found inside archive"), true)
		},
		ArchiveReply::Dir { entries, limit } => {
			read_archive_directory(&archive_display, &parsed, entries, limit, suffix.as_ref())
		},
		ArchiveReply::File { path, size, bytes } => read_archive_file(
			&archive_display,
			&path,
			size,
			&bytes,
			&parsed,
			tool,
			suffix.as_ref(),
			ctx,
		)
		.await,
	}
}

fn read_archive_directory(
	archive: &str,
	parsed: &Sel,
	entries: Vec<ListEntry>,
	limit: Option<u64>,
	suffix: Option<&(String, String)>,
) -> ToolResult {
	if let Sel::Lines { ranges, .. } = parsed
		&& ranges.len() > 1
	{
		return tool_result(
			"Multi-range line selectors are not supported for archive directory listings.",
			true,
		);
	}
	let lines: Vec<String> = entries
		.iter()
		.map(|entry| {
			if entry.is_dir {
				format!("{}/", entry.name)
			} else if entry.size > 0 {
				format!("{} ({})", entry.name, format_bytes(entry.size))
			} else {
				entry.name.clone()
			}
		})
		.collect();
	let output = if lines.is_empty() {
		"(empty archive directory)".to_string()
	} else {
		lines.join("\n")
	};
	let text = prepend_suffix_notice(output, suffix);
	let head = truncate_head(&text, DEFAULT_MAX_BYTES);
	let truncated = head.total_bytes > DEFAULT_MAX_BYTES;

	let mut details = Map::new();
	details.insert("isDirectory".into(), json!(true));
	details.insert("resolvedPath".into(), json!(archive));
	let mut meta = Map::new();
	meta.insert(
		"source".into(),
		json!({ "type": "path", "value": archive }),
	);
	if let Some(reached) = limit {
		meta.insert(
			"limits".into(),
			json!({ "resultLimit": { "reached": reached, "suggestion": reached.saturating_mul(2) } }),
		);
	}
	if truncated {
		details.insert(
			"truncation".into(),
			truncation_json(
				&head.content,
				true,
				Some(head.truncated_by),
				head.total_lines,
				head.total_bytes,
				head.output_lines,
				head.output_bytes,
				head.first_line_exceeds,
			),
		);
		meta.insert(
			"truncation".into(),
			meta_truncation_json(
				head.truncated_by,
				head.total_lines,
				head.total_bytes,
				head.output_lines,
				head.output_bytes,
				1,
				head.output_lines,
			),
		);
	}
	details.insert("meta".into(), Value::Object(meta));
	if let Some((from, to)) = suffix {
		details.insert("suffixResolution".into(), json!({ "from": from, "to": to }));
	}
	super::read::result_with_details(head.content, Value::Object(details), false)
}

#[allow(clippy::too_many_arguments)]
async fn read_archive_file(
	archive: &str,
	path: &str,
	size: u64,
	bytes: &[u8],
	parsed: &Sel,
	tool: &ReadTool,
	suffix: Option<&(String, String)>,
	ctx: &AgentContext,
) -> ToolResult {
	// Upstream `decodeUtf8Text` rejects NUL bytes and invalid UTF-8 alike; both
	// render the binary notice instead of a lossy decode.
	let text = !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok();
	if !text {
		let mut details = Map::new();
		details.insert("resolvedPath".into(), json!(archive));
		details.insert("fileSize".into(), json!(size));
		details.insert(
			"meta".into(),
			json!({ "source": { "type": "path", "value": archive } }),
		);
		if let Some((from, to)) = suffix {
			details.insert("suffixResolution".into(), json!({ "from": from, "to": to }));
		}
		return super::read::result_with_details(
			prepend_suffix_notice(
				format!(
					"[Cannot read binary archive entry '{path}' ({})]",
					format_bytes(size)
				),
				suffix,
			),
			Value::Object(details),
			false,
		);
	}

	let source = TextSource::Memory {
		url: archive,
		bytes,
		path: None,
	};
	let raw = is_raw(parsed);
	let display_mode = tool.display_mode(raw, true);
	let outcome = match parsed {
		Sel::Lines { ranges, raw } if ranges.len() > 1 => {
			read_multi_range(&source, ranges, *raw, None, display_mode, ctx).await
		},
		Sel::Lines { ranges, raw } => {
			read_single_range(&source, ranges[0], *raw, None, display_mode, ctx).await
		},
		Sel::Raw | Sel::None | Sel::Conflicts | Sel::Image => {
			read_single_range(
				&source,
				LineRange { start_line: 1, end_line: None },
				raw,
				None,
				display_mode,
				ctx,
			)
			.await
		},
	};
	match outcome {
		Ok(mut result) => {
			finalize_details(&mut result, archive, suffix);
			result.content = prepend_suffix_notice(result.content, suffix);
			result
		},
		Err(message) => tool_result(message, true),
	}
}

/// Archive members are plain path-backed text: keep `resolvedPath` on the
/// archive and make `meta.source` a real path even though the read was
/// in-memory.
fn finalize_details(result: &mut ToolResult, archive: &str, suffix: Option<&(String, String)>) {
	let Some(Value::Object(details)) = result.details.as_mut() else {
		return;
	};
	details.insert("resolvedPath".into(), json!(archive));
	let mut meta = match details.remove("meta") {
		Some(Value::Object(meta)) => meta,
		_ => Map::new(),
	};
	meta.insert(
		"source".into(),
		json!({ "type": "path", "value": archive }),
	);
	details.insert("meta".into(), Value::Object(meta));
	if let Some((from, to)) = suffix {
		details.insert("suffixResolution".into(), json!({ "from": from, "to": to }));
	}
}

// ---------------------------------------------------------------------------
// Helper protocol
// ---------------------------------------------------------------------------

struct ListEntry {
	name:   String,
	is_dir: bool,
	size:   u64,
}

enum ArchiveReply {
	Dir { entries: Vec<ListEntry>, limit: Option<u64> },
	File { path: String, size: u64, bytes: Vec<u8> },
	NotFound,
}

enum HelperError {
	Cancelled,
	Message(String),
}

async fn fetch(
	format: &str,
	archive: &str,
	member: &str,
	parsed: &Sel,
	ctx: &AgentContext,
) -> Result<ArchiveReply, HelperError> {
	let (offset, limit) = selector_offset_limit(parsed);
	let request = json!({
		"format": format,
		"archive": archive,
		"path": member,
		"offset": offset,
		"limit": limit,
	});
	let reply = run_helper(&request, ctx).await?;
	interpret(reply)
}

struct HelperReply {
	header:  Value,
	payload: Vec<u8>,
}

async fn run_helper(request: &Value, ctx: &AgentContext) -> Result<HelperReply, HelperError> {
	if ctx.cancellation_token.is_cancelled() {
		return Err(HelperError::Cancelled);
	}
	let encoded = serde_json::to_string(request)
		.map_err(|error| HelperError::Message(format!("Archive request encoding failed: {error}")))?;
	let mut command = Command::new("python3");
	command
		.args(["-I", "-B", "-c", HELPER_SCRIPT])
		.arg(encoded)
		.current_dir(&ctx.cwd)
		.env_clear()
		.envs(&ctx.env)
		.stdin(Stdio::null())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.kill_on_drop(true);
	#[cfg(unix)]
	command.process_group(0);
	let mut child = command.spawn().map_err(|error| {
		if error.kind() == std::io::ErrorKind::NotFound {
			// NotFound covers both a missing `python3` and a missing cwd, so
			// report the real cause rather than asserting one of them.
			HelperError::Message(format!(
				"Cannot start the archive backend (`python3`) in '{}': {error}. Archive reads need Python 3 on PATH and an existing working directory.",
				ctx.cwd.display()
			))
		} else {
			HelperError::Message(format!("Cannot start archive backend: {error}"))
		}
	})?;
	let pid = child.id();
	let stdout = child
		.stdout
		.take()
		.ok_or_else(|| HelperError::Message("Archive backend stdout unavailable".to_string()))?;
	let stderr = child
		.stderr
		.take()
		.ok_or_else(|| HelperError::Message("Archive backend stderr unavailable".to_string()))?;

	let stop = CancellationToken::new();
	let stdout_task = spawn_stdout(stdout, stop.clone());
	let stderr_task = spawn_stderr(stderr);

	enum Stop {
		Exited(std::process::ExitStatus),
		Failed(String),
	}
	let stop_reason = tokio::select! {
		biased;
		() = ctx.cancellation_token.cancelled() => Stop::Failed("Read cancelled".to_string()),
		() = tokio::time::sleep(HELPER_TIMEOUT) => Stop::Failed(format!(
			"Archive backend timed out after {}s",
			HELPER_TIMEOUT.as_secs()
		)),
		() = stop.cancelled() => Stop::Failed(String::new()),
		status = child.wait() => match status {
			Ok(status) => Stop::Exited(status),
			Err(error) => Stop::Failed(format!("Archive backend wait failed: {error}")),
		},
	};
	let mut failure = match stop_reason {
		Stop::Exited(status) => {
			if status.success() {
				None
			} else {
				Some(format!("Archive backend exited with status {status}"))
			}
		},
		Stop::Failed(message) => Some(message),
	};
	if failure.is_some() {
		kill_group(&mut child, pid).await;
	}
	let stdout_result = join_stdout(stdout_task).await;
	let stderr_text = join_stderr(stderr_task).await;
	if let Some(message) = failure.take() {
		if message.is_empty() {
			return Err(HelperError::Message(
				stdout_result.err().unwrap_or_else(|| {
					"Archive backend output exceeded its byte limit".to_string()
				}),
			));
		}
		if message == "Read cancelled" {
			return Err(HelperError::Cancelled);
		}
		return Err(HelperError::Message(message));
	}
	let output = match stdout_result {
		Ok(output) => output,
		Err(message) => return Err(HelperError::Message(message)),
	};
	parse_reply(output).map_err(|message| {
		let stderr_text = stderr_text.trim();
		if stderr_text.is_empty() {
			HelperError::Message(message)
		} else {
			HelperError::Message(format!("{message}: {stderr_text}"))
		}
	})
}

fn spawn_stdout(
	mut stdout: tokio::process::ChildStdout,
	stop: CancellationToken,
) -> JoinHandle<Result<Vec<u8>, String>> {
	tokio::spawn(async move {
		let result = read_bounded(&mut stdout, STDOUT_CAP).await;
		if result.is_err() {
			stop.cancel();
		}
		result
	})
}

fn spawn_stderr(mut stderr: tokio::process::ChildStderr) -> JoinHandle<String> {
	tokio::spawn(async move {
		match read_bounded(&mut stderr, STDERR_CAP).await {
			Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
			Err(_) => "stderr exceeded its byte limit".to_string(),
		}
	})
}

async fn join_stdout(
	task: JoinHandle<Result<Vec<u8>, String>>,
) -> Result<Vec<u8>, String> {
	task.await
		.unwrap_or_else(|error| Err(format!("Archive backend reader failed: {error}")))
}

async fn join_stderr(task: JoinHandle<String>) -> String {
	task.await
		.unwrap_or_else(|error| format!("Archive backend stderr reader failed: {error}"))
}

async fn read_bounded<R: AsyncRead + Unpin>(
	reader: &mut R,
	cap: usize,
) -> Result<Vec<u8>, String> {
	let mut output = Vec::new();
	let mut buffer = vec![0u8; 64 * 1024];
	loop {
		let read = reader
			.read(&mut buffer)
			.await
			.map_err(|error| format!("Archive backend pipe failed: {error}"))?;
		if read == 0 {
			break;
		}
		if output.len() + read > cap {
			return Err(format!(
				"Archive backend output exceeded {} bytes",
				format_bytes(cap as u64)
			));
		}
		output.extend_from_slice(&buffer[..read]);
	}
	Ok(output)
}

async fn kill_group(child: &mut Child, pid: Option<u32>) {
	#[cfg(unix)]
	if let Some(pid) = pid.filter(|pid| *pid > 1) {
		// SAFETY: signalling the process group this helper leads; guarded
		// against the catastrophic 0/1 targets.
		unsafe {
			libc::kill(-(pid as i32), libc::SIGKILL);
		}
	}
	let _ = child.start_kill();
	let _ = child.wait().await;
}

fn parse_reply(mut output: Vec<u8>) -> Result<HelperReply, String> {
	let newline = output
		.iter()
		.position(|byte| *byte == b'\n')
		.ok_or_else(|| "Archive backend returned no framed header".to_string())?;
	if newline > HEADER_CAP {
		return Err(format!(
			"Archive backend metadata exceeded {} limit",
			format_bytes(HEADER_CAP as u64)
		));
	}
	// Split once: the payload can be a 64 MiB member, so never copy the tail.
	let payload = output.split_off(newline + 1);
	let header: Value = serde_json::from_slice(&output[..newline])
		.map_err(|error| format!("Archive backend returned malformed metadata: {error}"))?;
	if !header.is_object() {
		return Err("Archive backend returned malformed metadata".to_string());
	}
	Ok(HelperReply { header, payload })
}

fn malformed() -> HelperError {
	HelperError::Message("Archive backend returned malformed metadata".to_string())
}

fn interpret(reply: HelperReply) -> Result<ArchiveReply, HelperError> {
	let HelperReply { header, payload } = reply;
	let object = header.as_object().ok_or_else(malformed)?;
	let ok = object
		.get("ok")
		.and_then(Value::as_bool)
		.ok_or_else(malformed)?;
	if !ok {
		let kind = object.get("kind").and_then(Value::as_str).unwrap_or("");
		let message = object
			.get("error")
			.and_then(Value::as_str)
			.ok_or_else(malformed)?;
		if kind == "not_found" {
			return Ok(ArchiveReply::NotFound);
		}
		return Err(HelperError::Message(format!("Archive read failed: {message}")));
	}
	match object.get("type").and_then(Value::as_str) {
		Some("dir") => {
			if !payload.is_empty() {
				return Err(malformed());
			}
			let raw_entries = object
				.get("entries")
				.and_then(Value::as_array)
				.ok_or_else(malformed)?;
			if raw_entries.len() > MAX_ENTRIES {
				return Err(malformed());
			}
			let mut entries = Vec::with_capacity(raw_entries.len());
			for raw in raw_entries {
				let entry = raw.as_object().ok_or_else(malformed)?;
				let name = entry
					.get("name")
					.and_then(Value::as_str)
					.filter(|name| !name.is_empty())
					.ok_or_else(malformed)?;
				if name.len() > MAX_PATH_BYTES {
					return Err(malformed());
				}
				let is_dir = entry
					.get("is_dir")
					.and_then(Value::as_bool)
					.ok_or_else(malformed)?;
				let size = entry
					.get("size")
					.and_then(Value::as_u64)
					.filter(|size| *size <= MAX_MEMBER_BYTES)
					.ok_or_else(malformed)?;
				entries.push(ListEntry {
					name: name.to_string(),
					is_dir,
					size,
				});
			}
			let limit = match object.get("limit") {
				None | Some(Value::Null) => None,
				Some(value) => Some(value.as_u64().filter(|limit| *limit > 0).ok_or_else(malformed)?),
			};
			Ok(ArchiveReply::Dir { entries, limit })
		},
		Some("file") => {
			let path = object
				.get("path")
				.and_then(Value::as_str)
				.filter(|path| !path.is_empty() && path.len() <= MAX_PATH_BYTES)
				.ok_or_else(malformed)?;
			let size = object
				.get("size")
				.and_then(Value::as_u64)
				.filter(|size| *size <= MAX_MEMBER_BYTES)
				.ok_or_else(malformed)?;
			if payload.len() as u64 != size {
				return Err(malformed());
			}
			Ok(ArchiveReply::File {
				path: path.to_string(),
				size,
				bytes: payload,
			})
		},
		_ => Err(malformed()),
	}
}
