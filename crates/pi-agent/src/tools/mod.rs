pub mod ask;
pub mod ast_edit;
pub mod ast_grep;
pub mod checkpoint;
pub mod edit_variants;
pub mod hashline;
pub mod hub;
pub mod internal_urls;
pub mod pending;
pub mod process;
pub mod read;
mod read_sqlite;
pub mod registry;
pub mod scheduler;
pub mod task;
pub mod task_catalog;
pub mod task_isolation;
pub mod think;
pub mod todo;
pub mod yield_tool;

pub use ask::AskTool;
pub use ast_edit::{AstEditTool, PreviewDisplay};
pub use ast_grep::AstGrepTool;
pub use checkpoint::{
	CheckpointController, CheckpointState, CheckpointTool, CompletedRewindState, RewindTool,
};
pub use hub::HubTool;
pub use internal_urls::{
	InternalResource, InternalUrl, InternalUrlRouter, ProtocolHandler, ResolveContext, UrlScope,
	WriteContext,
};
pub use pending::{PendingAction, PendingStore, REJECT_DEVICE_PATH, RESOLVE_DEVICE_PATH};
pub use read::{ReadSnapshotSink, ReadTool};
pub use task::TaskTool;
pub use think::ThinkTool;
pub use todo::TodoTool;
pub use yield_tool::YieldTool;

use std::{
	collections::{BTreeMap, HashMap},
	io::Write,
	path::{Path, PathBuf},
	sync::{
		Arc, Mutex,
		atomic::{AtomicU64, Ordering},
	},
};

use async_trait::async_trait;
use regex::RegexBuilder;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use crate::state::{
	AcpClientBridge, AcpClientError, AgentContext, PlanWriteOp, PlanWriteTarget, ProposeHandler,
	ToolResult,
};
use crate::tools::process::{ProcessBackend, ProcessError, ProcessSpec, RestartPolicy};

/// Captured stdout+stderr budget for one bash call.
const MAX_BASH_OUTPUT_BYTES: usize = 64 * 1024;
/// Bash `timeout` bounds in seconds, matching the TypeScript tool.
const DEFAULT_BASH_TIMEOUT_SECS: f64 = 300.0;
const MIN_BASH_TIMEOUT_SECS: f64 = 1.0;
const MAX_BASH_TIMEOUT_SECS: f64 = 3600.0;
/// Bytes accepted by one `write` call.
const MAX_WRITE_BYTES: usize = 1024 * 1024;
/// Bytes accepted by one `edit` call: the whole file is read and rewritten.
pub(crate) const MAX_EDIT_BYTES: usize = 4 * 1024 * 1024;
/// Default and maximum entries returned by `glob`.
const DEFAULT_GLOB_LIMIT: usize = 200;
const MAX_GLOB_LIMIT: usize = 200;
const MAX_GLOB_ENTRIES: usize = MAX_GLOB_LIMIT;
/// Files scanned and bytes read per file by `grep`.
const MAX_GREP_FILES: usize = 2000;
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
	#[error("Operation aborted: {0}")]
	Aborted(String),
	/// Upstream `ToolError.render()` text, surfaced verbatim (no prefix).
	#[error("{0}")]
	Rejected(String),
}

/// Dynamic trait implemented by all agent tools.
#[async_trait]
pub trait Tool: Send + Sync {
	fn name(&self) -> &str;
	/// Model-facing name when it differs from the registry key (upstream
	/// `customWireName`); `None` advertises [`Tool::name`].
	fn wire_name(&self) -> Option<&str> {
		None
	}
	fn description(&self) -> &str;
	fn schema(&self) -> Value;
	fn is_exclusive(&self) -> bool {
		false
	}
	async fn execute(&self, args: &Value, ctx: &AgentContext) -> Result<ToolResult, ToolError>;
	/// Execute with the model's tool-call id.
	///
	/// Host-bridged tools need the real id (`host_tool_call.toolCallId`), but
	/// the scheduler's later result-id rewrite cannot repair a frame that was
	/// already emitted. Defaults to [`Tool::execute`], so every existing
	/// implementer is unchanged.
	async fn execute_with_call_id(
		&self,
		_id: &str,
		args: &Value,
		ctx: &AgentContext,
	) -> Result<ToolResult, ToolError> {
		self.execute(args, ctx).await
	}
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
pub(crate) async fn read_capped(path: &Path, cap: usize) -> std::io::Result<(Vec<u8>, u64)> {
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
	ToolResult {
		tool_call_id: String::new(),
		content: content.into(),
		is_error,
		terminate: false,
		details: None,
	}
}

/// Portable shell variable name: `[A-Za-z_][A-Za-z0-9_]*`.
fn is_valid_env_name(name: &str) -> bool {
	let mut chars = name.chars();
	match chars.next() {
		Some(first) if first.is_ascii_alphabetic() || first == '_' => {},
		_ => return false,
	}
	chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Resolve the per-call `cwd`: empty means the agent cwd, `~` means `$HOME`,
/// absolute paths are taken as given, and relative paths are joined to the
/// agent cwd.
fn resolve_bash_cwd(raw: Option<&str>, ctx: &AgentContext) -> Result<PathBuf, String> {
	let Some(raw) = raw.filter(|raw| !raw.is_empty()) else {
		return Ok(ctx.cwd.clone());
	};
	if raw == "~" || raw.starts_with("~/") {
		let Some(home) = ctx.env.get("HOME").map(PathBuf::from) else {
			return Err("Cannot resolve '~': HOME is not set".to_string());
		};
		return Ok(if raw == "~" { home } else { home.join(&raw[2..]) });
	}
	let path = PathBuf::from(raw);
	Ok(if path.is_absolute() { path } else { ctx.cwd.join(path) })
}

/// Report a `cwd` that cannot be entered, before any command runs.
async fn invalid_bash_cwd(path: &Path) -> Option<ToolResult> {
	match tokio::fs::metadata(path).await {
		Ok(meta) if meta.is_dir() => None,
		Ok(_) => {
			Some(tool_result(format!("Working directory is not a directory: {}", path.display()), true))
		},
		Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
			Some(tool_result(format!("Working directory does not exist: {}", path.display()), true))
		},
		Err(err) => Some(tool_result(
			format!("Cannot access working directory {}: {err}", path.display()),
			true,
		)),
	}
}

/// Overlay a per-call `env` object onto the agent environment.
fn bash_env(
	raw: Option<&Value>,
	base: &HashMap<String, String>,
) -> Result<HashMap<String, String>, ToolError> {
	let Some(value) = raw.filter(|value| !value.is_null()) else {
		return Ok(base.clone());
	};
	let Some(object) = value.as_object() else {
		return Err(ToolError::InvalidArguments("'env' must be an object of string values".to_string()));
	};
	let mut env = base.clone();
	for (name, value) in object {
		if !is_valid_env_name(name) {
			return Err(ToolError::InvalidArguments(format!("Invalid bash env name: {name}")));
		}
		let Some(text) = value.as_str() else {
			return Err(ToolError::InvalidArguments(format!("'env' value for {name} must be a string")));
		};
		env.insert(name.clone(), text.to_string());
	}
	Ok(env)
}

/// Resolve the per-call `timeout` in seconds: default 300, `0` disables the
/// deadline, and nonzero values clamp to `1..=3600` seconds.
fn bash_timeout_ms(raw: Option<&Value>) -> Result<Option<u32>, ToolError> {
	let Some(value) = raw.filter(|value| !value.is_null()) else {
		return Ok(Some((DEFAULT_BASH_TIMEOUT_SECS * 1000.0) as u32));
	};
	let Some(seconds) = value.as_f64() else {
		return Err(ToolError::InvalidArguments("'timeout' must be a number of seconds".to_string()));
	};
	if !seconds.is_finite() {
		return Err(ToolError::InvalidArguments("'timeout' must be a finite number".to_string()));
	}
	if seconds == 0.0 {
		return Ok(None);
	}
	let clamped = seconds.clamp(MIN_BASH_TIMEOUT_SECS, MAX_BASH_TIMEOUT_SECS);
	Ok(Some((clamped * 1000.0).round() as u32))
}

/// Reject arguments whose TypeScript semantics live in a separate subsystem.
/// `timeout_ms` is the removed legacy spelling and is rejected in both run
/// modes; a *foreground* PTY needs the interactive terminal bridge this
/// headless tool does not own (supervised PTYs run via `hub start` or `bash
/// async` with `pty:true`). Background PTYs are validated by
/// `execute_background` before the supervisor starts the process.
fn reject_unsupported_bash_args(args: &Value, run_async: bool) -> Result<(), ToolError> {
	if args.get("timeout_ms").is_some() {
		return Err(ToolError::InvalidArguments(
			"'timeout_ms' was removed; pass 'timeout' in seconds".to_string(),
		));
	}
	if run_async {
		return Ok(());
	}
	match args.get("pty") {
		None | Some(Value::Null) => {},
		Some(value) if value.as_bool() == Some(false) => {},
		Some(_) => {
			return Err(ToolError::InvalidArguments(
				"Foreground 'pty' needs an interactive terminal UI; use hub start with pty:true (or async bash with pty:true) for a PTY-backed process".to_string(),
			));
		},
	}
	Ok(())
}

/// Builtin bash execution tool.
///
/// Foreground execution is unchanged. When a process backend is wired in
/// (`with_processes`), `async:true` starts a real supervised OS process through
/// the same [`ProcessBackend`] that serves `hub` process ops, so `hub ps`,
/// `logs`, `wait`, `send`, and `stop` operate on the exact job id returned here.
pub struct BashTool {
	processes: Option<Arc<dyn ProcessBackend>>,
	owner: Option<String>,
	/// ACP client reverse bridge; `None` keeps every command local.
	acp: Option<Arc<dyn AcpClientBridge>>,
}

impl BashTool {
	#[must_use]
	pub fn new() -> Self {
		Self { processes: None, owner: None, acp: None }
	}

	#[must_use]
	pub fn with_processes(processes: Arc<dyn ProcessBackend>, owner: Option<String>) -> Self {
		Self { processes: Some(processes), owner, acp: None }
	}

	/// Mount an ACP client bridge. Foreground commands then run through
	/// `terminal/*` when the client advertised the capability; asynchronous
	/// jobs keep the native process supervisor because ACP has no job handle.
	#[must_use]
	pub fn with_acp_bridge(mut self, bridge: Option<Arc<dyn AcpClientBridge>>) -> Self {
		self.acp = bridge;
		self
	}

	/// Background path: `$SHELL -c <command>` under the shared manager.
	async fn execute_background(
		&self,
		command: &str,
		args: &Value,
		ctx: &AgentContext,
	) -> Result<ToolResult, ToolError> {
		let Some(processes) = self.processes.as_ref() else {
			return Err(ToolError::InvalidArguments(
				"Background bash ('async') requires the session process supervisor; use hub start for a supervised process".to_string(),
			));
		};
		if !processes.async_enabled() {
			return Err(ToolError::InvalidArguments(
				"Background bash ('async') is disabled by config async.enabled".to_string(),
			));
		}
		let timeout_ms = bash_timeout_ms(args.get("timeout"))?;
		let env = bash_env(args.get("env"), &ctx.env)?;
		let cwd_arg = match args.get("cwd") {
			None | Some(Value::Null) => None,
			Some(Value::String(cwd)) => Some(cwd.as_str()),
			Some(_) => return Err(ToolError::InvalidArguments("'cwd' must be a string".to_string())),
		};
		let cwd = match resolve_bash_cwd(cwd_arg, ctx) {
			Ok(cwd) => cwd,
			Err(message) => return Ok(tool_result(message, true)),
		};
		if let Some(error) = invalid_bash_cwd(&cwd).await {
			return Ok(error);
		}
		let pty = match args.get("pty") {
			None | Some(Value::Null) => false,
			Some(value) => value
				.as_bool()
				.ok_or_else(|| ToolError::InvalidArguments("'pty' must be a boolean".to_string()))?,
		};
		let shell = env
			.get("SHELL")
			.map(String::as_str)
			.filter(|shell| !shell.trim().is_empty())
			.unwrap_or("sh")
			.to_string();
		let name = processes.next_background_name();
		let spec = ProcessSpec {
			name: name.clone(),
			application: shell,
			args: vec!["-c".to_string(), command.to_string()],
			env,
			cwd: cwd.to_string_lossy().into_owned(),
			pty,
			ready: None,
			restart: RestartPolicy::No,
			persist: false,
			detached: false,
		};
		let outcome = processes
			.clone()
			.start(spec, self.owner.clone())
			.await
			.map_err(process_tool_error)?;
		if let Some(timeout_ms) = timeout_ms {
			processes
				.clone()
				.arm_timeout(name.clone(), self.owner.clone(), u64::from(timeout_ms));
		}
		let content = format!(
			"Backgrounded as job {name}.\nFollow with hub logs name=\"{name}\" (follow:true), stop with hub stop name=\"{name}\", or wait for exit with hub wait name=\"{name}\"."
		);
		let details = serde_json::json!({
			"async": {
				"state": outcome.snapshot.state.as_str(),
				"jobId": name,
				"type": "bash",
			},
			"daemon": outcome.snapshot,
		});
		Ok(ToolResult {
			tool_call_id: String::new(),
			content,
			is_error: false,
			terminate: false,
			details: Some(details),
		})
	}

	/// Foreground path through ACP `terminal/*`. `Ok(None)` means the client
	/// did not advertise the capability and the caller may run natively.
	///
	/// Once `terminal/create` returns an id the command may be running on the
	/// client: every later failure (including cancellation) kills and releases
	/// the terminal and is reported. It is never retried locally, because a
	/// native rerun could execute the command twice.
	async fn execute_terminal(
		&self,
		bridge: &Arc<dyn AcpClientBridge>,
		command: &str,
		args: &Value,
		ctx: &AgentContext,
	) -> Result<Option<ToolResult>, ToolError> {
		let env = bash_env(args.get("env"), &ctx.env)?;
		let cwd_arg = match args.get("cwd") {
			None | Some(Value::Null) => None,
			Some(Value::String(cwd)) => Some(cwd.as_str()),
			Some(_) => return Err(ToolError::InvalidArguments("'cwd' must be a string".to_string())),
		};
		let cwd = match resolve_bash_cwd(cwd_arg, ctx) {
			Ok(cwd) => cwd,
			Err(message) => return Ok(Some(tool_result(message, true))),
		};
		if let Some(error) = invalid_bash_cwd(&cwd).await {
			return Ok(Some(error));
		}
		let shell = env
			.get("SHELL")
			.map(String::as_str)
			.filter(|shell| !shell.trim().is_empty())
			.unwrap_or("sh")
			.to_string();
		let env_pairs: Vec<(String, String)> = env.into_iter().collect();
		let cwd_text = cwd.to_string_lossy().to_string();
		let terminal = match bridge
			.terminal_create(
				&shell,
				vec!["-c".to_string(), command.to_string()],
				env_pairs,
				Some(&cwd_text),
				Some(MAX_BASH_OUTPUT_BYTES as u64),
				&ctx.cancellation_token,
			)
			.await
		{
			Ok(id) => id,
			Err(AcpClientError::Unsupported) => return Ok(None),
			Err(AcpClientError::Cancelled) => {
				return Ok(Some(tool_result("bash cancelled", true)));
			},
			Err(error) => {
				return Ok(Some(tool_result(format!("Error running bash command: {error}"), true)));
			},
		};
		let wait = bridge
			.terminal_wait_for_exit(&terminal, &ctx.cancellation_token)
			.await;
		if wait.is_err() || ctx.cancellation_token.is_cancelled() {
			// Cleanup must reach the wire even when the turn token is already
			// cancelled: a cancelled reverse call never writes its frame.
			let cleanup = CancellationToken::new();
			let _ = bridge.terminal_kill(&terminal, &cleanup).await;
			let _ = bridge.terminal_release(&terminal, &cleanup).await;
			return match wait {
				Ok(_) => Ok(Some(tool_result("bash cancelled", true))),
				Err(AcpClientError::Unsupported) => Ok(Some(tool_result(
					"ACP client did not advertise terminal/wait_for_exit",
					true,
				))),
				Err(AcpClientError::Cancelled) => Ok(Some(tool_result("bash cancelled", true))),
				Err(error) => {
					Ok(Some(tool_result(format!("Error running bash command: {error}"), true)))
				},
			};
		}
		let output = bridge.terminal_output(&terminal, &ctx.cancellation_token).await;
		let cleanup = CancellationToken::new();
		let _ = bridge.terminal_release(&terminal, &cleanup).await;
		let captured = match output {
			Ok(value) => value.get("output").and_then(Value::as_str).unwrap_or("").to_string(),
			Err(AcpClientError::Unsupported) => String::new(),
			Err(AcpClientError::Cancelled) => return Ok(Some(tool_result("bash cancelled", true))),
			Err(error) => {
				return Ok(Some(tool_result(format!("Error reading bash output: {error}"), true)));
			},
		};
		let exit = wait.unwrap_or(Value::Null);
		let exit_code = exit.get("exitCode").and_then(Value::as_i64);
		let signal = exit.get("signal").and_then(Value::as_str);
		let status = format!(
			"exit code: {} | cancelled: false | timed out: false",
			exit_code.map_or_else(|| signal.unwrap_or("unknown").to_string(), |code| code.to_string())
		);
		let content = if captured.is_empty() { status } else { format!("{captured}\n[{status}]") };
		// A missing exit code with no signal means the client reported no
		// status; treat that as failure rather than success.
		let is_error = exit_code != Some(0) || (exit_code.is_none() && signal.is_none());
		Ok(Some(tool_result(content, is_error)))
	}
}

impl Default for BashTool {
	fn default() -> Self {
		Self::new()
	}
}

fn process_tool_error(error: ProcessError) -> ToolError {
	match error {
		ProcessError::Invalid(message) => ToolError::InvalidArguments(message),
		ProcessError::Failed(message) => ToolError::ExecutionFailed(message),
	}
}

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
				"env": {
					"type": "object",
					"additionalProperties": { "type": "string" },
					"description": "Extra environment variables for this call"
				},
				"cwd": {
					"type": "string",
					"description": "Working directory: absolute, ~, or relative to the agent cwd"
				},
				"timeout": {
					"type": "number",
					"description": "Timeout in seconds; 0 disables the command deadline; nonzero values are clamped to 1-3600"
				},
				"async": {
					"type": "boolean",
					"description": "Run in the background under the session process supervisor; returns a job name for hub ps/logs/wait/stop"
				},
				"pty": {
					"type": "boolean",
					"description": "Allocate a PTY for the process (async only; requires the session process supervisor)"
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

		let run_async = match args.get("async") {
			None | Some(Value::Null) => false,
			Some(value) => value
				.as_bool()
				.ok_or_else(|| ToolError::InvalidArguments("'async' must be a boolean".to_string()))?,
		};
		reject_unsupported_bash_args(args, run_async)?;
		if run_async {
			return self.execute_background(command, args, ctx).await;
		}

		// Foreground commands belong to the client when it advertised
		// `terminal`; asynchronous jobs keep the native supervisor (ACP has no
		// job handle or follow-up stream).
		if let Some(bridge) = &self.acp
			&& bridge.supports_terminal()
		{
			if let Some(result) = self.execute_terminal(bridge, command, args, ctx).await? {
				return Ok(result);
			}
		}

		let timeout_ms = bash_timeout_ms(args.get("timeout"))?;
		let env = bash_env(args.get("env"), &ctx.env)?;
		let cwd_arg = match args.get("cwd") {
			None | Some(Value::Null) => None,
			Some(Value::String(cwd)) => Some(cwd.as_str()),
			Some(_) => return Err(ToolError::InvalidArguments("'cwd' must be a string".to_string())),
		};
		let cwd = match resolve_bash_cwd(cwd_arg, ctx) {
			Ok(cwd) => cwd,
			Err(message) => return Ok(tool_result(message, true)),
		};
		if let Some(error) = invalid_bash_cwd(&cwd).await {
			return Ok(error);
		}

		let options = pi_shell::ShellExecuteOptions {
			command: command.to_string(),
			cwd: Some(cwd.to_string_lossy().to_string()),
			env: Some(env),
			session_env: None,
			timeout_ms,
			snapshot_path: None,
			minimizer: None,
		};

		// Bridge the run's cancellation into the shell's own abort flag. A
		// `None` deadline (timeout 0) still honours that cancellation.
		let mut cancel_token = pi_shell::cancel::CancelToken::new(timeout_ms);
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

fn mode_permissions(mode: Option<u32>) -> Option<std::fs::Permissions> {
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		mode.map(std::fs::Permissions::from_mode)
	}
	#[cfg(not(unix))]
	{
		let _ = mode;
		None
	}
}

fn write_atomic_sync(
	real_target: &Path,
	bytes: &[u8],
	token: &CancellationToken,
	commit_state: &Arc<Mutex<CommitState>>,
	permissions: Option<std::fs::Permissions>,
) -> std::io::Result<()> {
	if token.is_cancelled() {
		return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled"));
	}

	let (temp_path, mut temp_file) = create_exclusive_temp(real_target, &permissions)?;
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

	if let Some(perms) = permissions {
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

pub(crate) async fn write_atomic(
	target: &Path,
	bytes: &[u8],
	token: &CancellationToken,
) -> std::io::Result<()> {
	write_atomic_with_mode(target, bytes, token, None).await
}

/// Atomic write with an explicit file mode applied to the unpublished temp file
/// before rename, so a mode-restricted result is never briefly world-readable.
/// `None` keeps the destination's existing mode, or the platform default.
pub(crate) async fn write_atomic_with_mode(
	target: &Path,
	bytes: &[u8],
	token: &CancellationToken,
	mode: Option<u32>,
) -> std::io::Result<()> {
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

	let effective_permissions = match mode_permissions(mode) {
		Some(permissions) => Some(permissions),
		None => tokio::fs::metadata(&real_target).await.ok().map(|m| m.permissions()),
	};

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
			effective_permissions,
		)
	})
	.await
	.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))??;

	abort_guard.disarm();
	Ok(())
}

/// Reject `.`/`..`/empty components of a sandbox-relative path. `openat` and
/// `O_NOFOLLOW` do not block `..`, so every contained helper validates first.
pub(crate) fn validate_artifact_relative(relative: &str) -> Result<(), String> {
	if relative.is_empty() {
		return Err("local:// needs a file name".to_string());
	}
	for part in relative.split('/') {
		if part.is_empty() || part == "." || part == ".." {
			return Err(format!("local://{relative} escapes the artifact root"));
		}
	}
	Ok(())
}

/// Contained UTF-8 read of one artifact, for hosts that resolve a `local:`
/// URL outside the read tool.
pub fn read_local_artifact(root: &Path, relative: &str) -> Result<String, String> {
	validate_artifact_relative(relative)?;
	let (bytes, _) = internal_urls::read_contained(root, relative)?;
	String::from_utf8(bytes).map_err(|_| format!("local://{relative} is not UTF-8 text"))
}

/// Validated relative path of a `local://<path>` target. Traversal, empty
/// components and other schemes yield `None`.
#[must_use]
pub fn local_artifact_relative(path: &str) -> Option<String> {
	let url = internal_urls::InternalUrl::parse(path)?;
	if url.scheme != internal_urls::LOCAL_SCHEME {
		return None;
	}
	let relative = format!("{}{}", url.host, url.path);
	let relative = relative.trim_start_matches('/');
	if relative.is_empty() {
		return None;
	}
	if relative.split('/').any(|part| part.is_empty() || part == "." || part == "..") {
		return None;
	}
	Some(relative.to_string())
}

/// Write `bytes` to `relative` below `root` through descriptors only.
///
/// Every existing component is opened with `O_NOFOLLOW`, missing directories
/// are created with `mkdirat`, and the leaf is published by `renameat` from an
/// unpublished temp file in the same directory, so no component or leaf symlink
/// is ever followed. Cancellation is re-checked under the same commit-state lock
/// `write_atomic` uses, immediately before the rename.
///
/// This does not confine a directory that another same-UID process relocates
/// after it is opened: the walk pins inodes, it is not an isolation boundary.
pub async fn write_artifact(
	root: &Path,
	relative: &str,
	bytes: &[u8],
	token: &CancellationToken,
) -> std::io::Result<()> {
	if token.is_cancelled() {
		return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled"));
	}
	let write_token = token.child_token();
	let commit_state = Arc::new(Mutex::new(CommitState::default()));
	let mut abort_guard = AbortGuard::new(write_token.clone(), commit_state.clone());
	let root = root.to_path_buf();
	let relative = relative.to_string();
	let bytes = bytes.to_vec();
	tokio::task::spawn_blocking(move || {
		write_artifact_sync(&root, &relative, &bytes, &write_token, &commit_state)
	})
	.await
	.map_err(|error| std::io::Error::new(std::io::ErrorKind::Other, error))??;
	abort_guard.disarm();
	Ok(())
}

#[cfg(unix)]
fn write_artifact_sync(
	root: &Path,
	relative: &str,
	bytes: &[u8],
	token: &CancellationToken,
	commit_state: &Arc<Mutex<CommitState>>,
) -> std::io::Result<()> {
	use std::{
		ffi::CString,
		io::Write,
		os::{
			fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
			unix::ffi::OsStrExt,
		},
	};

	fn interrupted() -> std::io::Error {
		std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled")
	}
	fn last_error() -> std::io::Error {
		std::io::Error::last_os_error()
	}
	fn cstring(part: &str) -> std::io::Result<CString> {
		CString::new(part).map_err(|_| {
			std::io::Error::new(
				std::io::ErrorKind::InvalidInput,
				format!("local artifact component contains a NUL: {part:?}"),
			)
		})
	}

	if token.is_cancelled() {
		return Err(interrupted());
	}
	validate_artifact_relative(relative).map_err(|message| {
		std::io::Error::new(std::io::ErrorKind::InvalidInput, message)
	})?;
	let root_c = CString::new(root.as_os_str().as_bytes()).map_err(|_| {
		std::io::Error::new(
			std::io::ErrorKind::InvalidInput,
			"local artifact root contains a NUL",
		)
	})?;
	// SAFETY: `root_c` is NUL-terminated; the returned descriptor is owned here.
	let root_fd = unsafe {
		libc::open(
			root_c.as_ptr(),
			libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
		)
	};
	if root_fd < 0 {
		return Err(std::io::Error::new(
			std::io::ErrorKind::NotFound,
			format!("local artifact root {}: {}", root.display(), last_error()),
		));
	}
	// SAFETY: `root_fd` is a fresh descriptor this call owns.
	let mut dir = unsafe { OwnedFd::from_raw_fd(root_fd) };

	let mut parts: Vec<&str> = relative.split('/').filter(|part| !part.is_empty()).collect();
	let Some(name) = parts.pop() else {
		return Err(std::io::Error::new(
			std::io::ErrorKind::InvalidInput,
			format!("local://{relative} needs a file name"),
		));
	};
	for part in parts {
		if token.is_cancelled() {
			return Err(interrupted());
		}
		let c_part = cstring(part)?;
		// SAFETY: `dir` is open and `c_part` is NUL-terminated.
		let mut fd = unsafe {
			libc::openat(
				dir.as_raw_fd(),
				c_part.as_ptr(),
				libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
			)
		};
		if fd < 0 {
			let missing = last_error();
			if missing.raw_os_error() != Some(libc::ENOENT) {
				return Err(missing);
			}
			// A raced symlink makes `mkdirat` fail with EEXIST and the reopen
			// below reject it; the component is never followed.
			if unsafe { libc::mkdirat(dir.as_raw_fd(), c_part.as_ptr(), 0o755) } != 0 {
				let mkdir = last_error();
				if mkdir.raw_os_error() != Some(libc::EEXIST) {
					return Err(mkdir);
				}
			}
			fd = unsafe {
				libc::openat(
					dir.as_raw_fd(),
					c_part.as_ptr(),
					libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
				)
			};
			if fd < 0 {
				return Err(last_error());
			}
		}
		// SAFETY: fresh descriptor, ownership moves into the guard.
		dir = unsafe { OwnedFd::from_raw_fd(fd) };
	}

	let c_name = cstring(name)?;
	let mut stat: libc::stat = unsafe { std::mem::zeroed() };
	// SAFETY: `dir` and `c_name` are valid for this call.
	let stat_rc = unsafe {
		libc::fstatat(dir.as_raw_fd(), c_name.as_ptr(), &mut stat, libc::AT_SYMLINK_NOFOLLOW)
	};
	let mode: libc::mode_t = if stat_rc == 0 {
		let kind = stat.st_mode & libc::S_IFMT;
		if kind == libc::S_IFLNK {
			return Err(std::io::Error::new(
				std::io::ErrorKind::PermissionDenied,
				format!("local://{relative} is a symlink"),
			));
		}
		if kind == libc::S_IFDIR {
			return Err(std::io::Error::new(
				std::io::ErrorKind::IsADirectory,
				format!("local://{relative} is a directory"),
			));
		}
		stat.st_mode & 0o777
	} else {
		let err = last_error();
		if err.raw_os_error() != Some(libc::ENOENT) {
			return Err(err);
		}
		0o644
	};

	let temp = format!(
		".omp-tmp-{}-{}",
		std::process::id(),
		TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
	);
	let c_temp = cstring(&temp)?;
	// SAFETY: `dir` and `c_temp` are valid; the descriptor is owned below.
	let temp_fd = unsafe {
		libc::openat(
			dir.as_raw_fd(),
			c_temp.as_ptr(),
			libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
			0o600,
		)
	};
	if temp_fd < 0 {
		return Err(last_error());
	}
	// SAFETY: fresh descriptor, ownership moves into the `File`.
	let mut temp_file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(temp_fd) });
	let dir_fd: RawFd = dir.as_raw_fd();
	let cleanup = || {
		// SAFETY: the parent descriptor stays open for this call.
		unsafe { libc::unlinkat(dir_fd, c_temp.as_ptr(), 0) };
	};

	let written = (|| -> std::io::Result<()> {
		for chunk in bytes.chunks(64 * 1024) {
			if token.is_cancelled() {
				return Err(interrupted());
			}
			temp_file.write_all(chunk)?;
		}
		temp_file.flush()?;
		if unsafe { libc::fchmod(temp_file.as_raw_fd(), mode) } != 0 {
			return Err(last_error());
		}
		Ok(())
	})();
	if let Err(error) = written {
		cleanup();
		return Err(error);
	}

	{
		let mut lock = commit_state.lock().unwrap_or_else(|poison| poison.into_inner());
		if lock.cancelled || token.is_cancelled() {
			cleanup();
			return Err(interrupted());
		}
		// SAFETY: both names are NUL-terminated and live in `dir`; `renameat`
		// replaces the leaf name without following it.
		if unsafe { libc::renameat(dir_fd, c_temp.as_ptr(), dir_fd, c_name.as_ptr()) } != 0 {
			let error = last_error();
			cleanup();
			return Err(error);
		}
		lock.committed = true;
	}
	Ok(())
}

#[cfg(not(unix))]
fn write_artifact_sync(
	_root: &Path,
	_relative: &str,
	_bytes: &[u8],
	_token: &CancellationToken,
	_commit_state: &Arc<Mutex<CommitState>>,
) -> std::io::Result<()> {
	Err(std::io::Error::new(
		std::io::ErrorKind::Unsupported,
		"local:// writes need descriptor-relative traversal; unsupported on this platform",
	))
}

/// Builtin file writing tool.
///
/// A write to `xd://resolve` or `xd://reject` is not a filesystem write: it
/// settles the newest staged action from the session's [`pending::PendingStore`]
/// before any path is resolved. The store is injected by the owner of the
/// agent session (the runner for roots, a fresh one per child), never created
/// per registry — registries are rebuilt every prompt.
pub struct WriteTool {
	pending: Option<Arc<pending::PendingStore>>,
	/// Internal URL router consulted before path resolution; absent in
	/// embedders with no protocol handlers.
	router:  Option<Arc<InternalUrlRouter>>,
	/// Per-session scope handed to handlers; never mutated after construction.
	scope:   Arc<UrlScope>,
	/// ACP client reverse bridge; `None` keeps every write local.
	acp:     Option<Arc<dyn AcpClientBridge>>,
	/// Plan-approval handler for `xd://propose`; mounted only in plan mode.
	/// The write guard itself rides on `AgentContext.plan`, never here.
	propose: Option<Arc<dyn ProposeHandler>>,
}

impl Default for WriteTool {
	fn default() -> Self {
		Self {
			pending: None,
			router:  None,
			scope:   Arc::new(UrlScope::default()),
			acp:     None,
			propose: None,
		}
	}
}

impl WriteTool {
	pub fn new() -> Self {
		Self::default()
	}

	/// Mount the session's staging queue on this tool.
	pub fn with_pending(pending: Arc<pending::PendingStore>) -> Self {
		Self { pending: Some(pending), ..Self::default() }
	}

	/// Mount an ACP client bridge. Live writes then go through
	/// `fs/write_text_file` when the client advertised it. A failed remote
	/// write is never repeated locally: the client may already have written.
	#[must_use]
	pub fn with_acp_bridge(mut self, bridge: Option<Arc<dyn AcpClientBridge>>) -> Self {
		self.acp = bridge;
		self
	}

	/// Mount the session's internal URL router with its per-session scope.
	#[must_use]
	pub fn with_internal_urls(
		mut self,
		router: Arc<InternalUrlRouter>,
		scope: Arc<UrlScope>,
	) -> Self {
		self.router = Some(router);
		self.scope = scope;
		self
	}

	/// Mount the `xd://propose` handler. `None` outside plan mode, which keeps
	/// the device failing with the upstream wording.
	#[must_use]
	pub fn with_propose(mut self, propose: Option<Arc<dyn ProposeHandler>>) -> Self {
		self.propose = propose;
		self
	}

	/// Dispatch an `xd://resolve|reject` write. Never touches the filesystem.
	async fn dispatch_pending(
		&self,
		path: &str,
		target: pending::XdTarget<'_>,
		content: &str,
		ctx: &AgentContext,
	) -> Result<ToolResult, ToolError> {
		let device = match target {
			pending::XdTarget::Device(device) => device,
			pending::XdTarget::Invalid => {
				return Err(ToolError::InvalidArguments(format!(
					"Invalid xd:// URL: '{path}'. Use xd://resolve or xd://reject."
				)));
			},
		};
		if pending::resolution_device_usage(device).is_none() {
			return Err(ToolError::InvalidArguments(format!(
				"Unknown xd:// device '{device}'. Mounted devices: xd://resolve, xd://reject."
			)));
		}
		// An interrupt outside `apply` must not consume the staged action.
		if ctx.cancellation_token.is_cancelled() {
			return Ok(tool_result(format!("Write to {path} cancelled"), true));
		}
		let Some(store) = &self.pending else {
			return Err(ToolError::ExecutionFailed("xd:// is not mounted in this session.".to_string()));
		};
		// Deferred mutation boundary: a staged action is never applied while the
		// live guard says plan mode is active. `reject` stays available.
		if device == pending::RESOLVE_DEVICE
			&& let Some(guard) = ctx.plan.as_ref()
		{
			let declared = store.peek().map(|action| action.targets()).unwrap_or_default();
			let owned: Vec<String> = declared;
			let wildcard = [PlanWriteTarget::new("", PlanWriteOp::DeferredApply)];
			let targets: Vec<PlanWriteTarget<'_>> = if owned.is_empty() {
				wildcard.to_vec()
			} else {
				owned
					.iter()
					.map(|path| PlanWriteTarget::new(path, PlanWriteOp::DeferredApply))
					.collect()
			};
			if let Some(message) = guard.write_error("ast_edit", &targets) {
				return Err(ToolError::Rejected(message));
			}
		}
		let reason = content.trim();
		let mut result = if device == pending::RESOLVE_DEVICE {
			store.resolve(ctx, reason).await?
		} else {
			store.reject(ctx, reason).await?
		};
		let inner = result.details.take();
		result.details = Some(serde_json::json!({
			"xdev": {
				"tool": device,
				"mode": "execute",
				"args": { "reason": reason },
				"inner": inner,
			}
		}));
		Ok(result)
	}
}

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
		if pending::is_propose_device_path(path) {
			// Plan approval is a device dispatch, never a filesystem write.
			let Some(handler) = &self.propose else {
				return Err(ToolError::Rejected(format!(
					"No plan is awaiting approval — {0} only accepts a plan title while plan mode is active.",
					pending::PROPOSE_DEVICE_PATH
				)));
			};
			return handler.propose(content, ctx).await;
		}
		if let Some(target) = pending::parse_xd_target(path) {
			return self.dispatch_pending(path, target, content, ctx).await;
		}
		// Plan mode keeps the working tree read-only; the host classifies the
		// resolved target (local:// artifacts stay writable).
		let targets = [PlanWriteTarget::new(path, PlanWriteOp::Update)];
		if let Some(message) = ctx
			.plan
			.as_ref()
			.and_then(|guard| guard.write_error("write", &targets))
		{
			return Err(ToolError::Rejected(message));
		}
		if content.len() > MAX_WRITE_BYTES {
			return Err(ToolError::InvalidArguments(format!(
				"content is {} bytes, the limit is {MAX_WRITE_BYTES}",
				content.len()
			)));
		}

		// Plan mode allows the `local://` artifact sandbox. That scheme's handler
		// is read-only, so an active guard's sandbox root owns the bytes; the sink
		// walks descriptors and never follows a component or leaf symlink.
		if let Some(root) = ctx.plan.as_ref().and_then(|guard| guard.artifact_root())
			&& let Some(relative) = local_artifact_relative(path)
		{
			return match write_artifact(&root, &relative, content.as_bytes(), &ctx.cancellation_token)
				.await
			{
				Ok(()) => Ok(tool_result(
					format!("Successfully wrote {} bytes to {path}", content.len()),
					false,
				)),
				Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {
					Ok(tool_result(format!("Write to {path} cancelled"), true))
				},
				Err(err) => Ok(tool_result(format!("Failed to write {path}: {err}"), true)),
			};
		}

		// Internal URLs never become files: a mounted handler owns the write,
		// and a scheme with no handler is an explicit protocol error.
		if let Some(scheme) = internal_urls::scheme_of(path) {
			let Some(router) = &self.router else {
				return Ok(tool_result(
					internal_urls::unknown_protocol_error(&scheme, &[]),
					true,
				));
			};
			return match router
				.write(
					path,
					content,
					&self.scope.write_context(&ctx.cwd, &ctx.cancellation_token),
				)
				.await
			{
				Ok(()) => Ok(tool_result(
					format!("Successfully wrote {} bytes to {path}", content.len()),
					false,
				)),
				Err(message) => Ok(tool_result(message, true)),
			};
		}

		let target = ctx.cwd.join(path);
		// ACP `fs/write_text_file` owns the write when the client advertised
		// it. Only `Unsupported` falls through to the local filesystem: after
		// a remote attempt (including a remote error or a cancellation) the
		// client may already hold the bytes, and a native retry would write
		// twice.
		if let Some(bridge) = &self.acp
			&& bridge.supports_fs_write()
		{
			if ctx.cancellation_token.is_cancelled() {
				return Ok(tool_result(format!("Write to {} cancelled", target.display()), true));
			}
			match bridge
				.write_text_file(&target.display().to_string(), content, &ctx.cancellation_token)
				.await
			{
				Ok(()) => {
					return Ok(tool_result(
						format!("Successfully wrote {} bytes to {}", content.len(), target.display()),
						false,
					));
				},
				Err(AcpClientError::Unsupported) => {},
				Err(AcpClientError::Cancelled) => {
					return Ok(tool_result(format!("Write to {} cancelled", target.display()), true));
				},
				Err(error) => {
					return Ok(tool_result(
						format!("Failed to write {}: {error}", target.display()),
						true,
					));
				},
			}
		}
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

/// Builtin hashline `edit` tool: one line-anchored patch over one or more
/// `[PATH#TAG]` sections.
///
/// `store` MUST be the same `Arc<SnapshotStore>` handed to the session's read
/// sink: tags and seen-line provenance are per agent, never process-global.
/// Named `CUT` registers persist for this tool instance only.
pub struct EditTool {
	store: Arc<hashline::SnapshotStore>,
	registers: Mutex<BTreeMap<String, Vec<String>>>,
	noop_guard: hashline::edit::NoopGuard,
	options: Mutex<hashline::edit::EditToolOptions>,
	/// Active wire contract. Mutable because a root turn keeps one instance
	/// (CUT registers, no-op guard, store identity) across REPL turns while the
	/// selector may follow the active model; upstream resolves lazily too.
	mode: Mutex<edit_variants::EditMode>,
}

impl EditTool {
	pub fn new(store: Arc<hashline::SnapshotStore>) -> Self {
		Self::with_options(store, hashline::edit::EditToolOptions::default())
	}

	/// Construct with explicit knobs (e.g. `enforce_seen_lines = false`).
	pub fn with_options(store: Arc<hashline::SnapshotStore>, options: hashline::edit::EditToolOptions) -> Self {
		Self::with_mode(store, options, edit_variants::DEFAULT_EDIT_MODE)
	}

	/// Construct with a pinned mode (env-pinned or resolved per session).
	pub fn with_mode(
		store: Arc<hashline::SnapshotStore>,
		options: hashline::edit::EditToolOptions,
		mode: edit_variants::EditMode,
	) -> Self {
		Self {
			store,
			registers: Mutex::new(BTreeMap::new()),
			noop_guard: Mutex::new(BTreeMap::new()),
			options: Mutex::new(options),
			mode: Mutex::new(mode),
		}
	}

	/// Replace the behavioural knobs for later calls without dropping the
	/// registers or the no-op guard this instance carries.
	pub fn set_options(&self, options: hashline::edit::EditToolOptions) {
		*self.options.lock().unwrap_or_else(|poison| poison.into_inner()) = options;
	}

	pub fn mode(&self) -> edit_variants::EditMode {
		*self.mode.lock().unwrap_or_else(|poison| poison.into_inner())
	}

	/// Refresh the wire contract for a later turn without dropping state.
	pub fn set_mode(&self, mode: edit_variants::EditMode) {
		*self.mode.lock().unwrap_or_else(|poison| poison.into_inner()) = mode;
	}
}

#[async_trait]
impl Tool for EditTool {
	fn name(&self) -> &str {
		"edit"
	}

	fn wire_name(&self) -> Option<&str> {
		match self.mode() {
			edit_variants::EditMode::ApplyPatch => Some("apply_patch"),
			_ => None,
		}
	}

	fn description(&self) -> &str {
		edit_variants::contract(self.mode()).map_or(
			"Apply hashline sections to existing files; anchors must carry the 4-hex snapshot tag from the latest read",
			|contract| contract.description,
		)
	}

	fn schema(&self) -> Value {
		edit_variants::contract(self.mode())
			.map(|contract| contract.schema())
			.unwrap_or_else(|| {
				serde_json::json!({
					"type": "object",
					"properties": {
						"input": {
							"type": "string",
							"description": "One or more hashline sections: `[PATH#TAG]` headers followed by PUT/CUT/REM/MV ops. TAG is the 4-hex content-hash tag printed by the latest read/search/edit of that file. Line numbers index that tagged snapshot."
						}
					},
					"required": ["input"]
				})
			})
	}

	fn is_exclusive(&self) -> bool {
		true
	}

	async fn execute(&self, args: &Value, ctx: &AgentContext) -> Result<ToolResult, ToolError> {
		let guard = ctx.plan.clone();
		let artifact_root = guard.as_ref().and_then(|guard| guard.artifact_root());
		let mode = self.mode();
		let options = *self.options.lock().unwrap_or_else(|poison| poison.into_inner());

		if mode == edit_variants::EditMode::Hashline {
			let input = args.get("input").and_then(Value::as_str).unwrap_or_default();
			let mut sections = match hashline::parser::split_patch_sections(
				input,
				Some(ctx.cwd.as_path()),
			) {
				Ok(sections) => sections,
				Err(message) => {
					// An unparsable payload while plan mode is active is an
					// unclassifiable mutation: fail closed through the host.
					if let Some(guard) = &guard
						&& artifact_root.is_some()
						&& let Some(blocked) = guard.write_error("edit", &[])
					{
						return Err(ToolError::Rejected(blocked));
					}
					return Ok(tool_result(format!("Failed to edit: {message}"), true));
				},
			};
			if let Some(guard) = &guard {
				let mut ops: Vec<(String, PlanWriteOp, Option<String>)> = Vec::new();
				for section in &sections {
					let file_op = hashline::parser::parse_section_body(&section.diff)
						.ok()
						.and_then(|parsed| parsed.file_op);
					ops.push(match file_op {
						Some(hashline::parser::FileOp::Rem) => {
							(section.path.clone(), PlanWriteOp::Delete, None)
						},
						Some(hashline::parser::FileOp::Move(dest)) => {
							(section.path.clone(), PlanWriteOp::Rename, Some(dest))
						},
						None => (section.path.clone(), PlanWriteOp::Update, None),
					});
				}
				let targets: Vec<PlanWriteTarget<'_>> = ops
					.iter()
					.map(|(path, op, rename_to)| PlanWriteTarget {
						path:      path.as_str(),
						op:        *op,
						rename_to: rename_to.as_deref(),
					})
					.collect();
				if let Some(message) = guard.write_error("edit", &targets) {
					return Err(ToolError::Rejected(message));
				}
			}
			// Artifact sections keep their authored URL in the tag lookup but
			// execute against the sandbox path the same walk would produce.
			if let Some(root) = artifact_root.as_deref() {
				for section in &mut sections {
					if let Some(relative) = local_artifact_relative(&section.path) {
						section.file_path = root.join(relative);
					}
				}
			}
			return hashline::edit::execute_sections(
				self.store.as_ref(),
				&self.registers,
				&self.noop_guard,
				&options,
				sections,
				artifact_root.as_deref(),
				input,
				ctx,
			)
			.await;
		}

		if let Some(guard) = &guard {
			// Non-hashline variants cannot route artifacts through the fd sink,
			// so an authored local target is refused rather than written by a
			// path helper. Operations and rename destinations are preserved.
			let authored = edit_variant_targets(mode, args);
			let touches_local = authored.iter().any(|(path, _, rename_to)| {
				local_artifact_relative(path).is_some()
					|| rename_to
						.as_deref()
						.is_some_and(|dest| local_artifact_relative(dest).is_some())
			});
			if artifact_root.is_some()
				&& touches_local
				&& let Some(blocked) = guard.write_error("edit", &[])
			{
				return Err(ToolError::Rejected(blocked));
			}
			let targets: Vec<PlanWriteTarget<'_>> = authored
				.iter()
				.map(|(path, op, rename_to)| PlanWriteTarget {
					path:      path.as_str(),
					op:        *op,
					rename_to: rename_to.as_deref(),
				})
				.collect();
			if let Some(message) = guard.write_error("edit", &targets) {
				return Err(ToolError::Rejected(message));
			}
		}
		edit_variants::execute(mode, args, ctx).await
	}
}

/// Parsed targets of a non-hashline edit variant, used only for plan-mode
/// classification (the variant itself resolves them). Operations and rename
/// destinations are preserved so a delete/move cannot hide behind `Update`.
fn edit_variant_targets(
	mode: edit_variants::EditMode,
	args: &Value,
) -> Vec<(String, PlanWriteOp, Option<String>)> {
	let mut targets = Vec::new();
	if mode == edit_variants::EditMode::ApplyPatch {
		if let Some(input) = args.get("input").and_then(Value::as_str)
			&& let Ok(hunks) = edit_variants::apply_patch::parse_apply_patch(input)
		{
			for hunk in hunks {
				let op = match hunk.op {
					edit_variants::patch::Operation::Create => PlanWriteOp::Create,
					edit_variants::patch::Operation::Delete => PlanWriteOp::Delete,
					edit_variants::patch::Operation::Update => match &hunk.rename {
						Some(_) => PlanWriteOp::Rename,
						None => PlanWriteOp::Update,
					},
				};
				targets.push((hunk.path, op, hunk.rename));
			}
			return targets;
		}
	}
	let top_level = args.get("path").and_then(Value::as_str);
	if let Some(edits) = args.get("edits").and_then(Value::as_array) {
		// `patch`: every entry applies to the top-level `path`; an entry carries
		// only `op`/`rename`/`diff`.
		for edit in edits {
			let path = top_level.unwrap_or_default();
			let rename_to = edit
				.get("rename")
				.and_then(Value::as_str)
				.map(str::to_string);
			let op = match edit.get("op").and_then(Value::as_str) {
				Some("delete") => PlanWriteOp::Delete,
				Some("create") => PlanWriteOp::Create,
				_ if rename_to.is_some() => PlanWriteOp::Rename,
				_ => PlanWriteOp::Update,
			};
			targets.push((path.to_string(), op, rename_to));
		}
		return targets;
	}
	if let Some(path) = top_level {
		targets.push((path.to_string(), PlanWriteOp::Update, None));
	}
	if mode == edit_variants::EditMode::Sloppy
		&& let Some(input) = args.get("input").and_then(Value::as_str)
	{
		// Reuse the variant's own section extractor; no second parser.
		for section in edit_variants::sloppy::split_sections(input) {
			targets.push((section.path, PlanWriteOp::Update, None));
		}
	}
	targets
}

fn normalize_path_like_input(input: &str) -> String {
	let trimmed = input.trim();
	let stripped = if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() > 1 {
		&trimmed[1..trimmed.len() - 1]
	} else {
		trimmed
	};
	stripped.trim().replace('\\', "/")
}

fn split_top_level_delimited(entry: &str) -> Vec<String> {
	let mut parts = Vec::new();
	let mut brace_depth = 0;
	let mut start = 0;
	let chars: Vec<char> = entry.chars().collect();
	let mut i = 0;
	while i < chars.len() {
		let ch = chars[i];
		if ch == '\\' && i + 1 < chars.len() {
			i += 2;
			continue;
		}
		if ch == '{' {
			brace_depth += 1;
		} else if ch == '}' {
			if brace_depth > 0 {
				brace_depth -= 1;
			}
		} else if brace_depth == 0 && ch == ';' {
			let part: String = chars[start..i].iter().collect();
			let trimmed = normalize_path_like_input(&part);
			if !trimmed.is_empty() {
				parts.push(trimmed);
			}
			start = i + 1;
		}
		i += 1;
	}
	let last: String = chars[start..].iter().collect();
	let trimmed = normalize_path_like_input(&last);
	if !trimmed.is_empty() {
		parts.push(trimmed);
	}
	if parts.is_empty() {
		vec![entry.to_string()]
	} else {
		parts
	}
}

fn has_glob_path_chars(s: &str) -> bool {
	s.contains('*') || s.contains('?') || s.contains('[') || s.contains('{')
}

fn fix_unclosed_braces(mut pattern: String) -> String {
	let mut depth = 0;
	for ch in pattern.chars() {
		if ch == '{' {
			depth += 1;
		} else if ch == '}' && depth > 0 {
			depth -= 1;
		}
	}
	for _ in 0..depth {
		pattern.push('}');
	}
	pattern
}

fn walk_depth_bound(pattern: &str) -> usize {
	if pattern.contains("**") || pattern.contains('{') {
		return usize::MAX;
	}
	pattern
		.split('/')
		.filter(|seg| !seg.is_empty())
		.count()
		.max(1)
}

struct ParsedFindPattern {
	base_path: String,
	glob_pattern: String,
	has_glob: bool,
}

fn parse_find_pattern(pattern: &str) -> ParsedFindPattern {
	let normalized = pattern.replace('\\', "/");
	let segments: Vec<&str> = normalized.split('/').collect();
	let mut first_glob_index = None;
	for (i, segment) in segments.iter().enumerate() {
		if has_glob_path_chars(segment) {
			first_glob_index = Some(i);
			break;
		}
	}

	match first_glob_index {
		None => ParsedFindPattern {
			base_path: normalized,
			glob_pattern: "**/*".to_string(),
			has_glob: false,
		},
		Some(0) => {
			let needs_recursive = !normalized.starts_with("**/");
			let glob_pattern = if needs_recursive {
				format!("**/{normalized}")
			} else {
				normalized
			};
			ParsedFindPattern {
				base_path: ".".to_string(),
				glob_pattern,
				has_glob: true,
			}
		},
		Some(idx) => {
			let raw_base = segments[..idx].join("/");
			let base_path = if raw_base.is_empty() {
				if normalized.starts_with('/') {
					"/".to_string()
				} else {
					".".to_string()
				}
			} else {
				raw_base
			};
			let glob_pattern = segments[idx..].join("/");
			ParsedFindPattern {
				base_path,
				glob_pattern,
				has_glob: true,
			}
		},
	}
}

fn resolve_to_cwd(base_path: &str, cwd: &Path) -> PathBuf {
	if base_path.is_empty() || base_path == "." {
		cwd.to_path_buf()
	} else {
		let p = Path::new(base_path);
		if p.is_absolute() {
			p.to_path_buf()
		} else {
			cwd.join(p)
		}
	}
}
fn normalize_path(path: &Path) -> PathBuf {
	if let Ok(canon) = path.canonicalize() {
		return canon;
	}
	let mut normalized = PathBuf::new();
	for comp in path.components() {
		match comp {
			std::path::Component::CurDir => {},
			std::path::Component::ParentDir => {
				normalized.pop();
			},
			c => normalized.push(c.as_os_str()),
		}
	}
	normalized
}

fn is_root_path(path: &Path) -> bool {
	if path == Path::new("/") {
		return true;
	}
	let norm = normalize_path(path);
	norm == Path::new("/") || (norm.parent().is_none() && norm.is_absolute())
}


fn format_path_relative_to_cwd(path: &Path, cwd: &Path, is_dir: bool) -> String {
	let mut rel = match path.strip_prefix(cwd) {
		Ok(p) => p.to_string_lossy().replace('\\', "/"),
		Err(_) => path.to_string_lossy().replace('\\', "/"),
	};
	if rel.is_empty() {
		rel = ".".to_string();
	}
	if is_dir && !rel.ends_with('/') {
		rel.push('/');
	}
	rel
}

fn extract_raw_paths(args: &Value) -> Vec<String> {

	if let Some(arr) = args.get("path").and_then(Value::as_array) {
		let list: Vec<String> = arr
			.iter()
			.filter_map(Value::as_str)
			.map(|s| s.to_string())
			.collect();
		if !list.is_empty() {
			return list;
		}
	}

	if let Some(path_str) = args.get("path").and_then(Value::as_str) {
		let trimmed = path_str.trim();
		if trimmed.starts_with('[') && trimmed.ends_with(']') {
			if let Ok(parsed) = serde_json::from_str::<Vec<String>>(trimmed) {
				if !parsed.is_empty() {
					return parsed;
				}
			}
		}
		return vec![path_str.to_string()];
	}


	vec![".".to_string()]
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
				"path": {
					"type": "string",
					"description": "glob, file, or directory to search — a single path or a semicolon-delimited list (\"src/**/*.ts; test/**/*.ts\"). Omitted -> searches the workspace root (\".\")"
				},
				"hidden": {
					"type": "boolean",
					"description": "include hidden files"
				},
				"gitignore": {
					"type": "boolean",
					"description": "respect gitignore"
				},
				"limit": {
					"type": "number",
					"description": "max results"
				}
			}
		})
	}

	async fn execute(&self, args: &Value, ctx: &AgentContext) -> Result<ToolResult, ToolError> {
		if ctx.cancellation_token.is_cancelled() {
			return Ok(tool_result("Glob scan cancelled", true));
		}

		if let Some(limit_val) = args.get("limit") {
			if limit_val.is_null() {
				// null defaults to DEFAULT_GLOB_LIMIT
			} else {
				let is_valid_positive = if let Some(v) = limit_val.as_i64() {
					v > 0
				} else if let Some(v) = limit_val.as_f64() {
					v > 0.0 && v.is_finite()
				} else {
					false
				};
				if !is_valid_positive {
					return Ok(tool_result("Limit must be a positive number", true));
				}
			}
		}

		let limit = args
			.get("limit")
			.and_then(Value::as_u64)
			.unwrap_or(DEFAULT_GLOB_LIMIT as u64)
			.clamp(1, MAX_GLOB_LIMIT as u64) as usize;

		let legacy_pattern = args
			.get("pattern")
			.and_then(Value::as_str)
			.filter(|s| !s.is_empty());

		let parsed_targets: Vec<(String, ParsedFindPattern, PathBuf)> = if let Some(pat) = legacy_pattern {
			let search_base = args.get("path").and_then(Value::as_str).unwrap_or(".");
			let base = if search_base.is_empty() { "." } else { search_base };
			let search_path = resolve_to_cwd(base, &ctx.cwd);
			let parsed = ParsedFindPattern {
				base_path: base.to_string(),
				glob_pattern: pat.to_string(),
				has_glob: true,
			};
			vec![(base.to_string(), parsed, search_path)]
		} else {
			let raw_paths = extract_raw_paths(args);
			let mut target_inputs: Vec<(String, bool)> = Vec::new();
			for raw in raw_paths {
				let norm = normalize_path_like_input(&raw);
				if norm.is_empty() {
					target_inputs.push((String::new(), false));
					continue;
				}
				let literal_path = resolve_to_cwd(&norm, &ctx.cwd);
				if literal_path.exists() {
					target_inputs.push((norm, true));
				} else {
					for item in split_top_level_delimited(&norm) {
						let item_exists = resolve_to_cwd(&item, &ctx.cwd).exists();
						target_inputs.push((item, item_exists));
					}
				}
			}

			if target_inputs.is_empty() || target_inputs.iter().any(|(p, _)| p.is_empty()) {
				return Ok(tool_result("`path` must contain non-empty globs or paths", true));
			}

			if target_inputs.iter().any(|(p, _)| p.chars().all(|c| c == '/')) {
				return Ok(tool_result("Searching from root directory '/' is not allowed", true));
			}

			target_inputs
				.into_iter()
				.map(|(pat, is_literal)| {
					if is_literal {
						let search_path = resolve_to_cwd(&pat, &ctx.cwd);
						let parsed = ParsedFindPattern {
							base_path: pat.clone(),
							glob_pattern: "**/*".to_string(),
							has_glob: false,
						};
						(pat, parsed, search_path)
					} else {
						let parsed = parse_find_pattern(&pat);
						let search_path = resolve_to_cwd(&parsed.base_path, &ctx.cwd);
						(pat, parsed, search_path)
					}
				})
				.collect()
		};

		for (_, _, search_path) in &parsed_targets {
			if is_root_path(search_path) {
				return Ok(tool_result("Searching from root directory '/' is not allowed", true));
			}
		}

		let is_single = parsed_targets.len() == 1;
		let mut valid_targets = Vec::new();
		let mut missing_paths = Vec::new();

		if is_single {
			let (pat, parsed, search_path) = parsed_targets.into_iter().next().unwrap();
			if !search_path.exists() {
				return Ok(tool_result(format!("Path not found: {pat}"), true));
			}
			valid_targets.push((pat, parsed, search_path));
		} else {
			for (pat, parsed, search_path) in parsed_targets {
				if search_path.exists() {
					valid_targets.push((pat, parsed, search_path));
				} else {
					missing_paths.push(pat);
				}
			}
			if valid_targets.is_empty() {
				return Ok(tool_result(format!("Path not found: {}", missing_paths.join(", ")), true));
			}
		}

		let include_hidden = args.get("hidden").and_then(Value::as_bool).unwrap_or(true);
		let use_gitignore = args.get("gitignore").and_then(Value::as_bool).unwrap_or(true);

		let mut all_collected: Vec<(String, Option<f64>)> = Vec::new();

		for (pat, parsed, search_path) in valid_targets {
			if ctx.cancellation_token.is_cancelled() {
				return Ok(tool_result("Glob scan cancelled", true));
			}

			if search_path.is_file() {
				if !parsed.has_glob {
					let rel = format_path_relative_to_cwd(&search_path, &ctx.cwd, false);
					let mtime = search_path
						.metadata()
						.ok()
						.and_then(|m| m.modified().ok())
						.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
						.map(|d| d.as_secs_f64() * 1000.0);
					all_collected.push((rel, mtime));
					continue;
				} else if is_single {
					return Ok(tool_result(format!("Path is not a directory: {}", search_path.display()), true));
				} else {
					continue;
				}
			}

			if !search_path.is_dir() {
				if is_single {
					return Ok(tool_result(format!("Path is not a directory: {}", search_path.display()), true));
				} else {
					continue;
				}
			}

			let fixed_pattern = fix_unclosed_braces(parsed.glob_pattern.clone());
			let walk_glob = match pi_walker::CompiledWalkGlob::new([&fixed_pattern]) {
				Ok(compiled) => compiled,
				Err(err) => {
					return Ok(tool_result(format!("Invalid glob pattern '{pat}': {err}"), true));
				},
			};

			let mentions_node_modules = parsed.glob_pattern.contains("node_modules");
			let depth_bound = walk_depth_bound(&parsed.glob_pattern);
			let request = pi_walker::WalkRequest::new(&search_path)
				.hidden(include_hidden)
				.gitignore(use_gitignore)
				.skip_git(true)
				.skip_node_modules(!mentions_node_modules)
				.follow_links(pi_walker::FollowLinks::Never)
				.detail(pi_walker::WalkDetail::Full)
				.order(pi_walker::WalkOrder::Path)
				.emit_root(false)
				.depth(1, depth_bound)
				.directory_errors(pi_walker::DirectoryErrorMode::SkipSkippable)
				.empty_recheck(pi_walker::EmptyRecheck::Configured)
				.filter(
					pi_walker::WalkFilter::all()
						.glob(walk_glob)
						.node_modules_unless_mentioned(mentions_node_modules),
				);

			let heartbeat = || {
				if ctx.cancellation_token.is_cancelled() {
					Err("Glob scan cancelled".to_string())
				} else {
					Ok(())
				}
			};

			let outcome = match request.collect_ranked_with_heartbeat(
				pi_walker::WalkRank::MtimeDescPathAsc,
				limit,
				heartbeat,
			) {
				Ok(outcome) => outcome,
				Err(err) => {
					if ctx.cancellation_token.is_cancelled()
						|| matches!(&err, pi_walker::WalkError::Interrupted(_))
					{
						return Ok(tool_result("Glob scan cancelled", true));
					}
					return Ok(tool_result(format!("Glob scan error: {err}"), true));
				},
			};

			for entry in outcome.entries {
				let abs_path = entry.absolute_path(&search_path);
				let is_dir = entry.file_type == pi_walker::FileType::Dir;
				let rel_path = format_path_relative_to_cwd(&abs_path, &ctx.cwd, is_dir);
				let mtime = entry.mtime;
				all_collected.push((rel_path, mtime));
			}
		}

		let mut seen = std::collections::HashSet::new();
		let mut deduped = Vec::new();
		for (path, mtime) in all_collected {
			if seen.insert(path.clone()) {
				deduped.push((path, mtime));
			}
		}

		deduped.sort_by(|a, b| {
			let mtime_order = match (a.1, b.1) {
				(Some(left), Some(right)) => right.total_cmp(&left),
				(Some(_), None) => std::cmp::Ordering::Less,
				(None, Some(_)) => std::cmp::Ordering::Greater,
				(None, None) => std::cmp::Ordering::Equal,
			};
			mtime_order.then_with(|| a.0.cmp(&b.0))
		});
		deduped.truncate(limit);
		let final_paths: Vec<String> = deduped.into_iter().map(|(p, _)| p).collect();

		let missing_note = if !missing_paths.is_empty() {
			Some(format!("Skipped missing paths: {}", missing_paths.join(", ")))
		} else {
			None
		};

		if final_paths.is_empty() {
			let mut msg = "No files found matching pattern".to_string();
			if let Some(note) = missing_note {
				msg.push('\n');
				msg.push_str(&note);
			}
			return Ok(tool_result(msg, false));
		}

		let base_output = final_paths.join("\n");
		let output = if let Some(note) = missing_note {
			format!("{base_output}\n\n{note}")
		} else {
			base_output
		};
		Ok(tool_result(output, false))
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
					"description": "glob, file, or directory to search — a single path or a semicolon-delimited list (\"src/**/*.ts; test/**/*.ts\"). Omitted -> searches the workspace root (\".\")"
				},
				"case": {
					"type": "boolean",
					"description": "case-sensitive search"
				},
				"gitignore": {
					"type": "boolean",
					"description": "respect gitignore"
				},
				"skip": {
					"type": "number",
					"description": "files to skip before collecting results — use to paginate when the prior call hit the file limit"
				}
			},
			"required": ["pattern"]
		})
	}

	async fn execute(&self, args: &Value, ctx: &AgentContext) -> Result<ToolResult, ToolError> {
		if ctx.cancellation_token.is_cancelled() {
			return Ok(tool_result("Grep search cancelled", true));
		}

		let pattern = args
			.get("pattern")
			.and_then(Value::as_str)
			.ok_or_else(|| ToolError::InvalidArguments("missing 'pattern' parameter".to_string()))?;
		if pattern.trim().is_empty() {
			return Ok(tool_result("Pattern must not be empty", true));
		}

		let case_sensitive = args.get("case").and_then(Value::as_bool).unwrap_or(true);
		let ignore_case = !case_sensitive;

		let regex = match RegexBuilder::new(pattern).case_insensitive(ignore_case).build() {
			Ok(regex) => regex,
			Err(err) => {
				return Ok(tool_result(format!("Invalid regex pattern '{pattern}': {err}"), true));
			},
		};

		let skip = if let Some(skip_val) = args.get("skip") {
			if skip_val.is_null() {
				0
			} else if let Some(n) = skip_val.as_i64() {
				if n < 0 {
					return Ok(tool_result("Skip must be a non-negative number", true));
				}
				n as usize
			} else if let Some(f) = skip_val.as_f64() {
				if f < 0.0 || !f.is_finite() {
					return Ok(tool_result("Skip must be a non-negative number", true));
				}
				f as usize
			} else {
				return Ok(tool_result("Skip must be a non-negative number", true));
			}
		} else {
			0
		};

		let use_gitignore = args.get("gitignore").and_then(Value::as_bool).unwrap_or(true);

		let raw_paths = extract_raw_paths(args);
		let mut target_inputs: Vec<(String, bool)> = Vec::new();
		for raw in raw_paths {
			let norm = normalize_path_like_input(&raw);
			if norm.is_empty() {
				target_inputs.push((String::new(), false));
				continue;
			}
			let literal_path = resolve_to_cwd(&norm, &ctx.cwd);
			if literal_path.exists() {
				target_inputs.push((norm, true));
			} else {
				for item in split_top_level_delimited(&norm) {
					let item_exists = resolve_to_cwd(&item, &ctx.cwd).exists();
					target_inputs.push((item, item_exists));
				}
			}
		}

		if target_inputs.is_empty() || target_inputs.iter().any(|(p, _)| p.is_empty()) {
			return Ok(tool_result("`path` must contain non-empty globs or paths", true));
		}

		if target_inputs.iter().any(|(p, _)| !p.is_empty() && p.chars().all(|c| c == '/')) {
			return Ok(tool_result("Searching from root directory '/' is not allowed", true));
		}

		let is_single = target_inputs.len() == 1;
		let parsed_targets: Vec<(String, ParsedFindPattern, PathBuf)> = target_inputs
			.into_iter()
			.map(|(pat, is_literal)| {
				if is_literal {
					let search_path = resolve_to_cwd(&pat, &ctx.cwd);
					let parsed = ParsedFindPattern {
						base_path: pat.clone(),
						glob_pattern: "**/*".to_string(),
						has_glob: false,
					};
					(pat, parsed, search_path)
				} else {
					let parsed = parse_find_pattern(&pat);
					let search_path = resolve_to_cwd(&parsed.base_path, &ctx.cwd);
					(pat, parsed, search_path)
				}
			})
			.collect();

		for (_, _, search_path) in &parsed_targets {
			if is_root_path(search_path) {
				return Ok(tool_result("Searching from root directory '/' is not allowed", true));
			}
		}

		let mut valid_targets = Vec::new();
		let mut missing_paths = Vec::new();

		if is_single {
			let (pat, parsed, search_path) = parsed_targets.into_iter().next().unwrap();
			if !search_path.exists() {
				return Ok(tool_result(format!("Path not found: {pat}"), true));
			}
			valid_targets.push((pat, parsed, search_path));
		} else {
			for (pat, parsed, search_path) in parsed_targets {
				if search_path.exists() {
					valid_targets.push((pat, parsed, search_path));
				} else {
					missing_paths.push(pat);
				}
			}
			if valid_targets.is_empty() {
				return Ok(tool_result(format!("Path not found: {}", missing_paths.join(", ")), true));
			}
		}

		let is_single_file =
			valid_targets.len() == 1 && !valid_targets[0].1.has_glob && valid_targets[0].2.is_file();
		let can_paginate = !is_single_file;

		let mut limit_reached = false;
		let mut candidates: Vec<PathBuf> = Vec::new();
		let mut seen_candidates = std::collections::HashSet::new();

		for (pat, parsed, search_path) in &valid_targets {
			if ctx.cancellation_token.is_cancelled() {
				return Ok(tool_result("Grep search cancelled", true));
			}
			if search_path.is_file() {
				if !parsed.has_glob {
					let abs = search_path.clone();
					let norm = normalize_path(&abs);
					if !seen_candidates.contains(&norm) {
						if candidates.len() < MAX_GREP_FILES {
							seen_candidates.insert(norm);
							candidates.push(abs);
						} else {
							limit_reached = true;
							break;
						}
					}
				} else if is_single {
					return Ok(tool_result(format!("Path is not a directory: {}", search_path.display()), true));
				}
			} else if search_path.is_dir() {
				let heartbeat = || {
					if ctx.cancellation_token.is_cancelled() {
						Err("Grep search cancelled".to_string())
					} else {
						Ok(())
					}
				};

				let mut filter = pi_walker::WalkFilter::files_only();
				if parsed.has_glob {
					let fixed_pattern = fix_unclosed_braces(parsed.glob_pattern.clone());
					let walk_glob = match pi_walker::CompiledWalkGlob::new([&fixed_pattern]) {
						Ok(compiled) => compiled,
						Err(err) => {
							return Ok(tool_result(format!("Invalid glob pattern '{pat}': {err}"), true));
						},
					};
					let mentions_node_modules = parsed.glob_pattern.contains("node_modules");
					filter = filter
						.glob(walk_glob)
						.node_modules_unless_mentioned(mentions_node_modules);
				}

				let request = pi_walker::WalkRequest::new(search_path.clone())
					.hidden(true)
					.gitignore(use_gitignore)
					.skip_git(true)
					.follow_links(pi_walker::FollowLinks::Never)
					.order(pi_walker::WalkOrder::Path)
					.filter(filter);

				let mut has_unseen = false;
				let walk_res = request.for_each_entry_with_heartbeat(
					heartbeat,
					|meta| {
						if meta.file_type == pi_walker::FileType::File
							&& meta.size.unwrap_or(0.0) <= MAX_GREP_FILE_BYTES as f64
						{
							let abs = meta.absolute_path.into_owned();
							let norm = normalize_path(&abs);
							if !seen_candidates.contains(&norm) {
								if candidates.len() < MAX_GREP_FILES {
									seen_candidates.insert(norm);
									candidates.push(abs);
								} else {
									has_unseen = true;
									return Ok(pi_walker::WalkDecision::Stop);
								}
							}
						}
						Ok(pi_walker::WalkDecision::Include)
					},
					|_| Ok(pi_walker::WalkDecision::Include),
				);

				match walk_res {
					Ok(_) => {
						if has_unseen {
							limit_reached = true;
							break;
						}
					},
					Err(err) => {
						if ctx.cancellation_token.is_cancelled()
							|| matches!(&err, pi_walker::WalkError::Interrupted(_))
						{
							return Ok(tool_result("Grep search cancelled", true));
						}
						return Ok(tool_result(format!("Walk error: {err}"), true));
					},
				}
			}
		}

		candidates.sort();

		let mut sink = BoundedText::new(MAX_GREP_OUTPUT_BYTES);
		let mut matches = 0usize;
		let mut skipped = 0usize;
		let mut total_matching_files = 0usize;

		for path in candidates {
			if ctx.cancellation_token.is_cancelled() {
				return Ok(tool_result("Grep search cancelled", true));
			}
			if matches >= MAX_GREP_MATCHES {
				break;
			}
			let Ok((bytes, size)) = read_capped(&path, MAX_GREP_FILE_BYTES).await else {
				skipped += 1;
				continue;
			};
			if size > MAX_GREP_FILE_BYTES as u64 {
				skipped += 1;
				continue;
			}
			let text = String::from_utf8_lossy(&bytes);
			let normalized = normalize_path(&path);
			let display = format_path_relative_to_cwd(&normalized, &ctx.cwd, false);

			let is_skipping_file = can_paginate && total_matching_files < skip;
			let mut file_matched = false;
			let mut file_matches = Vec::new();

			for (number, line) in text.lines().enumerate() {
				if ctx.cancellation_token.is_cancelled() {
					return Ok(tool_result("Grep search cancelled", true));
				}
				if !regex.is_match(line) {
					continue;
				}
				file_matched = true;
				if is_skipping_file {
					break;
				}
				let mut line_str = line.trim();
				if line_str.chars().count() > MAX_GREP_LINE_CHARS {
					line_str = &line_str[..floor_char_boundary(line_str, MAX_GREP_LINE_CHARS)];
				}
				file_matches.push(format!("{display}:{}: {line_str}\n", number + 1));
				if matches + file_matches.len() >= MAX_GREP_MATCHES {
					break;
				}
			}

			if file_matched {
				total_matching_files += 1;
				if !is_skipping_file {
					for m in file_matches {
						if matches >= MAX_GREP_MATCHES {
							break;
						}
						sink.push(&m);
						matches += 1;
					}
					if matches >= MAX_GREP_MATCHES {
						break;
					}
				}
			}
		}

		let missing_note = if !missing_paths.is_empty() {
			Some(format!("Skipped missing paths: {}", missing_paths.join(", ")))
		} else {
			None
		};

		let skip_past_end =
			can_paginate && skip > 0 && total_matching_files > 0 && skip >= total_matching_files;

		if matches == 0 {
			let base_msg = if limit_reached {
				format!(
					"Search limit reached ({MAX_GREP_FILES} files scanned); scan truncated, more matching files may exist beyond this window"
				)
			} else if skip_past_end {
				format!("No more results ({total_matching_files} files total; skip={skip} is past the end)")
			} else {
				"No matches found".to_string()
			};
			let mut msg = skip_note(&base_msg, skipped);
			if let Some(note) = missing_note {
				msg.push('\n');
				msg.push_str(&note);
			}
			return Ok(tool_result(msg, false));
		}

		if skipped > 0 {
			sink.push(&format!("[{skipped} file(s) skipped: unreadable or above the size limit]\n"));
		}
		if limit_reached {
			sink.push(&format!("[Search limit reached: {MAX_GREP_FILES} files scanned; scan truncated]\n"));
		}
		if let Some(note) = missing_note {
			sink.push(&format!("\n{note}"));
		}

		Ok(tool_result(sink.finish(), false))
	}
}

/// The hashline sink seam: one concrete store satisfies the read-side trait.
impl ReadSnapshotSink for hashline::NativeSnapshotSink {
	fn record_file(&self, absolute_path: &Path, full_text: &str) -> Option<String> {
		hashline::NativeSnapshotSink::record_file(self, absolute_path, full_text)
	}

	fn record_seen_lines(&self, absolute_path: &Path, tag: &str, lines: &[u32]) {
		hashline::NativeSnapshotSink::record_seen_lines(self, absolute_path, tag, lines);
	}

	fn header(&self, absolute_path: &Path, tag: &str, cwd: &Path) -> String {
		hashline::NativeSnapshotSink::header(self, absolute_path, tag, cwd)
	}
}

/// Tools registered by default.
pub fn builtin_tools() -> Vec<Arc<dyn Tool>> {
	// One snapshot store per registry; `read` mint tags here and `edit`
	// validates against them.
	let snapshots = Arc::new(hashline::SnapshotStore::new());
	let sink = Arc::new(hashline::NativeSnapshotSink::new(snapshots.clone()));
	let pending = Arc::new(pending::PendingStore::new());
	// Config-free default: hashline, `edit` mounted, no plain line numbers.
	let display = edit_variants::default_session_tools(true).display;
	vec![
		Arc::new(BashTool::new()),
		Arc::new(ReadTool::with_snapshot_sink(sink.clone()).with_display_inputs(display)),
		Arc::new(WriteTool::with_pending(Arc::clone(&pending))),
		Arc::new(EditTool::new(Arc::clone(&snapshots))),
		Arc::new(GlobTool),
		Arc::new(GrepTool),
		Arc::new(AstGrepTool::with_snapshot_sink(sink).with_display_inputs(display)),
		Arc::new(AstEditTool::with_display_inputs(
			snapshots,
			pending,
			display,
		)),
		Arc::new(TodoTool::new()),
	]
}

#[cfg(test)]
mod internal_url_write_tests {
	use std::sync::{
		Arc,
		atomic::{AtomicUsize, Ordering},
	};

	use serde_json::json;

	use super::*;
	use crate::state::AgentContext;

	struct CountingHandler {
		writable: bool,
		writes:   AtomicUsize,
	}

	#[async_trait::async_trait]
	impl internal_urls::ProtocolHandler for CountingHandler {
		fn scheme(&self) -> &str {
			"notes"
		}

		fn writable(&self) -> bool {
			self.writable
		}

		async fn resolve(
			&self,
			_url: &internal_urls::InternalUrl,
			_context: &internal_urls::ResolveContext,
		) -> Result<internal_urls::InternalResource, String> {
			Err("read is served by the read tool in this test".to_string())
		}

		async fn write(
			&self,
			_url: &internal_urls::InternalUrl,
			_content: &str,
			_context: &internal_urls::WriteContext,
		) -> Result<(), String> {
			self.writes.fetch_add(1, Ordering::SeqCst);
			Ok(())
		}
	}

	fn temp_cwd(label: &str) -> std::path::PathBuf {
		let path = std::env::temp_dir().join(format!(
			"omp-write-uri-{label}-{}",
			std::process::id()
		));
		std::fs::create_dir_all(&path).expect("temp cwd");
		path
	}

	#[tokio::test]
	async fn unknown_scheme_write_is_not_a_file() {
		let cwd = temp_cwd("unknown");
		let tool = WriteTool::new();
		let result = tool
			.execute(
				&json!({ "path": "notes://a", "content": "x" }),
				&AgentContext::new(cwd.clone()),
			)
			.await
			.expect("write executes");
		assert!(result.is_error);
		assert_eq!(result.content, "Unknown protocol: notes://\nSupported: none");
		assert!(!cwd.join("notes:").exists(), "no directory may be created");
	}

	#[tokio::test]
	async fn read_only_scheme_rejects_without_reaching_the_handler() {
		let cwd = temp_cwd("readonly");
		let router = Arc::new(InternalUrlRouter::new());
		let read_only = Arc::new(CountingHandler {
			writable: false,
			writes:   AtomicUsize::new(0),
		});
		router.register(read_only.clone()).expect("register");
		let tool = WriteTool::new()
			.with_internal_urls(Arc::clone(&router), Arc::new(UrlScope::default()));
		let result = tool
			.execute(
				&json!({ "path": "notes://a", "content": "x" }),
				&AgentContext::new(cwd.clone()),
			)
			.await
			.expect("write executes");
		assert!(result.is_error);
		assert_eq!(
			result.content,
			"notes:// URLs are read-only for write; use the protocol-specific tool for mutations."
		);
		assert_eq!(read_only.writes.load(Ordering::SeqCst), 0);

		let writable = Arc::new(CountingHandler {
			writable: true,
			writes:   AtomicUsize::new(0),
		});
		router.register(writable.clone()).expect("register");
		let result = tool
			.execute(
				&json!({ "path": "notes://a", "content": "x" }),
				&AgentContext::new(cwd.clone()),
			)
			.await
			.expect("write executes");
		assert!(!result.is_error, "{}", result.content);
		assert_eq!(result.content, "Successfully wrote 1 bytes to notes://a");
		assert_eq!(writable.writes.load(Ordering::SeqCst), 1);
	}
}

#[cfg(test)]
mod plan_mode_tool_tests {
	use std::sync::{Arc, Mutex};

	use serde_json::json;

	use super::*;
	use crate::state::{AgentContext, PlanModeGuard, PlanWriteOp, PlanWriteTarget, ProposeHandler};

	const READ_ONLY: &str =
		"Plan mode: the working tree is read-only. Write your plan to a local://<slug>-plan.md file instead.";

	/// Mirrors the host classification: only `local:` artifacts pass.
	struct SandboxGuard {
		root: PathBuf,
	}

	impl PlanModeGuard for SandboxGuard {
		fn write_error(&self, _tool_name: &str, targets: &[PlanWriteTarget<'_>]) -> Option<String> {
			if targets.is_empty() {
				return Some(READ_ONLY.to_string());
			}
			for target in targets {
				match target.op {
					PlanWriteOp::Delete => {
						return Some("Plan mode: deleting files is not allowed.".to_string());
					},
					PlanWriteOp::Rename => {
						return Some("Plan mode: renaming files is not allowed.".to_string());
					},
					PlanWriteOp::DeferredApply => return Some(READ_ONLY.to_string()),
					PlanWriteOp::Create | PlanWriteOp::Update => {
						if local_artifact_relative(target.path).is_none() {
							return Some(READ_ONLY.to_string());
						}
					},
				}
			}
			None
		}

		fn artifact_root(&self) -> Option<PathBuf> {
			Some(self.root.clone())
		}
	}

	fn guard(root: &Path) -> Arc<dyn PlanModeGuard> {
		Arc::new(SandboxGuard { root: root.to_path_buf() })
	}

	#[derive(Default)]
	struct EchoPropose {
		seen: Mutex<Vec<String>>,
	}

	#[async_trait]
	impl ProposeHandler for EchoPropose {
		async fn propose(
			&self,
			body: &str,
			_ctx: &AgentContext,
		) -> Result<ToolResult, ToolError> {
			self
				.seen
				.lock()
				.unwrap_or_else(|poison| poison.into_inner())
				.push(body.to_string());
			Ok(tool_result("proposed", false))
		}
	}

	#[derive(Default)]
	struct DeclaredAction {
		targets: Vec<String>,
	}

	#[async_trait]
	impl PendingAction for DeclaredAction {
		fn label(&self) -> &str {
			"declared rewrite"
		}

		fn source_tool(&self) -> &str {
			"ast_edit"
		}

		fn targets(&self) -> Vec<String> {
			self.targets.clone()
		}

		async fn apply(&self, _ctx: &AgentContext, _reason: &str) -> Result<ToolResult, ToolError> {
			Ok(tool_result("applied", false))
		}
	}

	fn temp_cwd(label: &str) -> PathBuf {
		let nanos = std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.expect("clock")
			.as_nanos();
		let path = std::env::temp_dir().join(format!("omp-plan-{label}-{nanos}"));
		std::fs::create_dir_all(&path).expect("mkdir");
		path
	}

	#[tokio::test]
	async fn write_blocks_working_tree_but_allows_plan_targets() {
		let cwd = temp_cwd("write");
		let artifacts = cwd.join("artifacts");
		std::fs::create_dir_all(&artifacts).expect("artifact root");
		let ctx = AgentContext::new(cwd.clone()).with_plan(Some(guard(&artifacts)));
		let tool = WriteTool::with_pending(Arc::new(PendingStore::new()));

		let blocked = tool
			.execute(&json!({ "path": "src/main.rs", "content": "x" }), &ctx)
			.await
			.expect_err("working tree blocked");
		assert_eq!(blocked.to_string(), READ_ONLY);
		assert!(!cwd.join("src/main.rs").exists());

		let local = tool
			.execute(&json!({ "path": "local://demo-plan.md", "content": "# Plan\n" }), &ctx)
			.await
			.expect("local write");
		assert!(!local.is_error, "{}", local.content);
		assert_eq!(
			std::fs::read_to_string(artifacts.join("demo-plan.md")).expect("plan file"),
			"# Plan\n"
		);
	}

	#[tokio::test]
	async fn artifact_write_never_follows_symlinks() {
		let cwd = temp_cwd("symlink");
		let artifacts = cwd.join("artifacts");
		let outside = cwd.join("outside");
		std::fs::create_dir_all(&artifacts).expect("mkdir");
		std::fs::create_dir_all(&outside).expect("mkdir");
		let outside_file = outside.join("target.md");
		std::fs::write(&outside_file, "untouched").expect("seed");
		std::os::unix::fs::symlink(&outside_file, artifacts.join("leaf.md")).expect("leaf link");
		std::os::unix::fs::symlink(&outside, artifacts.join("dir")).expect("dir link");

		let leaf = write_artifact(&artifacts, "leaf.md", b"pwn", &CancellationToken::new())
			.await
			.expect_err("leaf symlink rejected");
		assert!(leaf.to_string().contains("symlink"), "{leaf}");
		assert_eq!(std::fs::read_to_string(&outside_file).expect("outside"), "untouched");

		let nested = write_artifact(&artifacts, "dir/new/sub/plan.md", b"pwn", &CancellationToken::new())
			.await
			.expect_err("intermediate symlink rejected");
		assert!(!nested.to_string().is_empty());
		assert!(!outside.join("new").exists(), "no directory created outside the root");

		// Contained reader and writer both reject `..` before touching disk.
		assert!(read_local_artifact(&artifacts, "../outside/target.md").is_err());
		assert!(write_artifact(&artifacts, "../outside/new.md", b"pwn", &CancellationToken::new())
			.await
			.is_err());
		assert!(read_local_artifact(&artifacts, "leaf.md").is_err(), "leaf symlink read rejected");
		assert!(!outside.join("new.md").exists());
	}

	#[tokio::test]
	async fn edit_blocks_working_tree_and_classifies_parsed_targets() {
		let cwd = temp_cwd("edit");
		let artifacts = cwd.join("artifacts");
		let store = Arc::new(hashline::SnapshotStore::new());
		let edit = EditTool::new(Arc::clone(&store));

		// No guard: the working-tree call proceeds to tag validation.
		let unguarded = AgentContext::new(cwd.clone());
		assert!(edit
			.execute(&json!({ "input": "[a.txt#ABCD]\nPUT 1: x" }), &unguarded)
			.await
			.is_ok());

		let ctx = AgentContext::new(cwd.clone()).with_plan(Some(guard(&artifacts)));
		let blocked = edit
			.execute(&json!({ "input": "[a.txt#ABCD]\nPUT 1: x" }), &ctx)
			.await
			.expect_err("working tree blocked");
		assert_eq!(blocked.to_string(), READ_ONLY);

		// Spoof: an unused local `path` cannot authorize a working-tree section.
		let spoof = edit
			.execute(
				&json!({ "path": "local://safe.md", "input": "[src/file.rs#ABCD]\nPUT 1: x" }),
				&ctx,
			)
			.await
			.expect_err("spoofed path ignored");
		assert_eq!(spoof.to_string(), READ_ONLY);

		// Rename/delete sections are rejected even for artifacts.
		let rename = edit
			.execute(&json!({ "input": "[local://demo-plan.md#ABCD]\nMV other.md" }), &ctx)
			.await
			.expect_err("rename rejected");
		assert!(rename.to_string().contains("renaming files"));
	}

	#[tokio::test]
	async fn local_hashline_edit_updates_the_artifact() {
		let cwd = temp_cwd("local-edit");
		let artifacts = cwd.join("artifacts");
		std::fs::create_dir_all(&artifacts).expect("artifact root");
		let store = Arc::new(hashline::SnapshotStore::new());
		write_artifact(&artifacts, "demo-plan.md", b"# Plan\nstep one\n", &CancellationToken::new())
			.await
			.expect("seed artifact");
		let path = artifacts.join("demo-plan.md");
		let key = hashline::snapshot::canonical_key(&path);
		let tag = store.record(&key, "# Plan\nstep one\n", None).expect("tag");

		let ctx = AgentContext::new(cwd).with_plan(Some(guard(&artifacts)));
		let edit = EditTool::new(Arc::clone(&store));
		let result = edit
			.execute(
				&json!({ "input": format!("[local://demo-plan.md#{tag}]\nPUT 2: step two") }),
				&ctx,
			)
			.await
			.expect("local edit");
		assert!(!result.is_error, "{}", result.content);
		assert!(std::fs::read_to_string(&path)
			.expect("artifact")
			.contains("step two"));
	}

	#[tokio::test]
	async fn apply_patch_spoof_is_rejected() {
		let cwd = temp_cwd("apply");
		let artifacts = cwd.join("artifacts");
		let ctx = AgentContext::new(cwd).with_plan(Some(guard(&artifacts)));
		let edit = EditTool::with_mode(
			Arc::new(hashline::SnapshotStore::new()),
			hashline::edit::EditToolOptions::default(),
			edit_variants::EditMode::ApplyPatch,
		);
		let blocked = edit
			.execute(
				&json!({
					"path": "local://safe.md",
					"input": "*** Begin Patch\n*** Add File: src/new.rs\n+x\n*** End Patch"
				}),
				&ctx,
			)
			.await
			.expect_err("spoofed envelope rejected");
		assert_eq!(blocked.to_string(), READ_ONLY);
	}

	#[tokio::test]
	async fn patch_delete_and_rename_keep_their_operations() {
		let cwd = temp_cwd("patch");
		let artifacts = cwd.join("artifacts");
		let ctx = AgentContext::new(cwd).with_plan(Some(guard(&artifacts)));
		let edit = EditTool::with_mode(
			Arc::new(hashline::SnapshotStore::new()),
			hashline::edit::EditToolOptions::default(),
			edit_variants::EditMode::Patch,
		);
		let delete = edit
			.execute(
				&json!({ "path": "src/a.rs", "edits": [{ "op": "delete" }] }),
				&ctx,
			)
			.await
			.expect_err("delete rejected");
		assert!(delete.to_string().contains("deleting files"));
		let rename = edit
			.execute(
				&json!({
					"path": "src/a.rs",
					"edits": [{ "op": "update", "rename": "src/b.rs" }]
				}),
				&ctx,
			)
			.await
			.expect_err("rename rejected");
		assert!(rename.to_string().contains("renaming files"));
	}

	#[tokio::test]
	async fn absolute_artifact_paths_are_rejected() {
		let cwd = temp_cwd("absolute");
		let artifacts = cwd.join("artifacts");
		let ctx = AgentContext::new(cwd.clone()).with_plan(Some(guard(&artifacts)));
		let write = WriteTool::with_pending(Arc::new(PendingStore::new()));
		let escape = artifacts.join("../outside.txt").display().to_string();
		let blocked = write
			.execute(&json!({ "path": escape, "content": "pwn" }), &ctx)
			.await
			.expect_err("absolute escape rejected");
		assert_eq!(blocked.to_string(), READ_ONLY);
		assert!(!cwd.join("outside.txt").exists());
		let inside = artifacts.join("plan.md").display().to_string();
		assert!(write
			.execute(&json!({ "path": inside, "content": "pwn" }), &ctx)
			.await
			.is_err(), "authored absolutes are never trusted");
	}

	#[tokio::test]
	async fn ast_edit_spoof_and_deferred_apply_are_rejected() {
		let cwd = temp_cwd("ast");
		let artifacts = cwd.join("artifacts");
		let ctx = AgentContext::new(cwd).with_plan(Some(guard(&artifacts)));
		let tool = AstEditTool::new(
			Arc::new(hashline::SnapshotStore::new()),
			Arc::new(PendingStore::new()),
		);
		let blocked = tool
			.execute(
				&json!({ "path": "local://safe.md", "ops": [{ "pat": "a", "out": "b" }], "paths": ["x.ts"] }),
				&ctx,
			)
			.await
			.expect_err("spoofed paths rejected");
		assert_eq!(blocked.to_string(), READ_ONLY);

		let pending = Arc::new(PendingStore::new());
		pending.register(Arc::new(DeclaredAction {
			targets: vec!["src/file.rs".to_string()],
		}));
		let write = WriteTool::with_pending(Arc::clone(&pending));
		let deferred = write
			.execute(&json!({ "path": "xd://resolve", "content": "apply" }), &ctx)
			.await
			.expect_err("deferred apply refused");
		assert_eq!(deferred.to_string(), READ_ONLY);
		assert!(pending.peek_head().is_some(), "entry survives a refused resolve");
		let rejected = write
			.execute(&json!({ "path": "xd://reject", "content": "discard" }), &ctx)
			.await
			.expect("reject stays available");
		assert!(!rejected.is_error);
		assert!(pending.peek_head().is_none());
	}

	#[tokio::test]
	async fn propose_dispatches_to_the_handler_or_fails_closed() {
		let cwd = temp_cwd("propose");
		let ctx = AgentContext::new(cwd.clone());

		let bare = WriteTool::with_pending(Arc::new(PendingStore::new()));
		let error = bare
			.execute(&json!({ "path": "xd://propose", "content": "my-plan" }), &ctx)
			.await
			.expect_err("no handler mounted");
		assert!(error.to_string().contains("No plan is awaiting approval"), "{error}");

		let handler = Arc::new(EchoPropose::default());
		let tool = WriteTool::with_pending(Arc::new(PendingStore::new()))
			.with_propose(Some(handler.clone()));
		let result = tool
			.execute(
				&json!({ "path": "xd://propose", "content": "my-plan" }),
				&ctx,
			)
			.await
			.expect("handler dispatch");
		assert_eq!(result.content, "proposed");
		assert_eq!(
			handler
				.seen
				.lock()
				.unwrap_or_else(|poison| poison.into_inner())
				.as_slice(),
			["my-plan".to_string()]
		);
		assert!(std::fs::read_dir(&cwd).expect("read dir").next().is_none());
	}
}
