//! Durable OS process supervision for `hub` process ops and `bash async`.
//!
//! One [`ProcessManager`] lives for the lifetime of the root runner (shared by
//! the root registry and every child registry) and owns a named table of real
//! OS processes: argv launch, PTY or pipe I/O, disk-backed bounded logs with
//! cursors, log/TCP readiness, restart policy, process-tree signals, tree
//! cleanup on stop/drop, and an exit/pattern wait. The TypeScript broker keeps
//! this table in a separate process so it survives client exits; this build
//! keeps the same table in-process, so process lifetime is runner lifetime.
//!
//! Cross-exit `persist`/`detached` supervision is *not* implemented by the
//! runner-scoped manager. It belongs to the broker worker the TypeScript CLI
//! reaches through `__omp_worker_daemon_broker`: a CLI startup arm that forks a
//! broker process, a project-scoped runtime directory (`0700` dirs, `0600`
//! socket + token), peer-credential authentication, persisted `meta.json`
//! records, detached stdio into the log file, and client reconnect/replay.
//! Tools reach whichever owner exists through [`ProcessBackend`]: the local
//! [`ProcessManager`] rejects `persist`/`detached` with that prerequisite
//! named, while a broker client backend forwards the same calls to the
//! broker-owned manager (`ProcessManager::new_keepalive`), which accepts them.
//!
//! Ownership: the root caller (owner `None`) sees and controls every process;
//! a child agent sees root-owned plus its own processes and may only write
//! stdin, signal, stop, or restart processes it started itself.

use std::{
	collections::{BTreeMap, HashMap},
	fs::{File, OpenOptions},
	io::{Read, Seek, SeekFrom, Write},
	path::{Path, PathBuf},
	process::Stdio,
	sync::{
		Arc, Mutex, MutexGuard, Weak,
		atomic::{AtomicBool, AtomicU64, Ordering},
	},
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use pi_shell::{
	cancel::CancelToken,
	process::{KILL_SIGNAL, Process, TERM_SIGNAL},
};
use portable_pty::{
	ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::{
	io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
	process::{ChildStderr, ChildStdout, Command},
	sync::Notify,
};

/// PTY geometry negotiated with every supervised PTY (TypeScript parity).
const DAEMON_PTY_COLUMNS: u16 = 120;
const DAEMON_PTY_ROWS: u16 = 40;
/// Physical cap of one log generation; rotation keeps current + previous, so
/// the on-disk bound per daemon is two files of this size.
const MAX_LOG_BYTES: u64 = 25 * 1024 * 1024;
/// Bytes read from each log generation per `logs` call.
const LOG_READ_BYTES: u64 = 2 * 1024 * 1024;
/// Model-facing log body budget per `logs` call (TypeScript parity).
const LOG_TEXT_BYTES: usize = 256 * 1024;
/// Sanitized tail kept for readiness/pattern matching.
const READINESS_BUFFER_BYTES: usize = 64 * 1024;
/// Settled records stay inspectable for five minutes, capped at 100.
const RECORD_RETENTION_MS: u64 = 5 * 60 * 1000;
const MAX_SETTLED_RECORDS: usize = 100;
/// Terminal records surfaced by one `ps` call (active records are never cut).
const MAX_TERMINAL_LISTED: usize = 10;
const LOG_FILE: &str = "output.log";
const PREVIOUS_LOG_FILE: &str = "output.previous.log";

/// Restart behavior applied after an unexpected exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RestartPolicy {
	#[serde(rename = "no")]
	No,
	#[serde(rename = "on-failure")]
	OnFailure,
	#[serde(rename = "always")]
	Always,
}

impl Default for RestartPolicy {
	fn default() -> Self {
		Self::No
	}
}

/// Lifecycle state exposed on every snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProcessState {
	Starting,
	Running,
	Ready,
	Restarting,
	Stopping,
	Exited,
	Failed,
}

impl ProcessState {
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Starting => "starting",
			Self::Running => "running",
			Self::Ready => "ready",
			Self::Restarting => "restarting",
			Self::Stopping => "stopping",
			Self::Exited => "exited",
			Self::Failed => "failed",
		}
	}
}

#[must_use]
pub const fn is_terminal_state(state: ProcessState) -> bool {
	matches!(state, ProcessState::Exited | ProcessState::Failed)
}

/// Signals accepted by [`ProcessManager::send`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Signal {
	#[serde(rename = "SIGINT")]
	Int,
	#[serde(rename = "SIGTERM")]
	Term,
	#[serde(rename = "SIGHUP")]
	Hup,
	#[serde(rename = "SIGQUIT")]
	Quit,
	#[serde(rename = "SIGKILL")]
	Kill,
}

impl Signal {
	#[must_use]
	pub fn parse(raw: &str) -> Option<Self> {
		match raw {
			"SIGINT" => Some(Self::Int),
			"SIGTERM" => Some(Self::Term),
			"SIGHUP" => Some(Self::Hup),
			"SIGQUIT" => Some(Self::Quit),
			"SIGKILL" => Some(Self::Kill),
			_ => None,
		}
	}

	#[must_use]
	pub const fn number(self) -> i32 {
		match self {
			Self::Hup => 1,
			Self::Int => 2,
			Self::Quit => 3,
			Self::Kill => KILL_SIGNAL,
			Self::Term => TERM_SIGNAL,
		}
	}

	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Int => "SIGINT",
			Self::Term => "SIGTERM",
			Self::Hup => "SIGHUP",
			Self::Quit => "SIGQUIT",
			Self::Kill => "SIGKILL",
		}
	}
}

/// Readiness conditions; every supplied condition must pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadySpec {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub log: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub port: Option<u16>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub host: Option<String>,
	#[serde(default = "default_ready_timeout_ms")]
	pub timeout_ms: u64,
}

const fn default_ready_timeout_ms() -> u64 {
	30_000
}

/// Immutable launch specification retained for restart and inspection.
///
/// `env` is the child's *full* resolved environment (session env plus the
/// call's extras). It is never serialized into tool details or `describe`, so
/// provider keys and other inherited secrets do not leak through hub replies.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessSpec {
	pub name: String,
	pub application: String,
	#[serde(default)]
	pub args: Vec<String>,
	#[serde(default, skip_serializing)]
	pub env: HashMap<String, String>,
	pub cwd: String,
	pub pty: bool,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub ready: Option<ReadySpec>,
	#[serde(default)]
	pub restart: RestartPolicy,
	#[serde(default)]
	pub persist: bool,
	#[serde(default)]
	pub detached: bool,
}

impl ProcessSpec {
	fn fresh_snapshot(&self, id: String, owner: Option<String>, now: u64) -> ProcessSnapshot {
		ProcessSnapshot {
			name: self.name.clone(),
			id,
			state: if self.ready.is_some() { ProcessState::Starting } else { ProcessState::Running },
			pid: None,
			created_at: now,
			started_at: now,
			ready_at: None,
			exited_at: None,
			exit_code: None,
			exit_reason: None,
			restart_count: 0,
			output_bytes: 0,
			owner,
			ready_match: None,
			ready_pending: None,
			start_identity: None,
			persist: self.persist,
			detached: self.detached,
		}
	}
}

/// Pinned kernel identity of a spawned process: pid plus an opaque per-boot
/// start token. Persisting both lets a later broker generation re-adopt only
/// the exact process it launched, never a recycled pid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartIdentity {
	pub pid: u32,
	pub token: String,
}

impl StartIdentity {
	fn of(process: &Process) -> Self {
		Self { pid: process.pid() as u32, token: process.identity_token() }
	}

	/// True only when this exact kernel process is still the one occupying the
	/// pid, alive (not a zombie) and token-matched.
	pub fn still_matches(&self) -> bool {
		self.pin().is_some()
	}

	/// Resolve and retain exactly the recorded kernel process. The returned
	/// handle is identity-checked at creation; callers must keep it instead of
	/// reopening the pid afterwards.
	fn pin(&self) -> Option<Process> {
		let process = Process::from_pid(self.pid as i32)?;
		(process.identity_token() == self.token
			&& process.status() == pi_shell::process::ProcessStatus::Running)
			.then_some(process)
	}
}

/// Serializable process state visible to hub process ops.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessSnapshot {
	pub name: String,
	pub id: String,
	pub state: ProcessState,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub pid: Option<u32>,
	pub created_at: u64,
	pub started_at: u64,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub ready_at: Option<u64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub exited_at: Option<u64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub exit_code: Option<i32>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub exit_reason: Option<String>,
	pub restart_count: u32,
	pub output_bytes: u64,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub owner: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub ready_match: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub ready_pending: Option<Vec<String>>,
	/// Pinned `(pid, start token)`; survives broker restarts through `meta.json`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub start_identity: Option<StartIdentity>,
	pub persist: bool,
	pub detached: bool,
}

#[derive(Debug, Clone)]
pub enum ProcessError {
	Invalid(String),
	Failed(String),
}

impl std::fmt::Display for ProcessError {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Invalid(message) | Self::Failed(message) => formatter.write_str(message),
		}
	}
}

impl std::error::Error for ProcessError {}

/// `start` result: the snapshot plus whether readiness hit its deadline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartOutcome {
	pub snapshot: ProcessSnapshot,
	pub ready_timed_out: bool,
}

/// One `logs` call result. `cursor` is the output cursor after this read.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogsOutcome {
	pub name: String,
	pub text: String,
	pub cursor: u64,
	pub timed_out: bool,
	pub state: ProcessState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogOptions {
	pub lines: usize,
	pub head: bool,
	pub grep: Option<String>,
	pub follow: bool,
	pub cursor: Option<u64>,
	pub timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitRequest {
	pub for_exit: bool,
	pub pattern: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitOutcome {
	pub snapshot: ProcessSnapshot,
	pub matched: Option<String>,
	pub timed_out: bool,
}

/// Lifecycle transition published immediately after the authoritative state
/// mutation, so a broker can persist identity/spec before any waiter returns.
#[derive(Debug, Clone)]
pub struct ProcessEvent {
	pub kind: ProcessEventKind,
	pub snapshot: ProcessSnapshot,
	/// Full launch spec including `env`; never exposed through tool details.
	pub spec: ProcessSpec,
}

/// What changed; every variant ships the full current snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessEventKind {
	/// A new kernel process exists for this generation (initial or restarted).
	Spawned,
	/// The generation reached its readiness spec.
	Ready,
	/// The generation settled and a restart was armed.
	Restarting,
	/// The generation settled without a restart.
	Exited,
	/// A previous broker generation's process was re-adopted.
	Adopted,
}

/// Tool-facing supervision surface: the in-process [`ProcessManager`] and the
/// cross-exit broker client both implement it, so `hub` process ops and
/// `bash async` keep one call shape and one table regardless of who owns the
/// processes. Only the backend decides whether `persist`/`detached` is legal
/// (the local manager rejects it, the broker-owned manager accepts it).
#[async_trait]
pub trait ProcessBackend: Send + Sync {
	async fn start(
		self: Arc<Self>,
		spec: ProcessSpec,
		owner: Option<String>,
	) -> Result<StartOutcome, ProcessError>;
	async fn list(
		self: Arc<Self>,
		owner: Option<String>,
	) -> Result<Vec<ProcessSnapshot>, ProcessError>;
	async fn logs(
		self: Arc<Self>,
		name: String,
		options: LogOptions,
		owner: Option<String>,
	) -> Result<LogsOutcome, ProcessError>;
	async fn wait(
		self: Arc<Self>,
		name: String,
		request: WaitRequest,
		timeout_ms: u64,
		owner: Option<String>,
	) -> Result<WaitOutcome, ProcessError>;
	async fn send(
		self: Arc<Self>,
		name: String,
		data: Option<String>,
		signal: Option<Signal>,
		owner: Option<String>,
	) -> Result<ProcessSnapshot, ProcessError>;
	async fn stop(
		self: Arc<Self>,
		name: String,
		timeout_ms: u64,
		owner: Option<String>,
	) -> Result<ProcessSnapshot, ProcessError>;
	async fn restart(
		self: Arc<Self>,
		name: String,
		owner: Option<String>,
	) -> Result<ProcessSnapshot, ProcessError>;
	async fn describe(
		self: Arc<Self>,
		name: String,
		owner: Option<String>,
	) -> Result<(ProcessSnapshot, ProcessSpec), ProcessError>;
	/// Whether background (`bash async`) execution is allowed by the session
	/// config that owns this backend.
	fn async_enabled(&self) -> bool;
	/// Apply the session `async.enabled` setting to this backend.
	fn set_async_enabled(&self, enabled: bool);
	/// Allocate a collision-free job name for `bash async` on this backend.
	fn next_background_name(&self) -> String;
	/// Arm the background deadline for a `bash async` job; the backend records
	/// the timeout in its own exit-reason vocabulary.
	fn arm_timeout(self: Arc<Self>, name: String, owner: Option<String>, timeout_ms: u64);
}

fn now_ms() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|duration| duration.as_millis() as u64)
		.unwrap_or_default()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
	mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Strip ANSI CSI sequences, then every control character except tab/newline
/// (TypeScript `sanitizeText` behavior: CR and ESC-class controls are removed).
fn sanitize_text(input: &str) -> String {
	let stripped = if input.contains('\x1b') {
		pi_shell::minimizer::primitives::strip_ansi(input)
	} else {
		input.to_string()
	};
	stripped
		.chars()
		.filter(|ch| {
			let code = *ch as u32;
			!matches!(code, 0x00..=0x08 | 0x0b..=0x1f | 0x7f..=0x9f)
		})
		.collect()
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

/// Keep `max_lines` / `max_bytes` from the head or tail of `text`. Lines are
/// split exactly like TypeScript's newline-based helpers, so a trailing
/// newline survives an untruncated read.
fn truncate_lines(text: &str, max_lines: usize, max_bytes: usize, head: bool) -> String {
	let parts: Vec<&str> = text.split('\n').collect();
	if parts.len() <= max_lines && text.len() <= max_bytes {
		return text.to_string();
	}
	let mut selected: Vec<&str> = Vec::new();
	let mut bytes = 0usize;
	let lines: Vec<&str> = if head {
		parts
	} else {
		parts.into_iter().rev().collect()
	};
	for line in lines {
		let extra = line.len() + if selected.is_empty() { 0 } else { 1 };
		if selected.len() >= max_lines || bytes + extra > max_bytes {
			if selected.is_empty() {
				let cut = floor_char_boundary(line, max_bytes);
				selected.push(&line[..cut]);
			}
			break;
		}
		bytes += extra;
		selected.push(line);
	}
	if !head {
		selected.reverse();
	}
	selected.join("\n")
}

fn state_label(state: ProcessState) -> &'static str {
	state.as_str()
}

struct LogState {
	file: Option<File>,
	size: u64,
}

/// Disk-backed bounded log with current/previous generations.
struct LogFile {
	dir: PathBuf,
	path: PathBuf,
	previous: PathBuf,
	state: Mutex<LogState>,
}

fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
	let mut builder = std::fs::DirBuilder::new();
	builder.recursive(true);
	#[cfg(unix)]
	{
		use std::os::unix::fs::DirBuilderExt;
		builder.mode(0o700);
	}
	builder.create(dir)
}

fn create_private(path: &Path) -> std::io::Result<File> {
	let mut options = OpenOptions::new();
	options.write(true).create(true).truncate(true);
	#[cfg(unix)]
	{
		use std::os::unix::fs::OpenOptionsExt;
		options.mode(0o600);
	}
	options.open(path)
}

fn read_slice(path: &Path, head: bool) -> String {
	let Ok(metadata) = std::fs::metadata(path) else {
		return String::new();
	};
	let size = metadata.len();
	let Ok(mut file) = File::open(path) else {
		return String::new();
	};
	let mut bytes = Vec::new();
	if size <= LOG_READ_BYTES {
		if file.take(LOG_READ_BYTES).read_to_end(&mut bytes).is_err() {
			return String::new();
		}
	} else if head {
		bytes.resize(LOG_READ_BYTES as usize, 0);
		if file.read_exact(&mut bytes).is_err() {
			return String::new();
		}
	} else {
		let start = size - LOG_READ_BYTES;
		if file.seek(SeekFrom::Start(start)).is_err() {
			return String::new();
		}
		if file.take(LOG_READ_BYTES).read_to_end(&mut bytes).is_err() {
			return String::new();
		}
	}
	String::from_utf8_lossy(&bytes).into_owned()
}

/// Open an existing log generation for appending without truncating it.
fn open_append_private(path: &Path) -> std::io::Result<File> {
	let mut options = OpenOptions::new();
	options.append(true).create(true);
	#[cfg(unix)]
	{
		use std::os::unix::fs::OpenOptionsExt;
		options.mode(0o600);
	}
	options.open(path)
}

impl LogFile {
	fn open(dir: &Path) -> std::io::Result<Self> {
		ensure_private_dir(dir)?;
		let path = dir.join(LOG_FILE);
		let previous = dir.join(PREVIOUS_LOG_FILE);
		let _ = std::fs::remove_file(&previous);
		let _ = std::fs::rename(&path, &previous);
		let file = create_private(&path)?;
		Ok(Self {
			dir: dir.to_path_buf(),
			path,
			previous,
			state: Mutex::new(LogState { file: Some(file), size: 0 }),
		})
	}

	/// Reopen a log directory left by a previous broker generation, keeping the
	/// existing bytes (detached processes write this file through their own fd).
	fn adopt(dir: &Path) -> std::io::Result<Self> {
		ensure_private_dir(dir)?;
		let path = dir.join(LOG_FILE);
		let previous = dir.join(PREVIOUS_LOG_FILE);
		let size = std::fs::metadata(&path).map_or(0, |metadata| metadata.len());
		let file = open_append_private(&path)?;
		Ok(Self {
			dir: dir.to_path_buf(),
			path,
			previous,
			state: Mutex::new(LogState { file: Some(file), size }),
		})
	}

	/// Size of the current log generation as visible to a detached writer.
	fn current_size(&self) -> u64 {
		std::fs::metadata(&self.path).map_or(0, |metadata| metadata.len())
	}

	/// Bytes appended since `offset`, capped to the last readable window. Returns
	/// the text and the new cursor.
	fn read_appended(&self, offset: u64) -> (String, u64) {
		if let Some(file) = lock(&self.state).file.as_mut() {
			let _ = file.flush();
		}
		let size = self.current_size();
		let start = offset.min(size);
		if start == size {
			return (String::new(), size);
		}
		let tail_start = start.max(size.saturating_sub(LOG_READ_BYTES));
		let Ok(mut file) = File::open(&self.path) else {
			return (String::new(), size);
		};
		if file.seek(SeekFrom::Start(tail_start)).is_err() {
			return (String::new(), size);
		}
		let mut bytes = Vec::new();
		if file.take(LOG_READ_BYTES).read_to_end(&mut bytes).is_err() {
			return (String::new(), size);
		}
		(String::from_utf8_lossy(&bytes).into_owned(), size)
	}

	/// Move the current generation aside and start an empty one.
	fn rotate(&self) -> std::io::Result<()> {
		let mut state = lock(&self.state);
		if let Some(file) = state.file.as_mut() {
			let _ = file.flush();
		}
		state.file = None;
		let _ = std::fs::remove_file(&self.previous);
		let _ = std::fs::rename(&self.path, &self.previous);
		let file = create_private(&self.path)?;
		state.file = Some(file);
		state.size = 0;
		Ok(())
	}

	/// Append raw output, rotating before the physical cap is crossed. Returns
	/// the number of bytes accepted.
	fn append(&self, text: &str) -> u64 {
		let mut state = lock(&self.state);
		let bytes = text.len() as u64;
		if state.size > 0 && state.size.saturating_add(bytes) > MAX_LOG_BYTES {
			if let Some(file) = state.file.as_mut() {
				let _ = file.flush();
			}
			state.file = None;
			let _ = std::fs::remove_file(&self.previous);
			let _ = std::fs::rename(&self.path, &self.previous);
			state.file = create_private(&self.path).ok();
			state.size = 0;
		}
		if let Some(file) = state.file.as_mut() {
			let _ = file.write_all(text.as_bytes());
			let _ = file.flush();
			state.size = state.size.saturating_add(bytes);
		}
		bytes
	}

	/// Read both generations, sanitize, filter, and bound the returned body.
	fn read(&self, head: bool, lines: usize, grep: Option<&Regex>) -> String {
		let mut state = lock(&self.state);
		if let Some(file) = state.file.as_mut() {
			let _ = file.flush();
		}
		// Keep the append lock while reading so the two generations and the
		// reported cursor describe one consistent snapshot.
		let previous = read_slice(&self.previous, head);
		let current = read_slice(&self.path, head);
		drop(state);
		let mut combined = previous;
		if !combined.is_empty() && !current.is_empty() && !combined.ends_with('\n') {
			combined.push('\n');
		}
		combined.push_str(&current);
		let windowed = if head {
			let cut = floor_char_boundary(&combined, LOG_READ_BYTES as usize);
			combined[..cut].to_string()
		} else {
			let start = combined.len().saturating_sub(LOG_READ_BYTES as usize);
			let cut = floor_char_boundary(&combined, start);
			combined[cut..].to_string()
		};
		let mut text = sanitize_text(&windowed);
		if let Some(pattern) = grep {
			text = text
				.split('\n')
				.filter(|line| pattern.is_match(line))
				.collect::<Vec<_>>()
				.join("\n");
		}
		truncate_lines(&text, lines, LOG_TEXT_BYTES, head)
	}

	/// Physical cleanup for a pruned record.
	fn discard(&self) {
		lock(&self.state).file = None;
		let _ = std::fs::remove_dir_all(&self.dir);
	}
}

/// Input accepted by a live generation: pipe stdin or a PTY master writer.
enum Input {
	Pipe(tokio::process::ChildStdin),
	Blocking(Box<dyn Write + Send>),
}

struct RecordState {
	spec: ProcessSpec,
	snapshot: ProcessSnapshot,
	generation: u64,
	stop_requested: bool,
	log_ready: bool,
	port_ready: bool,
	readiness: String,
	ready_regex: Option<Regex>,
	consecutive_failures: u32,
	restart_task: Option<tokio::task::JoinHandle<()>>,
	input: Option<Arc<tokio::sync::Mutex<Input>>>,
	killer: Option<Box<dyn ChildKiller + Send + Sync>>,
	/// Identity-checked handle captured immediately after spawn; all signals
	/// and liveness checks use this instead of a bare pid.
	pinned: Option<Process>,
	/// Process-group id captured while the root was alive; used to sweep
	/// descendants after the root has already been reaped.
	group_id: Option<i32>,
	/// Kept alive so PTY master clones stay valid for the process lifetime.
	_master: Option<Box<dyn MasterPty + Send>>,
	/// Reason the caller imposed (timeout), merged into the settle snapshot.
	pending_exit_reason: Option<String>,
}

struct Record {
	state: Mutex<RecordState>,
	changed: Notify,
	log: Arc<LogFile>,
	/// Serializes stop/restart/timeout sequences so a relaunch cannot slip
	/// between another caller's terminate and settle.
	lifecycle: tokio::sync::Mutex<()>,
}

impl Record {
	fn notify_change(&self) {
		self.changed.notify_waiters();
	}
}

fn sync_ready_pending(state: &mut RecordState) {
	if state.snapshot.state != ProcessState::Starting {
		state.snapshot.ready_pending = None;
		return;
	}
	let mut pending = Vec::new();
	if !state.log_ready {
		pending.push("log".to_string());
	}
	if !state.port_ready {
		pending.push("port".to_string());
	}
	state.snapshot.ready_pending = if pending.is_empty() { None } else { Some(pending) };
}

fn mark_ready(state: &mut RecordState) -> bool {
	if state.spec.ready.is_none() || state.snapshot.state != ProcessState::Starting {
		return false;
	}
	if !state.log_ready || !state.port_ready {
		return false;
	}
	state.snapshot.state = ProcessState::Ready;
	state.snapshot.ready_at = Some(now_ms());
	state.snapshot.ready_pending = None;
	true
}

fn push_readiness(buffer: &mut String, text: &str) {
	buffer.push_str(text);
	if buffer.len() > READINESS_BUFFER_BYTES {
		let start = buffer.len() - READINESS_BUFFER_BYTES;
		let cut = floor_char_boundary(buffer, start);
		buffer.drain(..cut);
	}
}

fn valid_process_name(name: &str) -> bool {
	let mut chars = name.chars();
	match chars.next() {
		Some(first) if first.is_ascii_alphanumeric() => {},
		_ => return false,
	}
	name.chars().count() <= 48
		&& chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

fn validate_spec(spec: &ProcessSpec, allow_cross_exit: bool) -> Result<(), ProcessError> {
	if !valid_process_name(&spec.name) {
		return Err(ProcessError::Invalid(
			"Process name must be 1-48 letters, numbers, dots, underscores, or hyphens".to_string(),
		));
	}
	if spec.application.trim().is_empty() {
		return Err(ProcessError::Invalid("start requires application".to_string()));
	}
	if (spec.persist || spec.detached) && !allow_cross_exit {
		return Err(ProcessError::Invalid(
			"persist/detached supervision needs the cross-process daemon broker, which is not the owner of this process manager; the local manager is runner-scoped. Retry with persist:false, detached:false (or use foreground bash).".to_string(),
		));
	}
	if spec.detached && spec.pty {
		return Err(ProcessError::Invalid("A detached process cannot allocate a PTY".to_string()));
	}
	let cwd = Path::new(&spec.cwd);
	if !cwd.exists() {
		return Err(ProcessError::Invalid(format!("Process cwd does not exist: {}", spec.cwd)));
	}
	if !cwd.is_dir() {
		return Err(ProcessError::Invalid(format!(
			"Process cwd is not a directory: {}",
			spec.cwd
		)));
	}
	if let Some(ready) = &spec.ready {
		if ready.log.as_deref().map(str::trim).unwrap_or_default().is_empty() && ready.port.is_none() {
			return Err(ProcessError::Invalid("ready requires log or port".to_string()));
		}
		if let Some(pattern) = &ready.log {
			Regex::new(pattern)
				.map_err(|err| ProcessError::Invalid(format!("Invalid readiness regex: {err}")))?;
		}
	}
	Ok(())
}

fn ready_regex(spec: &ProcessSpec) -> Option<Regex> {
	spec.ready
		.as_ref()
		.and_then(|ready| ready.log.as_ref())
		.and_then(|pattern| Regex::new(pattern).ok())
}

/// Resolve the secure per-user, per-profile, per-runner log root.
///
/// Mirrors `omp-cli::dirs` (`PI_CONFIG_DIR` > `~/.omp`, named profile
/// `<base>/profiles/<profile>/agent`, `PI_CODING_AGENT_DIR` override) without
/// depending on the CLI crate, then appends `processes/<pid>` so two runners
/// never share daemon names or logs. Never falls back to ambient `/tmp`.
#[must_use]
pub fn default_log_root() -> PathBuf {
	let base = std::env::var_os("PI_CONFIG_DIR")
		.map(PathBuf::from)
		.filter(|path| !path.as_os_str().is_empty())
		.unwrap_or_else(|| {
			std::env::var_os("HOME")
				.or_else(|| std::env::var_os("USERPROFILE"))
				.map(PathBuf::from)
				.filter(|path| !path.as_os_str().is_empty())
				.unwrap_or_else(|| PathBuf::from(".omp"))
				.join(".omp")
		});
	let profile = ["OMP_PROFILE", "PI_PROFILE"]
		.iter()
		.find_map(|key| std::env::var(key).ok())
		.filter(|value| !value.is_empty());
	let agent = match profile {
		Some(profile) => base.join("profiles").join(profile).join("agent"),
		None => std::env::var_os("PI_CODING_AGENT_DIR")
			.map(PathBuf::from)
			.filter(|path| !path.as_os_str().is_empty())
			.unwrap_or_else(|| base.join("agent")),
	};
	agent.join("processes").join(std::process::id().to_string())
}

struct Inner {
	log_root: PathBuf,
	/// Broker-owned managers (the cross-exit daemon broker worker) accept
	/// `persist`/`detached`, keep their log tree across manager drops, and let
	/// persistent processes outlive broker shutdown. Runner managers do none of
	/// those: a local persistence promise would be fake.
	broker_owned: bool,
	records: Mutex<BTreeMap<String, Arc<Record>>>,
	async_enabled: AtomicBool,
	shutting_down: AtomicBool,
	next_id: AtomicU64,
	background_seq: AtomicU64,
	events: tokio::sync::broadcast::Sender<ProcessEvent>,
}

/// Cancellation guard for a `start` future that is dropped while waiting for
/// readiness: the caller never received a handle, so the spawned child must not
/// be left running (PTY children especially would otherwise outlive the call).
/// Synchronous kill only; the generation's normal settle task marks the record.
struct AbandonLaunch {
	record: Arc<Record>,
	armed: bool,
}

impl AbandonLaunch {
	fn disarm(&mut self) {
		self.armed = false;
	}
}

impl Drop for AbandonLaunch {
	fn drop(&mut self) {
		if !self.armed {
			return;
		}
		let (pinned, group_id) = {
			let mut state = lock(&self.record.state);
			if is_terminal_state(state.snapshot.state) {
				return;
			}
			state.stop_requested = true;
			state.snapshot.state = ProcessState::Stopping;
			state.snapshot.ready_pending = None;
			(state.pinned.take(), state.group_id.take())
		};
		if let Some(group_id) = group_id {
			kill_process_group(group_id, KILL_SIGNAL);
		}
		if let Some(process) = pinned {
			let _ = process.kill_tree(Some(KILL_SIGNAL));
		}
		self.record.notify_change();
	}
}

/// Named OS-process table shared by `bash async` and every `hub` process op.
pub struct ProcessManager {
	inner: Arc<Inner>,
}

impl Clone for ProcessManager {
	fn clone(&self) -> Self {
		Self { inner: Arc::clone(&self.inner) }
	}
}

impl ProcessManager {
	pub fn new(log_root: PathBuf) -> Self {
		Self::with_mode(log_root, false)
	}

	/// Broker-owned manager: accepts `persist`/`detached`, retains the log tree
	/// on drop, and leaves persistent processes running when the broker shuts
	/// down. Only the cross-exit daemon broker worker may construct this; the
	/// in-process runner must use [`ProcessManager::new`].
	pub fn new_keepalive(log_root: PathBuf) -> Self {
		Self::with_mode(log_root, true)
	}

	fn with_mode(log_root: PathBuf, broker_owned: bool) -> Self {
		Self {
			inner: Arc::new(Inner {
				log_root,
				broker_owned,
				records: Mutex::new(BTreeMap::new()),
				async_enabled: AtomicBool::new(true),
				shutting_down: AtomicBool::new(false),
				next_id: AtomicU64::new(1),
				background_seq: AtomicU64::new(1),
				events: tokio::sync::broadcast::channel(64).0,
			}),
		}
	}

	pub fn set_async_enabled(&self, enabled: bool) {
		self.inner.async_enabled.store(enabled, Ordering::SeqCst);
	}

	#[must_use]
	pub fn async_enabled(&self) -> bool {
		self.inner.async_enabled.load(Ordering::SeqCst)
	}

	/// Next stable `bash-<n>` name for a backgrounded bash call.
	#[must_use]
	pub fn next_background_name(&self) -> String {
		format!("bash-{}", self.inner.background_seq.fetch_add(1, Ordering::SeqCst))
	}

	/// Subscribe to lifecycle transitions.
	///
	/// Ordering guarantee: each event is built under the record's state lock
	/// after the mutation and sent before `notify_change`, so:
	/// - `Spawned` is observable before the launch's readiness wait returns and
	///   before any `logs`/`ps` can expose the pid;
	/// - `Exited`/`Restarting` precede the corresponding waiter wake-up.
	/// Slow consumers receive `Lagged`; recover by resubscribing and then calling
	/// `list(None)` + `describe(name, None)` (events are idempotent snapshots).
	#[must_use]
	pub fn events(&self) -> tokio::sync::broadcast::Receiver<ProcessEvent> {
		self.inner.events.subscribe()
	}

	fn publish(&self, record: &Arc<Record>, kind: ProcessEventKind) {
		let event = {
			let state = lock(&record.state);
			ProcessEvent { kind, snapshot: state.snapshot.clone(), spec: state.spec.clone() }
		};
		let _ = self.inner.events.send(event);
	}

	fn next_id(&self) -> String {
		format!(
			"{:016x}-{:04x}",
			now_ms(),
			self.inner.next_id.fetch_add(1, Ordering::SeqCst)
		)
	}

	/// Kill every supervised process and abort restart timers. Idempotent.
	///
	/// A broker-owned manager leaves `persist`/`detached` processes running:
	/// that is the whole cross-exit contract; their stdio stays attached to the
	/// log files. Everything else (and every stateless runner manager) is killed.
	pub fn shutdown(&self) {
		self.inner.shutting_down.store(true, Ordering::SeqCst);
		let records: Vec<Arc<Record>> = lock(&self.inner.records).values().cloned().collect();
		for record in records {
			let (task, pinned, mut killer, persistent, group_id) = {
				let mut state = lock(&record.state);
				let task = state.restart_task.take();
				let persistent = self.inner.broker_owned
					&& state.snapshot.detached
					&& !is_terminal_state(state.snapshot.state);
				if persistent {
					(task, None, None, true, None)
				} else {
					let pinned = state.pinned.take();
					let group_id = state.group_id.take();
					let killer = state.killer.as_ref().map(|killer| killer.clone_killer());
					state.input = None;
					state.snapshot.pid = None;
					if !is_terminal_state(state.snapshot.state) {
						state.snapshot.state = ProcessState::Exited;
						state.snapshot.exited_at = Some(now_ms());
						state.snapshot.ready_pending = None;
						state.snapshot.exit_reason = Some("process manager shut down".to_string());
					}
					(task, pinned, killer, false, group_id)
				}
			};
			if let Some(task) = task {
				task.abort();
			}
			if persistent {
				continue;
			}
			if let Some(group_id) = group_id {
				kill_process_group(group_id, KILL_SIGNAL);
			}
			if let Some(process) = pinned {
				let _ = process.kill_tree(Some(KILL_SIGNAL));
			} else if let Some(killer) = killer.as_mut() {
				let _ = killer.kill();
			}
			record.notify_change();
		}
	}

	fn record_visible(state: &RecordState, caller: Option<&str>) -> bool {
		match caller {
			None => true,
			Some(caller) => state
				.snapshot
				.owner
				.as_deref()
				.is_none_or(|owner| owner == caller),
		}
	}

	fn record(&self, name: &str, caller: Option<&str>) -> Result<Arc<Record>, ProcessError> {
		let record = lock(&self.inner.records).get(name).cloned();
		match record {
			Some(record) if Self::record_visible(&lock(&record.state), caller) => Ok(record),
			_ => {
				let available: Vec<String> = lock(&self.inner.records)
					.iter()
					.filter(|(_, record)| Self::record_visible(&lock(&record.state), caller))
					.map(|(name, _)| name.clone())
					.collect();
				Err(ProcessError::Invalid(format!(
					"Unknown process {name}{}",
					if available.is_empty() {
						String::new()
					} else {
						format!(". Available: {}", available.join(", "))
					}
				)))
			},
		}
	}

	fn ensure_control(state: &RecordState, caller: Option<&str>) -> Result<(), ProcessError> {
		match caller {
			None => Ok(()),
			Some(caller) => {
				if state.snapshot.owner.as_deref() == Some(caller) {
					Ok(())
				} else {
					Err(ProcessError::Invalid(format!(
						"Process \"{}\" belongs to another agent; stdin, signals, stop, and restart are owner-scoped",
						state.snapshot.name
					)))
				}
			},
		}
	}

	/// Start (or replace a terminal record with) a supervised process and, when
	/// a readiness spec exists, wait for it up to its deadline.
	pub async fn start(
		self: &Arc<Self>,
		spec: ProcessSpec,
		owner: Option<String>,
	) -> Result<StartOutcome, ProcessError> {
		validate_spec(&spec, self.inner.broker_owned)?;
		// Check-and-insert or check-and-rearm happens under one `records` guard,
		// so two concurrent `start` calls for the same name can never both spawn:
		// the winner leaves a non-terminal record before releasing the guard.
		let record = loop {
			let existing = {
				let mut records = lock(&self.inner.records);
				self.prune(&mut records);
				records.get(&spec.name).cloned()
			};
			match existing {
				Some(record) => {
					let gate = record.lifecycle.lock().await;
					// Close the record/state guards lexically before the spawn
					// await; only the lifecycle gate spans `launch`.
					{
					let records = lock(&self.inner.records);
					let current = records.get(&spec.name).cloned();
					if !current.as_ref().is_some_and(|current| Arc::ptr_eq(current, &record)) {
						drop(records);
						drop(gate);
						continue;
					}
					{
						let mut state = lock(&record.state);
						if !is_terminal_state(state.snapshot.state) {
							return Err(ProcessError::Invalid(format!(
								"Process \"{}\" is already registered and {}; choose another name",
								spec.name,
								state_label(state.snapshot.state)
							)));
						}
						if let Some(caller) = owner.as_deref()
							&& state.snapshot.owner.as_deref() != Some(caller)
						{
							return Err(ProcessError::Invalid(format!(
								"Process \"{}\" was started by another agent; choose another name",
								spec.name
							)));
						}
						if let Some(task) = state.restart_task.take() {
							task.abort();
						}
						record.log.rotate().map_err(|err| {
							ProcessError::Failed(format!("cannot rotate process log: {err}"))
						})?;
						// Reusing the record keeps waiters bound to the old generation,
						// which then observe the generation change instead of reading the
						// replacement's output. Mutated only under the lifecycle gate.
						state.snapshot = spec.fresh_snapshot(self.next_id(), owner.clone(), now_ms());
						state.spec = spec.clone();
						state.stop_requested = false;
						state.log_ready =
							spec.ready.as_ref().is_none_or(|ready| ready.log.is_none());
						state.port_ready =
							spec.ready.as_ref().is_none_or(|ready| ready.port.is_none());
						state.readiness.clear();
						state.ready_regex = ready_regex(&spec);
						state.consecutive_failures = 0;
						state.pending_exit_reason = None;
						state.input = None;
						state.killer = None;
						state.pinned = None;
						state.group_id = None;
						state._master = None;
						sync_ready_pending(&mut state);
					}
					drop(records);
					}
					record.notify_change();
					self.launch(&record).await;
					drop(gate);
					break record;
				},
				None => {
					// Create + insert under one records guard, hand the Arc out of
					// this scope, and only then await the lifecycle gate: the
					// reservation stays atomic and no std guard crosses the await.
					let record = {
					let mut records = lock(&self.inner.records);
					self.prune(&mut records);
					if records.contains_key(&spec.name) {
						drop(records);
						continue;
					}
					let dir = self.inner.log_root.join(&spec.name);
					let log = Arc::new(LogFile::open(&dir).map_err(|err| {
						ProcessError::Failed(format!("cannot open process log: {err}"))
					})?);
					let mut state = RecordState {
						spec: spec.clone(),
						snapshot: spec.fresh_snapshot(self.next_id(), owner.clone(), now_ms()),
						generation: 0,
						stop_requested: false,
						log_ready: spec.ready.as_ref().is_none_or(|ready| ready.log.is_none()),
						port_ready: spec.ready.as_ref().is_none_or(|ready| ready.port.is_none()),
						readiness: String::new(),
						ready_regex: ready_regex(&spec),
						consecutive_failures: 0,
						restart_task: None,
						input: None,
						killer: None,
						pinned: None,
						group_id: None,
						_master: None,
						pending_exit_reason: None,
					};
					sync_ready_pending(&mut state);
					let record = Arc::new(Record {
						state: Mutex::new(state),
						changed: Notify::new(),
						log,
						lifecycle: tokio::sync::Mutex::new(()),
					});
					records.insert(spec.name.clone(), Arc::clone(&record));
					record
					};
					let gate = record.lifecycle.lock().await;
					self.launch(&record).await;
					drop(gate);
					break record;
				},
			}
		};

		let mut abandoned = AbandonLaunch { record: Arc::clone(&record), armed: true };

		let mut ready_timed_out = false;
		if let Some(ready) = spec.ready.as_ref()
			&& !is_terminal_state(lock(&record.state).snapshot.state)
		{
			let woke = wait_until(&record, ready.timeout_ms, || {
				let state = lock(&record.state);
				state.snapshot.ready_at.is_some()
					|| is_terminal_state(state.snapshot.state)
					|| state.snapshot.state == ProcessState::Restarting
			})
			.await;
			ready_timed_out = !woke;
		}
		abandoned.disarm();
		Ok(StartOutcome {
			snapshot: lock(&record.state).snapshot.clone(),
			ready_timed_out,
		})
	}

	/// One launch generation: spawn, store handles, and stream output.
	async fn launch(self: &Arc<Self>, record: &Arc<Record>) {
		let handle = tokio::runtime::Handle::current();
		let (spec, generation) = {
			let mut state = lock(&record.state);
			// A stop (or a stop that raced the restart timer) wins over this
			// generation: never respawn a record that was asked to stop.
			if state.stop_requested || state.snapshot.state == ProcessState::Stopping {
				state.snapshot.state = ProcessState::Exited;
				state.snapshot.exited_at.get_or_insert_with(now_ms);
				state.snapshot.ready_pending = None;
				state.pending_exit_reason = None;
				drop(state);
				record.notify_change();
				return;
			}
			state.generation = state.generation.wrapping_add(1);
			let generation = state.generation;
			state.stop_requested = false;
			state.snapshot.state = if state.spec.ready.is_some() {
				ProcessState::Starting
			} else {
				ProcessState::Running
			};
			state.snapshot.started_at = now_ms();
			state.snapshot.ready_at = None;
			state.snapshot.exited_at = None;
			state.snapshot.exit_code = None;
			state.snapshot.exit_reason = None;
			state.snapshot.pid = None;
			state.snapshot.start_identity = None;
			state.snapshot.ready_match = None;
			state.snapshot.ready_pending = None;
			state.log_ready = state.spec.ready.as_ref().is_none_or(|ready| ready.log.is_none());
			state.port_ready = state.spec.ready.as_ref().is_none_or(|ready| ready.port.is_none());
			state.readiness.clear();
			state.input = None;
			state.killer = None;
			state.pinned = None;
			state.group_id = None;
			state._master = None;
			state.pending_exit_reason = None;
			sync_ready_pending(&mut state);
			(state.spec.clone(), generation)
		};
		record.notify_change();

		let mut launched = false;
		if spec.detached {
			match spawn_detached(&spec, &record.log.path) {
				Ok(spawned) => {
					let pid = spawned.pid;
					let pinned = pid.and_then(|pid| Process::from_pid(pid as i32));
					let identity = pinned.as_ref().map(StartIdentity::of);
					let keep = {
						let mut state = lock(&record.state);
						let cancelled = state.generation != generation
							|| is_terminal_state(state.snapshot.state)
							|| state.snapshot.state == ProcessState::Stopping
							|| state.stop_requested;
						if cancelled {
							false
						} else {
							state.snapshot.pid = pid;
							state.snapshot.start_identity = identity;
							state.pinned.clone_from(&pinned);
							state.group_id = pinned.as_ref().and_then(|process| process.group_id());
							state.input = None;
							state.killer = None;
							state._master = None;
							true
						}
					};
					if keep {
						self.publish(record, ProcessEventKind::Spawned);
					}
					record.notify_change();
					if !keep {
						if let Some(pinned) = pinned {
							let _ = pinned.kill_tree(Some(KILL_SIGNAL));
						}
						let mut child = spawned.child;
						tokio::spawn(async move {
							let _ = child.wait().await;
						});
						return;
					}
					launched = true;
					let mut child = spawned.child;
					tokio::spawn(async move {
						let _ = child.wait().await;
					});
					self.monitor_detached(record, generation);
				},
				Err(message) => self.fail_launch(record, generation, message, &handle),
			}
		} else if spec.pty {
			match spawn_pty(&spec) {
				Ok(spawned) => {
					let SpawnedPty { pid, input, killer, master, reader, child } = spawned;
					let pinned = pid.and_then(|pid| Process::from_pid(pid as i32));
					let identity = pinned.as_ref().map(StartIdentity::of);
					let keep = {
						let mut state = lock(&record.state);
						let cancelled = state.generation != generation
							|| is_terminal_state(state.snapshot.state)
							|| state.snapshot.state == ProcessState::Stopping
							|| state.stop_requested;
						if cancelled {
							false
						} else {
							state.snapshot.pid = pid;
							state.snapshot.start_identity = identity;
							state.input = Some(Arc::new(tokio::sync::Mutex::new(Input::Blocking(
								input,
							))));
							state.killer = Some(killer);
							state.pinned.clone_from(&pinned);
							state.group_id = pinned.as_ref().and_then(|process| process.group_id());
							state._master = Some(master);
							true
						}
					};
					if keep {
						self.publish(record, ProcessEventKind::Spawned);
					}
					record.notify_change();
					if !keep {
						if let Some(pinned) = pinned {
							let _ = pinned.kill_tree(Some(KILL_SIGNAL));
						}
						// `spawn_pty` returns a live child; whoever rejects the
						// generation must reap it, never leak an orphan.
						std::thread::spawn(move || {
							let mut child = child;
							let _ = child.wait();
						});
						return;
					}
					launched = true;
					let manager = Arc::downgrade(self);
					let read_record = Arc::downgrade(record);
					std::thread::spawn(move || {
						let mut reader = reader;
						let mut buffer = [0u8; 8192];
						loop {
							match reader.read(&mut buffer) {
								Ok(0) | Err(_) => break,
								Ok(read) => {
									let (Some(manager), Some(record)) =
										(manager.upgrade(), read_record.upgrade())
									else {
										break;
									};
									let stale = lock(&record.state).generation != generation;
									if stale {
										break;
									}
									manager.on_output(&record, generation, &buffer[..read]);
								},
							}
						}
					});
					let exit_manager = Arc::downgrade(self);
					let exit_record = Arc::downgrade(record);
					std::thread::spawn(move || {
						let mut child = child;
						let (code, reason) = match child.wait() {
							Ok(status) => (
								Some(status.exit_code() as i32),
								status.signal().map(|signal| format!("signal {signal}")),
							),
							Err(err) => (None, Some(format!("wait failed: {err}"))),
						};
						if let Some(manager) = exit_manager.upgrade()
							&& let Some(record) = exit_record.upgrade()
						{
							manager.settle(&record, generation, code, reason, &handle);
						}
					});
				},
				Err(message) => self.fail_launch(record, generation, message, &handle),
			}
		} else {
			match spawn_pipe(&spec) {
				Ok(spawned) => {
					let SpawnedPipe { pid, input: stdin, stdout, stderr, child } = spawned;
					let pinned = pid.and_then(|pid| Process::from_pid(pid as i32));
					let identity = pinned.as_ref().map(StartIdentity::of);
					let keep = {
						let mut state = lock(&record.state);
						let cancelled = state.generation != generation
							|| is_terminal_state(state.snapshot.state)
							|| state.snapshot.state == ProcessState::Stopping
							|| state.stop_requested;
						if cancelled {
							false
						} else {
							state.snapshot.pid = pid;
							state.snapshot.start_identity = identity;
							state.input =
								Some(Arc::new(tokio::sync::Mutex::new(Input::Pipe(stdin))));
							state.pinned.clone_from(&pinned);
							state.group_id = pinned.as_ref().and_then(|process| process.group_id());
							true
						}
					};
					if keep {
						self.publish(record, ProcessEventKind::Spawned);
					}
					record.notify_change();
					if !keep {
						if let Some(pinned) = pinned {
							let _ = pinned.kill_tree(Some(KILL_SIGNAL));
						}
						let mut child = child;
						let _ = child.start_kill();
						tokio::spawn(async move {
							let _ = child.wait().await;
						});
						return;
					}
					launched = true;
					let manager = Arc::downgrade(self);
					let read_record = Arc::downgrade(record);
					drain_stream(&manager, &read_record, generation, stdout);
					let manager = Arc::downgrade(self);
					let read_record = Arc::downgrade(record);
					drain_stream(&manager, &read_record, generation, stderr);
					let exit_manager = Arc::downgrade(self);
					let exit_record = Arc::downgrade(record);
					tokio::spawn(async move {
						let mut child = child;
						let (code, reason) = match child.wait().await {
							Ok(status) => (status.code(), status_reason(&status)),
							Err(err) => (None, Some(format!("wait failed: {err}"))),
						};
						if let Some(manager) = exit_manager.upgrade()
							&& let Some(record) = exit_record.upgrade()
						{
							manager.settle(&record, generation, code, reason, &handle);
						}
					});
				},
				Err(message) => self.fail_launch(record, generation, message, &handle),
			}
		}

		if launched
			&& let Some(ready) = spec.ready.as_ref()
			&& ready.port.is_some()
		{
			let poll_record = Arc::downgrade(record);
			let poll_manager = Arc::downgrade(self);
			let ready = ready.clone();
			tokio::spawn(async move {
				poll_port(&poll_manager, &poll_record, generation, &ready).await;
			});
		}
	}

	fn fail_launch(
		self: &Arc<Self>,
		record: &Arc<Record>,
		generation: u64,
		message: String,
		handle: &tokio::runtime::Handle,
	) {
		record.log.append(&format!("Daemon launch failed: {message}\n"));
		self.settle(record, generation, None, Some(message), handle);
	}

	fn monitor_detached(self: &Arc<Self>, record: &Arc<Record>, generation: u64) {
		let Ok(handle) = tokio::runtime::Handle::try_current() else {
			return;
		};
		let manager = Arc::downgrade(self);
		let watched = Arc::downgrade(record);
		handle.spawn(async move {
			loop {
				tokio::time::sleep(Duration::from_millis(100)).await;
				let (Some(manager), Some(watched)) = (manager.upgrade(), watched.upgrade()) else {
					return;
				};
				{
					let state = lock(&watched.state);
					if state.generation != generation || is_terminal_state(state.snapshot.state) {
						return;
					}
					if !state.spec.detached {
						return;
					}
				}
				let handle = tokio::runtime::Handle::current();
				if manager.refresh_record(&watched, &handle).is_err() {
					return;
				}
			}
		});
	}

	/// Adopt new detached output and settle the record when its pinned kernel
	/// process is gone (identity check, never a bare pid). Detached stdio
	/// bypasses the manager, so the file size *is* the output cursor, and the
	/// physical file is trimmed to keep direct-writer output bounded.
	fn refresh_record(
		self: &Arc<Self>,
		record: &Arc<Record>,
		handle: &tokio::runtime::Handle,
	) -> Result<ProcessSnapshot, ProcessError> {
		let (generation, detached, offset, terminal, identity) = {
			let state = lock(&record.state);
			(
				state.generation,
				state.spec.detached,
				state.snapshot.output_bytes,
				is_terminal_state(state.snapshot.state),
				state.snapshot.start_identity.clone(),
			)
		};
		if !detached || terminal {
			return Ok(lock(&record.state).snapshot.clone());
		}
		if !identity.as_ref().is_some_and(StartIdentity::still_matches) {
			self.settle(record, generation, None, None, handle);
			return Ok(lock(&record.state).snapshot.clone());
		}
		let (appended, size) = record.log.read_appended(offset);
		let mut became_ready = false;
		if size != offset {
			let mut state = lock(&record.state);
			if state.generation == generation && !is_terminal_state(state.snapshot.state) {
				state.snapshot.output_bytes = size;
				if !appended.is_empty() {
					let sanitized = sanitize_text(&appended);
					push_readiness(&mut state.readiness, &sanitized);
					let ready_match = state
						.ready_regex
						.as_ref()
						.and_then(|pattern| pattern.find(&state.readiness))
						.map(|found| found.as_str().chars().take(500).collect::<String>());
					if !state.log_ready
						&& let Some(found) = ready_match
					{
						state.log_ready = true;
						state.snapshot.ready_match = Some(found);
						sync_ready_pending(&mut state);
					}
					became_ready = mark_ready(&mut state);
				}
			}
			drop(state);
			if became_ready {
				self.publish(record, ProcessEventKind::Ready);
			}
			record.notify_change();
		}
		Ok(lock(&record.state).snapshot.clone())
	}

	/// Reattach a detached record persisted by a previous broker generation.
	///
	/// Ownership stays broker-internal: a runner manager has no cross-exit
	/// lifetime, so adopting there would promise supervision it cannot provide.
	pub fn adopt_detached(
		self: &Arc<Self>,
		spec: ProcessSpec,
		mut snapshot: ProcessSnapshot,
	) -> Result<ProcessSnapshot, ProcessError> {
		if !self.inner.broker_owned {
			return Err(ProcessError::Invalid(
				"detached adoption requires a broker-owned process manager".to_string(),
			));
		}
		if !valid_process_name(&spec.name) || spec.application.trim().is_empty() {
			return Err(ProcessError::Invalid(
				"adopt_detached requires a valid process name and application".to_string(),
			));
		}
		if !spec.detached {
			return Err(ProcessError::Invalid(
				"adopt_detached requires spec.detached".to_string(),
			));
		}
		let dir = self.inner.log_root.join(&spec.name);
		let log = Arc::new(
			LogFile::adopt(&dir)
				.map_err(|err| ProcessError::Failed(format!("cannot adopt process log: {err}")))?,
		);
		let pinned = snapshot.start_identity.as_ref().and_then(StartIdentity::pin);
		let trusted = pinned.is_some();
		let stop_requested = snapshot.state == ProcessState::Stopping;
		if snapshot.state == ProcessState::Restarting {
			snapshot.state = if spec.ready.is_some() {
				ProcessState::Starting
			} else {
				ProcessState::Running
			};
		}
		let alive = trusted && !is_terminal_state(snapshot.state) && !stop_requested;
		if !alive {
			if snapshot.state != ProcessState::Stopping {
				snapshot.state = ProcessState::Exited;
			}
			snapshot.pid = None;
			snapshot.start_identity = None;
			snapshot.exited_at.get_or_insert_with(now_ms);
			snapshot.exit_reason.get_or_insert_with(|| {
				"detached process exited while the broker was away".to_string()
			});
			snapshot.ready_pending = None;
		} else {
			snapshot.output_bytes = log.current_size();
		}
		snapshot.persist = spec.persist;
		snapshot.detached = spec.detached;
		let log_ready = spec.ready.as_ref().is_none_or(|ready| ready.log.is_none())
			|| !alive
			|| snapshot.ready_at.is_some();
		let port_ready = spec.ready.as_ref().is_none_or(|ready| ready.port.is_none())
			|| !alive
			|| snapshot.ready_at.is_some();
		let generation = 1;
		let adopted = snapshot.clone();
		let adopted_group = pinned.as_ref().and_then(|process| process.group_id());
		let mut state = RecordState {
			spec: spec.clone(),
			snapshot,
			generation,
			stop_requested,
			log_ready,
			port_ready,
			readiness: String::new(),
			ready_regex: ready_regex(&spec),
			consecutive_failures: 0,
			restart_task: None,
			input: None,
			killer: None,
			pinned,
			group_id: adopted_group,
			_master: None,
			pending_exit_reason: None,
		};
		sync_ready_pending(&mut state);
		let record = Arc::new(Record {
			state: Mutex::new(state),
			changed: Notify::new(),
			log,
			lifecycle: tokio::sync::Mutex::new(()),
		});
		{
			let mut records = lock(&self.inner.records);
			if records.contains_key(&spec.name) {
				return Err(ProcessError::Invalid(format!(
					"Process \"{}\" is already registered; cannot adopt it twice",
					spec.name
				)));
			}
			records.insert(spec.name.clone(), Arc::clone(&record));
		}
		record.notify_change();
		self.publish(&record, ProcessEventKind::Adopted);
		if alive {
			self.monitor_detached(&record, generation);
			if let Some(ready) = spec.ready.as_ref()
				&& ready.port.is_some()
				&& !port_ready
				&& let Ok(handle) = tokio::runtime::Handle::try_current()
			{
				let poll_record = Arc::downgrade(&record);
				let poll_manager = Arc::downgrade(self);
				let ready = ready.clone();
				handle.spawn(async move {
					poll_port(&poll_manager, &poll_record, generation, &ready).await;
				});
			}
		}
		Ok(adopted)
	}

	/// Public refresh hook for the broker: settle dead detached records and
	/// advance their output cursor before serving `list`/`logs`/`describe`.
	pub fn refresh_detached(self: &Arc<Self>, name: &str) -> Result<ProcessSnapshot, ProcessError> {
		let record = self.record(name, None)?;
		let handle = tokio::runtime::Handle::try_current().map_err(|_| {
			ProcessError::Failed("refresh_detached requires an async runtime".to_string())
		})?;
		self.refresh_record(&record, &handle)
	}


	fn on_output(&self, record: &Arc<Record>, generation: u64, chunk: &[u8]) {
		let raw = String::from_utf8_lossy(chunk).into_owned();
		let mut state = lock(&record.state);
		if state.generation != generation {
			return;
		}
		let appended = record.log.append(&raw);
		state.snapshot.output_bytes = state.snapshot.output_bytes.saturating_add(appended);
		let sanitized = sanitize_text(&raw);
		push_readiness(&mut state.readiness, &sanitized);
		if !state.log_ready {
			let ready_match = state
				.ready_regex
				.as_ref()
				.and_then(|pattern| pattern.find(&state.readiness))
				.map(|found| found.as_str().chars().take(500).collect::<String>());
			if let Some(found) = ready_match {
				state.log_ready = true;
				state.snapshot.ready_match = Some(found);
				sync_ready_pending(&mut state);
			}
		}
		let became_ready = mark_ready(&mut state);
		drop(state);
		if became_ready {
			self.publish(record, ProcessEventKind::Ready);
		}
		record.notify_change();
	}

	/// Settle one generation exactly once and arm a restart when its policy says so.
	fn settle(
		self: &Arc<Self>,
		record: &Arc<Record>,
		generation: u64,
		exit_code: Option<i32>,
		exit_reason: Option<String>,
		handle: &tokio::runtime::Handle,
	) {
		let mut state = lock(&record.state);
		if state.generation != generation
			|| is_terminal_state(state.snapshot.state)
			|| state.snapshot.state == ProcessState::Restarting
		{
			return;
		}
		let imposed = state.pending_exit_reason.take();
		state.input = None;
		state.killer = None;
		state._master = None;
		// A daemonized descendant can outlive the root and would keep inherited
		// pipe readers (and this record) alive forever. Take the pinned handle
		// now and sweep whatever is left of the generation's process group.
		let leftover_group = if state.spec.detached {
			None
		} else {
			state.group_id.take()
		};
		state.pinned = None;
		state.snapshot.pid = None;
		state.snapshot.exited_at = Some(now_ms());
		state.snapshot.exit_code = exit_code;
		state.snapshot.exit_reason = imposed.or(exit_reason);
		state.snapshot.ready_pending = None;
		let failed = state.snapshot.exit_reason.is_some()
			|| exit_code.is_some_and(|code| code != 0);
		let should_restart = !state.stop_requested
			&& match state.spec.restart {
				RestartPolicy::No => false,
				RestartPolicy::OnFailure => failed,
				RestartPolicy::Always => true,
			};
		if should_restart && !self.inner.shutting_down.load(Ordering::SeqCst) {
			let uptime = now_ms().saturating_sub(state.snapshot.started_at);
			state.consecutive_failures = if uptime >= 30_000 {
				0
			} else {
				state.consecutive_failures.saturating_add(1)
			};
			state.snapshot.restart_count = state.snapshot.restart_count.saturating_add(1);
			state.snapshot.ready_at = None;
			state.snapshot.ready_match = None;
			state.snapshot.state = ProcessState::Restarting;
			let delay = restart_delay(state.consecutive_failures);
			let exit_note = match exit_code {
				Some(code) => format!(" with code {code}"),
				None => String::new(),
			};
			record
				.log
				.append(&format!("\n[daemon exited{exit_note}; restarting in {delay}ms]\n"));
			let name_record = Arc::clone(record);
			let manager = Arc::downgrade(self);
			let task = handle.spawn(async move {
				tokio::time::sleep(Duration::from_millis(delay)).await;
				let Some(manager) = manager.upgrade() else {
					return;
				};
				// Same gate as stop/restart/start: a start that terminal-rearms
				// this record must exclude the armed timer, and vice versa.
				let gate = name_record.lifecycle.lock().await;
				{
					let mut state = lock(&name_record.state);
					if state.snapshot.state != ProcessState::Restarting {
						return;
					}
					state.restart_task = None;
				}
				manager.launch(&name_record).await;
				drop(gate);
			});
			state.restart_task = Some(task);
			drop(state);
			if let Some(group_id) = leftover_group {
				kill_process_group(group_id, KILL_SIGNAL);
			}
			self.publish(record, ProcessEventKind::Restarting);
			record.notify_change();
			return;
		}
		state.snapshot.state = if failed && !state.stop_requested {
			ProcessState::Failed
		} else {
			ProcessState::Exited
		};
		drop(state);
		if let Some(group_id) = leftover_group {
			kill_process_group(group_id, KILL_SIGNAL);
		}
		self.publish(record, ProcessEventKind::Exited);
		record.notify_change();
	}

	#[must_use]
	pub fn list(&self, caller: Option<&str>) -> Vec<ProcessSnapshot> {
		let mut records = lock(&self.inner.records);
		self.prune(&mut records);
		let mut active = Vec::new();
		let mut terminal = Vec::new();
		for record in records.values() {
			let state = lock(&record.state);
			if !Self::record_visible(&state, caller) {
				continue;
			}
			let snapshot = state.snapshot.clone();
			if is_terminal_state(snapshot.state) {
				terminal.push(snapshot);
			} else {
				active.push(snapshot);
			}
		}
		active.sort_by_key(|snapshot| snapshot.created_at);
		terminal.sort_by(|left, right| {
			right
				.exited_at
				.unwrap_or(right.created_at)
				.cmp(&left.exited_at.unwrap_or(left.created_at))
		});
		terminal.truncate(MAX_TERMINAL_LISTED);
		active.extend(terminal);
		active
	}

	/// Drop settled records past retention/count and reclaim their log dirs.
	fn prune(&self, records: &mut BTreeMap<String, Arc<Record>>) {
		let now = now_ms();
		let mut terminal: Vec<(u64, u64, String)> = Vec::new();
		for (name, record) in records.iter() {
			let state = lock(&record.state);
			if is_terminal_state(state.snapshot.state)
				&& state.restart_task.is_none()
				&& state.snapshot.ready_pending.is_none()
			{
				let exited = state.snapshot.exited_at.unwrap_or(state.snapshot.created_at);
				terminal.push((exited, state.snapshot.created_at, name.clone()));
			}
		}
		terminal.sort_by_key(|(exited, created, _)| (*exited, *created));
		let mut remove: Vec<String> = terminal
			.iter()
			.filter(|(exited, _, _)| now.saturating_sub(*exited) > RECORD_RETENTION_MS)
			.map(|(_, _, name)| name.clone())
			.collect();
		let cap = terminal.len().saturating_sub(MAX_SETTLED_RECORDS);
		remove.extend(terminal.iter().take(cap).map(|(_, _, name)| name.clone()));
		for name in remove {
			if let Some(record) = records.remove(&name) {
				record.log.discard();
			}
		}
	}

	/// Tail/head/filter logs with cursor-based follow semantics.
	pub async fn logs(
		&self,
		name: &str,
		caller: Option<&str>,
		options: LogOptions,
	) -> Result<LogsOutcome, ProcessError> {
		let record = self.record(name, caller)?;
		let grep = match options.grep.as_deref() {
			Some(pattern) => Some(
				Regex::new(pattern)
					.map_err(|err| ProcessError::Invalid(format!("Invalid log regex: {err}")))?,
			),
			None => None,
		};
		let (cursor, terminal) = {
			let state = lock(&record.state);
			(
				options.cursor.unwrap_or(state.snapshot.output_bytes),
				is_terminal_state(state.snapshot.state),
			)
		};
		let mut timed_out = false;
		if options.follow && !terminal {
			timed_out = !wait_until(&record, options.timeout_ms, || {
				let state = lock(&record.state);
				state.snapshot.output_bytes > cursor || is_terminal_state(state.snapshot.state)
			})
			.await;
		}
		let (text, state_cursor, state) = {
			let state = lock(&record.state);
			let text = record.log.read(options.head, options.lines.max(1), grep.as_ref());
			(text, state.snapshot.output_bytes, state.snapshot.state)
		};
		Ok(LogsOutcome {
			name: name.to_string(),
			text,
			cursor: state_cursor,
			timed_out,
			state,
		})
	}

	/// Wait for exit, readiness, or an output pattern on one generation.
	pub async fn wait(
		&self,
		name: &str,
		caller: Option<&str>,
		request: WaitRequest,
		timeout_ms: u64,
	) -> Result<WaitOutcome, ProcessError> {
		let record = self.record(name, caller)?;
		let bound_generation = lock(&record.state).generation;
		let pattern = match request.pattern.as_deref() {
			Some(pattern) => Some(
				Regex::new(pattern)
					.map_err(|err| ProcessError::Invalid(format!("Invalid wait regex: {err}")))?,
			),
			None => None,
		};
		let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
		let mut matched = None;
		let mut woke = false;
		loop {
			let mut notified = Box::pin(record.changed.notified());
			notified.as_mut().enable();
			let (generation_ended, condition_met) = {
				let state = lock(&record.state);
				let generation_ended = state.generation != bound_generation
					|| state.snapshot.state == ProcessState::Restarting;
				let condition_met = if let Some(pattern) = pattern.as_ref() {
					if let Some(found) = pattern.find(&state.readiness) {
						matched = Some(found.as_str().chars().take(500).collect::<String>());
						true
					} else {
						false
					}
				} else if request.for_exit {
					is_terminal_state(state.snapshot.state)
				} else {
					ready_observed(&state) || is_terminal_state(state.snapshot.state)
				};
				(generation_ended, condition_met)
			};
			if condition_met {
				woke = true;
				break;
			}
			if generation_ended || tokio::time::Instant::now() >= deadline {
				break;
			}
			tokio::select! {
				_ = &mut notified => {},
				_ = tokio::time::sleep_until(deadline) => break,
			}
		}
		let (snapshot, generation_ended, ready_observed_now) = {
			let state = lock(&record.state);
			(
				state.snapshot.clone(),
				state.generation != bound_generation
					|| state.snapshot.state == ProcessState::Restarting,
				ready_observed(&state),
			)
		};
		if generation_ended {
			let exit = snapshot
				.exit_code
				.map(|code| format!(" with exit code {code}"))
				.unwrap_or_default();
			return Err(ProcessError::Failed(format!(
				"Process {name} generation {bound_generation} exited{exit}; the wait was rejected instead of continuing against a replacement generation"
			)));
		}
		// Readiness success is judged by the observed state, not the wake: a
		// pre-ready exit wakes the wait but is still "not ready".
		let timed_out = if request.for_exit || request.pattern.is_some() {
			!woke
		} else {
			!ready_observed_now
		};
		Ok(WaitOutcome { snapshot, matched, timed_out })
	}

	/// Write stdin and/or signal the process tree.
	pub async fn send(
		&self,
		name: &str,
		caller: Option<&str>,
		data: Option<&str>,
		signal: Option<Signal>,
	) -> Result<ProcessSnapshot, ProcessError> {
		let record = self.record(name, caller)?;
		if data.is_none() && signal.is_none() {
			return Err(ProcessError::Invalid("send requires data or signal".to_string()));
		}
		let input = {
			let state = lock(&record.state);
			Self::ensure_control(&state, caller)?;
			if is_terminal_state(state.snapshot.state) || state.snapshot.state == ProcessState::Stopping {
				return Err(ProcessError::Failed(format!(
					"Process {name} is {}",
					state_label(state.snapshot.state)
				)));
			}
			state.input.clone()
		};
		if let Some(data) = data {
			let Some(input) = input else {
				return Err(ProcessError::Failed(format!("Process {name} stdin is unavailable")));
			};
			let mut guard = input.lock().await;
			match &mut *guard {
				Input::Pipe(stdin) => {
					AsyncWriteExt::write_all(stdin, data.as_bytes())
						.await
						.map_err(|err| ProcessError::Failed(format!("stdin write failed: {err}")))?;
					AsyncWriteExt::flush(stdin)
						.await
						.map_err(|err| ProcessError::Failed(format!("stdin flush failed: {err}")))?;
				},
				Input::Blocking(writer) => {
					writer
						.write_all(data.as_bytes())
						.map_err(|err| ProcessError::Failed(format!("stdin write failed: {err}")))?;
					writer
						.flush()
						.map_err(|err| ProcessError::Failed(format!("stdin flush failed: {err}")))?;
				},
			}
		}
		if let Some(signal) = signal {
			// Re-read the pinned handle after the stdin await: the reused pid is
			// only trusted through its start-token identity.
			let pinned = lock(&record.state).pinned.clone();
			let Some(process) = pinned else {
				return Err(ProcessError::Failed(format!(
					"Process {name} process is unavailable"
				)));
			};
			if process.kill_tree(Some(signal.number())) == 0 {
				return Err(ProcessError::Failed(format!(
					"Process {name} process is unavailable"
				)));
			}
		}
		Ok(lock(&record.state).snapshot.clone())
	}

	/// Stop the generation and its whole tree within `timeout_ms` of grace.
	pub async fn stop(
		self: &Arc<Self>,
		name: &str,
		caller: Option<&str>,
		timeout_ms: u64,
	) -> Result<ProcessSnapshot, ProcessError> {
		let record = self.record(name, caller)?;
		let _gate = record.lifecycle.lock().await;
		{
			let state = lock(&record.state);
			Self::ensure_control(&state, caller)?;
		}
		self.terminate(&record, timeout_ms, None, true).await
	}

	/// Relaunch the same spec after a stop, rotating the log generation.
	pub async fn restart(
		self: &Arc<Self>,
		name: &str,
		caller: Option<&str>,
	) -> Result<ProcessSnapshot, ProcessError> {
		let record = self.record(name, caller)?;
		let _gate = record.lifecycle.lock().await;
		{
			let state = lock(&record.state);
			Self::ensure_control(&state, caller)?;
		}
		self.terminate(&record, 2_000, None, true).await?;
		{
			let mut state = lock(&record.state);
			if let Some(task) = state.restart_task.take() {
				task.abort();
			}
			state.stop_requested = false;
		}
		record
			.log
			.rotate()
			.map_err(|err| ProcessError::Failed(format!("cannot rotate process log: {err}")))?;
		self.launch(&record).await;
		Ok(lock(&record.state).snapshot.clone())
	}

	/// Internal termination path shared by stop, restart, and the async timeout.
	async fn terminate(
		&self,
		record: &Arc<Record>,
		timeout_ms: u64,
		reason: Option<&str>,
		requested: bool,
	) -> Result<ProcessSnapshot, ProcessError> {
		let (pinned, killer, group_id) = {
			let mut state = lock(&record.state);
			if is_terminal_state(state.snapshot.state) {
				return Ok(state.snapshot.clone());
			}
			if requested {
				state.stop_requested = true;
			}
			if let Some(task) = state.restart_task.take() {
				task.abort();
				state.snapshot.state = ProcessState::Exited;
				state.snapshot.exited_at = Some(now_ms());
				state.snapshot.ready_at = None;
				state.snapshot.ready_pending = None;
				state.snapshot.exit_reason = reason.map(str::to_string);
				let snapshot = state.snapshot.clone();
				drop(state);
				record.notify_change();
				return Ok(snapshot);
			}
			state.snapshot.state = ProcessState::Stopping;
			if let Some(reason) = reason {
				state.pending_exit_reason = Some(reason.to_string());
			}
			(
				state.pinned.clone(),
				state.killer.as_ref().map(|killer| killer.clone_killer()),
				state.group_id,
			)
		};
		record.notify_change();
		if let Some(process) = pinned {
			let grace = timeout_ms.min(i32::MAX as u64) as i32;
			let hard = timeout_ms.saturating_add(1000).min(u32::MAX as u64) as u32;
			let _ = process
				.terminate_tree(true, grace, hard, CancelToken::new(None))
				.await;
		} else if let Some(mut killer) = killer {
			let _ = killer.kill();
		}
		let settled = wait_until(record, timeout_ms.saturating_add(1000), || {
			is_terminal_state(lock(&record.state).snapshot.state)
		})
		.await;
		if !settled {
			if let Some(group_id) = group_id {
				kill_process_group(group_id, KILL_SIGNAL);
			}
			if let Some(mut killer) = lock(&record.state)
				.killer
				.as_ref()
				.map(|killer| killer.clone_killer())
			{
				let _ = killer.kill();
			}
			let _ = wait_until(record, 1_000, || {
				is_terminal_state(lock(&record.state).snapshot.state)
			})
			.await;
		}
		Ok(lock(&record.state).snapshot.clone())
	}

	/// Best-effort deadline for a backgrounded `bash async` call. Expiry stops
	/// the process tree and records `timed out` as the exit reason.
	pub fn arm_timeout(
		self: &Arc<Self>,
		name: &str,
		caller: Option<&str>,
		timeout_ms: u64,
	) {
		let Ok(record) = self.record(name, caller) else {
			return;
		};
		{
			let state = lock(&record.state);
			if Self::ensure_control(&state, caller).is_err() {
				return;
			}
		}
		let generation = lock(&record.state).generation;
		let manager = Arc::downgrade(self);
		tokio::spawn(async move {
			tokio::time::sleep(Duration::from_millis(timeout_ms)).await;
			let Some(manager) = manager.upgrade() else {
				return;
			};
			let (current_generation, state) = {
				let state = lock(&record.state);
				(state.generation, state.snapshot.state)
			};
			if current_generation != generation
				|| is_terminal_state(state)
				|| state == ProcessState::Stopping
			{
				return;
			}
			let _ = manager
				.terminate(&record, 1_000, Some("timed out"), false)
				.await;
		});
	}

	/// Snapshot plus the immutable launch spec (environment excluded).
	pub fn describe(
		&self,
		name: &str,
		caller: Option<&str>,
	) -> Result<(ProcessSnapshot, ProcessSpec), ProcessError> {
		let record = self.record(name, caller)?;
		let state = lock(&record.state);
		Ok((state.snapshot.clone(), state.spec.clone()))
	}
}

#[async_trait]
impl ProcessBackend for ProcessManager {
	async fn start(
		self: Arc<Self>,
		spec: ProcessSpec,
		owner: Option<String>,
	) -> Result<StartOutcome, ProcessError> {
		ProcessManager::start(&self, spec, owner).await
	}

	async fn list(self: Arc<Self>, owner: Option<String>) -> Result<Vec<ProcessSnapshot>, ProcessError> {
		Ok(ProcessManager::list(&self, owner.as_deref()))
	}

	async fn logs(
		self: Arc<Self>,
		name: String,
		options: LogOptions,
		owner: Option<String>,
	) -> Result<LogsOutcome, ProcessError> {
		ProcessManager::logs(&self, &name, owner.as_deref(), options).await
	}

	async fn wait(
		self: Arc<Self>,
		name: String,
		request: WaitRequest,
		timeout_ms: u64,
		owner: Option<String>,
	) -> Result<WaitOutcome, ProcessError> {
		ProcessManager::wait(&self, &name, owner.as_deref(), request, timeout_ms).await
	}

	async fn send(
		self: Arc<Self>,
		name: String,
		data: Option<String>,
		signal: Option<Signal>,
		owner: Option<String>,
	) -> Result<ProcessSnapshot, ProcessError> {
		ProcessManager::send(&self, &name, owner.as_deref(), data.as_deref(), signal).await
	}

	async fn stop(
		self: Arc<Self>,
		name: String,
		timeout_ms: u64,
		owner: Option<String>,
	) -> Result<ProcessSnapshot, ProcessError> {
		ProcessManager::stop(&self, &name, owner.as_deref(), timeout_ms).await
	}

	async fn restart(
		self: Arc<Self>,
		name: String,
		owner: Option<String>,
	) -> Result<ProcessSnapshot, ProcessError> {
		ProcessManager::restart(&self, &name, owner.as_deref()).await
	}

	async fn describe(
		self: Arc<Self>,
		name: String,
		owner: Option<String>,
	) -> Result<(ProcessSnapshot, ProcessSpec), ProcessError> {
		ProcessManager::describe(&self, &name, owner.as_deref())
	}

	fn arm_timeout(self: Arc<Self>, name: String, owner: Option<String>, timeout_ms: u64) {
		ProcessManager::arm_timeout(&self, &name, owner.as_deref(), timeout_ms);
	}

	fn next_background_name(&self) -> String {
		ProcessManager::next_background_name(self)
	}

	fn async_enabled(&self) -> bool {
		ProcessManager::async_enabled(self)
	}

	fn set_async_enabled(&self, enabled: bool) {
		ProcessManager::set_async_enabled(self, enabled);
	}
}

impl Drop for Inner {
	fn drop(&mut self) {
		self.shutting_down.store(true, Ordering::SeqCst);
		let records: Vec<Arc<Record>> = lock(&self.records).values().cloned().collect();
		for record in records {
			let (task, pinned, mut killer, group_id) = {
				let mut state = lock(&record.state);
				let task = state.restart_task.take();
				let keep = self.broker_owned
					&& state.snapshot.detached
					&& !is_terminal_state(state.snapshot.state);
				state.input = None;
				if keep {
					// Broker lifecycle contract: persistent/detached children
					// outlive this manager (and its process) with stdio still on
					// their log files; only the runner manager kills everything.
					(task, None, None, None)
				} else {
					let pinned = state.pinned.take();
					let group_id = state.group_id.take();
					let killer = state.killer.as_ref().map(|killer| killer.clone_killer());
					state.snapshot.pid = None;
					(task, pinned, killer, group_id)
				}
			};
			if let Some(task) = task {
				task.abort();
			}
			if let Some(group_id) = group_id {
				kill_process_group(group_id, KILL_SIGNAL);
			}
			if let Some(process) = pinned {
				let _ = process.kill_tree(Some(KILL_SIGNAL));
			} else if let Some(killer) = killer.as_mut() {
				let _ = killer.kill();
			}
		}
		// Logs are disposable runner state; remove the per-runner tree so a
		// long-lived machine does not accumulate orphaned daemon output. A
		// broker-owned manager retains it for reconnect/replay.
		if !self.broker_owned && !self.log_root.as_os_str().is_empty() {
			let _ = std::fs::remove_dir_all(&self.log_root);
		}
	}
}

fn restart_delay(failures: u32) -> u64 {
	let exponent = failures.min(5);
	(1_000u64 << exponent).min(30_000)
}

fn ready_observed(state: &RecordState) -> bool {
	state.snapshot.ready_at.is_some()
		|| state.snapshot.state == ProcessState::Ready
		|| (state.snapshot.state == ProcessState::Running && state.spec.ready.is_none())
}

#[cfg(unix)]
fn status_reason(status: &std::process::ExitStatus) -> Option<String> {
	use std::os::unix::process::ExitStatusExt;
	status.signal().map(|signal| format!("signal {signal}"))
}

#[cfg(not(unix))]
fn status_reason(_status: &std::process::ExitStatus) -> Option<String> {
	None
}

fn drain_stream<R>(
	manager: &Weak<ProcessManager>,
	record: &Weak<Record>,
	generation: u64,
	mut stream: R,
) where
	R: AsyncRead + Unpin + Send + 'static,
{
	let manager = manager.clone();
	let record = record.clone();
	tokio::spawn(async move {
		let mut buffer = [0u8; 8192];
		loop {
			match stream.read(&mut buffer).await {
				Ok(0) | Err(_) => break,
				Ok(read) => {
					let (Some(manager), Some(record)) = (manager.upgrade(), record.upgrade()) else {
						break;
					};
					let stale = lock(&record.state).generation != generation;
					if stale {
						break;
					}
					manager.on_output(&record, generation, &buffer[..read]);
				},
			}
		}
	});
}

async fn poll_port(
	manager: &Weak<ProcessManager>,
	record: &Weak<Record>,
	generation: u64,
	ready: &ReadySpec,
) {
	let Some(port) = ready.port else {
		return;
	};
	let host = ready.host.clone().unwrap_or_else(|| "127.0.0.1".to_string());
	loop {
		let Some(record) = record.upgrade() else {
			return;
		};
		{
			let state = lock(&record.state);
			if state.generation != generation
				|| is_terminal_state(state.snapshot.state)
				|| state.port_ready
			{
				return;
			}
		}
		let connected = tokio::time::timeout(
			Duration::from_millis(250),
			tokio::net::TcpStream::connect((host.as_str(), port)),
		)
		.await
		.is_ok_and(|result| result.is_ok());
		if connected {
			let mut became_ready = false;
			{
				let mut state = lock(&record.state);
				if state.generation == generation && !is_terminal_state(state.snapshot.state) {
					state.port_ready = true;
					sync_ready_pending(&mut state);
					became_ready = mark_ready(&mut state);
				}
			}
			if became_ready && let Some(manager) = manager.upgrade() {
				manager.publish(&record, ProcessEventKind::Ready);
			}
			record.notify_change();
			return;
		}
		tokio::time::sleep(Duration::from_millis(100)).await;
	}
}

async fn wait_until<F: Fn() -> bool>(record: &Arc<Record>, timeout_ms: u64, condition: F) -> bool {
	let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
	loop {
		let mut notified = Box::pin(record.changed.notified());
		notified.as_mut().enable();
		if condition() {
			return true;
		}
		if tokio::time::Instant::now() >= deadline {
			return condition();
		}
		tokio::select! {
			_ = &mut notified => {},
			_ = tokio::time::sleep_until(deadline) => return condition(),
		}
	}
}

struct SpawnedPty {
	pid: Option<u32>,
	input: Box<dyn Write + Send>,
	killer: Box<dyn ChildKiller + Send + Sync>,
	master: Box<dyn MasterPty + Send>,
	reader: Box<dyn Read + Send>,
	child: Box<dyn portable_pty::Child + Send + Sync>,
}

/// Open the PTY and spawn the child synchronously: the caller can only be
/// cancelled at an await point, so a cancelled launch can never leave a
/// half-registered PTY child. Any setup failure after `spawn_command` kills and
/// reaps the child before returning.
fn spawn_pty(spec: &ProcessSpec) -> Result<SpawnedPty, String> {
	let system = native_pty_system();
	let pair = system
		.openpty(PtySize {
			rows: DAEMON_PTY_ROWS,
			cols: DAEMON_PTY_COLUMNS,
			pixel_width: 0,
			pixel_height: 0,
		})
		.map_err(|err| format!("cannot open PTY: {err}"))?;
	let mut builder = CommandBuilder::new(&spec.application);
	builder.args(&spec.args);
	builder.cwd(&spec.cwd);
	builder.env_clear();
	for (key, value) in &spec.env {
		builder.env(key, value);
	}
	let mut child = pair
		.slave
		.spawn_command(builder)
		.map_err(|err| format!("cannot spawn PTY process: {err}"))?;
	drop(pair.slave);
	let setup = (|| -> Result<(Box<dyn Read + Send>, Box<dyn Write + Send>), String> {
		let reader = pair
			.master
			.try_clone_reader()
			.map_err(|err| format!("cannot read PTY: {err}"))?;
		let input = pair
			.master
			.take_writer()
			.map_err(|err| format!("cannot write PTY: {err}"))?;
		Ok((reader, input))
	})();
	let (reader, input) = match setup {
		Ok(parts) => parts,
		Err(message) => {
			let _ = child.kill();
			let _ = child.wait();
			return Err(message);
		},
	};
	let pid = child.process_id();
	let killer = child.clone_killer();
	Ok(SpawnedPty { pid, input, killer, master: pair.master, reader, child })
}

struct SpawnedDetached {
	pid: Option<u32>,
	child: tokio::process::Child,
}

/// Spawn a detached process in a fresh session so broker exit cannot deliver
/// SIGHUP. Stdio appends directly to `output.log` (no pipe/reader), matching
/// the TypeScript broker: detached logs are explicitly unbounded on disk.
fn spawn_detached(spec: &ProcessSpec, log_path: &Path) -> Result<SpawnedDetached, String> {
	let stdout = open_append_private(log_path)
		.map_err(|err| format!("cannot open detached log: {err}"))?;
	let stderr = stdout
		.try_clone()
		.map_err(|err| format!("cannot share detached log: {err}"))?;
	let mut command = Command::new(&spec.application);
	command
		.args(&spec.args)
		.current_dir(&spec.cwd)
		.env_clear()
		.envs(&spec.env)
		.stdin(Stdio::null())
		.stdout(Stdio::from(stdout))
		.stderr(Stdio::from(stderr))
		.kill_on_drop(false);
	#[cfg(unix)]
	{
		// SAFETY: `setsid` is async-signal-safe and needs no arguments; it returns
		// -1 with errno set on failure, which is surfaced as a spawn error before
		// exec. No other work happens between fork and exec.
		unsafe {
			command.pre_exec(|| {
				if setsid() < 0 {
					return Err(std::io::Error::last_os_error());
				}
				Ok(())
			});
		}
	}
	let child = command
		.spawn()
		.map_err(|err| format!("cannot spawn detached process: {err}"))?;
	let pid = child.id();
	Ok(SpawnedDetached { pid, child })
}

#[cfg(unix)]
unsafe extern "C" {
	/// `setsid(2)` from the platform C library; no `libc` crate wiring needed.
	fn setsid() -> i32;
	/// `kill(2)`; used only with a negative pid to sweep a process group.
	fn kill(pid: i32, signal: i32) -> i32;
}

/// Sweep the remaining members of a generation's process group. The root pid's
/// group id stays reserved while any member lives, so this cannot hit an
/// unrelated recycled pid; `ESRCH` (empty group) is the normal case.
fn kill_process_group(group_id: i32, signal: i32) {
	if group_id <= 1 {
		return;
	}
	#[cfg(unix)]
	{
		// SAFETY: `kill(2)` is async-signal-safe and has no memory effects; a
		// negative pid targets exactly that process group.
		unsafe {
			let _ = kill(-group_id, signal);
		}
	}
	#[cfg(not(unix))]
	{
		let _ = (group_id, signal);
	}
}

struct SpawnedPipe {
	pid: Option<u32>,
	input: tokio::process::ChildStdin,
	stdout: ChildStdout,
	stderr: ChildStderr,
	child: tokio::process::Child,
}

fn spawn_pipe(spec: &ProcessSpec) -> Result<SpawnedPipe, String> {
	let mut command = Command::new(&spec.application);
	command
		.args(&spec.args)
		.current_dir(&spec.cwd)
		.env_clear()
		.envs(&spec.env)
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.kill_on_drop(true);
	#[cfg(unix)]
	command.process_group(0);
	let mut child = command
		.spawn()
		.map_err(|err| format!("cannot spawn process: {err}"))?;
	let pid = child.id();
	let input = child.stdin.take().ok_or_else(|| "stdin pipe unavailable".to_string())?;
	let stdout = child
		.stdout
		.take()
		.ok_or_else(|| "stdout pipe unavailable".to_string())?;
	let stderr = child
		.stderr
		.take()
		.ok_or_else(|| "stderr pipe unavailable".to_string())?;
	Ok(SpawnedPipe { pid, input, stdout, stderr, child })
}

#[cfg(test)]
mod tests {
	use super::*;

	struct TempRoot(PathBuf);

	impl TempRoot {
		fn new(label: &str) -> Self {
			static NEXT: AtomicU64 = AtomicU64::new(0);
			let path = std::env::temp_dir().join(format!(
				"omp-process-unit-{label}-{}-{}",
				std::process::id(),
				NEXT.fetch_add(1, Ordering::SeqCst)
			));
			std::fs::create_dir_all(&path).expect("temp root");
			Self(path)
		}
	}

	impl Drop for TempRoot {
		fn drop(&mut self) {
			let _ = std::fs::remove_dir_all(&self.0);
		}
	}

	fn spec(name: &str, command: &str) -> ProcessSpec {
		ProcessSpec {
			name: name.to_string(),
			application: "sh".to_string(),
			args: vec!["-c".to_string(), command.to_string()],
			env: std::env::vars().collect(),
			cwd: std::env::temp_dir().to_string_lossy().into_owned(),
			pty: false,
			ready: None,
			restart: RestartPolicy::No,
			persist: false,
			detached: false,
		}
	}

	#[test]
	fn log_file_rotates_and_stays_bounded() {
		let root = TempRoot::new("log");
		let log = LogFile::open(&root.0.join("app")).expect("log");
		let chunk = "x".repeat(1024 * 1024);
		for _ in 0..27 {
			log.append(&chunk);
		}
		let current = std::fs::metadata(root.0.join("app").join(LOG_FILE)).unwrap().len();
		let previous = std::fs::metadata(root.0.join("app").join(PREVIOUS_LOG_FILE)).unwrap().len();
		assert!(current <= MAX_LOG_BYTES, "current={current}");
		assert!(previous <= MAX_LOG_BYTES, "previous={previous}");
		assert!(current > 0 && previous > 0);
	}

	#[test]
	fn truncate_lines_keeps_head_and_tail() {
		let text = "a\nb\nc\nd\ne\n";
		assert_eq!(truncate_lines(text, 2, 1024, true), "a\nb");
		assert_eq!(truncate_lines(text, 2, 1024, false), "e\n");
		assert_eq!(truncate_lines(text, 10, 1024, false), text);
	}

	#[test]
	fn validation_names_cwd_and_persist_prerequisite() {
		let mut bad = spec("web", "true");
		bad.name = "-bad".to_string();
		assert!(matches!(validate_spec(&bad, false), Err(ProcessError::Invalid(_))));
		let mut missing = spec("web", "true");
		missing.cwd = "/definitely/not/here".to_string();
		assert!(validate_spec(&missing, false).is_err());
		let mut persist = spec("web", "true");
		persist.persist = true;
		let error = validate_spec(&persist, false).expect_err("persist must be rejected");
		assert!(error.to_string().contains("broker"));
		assert!(validate_spec(&persist, true).is_ok(), "broker mode accepts persist");
		let mut detached = spec("web", "true");
		detached.detached = true;
		assert!(validate_spec(&detached, false).is_err());
		assert!(validate_spec(&detached, true).is_ok(), "broker mode accepts detached");
		let mut bad_ready = spec("web", "true");
		bad_ready.ready = Some(ReadySpec {
			log: None,
			port: None,
			host: None,
			timeout_ms: 30_000,
		});
		assert!(validate_spec(&bad_ready, false).is_err());
	}

	#[tokio::test]
	async fn broker_mode_persist_survives_shutdown() {
		let root = TempRoot::new("broker");
		let manager = Arc::new(ProcessManager::new_keepalive(root.0.clone()));
		let mut start = spec("keeper", "sleep 30");
		start.persist = true;
		start.detached = true;
		let outcome = ProcessManager::start(&manager, start, None).await.expect("broker start");
		assert!(outcome.snapshot.persist);
		assert!(outcome.snapshot.detached);
		let pid = outcome.snapshot.pid.expect("pid");
		manager.shutdown();
		assert!(
			Process::from_pid(pid as i32)
				.is_some_and(|process| process.status() == pi_shell::process::ProcessStatus::Running),
			"persistent process must outlive broker shutdown"
		);
		if let Some(process) = Process::from_pid(pid as i32) {
			let _ = process.kill_tree(Some(KILL_SIGNAL));
		}
	}

	#[tokio::test]
	async fn lifecycle_ready_logs_cursor_and_stop() {
		let root = TempRoot::new("lifecycle");
		let manager = Arc::new(ProcessManager::new(root.0.clone()));
		let mut start = spec("web", "echo ready; sleep 1; echo later; sleep 30");
		start.ready = Some(ReadySpec {
			log: Some("ready".to_string()),
			port: None,
			host: None,
			timeout_ms: 5_000,
		});
		let outcome = ProcessManager::start(&manager, start, None).await.expect("start");
		assert!(!outcome.ready_timed_out);
		assert_eq!(outcome.snapshot.state, ProcessState::Ready);
		assert!(outcome.snapshot.ready_match.as_deref() == Some("ready"));
		assert!(outcome.snapshot.pid.is_some());

		let first = ProcessManager::logs(&manager, 
				"web",
				None,
				LogOptions {
					lines: 10,
					head: false,
					grep: None,
					follow: false,
					cursor: None,
					timeout_ms: 1_000,
				},
			)
			.await
			.expect("logs");
		assert!(first.text.contains("ready"), "text={:?}", first.text);
		assert!(first.cursor > 0);

		let followed = ProcessManager::logs(&manager, 
				"web",
				None,
				LogOptions {
					lines: 10,
					head: false,
					grep: None,
					follow: true,
					cursor: Some(first.cursor),
					timeout_ms: 4_000,
				},
			)
			.await
			.expect("follow");
		assert!(!followed.timed_out, "follow timed out");
		assert!(followed.text.contains("later"), "text={:?}", followed.text);

		let (snapshot, described) = ProcessManager::describe(&manager, "web", None).expect("describe");
		assert_eq!(described.application, "sh");
		assert!(snapshot.owner.is_none());
		assert!(ProcessManager::list(&manager, None).iter().any(|item| item.name == "web"));

		let stopped = ProcessManager::stop(&manager, "web", None, 2_000).await.expect("stop");
		assert!(is_terminal_state(stopped.state));
		let again = ProcessManager::stop(&manager, "web", None, 500).await.expect("idempotent stop");
		assert!(is_terminal_state(again.state));
	}

	#[tokio::test]
	async fn pty_process_reaches_ready_and_stops() {
		let root = TempRoot::new("pty");
		let manager = Arc::new(ProcessManager::new(root.0.clone()));
		let mut start = spec("tty", "echo ready; sleep 30");
		start.pty = true;
		start.ready = Some(ReadySpec {
			log: Some("ready".to_string()),
			port: None,
			host: None,
			timeout_ms: 5_000,
		});
		let outcome = ProcessManager::start(&manager, start, None).await.expect("pty start");
		assert!(!outcome.ready_timed_out, "pty readiness timed out");
		assert_eq!(outcome.snapshot.state, ProcessState::Ready);
		let logs = ProcessManager::logs(&manager, 
				"tty",
				None,
				LogOptions {
					lines: 10,
					head: false,
					grep: None,
					follow: false,
					cursor: None,
					timeout_ms: 500,
				},
			)
			.await
			.expect("pty logs");
		assert!(logs.text.contains("ready"), "text={:?}", logs.text);
		assert!(!logs.text.contains('\r'), "CR should be sanitized");
		ProcessManager::send(&manager, "tty", None, Some("x\n"), None).await.expect("pty send");
		let stopped = ProcessManager::stop(&manager, "tty", None, 2_000).await.expect("pty stop");
		assert!(is_terminal_state(stopped.state));
	}

	#[tokio::test]
	async fn pipe_stdin_signal_and_wait() {
		let root = TempRoot::new("pipe");
		let manager = Arc::new(ProcessManager::new(root.0.clone()));
		ProcessManager::start(&manager, spec("cat", "cat"), None).await.expect("start");
		ProcessManager::send(&manager, "cat", None, Some("hello\n"), None)
			.await
			.expect("send");
		let logs = ProcessManager::logs(&manager, 
				"cat",
				None,
				LogOptions {
					lines: 20,
					head: false,
					grep: None,
					follow: true,
					cursor: Some(0),
					timeout_ms: 3_000,
				},
			)
			.await
			.expect("logs");
		assert!(!logs.timed_out);
		assert!(logs.text.contains("hello"), "text={:?}", logs.text);

		let _signaled = ProcessManager::send(&manager, "cat", None, None, Some(Signal::Term))
			.await
			.expect("signal");
		let waited = ProcessManager::wait(&manager, 
				"cat",
				None,
				WaitRequest { for_exit: true, pattern: None },
				5_000,
			)
			.await
			.expect("wait");
		assert!(is_terminal_state(waited.snapshot.state));
		assert!(!waited.timed_out);
	}

	#[tokio::test]
	async fn stop_kills_descendants_and_does_not_reuse_pid() {
		let root = TempRoot::new("tree");
		let manager = Arc::new(ProcessManager::new(root.0.clone()));
		ProcessManager::start(&manager, spec("tree", "sleep 60 & echo $!; wait"), None)
			.await
			.expect("start");
		let logs = ProcessManager::logs(&manager, 
				"tree",
				None,
				LogOptions {
					lines: 10,
					head: false,
					grep: None,
					follow: true,
					cursor: Some(0),
					timeout_ms: 3_000,
				},
			)
			.await
			.expect("logs");
		let child_pid: i32 = logs
			.text
			.lines()
			.find_map(|line| line.trim().parse().ok())
			.expect("child pid");
		ProcessManager::stop(&manager, "tree", None, 2_000).await.expect("stop");
		let mut gone = false;
		for _ in 0..50 {
			let alive = Process::from_pid(child_pid)
				.is_some_and(|process| process.status() == pi_shell::process::ProcessStatus::Running);
			if !alive {
				gone = true;
				break;
			}
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
		assert!(gone, "descendant {child_pid} survived stop");
	}

	#[tokio::test]
	async fn restart_replaces_the_pid_and_resets_state() {
		let root = TempRoot::new("restart");
		let manager = Arc::new(ProcessManager::new(root.0.clone()));
		let started = ProcessManager::start(&manager, spec("sleeper", "sleep 30"), None).await.expect("start");
		let first_pid = started.snapshot.pid.expect("pid");
		let restarted = ProcessManager::restart(&manager, "sleeper", None).await.expect("restart");
		assert_ne!(restarted.pid, Some(first_pid));
		assert!(!is_terminal_state(restarted.state));
		ProcessManager::stop(&manager, "sleeper", None, 1_000).await.expect("stop");
	}

	#[tokio::test]
	async fn restart_policy_arms_a_backoff_and_stop_cancels_it() {
		let root = TempRoot::new("policy");
		let manager = Arc::new(ProcessManager::new(root.0.clone()));
		let mut start = spec("flaky", "exit 3");
		start.restart = RestartPolicy::OnFailure;
		ProcessManager::start(&manager, start, None).await.expect("start");
		tokio::time::sleep(Duration::from_millis(1_500)).await;
		let snapshot = ProcessManager::describe(&manager, "flaky", None).expect("describe").0;
		assert!(snapshot.restart_count >= 1, "restart_count={}", snapshot.restart_count);
		let stopped = ProcessManager::stop(&manager, "flaky", None, 500).await.expect("stop");
		assert!(is_terminal_state(stopped.state));
	}

	#[tokio::test]
	async fn follow_without_output_times_out_and_owner_scopes_control() {
		let root = TempRoot::new("follow");
		let manager = Arc::new(ProcessManager::new(root.0.clone()));
		ProcessManager::start(&manager, spec("quiet", "sleep 30"), Some("child-a".to_string()))
			.await
			.expect("start");
		let logs = ProcessManager::logs(&manager, 
				"quiet",
				Some("child-a"),
				LogOptions {
					lines: 10,
					head: false,
					grep: None,
					follow: true,
					cursor: None,
					timeout_ms: 300,
				},
			)
			.await
			.expect("logs");
		assert!(logs.timed_out);
		assert!(ProcessManager::logs(&manager, "quiet", Some("child-b"), LogOptions {
			lines: 10,
			head: false,
			grep: None,
			follow: false,
			cursor: None,
			timeout_ms: 100,
		}).await.is_err());
		assert!(ProcessManager::stop(&manager, "quiet", Some("child-b"), 500).await.is_err());
		ProcessManager::stop(&manager, "quiet", Some("child-a"), 1_000).await.expect("owner stop");
		ProcessManager::stop(&manager, "quiet", None, 1_000).await.expect("root stop");
	}

	#[tokio::test]
	async fn broker_registry_drop_keeps_persistent_children() {
		let root = TempRoot::new("brokerdrop");
		let manager = Arc::new(ProcessManager::new_keepalive(root.0.clone()));
		let mut start = spec("keeper2", "sleep 30");
		start.persist = true;
		start.detached = true;
		let outcome = ProcessManager::start(&manager, start, None).await.expect("broker start");
		let pid = outcome.snapshot.pid.expect("pid");
		drop(manager);
		assert!(
			Process::from_pid(pid as i32)
				.is_some_and(|process| process.status() == pi_shell::process::ProcessStatus::Running),
			"persistent process must outlive the broker manager"
		);
		if let Some(process) = Process::from_pid(pid as i32) {
			let _ = process.kill_tree(Some(KILL_SIGNAL));
		}
	}

	#[test]
	fn spec_serialization_never_leaks_the_environment() {
		let mut spec = spec("web", "true");
		spec.env.insert("SECRET_TOKEN".to_string(), "hunter2".to_string());
		let encoded = serde_json::to_string(&spec).expect("spec serializes");
		assert!(!encoded.contains("hunter2"), "env value leaked: {encoded}");
		assert!(!encoded.contains("SECRET_TOKEN"), "env key leaked: {encoded}");
		assert!(encoded.contains("\"application\":\"sh\""));
	}

	#[tokio::test]
	async fn port_readiness_times_out_with_pending_condition() {
		let root = TempRoot::new("port");
		let manager = Arc::new(ProcessManager::new(root.0.clone()));
		let mut start = spec("srv", "sleep 30");
		start.ready = Some(ReadySpec {
			log: None,
			port: Some(59_998),
			host: Some("127.0.0.1".to_string()),
			timeout_ms: 300,
		});
		let outcome = ProcessManager::start(&manager, start, None).await.expect("start");
		assert!(outcome.ready_timed_out);
		assert_eq!(outcome.snapshot.state, ProcessState::Starting);
		assert_eq!(
			outcome.snapshot.ready_pending.as_deref(),
			Some(&["port".to_string()][..])
		);
		ProcessManager::stop(&manager, "srv", None, 2_000).await.expect("stop");
	}

	#[tokio::test]
	async fn logs_head_and_grep_filter_the_log_file() {
		let root = TempRoot::new("grep");
		let manager = Arc::new(ProcessManager::new(root.0.clone()));
		let mut start = spec("w", "echo alpha; echo beta; echo gamma; sleep 30");
		start.ready = Some(ReadySpec {
			log: Some("gamma".to_string()),
			port: None,
			host: None,
			timeout_ms: 5_000,
		});
		ProcessManager::start(&manager, start, None).await.expect("start");

		let head = ProcessManager::logs(&manager, 
				"w",
				None,
				LogOptions {
					lines: 2,
					head: true,
					grep: None,
					follow: false,
					cursor: None,
					timeout_ms: 100,
				},
			)
			.await
			.expect("head logs");
		assert_eq!(head.text.trim_end(), "alpha\nbeta", "got: {:?}", head.text);

		let filtered = ProcessManager::logs(&manager, 
				"w",
				None,
				LogOptions {
					lines: 10,
					head: false,
					grep: Some("beta|gamma".to_string()),
					follow: false,
					cursor: None,
					timeout_ms: 100,
				},
			)
			.await
			.expect("grep logs");
		assert!(filtered.text.contains("beta"), "got: {:?}", filtered.text);
		assert!(filtered.text.contains("gamma"), "got: {:?}", filtered.text);
		assert!(!filtered.text.contains("alpha"), "got: {:?}", filtered.text);

		ProcessManager::stop(&manager, "w", None, 2_000).await.expect("stop");
	}
}
