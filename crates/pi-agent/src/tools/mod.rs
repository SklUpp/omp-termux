pub mod registry;
pub mod scheduler;

use std::{
	io::Write,
	path::{Path, PathBuf},
	sync::{
		Arc, Mutex,
		atomic::{AtomicU64, Ordering},
	},
};

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use crate::state::{AgentContext, ToolResult};

/// Captured stdout+stderr budget for one bash call.
const MAX_BASH_OUTPUT_BYTES: usize = 64 * 1024;
/// Default and ceiling for the bash timeout.
const DEFAULT_BASH_TIMEOUT_MS: u64 = 300_000;
const MAX_BASH_TIMEOUT_MS: u64 = 600_000;
/// Bytes returned by one `read` call.
const MAX_READ_BYTES: usize = 256 * 1024;
/// Bytes accepted by one `write` call.
const MAX_WRITE_BYTES: usize = 1024 * 1024;
/// Bytes accepted by one `edit` call: the whole file is read and rewritten.
const MAX_EDIT_BYTES: usize = 4 * 1024 * 1024;
/// Characters of the search string echoed back in an `edit` error.
const MAX_EDIT_PREVIEW_CHARS: usize = 120;
/// Entries returned by `glob`.
const MAX_GLOB_ENTRIES: usize = 1000;
/// Files scanned and bytes read per file by `grep`.
const MAX_GREP_FILES: usize = 500;
const MAX_GREP_FILE_BYTES: usize = 256 * 1024;
/// Matches and characters returned by `grep`.
const MAX_GREP_MATCHES: usize = 100;
const MAX_GREP_OUTPUT_BYTES: usize = 32 * 1024;
/// Characters of a single matched line kept in `grep` output.
const MAX_GREP_LINE_CHARS: usize = 200;

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
	#[error("Invalid arguments: {0}")]
	InvalidArguments(String),
	#[error("Execution failed: {0}")]
	ExecutionFailed(String),
}

/// Dynamic trait implemented by all agent tools.
#[async_trait]
pub trait Tool: Send + Sync {
	fn name(&self) -> &str;
	fn description(&self) -> &str;
	fn schema(&self) -> Value;
	fn is_exclusive(&self) -> bool {
		false
	}
	async fn execute(&self, args: &Value, ctx: &AgentContext) -> Result<ToolResult, ToolError>;
}

/// String buffer that stops growing past `cap` and counts what it dropped.
struct BoundedText {
	text: String,
	dropped: usize,
	cap: usize,
}

impl BoundedText {
	fn new(cap: usize) -> Self {
		Self { text: String::with_capacity(cap.min(8 * 1024)), dropped: 0, cap }
	}

	fn push(&mut self, chunk: &str) {
		let room = self.cap.saturating_sub(self.text.len());
		if chunk.len() <= room {
			self.text.push_str(chunk);
			return;
		}
		let cut = floor_char_boundary(chunk, room);
		self.text.push_str(&chunk[..cut]);
		self.dropped += chunk.len() - cut;
	}

	fn finish(self) -> String {
		if self.dropped == 0 {
			self.text
		} else {
			format!("{}\n[output truncated: {} more bytes dropped]", self.text, self.dropped)
		}
	}
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
	if index >= text.len() {
		return text.len();
	}
	while index > 0 && !text.is_char_boundary(index) {
		index -= 1;
	}
	index
}

/// Read at most `cap` bytes from a regular file.
async fn read_capped(path: &Path, cap: usize) -> std::io::Result<(Vec<u8>, u64)> {
	let file = tokio::fs::File::open(path).await?;
	let mut bytes = Vec::new();
	file.take(cap as u64).read_to_end(&mut bytes).await?;
	let size = tokio::fs::metadata(path).await?.len();
	Ok((bytes, size))
}

/// Append the skipped-file count to a grep result, when there is one.
fn skip_note(base: &str, skipped: usize) -> String {
	if skipped == 0 {
		base.to_string()
	} else {
		format!("{base} ({skipped} file(s) skipped: unreadable or above the size limit)")
	}
}

fn tool_result(content: impl Into<String>, is_error: bool) -> ToolResult {
	ToolResult { tool_call_id: String::new(), content: content.into(), is_error }
}

/// Builtin bash execution tool.
pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
	fn name(&self) -> &str {
		"bash"
	}

	fn description(&self) -> &str {
		"Execute a shell command within the working directory"
	}

	fn schema(&self) -> Value {
		serde_json::json!({
			"type": "object",
			"properties": {
				"command": {
					"type": "string",
					"description": "Shell command to run"
				},
				"timeout_ms": {
					"type": "number",
					"description": "Optional timeout in milliseconds"
				}
			},
			"required": ["command"]
		})
	}

	fn is_exclusive(&self) -> bool {
		true
	}

	async fn execute(&self, args: &Value, ctx: &AgentContext) -> Result<ToolResult, ToolError> {
		let command = args
			.get("command")
			.and_then(Value::as_str)
			.ok_or_else(|| ToolError::InvalidArguments("missing 'command' parameter".to_string()))?;

		let timeout_ms = args
			.get("timeout_ms")
			.and_then(Value::as_u64)
			.unwrap_or(DEFAULT_BASH_TIMEOUT_MS)
			.min(MAX_BASH_TIMEOUT_MS) as u32;

		let options = pi_shell::ShellExecuteOptions {
			command: command.to_string(),
			cwd: Some(ctx.cwd.to_string_lossy().to_string()),
			env: Some(ctx.env.clone()),
			session_env: None,
			timeout_ms: Some(timeout_ms),
			snapshot_path: None,
			minimizer: None,
		};

		// Bridge the run's cancellation into the shell's own abort flag.
		let mut cancel_token = pi_shell::cancel::CancelToken::new(Some(timeout_ms));
		let abort_token = cancel_token.emplace_abort_token();
		let hub_cancel = ctx.cancellation_token.clone();
		let watcher = tokio::spawn(async move {
			hub_cancel.cancelled().await;
			abort_token.abort(pi_shell::cancel::AbortReason::Signal);
		});

		// Bounded sink: the drain task consumes every chunk (so the child never
		// blocks on a full pipe) but keeps only `MAX_BASH_OUTPUT_BYTES` of it.
		let (tx, rx) = flume::unbounded::<String>();
		let drain = tokio::spawn(async move {
			let mut sink = BoundedText::new(MAX_BASH_OUTPUT_BYTES);
			while let Ok(chunk) = rx.recv_async().await {
				sink.push(&chunk);
			}
			sink.finish()
		});

		let outcome = pi_shell::execute_shell(options, Some(tx), cancel_token).await;
		watcher.abort();
		let captured = match drain.await {
			Ok(captured) => captured,
			Err(err) => {
				return Ok(tool_result(format!("bash output collector failed: {err}"), true));
			},
		};

		let result = match outcome {
			Ok(result) => result,
			Err(err) => {
				return Ok(tool_result(format!("Error running bash command: {err}\n{captured}"), true));
			},
		};

		let status = format!(
			"exit code: {} | cancelled: {} | timed out: {}",
			result.exit_code.unwrap_or(-1),
			result.cancelled,
			result.timed_out
		);
		let content = if captured.is_empty() {
			status
		} else {
			format!("{captured}\n[{status}]")
		};
		// A missing exit code means the child was killed, never success.
		let is_error = result.exit_code != Some(0) || result.cancelled || result.timed_out;
		Ok(tool_result(content, is_error))
	}
}

/// Builtin file reading tool.
pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
	fn name(&self) -> &str {
		"read"
	}

	fn description(&self) -> &str {
		"Read the contents of a regular file"
	}

	fn schema(&self) -> Value {
		serde_json::json!({
			"type": "object",
			"properties": {
				"path": {
					"type": "string",
					"description": "Path to file to read"
				}
			},
			"required": ["path"]
		})
	}

	async fn execute(&self, args: &Value, ctx: &AgentContext) -> Result<ToolResult, ToolError> {
		let path = args
			.get("path")
			.and_then(Value::as_str)
			.ok_or_else(|| ToolError::InvalidArguments("missing 'path' parameter".to_string()))?;
		let target = ctx.cwd.join(path);

		let meta = match tokio::fs::metadata(&target).await {
			Ok(meta) => meta,
			Err(err) => {
				return Ok(tool_result(format!("Failed to read {}: {err}", target.display()), true));
			},
		};
		if !meta.is_file() {
			return Ok(tool_result(format!("{} is not a regular file", target.display()), true));
		}

		let (bytes, size) = read_capped(&target, MAX_READ_BYTES)
			.await
			.map_err(|err| ToolError::ExecutionFailed(format!("{}: {err}", target.display())))?;
		let mut content = String::from_utf8_lossy(&bytes).into_owned();
		if size > bytes.len() as u64 {
			content.push_str(&format!("\n[truncated at {} bytes of {size}]", bytes.len()));
		}
		Ok(tool_result(content, false))
	}
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Guard that removes a temporary file on drop unless disarmed.
struct TempFileGuard {
	path: Option<PathBuf>,
}

impl TempFileGuard {
	fn new(path: PathBuf) -> Self {
		Self { path: Some(path) }
	}

	fn disarm(&mut self) {
		self.path = None;
	}
}

impl Drop for TempFileGuard {
	fn drop(&mut self) {
		if let Some(path) = self.path.take() {
			let _ = std::fs::remove_file(path);
		}
	}
}

#[derive(Default)]
struct CommitState {
	cancelled: bool,
	committed: bool,
}

/// Drops the write token on future cancellation/drop so detached workers cannot rename.
struct AbortGuard {
	token: Option<CancellationToken>,
	state: Option<Arc<Mutex<CommitState>>>,
}

impl AbortGuard {
	fn new(token: CancellationToken, state: Arc<Mutex<CommitState>>) -> Self {
		Self {
			token: Some(token),
			state: Some(state),
		}
	}

	fn disarm(&mut self) {
		self.token = None;
		self.state = None;
	}
}

impl Drop for AbortGuard {
	fn drop(&mut self) {
		if let Some(token) = self.token.take() {
			token.cancel();
		}
		if let Some(state) = self.state.take() {
			let mut lock = state.lock().unwrap_or_else(|p| p.into_inner());
			if lock.committed {
				return;
			}
			lock.cancelled = true;
		}
	}
}

async fn resolve_symlink_target(mut path: PathBuf) -> std::io::Result<PathBuf> {
	for _ in 0..32 {
		match tokio::fs::symlink_metadata(&path).await {
			Ok(meta) if meta.file_type().is_symlink() => {
				let link = tokio::fs::read_link(&path).await?;
				path = if link.is_relative() {
					match path.parent() {
						Some(p) => p.join(link),
						None => link,
					}
				} else {
					link
				};
			},
			_ => return Ok(path),
		}
	}
	Err(std::io::Error::new(
		std::io::ErrorKind::Other,
		"too many levels of symbolic links",
	))
}

fn create_exclusive_temp(
	target: &Path,
	existing_permissions: &Option<std::fs::Permissions>,
) -> std::io::Result<(PathBuf, std::fs::File)> {
	let dir = target
		.parent()
		.filter(|p| !p.as_os_str().is_empty())
		.unwrap_or_else(|| Path::new("."));
	let base = target
		.file_name()
		.and_then(|n| n.to_str())
		.unwrap_or("file");

	#[cfg(unix)]
	let mode = match existing_permissions {
		Some(perms) => {
			use std::os::unix::fs::PermissionsExt;
			perms.mode() & 0o777
		},
		None => 0o666,
	};

	for _ in 0..64 {
		let nanos = std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.map_or(0, |d| d.as_nanos() as u64);
		let tag = nanos
			.wrapping_mul(0x517c_c1b7_2722_0a95)
			.wrapping_add(TEMP_COUNTER.fetch_add(1, Ordering::Relaxed))
			.wrapping_add((std::process::id() as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15));
		let path = dir.join(format!(".{base}.tmp-{tag:016x}"));

		let mut options = std::fs::OpenOptions::new();
		options.write(true).create_new(true);

		#[cfg(unix)]
		{
			use std::os::unix::fs::OpenOptionsExt;
			options.mode(mode);
		}

		match options.open(&path) {
			Ok(file) => return Ok((path, file)),
			Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
			Err(err) => return Err(err),
		}
	}

	Err(std::io::Error::new(
		std::io::ErrorKind::AlreadyExists,
		"could not create temporary file after retries",
	))
}

fn write_atomic_sync(
	real_target: &Path,
	bytes: &[u8],
	token: &CancellationToken,
	commit_state: &Arc<Mutex<CommitState>>,
	existing_permissions: Option<std::fs::Permissions>,
) -> std::io::Result<()> {
	if token.is_cancelled() {
		return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled"));
	}

	let (temp_path, mut temp_file) = create_exclusive_temp(real_target, &existing_permissions)?;
	let mut guard = TempFileGuard::new(temp_path.clone());

	for chunk in bytes.chunks(64 * 1024) {
		if token.is_cancelled() {
			return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled"));
		}
		temp_file.write_all(chunk)?;
	}
	temp_file.flush()?;

	if token.is_cancelled() {
		return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled"));
	}

	if let Some(perms) = existing_permissions {
		std::fs::set_permissions(&temp_path, perms)?;
	}

	{
		let mut lock = commit_state.lock().unwrap_or_else(|p| p.into_inner());
		if lock.cancelled || token.is_cancelled() {
			return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled"));
		}

		std::fs::rename(&temp_path, real_target)?;
		guard.disarm();
		lock.committed = true;
	}
	Ok(())
}

async fn write_atomic(target: &Path, bytes: &[u8], token: &CancellationToken) -> std::io::Result<()> {
	if token.is_cancelled() {
		return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled"));
	}

	let real_target = resolve_symlink_target(target.to_path_buf()).await?;

	if let Ok(meta) = tokio::fs::metadata(&real_target).await {
		if meta.is_dir() {
			return Err(std::io::Error::new(
				std::io::ErrorKind::IsADirectory,
				format!("{} is a directory", real_target.display()),
			));
		}
	}

	if let Some(parent) = real_target.parent() {
		tokio::fs::create_dir_all(parent).await?;
	}

	let existing_permissions = tokio::fs::metadata(&real_target).await.ok().map(|m| m.permissions());

	let write_token = token.child_token();
	let commit_state = Arc::new(Mutex::new(CommitState::default()));
	let mut abort_guard = AbortGuard::new(write_token.clone(), commit_state.clone());
	let bytes_vec = bytes.to_vec();
	let target_clone = real_target.clone();

	tokio::task::spawn_blocking(move || {
		write_atomic_sync(
			&target_clone,
			&bytes_vec,
			&write_token,
			&commit_state,
			existing_permissions,
		)
	})
	.await
	.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))??;

	abort_guard.disarm();
	Ok(())
}

/// Builtin file writing tool.
pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
	fn name(&self) -> &str {
		"write"
	}

	fn description(&self) -> &str {
		"Write content to a file, creating parent directories if necessary"
	}

	fn schema(&self) -> Value {
		serde_json::json!({
			"type": "object",
			"properties": {
				"path": {
					"type": "string",
					"description": "Path to file to write"
				},
				"content": {
					"type": "string",
					"description": "Content to write to file"
				}
			},
			"required": ["path", "content"]
		})
	}

	fn is_exclusive(&self) -> bool {
		true
	}

	async fn execute(&self, args: &Value, ctx: &AgentContext) -> Result<ToolResult, ToolError> {
		let path = args
			.get("path")
			.and_then(Value::as_str)
			.ok_or_else(|| ToolError::InvalidArguments("missing 'path' parameter".to_string()))?;
		let content = args
			.get("content")
			.and_then(Value::as_str)
			.ok_or_else(|| ToolError::InvalidArguments("missing 'content' parameter".to_string()))?;
		if content.len() > MAX_WRITE_BYTES {
			return Err(ToolError::InvalidArguments(format!(
				"content is {} bytes, the limit is {MAX_WRITE_BYTES}",
				content.len()
			)));
		}

		let target = ctx.cwd.join(path);
		if ctx.cancellation_token.is_cancelled() {
			return Ok(tool_result(format!("Write to {} cancelled", target.display()), true));
		}
		if let Some(parent) = target.parent() {
			tokio::fs::create_dir_all(parent)
				.await
				.map_err(|err| ToolError::ExecutionFailed(format!("{}: {err}", parent.display())))?;
		}
		match write_atomic(&target, content.as_bytes(), &ctx.cancellation_token).await {
			Ok(()) => Ok(tool_result(
				format!("Successfully wrote {} bytes to {}", content.len(), target.display()),
				false,
			)),
			Err(err)
				if err.kind() == std::io::ErrorKind::Interrupted
					|| ctx.cancellation_token.is_cancelled() =>
			{
				Ok(tool_result(format!("Write to {} cancelled", target.display()), true))
			},
			Err(err) => Ok(tool_result(format!("Failed to write {}: {err}", target.display()), true)),
		}
	}
}

/// Builtin exact-match search/replace tool.
pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
	fn name(&self) -> &str {
		"edit"
	}

	fn description(&self) -> &str {
		"Replace exact text in a file; the match must be unique unless replace_all is set"
	}

	fn schema(&self) -> Value {
		serde_json::json!({
			"type": "object",
			"properties": {
				"path": {
					"type": "string",
					"description": "Path to file to edit"
				},
				"old_string": {
					"type": "string",
					"description": "Exact text to replace"
				},
				"new_string": {
					"type": "string",
					"description": "Replacement text (empty string deletes the match)"
				},
				"replace_all": {
					"type": "boolean",
					"description": "Replace every occurrence instead of requiring a unique match"
				}
			},
			"required": ["path", "old_string", "new_string"]
		})
	}

	fn is_exclusive(&self) -> bool {
		true
	}

	async fn execute(&self, args: &Value, ctx: &AgentContext) -> Result<ToolResult, ToolError> {
		let path = args
			.get("path")
			.and_then(Value::as_str)
			.ok_or_else(|| ToolError::InvalidArguments("missing 'path' parameter".to_string()))?;
		let search = args
			.get("old_string")
			.and_then(Value::as_str)
			.ok_or_else(|| {
				ToolError::InvalidArguments("missing 'old_string' parameter".to_string())
			})?;
		if search.is_empty() {
			return Err(ToolError::InvalidArguments("'old_string' must not be empty".to_string()));
		}
		let replacement = args
			.get("new_string")
			.and_then(Value::as_str)
			.ok_or_else(|| {
				ToolError::InvalidArguments("missing 'new_string' parameter".to_string())
			})?;
		let replace_all = args
			.get("replace_all")
			.and_then(Value::as_bool)
			.unwrap_or(false);

		let target = ctx.cwd.join(path);
		let meta = match tokio::fs::metadata(&target).await {
			Ok(meta) => meta,
			Err(err) => {
				return Ok(tool_result(format!("Failed to edit {}: {err}", target.display()), true));
			},
		};
		if !meta.is_file() {
			return Ok(tool_result(format!("{} is not a regular file", target.display()), true));
		}
		// The size decision is taken on the read result, never on an earlier
		// `metadata` call: the file can grow in between, and rewriting a capped
		// buffer would silently drop the tail. One limit, one code path.
		let (bytes, size) = read_capped(&target, MAX_EDIT_BYTES)
			.await
			.map_err(|err| ToolError::ExecutionFailed(format!("{}: {err}", target.display())))?;
		if size > bytes.len() as u64 {
			return Ok(tool_result(
				format!("{} is {size} bytes, the edit limit is {MAX_EDIT_BYTES}", target.display()),
				true,
			));
		}
		let content = String::from_utf8(bytes).map_err(|_| {
			ToolError::ExecutionFailed(format!("{} is not valid UTF-8", target.display()))
		})?;

		let occurrences = content.matches(search).count();
		if occurrences == 0 {
			return Ok(tool_result(
				format!("Not found in {}: \"{}\"", target.display(), preview(search)),
				true,
			));
		}
		if occurrences > 1 && !replace_all {
			return Ok(tool_result(
				format!(
					"{} occurrences in {}; make the match unique or set replace_all",
					occurrences,
					target.display()
				),
				true,
			));
		}
		if ctx.cancellation_token.is_cancelled() {
			return Ok(tool_result(format!("Edit of {} cancelled", target.display()), true));
		}

		let (updated, replacements) = if replace_all {
			(content.replace(search, replacement), occurrences)
		} else {
			(content.replacen(search, replacement, 1), 1)
		};
		match write_atomic(&target, updated.as_bytes(), &ctx.cancellation_token).await {
			Ok(()) => Ok(tool_result(
				format!("Replaced {replacements} occurrence(s) in {}", target.display()),
				false,
			)),
			Err(err)
				if err.kind() == std::io::ErrorKind::Interrupted
					|| ctx.cancellation_token.is_cancelled() =>
			{
				Ok(tool_result(format!("Edit of {} cancelled", target.display()), true))
			},
			Err(err) => Ok(tool_result(format!("Failed to write {}: {err}", target.display()), true)),
		}
	}
}

/// Short, single-line echo of a search string for error messages.
fn preview(text: &str) -> String {
	text
		.chars()
		.take(MAX_EDIT_PREVIEW_CHARS)
		.collect::<String>()
		.replace('\n', "\\n")
}

/// Builtin glob matching tool powered by pi-walker.
pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
	fn name(&self) -> &str {
		"glob"
	}

	fn description(&self) -> &str {
		"Find files matching a glob pattern"
	}

	fn schema(&self) -> Value {
		serde_json::json!({
			"type": "object",
			"properties": {
				"pattern": {
					"type": "string",
					"description": "Glob pattern (e.g. *.rs, **/*.txt)"
				},
				"path": {
					"type": "string",
					"description": "Subdirectory to scan"
				},
				"limit": {
					"type": "number",
					"description": "Maximum number of entries to return"
				}
			}
		})
	}

	async fn execute(&self, args: &Value, ctx: &AgentContext) -> Result<ToolResult, ToolError> {
		let sub_dir = args.get("path").and_then(Value::as_str).unwrap_or(".");
		let pattern = args.get("pattern").and_then(Value::as_str);
		let limit = args
			.get("limit")
			.and_then(Value::as_u64)
			.unwrap_or(200)
			.clamp(1, MAX_GLOB_ENTRIES as u64) as usize;

		let root = ctx.cwd.join(sub_dir);
		let mut request = pi_walker::WalkRequest::new(root).limit(limit);

		if let Some(pattern) = pattern {
			match pi_walker::CompiledWalkGlob::new([pattern]) {
				Ok(compiled) => request = request.filter(pi_walker::WalkFilter::all().glob(compiled)),
				Err(err) => {
					return Ok(tool_result(format!("Invalid glob pattern '{pattern}': {err}"), true));
				},
			}
		}

		match request.collect() {
			Ok(outcome) => {
				let paths: Vec<String> = outcome
					.entries
					.into_iter()
					.map(|entry| entry.path)
					.collect();
				match serde_json::to_string_pretty(&paths) {
					Ok(body) => Ok(tool_result(body, false)),
					Err(err) => Ok(tool_result(format!("Failed to format glob result: {err}"), true)),
				}
			},
			Err(err) => Ok(tool_result(format!("Glob scan error: {err}"), true)),
		}
	}
}

/// Builtin grep text search tool.
pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
	fn name(&self) -> &str {
		"grep"
	}

	fn description(&self) -> &str {
		"Search for a regex pattern across regular files"
	}

	fn schema(&self) -> Value {
		serde_json::json!({
			"type": "object",
			"properties": {
				"pattern": {
					"type": "string",
					"description": "Regex pattern to search for"
				},
				"path": {
					"type": "string",
					"description": "Subdirectory or file path"
				}
			},
			"required": ["pattern"]
		})
	}

	async fn execute(&self, args: &Value, ctx: &AgentContext) -> Result<ToolResult, ToolError> {
		let pattern = args
			.get("pattern")
			.and_then(Value::as_str)
			.ok_or_else(|| ToolError::InvalidArguments("missing 'pattern' parameter".to_string()))?;
		let regex = match Regex::new(pattern) {
			Ok(regex) => regex,
			Err(err) => {
				return Ok(tool_result(format!("Invalid regex pattern '{pattern}': {err}"), true));
			},
		};

		let sub_dir = args.get("path").and_then(Value::as_str).unwrap_or(".");
		let root = ctx.cwd.join(sub_dir);

		let candidates: Vec<PathBuf> = if root.is_file() {
			vec![root.clone()]
		} else {
			match pi_walker::WalkRequest::new(root.clone())
				.limit(MAX_GREP_FILES)
				.collect()
			{
				Ok(outcome) => outcome
					.entries
					.into_iter()
					.filter(|entry| entry.is_file())
					.filter(|entry| entry.size.unwrap_or(0.0) <= MAX_GREP_FILE_BYTES as f64)
					.map(|entry| entry.absolute_path(&root))
					.collect(),
				Err(err) => return Ok(tool_result(format!("Walk error: {err}"), true)),
			}
		};

		let mut sink = BoundedText::new(MAX_GREP_OUTPUT_BYTES);
		let mut matches = 0usize;
		let mut skipped = 0usize;
		for path in candidates {
			if ctx.cancellation_token.is_cancelled() || matches >= MAX_GREP_MATCHES {
				break;
			}
			let Ok((bytes, size)) = read_capped(&path, MAX_GREP_FILE_BYTES).await else {
				skipped += 1;
				continue;
			};
			// Re-check the real size: a file can grow after the walk saw it, and a
			// truncated buffer would report misleading matches.
			if size > MAX_GREP_FILE_BYTES as u64 {
				skipped += 1;
				continue;
			}
			let text = String::from_utf8_lossy(&bytes);
			let display = path
				.strip_prefix(&ctx.cwd)
				.unwrap_or(&path)
				.display()
				.to_string();
			for (number, line) in text.lines().enumerate() {
				if matches >= MAX_GREP_MATCHES {
					break;
				}
				if !regex.is_match(line) {
					continue;
				}
				matches += 1;
				let mut line = line.trim();
				if line.chars().count() > MAX_GREP_LINE_CHARS {
					line = &line[..floor_char_boundary(line, MAX_GREP_LINE_CHARS)];
				}
				sink.push(&format!("{display}:{}: {line}\n", number + 1));
			}
		}

		if matches == 0 {
			return Ok(tool_result(skip_note("No matches found", skipped), false));
		}
		if skipped > 0 {
			sink.push(&format!("[{skipped} file(s) skipped: unreadable or above the size limit]"));
		}
		Ok(tool_result(sink.finish(), false))
	}
}

/// Builtin todo management tool.
pub struct TodoTool {
	todos: std::sync::RwLock<Vec<String>>,
}

impl Default for TodoTool {
	fn default() -> Self {
		Self::new()
	}
}

impl TodoTool {
	pub fn new() -> Self {
		Self { todos: std::sync::RwLock::new(Vec::new()) }
	}
}

#[async_trait]
impl Tool for TodoTool {
	fn name(&self) -> &str {
		"todo"
	}

	fn description(&self) -> &str {
		"Manage task todo list: add, list, or clear items"
	}

	fn schema(&self) -> Value {
		serde_json::json!({
			"type": "object",
			"properties": {
				"action": {
					"type": "string",
					"enum": ["add", "list", "clear"],
					"description": "Action to perform"
				},
				"item": {
					"type": "string",
					"description": "Todo item description (for 'add')"
				}
			},
			"required": ["action"]
		})
	}

	fn is_exclusive(&self) -> bool {
		true
	}

	async fn execute(&self, args: &Value, ctx: &AgentContext) -> Result<ToolResult, ToolError> {
		let action = args
			.get("action")
			.and_then(Value::as_str)
			.ok_or_else(|| ToolError::InvalidArguments("missing 'action'".to_string()))?;
		if ctx.cancellation_token.is_cancelled() {
			return Ok(tool_result("Todo update cancelled", true));
		}

		match action {
			"add" => {
				let item = args
					.get("item")
					.and_then(Value::as_str)
					.ok_or_else(|| ToolError::InvalidArguments("missing 'item'".to_string()))?;
				let mut todos = self
					.todos
					.write()
					.map_err(|_| ToolError::ExecutionFailed("todo lock poisoned".to_string()))?;
				todos.push(item.to_string());
				Ok(tool_result(format!("Added todo item: {item}"), false))
			},
			"clear" => {
				let mut todos = self
					.todos
					.write()
					.map_err(|_| ToolError::ExecutionFailed("todo lock poisoned".to_string()))?;
				todos.clear();
				Ok(tool_result("Todo list cleared", false))
			},
			"list" => {
				let todos = self
					.todos
					.read()
					.map_err(|_| ToolError::ExecutionFailed("todo lock poisoned".to_string()))?;
				let content = if todos.is_empty() {
					"Todo list is empty".to_string()
				} else {
					todos
						.iter()
						.enumerate()
						.map(|(index, item)| format!("{}. {item}", index + 1))
						.collect::<Vec<_>>()
						.join("\n")
				};
				Ok(tool_result(content, false))
			},
			other => Err(ToolError::InvalidArguments(format!("unknown action '{other}'"))),
		}
	}
}

/// Tools registered by default.
pub fn builtin_tools() -> Vec<Arc<dyn Tool>> {
	vec![
		Arc::new(BashTool),
		Arc::new(ReadTool),
		Arc::new(WriteTool),
		Arc::new(EditTool),
		Arc::new(GlobTool),
		Arc::new(GrepTool),
		Arc::new(TodoTool::new()),
	]
}
