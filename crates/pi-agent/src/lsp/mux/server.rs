//! Broker-owned, multi-link LSP multiplexer.
//!
//! Port of `packages/coding-agent/src/lsp/mux/server.ts`. Each accepted Unix
//! socket is a *session*; a session binds once via [`MUX_CONNECT_METHOD`] to a
//! server instance keyed by command/args/cwd/env. Several sessions may share
//! one server: initialize results, dynamic registrations, diagnostics, and
//! progress are cached and replayed to late joiners, while requests are
//! relayed with mux-owned ids so concurrent links never collide.
//!
//! Lifecycle bounds mirror upstream: an idle server lingers 5 minutes for
//! reuse, and the whole mux idles out after 15 minutes with no sessions.

use std::{
	collections::{HashMap, HashSet},
	process::Stdio,
	sync::{
		Arc, Mutex, MutexGuard,
		atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
	},
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt},
	net::{UnixListener, UnixStream, unix::OwnedWriteHalf},
	process::{Child, ChildStdin, ChildStdout, Command},
	sync::{mpsc, oneshot, watch},
	task::JoinHandle,
};

use super::{
	MUX_CONNECT_METHOD, MUX_PING_METHOD, MUX_PING_RESULT, MUX_RESTART_METHOD, MuxConnectParams,
	MuxConnectResult, frame, server_key,
};
use crate::lsp::framing::MessageFramer;

/// How long an unbound server child survives for reuse by a later link.
const SERVER_LINGER: Duration = Duration::from_secs(5 * 60);
/// How long the mux keeps running with no connected links.
const MUX_IDLE: Duration = Duration::from_secs(15 * 60);
/// Bound on the graceful `shutdown` request before killing a child.
const SHUTDOWN_BUDGET: Duration = Duration::from_millis(2_000);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
	mutex.lock().unwrap_or_else(|error| error.into_inner())
}

fn rpc_result(id: Value, result: Value) -> Value {
	json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
	json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn id_key(id: &Value) -> String {
	id.to_string()
}

fn method(message: &Value) -> Option<&str> {
	message.get("method").and_then(Value::as_str)
}

fn request_id(message: &Value) -> Option<Value> {
	message.get("id").cloned().filter(|id| !id.is_null())
}

fn text_document_uri(message: &Value) -> Option<String> {
	message
		.get("params")
		.and_then(|params| params.get("textDocument"))
		.and_then(|document| document.get("uri"))
		.and_then(Value::as_str)
		.map(str::to_string)
}

fn progress_token(message: &Value) -> Option<String> {
	message
		.get("params")
		.and_then(|params| params.get("token"))
		.map(id_key)
}

fn progress_kind(message: &Value) -> Option<&str> {
	message
		.get("params")
		.and_then(|params| params.get("value"))
		.and_then(|value| value.get("kind"))
		.and_then(Value::as_str)
}

// =============================================================================
// Session
// =============================================================================

struct ClientPending {
	relay: bool,
	server_id: Option<Value>,
}

struct Session {
	id: u64,
	out: mpsc::UnboundedSender<String>,
	close: watch::Sender<bool>,
	open_uris: Mutex<HashSet<String>>,
	forwarded: Mutex<HashMap<String, i64>>,
	client_pending: Mutex<HashMap<String, ClientPending>>,
	server: Mutex<Option<Arc<ServerInstance>>>,
	initialized: AtomicBool,
	closed: AtomicBool,
	last_activity: AtomicU64,
}

impl Session {
	fn send(&self, message: Value) {
		if !self.closed.load(Ordering::SeqCst) {
			let _ = self.out.send(frame(&message));
		}
	}
}

// =============================================================================
// Server instance
// =============================================================================

enum Pending {
	Forward {
		session: Arc<Session>,
		original_id: Value,
	},
	Initialize,
	Shutdown(oneshot::Sender<()>),
}

#[derive(Default)]
struct InitializeState {
	result: Option<Value>,
	cached: bool,
	in_flight: bool,
	waiters: Vec<(Arc<Session>, Value)>,
}

struct ServerInstance {
	key: String,
	pid: u32,
	next_id: AtomicI64,
	next_client_id: AtomicI64,
	sink: mpsc::UnboundedSender<String>,
	stopping: AtomicBool,
	exited: AtomicBool,
	sessions: Mutex<Vec<Arc<Session>>>,
	documents: Mutex<HashSet<String>>,
	diagnostics: Mutex<HashMap<String, Value>>,
	registrations: Mutex<Vec<Value>>,
	progress: Mutex<HashMap<String, Value>>,
	pending: Mutex<HashMap<i64, Pending>>,
	initialize: Mutex<InitializeState>,
	initialized_sent: AtomicBool,
	linger: Mutex<Option<JoinHandle<()>>>,
}

impl ServerInstance {
	fn new(key: String, pid: u32, sink: mpsc::UnboundedSender<String>) -> Self {
		Self {
			key,
			pid,
			next_id: AtomicI64::new(1),
			next_client_id: AtomicI64::new(1),
			sink,
			stopping: AtomicBool::new(false),
			exited: AtomicBool::new(false),
			sessions: Mutex::new(Vec::new()),
			documents: Mutex::new(HashSet::new()),
			diagnostics: Mutex::new(HashMap::new()),
			registrations: Mutex::new(Vec::new()),
			progress: Mutex::new(HashMap::new()),
			pending: Mutex::new(HashMap::new()),
			initialize: Mutex::new(InitializeState::default()),
			initialized_sent: AtomicBool::new(false),
			linger: Mutex::new(None),
		}
	}

	fn mux_id(&self) -> i64 {
		self.next_id.fetch_add(1, Ordering::SeqCst)
	}

	fn client_id(&self) -> String {
		format!("mux:{}", self.next_client_id.fetch_add(1, Ordering::SeqCst))
	}

	fn send(&self, message: Value) {
		let _ = self.sink.send(frame(&message));
	}

	fn is_stopping(&self) -> bool {
		self.stopping.load(Ordering::SeqCst)
	}

	fn cancel_linger(&self) {
		if let Some(handle) = lock(&self.linger).take() {
			handle.abort();
		}
	}
}

// =============================================================================
// Mux daemon
// =============================================================================

/// Listening multiplexer. One instance per project scope.
pub struct LspMuxServer {
	servers: Mutex<HashMap<String, Arc<ServerInstance>>>,
	sessions: Mutex<HashMap<u64, Arc<Session>>>,
	next_session: AtomicU64,
	idle_notify: Mutex<Option<oneshot::Sender<()>>>,
	idle_timer: Mutex<Option<JoinHandle<()>>>,
	accept_stop: Mutex<Option<oneshot::Sender<()>>>,
	accept_done: Mutex<Option<oneshot::Receiver<()>>>,
	shutting_down: AtomicBool,
	endpoint: Mutex<Option<std::path::PathBuf>>,
	connect_lock: Mutex<()>,
}

impl Default for LspMuxServer {
	fn default() -> Self {
		Self::new()
	}
}

impl LspMuxServer {
	#[must_use]
	pub fn new() -> Self {
		Self {
			servers: Mutex::new(HashMap::new()),
			sessions: Mutex::new(HashMap::new()),
			next_session: AtomicU64::new(1),
			idle_notify: Mutex::new(None),
			idle_timer: Mutex::new(None),
			accept_stop: Mutex::new(None),
			accept_done: Mutex::new(None),
			shutting_down: AtomicBool::new(false),
			endpoint: Mutex::new(None),
			connect_lock: Mutex::new(()),
		}
	}

	/// Number of connected links, including ping-only links.
	#[must_use]
	pub fn session_count(&self) -> usize {
		lock(&self.sessions).len()
	}

	/// Keys of currently live shared language-server children.
	#[must_use]
	pub fn server_keys(&self) -> Vec<String> {
		lock(&self.servers)
			.values()
			.map(|server| server.key.clone())
			.collect()
	}

	/// Set the channel fired once the mux has been idle for its grace period.
	pub fn set_idle_notify(&self, sender: oneshot::Sender<()>) {
		*lock(&self.idle_notify) = Some(sender);
	}

	/// Bind the endpoint and serve until [`shutdown`](Self::shutdown).
	pub async fn listen(self: &Arc<Self>, endpoint: &std::path::Path) -> Result<(), String> {
		if lock(&self.endpoint).is_some() {
			return Err("LSP mux is already listening".to_string());
		}
		if endpoint.exists() {
			if probe_endpoint(endpoint).await {
				return Err(format!("LSP mux already listening on {}", endpoint.display()));
			}
			std::fs::remove_file(endpoint)
				.map_err(|error| format!("cannot remove stale LSP mux socket: {error}"))?;
		}
		if let Some(parent) = endpoint.parent() {
			std::fs::create_dir_all(parent)
				.map_err(|error| format!("cannot create LSP mux runtime dir: {error}"))?;
		}
		let listener = UnixListener::bind(endpoint)
			.map_err(|error| format!("cannot bind LSP mux socket {}: {error}", endpoint.display()))?;
		*lock(&self.endpoint) = Some(endpoint.to_path_buf());
		let (stop_tx, mut stop_rx) = oneshot::channel();
		let (done_tx, done_rx) = oneshot::channel();
		*lock(&self.accept_stop) = Some(stop_tx);
		*lock(&self.accept_done) = Some(done_rx);
		self.arm_idle();
		let server = self.clone();
		tokio::spawn(async move {
			loop {
				tokio::select! {
					_ = &mut stop_rx => break,
					accepted = listener.accept() => match accepted {
						Ok((stream, _)) => server.start_session(stream),
						Err(error) => {
							if server.shutting_down.load(Ordering::SeqCst) {
								break;
							}
							eprintln!("LSP mux accept failed: {error}");
						}
					},
				}
			}
			drop(listener);
			if let Some(endpoint) = lock(&server.endpoint).take() {
				let _ = std::fs::remove_file(endpoint);
			}
			let _ = done_tx.send(());
		});
		Ok(())
	}

	/// Gracefully close all links, children, and the listening endpoint.
	pub async fn shutdown(self: &Arc<Self>) {
		if self.shutting_down.swap(true, Ordering::SeqCst) {
			return;
		}
		self.disarm_idle();
		if let Some(stop) = lock(&self.accept_stop).take() {
			let _ = stop.send(());
		}
		if let Some(done) = lock(&self.accept_done).take() {
			let _ = done.await;
		}
		let sessions: Vec<Arc<Session>> = lock(&self.sessions)
			.values()
			.cloned()
			.collect();
		for session in sessions {
			self.close_session(&session);
		}
		let servers: Vec<Arc<ServerInstance>> = lock(&self.servers)
			.values()
			.cloned()
			.collect();
		for server in servers {
			self.stop_server(&server).await;
		}
	}

	fn start_session(self: &Arc<Self>, stream: UnixStream) {
		let id = self.next_session.fetch_add(1, Ordering::SeqCst);
		let (mut read, write) = stream.into_split();
		let (out_tx, out_rx) = mpsc::unbounded_channel::<String>();
		let (close_tx, close_rx) = watch::channel(false);
		let writer_close = close_rx.clone();
		tokio::spawn(async move { writer_loop(write, out_rx, writer_close).await });
		let session = Arc::new(Session {
			id,
			out: out_tx,
			close: close_tx,
			open_uris: Mutex::new(HashSet::new()),
			forwarded: Mutex::new(HashMap::new()),
			client_pending: Mutex::new(HashMap::new()),
			server: Mutex::new(None),
			initialized: AtomicBool::new(false),
			closed: AtomicBool::new(false),
			last_activity: AtomicU64::new(0),
		});
		lock(&self.sessions).insert(id, session.clone());
		self.disarm_idle();
		let server = self.clone();
		tokio::spawn(async move {
			let mut framer = MessageFramer::new();
			let mut buffer = [0u8; 16 * 1024];
			let mut close_rx = close_rx;
			loop {
				tokio::select! {
					_ = wait_close(&mut close_rx) => break,
					read_result = read.read(&mut buffer) => match read_result {
						Ok(0) | Err(_) => break,
						Ok(count) => {
							framer.push(&buffer[..count]);
							let messages = match framer.drain(|header| {
								eprintln!("LSP mux client framing resync: {}", header.trim());
							}) {
								Ok(messages) => messages,
								Err(error) => {
									eprintln!("LSP mux client framing failed: {error}");
									break;
								},
							};
							for text in messages {
								if let Ok(message) = serde_json::from_str::<Value>(&text)
									&& message.get("jsonrpc").and_then(Value::as_str) == Some("2.0")
								{
									server.from_session(&session, message);
								}
							}
						}
					},
				}
			}
			server.close_session(&session);
		});
	}

	// -------------------------------------------------------------------------
	// Client -> mux
	// -------------------------------------------------------------------------

	fn from_session(self: &Arc<Self>, session: &Arc<Session>, message: Value) {
		if session.closed.load(Ordering::SeqCst) {
			return;
		}
		touch(session);
		if let Some(name) = method(&message).map(str::to_string) {
			let id = request_id(&message);
			self.handle_session_method(session, &message, &name, id);
			return;
		}
		self.handle_client_response(session, &message);
	}

	fn handle_session_method(
		self: &Arc<Self>,
		session: &Arc<Session>,
		message: &Value,
		name: &str,
		id: Option<Value>,
	) {
		let request = id.is_some();
		if name == MUX_PING_METHOD && request {
			session.send(rpc_result(id.unwrap_or(Value::Null), json!(MUX_PING_RESULT)));
			return;
		}
		if name == MUX_CONNECT_METHOD && request {
			self.handle_connect(session, message.get("params"), id.unwrap_or(Value::Null));
			return;
		}
		let bound = lock(&session.server).clone();
		let Some(server) = bound else {
			if let Some(id) = id {
				session.send(rpc_error(id, -32_002, "muxConnect must be first"));
			}
			return;
		};
		match name {
			MUX_RESTART_METHOD if !request => self.kill_server(&server),
			"initialize" if request => {
				self.initialize(session, &server, message, id.unwrap_or(Value::Null));
			},
			"initialized" if !request => {
				if !server.initialized_sent.swap(true, Ordering::SeqCst) {
					server.send(message.clone());
				} else {
					self.replay_state(session, &server);
				}
				session.initialized.store(true, Ordering::SeqCst);
			},
			"shutdown" if request => {
				session.send(rpc_result(id.unwrap_or(Value::Null), Value::Null));
			},
			"exit" if !request => {
				let _ = session.close.send(true);
			},
			"textDocument/didOpen" if !request => {
				if let Some(uri) = text_document_uri(message) {
					lock(&session.open_uris).insert(uri.clone());
					lock(&server.documents).insert(uri);
				}
				server.send(message.clone());
			},
			"textDocument/didChange" if !request => server.send(message.clone()),
			"textDocument/didClose" if !request => {
				if let Some(uri) = text_document_uri(message) {
					lock(&session.open_uris).remove(&uri);
					lock(&server.documents).remove(&uri);
				}
				server.send(message.clone());
			},
			"$/cancelRequest" if !request => {
				let original = message
					.get("params")
					.and_then(|params| params.get("id"))
					.cloned();
				if let Some(original) = original {
					let mux_id = lock(&session.forwarded).get(&id_key(&original)).copied();
					if let Some(mux_id) = mux_id {
						let mut cancelled = message.clone();
						cancelled["params"] = json!({ "id": mux_id });
						server.send(cancelled);
					}
				}
			},
			_ if request => {
				let mux_id = server.mux_id();
				lock(&server.pending).insert(
					mux_id,
					Pending::Forward {
						session: session.clone(),
						original_id: id.clone().unwrap_or(Value::Null),
					},
				);
				if let Some(original) = id {
					lock(&session.forwarded).insert(id_key(&original), mux_id);
				}
				let mut forwarded = message.clone();
				forwarded["id"] = json!(mux_id);
				server.send(forwarded);
			},
			_ => server.send(message.clone()),
		}
	}

	fn handle_connect(self: &Arc<Self>, session: &Arc<Session>, params: Option<&Value>, id: Value) {
		let _guard = lock(&self.connect_lock);
		if lock(&session.server).is_some() {
			session.send(rpc_error(id, -32_600, "session already bound"));
			return;
		}
		let Some(params) = params.cloned().and_then(|params| serde_json::from_value::<MuxConnectParams>(params).ok())
		else {
			session.send(rpc_error(id, -32_602, "invalid mux connect params"));
			return;
		};
		let key = server_key(&params);
		let mut server = lock(&self.servers)
			.values()
			.find(|candidate| candidate.key == key && lock(&candidate.sessions).is_empty() && !candidate.is_stopping())
			.cloned();
		let mut spawned = false;
		if server.is_none() {
			match self.spawn_server(key.clone(), &params) {
				Ok(instance) => {
					server = Some(instance);
					spawned = true;
				},
				Err(error) => {
					session.send(rpc_error(id, -32_600, &format!("cannot spawn language server: {error}")));
					return;
				},
			}
		}
		let Some(server) = server else { return };
		server.cancel_linger();
		lock(&server.sessions).push(session.clone());
		*lock(&session.server) = Some(server.clone());
		session.send(rpc_result(
			id,
			serde_json::to_value(MuxConnectResult {
				key,
				spawned,
				pid: Some(server.pid),
			})
			.unwrap_or(Value::Null),
		));
	}

	fn initialize(&self, session: &Arc<Session>, server: &Arc<ServerInstance>, message: &Value, id: Value) {
		let mut state = lock(&server.initialize);
		if state.cached {
			let result = state.result.clone().unwrap_or(Value::Null);
			drop(state);
			session.send(rpc_result(id, result));
			return;
		}
		state.waiters.push((session.clone(), id));
		if state.in_flight {
			return;
		}
		state.in_flight = true;
		drop(state);
		let mux_id = server.mux_id();
		lock(&server.pending).insert(mux_id, Pending::Initialize);
		let mut params = message
			.get("params")
			.filter(|params| params.is_object())
			.cloned()
			.unwrap_or_else(|| json!({}));
		// Language servers commonly self-terminate with their advertised client pid.
		params["processId"] = json!(std::process::id());
		let mut forwarded = message.clone();
		forwarded["id"] = json!(mux_id);
		forwarded["params"] = params;
		server.send(forwarded);
	}

	// -------------------------------------------------------------------------
	// Mux -> child
	// -------------------------------------------------------------------------

	fn spawn_server(
		self: &Arc<Self>,
		key: String,
		params: &MuxConnectParams,
	) -> Result<Arc<ServerInstance>, String> {
		let mut command = Command::new(&params.command);
		command
			.args(&params.args)
			.current_dir(&params.cwd)
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::inherit())
			.kill_on_drop(true);
		if let Some(env) = &params.env {
			for (name, value) in env {
				command.env(name, value);
			}
		}
		#[cfg(unix)]
		command.process_group(0);
		let mut child = command
			.spawn()
			.map_err(|error| format!("failed to spawn '{}': {error}", params.command))?;
		let pid = child
			.id()
			.ok_or_else(|| "language server has no pid".to_string())?;
		let stdin = child
			.stdin
			.take()
			.ok_or_else(|| "failed to capture language server stdin".to_string())?;
		let stdout = child
			.stdout
			.take()
			.ok_or_else(|| "failed to capture language server stdout".to_string())?;
		let (sink, receiver) = mpsc::unbounded_channel();
		tokio::spawn(async move { write_queue(stdin, receiver).await });
		let server = Arc::new(ServerInstance::new(key, pid, sink));
		lock(&self.servers).insert(server.key.clone(), server.clone());
		tokio::spawn(read_server(self.clone(), server.clone(), stdout));
		let mux = self.clone();
		let watched = server.clone();
		tokio::spawn(async move {
			let _ = wait_child(child).await;
			mux.server_exited(&watched);
		});
		Ok(server)
	}

	// -------------------------------------------------------------------------
	// Child -> mux
	// -------------------------------------------------------------------------

	fn from_server(self: &Arc<Self>, server: &Arc<ServerInstance>, message: Value) {
		let Some(name) = method(&message) else {
			self.handle_server_response(server, &message);
			return;
		};
		if let Some(id) = request_id(&message) {
			self.handle_server_request(server, &message, name, id);
			return;
		}
		match name {
			"textDocument/publishDiagnostics" => {
				if let Some(uri) = message
					.get("params")
					.and_then(|params| params.get("uri"))
					.and_then(Value::as_str)
				{
					lock(&server.diagnostics).insert(uri.to_string(), message.clone());
				}
			},
			"$/progress" => {
				if let Some(token) = progress_token(&message) {
					match progress_kind(&message) {
						Some("begin") => {
							lock(&server.progress).insert(token, message.clone());
						},
						Some("end") => {
							lock(&server.progress).remove(&token);
						},
						_ => {},
					}
				}
			},
			_ => {},
		}
		self.broadcast(server, &message);
	}

	fn handle_server_response(self: &Arc<Self>, server: &Arc<ServerInstance>, message: &Value) {
		let Some(id) = message.get("id").and_then(Value::as_i64) else {
			return;
		};
		let Some(pending) = lock(&server.pending).remove(&id) else {
			return;
		};
		match pending {
			Pending::Shutdown(sender) => {
				let _ = sender.send(());
			},
			Pending::Initialize => self.finish_initialize(server, message),
			Pending::Forward {
				session,
				original_id,
			} => {
				if session.closed.load(Ordering::SeqCst) {
					return;
				}
				lock(&session.forwarded).remove(&id_key(&original_id));
				let mut response = message.clone();
				response["id"] = original_id;
				session.send(response);
			},
		}
	}

	fn finish_initialize(self: &Arc<Self>, server: &Arc<ServerInstance>, message: &Value) {
		let waiters = {
			let mut state = lock(&server.initialize);
			state.in_flight = false;
			if let Some(error) = message.get("error") {
				let error = error.clone();
				let waiters = std::mem::take(&mut state.waiters);
				for (session, id) in &waiters {
					session.send(json!({ "jsonrpc": "2.0", "id": id, "error": error }));
				}
				drop(state);
				self.kill_server(server);
				return;
			}
			state.result = message.get("result").cloned();
			state.cached = true;
			std::mem::take(&mut state.waiters)
		};
		let result = lock(&server.initialize).result.clone().unwrap_or(Value::Null);
		for (session, id) in waiters {
			session.send(rpc_result(id, result.clone()));
		}
	}

	fn handle_server_request(
		&self,
		server: &Arc<ServerInstance>,
		message: &Value,
		name: &str,
		id: Value,
	) {
		let registration = matches!(
			name,
			"client/registerCapability" | "client/unregisterCapability" | "window/workDoneProgress/create"
		);
		if registration {
			match name {
				"client/registerCapability" => {
					let registrations = message
						.get("params")
						.and_then(|params| params.get("registrations"))
						.and_then(Value::as_array)
						.map(|registrations| {
							registrations
								.iter()
								.filter(|registration| {
									registration.get("id").is_some_and(Value::is_string)
								})
								.cloned()
								.collect::<Vec<_>>()
						})
						.unwrap_or_default();
					lock(&server.registrations).push(json!({ "registrations": registrations }));
				},
				"client/unregisterCapability" => {
					let ids: HashSet<String> = message
						.get("params")
						.and_then(|params| {
							params
								.get("unregisterations")
								.or_else(|| params.get("unregistrations"))
						})
						.and_then(Value::as_array)
						.map(|entries| {
							entries
								.iter()
								.filter_map(|entry| entry.get("id").and_then(Value::as_str))
								.map(str::to_string)
								.collect()
						})
						.unwrap_or_default();
					for batch in lock(&server.registrations).iter_mut() {
						if let Some(registrations) = batch.get_mut("registrations").and_then(Value::as_array_mut) {
							registrations.retain(|registration| {
								registration
									.get("id")
									.and_then(Value::as_str)
									.is_none_or(|id| !ids.contains(id))
							});
						}
					}
				},
				_ => {},
			}
			server.send(rpc_result(id, Value::Null));
			for session in lock(&server.sessions).clone() {
				if !session.initialized.load(Ordering::SeqCst) {
					continue;
				}
				let client_id = server.client_id();
				lock(&session.client_pending).insert(
					client_id.clone(),
					ClientPending {
						relay: false,
						server_id: None,
					},
				);
				let mut forwarded = message.clone();
				forwarded["id"] = json!(client_id);
				session.send(forwarded);
			}
			return;
		}
		let focus = lock(&server.sessions)
			.iter()
			.max_by_key(|session| session.last_activity.load(Ordering::SeqCst))
			.cloned();
		let Some(focus) = focus else {
			server.send(rpc_error(id, -32_601, "no client attached"));
			return;
		};
		let client_id = server.client_id();
		lock(&focus.client_pending).insert(
			client_id.clone(),
			ClientPending {
				relay: true,
				server_id: Some(id.clone()),
			},
		);
		let mut forwarded = message.clone();
		forwarded["id"] = json!(client_id);
		focus.send(forwarded);
	}

	fn handle_client_response(&self, session: &Arc<Session>, message: &Value) {
		let Some(id) = message.get("id").and_then(Value::as_str) else {
			return;
		};
		let Some(pending) = lock(&session.client_pending).remove(id) else {
			return;
		};
		if !pending.relay {
			return;
		}
		let (Some(server), Some(server_id)) = (lock(&session.server).clone(), pending.server_id) else {
			return;
		};
		let mut response = message.clone();
		response["id"] = server_id;
		server.send(response);
	}

	fn broadcast(&self, server: &Arc<ServerInstance>, message: &Value) {
		for session in lock(&server.sessions).clone() {
			if session.initialized.load(Ordering::SeqCst) {
				session.send(message.clone());
			}
		}
	}

	fn replay_state(&self, session: &Arc<Session>, server: &Arc<ServerInstance>) {
		for batch in lock(&server.registrations).clone() {
			let registrations = batch
				.get("registrations")
				.and_then(Value::as_array)
				.map_or(0, Vec::len);
			if registrations == 0 {
				continue;
			}
			let client_id = server.client_id();
			lock(&session.client_pending).insert(
				client_id.clone(),
				ClientPending {
					relay: false,
					server_id: None,
				},
			);
			session.send(json!({
				"jsonrpc": "2.0",
				"id": client_id,
				"method": "client/registerCapability",
				"params": batch,
			}));
		}
		for diagnostic in lock(&server.diagnostics).values() {
			session.send(diagnostic.clone());
		}
		for progress in lock(&server.progress).values() {
			session.send(progress.clone());
		}
	}

	// -------------------------------------------------------------------------
	// Lifecycle
	// -------------------------------------------------------------------------

	fn close_session(self: &Arc<Self>, session: &Arc<Session>) {
		if session.closed.swap(true, Ordering::SeqCst) {
			return;
		}
		lock(&self.sessions).remove(&session.id);
		let server = lock(&session.server).take();
		if let Some(server) = server {
			let open: Vec<String> = lock(&session.open_uris).drain().collect();
			for uri in open {
				lock(&server.documents).remove(&uri);
				server.send(json!({
					"jsonrpc": "2.0",
					"method": "textDocument/didClose",
					"params": { "textDocument": { "uri": uri } },
				}));
			}
			let cancelled: Vec<i64> = lock(&server.pending)
				.iter()
				.filter_map(|(id, pending)| match pending {
					Pending::Forward { session: owner, .. } if Arc::ptr_eq(owner, session) => Some(*id),
					_ => None,
				})
				.collect();
			for mux_id in cancelled {
				server.send(json!({
					"jsonrpc": "2.0",
					"method": "$/cancelRequest",
					"params": { "id": mux_id },
				}));
			}
			lock(&server.initialize)
				.waiters
				.retain(|(waiter, _)| !Arc::ptr_eq(waiter, session));
			let empty = {
				let mut sessions = lock(&server.sessions);
				sessions.retain(|candidate| !Arc::ptr_eq(candidate, session));
				sessions.is_empty()
			};
			if empty && !server.is_stopping() {
				self.arm_linger(&server);
			}
		}
		let _ = session.close.send(true);
		let no_sessions = lock(&self.sessions).is_empty();
		if no_sessions {
			self.arm_idle();
		}
	}

	fn server_exited(self: &Arc<Self>, server: &Arc<ServerInstance>) {
		if server.exited.swap(true, Ordering::SeqCst) {
			return;
		}
		server.stopping.store(true, Ordering::SeqCst);
		server.cancel_linger();
		lock(&server.pending).clear();
		// A reconnect can replace the map entry while this child is reaped;
		// only remove the entry that still belongs to this instance.
		{
			let mut servers = lock(&self.servers);
			if servers.get(&server.key).is_some_and(|current| Arc::ptr_eq(current, server)) {
				servers.remove(&server.key);
			}
		}
		// Drop the session guard before close_session re-locks the same server.
		let sessions: Vec<Arc<Session>> = {
			let mut guard = lock(&server.sessions);
			guard.drain(..).collect()
		};
		for session in sessions {
			*lock(&session.server) = None;
			self.close_session(&session);
		}
	}

	fn kill_server(self: &Arc<Self>, server: &Arc<ServerInstance>) {
		if server.exited.load(Ordering::SeqCst) {
			return;
		}
		server.stopping.store(true, Ordering::SeqCst);
		#[cfg(unix)]
		{
			if let Ok(pid) = i32::try_from(server.pid) {
				// The child is its own process group (pgid == pid).
				unsafe {
					libc::kill(-pid, libc::SIGKILL);
					libc::kill(pid, libc::SIGKILL);
				}
				return;
			}
		}
		self.server_exited(server);
	}

	async fn stop_server(self: &Arc<Self>, server: &Arc<ServerInstance>) {
		if server.stopping.swap(true, Ordering::SeqCst) {
			return;
		}
		server.cancel_linger();
		let id = server.mux_id();
		let (sender, receiver) = oneshot::channel();
		lock(&server.pending).insert(id, Pending::Shutdown(sender));
		server.send(json!({ "jsonrpc": "2.0", "id": id, "method": "shutdown", "params": null }));
		let _ = tokio::time::timeout(SHUTDOWN_BUDGET, receiver).await;
		server.send(json!({ "jsonrpc": "2.0", "method": "exit" }));
		self.kill_server(server);
	}

	fn arm_linger(self: &Arc<Self>, server: &Arc<ServerInstance>) {
		if let Some(handle) = lock(&server.linger).take() {
			handle.abort();
		}
		let mux = self.clone();
		let watched = server.clone();
		let handle = tokio::spawn(async move {
			tokio::time::sleep(SERVER_LINGER).await;
			let empty = lock(&watched.sessions).is_empty();
			if empty {
				mux.stop_server(&watched).await;
			}
		});
		*lock(&server.linger) = Some(handle);
	}

	fn disarm_idle(&self) {
		if let Some(handle) = lock(&self.idle_timer).take() {
			handle.abort();
		}
	}

	fn arm_idle(self: &Arc<Self>) {
		self.disarm_idle();
		if self.shutting_down.load(Ordering::SeqCst) || !lock(&self.sessions).is_empty() {
			return;
		}
		let mux = self.clone();
		let handle = tokio::spawn(async move {
			tokio::time::sleep(MUX_IDLE).await;
			if lock(&mux.sessions).is_empty()
				&& let Some(notify) = lock(&mux.idle_notify).take()
			{
				let _ = notify.send(());
			}
		});
		*lock(&self.idle_timer) = Some(handle);
	}
}

fn millis() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map_or(0, |elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
}

fn touch(session: &Session) {
	session
		.last_activity
		.store(millis(), Ordering::SeqCst);
}

async fn wait_close(close: &mut watch::Receiver<bool>) {
	let _ = close.wait_for(|closed| *closed).await;
}

async fn writer_loop(
	mut write: OwnedWriteHalf,
	mut receiver: mpsc::UnboundedReceiver<String>,
	mut close: watch::Receiver<bool>,
) {
	loop {
		tokio::select! {
			_ = wait_close(&mut close) => {
				let _ = write.shutdown().await;
				return;
			},
			message = receiver.recv() => match message {
				Some(message) => {
					if write.write_all(message.as_bytes()).await.is_err() {
						return;
					}
				},
				None => return,
			},
		}
	}
}

async fn write_queue(mut stdin: ChildStdin, mut receiver: mpsc::UnboundedReceiver<String>) {
	while let Some(message) = receiver.recv().await {
		if stdin.write_all(message.as_bytes()).await.is_err() {
			return;
		}
	}
}

async fn read_server(
	mux: Arc<LspMuxServer>,
	server: Arc<ServerInstance>,
	mut stdout: ChildStdout,
) {
	let mut framer = MessageFramer::new();
	let mut buffer = [0u8; 16 * 1024];
	loop {
		let count = match stdout.read(&mut buffer).await {
			Ok(0) | Err(_) => break,
			Ok(count) => count,
		};
		framer.push(&buffer[..count]);
		let messages = match framer.drain(|header| {
			eprintln!("LSP mux server framing resync: {}", header.trim());
		}) {
			Ok(messages) => messages,
			Err(error) => {
				eprintln!("LSP mux server framing failed: {error}");
				break;
			},
		};
		for text in messages {
			if let Ok(message) = serde_json::from_str::<Value>(&text)
				&& message.get("jsonrpc").and_then(Value::as_str) == Some("2.0")
			{
				mux.from_server(&server, message);
			}
		}
	}
	mux.server_exited(&server);
}

async fn wait_child(mut child: Child) -> Option<i32> {
	child.wait().await.ok().and_then(|status| status.code())
}

async fn probe_endpoint(endpoint: &std::path::Path) -> bool {
	let Ok(Ok(stream)) = tokio::time::timeout(
		Duration::from_millis(1_500),
		UnixStream::connect(endpoint),
	)
	.await
	else {
		return false;
	};
	drop(stream);
	true
}
