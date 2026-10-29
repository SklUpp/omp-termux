//! DAP transport: adapter process/socket spawn, Content-Length framing,
//! request/response correlation, events, reverse requests, timeouts and
//! cancellation.
//!
//! Framing is decoded by the shared LSP/DAP `MessageFramer`; this module only
//! owns the DAP-specific write path, lifecycle and message routing.

use std::{
	collections::HashMap,
	path::{Path, PathBuf},
	process::Stdio,
	sync::{
		Arc, Mutex as StdMutex, RwLock as StdRwLock,
		atomic::{AtomicBool, AtomicI64, Ordering},
	},
	time::{Duration, Instant},
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt, DuplexStream},
	net::{
		TcpListener, TcpStream,
		tcp::{OwnedReadHalf, OwnedWriteHalf},
	},
	process::{Child, ChildStdin, ChildStdout, Command},
	sync::{Mutex as AsyncMutex, broadcast, oneshot, watch},
	time::timeout,
};
#[cfg(unix)]
use tokio::net::{
	UnixStream,
	unix::{OwnedReadHalf as UnixReadHalf, OwnedWriteHalf as UnixWriteHalf},
};
use tokio_util::sync::CancellationToken;

use crate::lsp::framing::{FramingError, MessageFramer};

use super::config::{ConnectMode, ResolvedAdapter, substitute_port};
use super::types::Capabilities;

pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_MESSAGE_TIMEOUT: Duration = Duration::from_secs(30);
const SOCKET_READY_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_STDERR_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, thiserror::Error)]
pub enum DapError {
	#[error("{0}")]
	Message(String),
	#[error("DAP request {command} timed out after {ms}ms")]
	RequestTimeout { command: String, ms: u128 },
	#[error("Operation aborted")]
	Aborted,
	#[error("DAP adapter {adapter} is not running")]
	NotRunning { adapter: String },
}

impl DapError {
	pub fn message(message: impl Into<String>) -> Self {
		Self::Message(message.into())
	}
}

pub type EventHandler = Arc<dyn Fn(Value) + Send + Sync>;
pub type EventPredicate = Arc<dyn Fn(&Value) -> bool + Send + Sync>;

#[async_trait]
pub trait ReverseRequestHandler: Send + Sync {
	async fn handle(&self, arguments: Value) -> Result<Value, DapError>;
}

struct Pending {
	command: String,
	tx: oneshot::Sender<Result<Value, String>>,
}

#[derive(Default)]
struct ClientState {
	next_seq: u64,
	pending: HashMap<u64, Pending>,
	capabilities: Option<Capabilities>,
}

#[derive(Default)]
struct EventHandlers {
	by_name: HashMap<String, Vec<EventHandler>>,
	any: Vec<EventHandler>,
}

#[derive(Clone)]
struct EventFrame {
	event: String,
	body: Value,
}

enum Reader {
	Stdin(ChildStdout),
	Tcp(OwnedReadHalf),
	#[cfg(unix)]
	Unix(UnixReadHalf),
	Duplex(DuplexStream),
}

impl Reader {
	async fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
		match self {
			Self::Stdin(stream) => stream.read(buffer).await,
			Self::Tcp(stream) => stream.read(buffer).await,
			#[cfg(unix)]
			Self::Unix(stream) => stream.read(buffer).await,
			Self::Duplex(stream) => stream.read(buffer).await,
		}
	}
}

enum Writer {
	Stdin(ChildStdin),
	Tcp(OwnedWriteHalf),
	#[cfg(unix)]
	Unix(UnixWriteHalf),
	Duplex(DuplexStream),
}

impl Writer {
	async fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
		match self {
			Self::Stdin(stream) => stream.write_all(bytes).await,
			Self::Tcp(stream) => stream.write_all(bytes).await,
			#[cfg(unix)]
			Self::Unix(stream) => stream.write_all(bytes).await,
			Self::Duplex(stream) => stream.write_all(bytes).await,
		}
	}

	async fn flush(&mut self) -> std::io::Result<()> {
		match self {
			Self::Stdin(stream) => stream.flush().await,
			Self::Tcp(stream) => stream.flush().await,
			#[cfg(unix)]
			Self::Unix(stream) => stream.flush().await,
			Self::Duplex(stream) => stream.flush().await,
		}
	}
}

struct Inner {
	adapter_name: String,
	state: StdMutex<ClientState>,
	events: broadcast::Sender<EventFrame>,
	closed_tx: watch::Sender<Option<String>>,
	handlers: StdMutex<EventHandlers>,
	reverse: StdRwLock<HashMap<String, Arc<dyn ReverseRequestHandler>>>,
	writer: AsyncMutex<Writer>,
	process: StdMutex<Option<Child>>,
	stderr: Arc<StdMutex<Vec<u8>>>,
	socket_path: StdMutex<Option<PathBuf>>,
	alive: AtomicBool,
	disposed: AtomicBool,
	last_activity: AtomicI64,
	port: Option<u16>,
}

impl Inner {
	fn fail(&self, message: impl Into<String>) {
		let message = message.into();
		self.alive.store(false, Ordering::SeqCst);
		let _ = self.closed_tx.send_replace(Some(message.clone()));
		let mut state = self.state.lock().unwrap_or_else(|poison| poison.into_inner());
		for (_, pending) in state.pending.drain() {
			let _ = pending.tx.send(Err(message.clone()));
		}
	}

	fn reject(&self, seq: u64, message: String) {
		let mut state = self.state.lock().unwrap_or_else(|poison| poison.into_inner());
		if let Some(pending) = state.pending.remove(&seq) {
			let _ = pending.tx.send(Err(message));
		}
	}
}

#[derive(Clone)]
pub struct DapClient {
	inner: Arc<Inner>,
}

impl DapClient {
	fn new(
		adapter_name: &str,
		reader: Reader,
		writer: Writer,
		process: Option<Child>,
		stderr: Arc<StdMutex<Vec<u8>>>,
		port: Option<u16>,
	) -> Self {
		let (events, _) = broadcast::channel(256);
		let (closed_tx, _) = watch::channel(None);
		let client = Self {
			inner: Arc::new(Inner {
				adapter_name: adapter_name.to_string(),
				state: StdMutex::new(ClientState::default()),
				events,
				closed_tx,
				handlers: StdMutex::new(EventHandlers::default()),
				reverse: StdRwLock::new(HashMap::new()),
				writer: AsyncMutex::new(writer),
				process: StdMutex::new(process),
				stderr,
				socket_path: StdMutex::new(None),
				alive: AtomicBool::new(true),
				disposed: AtomicBool::new(false),
				last_activity: AtomicI64::new(now_millis()),
				port,
			}),
		};
		client.spawn_reader(reader);
		client
	}

	/// In-process transport used by deterministic tests (fake adapter).
	#[cfg(test)]
	pub(crate) fn from_duplex(adapter_name: &str, reader: DuplexStream, writer: DuplexStream) -> Self {
		Self::new(
			adapter_name,
			Reader::Duplex(reader),
			Writer::Duplex(writer),
			None,
			Arc::new(StdMutex::new(Vec::new())),
			None,
		)
	}

	pub fn adapter_name(&self) -> &str {
		&self.inner.adapter_name
	}

	pub fn is_alive(&self) -> bool {
		if self.inner.disposed.load(Ordering::SeqCst) || !self.inner.alive.load(Ordering::SeqCst) {
			return false;
		}
		let mut process = self.inner.process.lock().unwrap_or_else(|poison| poison.into_inner());
		if let Some(child) = process.as_mut() {
			match child.try_wait() {
				Ok(Some(_)) => {
					self.inner.alive.store(false, Ordering::SeqCst);
					return false;
				},
				Ok(None) => {},
				Err(_) => {
					self.inner.alive.store(false, Ordering::SeqCst);
					return false;
				},
			}
		}
		true
	}

	pub fn port(&self) -> Option<u16> {
		self.inner.port
	}

	pub fn last_activity(&self) -> i64 {
		self.inner.last_activity.load(Ordering::SeqCst)
	}

	pub fn capabilities(&self) -> Option<Capabilities> {
		self.inner.state.lock().unwrap_or_else(|poison| poison.into_inner()).capabilities.clone()
	}

	pub fn on_event(&self, event: &str, handler: EventHandler) {
		let mut handlers = self.inner.handlers.lock().unwrap_or_else(|poison| poison.into_inner());
		handlers.by_name.entry(event.to_string()).or_default().push(handler);
	}

	pub fn on_any_event(&self, handler: EventHandler) {
		self.inner.handlers.lock().unwrap_or_else(|poison| poison.into_inner()).any.push(handler);
	}

	pub fn on_reverse_request(&self, command: &str, handler: Arc<dyn ReverseRequestHandler>) {
		self
			.inner
			.reverse
			.write()
			.unwrap_or_else(|poison| poison.into_inner())
			.insert(command.to_string(), handler);
	}

	pub async fn initialize(
		&self,
		arguments: Value,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<Value, DapError> {
		let body = self.send_request("initialize", Some(arguments), cancel, deadline).await?;
		let capabilities = Capabilities(if body.is_object() { body.clone() } else { json!({}) });
		self
			.inner
			.state
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
			.capabilities = Some(capabilities);
		Ok(body)
	}

	pub async fn send_request(
		&self,
		command: &str,
		arguments: Option<Value>,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<Value, DapError> {
		if cancel.is_cancelled() {
			return Err(DapError::Aborted);
		}
		if self.inner.disposed.load(Ordering::SeqCst) || !self.inner.alive.load(Ordering::SeqCst) {
			return Err(DapError::NotRunning { adapter: self.inner.adapter_name.clone() });
		}
		let request_timeout = request_timeout(command, deadline)?;
		let seq = {
			let mut state = self.inner.state.lock().unwrap_or_else(|poison| poison.into_inner());
			state.next_seq += 1;
			let seq = state.next_seq;
			let (tx, rx) = oneshot::channel();
			state.pending.insert(seq, Pending { command: command.to_string(), tx });
			drop(state);
			self.inner.last_activity.store(now_millis(), Ordering::SeqCst);
			let mut message = json!({ "seq": seq, "type": "request", "command": command });
			if let Some(arguments) = arguments.clone() {
				message["arguments"] = arguments;
			}
			let inner = Arc::clone(&self.inner);
			tokio::spawn(async move {
				if let Err(error) = write_frame(&inner, &message).await {
					inner.reject(seq, error.to_string());
					inner.fail(error.to_string());
					let client = DapClient { inner: Arc::clone(&inner) };
					tokio::spawn(async move { client.dispose().await });
				}
			});
			(seq, rx)
		};
		let (seq, rx) = seq;
		let result = tokio::select! {
			biased;
			_ = cancel.cancelled() => Err(DapError::Aborted),
			_ = tokio::time::sleep(request_timeout) => Err(DapError::RequestTimeout {
				command: command.to_string(),
				ms: request_timeout.as_millis(),
			}),
			received = rx => match received {
				Ok(Ok(body)) => Ok(body),
				Ok(Err(message)) => Err(DapError::Message(message)),
				Err(_) => Err(DapError::Message(self.closed_message())),
			},
		};
		if result.is_err() {
			self.inner.reject(seq, "request abandoned".to_string());
		}
		result
	}

	/// Subscribe eagerly so an event that arrives in the same chunk as the
	/// triggering response is still observed. Call `wait` afterwards.
	pub fn event_waiter(&self, event: &str, predicate: Option<EventPredicate>) -> EventWaiter {
		EventWaiter {
			inner: Arc::clone(&self.inner),
			event: event.to_string(),
			predicate,
			receiver: self.inner.events.subscribe(),
			closed: self.inner.closed_tx.subscribe(),
		}
	}

	pub async fn wait_for_event(
		&self,
		event: &str,
		predicate: Option<EventPredicate>,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<Value, DapError> {
		self.event_waiter(event, predicate).wait(cancel, deadline).await
	}

	/// Best-effort `terminate`/`disconnect`, then mandatory transport cleanup.
	pub async fn dispose(&self) {
		if self.inner.disposed.swap(true, Ordering::SeqCst) {
			return;
		}
		self.inner.fail(format!("DAP adapter {} disposed", self.inner.adapter_name));
		let mut writer = self.inner.writer.lock().await;
		let _ = writer.flush().await;
		drop(writer);
		let mut process = self.inner.process.lock().unwrap_or_else(|poison| poison.into_inner());
		if let Some(mut child) = process.take() {
			let _ = child.start_kill();
			tokio::spawn(async move {
				let _ = timeout(Duration::from_secs(2), child.wait()).await;
			});
		}
		drop(process);
		if let Some(path) = self.inner.socket_path.lock().unwrap_or_else(|poison| poison.into_inner()).take() {
			let _ = std::fs::remove_file(path);
		}
	}

	fn closed_message(&self) -> String {
		closed_message(&self.inner)
	}

	fn spawn_reader(&self, reader: Reader) {
		let inner = Arc::clone(&self.inner);
		tokio::spawn(async move { reader_loop(inner, reader).await });
	}

	// ── spawn paths ──────────────────────────────────────────────────────────

	pub async fn spawn(
		adapter: &ResolvedAdapter,
		cwd: &Path,
		env: &HashMap<String, String>,
	) -> Result<Self, DapError> {
		match adapter.connect_mode {
			ConnectMode::Stdio => Self::spawn_stdio(adapter, cwd, env).await,
			ConnectMode::Tcp => Self::spawn_tcp(adapter, cwd, env).await,
			ConnectMode::Socket => Self::spawn_socket(adapter, cwd, env).await,
		}
	}

	async fn spawn_stdio(
		adapter: &ResolvedAdapter,
		cwd: &Path,
		env: &HashMap<String, String>,
	) -> Result<Self, DapError> {
		let mut command = base_command(adapter, cwd, env, &adapter.args);
		let mut child = command
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.kill_on_drop(true)
			.spawn()
			.map_err(|error| spawn_error(adapter, error))?;
		let stdin = child.stdin.take().ok_or_else(|| spawn_error(adapter, std::io::Error::other("stdin unavailable")))?;
		let stdout = child
			.stdout
			.take()
			.ok_or_else(|| spawn_error(adapter, std::io::Error::other("stdout unavailable")))?;
		let stderr = Arc::new(StdMutex::new(Vec::new()));
		spawn_stderr(child.stderr.take(), Arc::clone(&stderr));
		Ok(Self::new(&adapter.name, Reader::Stdin(stdout), Writer::Stdin(stdin), Some(child), stderr, None))
	}

	async fn spawn_tcp(
		adapter: &ResolvedAdapter,
		cwd: &Path,
		env: &HashMap<String, String>,
	) -> Result<Self, DapError> {
		let reservation = TcpListener::bind(("127.0.0.1", 0))
			.await
			.map_err(|error| DapError::message(format!("failed to reserve a DAP TCP port: {error}")))?;
		let port = reservation
			.local_addr()
			.map_err(|error| DapError::message(format!("failed to read the reserved DAP TCP port: {error}")))?
			.port();
		drop(reservation);
		let args = substitute_port(&adapter.args, port);
		let mut command = base_command(adapter, cwd, env, &args);
		let mut child = command
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.kill_on_drop(true)
			.spawn()
			.map_err(|error| spawn_error(adapter, error))?;
		let stderr = Arc::new(StdMutex::new(Vec::new()));
		spawn_stderr(child.stderr.take(), Arc::clone(&stderr));
		let stdout = child.stdout.take();
		wait_for_tcp_banner(&mut child, stdout, port, SOCKET_READY_TIMEOUT).await;
		let stream = connect_loop(&mut child, port, SOCKET_READY_TIMEOUT).await.map_err(|error| {
			let _ = child.start_kill();
			error
		})?;
		let (reader, writer) = stream.into_split();
		Ok(Self::new(&adapter.name, Reader::Tcp(reader), Writer::Tcp(writer), Some(child), stderr, Some(port)))
	}

	#[cfg(unix)]
	async fn spawn_socket(
		adapter: &ResolvedAdapter,
		cwd: &Path,
		env: &HashMap<String, String>,
	) -> Result<Self, DapError> {
		use std::os::unix::fs::FileTypeExt;
		let socket_path = std::env::temp_dir().join(format!(
			"dap-{}-{}-{}.sock",
			adapter.name.replace(['/', '\\', ' '], "_"),
			now_millis(),
			std::process::id()
		));
		let mut args = adapter.args.clone();
		args.push(format!("--listen=unix:{}", socket_path.display()));
		let mut command = base_command(adapter, cwd, env, &args);
		let mut child = command
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.kill_on_drop(true)
			.spawn()
			.map_err(|error| spawn_error(adapter, error))?;
		let stderr = Arc::new(StdMutex::new(Vec::new()));
		spawn_stderr(child.stderr.take(), Arc::clone(&stderr));
		spawn_stdout_drain(child.stdout.take());
		let deadline = Instant::now() + SOCKET_READY_TIMEOUT;
		let stream = loop {
			let ready = std::fs::symlink_metadata(&socket_path)
				.map(|metadata| metadata.file_type().is_socket())
				.unwrap_or(false);
			if ready {
				match timeout(Duration::from_millis(250), UnixStream::connect(&socket_path)).await {
					Ok(Ok(stream)) => break stream,
					_ => {},
				}
			}
			if let Ok(Some(_)) = child.try_wait() {
				let _ = std::fs::remove_file(&socket_path);
				return Err(DapError::message(format!(
					"DAP adapter {} exited before socket {} was ready",
					adapter.name,
					socket_path.display()
				)));
			}
			if Instant::now() >= deadline {
				let _ = std::fs::remove_file(&socket_path);
				let _ = child.start_kill();
				return Err(DapError::message(format!(
					"socket {} not ready after {}ms",
					socket_path.display(),
					SOCKET_READY_TIMEOUT.as_millis()
				)));
			}
			tokio::time::sleep(Duration::from_millis(50)).await;
		};
		let (reader, writer) = stream.into_split();
		let client = Self::new(&adapter.name, Reader::Unix(reader), Writer::Unix(writer), Some(child), stderr, None);
		*client.inner.socket_path.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(socket_path);
		Ok(client)
	}

	#[cfg(not(unix))]
	async fn spawn_socket(
		adapter: &ResolvedAdapter,
		cwd: &Path,
		env: &HashMap<String, String>,
	) -> Result<Self, DapError> {
		Self::spawn_socket_client_addr(adapter, cwd, env).await
	}

	#[cfg(not(unix))]
	async fn spawn_socket_client_addr(
		adapter: &ResolvedAdapter,
		cwd: &Path,
		env: &HashMap<String, String>,
	) -> Result<Self, DapError> {
		let listener = TcpListener::bind(("127.0.0.1", 0))
			.await
			.map_err(|error| DapError::message(format!("failed to listen for the DAP adapter callback: {error}")))?;
		let port = listener
			.local_addr()
			.map_err(|error| DapError::message(format!("failed to read the DAP callback port: {error}")))?
			.port();
		let mut args = adapter.args.clone();
		args.push(format!("--client-addr=127.0.0.1:{port}"));
		let mut command = base_command(adapter, cwd, env, &args);
		let mut child = command
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.kill_on_drop(true)
			.spawn()
			.map_err(|error| spawn_error(adapter, error))?;
		let stderr = Arc::new(StdMutex::new(Vec::new()));
		spawn_stderr(child.stderr.take(), Arc::clone(&stderr));
		spawn_stdout_drain(child.stdout.take());
		let accepted = timeout(SOCKET_READY_TIMEOUT, listener.accept()).await;
		drop(listener);
		let (stream, _) = match accepted {
			Ok(Ok(value)) => value,
			_ => {
				let _ = child.start_kill();
				return Err(DapError::message(format!(
					"{} did not connect within {}ms",
					adapter.name,
					SOCKET_READY_TIMEOUT.as_millis()
				)));
			},
		};
		let (reader, writer) = stream.into_split();
		Ok(Self::new(&adapter.name, Reader::Tcp(reader), Writer::Tcp(writer), Some(child), stderr, Some(port)))
	}

	/// Connect a child session to an existing DAP TCP server.
	pub async fn connect(adapter: &ResolvedAdapter, port: u16) -> Result<Self, DapError> {
		let stream = timeout(SOCKET_READY_TIMEOUT, TcpStream::connect(("127.0.0.1", port)))
			.await
			.map_err(|_| DapError::message(format!("timed out connecting child DAP session to 127.0.0.1:{port}")))?
			.map_err(|error| DapError::message(format!("child DAP session connect failed: {error}")))?;
		let (reader, writer) = stream.into_split();
		Ok(Self::new(
			&adapter.name,
			Reader::Tcp(reader),
			Writer::Tcp(writer),
			None,
			Arc::new(StdMutex::new(Vec::new())),
			Some(port),
		))
	}
}

/// An eagerly subscribed event wait. Created before the request that causes
/// the event, then awaited afterwards.
pub struct EventWaiter {
	inner: Arc<Inner>,
	event: String,
	predicate: Option<EventPredicate>,
	receiver: broadcast::Receiver<EventFrame>,
	closed: watch::Receiver<Option<String>>,
}

impl EventWaiter {
	pub async fn wait(mut self, cancel: &CancellationToken, deadline: Instant) -> Result<Value, DapError> {
		if cancel.is_cancelled() {
			return Err(DapError::Aborted);
		}
		let wait_timeout = request_timeout(&self.event, deadline)?;
		if let Some(message) = self.closed.borrow().clone() {
			return Err(DapError::Message(message));
		}
		let sleep = tokio::time::sleep(wait_timeout);
		tokio::pin!(sleep);
		loop {
			tokio::select! {
				biased;
				_ = cancel.cancelled() => return Err(DapError::Aborted),
				_ = &mut sleep => {
					return Err(DapError::Message(format!(
						"DAP event {} timed out after {}ms",
						self.event,
						wait_timeout.as_millis()
					)));
				},
				changed = self.closed.changed() => {
					if changed.is_err() || self.closed.borrow().is_some() {
						return Err(DapError::Message(closed_message(&self.inner)));
					}
				},
				received = self.receiver.recv() => match received {
					Ok(frame) => {
						if frame.event != self.event {
							continue;
						}
						if self.predicate.as_ref().is_some_and(|check| !check(&frame.body)) {
							continue;
						}
						return Ok(frame.body);
					},
					Err(broadcast::error::RecvError::Closed) => {
						return Err(DapError::Message(closed_message(&self.inner)));
					},
					Err(broadcast::error::RecvError::Lagged(_)) => continue,
				},
			}
		}
	}
}

fn closed_message(inner: &Inner) -> String {
	if let Some(message) = inner.closed_tx.borrow().clone() {
		return message;
	}
	let stderr = String::from_utf8_lossy(&inner.stderr.lock().unwrap_or_else(|poison| poison.into_inner()))
		.trim()
		.to_string();
	if stderr.is_empty() {
		format!("DAP adapter {} is not running", inner.adapter_name)
	} else {
		format!("DAP adapter {} exited: {stderr}", inner.adapter_name)
	}
}

fn now_millis() -> i64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|duration| duration.as_millis() as i64)
		.unwrap_or(0)
}

fn request_timeout(command: &str, deadline: Instant) -> Result<Duration, DapError> {
	let now = Instant::now();
	if now >= deadline {
		return Err(DapError::RequestTimeout {
			command: command.to_string(),
			ms: 0,
		});
	}
	Ok((deadline - now).min(DEFAULT_REQUEST_TIMEOUT))
}

fn spawn_error(adapter: &ResolvedAdapter, error: std::io::Error) -> DapError {
	DapError::message(format!(
		"failed to start DAP adapter '{}' ({}): {error}",
		adapter.name, adapter.resolved_command
	))
}

pub(crate) fn non_interactive_env() -> [(&'static str, &'static str); 25] {
	[
		("PAGER", "cat"),
		("GIT_PAGER", "cat"),
		("MANPAGER", "cat"),
		("SYSTEMD_PAGER", "cat"),
		("BAT_PAGER", "cat"),
		("GH_PAGER", "cat"),
		("LESS", "FRX"),
		("TERM", "dumb"),
		("NO_COLOR", "1"),
		("PYTHONUNBUFFERED", "1"),
		("GIT_EDITOR", "true"),
		("VISUAL", "true"),
		("EDITOR", "true"),
		("GIT_TERMINAL_PROMPT", "0"),
		("CI", "true"),
		("AGENT", "1"),
		("npm_config_yes", "true"),
		("npm_config_fund", "false"),
		("npm_config_audit", "false"),
		("npm_config_progress", "false"),
		("PIP_NO_INPUT", "1"),
		("GH_PROMPT_DISABLED", "1"),
		("DEBIAN_FRONTEND", "noninteractive"),
		("CARGO_TERM_PROGRESS_WHEN", "never"),
		("TF_IN_AUTOMATION", "1"),
	]
}

fn base_command(
	adapter: &ResolvedAdapter,
	cwd: &Path,
	env: &HashMap<String, String>,
	extra_args: &[String],
) -> Command {
	let mut command = Command::new(&adapter.resolved_command);
	command.args(extra_args);
	command.current_dir(cwd);
	command.envs(non_interactive_env());
	// The agent environment is authoritative for PATH/HOME overrides.
	command.envs(env);
	apply_detached(&mut command);
	command
}

/// Start the child in its own session so debuggee children have no
/// controlling terminal (`SIGTTIN` under shell job control).
pub(crate) fn apply_detached(command: &mut Command) {
	#[cfg(unix)]
	unsafe {
		command.pre_exec(|| {
			libc::setsid();
			Ok(())
		});
	}
}

fn spawn_stderr(mut stderr: Option<tokio::process::ChildStderr>, buffer: Arc<StdMutex<Vec<u8>>>) {
	let Some(mut stderr) = stderr.take() else {
		return;
	};
	tokio::spawn(async move {
		let mut chunk = [0u8; 4096];
		loop {
			match stderr.read(&mut chunk).await {
				Ok(0) | Err(_) => break,
				Ok(read) => {
					let mut buffer = buffer.lock().unwrap_or_else(|poison| poison.into_inner());
					buffer.extend_from_slice(&chunk[..read]);
					if buffer.len() > MAX_STDERR_BYTES {
						let excess = buffer.len() - MAX_STDERR_BYTES;
						buffer.drain(..excess);
					}
				},
			}
		}
	});
}

fn spawn_stdout_drain(mut stdout: Option<ChildStdout>) {
	let Some(mut stdout) = stdout.take() else {
		return;
	};
	tokio::spawn(async move {
		let mut chunk = [0u8; 4096];
		while matches!(stdout.read(&mut chunk).await, Ok(read) if read > 0) {}
	});
}

/// Best-effort banner gate: wait for the reserved port to appear on stdout,
/// while keeping stdout drained for the adapter's lifetime.
async fn wait_for_tcp_banner(child: &mut Child, stdout: Option<ChildStdout>, port: u16, wait: Duration) {
	let Some(mut stdout) = stdout else {
		return;
	};
	let port_text = port.to_string();
	let mut buffer = String::new();
	let deadline = Instant::now() + wait;
	let mut banner_seen = false;
	loop {
		if banner_seen && buffer.len() > 4096 {
			buffer = buffer[buffer.len() - 1024..].to_string();
		}
		let now = Instant::now();
		if now >= deadline {
			break;
		}
		let mut chunk = [0u8; 4096];
		match timeout(deadline - now, stdout.read(&mut chunk)).await {
			Ok(Ok(0)) | Err(_) => break,
			Ok(Ok(read)) => {
				buffer.push_str(&String::from_utf8_lossy(&chunk[..read]));
				if buffer.contains(&port_text) {
					banner_seen = true;
					break;
				}
			},
			Ok(Err(_)) => break,
		}
	}
	// Continue draining so a chatty adapter cannot fill the pipe and wedge.
	tokio::spawn(async move {
		let mut chunk = [0u8; 4096];
		while matches!(stdout.read(&mut chunk).await, Ok(read) if read > 0) {}
	});
	let _ = child;
}

async fn connect_loop(child: &mut Child, port: u16, wait: Duration) -> Result<TcpStream, DapError> {
	let deadline = Instant::now() + wait;
	loop {
		if let Ok(Some(_)) = child.try_wait() {
			return Err(DapError::message(format!(
				"DAP adapter exited before TCP port 127.0.0.1:{port} was ready"
			)));
		}
		match timeout(Duration::from_millis(250), TcpStream::connect(("127.0.0.1", port))).await {
			Ok(Ok(stream)) => return Ok(stream),
			_ => {},
		}
		if Instant::now() >= deadline {
			return Err(DapError::message(format!(
				"TCP port 127.0.0.1:{port} was not ready after {}ms",
				wait.as_millis()
			)));
		}
		tokio::time::sleep(Duration::from_millis(50)).await;
	}
}

async fn write_frame(inner: &Arc<Inner>, message: &Value) -> Result<(), DapError> {
	let content = serde_json::to_string(message).map_err(|error| DapError::message(error.to_string()))?;
	let mut frame = Vec::with_capacity(content.len() + 32);
	frame.extend_from_slice(format!("Content-Length: {}\r\n\r\n", content.len()).as_bytes());
	frame.extend_from_slice(content.as_bytes());
	let mut writer = inner.writer.lock().await;
	let write = async {
		writer.write_all(&frame).await?;
		writer.flush().await
	};
	match timeout(WRITE_MESSAGE_TIMEOUT, write).await {
		Ok(Ok(())) => Ok(()),
		Ok(Err(error)) => Err(DapError::message(format!(
			"DAP adapter {} exited before write completed: {error}",
			inner.adapter_name
		))),
		Err(_) => Err(DapError::message(format!(
			"DAP adapter {} write timed out after {}ms",
			inner.adapter_name,
			WRITE_MESSAGE_TIMEOUT.as_millis()
		))),
	}
}

async fn reader_loop(inner: Arc<Inner>, mut reader: Reader) {
	let mut framer = MessageFramer::new();
	let mut buffer = vec![0u8; 64 * 1024];
	let mut close: Option<String> = None;
	'read: loop {
		match reader.read(&mut buffer).await {
			Ok(0) => break,
			Ok(read) => {
				framer.push(&buffer[..read]);
				match framer.drain(|_| eprintln!("DAP framing resync: header block without Content-Length")) {
					Ok(messages) => {
						for message in messages {
							handle_message(&inner, &message).await;
						}
					},
					Err(error) => {
						close = Some(framing_error_message(&inner, &error));
						break 'read;
					},
				}
			},
			Err(error) => {
				close = Some(format!("DAP connection closed: {error}"));
				break;
			},
		}
	}
	if close.is_none() {
		close = Some(inner.closed_message_hint());
	}
	inner.fail(close.unwrap_or_else(|| format!("DAP connection closed: {}", inner.adapter_name)));
}

impl Inner {
	fn closed_message_hint(&self) -> String {
		let mut process = self.process.lock().unwrap_or_else(|poison| poison.into_inner());
		if let Some(child) = process.as_mut() {
			if let Ok(Some(status)) = child.try_wait() {
				let stderr = String::from_utf8_lossy(
					&self.stderr.lock().unwrap_or_else(|poison| poison.into_inner()),
				)
				.trim()
				.to_string();
				if stderr.is_empty() {
					return format!("DAP adapter exited unexpectedly (code {})", status.code().unwrap_or(-1));
				}
				return format!(
					"DAP adapter exited (code {}): {stderr}",
					status.code().unwrap_or(-1)
				);
			}
		}
		format!("DAP connection closed: {} transport ended", self.adapter_name)
	}
}

fn framing_error_message(inner: &Inner, error: &FramingError) -> String {
	format!("DAP framing error for {}: {error}", inner.adapter_name)
}

async fn handle_message(inner: &Arc<Inner>, text: &str) {
	let message: Value = match serde_json::from_str(text) {
		Ok(message) => message,
		Err(error) => {
			eprintln!("DAP message handling failed for {}: {error}", inner.adapter_name);
			return;
		},
	};
	inner.last_activity.store(now_millis(), Ordering::SeqCst);
	match message.get("type").and_then(Value::as_str) {
		Some("response") => {
			let Some(request_seq) = message.get("request_seq").and_then(Value::as_u64) else {
				return;
			};
			let success = message.get("success").and_then(Value::as_bool).unwrap_or(false);
			let mut state = inner.state.lock().unwrap_or_else(|poison| poison.into_inner());
			let Some(pending) = state.pending.remove(&request_seq) else {
				return;
			};
			if success {
				let body = message.get("body").cloned().unwrap_or(Value::Null);
				let _ = pending.tx.send(Ok(body));
			} else {
				let error = message
					.get("message")
					.and_then(Value::as_str)
					.map(str::to_string)
					.unwrap_or_else(|| format!("DAP request {} failed", pending.command));
				let _ = pending.tx.send(Err(error));
			}
		},
		Some("event") => {
			let Some(event) = message.get("event").and_then(Value::as_str) else {
				return;
			};
			let body = message.get("body").cloned().unwrap_or(Value::Null);
			let handlers = {
				let handlers = inner.handlers.lock().unwrap_or_else(|poison| poison.into_inner());
				let specific = handlers.by_name.get(event).cloned().unwrap_or_default();
				let mut all = specific;
				all.extend(handlers.any.iter().cloned());
				all
			};
			for handler in handlers {
				handler(body.clone());
			}
			let _ = inner.events.send(EventFrame { event: event.to_string(), body });
		},
		Some("request") => {
			let Some(request_seq) = message.get("seq").and_then(Value::as_u64) else {
				return;
			};
			let command = message.get("command").and_then(Value::as_str).unwrap_or_default().to_string();
			let arguments = message.get("arguments").cloned().unwrap_or(Value::Null);
			let inner = Arc::clone(inner);
			tokio::spawn(async move {
				let handler = inner
					.reverse
					.read()
					.unwrap_or_else(|poison| poison.into_inner())
					.get(&command)
					.cloned();
				let response = match handler {
					Some(handler) => match handler.handle(arguments).await {
						Ok(body) => success_response(&inner, request_seq, &command, body),
						Err(error) => error_response(&inner, request_seq, &command, error.to_string()),
					},
					None => error_response(&inner, request_seq, &command, format!("Unsupported DAP request: {command}")),
				};
				if let Err(error) = write_frame(&inner, &response).await {
					inner.fail(error.to_string());
				}
			});
		},
		_ => {},
	}
}

fn next_seq(inner: &Inner) -> u64 {
	let mut state = inner.state.lock().unwrap_or_else(|poison| poison.into_inner());
	state.next_seq += 1;
	state.next_seq
}

fn success_response(inner: &Inner, request_seq: u64, command: &str, body: Value) -> Value {
	json!({
		"seq": next_seq(inner),
		"type": "response",
		"request_seq": request_seq,
		"success": true,
		"command": command,
		"body": body,
	})
}

fn error_response(inner: &Inner, request_seq: u64, command: &str, message: String) -> Value {
	json!({
		"seq": next_seq(inner),
		"type": "response",
		"request_seq": request_seq,
		"success": false,
		"command": command,
		"message": message,
		"body": { "error": { "id": 1, "format": message } },
	})
}
