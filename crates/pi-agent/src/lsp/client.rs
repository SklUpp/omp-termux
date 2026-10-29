//! Native LSP client: subprocess lifecycle, Content-Length JSON-RPC, request
//! correlation, cancellation/timeouts, diagnostics cache, document sync, and
//! workspace-edit reconciliation.
//!
//! Port of `packages/coding-agent/src/lsp/client.ts`. One client is shared per
//! `command + cwd + initialization identity`. When `lsp.shared` is enabled the
//! client first tries the broker-owned mux (see [`super::mux`]) and otherwise
//! spawns a private process; an external `lspmux` wrapper (see
//! [`super::lspmux`]) always wins over the native mux. No binary is ever
//! downloaded or installed.

use std::{
	collections::{BTreeMap, HashMap, HashSet},
	path::{Path, PathBuf},
	pin::Pin,
	sync::{
		Arc, LazyLock, Mutex, MutexGuard,
		atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
	},
	task::{Context, Poll},
	time::{Duration, Instant},
};

use serde_json::{json, Value};
use tokio::{
	io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
	net::unix::{OwnedReadHalf, OwnedWriteHalf},
	process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
	sync::{oneshot, watch},
};
use tokio_util::sync::CancellationToken;

use super::{
	config::{get_lsp_server_for_file, get_lsp_servers, is_method_not_found_error, load_config},
	edits::{apply_workspace_edit, ExecutedWorkspaceChange},
	framing::{FramingError, MessageFramer},
	lspmux,
	mux::{self, MuxConnectParams},
	types::{
		Diagnostic, JsonRpcError, JsonRpcNotification, JsonRpcRequest,
		JsonRpcResponse, LspConfig, PublishedDiagnostics, PublishDiagnosticsParams, ServerConfig,
	},
	utils::{file_to_uri, format_path_relative_to_cwd, uri_key, uri_to_file},
};

pub const WARMUP_TIMEOUT_MS: u64 = 5_000;
pub const PROJECT_LOAD_TIMEOUT_MS: u64 = 15_000;
pub const SHUTDOWN_TIMEOUT_MS: u64 = 5_000;
pub const EXIT_TIMEOUT_MS: u64 = 1_000;
pub const INIT_FAILURE_BACKOFF_MS: u64 = 3 * 60 * 1000;
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const READER_EXIT_GRACE_MS: u64 = 100;

// =============================================================================
// Errors
// =============================================================================

#[derive(Debug, Clone, thiserror::Error)]
pub enum LspError {
	#[error("Operation aborted")]
	Cancelled,
	#[error("LSP request {method} timed out after {ms}ms")]
	Timeout { method: String, ms: u64 },
	#[error("{0}")]
	Message(String),
	#[error("LSP framing failed: {0}")]
	Framing(String),
}

impl LspError {
	#[must_use]
	pub fn message(&self) -> String {
		self.to_string()
	}
}

// =============================================================================
// Client state
// =============================================================================

#[derive(Debug, Clone)]
pub struct OpenFile {
	pub version: i64,
	pub language_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientStatus {
	Connecting,
	Ready,
	Error,
}

impl ClientStatus {
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Connecting => "connecting",
			Self::Ready => "ready",
			Self::Error => "error",
		}
	}
}

pub struct LspClient {
	pub name: String,
	pub cwd: PathBuf,
	pub config: ServerConfig,
	pub pid: u32,
	pub shared_mux: bool,
	stdin: tokio::sync::Mutex<Option<ClientWriter>>,
	mux_restart: Option<tokio::sync::mpsc::UnboundedSender<()>>,
	pending: Mutex<HashMap<i64, oneshot::Sender<Result<Value, LspError>>>>,
	state: Mutex<ClientState>,
	stderr: Mutex<String>,
	exit_tx: watch::Sender<bool>,
	exit_rx: watch::Receiver<bool>,
	project_tx: watch::Sender<bool>,
	project_rx: watch::Receiver<bool>,
	request_id: AtomicI64,
	last_activity_ms: AtomicU64,
	rust_ready: AtomicBool,
}

/// Write half of either a private child's stdin or a shared mux socket.
enum ClientWriter {
	Child(ChildStdin),
	Shared(OwnedWriteHalf),
}

impl AsyncWrite for ClientWriter {
	fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buffer: &[u8]) -> Poll<std::io::Result<usize>> {
		match self.get_mut() {
			Self::Child(writer) => Pin::new(writer).poll_write(cx, buffer),
			Self::Shared(writer) => Pin::new(writer).poll_write(cx, buffer),
		}
	}

	fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
		match self.get_mut() {
			Self::Child(writer) => Pin::new(writer).poll_flush(cx),
			Self::Shared(writer) => Pin::new(writer).poll_flush(cx),
		}
	}

	fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
		match self.get_mut() {
			Self::Child(writer) => Pin::new(writer).poll_shutdown(cx),
			Self::Shared(writer) => Pin::new(writer).poll_shutdown(cx),
		}
	}
}

/// Read half of either a private child's stdout or a shared mux socket.
enum ClientReader {
	Child(ChildStdout),
	Shared(OwnedReadHalf),
}

impl AsyncRead for ClientReader {
	fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buffer: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
		match self.get_mut() {
			Self::Child(reader) => Pin::new(reader).poll_read(cx, buffer),
			Self::Shared(reader) => Pin::new(reader).poll_read(cx, buffer),
		}
	}
}

#[derive(Default)]
struct ClientState {
	diagnostics: HashMap<String, PublishedDiagnostics>,
	diagnostics_version: u64,
	open_files: HashMap<String, OpenFile>,
	capabilities: Option<Value>,
	dynamic_registrations: HashMap<String, String>,
	progress_tokens: HashSet<String>,
	status: Option<ClientStatus>,
}

struct PendingGuard<'a> {
	pending: &'a std::sync::Mutex<HashMap<i64, oneshot::Sender<Result<Value, LspError>>>>,
	id:      i64,
}

impl Drop for PendingGuard<'_> {
	fn drop(&mut self) {
		self.pending.lock().unwrap_or_else(|error| error.into_inner()).remove(&self.id);
	}
}

impl LspClient {
	#[must_use]
	pub fn status(&self) -> ClientStatus {
		lock(&self.state).status.unwrap_or(ClientStatus::Connecting)
	}

	#[must_use]
	pub fn server_capabilities(&self) -> Option<Value> {
		lock(&self.state).capabilities.clone()
	}

	#[must_use]
	pub fn diagnostics_for(&self, uri: &str) -> Option<PublishedDiagnostics> {
		lock(&self.state).diagnostics.get(&uri_key(uri)).cloned()
	}

	#[must_use]
	pub fn diagnostics_version(&self) -> u64 {
		lock(&self.state).diagnostics_version
	}

	#[must_use]
	pub fn open_file_version(&self, uri: &str) -> Option<i64> {
		lock(&self.state).open_files.get(&uri_key(uri)).map(|file| file.version)
	}

	#[must_use]
	pub fn has_open_file(&self, uri: &str) -> bool {
		lock(&self.state).open_files.contains_key(&uri_key(uri))
	}

	/// Forget a closed document and its cached diagnostics.
	pub fn close_file(&self, uri: &str) {
		let key = uri_key(uri);
		let mut state = lock(&self.state);
		state.open_files.remove(&key);
		state.diagnostics.remove(&key);
	}

	#[must_use]
	pub fn pending_count(&self) -> usize {
		lock(&self.pending).len()
	}

	fn touch(&self) {
		self.last_activity_ms
			.store(epoch_millis(), Ordering::Relaxed);
	}

	fn idle_for(&self) -> Duration {
		let last = self.last_activity_ms.load(Ordering::Relaxed);
		Duration::from_millis(epoch_millis().saturating_sub(last))
	}

	fn set_status(&self, status: ClientStatus) {
		lock(&self.state).status = Some(status);
	}

	fn reject_pending(&self, message: &str) {
		let mut pending = lock(&self.pending);
		for (_, sender) in pending.drain() {
			let _ = sender.send(Err(LspError::Message(message.to_string())));
		}
	}

	#[must_use]
	pub fn stderr_tail(&self) -> String {
		lock(&self.stderr).trim().to_string()
	}

	fn kill_process(&self) {
		if let Some(restart) = &self.mux_restart {
			let _ = restart.send(());
			return;
		}
		#[cfg(unix)]
		{
			if let Ok(pid) = i32::try_from(self.pid) {
				// The child was started as its own process group (pgid == pid),
				// so a negative pid kills the server and any tool children it
				// spawned instead of leaking them.
				unsafe {
					libc::kill(-pid, libc::SIGKILL);
					libc::kill(pid, libc::SIGKILL);
				}
				return;
			}
		}
		#[cfg(not(unix))]
		{
			let _ = self.pid;
		}
	}

	async fn wait_for_exit(&self, timeout: Duration) -> bool {
		if *self.exit_rx.borrow() {
			return true;
		}
		let mut rx = self.exit_rx.clone();
		tokio::time::timeout(timeout, async move {
			let _ = rx.wait_for(|exited| *exited).await;
		})
		.await
		.is_ok()
	}

	async fn write_message(&self, message: Value, cancel: &CancellationToken) -> Result<(), LspError> {
		let payload = serde_json::to_vec(&message)
			.map_err(|error| LspError::Message(format!("failed to encode LSP message: {error}")))?;
		let mut framed = Vec::with_capacity(payload.len() + 32);
		framed.extend_from_slice(format!("Content-Length: {}\r\n\r\n", payload.len()).as_bytes());
		framed.extend_from_slice(&payload);

		let mut guard = tokio::select! {
			guard = self.stdin.lock() => guard,
			_ = cancel.cancelled() => return Err(LspError::Cancelled),
		};
		let Some(stdin) = guard.as_mut() else {
			return Err(LspError::Message("LSP stdin is closed".to_string()));
		};
		let write = async {
			stdin.write_all(&framed).await.map_err(|error| error.to_string())?;
			stdin.flush().await.map_err(|error| error.to_string())
		};
		tokio::select! {
			_ = cancel.cancelled() => Err(LspError::Cancelled),
			result = tokio::time::timeout(WRITE_TIMEOUT, write) => match result {
				Ok(Ok(())) => Ok(()),
				Ok(Err(error)) => {
					self.kill_process();
					Err(LspError::Message(format!("failed to write LSP message: {error}")))
				}
				Err(_) => {
					self.kill_process();
					Err(LspError::Message("LSP message write timed out".to_string()))
				}
			},
		}
	}

	pub async fn send_request(
		&self,
		method: &str,
		params: Value,
		cancel: &CancellationToken,
		timeout: Option<Duration>,
	) -> Result<Value, LspError> {
		if cancel.is_cancelled() {
			return Err(LspError::Cancelled);
		}
		let id = self.request_id.fetch_add(1, Ordering::SeqCst) + 1;
		// Upstream applies its 30s default only to signal-less requests. Every
		// Rust caller passes a cancellation token that carries its own budget
		// (tool deadline, diagnostics pipeline budget, warmup timeout), so the
		// internal default must never shadow a caller-supplied deadline.
		let effective_timeout = timeout;
		let (sender, receiver) = oneshot::channel();
		lock(&self.pending).insert(id, sender);
		let _pending_guard = PendingGuard { pending: &self.pending, id };
		self.touch();

		let request = JsonRpcRequest { jsonrpc: "2.0", id, method: method.to_string(), params };
		let message = serde_json::to_value(request)
			.map_err(|error| LspError::Message(format!("failed to encode LSP request: {error}")))?;
		if let Err(error) = self.write_message(message, cancel).await {
			lock(&self.pending).remove(&id);
			return Err(error);
		}

		let result = tokio::select! {
			response = receiver => match response {
				Ok(result) => result,
				Err(_) => Err(LspError::Message("LSP client shut down while awaiting response".to_string())),
			},
			_ = cancel.cancelled() => {
				lock(&self.pending).remove(&id);
				self.cancel_request(id);
				return Err(LspError::Cancelled);
			}
			_ = async {
				match effective_timeout {
					Some(duration) => tokio::time::sleep(duration).await,
					None => std::future::pending::<()>().await,
				}
			} => {
				let duration = effective_timeout.unwrap_or_default();
				lock(&self.pending).remove(&id);
				self.cancel_request(id);
				return Err(LspError::Timeout {
					method: method.to_string(),
					ms: duration.as_millis().min(u128::from(u64::MAX)) as u64,
				});
			}
		};
		self.touch();
		result
	}

	fn cancel_request(&self, id: i64) {
		let client_name = self.name.clone();
		// Best-effort notification: the server may be wedged, so never block the
		// caller's error path on the write.
		let token = CancellationToken::new();
		let message = json!({
			"jsonrpc": "2.0",
			"method": "$/cancelRequest",
			"params": { "id": id }
		});
		if let Some(arc) = registry().clients.lock().unwrap_or_else(|e| e.into_inner()).get(&client_name).cloned() {
			tokio::spawn(async move {
				let _ = arc.write_message(message, &token).await;
			});
		}
	}

	pub async fn send_notification(
		&self,
		method: &str,
		params: Option<Value>,
		cancel: &CancellationToken,
	) -> Result<(), LspError> {
		let notification = JsonRpcNotification { jsonrpc: "2.0", method: method.to_string(), params };
		let message = serde_json::to_value(notification)
			.map_err(|error| LspError::Message(format!("failed to encode LSP notification: {error}")))?;
		self.touch();
		self.write_message(message, cancel).await
	}

	async fn send_response(
		&self,
		id: Value,
		result: Option<Value>,
		error: Option<JsonRpcError>,
	) {
		let response = JsonRpcResponse { jsonrpc: "2.0", id, result, error };
		let Ok(message) = serde_json::to_value(response) else {
			return;
		};
		let _ = self.write_message(message, &CancellationToken::new()).await;
	}

	#[must_use]
	pub fn is_rust_analyzer(&self) -> bool {
		fn basename(command: &Path) -> String {
			command.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()
		}
		basename(Path::new(&self.config.command)) == "rust-analyzer"
			|| self
				.config
				.resolved_command
				.as_ref()
				.is_some_and(|command| basename(command) == "rust-analyzer")
	}
}

fn epoch_millis() -> u64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
		.unwrap_or(0)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
	mutex.lock().unwrap_or_else(|error| error.into_inner())
}

// =============================================================================
// Registry
// =============================================================================

struct ClientCell {
	lock: tokio::sync::Mutex<()>,
}

struct Registry {
	clients: Mutex<HashMap<String, Arc<LspClient>>>,
	cells: Mutex<HashMap<String, Arc<ClientCell>>>,
	file_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
	failures: Mutex<HashMap<String, (Instant, String)>>,
	/// Live [`LspSession`] ids per client key. A shared client stays open while
	/// any owner remains; the last owner to release closes the connection.
	owners: Mutex<HashMap<String, HashSet<u64>>>,
}

static REGISTRY: LazyLock<Registry> = LazyLock::new(|| Registry {
	clients: Mutex::new(HashMap::new()),
	cells: Mutex::new(HashMap::new()),
	file_locks: Mutex::new(HashMap::new()),
	failures: Mutex::new(HashMap::new()),
	owners: Mutex::new(HashMap::new()),
});

static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

fn registry() -> &'static Registry {
	&REGISTRY
}

fn client_key(config: &ServerConfig, cwd: &Path, shared_mux: bool) -> String {
	let spawn_command = config
		.resolved_command
		.as_ref()
		.map(|path| path.to_string_lossy().into_owned())
		.unwrap_or_else(|| config.command.clone());
	let identity = json!([
		config.args,
		config.init_options,
		config.settings,
		config.language_id,
	]);
	format!(
		"{spawn_command}:{}:{}:{}",
		normalize_cwd(cwd),
		if shared_mux { "shared" } else { "private" },
		serde_json::to_string(&identity).unwrap_or_default()
	)
}

fn normalize_cwd(cwd: &Path) -> String {
	cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf()).to_string_lossy().into_owned()
}

fn remove_client(client: &Arc<LspClient>) {
	let removed = {
		let mut clients = lock(&registry().clients);
		if clients.get(&client.name).is_some_and(|candidate| Arc::ptr_eq(candidate, client)) {
			clients.remove(&client.name);
			true
		} else {
			false
		}
	};
	// Only the removed instance's claims die: a newer client may already own
	// the same key.
	if removed {
		lock(&registry().owners).remove(&client.name);
	}
}

/// Per-owner claim set over the process-wide client registry.
///
/// Clients are shared per `command + cwd + initialization identity` (and, for
/// `lsp.shared`, through the broker mux), matching upstream. A root runner and
/// its children therefore see the same live client, so teardown must be
/// ownership-scoped: the last owner to release closes the connection, and a
/// shared mux server is never killed — the broker answers `shutdown` itself and
/// `exit` closes only this session's link.
pub struct LspSession {
	id:         u64,
	shared_mux: bool,
	held:       Mutex<HashSet<String>>,
	released:   AtomicBool,
}

impl LspSession {
	#[must_use]
	pub fn new(shared_mux: bool) -> Arc<Self> {
		Arc::new(Self {
			id: NEXT_OWNER.fetch_add(1, Ordering::Relaxed),
			shared_mux,
			held: Mutex::new(HashSet::new()),
			released: AtomicBool::new(false),
		})
	}

	#[must_use]
	pub fn shared_mux(&self) -> bool {
		self.shared_mux
	}

	/// Record this owner on `client`, unless the owner already released.
	fn claim(&self, client: &Arc<LspClient>) {
		let key = client.name.clone();
		if self.released.load(Ordering::SeqCst) {
			// A tool call raced session teardown. Close the connection when this
			// would have been its only owner instead of leaking it.
			if last_owner_of(&key) {
				shutdown_detached(vec![key]);
			}
			return;
		}
		lock(&registry().owners).entry(key.clone()).or_default().insert(self.id);
		lock(&self.held).insert(key);
	}

	/// Remove this owner from `key`; true when no owner remains.
	fn drop_owner(&self, key: &str) -> bool {
		let mut owners = lock(&registry().owners);
		let Some(entry) = owners.get_mut(key) else {
			return false;
		};
		entry.remove(&self.id);
		if entry.is_empty() {
			owners.remove(key);
			true
		} else {
			false
		}
	}

	/// Close every client this owner kept open and no other owner still uses.
	///
	/// The shared-mux path stays broker-safe: `shutdown_client_instance` sends
	/// `shutdown` (answered by the broker, the shared server keeps running) and
	/// `exit` (closes this session's link only).
	pub async fn release(&self) {
		if self.released.swap(true, Ordering::SeqCst) {
			return;
		}
		let keys: Vec<String> = lock(&self.held).drain().collect();
		for key in keys {
			if !self.drop_owner(&key) {
				continue;
			}
			let client = lock(&registry().clients).get(&key).cloned();
			if let Some(client) = client {
				let _ = shutdown_client_instance(&client).await;
			}
		}
	}
}

impl Drop for LspSession {
	fn drop(&mut self) {
		if self.released.swap(true, Ordering::SeqCst) {
			return;
		}
		let keys: Vec<String> = lock(&self.held).drain().collect();
		let mut stop = Vec::new();
		for key in keys {
			if self.drop_owner(&key) {
				stop.push(key);
			}
		}
		if stop.is_empty() {
			return;
		}
		// `Drop` cannot await. Prefer the graceful detached path; outside a
		// runtime, at least kill private servers instead of orphaning them.
		shutdown_detached(stop);
	}
}

/// True when no live owner claims `key` any more.
fn last_owner_of(key: &str) -> bool {
	lock(&registry().owners).get(key).is_none_or(HashSet::is_empty)
}

fn shutdown_detached(keys: Vec<String>) {
	if let Ok(handle) = tokio::runtime::Handle::try_current() {
		handle.spawn(async move {
			for key in keys {
				let client = lock(&registry().clients).get(&key).cloned();
				if let Some(client) = client {
					let _ = shutdown_client_instance(&client).await;
				}
			}
		});
		return;
	}
	for key in keys {
		if let Some(client) = lock(&registry().clients).remove(&key) {
			client.reject_pending("LSP owner released");
			if !client.shared_mux {
				client.kill_process();
			}
		}
	}
}

// =============================================================================
// Spawn and initialize
// =============================================================================

/// Resolve (or spawn) a client without ownership tracking.
pub async fn get_or_create_client(
	config: &ServerConfig,
	cwd: &Path,
	init_timeout: Option<Duration>,
	cancel: &CancellationToken,
	shared_mux: bool,
) -> Result<Arc<LspClient>, LspError> {
	get_or_create_client_inner(config, cwd, init_timeout, cancel, shared_mux).await
}

/// Resolve (or spawn) a client and claim it for `session`.
///
/// Session-scoped callers (the `lsp` tool and its diagnostics/format helpers)
/// use this so runner/child teardown closes only clients the owner introduced.
pub async fn get_or_create_client_for(
	session: &Arc<LspSession>,
	config: &ServerConfig,
	cwd: &Path,
	init_timeout: Option<Duration>,
	cancel: &CancellationToken,
) -> Result<Arc<LspClient>, LspError> {
	let client = get_or_create_client_inner(
		config,
		cwd,
		init_timeout,
		cancel,
		session.shared_mux(),
	)
	.await?;
	session.claim(&client);
	Ok(client)
}

async fn get_or_create_client_inner(
	config: &ServerConfig,
	cwd: &Path,
	init_timeout: Option<Duration>,
	cancel: &CancellationToken,
	shared_mux: bool,
) -> Result<Arc<LspClient>, LspError> {
	let key = client_key(config, cwd, shared_mux);
	if let Some(client) = lock(&registry().clients).get(&key).cloned() {
		client.touch();
		return Ok(client);
	}
	let previous_failure = lock(&registry().failures).get(&key).cloned();
	if let Some((at, message)) = previous_failure {
		if at.elapsed() < Duration::from_millis(INIT_FAILURE_BACKOFF_MS) {
			return Err(LspError::Message(format!(
				"LSP server {} failed to initialize recently: {message}",
				config.command
			)));
		}
		lock(&registry().failures).remove(&key);
	}

	let cell = {
		let mut cells = lock(&registry().cells);
		cells
			.entry(key.clone())
			.or_insert_with(|| Arc::new(ClientCell { lock: tokio::sync::Mutex::new(()) }))
			.clone()
	};
	let _guard = cell.lock.lock().await;
	let existing_client = lock(&registry().clients).get(&key).cloned();
	if let Some(client) = existing_client {
		if client.status() == ClientStatus::Ready {
			client.touch();
			return Ok(client);
		}
		remove_client(&client);
	}
	if cancel.is_cancelled() {
		return Err(LspError::Cancelled);
	}

	match spawn_and_initialize(config, cwd, init_timeout, cancel, &key, shared_mux).await {
		Ok(client) => {
			lock(&registry().failures).remove(&key);
			lock(&registry().clients).insert(key, client.clone());
			Ok(client)
		}
		Err(error) => {
			// Negative-cache deterministic failures only. Caller aborts are
			// transient, and a timeout under a caller-shortened init deadline
			// (warmup, diagnostics pipeline, tool timeout) says the budget was
			// small, not that the server is broken; a later call with a fresh
			// deadline must retry.
			let caller_budget_timeout = init_timeout.is_some() && matches!(&error, LspError::Timeout { .. });
			if !cancel.is_cancelled() && !caller_budget_timeout {
				lock(&registry().failures)
					.insert(key, (Instant::now(), error.message()));
			}
			Err(error)
		},
	}
}

async fn spawn_and_initialize(
	config: &ServerConfig,
	cwd: &Path,
	init_timeout: Option<Duration>,
	cancel: &CancellationToken,
	key: &str,
	shared_mux: bool,
) -> Result<Arc<LspClient>, LspError> {
	let base_command = config
		.resolved_command
		.as_ref()
		.map(|path| path.to_string_lossy().into_owned())
		.unwrap_or_else(|| config.command.clone());
	let wrapped = lspmux::get_lspmux_command(&base_command, &config.args).await;
	// Prefer the broker-shared server unless an external `lspmux` wrapper is
	// already multiplexing this command. Any shared-path failure falls back to a
	// private spawn so LSP never regresses on broker trouble.
	let shared = if shared_mux && !wrapped.shared {
		let env = if wrapped.env.is_empty() {
			None
		} else {
			Some(wrapped.env.iter().cloned().collect::<BTreeMap<String, String>>())
		};
		mux::client::connect(
			cwd,
			&MuxConnectParams {
				command: wrapped.command.clone(),
				args: wrapped.args.clone(),
				cwd: cwd.to_string_lossy().into_owned(),
				env,
			},
		)
		.await
	} else {
		None
	};
	let (pid, shared_mux, writer, reader, leftover, child, stderr) = if let Some(link) = shared {
		(
			link.pid,
			true,
			ClientWriter::Shared(link.writer),
			ClientReader::Shared(link.reader),
			link.leftover,
			None,
			None,
		)
	} else {
		let mut command = Command::new(&wrapped.command);
		command
			.args(&wrapped.args)
			.current_dir(cwd)
			.stdin(std::process::Stdio::piped())
			.stdout(std::process::Stdio::piped())
			.stderr(std::process::Stdio::piped())
			.kill_on_drop(true);
		for (key, value) in &wrapped.env {
			command.env(key, value);
		}
		#[cfg(unix)]
		command.process_group(0);

		let mut child = command.spawn().map_err(|error| {
			LspError::Message(format!("Failed to spawn LSP server '{}': {error}", wrapped.command))
		})?;
		let pid = child.id().ok_or_else(|| LspError::Message("LSP server has no pid".to_string()))?;
		let stdin = child.stdin.take();
		let stdout = child
			.stdout
			.take()
			.ok_or_else(|| LspError::Message("Failed to capture LSP stdout".to_string()))?;
		let stderr = child
			.stderr
			.take()
			.ok_or_else(|| LspError::Message("Failed to capture LSP stderr".to_string()))?;
		(
			pid,
			wrapped.shared,
			ClientWriter::Child(stdin.ok_or_else(|| LspError::Message("Failed to capture LSP stdin".to_string()))?),
			ClientReader::Child(stdout),
			Vec::new(),
			Some(child),
			Some(stderr),
		)
	};
	let (exit_tx, exit_rx) = watch::channel(false);
	let (project_tx, project_rx) = watch::channel(false);
	let (mux_restart_tx, mux_restart_rx) = tokio::sync::mpsc::unbounded_channel();
	let mux_restart = shared_mux.then_some(mux_restart_tx);

	let client = Arc::new(LspClient {
		name: key.to_string(),
		cwd: cwd.to_path_buf(),
		config: config.clone(),
		pid,
		shared_mux,
		stdin: tokio::sync::Mutex::new(Some(writer)),
		mux_restart,
		pending: Mutex::new(HashMap::new()),
		state: Mutex::new(ClientState {
			status: Some(ClientStatus::Connecting),
			..ClientState::default()
		}),
		stderr: Mutex::new(String::new()),
		exit_tx: exit_tx.clone(),
		exit_rx,
		project_tx: project_tx.clone(),
		project_rx,
		request_id: AtomicI64::new(0),
		last_activity_ms: AtomicU64::new(epoch_millis()),
		rust_ready: AtomicBool::new(false),
	});

	tokio::spawn(read_loop(client.clone(), reader, leftover));
	if let Some(stderr) = stderr {
		tokio::spawn(drain_stderr(stderr, client.clone()));
	}
	if let Some(child) = child {
		tokio::spawn(watch_child(child, client.clone(), exit_tx));
	}
	if shared_mux {
		let mut restart = mux_restart_rx;
		// Hold only a weak handle: the client owns the matching sender, so a
		// strong handle here would keep the client (and this task) alive forever.
		let client = Arc::downgrade(&client);
		tokio::spawn(async move {
			if restart.recv().await.is_none() {
				return;
			}
			let Some(client) = client.upgrade() else {
				return;
			};
			let message = json!({ "jsonrpc": "2.0", "method": mux::MUX_RESTART_METHOD });
			let mut guard = client.stdin.lock().await;
			if let Some(writer) = guard.as_mut() {
				let _ = writer.write_all(mux::frame(&message).as_bytes()).await;
				let _ = writer.flush().await;
			}
			guard.take();
		});
	}
	tokio::spawn({
		let project_tx = project_tx.clone();
		async move {
			tokio::time::sleep(Duration::from_millis(PROJECT_LOAD_TIMEOUT_MS)).await;
			let _ = project_tx.send(true);
		}
	});

	let init_params = json!({
		"processId": std::process::id(),
		"rootUri": file_to_uri(cwd),
		"rootPath": cwd.to_string_lossy(),
		"capabilities": client_capabilities(),
		"initializationOptions": config.init_options.clone().unwrap_or_else(|| json!({})),
		"workspaceFolders": current_workspace_folders(cwd),
	});
	let init_result = match client
		.send_request("initialize", init_params, cancel, init_timeout)
		.await
	{
		Ok(result) => result,
		Err(error) => {
			client.kill_process();
			return Err(error);
		}
	};
	let capabilities = init_result.get("capabilities").cloned();
	client.set_status(ClientStatus::Ready);
	lock(&client.state).capabilities = capabilities;
	if let Err(error) = client
		.send_notification("initialized", Some(json!({})), cancel)
		.await
	{
		client.kill_process();
		return Err(error);
	}
	if let Err(error) = client
		.send_notification(
			"workspace/didChangeConfiguration",
			Some(json!({ "settings": config.settings.clone().unwrap_or_else(|| json!({})) })),
			cancel,
		)
		.await
	{
		client.kill_process();
		return Err(error);
	}
	Ok(client)
}

fn current_workspace_folders(cwd: &Path) -> Value {
	let name = cwd
		.file_name()
		.map(|name| name.to_string_lossy().into_owned())
		.filter(|name| !name.is_empty())
		.unwrap_or_else(|| "workspace".to_string());
	json!([{ "uri": file_to_uri(cwd), "name": name }])
}

async fn drain_stderr(mut stderr: ChildStderr, client: Arc<LspClient>) {
	let mut chunk = [0u8; 4096];
	loop {
		match stderr.read(&mut chunk).await {
			Ok(0) | Err(_) => break,
			Ok(count) => {
				let text = String::from_utf8_lossy(&chunk[..count]);
				let mut buffer = lock(&client.stderr);
				buffer.push_str(&text);
				if buffer.len() > 16 * 1024 {
					let drain_to = buffer.len() - 16 * 1024;
					let boundary = buffer
						.char_indices()
						.find(|(index, _)| *index >= drain_to)
						.map(|(index, _)| index)
						.unwrap_or(buffer.len());
					buffer.drain(..boundary);
				}
			}
		}
	}
}

async fn watch_child(mut child: Child, client: Arc<LspClient>, exit_tx: watch::Sender<bool>) {
	let status = child.wait().await;
	let code = status.as_ref().ok().and_then(std::process::ExitStatus::code);
	client.set_status(ClientStatus::Error);
	client.reject_pending(&exit_error_message(&client, code));
	remove_client(&client);
	let _ = exit_tx.send(true);
	let _ = client.project_tx.send(true);
}

fn exit_error_message(client: &LspClient, code: Option<i32>) -> String {
	let stderr = client
		.stderr_tail()
		.lines()
		.filter(|line| {
			!line.trim_start().starts_with('[')
				|| !(line.contains(" INF]") || line.contains(" DBG]") || line.contains(" VRB]"))
		})
		.collect::<Vec<_>>()
		.join("\n");
	match (code, stderr.is_empty()) {
		(Some(code), false) => format!("LSP server exited (code {code}): {stderr}"),
		(Some(code), true) => format!("LSP server exited unexpectedly (code {code})"),
		(None, false) => format!("LSP server exited: {stderr}"),
		(None, true) => "LSP server exited unexpectedly".to_string(),
	}
}

async fn read_loop(client: Arc<LspClient>, mut stdout: ClientReader, leftover: Vec<u8>) {
	let mut framer = MessageFramer::from_remainder(leftover);
	let mut chunk = [0u8; 16 * 1024];
	let mut failure: Option<String> = None;
	let mut stream_closed = false;
	loop {
		let count = match stdout.read(&mut chunk).await {
			Ok(0) => {
				stream_closed = true;
				break;
			},
			Ok(count) => count,
			Err(error) => {
				failure = Some(error.to_string());
				break;
			}
		};
		framer.push(&chunk[..count]);
		let messages = match framer.drain(|header| {
			let trimmed = header.trim();
			if !trimmed.is_empty() {
				eprintln!("LSP framing resync on {}: {}", client.config.command, trimmed);
			}
		}) {
			Ok(messages) => messages,
			Err(FramingError::HeaderTooLarge) | Err(FramingError::ContentTooLarge(_)) => {
				failure = Some("LSP framing bounds exceeded".to_string());
				break;
			}
		};
		for message in messages {
			let Ok(value) = serde_json::from_str::<Value>(&message) else {
				continue;
			};
			handle_message(&client, value).await;
		}
	}

	client.set_status(ClientStatus::Error);
	let error = failure.unwrap_or_else(|| "LSP connection closed".to_string());
	client.reject_pending(&format!("LSP connection closed: {error}"));
	if stream_closed && client.mux_restart.is_some() {
		let _ = client.exit_tx.send(true);
	}
	let _ = client.project_tx.send(true);
	// Give the normal exit watcher a moment; a reader that dies while the
	// process lives means no response can ever be routed again, so tear it down.
	tokio::time::sleep(Duration::from_millis(READER_EXIT_GRACE_MS)).await;
	if !*client.exit_rx.borrow() {
		client.kill_process();
	}
	// A shared link has no child watcher: the socket reader owns exit
	// notification, including after the framing-error teardown above.
	if client.mux_restart.is_some() {
		let _ = client.exit_tx.send(true);
	}
	remove_client(&client);
}

async fn handle_message(client: &Arc<LspClient>, message: Value) {
	let Some(object) = message.as_object() else {
		return;
	};
	if let Some(method) = object.get("method").and_then(Value::as_str) {
		if let Some(id) = object.get("id").filter(|id| !id.is_null()).cloned() {
			handle_server_request(client, id, method, object.get("params").cloned().unwrap_or(Value::Null))
				.await;
		} else {
			handle_server_notification(client, method, object.get("params").cloned().unwrap_or(Value::Null));
		}
		return;
	}
	let Some(id) = object.get("id").and_then(Value::as_i64) else {
		return;
	};
	let Some(sender) = lock(&client.pending).remove(&id) else {
		return;
	};
	if let Some(error) = object.get("error") {
		let code = error.get("code").and_then(Value::as_i64).unwrap_or(-1);
		let message = error
			.get("message")
			.and_then(Value::as_str)
			.unwrap_or("unknown JSON-RPC error");
		let _ = sender.send(Err(LspError::Message(format!("LSP error {code}: {message}"))));
	} else {
		let _ = sender.send(Ok(object.get("result").cloned().unwrap_or(Value::Null)));
	}
}

fn handle_server_notification(client: &Arc<LspClient>, method: &str, params: Value) {
	match method {
		"textDocument/publishDiagnostics" => {
			if let Ok(params) = serde_json::from_value::<PublishDiagnosticsParams>(params) {
				let mut state = lock(&client.state);
				state.diagnostics.insert(
					uri_key(&params.uri),
					PublishedDiagnostics { diagnostics: params.diagnostics, version: params.version },
				);
				state.diagnostics_version = state.diagnostics_version.wrapping_add(1);
			}
		}
		"$/progress" => {
			let token = params.get("token").map(|token| token.to_string()).filter(|token| !token.is_empty());
			let kind = params.get("value").and_then(|value| value.get("kind")).and_then(Value::as_str);
			if let Some(token) = token {
				let mut state = lock(&client.state);
				match kind {
					Some("begin") => {
						state.progress_tokens.insert(token);
					}
					Some("end") => {
						state.progress_tokens.remove(&token);
						if state.progress_tokens.is_empty() {
							drop(state);
							let _ = client.project_tx.send(true);
						}
					}
					_ => {}
				}
			}
		}
		_ => {}
	}
}

async fn handle_server_request(
	client: &Arc<LspClient>,
	id: Value,
	method: &str,
	params: Value,
) {
	match method {
		"workspace/configuration" => {
			let items = params.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
			let result: Vec<Value> = items
				.iter()
				.map(|item| {
					let section = item.get("section").and_then(Value::as_str).unwrap_or("");
					client
						.config
						.settings
						.as_ref()
						.and_then(|settings| settings.as_object())
						.and_then(|settings| settings.get(section))
						.cloned()
						.unwrap_or(Value::Null)
				})
				.collect();
			client.send_response(id, Some(Value::Array(result)), None).await;
		}
		"workspace/workspaceFolders" => {
			client.send_response(id, Some(current_workspace_folders(&client.cwd)), None).await;
		}
		"workspace/applyEdit" => {
			let edit = params.get("edit").cloned();
			let Some(edit) = edit else {
				client
					.send_response(
						id,
						Some(json!({ "applied": false, "failureReason": "No edit provided" })),
						None,
					)
					.await;
				return;
			};
			let parsed: Result<super::types::WorkspaceEdit, _> = serde_json::from_value(edit);
			match parsed {
				Ok(edit) => match apply_workspace_edit_with_lsp(&edit, &client.cwd, &CancellationToken::new()).await {
					Ok(_) => client.send_response(id, Some(json!({ "applied": true })), None).await,
					Err(error) => {
						client
							.send_response(
								id,
								Some(json!({ "applied": false, "failureReason": error })),
								None,
							)
							.await;
					}
				},
				Err(error) => {
					client
						.send_response(
							id,
							Some(json!({ "applied": false, "failureReason": error.to_string() })),
							None,
						)
						.await;
				}
			}
		}
		"window/workDoneProgress/create"
		| "client/registerCapability"
		| "client/unregisterCapability"
		| "window/showMessageRequest"
		| "workspace/semanticTokens/refresh"
		| "workspace/inlayHint/refresh"
		| "workspace/codeLens/refresh"
		| "workspace/codeAction/refresh"
		| "workspace/inlineValue/refresh"
		| "workspace/foldingRange/refresh"
		| "workspace/diagnostic/refresh" => {
			if method == "client/registerCapability" {
				if let Some(registrations) = params.get("registrations").and_then(Value::as_array) {
					let mut state = lock(&client.state);
					for registration in registrations {
						if let (Some(id), Some(method)) = (
							registration.get("id").and_then(Value::as_str),
							registration.get("method").and_then(Value::as_str),
						) {
							state.dynamic_registrations.insert(id.to_string(), method.to_string());
						}
					}
				}
			} else if method == "client/unregisterCapability" {
				let unregister = params
					.get("unregisterations")
					.or_else(|| params.get("unregistrations"))
					.and_then(Value::as_array);
				if let Some(unregister) = unregister {
					let mut state = lock(&client.state);
					for registration in unregister {
						if let Some(id) = registration.get("id").and_then(Value::as_str) {
							state.dynamic_registrations.remove(id);
						}
					}
				}
			}
			let result = match method {
				"window/showDocument" => json!({ "success": false }),
				"window/showMessageRequest" => Value::Null,
				_ => Value::Null,
			};
			client.send_response(id, Some(result), None).await;
		}
		_ => {
			client
				.send_response(
					id,
					None,
					Some(JsonRpcError { code: -32601, message: format!("Method not found: {method}"), data: None }),
				)
				.await;
		}
	}
}

fn client_capabilities() -> Value {
	json!({
		"textDocument": {
			"synchronization": {
				"didSave": true,
				"dynamicRegistration": false,
				"willSave": false,
				"willSaveWaitUntil": false
			},
			"hover": { "contentFormat": ["markdown", "plaintext"], "dynamicRegistration": false },
			"definition": { "dynamicRegistration": false, "linkSupport": true },
			"typeDefinition": { "dynamicRegistration": false, "linkSupport": true },
			"implementation": { "dynamicRegistration": false, "linkSupport": true },
			"references": { "dynamicRegistration": false },
			"documentSymbol": {
				"dynamicRegistration": false,
				"hierarchicalDocumentSymbolSupport": true,
				"symbolKind": { "valueSet": (1..=26).collect::<Vec<_>>() }
			},
			"rename": { "dynamicRegistration": false, "prepareSupport": true },
			"codeAction": {
				"dynamicRegistration": false,
				"codeActionLiteralSupport": {
					"codeActionKind": {
						"valueSet": [
							"quickfix", "refactor", "refactor.extract", "refactor.inline",
							"refactor.rewrite", "source", "source.organizeImports", "source.fixAll"
						]
					}
				},
				"resolveSupport": { "properties": ["edit"] }
			},
			"formatting": { "dynamicRegistration": false },
			"rangeFormatting": { "dynamicRegistration": false },
			"publishDiagnostics": {
				"relatedInformation": true,
				"versionSupport": true,
				"tagSupport": { "valueSet": [1, 2] },
				"codeDescriptionSupport": true,
				"dataSupport": true
			},
			"diagnostic": { "dynamicRegistration": true }
		},
		"window": { "workDoneProgress": true },
		"workspace": {
			"applyEdit": true,
			"workspaceEdit": {
				"documentChanges": true,
				"resourceOperations": ["create", "rename", "delete"],
				"failureHandling": "abort"
			},
			"configuration": true,
			"workspaceFolders": true,
			"symbol": {
				"dynamicRegistration": false,
				"symbolKind": { "valueSet": (1..=26).collect::<Vec<_>>() }
			},
			"fileOperations": {
				"dynamicRegistration": false,
				"willCreate": false,
				"didCreate": false,
				"willRename": true,
				"didRename": true,
				"willDelete": false,
				"didDelete": false
			}
		}
	})
}

// =============================================================================
// File synchronization
// =============================================================================

async fn file_lock(key: &str) -> tokio::sync::OwnedMutexGuard<()> {
	let lock = {
		let mut locks = lock(&registry().file_locks);
		locks
			.entry(key.to_string())
			.or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
			.clone()
	};
	lock.lock_owned().await
}

pub async fn ensure_file_open(
	client: &Arc<LspClient>,
	file_path: &Path,
	cancel: &CancellationToken,
) -> Result<(), LspError> {
	if cancel.is_cancelled() {
		return Err(LspError::Cancelled);
	}
	let uri = file_to_uri(file_path);
	let key = uri_key(&uri);
	if client.has_open_file(&uri) {
		return Ok(());
	}
	let lock_key = format!("{}:{key}", client.name);
	let _guard = file_lock(&lock_key).await;
	if client.has_open_file(&uri) {
		return Ok(());
	}
	let content = match tokio::fs::read_to_string(file_path).await {
		Ok(content) => content,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
		Err(error) => return Err(LspError::Message(error.to_string())),
	};
	let language_id = client
		.config
		.language_id
		.clone()
		.unwrap_or_else(|| super::utils::detect_language_id(file_path));
	client
		.send_notification(
			"textDocument/didOpen",
			Some(json!({
				"textDocument": {
					"uri": uri,
					"languageId": language_id,
					"version": 1,
					"text": content
				}
			})),
			cancel,
		)
		.await?;
	lock(&client.state)
		.open_files
		.insert(key, OpenFile { version: 1, language_id });
	Ok(())
}

pub async fn sync_content(
	client: &Arc<LspClient>,
	file_path: &Path,
	content: &str,
	cancel: &CancellationToken,
) -> Result<(), LspError> {
	let uri = file_to_uri(file_path);
	let key = uri_key(&uri);
	if cancel.is_cancelled() {
		return Err(LspError::Cancelled);
	}
	let lock_key = format!("{}:{key}", client.name);
	let _guard = file_lock(&lock_key).await;
	lock(&client.state).diagnostics.remove(&key);
	let existing = lock(&client.state).open_files.get(&key).cloned();
	if let Some(info) = existing {
		let version = info.version + 1;
		client
			.send_notification(
				"textDocument/didChange",
				Some(json!({
					"textDocument": { "uri": uri, "version": version },
					"contentChanges": [{ "text": content }]
				})),
				cancel,
			)
			.await?;
		if let Some(file) = lock(&client.state).open_files.get_mut(&key) {
			file.version = version;
		}
	} else {
		let language_id = client
			.config
			.language_id
			.clone()
			.unwrap_or_else(|| super::utils::detect_language_id(file_path));
		client
			.send_notification(
				"textDocument/didOpen",
				Some(json!({
					"textDocument": {
						"uri": uri,
						"languageId": language_id,
						"version": 1,
						"text": content
					}
				})),
				cancel,
			)
			.await?;
		lock(&client.state)
			.open_files
			.insert(key, OpenFile { version: 1, language_id });
	}
	Ok(())
}

pub async fn notify_saved(
	client: &Arc<LspClient>,
	file_path: &Path,
	cancel: &CancellationToken,
) -> Result<(), LspError> {
	let uri = file_to_uri(file_path);
	if !client.has_open_file(&uri) {
		return Ok(());
	}
	client
		.send_notification(
			"textDocument/didSave",
			Some(json!({ "textDocument": { "uri": uri } })),
			cancel,
		)
		.await
}

pub async fn refresh_file(
	client: &Arc<LspClient>,
	file_path: &Path,
	cancel: &CancellationToken,
) -> Result<(), LspError> {
	let uri = file_to_uri(file_path);
	let key = uri_key(&uri);
	let lock_key = format!("{}:{key}", client.name);
	if cancel.is_cancelled() {
		return Err(LspError::Cancelled);
	}
	let _guard = file_lock(&lock_key).await;
	lock(&client.state).diagnostics.remove(&key);
	let Some(info) = lock(&client.state).open_files.get(&key).cloned() else {
		drop(_guard);
		return ensure_file_open(client, file_path, cancel).await;
	};
	let content = match tokio::fs::read_to_string(file_path).await {
		Ok(content) => content,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
		Err(error) => return Err(LspError::Message(error.to_string())),
	};
	let version = info.version + 1;
	client
		.send_notification(
			"textDocument/didChange",
			Some(json!({
				"textDocument": { "uri": uri, "version": version },
				"contentChanges": [{ "text": content }]
			})),
			cancel,
		)
		.await?;
	client
		.send_notification(
			"textDocument/didSave",
			Some(json!({ "textDocument": { "uri": uri }, "text": content })),
			cancel,
		)
		.await?;
	if let Some(file) = lock(&client.state).open_files.get_mut(&key) {
		file.version = version;
	}
	Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileChangeType {
	Created = 1,
	Changed = 2,
	Deleted = 3,
}

pub async fn notify_workspace_watched_files(
	cwd: &Path,
	changes: &[(PathBuf, FileChangeType)],
	cancel: &CancellationToken,
) {
	if changes.is_empty() || cancel.is_cancelled() {
		return;
	}
	let workspace = normalize_cwd(cwd);
	let active: Vec<Arc<LspClient>> = lock(&registry().clients)
		.values()
		.filter(|client| {
			client.status() == ClientStatus::Ready && normalize_cwd(&client.cwd) == workspace
		})
		.cloned()
		.collect();
	if active.is_empty() {
		return;
	}
	let timeout = CancellationToken::new();
	let timeout_task = {
		let timeout = timeout.clone();
		tokio::spawn(async move {
			tokio::time::sleep(Duration::from_millis(2_000)).await;
			timeout.cancel();
		})
	};
	for client in active {
		let client_changes: Vec<Value> = changes
			.iter()
			.filter(|(path, _)| super::utils::is_path_within(path, cwd))
			.map(|(path, change)| {
				let uri = file_to_uri(path);
				// A watcher event means any published diagnostics for that URI
				// describe pre-change content; the server will republish.
				lock(&client.state).diagnostics.remove(&uri_key(&uri));
				json!({ "uri": uri, "type": *change as u8 })
			})
			.collect();
		if client_changes.is_empty() {
			continue;
		}
		let combined = CancellationToken::new();
		let combined_task = {
			let combined = combined.clone();
			let cancel = cancel.clone();
			let timeout = timeout.clone();
			tokio::spawn(async move {
				tokio::select! {
					_ = cancel.cancelled() => combined.cancel(),
					_ = timeout.cancelled() => combined.cancel(),
				}
			})
		};
		let _ = client
			.send_notification(
				"workspace/didChangeWatchedFiles",
				Some(json!({ "changes": client_changes })),
				&combined,
			)
			.await;
		combined_task.abort();
	}
	timeout_task.abort();
}

// =============================================================================
// Workspace edit reconciliation
// =============================================================================

pub async fn apply_workspace_edit_with_lsp(
	edit: &super::types::WorkspaceEdit,
	cwd: &Path,
	cancel: &CancellationToken,
) -> Result<Vec<String>, String> {
	let mut executed: Vec<ExecutedWorkspaceChange> = Vec::new();
	let applied = match apply_workspace_edit(edit, cwd, |change| executed.push(change.clone())).await {
		Ok(result) => result.applied,
		Err(error) => {
			let _ = reconcile_executed_changes(&executed, cwd, cancel).await;
			return Err(error);
		}
	};
	reconcile_executed_changes(&executed, cwd, cancel).await?;
	Ok(applied)
}

fn uri_is_within(uri: &str, root: &str) -> bool {
	uri == root || uri.starts_with(&format!("{}/", root.trim_end_matches('/')))
}

async fn reconcile_executed_changes(
	executed: &[ExecutedWorkspaceChange],
	cwd: &Path,
	cancel: &CancellationToken,
) -> Result<(), String> {
	if executed.is_empty() {
		return Ok(());
	}
	let mut final_uris = HashSet::new();
	let mut deleted_roots = HashSet::new();
	let mut watched = Vec::new();
	for change in executed {
		match change {
			ExecutedWorkspaceChange::Edit { uri } => {
				final_uris.insert(uri.clone());
				watched.push((uri_to_file(uri), FileChangeType::Changed));
			}
			ExecutedWorkspaceChange::Create { uri } => {
				final_uris.insert(uri.clone());
				watched.push((uri_to_file(uri), FileChangeType::Created));
			}
			ExecutedWorkspaceChange::Rename { old_uri, new_uri } => {
				deleted_roots.insert(old_uri.clone());
				final_uris.insert(new_uri.clone());
				watched.push((uri_to_file(old_uri), FileChangeType::Deleted));
				watched.push((uri_to_file(new_uri), FileChangeType::Created));
			}
			ExecutedWorkspaceChange::Delete { uri } => {
				deleted_roots.insert(uri.clone());
				watched.push((uri_to_file(uri), FileChangeType::Deleted));
			}
		}
	}
	let workspace = normalize_cwd(cwd);
	let active: Vec<Arc<LspClient>> = lock(&registry().clients)
		.values()
		.filter(|client| {
			client.status() == ClientStatus::Ready && normalize_cwd(&client.cwd) == workspace
		})
		.cloned()
		.collect();
	for client in active {
		let open_keys: Vec<String> = lock(&client.state).open_files.keys().cloned().collect();
		for key in open_keys {
			let deleted = deleted_roots
				.iter()
				.any(|root| uri_is_within(&key, &uri_key(root)));
			if !deleted {
				continue;
			}
			let uri = file_to_uri(Path::new(&key));
			let _ = client
				.send_notification(
					"textDocument/didClose",
					Some(json!({ "textDocument": { "uri": uri } })),
					cancel,
				)
				.await;
			lock(&client.state).open_files.remove(&key);
			lock(&client.state).diagnostics.remove(&key);
		}
		for uri in &final_uris {
			if !client.has_open_file(uri) {
				continue;
			}
			let _ = refresh_file(&client, &uri_to_file(uri), cancel).await;
		}
	}
	notify_workspace_watched_files(cwd, &watched, cancel).await;
	Ok(())
}

// =============================================================================
// Project readiness
// =============================================================================

pub async fn wait_for_project_loaded(
	client: &Arc<LspClient>,
	cancel: &CancellationToken,
	target_file: Option<&Path>,
) {
	if cancel.is_cancelled() {
		return;
	}
	// rust-analyzer only reports workspaces when the target lives under an
	// actual Rust workspace; polling analyzerStatus outside one burns the
	// settle budget on every request (the status command waits for a
	// workspace that will never load). Skip both the readiness wait and the
	// poll for such targets. `None` keeps the client-scope check used by the
	// references retry path.
	let rust_workspace = client.is_rust_analyzer()
		&& target_file.map_or_else(
			|| super::diagnostics::has_rust_workspace_ancestor(&client.cwd.join("Cargo.toml")),
			super::diagnostics::has_rust_workspace_ancestor,
		);
	if client.is_rust_analyzer() && !rust_workspace {
		return;
	}
	if !*client.project_rx.borrow() {
		let mut rx = client.project_rx.clone();
		tokio::select! {
			_ = cancel.cancelled() => return,
			_ = rx.wait_for(|loaded| *loaded) => {}
		}
	}
	if rust_workspace {
		wait_for_rust_analyzer_workspace(client, cancel).await;
	}
}

async fn wait_for_rust_analyzer_workspace(client: &Arc<LspClient>, cancel: &CancellationToken) {
	if client.rust_ready.load(Ordering::Acquire) {
		return;
	}
	let timings = client.config.workspace_ready_timings.clone().unwrap_or_default();
	let timeout_ms = timings.timeout_ms.unwrap_or(5_000);
	let poll_ms = timings.poll_ms.unwrap_or(100);
	let settle_ms = timings.settle_ms.unwrap_or(2_000);
	let status_timeout_ms = timings.status_request_timeout_ms.unwrap_or(1_000);
	let started = Instant::now();
	let deadline = started + Duration::from_millis(timeout_ms);
	loop {
		if cancel.is_cancelled() {
			return;
		}
		let status = client
			.send_request(
				"rust-analyzer/analyzerStatus",
				json!({}),
				cancel,
				Some(Duration::from_millis(status_timeout_ms)),
			)
			.await;
		match status {
			Ok(Value::String(status)) => {
				let ready = !status.starts_with("No workspaces");
				if ready && started.elapsed() >= Duration::from_millis(settle_ms) {
					client.rust_ready.store(true, Ordering::Release);
					return;
				}
			}
			Ok(_) => {}
			Err(LspError::Cancelled) => return,
			Err(LspError::Timeout { .. }) => {
				// The analyzer is busy indexing: a status-request timeout is the
				// expected signal to poll again until the workspace deadline.
				if Instant::now() >= deadline {
					return;
				}
			},
			Err(_) => return,
		}
		if Instant::now() >= deadline {
			return;
		}
		tokio::select! {
			_ = cancel.cancelled() => return,
			_ = tokio::time::sleep(Duration::from_millis(poll_ms)) => {}
		}
	}
}

#[must_use]
pub fn supports_document_diagnostics(client: &LspClient) -> bool {
	let state = lock(&client.state);
	let static_provider = state
		.capabilities
		.as_ref()
		.and_then(|capabilities| capabilities.get("diagnosticProvider"))
		.is_some_and(|provider| !provider.is_null() && provider != &Value::Bool(false));
	static_provider
		|| state
			.dynamic_registrations
			.values()
			.any(|method| method == "textDocument/diagnostic")
}

// =============================================================================
// Diagnostics wait
// =============================================================================

pub struct WaitDiagnosticsOptions {
	pub timeout: Duration,
	pub min_version: Option<u64>,
	pub expected_document_version: Option<i64>,
	pub settle: Duration,
}

impl Default for WaitDiagnosticsOptions {
	fn default() -> Self {
		Self {
			timeout: Duration::from_millis(3_000),
			min_version: None,
			expected_document_version: None,
			settle: Duration::from_millis(250),
		}
	}
}

pub async fn wait_for_diagnostics(
	client: &Arc<LspClient>,
	uri: &str,
	options: WaitDiagnosticsOptions,
	cancel: &CancellationToken,
) -> Result<Vec<Diagnostic>, LspError> {
	let deadline = Instant::now() + options.timeout;
	let mut pull_attempted = false;
	let mut pulled: Option<Vec<Diagnostic>> = None;
	let mut settled: Option<PublishedDiagnostics> = None;
	let mut settled_at = Instant::now();
	while Instant::now() < deadline {
		if cancel.is_cancelled() {
			return Err(LspError::Cancelled);
		}
		if !pull_attempted && supports_document_diagnostics(client) {
			pull_attempted = true;
			let remaining = deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
			if let Ok(report) = client
				.send_request(
					"textDocument/diagnostic",
					json!({ "textDocument": { "uri": uri } }),
					cancel,
					Some(remaining),
				)
				.await
			{
				if report.get("kind").and_then(Value::as_str) == Some("full") {
					if let Some(items) = report.get("items").and_then(Value::as_array) {
						pulled = Some(
							items
								.iter()
								.filter_map(|item| serde_json::from_value::<Diagnostic>(item.clone()).ok())
								.collect(),
						);
					}
				}
			}
		}

		let version_ok = options
			.min_version
			.is_none_or(|min_version| client.diagnostics_version() > min_version);
		let published = client.diagnostics_for(uri);
		if let Some(published) = &published {
			if version_ok {
				if options
					.expected_document_version
					.is_some_and(|expected| published.version == Some(expected))
				{
					return Ok(published.diagnostics.clone());
				}
				let same_reference = settled.as_ref().is_some_and(|settled| {
					settled.version == published.version && settled.diagnostics == published.diagnostics
				});
				if !same_reference {
					settled = Some(published.clone());
					settled_at = Instant::now();
				} else if settled_at.elapsed() >= options.settle {
					return Ok(published.diagnostics.clone());
				}
			}
		}
		if pulled.is_some() && published.is_none() {
			// A full pull is authoritative when no publishDiagnostics arrived for
			// this refresh; do not burn the remaining budget waiting for one.
			break;
		}
		let remaining = deadline.saturating_duration_since(Instant::now());
		if remaining.is_zero() {
			break;
		}
		tokio::select! {
			_ = cancel.cancelled() => return Err(LspError::Cancelled),
			_ = tokio::time::sleep(remaining.min(Duration::from_millis(100))) => {}
		}
	}

	if cancel.is_cancelled() {
		return Err(LspError::Cancelled);
	}
	let version_ok = options
		.min_version
		.is_none_or(|min_version| client.diagnostics_version() > min_version);
	if let Some(published) = client.diagnostics_for(uri) {
		if version_ok {
			return Ok(published.diagnostics);
		}
	}
	if let Some(pulled) = pulled {
		let mut state = lock(&client.state);
		let version = options
			.expected_document_version
			.or_else(|| state.open_files.get(&uri_key(uri)).map(|file| file.version));
		state
			.diagnostics
			.insert(uri_key(uri), PublishedDiagnostics { diagnostics: pulled.clone(), version });
		state.diagnostics_version = state.diagnostics_version.wrapping_add(1);
		return Ok(pulled);
	}
	Ok(Vec::new())
}

// =============================================================================
// Shutdown and reload
// =============================================================================

pub async fn shutdown_client_instance(client: &Arc<LspClient>) -> bool {
	remove_client(client);
	client.reject_pending("LSP client shutdown");
	let shutdown_token = CancellationToken::new();
	let shutdown_ok = client
		.send_request(
			"shutdown",
			Value::Null,
			&shutdown_token,
			Some(Duration::from_millis(SHUTDOWN_TIMEOUT_MS)),
		)
		.await
		.is_ok();
	if shutdown_ok {
		let _ = client.send_notification("exit", None, &shutdown_token).await;
		if client.wait_for_exit(Duration::from_millis(EXIT_TIMEOUT_MS)).await {
			return true;
		}
	}
	client.kill_process();
	client.wait_for_exit(Duration::from_millis(EXIT_TIMEOUT_MS)).await
}

pub async fn shutdown_stale_clients(
	cwd: &Path,
	configs: &[ServerConfig],
	cancel: &CancellationToken,
	shared_mux: bool,
) -> Result<Vec<String>, LspError> {
	let fresh: HashSet<String> = configs.iter().map(|config| client_key(config, cwd, shared_mux)).collect();
	let workspace = normalize_cwd(cwd);
	let stale: Vec<Arc<LspClient>> = lock(&registry().clients)
		.values()
		.filter(|client| normalize_cwd(&client.cwd) == workspace && !fresh.contains(&client.name))
		.cloned()
		.collect();
	let mut stopped = Vec::new();
	for client in stale {
		if cancel.is_cancelled() {
			return Err(LspError::Cancelled);
		}
		let command = client.config.command.clone();
		if shutdown_client_instance(&client).await {
			stopped.push(command);
		} else {
			return Err(LspError::Message(format!(
				"Failed to stop LSP server with superseded configuration: {command}"
			)));
		}
	}
	Ok(stopped)
}

pub async fn shutdown_all() {
	let clients: Vec<Arc<LspClient>> = lock(&registry().clients).values().cloned().collect();
	lock(&registry().clients).clear();
	// Owners die with the registry: a later claim must not keep a dead client.
	lock(&registry().owners).clear();
	for client in clients {
		let _ = shutdown_client_instance(&client).await;
	}
}

pub fn clear_initialization_failure(config: &ServerConfig, cwd: &Path, shared_mux: bool) {
	lock(&registry().failures).remove(&client_key(config, cwd, shared_mux));
}

#[must_use]
pub fn get_active_clients() -> Vec<ServerStatus> {
	lock(&registry().clients)
		.values()
		.map(|client| ServerStatus {
			name: client.config.command.clone(),
			status: client.status(),
			file_types: client.config.file_types.clone(),
		})
		.collect()
}

#[derive(Debug, Clone)]
pub struct ServerStatus {
	pub name: String,
	pub status: ClientStatus,
	pub file_types: Vec<String>,
}

pub async fn reload_server(
	client: &Arc<LspClient>,
	server_name: &str,
	cancel: &CancellationToken,
) -> Result<String, LspError> {
	if cancel.is_cancelled() {
		return Err(LspError::Cancelled);
	}
	if client.is_rust_analyzer() || server_name == "rust-analyzer" {
		match client
			.send_request("rust-analyzer/reloadWorkspace", Value::Null, cancel, None)
			.await
		{
			Ok(_) => return Ok(format!("Reloaded {server_name}")),
			Err(LspError::Cancelled) => return Err(LspError::Cancelled),
			Err(error) => {
				if !is_method_not_found_error(&error.message()) {
					return Err(error);
				}
			}
		}
	}
	let settings = client.config.settings.clone().unwrap_or_else(|| json!({}));
	match client
		.send_notification(
			"workspace/didChangeConfiguration",
			Some(json!({ "settings": settings })),
			cancel,
		)
		.await
	{
		Ok(()) => Ok(format!("Reloaded {server_name}")),
		Err(LspError::Cancelled) => Err(LspError::Cancelled),
		Err(_) => {
			if shutdown_client_instance(client).await {
				Ok(format!("Restarted {server_name}"))
			} else {
				Err(LspError::Message(format!(
					"Failed to restart {server_name}: server process did not exit after kill"
				)))
			}
		}
	}
}

// =============================================================================
// Config cache and idle shutdown
// =============================================================================

static IDLE_TIMEOUT_MS: AtomicU64 = AtomicU64::new(0);
static IDLE_CHECKER_STARTED: AtomicBool = AtomicBool::new(false);

pub fn set_idle_timeout(timeout_ms: Option<u64>) {
	IDLE_TIMEOUT_MS.store(timeout_ms.unwrap_or(0), Ordering::Relaxed);
	if timeout_ms.is_some_and(|value| value > 0)
		&& tokio::runtime::Handle::try_current().is_ok()
		&& !IDLE_CHECKER_STARTED.swap(true, Ordering::SeqCst)
	{
		tokio::spawn(async {
			loop {
				tokio::time::sleep(Duration::from_secs(60)).await;
				let timeout = IDLE_TIMEOUT_MS.load(Ordering::Relaxed);
				if timeout == 0 {
					continue;
				}
				let candidates: Vec<Arc<LspClient>> = lock(&registry().clients).values().cloned().collect();
				for client in candidates {
					if client.pending_count() > 0 {
						continue;
					}
					if client.idle_for() > Duration::from_millis(timeout) {
						let _ = shutdown_client_instance(&client).await;
					}
				}
			}
		});
	}
}

static CONFIG_CACHE: LazyLock<Mutex<HashMap<String, Arc<LspConfig>>>> =
	LazyLock::new(|| Mutex::new(HashMap::new()));

fn config_cache() -> &'static Mutex<HashMap<String, Arc<LspConfig>>> {
	&CONFIG_CACHE
}

/// Load and cache configuration per cwd. Workspace `reload` invalidates it.
#[must_use]
pub fn get_config(cwd: &Path) -> Arc<LspConfig> {
	let key = normalize_cwd(cwd);
	if let Some(config) = lock(config_cache()).get(&key).cloned() {
		set_idle_timeout(config.idle_timeout_ms);
		return config;
	}
	let config = Arc::new(load_config(cwd));
	lock(config_cache()).insert(key, config.clone());
	set_idle_timeout(config.idle_timeout_ms);
	config
}

pub fn invalidate_config(cwd: &Path) {
	lock(config_cache()).remove(&normalize_cwd(cwd));
}

/// Select the primary non-custom server for `file_path` under the cached config.
#[must_use]
pub fn server_for_file(config: &LspConfig, file_path: &Path) -> Option<(String, ServerConfig)> {
	get_lsp_server_for_file(config, file_path)
}

/// All configured non-custom servers.
#[must_use]
pub fn lsp_servers(config: &LspConfig) -> Vec<(String, ServerConfig)> {
	get_lsp_servers(config)
}

/// A diagnostics-only helper retained for callers that format a path.
#[must_use]
pub fn display_path(path: &Path, cwd: &Path) -> String {
	format_path_relative_to_cwd(path, cwd)
}
