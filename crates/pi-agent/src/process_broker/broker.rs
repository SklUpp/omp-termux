//! Profiled, project-scoped Unix-socket daemon broker.
//!
//! The broker is an auto-spawned hidden worker (`__omp_worker_daemon_broker`)
//! that owns ONE [`ProcessManager::new_keepalive`] table. Because it is a
//! separate process, `persist`/`detached` children genuinely outlive CLI
//! invocations. The manager performs the actual supervision (spawn, logs,
//! readiness, restart backoff, tree kill); this module only adds transport,
//! authentication, ownership, lease/idle lifecycle, metadata persistence and
//! detached recovery.
//!
//! Anchors: `packages/coding-agent/src/launch/broker.ts`,
//! `packages/coding-agent/src/launch/client.ts`, `paths.ts`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pi_shell::cancel::CancelToken;
use pi_shell::process::Process;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

use super::client;
use super::protocol::{self, LeaseLock, Operation, WireSpec};
use crate::tools::process::{
	LogOptions, LogsOutcome, ProcessError, ProcessEvent, ProcessManager, ProcessSnapshot, ProcessSpec,
	ProcessState, Signal, StartOutcome, WaitOutcome, WaitRequest, is_terminal_state,
};

/// Minimal supervision surface the broker needs. The concrete implementation
/// wraps the single shared [`ProcessManager`]; tests may provide a fake, but no
/// transport-only path exists on its own.
#[async_trait]
pub trait BrokerBackend: Send + Sync {
	async fn start(&self, spec: ProcessSpec, owner: Option<String>) -> Result<StartOutcome, ProcessError>;
	/// Failed snapshots must propagate; never report transport failure as an
	/// empty daemon table.
	async fn list(&self, owner: Option<String>) -> Result<Vec<ProcessSnapshot>, ProcessError>;
	async fn logs(
		&self,
		name: String,
		options: LogOptions,
		owner: Option<String>,
	) -> Result<LogsOutcome, ProcessError>;
	async fn wait(
		&self,
		name: String,
		request: WaitRequest,
		timeout_ms: u64,
		owner: Option<String>,
	) -> Result<WaitOutcome, ProcessError>;
	async fn send(
		&self,
		name: String,
		data: Option<String>,
		signal: Option<Signal>,
		owner: Option<String>,
	) -> Result<ProcessSnapshot, ProcessError>;
	async fn stop(
		&self,
		name: String,
		timeout_ms: u64,
		owner: Option<String>,
	) -> Result<ProcessSnapshot, ProcessError>;
	async fn restart(&self, name: String, owner: Option<String>) -> Result<ProcessSnapshot, ProcessError>;
	async fn describe(
		&self,
		name: String,
		owner: Option<String>,
	) -> Result<(ProcessSnapshot, ProcessSpec), ProcessError>;
	/// Refresh one recovered detached record (log size / pinned identity).
	async fn refresh(&self, name: String) -> Result<(), ProcessError>;
	/// Re-attach supervision to a detached child that survived a broker exit.
	async fn adopt_detached(&self, spec: ProcessSpec, snapshot: ProcessSnapshot) -> Result<(), ProcessError>;
	/// Authoritative process-transition stream (spawn/ready/restart/exit/adopt).
	/// The broker persists every event immediately instead of polling (B3).
	fn events(&self) -> tokio::sync::broadcast::Receiver<ProcessEvent>;
	/// Arm the manager's background deadline (fire-and-forget).
	fn arm_timeout(self: Arc<Self>, name: String, owner: Option<String>, timeout_ms: u64);
	/// Broker teardown. The manager stops non-persistent children; broker-owned
	/// `persist`/`detached` children keep running with their pinned identity.
	/// The next broker generation adopts `detached` and retires (identity-checked)
	/// non-detached orphans, matching the manager's broker-owned contract.
	fn shutdown(&self);
}

/// Concrete adapter over the one broker-owned [`ProcessManager`].
pub struct ManagerBackend {
	manager: Arc<ProcessManager>,
}

impl ManagerBackend {
	/// Broker-owned manager rooted at `<runtime>/daemons`, where each record's
	/// `output.log` and `meta.json` live together.
	#[must_use]
	pub fn new(log_root: PathBuf) -> Self {
		Self { manager: Arc::new(ProcessManager::new_keepalive(log_root)) }
	}

	#[must_use]
	pub fn manager(&self) -> &Arc<ProcessManager> {
		&self.manager
	}
}

#[async_trait]
impl BrokerBackend for ManagerBackend {
	async fn start(&self, spec: ProcessSpec, owner: Option<String>) -> Result<StartOutcome, ProcessError> {
		ProcessManager::start(&self.manager, spec, owner).await
	}

	async fn list(&self, owner: Option<String>) -> Result<Vec<ProcessSnapshot>, ProcessError> {
		for snapshot in self.manager.list(None) {
			if snapshot.detached && !is_terminal_state(snapshot.state) {
				let _ = self.manager.refresh_detached(&snapshot.name);
			}
		}
		Ok(self.manager.list(owner.as_deref()))
	}

	async fn logs(
		&self,
		name: String,
		options: LogOptions,
		owner: Option<String>,
	) -> Result<LogsOutcome, ProcessError> {
		self.manager.logs(&name, owner.as_deref(), options).await
	}

	async fn wait(
		&self,
		name: String,
		request: WaitRequest,
		timeout_ms: u64,
		owner: Option<String>,
	) -> Result<WaitOutcome, ProcessError> {
		self.manager.wait(&name, owner.as_deref(), request, timeout_ms).await
	}

	async fn send(
		&self,
		name: String,
		data: Option<String>,
		signal: Option<Signal>,
		owner: Option<String>,
	) -> Result<ProcessSnapshot, ProcessError> {
		self.manager.send(&name, owner.as_deref(), data.as_deref(), signal).await
	}

	async fn stop(
		&self,
		name: String,
		timeout_ms: u64,
		owner: Option<String>,
	) -> Result<ProcessSnapshot, ProcessError> {
		ProcessManager::stop(&self.manager, &name, owner.as_deref(), timeout_ms).await
	}

	async fn restart(&self, name: String, owner: Option<String>) -> Result<ProcessSnapshot, ProcessError> {
		ProcessManager::restart(&self.manager, &name, owner.as_deref()).await
	}

	async fn describe(
		&self,
		name: String,
		owner: Option<String>,
	) -> Result<(ProcessSnapshot, ProcessSpec), ProcessError> {
		if let Ok(snapshot) = self.manager.describe(&name, None)
			&& snapshot.0.detached
			&& !is_terminal_state(snapshot.0.state)
		{
			let _ = self.manager.refresh_detached(&name);
		}
		self.manager.describe(&name, owner.as_deref())
	}

	async fn refresh(&self, name: String) -> Result<(), ProcessError> {
		self.manager.refresh_detached(&name).map(|_| ())
	}

	async fn adopt_detached(&self, spec: ProcessSpec, snapshot: ProcessSnapshot) -> Result<(), ProcessError> {
		self.manager.adopt_detached(spec, snapshot).map(|_| ())
	}

	fn events(&self) -> tokio::sync::broadcast::Receiver<ProcessEvent> {
		self.manager.events()
	}

	fn arm_timeout(self: Arc<Self>, name: String, owner: Option<String>, timeout_ms: u64) {
		ProcessManager::arm_timeout(&self.manager, &name, owner.as_deref(), timeout_ms);
	}

	fn shutdown(&self) {
		self.manager.shutdown();
	}
}

/// Persisted recovery record (`<runtime>/daemons/<name>/meta.json`, 0600).
///
/// Contains the full child environment (needed for exact relaunch) and is
/// therefore deliberately not `Debug`.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MetaRecord {
	daemon: ProcessSnapshot,
	spec: WireSpec,
}

/// Durable pre-spawn intent (`<daemons>/<name>/start.json`). Written and
/// fsynced *before* the manager is asked to spawn, so recovery can resolve a
/// crash between intent and the first committed `Spawned` event to a terminal
/// row instead of losing the attempt or inventing a running one.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartIntent {
	spec: WireSpec,
	spec_hash: String,
	state: String,
	at: u64,
}

const START_FILE: &str = "start.json";
const INTENT_STARTING: &str = "starting";

struct Lease {
	instance_id: String,
}

/// One authenticated broker scope.
pub struct Broker {
	project_dir: PathBuf,
	runtime_dir: PathBuf,
	endpoint: PathBuf,
	token: String,
	idle_grace: Duration,
	backend: Arc<dyn BrokerBackend>,
	clients: AtomicUsize,
	zero_clients: Notify,
	shutdown: CancellationToken,
	shutting_down: AtomicBool,
	starting: Mutex<HashSet<String>>,
}

impl Broker {
	pub fn new(
		project_dir: PathBuf,
		runtime_dir: PathBuf,
		backend: Arc<dyn BrokerBackend>,
		idle_grace: Duration,
	) -> Result<Self, String> {
		let endpoint = protocol::endpoint(&runtime_dir)?;
		protocol::ensure_private_dir(&runtime_dir).map_err(|err| {
			format!("cannot create daemon broker runtime dir {}: {err}", runtime_dir.display())
		})?;
		let token = protocol::ensure_token(&runtime_dir)
			.map_err(|err| format!("cannot initialize daemon broker token: {err}"))?;
		Ok(Self {
			project_dir,
			runtime_dir,
			endpoint,
			token,
			idle_grace,
			backend,
			clients: AtomicUsize::new(0),
			zero_clients: Notify::new(),
			shutdown: CancellationToken::new(),
			shutting_down: AtomicBool::new(false),
			starting: Mutex::new(HashSet::new()),
		})
	}

	/// Serve until an explicit `shutdown` op or the idle rule fires.
	pub async fn serve(self: Arc<Self>) -> Result<(), String> {
		let Some(lease) = acquire_lease(&self.runtime_dir)? else {
			// Another live broker owns this scope; the lease winner serves it.
			return Ok(());
		};
		let instance_id = lease.instance_id.clone();
		if let Err(err) = self.write_scope() {
			eprintln!("daemon broker scope metadata failed: {err}");
		}

		let cancel = CancellationToken::new();
		// Subscribe before recovery so `Adopted` (and any spawn during startup)
		// is observed, then persist each authoritative transition immediately.
		let events = self.backend.events();
		let transitions = tokio::spawn(transitions_loop(Arc::clone(&self), events, cancel.clone()));
		self.recover().await;

		let _ = std::fs::remove_file(&self.endpoint);
		let listener = match UnixListener::bind(&self.endpoint) {
			Ok(listener) => listener,
			Err(err) => {
				cancel.cancel();
				let _ = transitions.await;
				release_lease(self.runtime_dir.join(protocol::PID_FILE), &instance_id);
				return Err(format!(
					"cannot bind daemon broker socket {}: {err}",
					self.endpoint.display()
				));
			},
		};
		if let Err(err) = secure_endpoint(&self.endpoint) {
			let _ = std::fs::remove_file(&self.endpoint);
			cancel.cancel();
			let _ = transitions.await;
			release_lease(self.runtime_dir.join(protocol::PID_FILE), &instance_id);
			return Err(err);
		}

		let accept = tokio::spawn(accept_loop(Arc::clone(&self), listener, cancel.clone()));
		let idle = tokio::spawn(idle_loop(Arc::clone(&self), cancel.clone()));

		self.shutdown.cancelled().await;
		self.shutting_down.store(true, Ordering::SeqCst);
		self.persist_all().await;
		self.backend.shutdown();
		// Give the manager's async tree-kills a bounded window to settle before
		// the runtime (and this process) goes away; persistent/detached children
		// are intentionally untouched by broker teardown.
		for _ in 0..40 {
			let Ok(daemons) = self.backend.clone().list(None).await else {
				break;
			};
			let pending = daemons
				.iter()
				.any(|daemon| !daemon.persist && !daemon.detached && !is_terminal_state(daemon.state));
			if !pending {
				break;
			}
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
		cancel.cancel();
		let _ = accept.await;
		let _ = idle.await;
		let _ = transitions.await;
		let _ = std::fs::remove_file(&self.endpoint);
		release_lease(self.runtime_dir.join(protocol::PID_FILE), &instance_id);
		Ok(())
	}

	fn begin_shutdown(&self) {
		if !self.shutting_down.swap(true, Ordering::SeqCst) {
			self.shutdown.cancel();
		}
	}

	fn add_client(&self) {
		self.clients.fetch_add(1, Ordering::SeqCst);
	}

	fn remove_client(&self) {
		if self.clients.fetch_sub(1, Ordering::SeqCst) == 1 {
			// `notify_one` stores a permit when nobody is waiting, so a drop to
			// zero between the idle loop's load and its registration cannot be
			// lost (B4).
			self.zero_clients.notify_one();
		}
	}

	async fn dispatch(&self, operation: &Operation) -> Result<Value, String> {
		use protocol::{
			DescribeReply, ListReply, LogsReply, PingReply, ShutdownReply, SnapshotReply, StartReply,
			TimeoutReply, WaitReply,
		};

		if self.shutting_down.load(Ordering::SeqCst) && !matches!(operation, Operation::Ping | Operation::Shutdown) {
			return Err("daemon broker is shutting down".to_string());
		}

		match operation {
			Operation::Ping => {
				let project_dir = self.project_dir.to_string_lossy().into_owned();
				serde_json::to_value(PingReply { op: "ping", project_dir: &project_dir })
					.map_err(|err| err.to_string())
			},
			Operation::Start { spec, owner } => {
				let spec = spec.clone().normalized();
				protocol::validate_start(&spec)?;
				{
					let mut starting = self.starting.lock().await;
					if !starting.insert(spec.name.clone()) {
						return Err(format!("Daemon {} is already starting", spec.name));
					}
				}
				if let Err(err) = self.preflight_start(&spec) {
					self.starting.lock().await.remove(&spec.name);
					return Err(err);
				}
				let result = self
					.backend
					.start(spec.into_process_spec(), owner.clone())
					.await;
				self.starting.lock().await.remove(&spec.name);
				let outcome = match result {
					Ok(outcome) => {
						// Commit the authoritative snapshot (fsynced) before the
						// start reply leaves the broker. On failure keep the
						// durable intent so recovery can resolve the partial
						// start, and report honestly instead of claiming success.
						if let Err(err) = self.persist_name(&spec.name).await {
							return Err(format!(
								"daemon {} started but its metadata commit failed: {err}",
								spec.name
							));
						}
						self.clear_intent(&spec.name);
						outcome
					},
					Err(err) => {
						self.clear_intent(&spec.name);
						return Err(process_error(err));
					},
				};
				serde_json::to_value(StartReply {
					op: "start",
					daemon: &outcome.snapshot,
					ready_timed_out: outcome.ready_timed_out,
				})
				.map_err(|err| err.to_string())
			},
			Operation::List { owner } => {
				let daemons = self
					.backend
					.clone()
					.list(owner.clone())
					.await
					.map_err(process_error)?;
				serde_json::to_value(ListReply { op: "list", daemons: &daemons }).map_err(|err| err.to_string())
			},
			Operation::Logs { name, lines, head, grep, follow, cursor, timeout_ms, owner } => {
				let outcome = self
					.backend
					.logs(
						name.clone(),
						LogOptions {
							lines: (*lines).clamp(1, 1_000),
							head: *head,
							grep: grep.clone(),
							follow: *follow,
							cursor: *cursor,
							timeout_ms: *timeout_ms,
						},
						owner.clone(),
					)
					.await
					.map_err(process_error)?;
				serde_json::to_value(LogsReply {
					op: "logs",
					name: &outcome.name,
					text: &outcome.text,
					cursor: outcome.cursor,
					timed_out: outcome.timed_out,
					state: outcome.state,
				})
				.map_err(|err| err.to_string())
			},
			Operation::Wait { name, target, pattern, timeout_ms, owner } => {
				let outcome = self
					.backend
					.wait(
						name.clone(),
						WaitRequest {
							for_exit: matches!(target, protocol::WaitTarget::Exit),
							pattern: pattern.clone(),
						},
						*timeout_ms,
						owner.clone(),
					)
					.await
					.map_err(process_error)?;
				self.persist_name(name).await?;
				serde_json::to_value(WaitReply {
					op: "wait",
					daemon: &outcome.snapshot,
					matched: outcome.matched.as_deref(),
					timed_out: outcome.timed_out,
				})
				.map_err(|err| err.to_string())
			},
			Operation::Send { name, data, signal, owner } => {
				let snapshot = self
					.backend
					.send(name.clone(), data.clone(), *signal, owner.clone())
					.await
					.map_err(process_error)?;
				self.persist_name(name).await?;
				serde_json::to_value(SnapshotReply { op: "send", daemon: &snapshot })
					.map_err(|err| err.to_string())
			},
			Operation::Stop { name, timeout_ms, owner } => {
				let snapshot = self
					.backend
					.stop(name.clone(), *timeout_ms, owner.clone())
					.await
					.map_err(process_error)?;
				self.persist_name(name).await?;
				serde_json::to_value(SnapshotReply { op: "stop", daemon: &snapshot })
					.map_err(|err| err.to_string())
			},
			Operation::Restart { name, owner } => {
				let snapshot = self
					.backend
					.restart(name.clone(), owner.clone())
					.await
					.map_err(process_error)?;
				self.persist_name(name).await?;
				serde_json::to_value(SnapshotReply { op: "restart", daemon: &snapshot })
					.map_err(|err| err.to_string())
			},
			Operation::Timeout { name, timeout_ms, owner } => {
				self.backend
					.clone()
					.arm_timeout(name.clone(), owner.clone(), *timeout_ms);
				serde_json::to_value(TimeoutReply { op: "timeout" }).map_err(|err| err.to_string())
			},
			Operation::Describe { name, owner } => {
				let (snapshot, spec) = self
					.backend
					.describe(name.clone(), owner.clone())
					.await
					.map_err(process_error)?;
				self.persist_name(name).await?;
				let public = protocol::PublicSpec::from(&spec);
				serde_json::to_value(DescribeReply { op: "describe", daemon: &snapshot, spec: &public })
					.map_err(|err| err.to_string())
			},
			Operation::Shutdown => {
				serde_json::to_value(ShutdownReply { op: "shutdown" }).map_err(|err| err.to_string())
			},
		}
	}

	/// Persist one record's snapshot + spec (0600, fsynced atomic rename).
	/// Describe failure is not a persistence failure (terminal/pruned record);
	/// a filesystem failure is surfaced so callers never report durable success
	/// after a silent ENOSPC.
	async fn persist_name(&self, name: &str) -> Result<(), String> {
		let Ok((snapshot, spec)) = self.backend.describe(name.to_string(), None).await else {
			return Ok(());
		};
		self.store_meta(&spec, snapshot)
			.map_err(|err| format!("cannot persist daemon metadata for {name}: {err}"))
	}

	/// Durably record the spawn intent before handing the spec to the manager.
	/// The bytes are fsynced (file + directory) before the process can exist.
	fn preflight_start(&self, spec: &WireSpec) -> Result<(), String> {
		let dir = self.runtime_dir.join(protocol::DAEMONS_DIR).join(&spec.name);
		protocol::ensure_private_dir(&dir).map_err(|err| {
			format!("cannot prepare daemon metadata directory {}: {err}", dir.display())
		})?;
		let encoded = serde_json::to_vec(spec).map_err(|err| err.to_string())?;
		let intent = StartIntent {
			spec: spec.clone(),
			spec_hash: protocol::scope_key(Path::new(&format!("intent:{}", String::from_utf8_lossy(&encoded)))),
			state: INTENT_STARTING.to_string(),
			at: protocol::now_ms(),
		};
		let bytes = serde_json::to_vec(&intent).map_err(|err| err.to_string())?;
		atomic_write(&dir.join(START_FILE), &bytes)
			.map_err(|err| format!("cannot persist daemon start intent: {err}"))
	}

	fn clear_intent(&self, name: &str) {
		let _ = std::fs::remove_file(
			self.runtime_dir.join(protocol::DAEMONS_DIR).join(name).join(START_FILE),
		);
	}

	/// Reconcile after a lagged broadcast: resubscribe happened first.
	async fn reconcile(&self) {
		let Ok(daemons) = self.backend.clone().list(None).await else {
			return;
		};
		for snapshot in daemons {
			if let Err(err) = self.persist_name(&snapshot.name).await {
				eprintln!("daemon broker reconcile commit failed: {err}");
			}
		}
	}

	fn store_meta(&self, spec: &ProcessSpec, snapshot: ProcessSnapshot) -> std::io::Result<()> {
		// `snapshot.startIdentity` is the manager's pinned identity; it is
		// persisted verbatim so adoption can never accept a recycled PID.
		let wire = WireSpec::from_process_spec(spec);
		let bytes = serde_json::to_vec(&MetaRecord { daemon: snapshot, spec: wire })
			.map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
		write_meta_bytes(&self.runtime_dir, &spec.name, &bytes)
	}

	async fn persist_all(&self) {
		match self.backend.clone().list(None).await {
			Ok(daemons) => {
				for snapshot in daemons {
					if let Err(err) = self.persist_name(&snapshot.name).await {
						eprintln!("daemon broker shutdown commit failed: {err}");
					}
				}
			},
			// Keep shutdown moving, but never pretend the table was empty.
			Err(err) => eprintln!("daemon broker shutdown snapshot failed: {}", process_error(err)),
		}
	}

	async fn has_live_persistent(&self) -> bool {
		match self.backend.clone().list(None).await {
			Ok(daemons) => daemons
				.into_iter()
				.any(|snapshot| (snapshot.persist || snapshot.detached) && !is_terminal_state(snapshot.state)),
			// Never idle-close on an unknown table: that would kill children we
			// may still own. Stay alive and retry on the next idle tick.
			Err(_) => true,
		}
	}

	fn write_scope(&self) -> std::io::Result<()> {
		// Preserve the client-written checkpoint for a shortened long-root
		// scope; only the project field is broker-owned.
		let existing = protocol::read_scope_record(&self.runtime_dir.join(protocol::SCOPE_FILE))
			.unwrap_or_default();
		#[derive(Serialize)]
		#[serde(rename_all = "camelCase")]
		struct Scope {
			version: u32,
			logical_root: String,
			root_token: String,
			project_dir: String,
		}
		let text = serde_json::to_vec(&Scope {
			version: existing.version,
			logical_root: existing.logical_root,
			root_token: existing.root_token,
			project_dir: self.project_dir.to_string_lossy().into_owned(),
		})
		.map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
		atomic_write(&self.runtime_dir.join(protocol::SCOPE_FILE), &text)
	}

	/// Recover records left by a previous broker generation.
	///
	/// Detached children are adopted only when their persisted `startIdentity`
	/// still matches the live process (opaque per-boot token). Everything else
	/// is marked exited with the PID cleared and is never signalled. A
	/// non-detached orphan left by a broker crash is terminated only after that
	/// same identity check and only through the pinned [`Process`] handle.
	async fn recover(&self) {
		let root = self.runtime_dir.join(protocol::DAEMONS_DIR);
		let Ok(entries) = std::fs::read_dir(&root) else {
			return;
		};
		for entry in entries.flatten() {
			let path = entry.path().join(protocol::META_FILE);
			let intent_path = entry.path().join(START_FILE);
			let Ok(text) = protocol::read_private_file(&path) else {
				// Crash between durable preflight and the first committed
				// `Spawned` event: the attempt is unresolved. Without a pinned
				// start token it can never be adopted or signalled, so record the
				// truth (terminal) rather than dropping it or faking a running
				// row. A later meta.json from a real spawn always wins.
				if let Ok(raw) = protocol::read_private_file(&intent_path)
					&& let Ok(intent) = serde_json::from_str::<StartIntent>(&raw)
				{
					let spec = intent.spec.into_process_spec();
					let snapshot = interrupted_snapshot(
						&spec,
						"start interrupted before the spawned process was recorded",
					);
					let _ = write_meta(&self.runtime_dir, &WireSpec::from_process_spec(&spec), &snapshot);
				}
				let _ = std::fs::remove_file(&intent_path);
				continue;
			};
			let Ok(mut meta) = serde_json::from_str::<MetaRecord>(&text) else {
				continue;
			};
			let _ = std::fs::remove_file(&intent_path);
			if is_terminal_state(meta.daemon.state) {
				continue;
			}
			let pinned = meta.daemon.start_identity.as_ref().and_then(|identity| {
				Process::from_pid(identity.pid as i32)
					.filter(|process| process.identity_token() == identity.token)
			});
			if meta.spec.detached {
				if pinned.is_some() {
					// Clone: the fallback below still needs `meta.spec` for the
					// terminal record when adoption is rejected.
					let spec = meta.spec.clone().into_process_spec();
					if self.backend.adopt_detached(spec, meta.daemon.clone()).await.is_ok() {
						let _ = self.backend.refresh(meta.daemon.name.clone()).await;
						if let Err(err) = self.persist_name(&meta.daemon.name).await {
							eprintln!("daemon broker adoption commit failed: {err}");
						}
						continue;
					}
				}
			} else if let Some(process) = pinned {
				let _ = process.terminate_tree(true, 500, 2_000, CancelToken::default()).await;
			}
			// No verified identity: never touch the PID, just record the truth
			// that this generation is no longer supervised.
			meta.daemon.pid = None;
			meta.daemon.start_identity = None;
			meta.daemon.state = crate::tools::process::ProcessState::Exited;
			meta.daemon.exited_at = Some(protocol::now_ms());
			meta.daemon.exit_reason = Some("previous broker exited".to_string());
			meta.daemon.ready_pending = None;
			let _ = write_meta(&self.runtime_dir, &meta.spec, &meta.daemon);
		}
	}
}

fn process_error(error: ProcessError) -> String {
	error.to_string()
}

/// Android/Linux `SO_PEERCRED` check: only this effective uid may talk to the
/// broker, even if socket permissions were widened behind our back.
fn peer_is_owner(stream: &UnixStream) -> bool {
	#[cfg(any(target_os = "linux", target_os = "android"))]
	{
		use std::os::unix::io::AsRawFd;
		let mut credentials = libc::ucred { pid: 0, uid: 0, gid: 0 };
		let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
		// SAFETY: `stream` owns a valid fd; the two pointers reference writable
		// storage of exactly the size this `getsockopt(SO_PEERCRED)` writes.
		let result = unsafe {
			libc::getsockopt(
				stream.as_raw_fd(),
				libc::SOL_SOCKET,
				libc::SO_PEERCRED,
				(&raw mut credentials).cast(),
				&raw mut length,
			)
		};
		result == 0 && credentials.uid == protocol::current_uid()
	}
	#[cfg(not(any(target_os = "linux", target_os = "android")))]
	{
		let _ = stream;
		true
	}
}

async fn accept_loop(broker: Arc<Broker>, listener: UnixListener, cancel: CancellationToken) {
	loop {
		tokio::select! {
			() = cancel.cancelled() => return,
			accepted = listener.accept() => {
				match accepted {
					Ok((stream, _addr)) => {
						let broker = Arc::clone(&broker);
						tokio::spawn(async move { handle_connection(broker, stream).await });
					},
					Err(_) => {
						broker.begin_shutdown();
						return;
					},
				}
			},
		}
	}
}

async fn handle_connection(broker: Arc<Broker>, stream: UnixStream) {
	if !peer_is_owner(&stream) {
		return;
	}
	let (mut reader, writer) = stream.into_split();
	let writer = Arc::new(Mutex::new(writer));
	let mut buffer: Vec<u8> = Vec::new();
	let mut chunk = vec![0u8; 64 * 1024];
	let mut authenticated = false;

	'connection: loop {
		let read = match reader.read(&mut chunk).await {
			Ok(0) | Err(_) => break,
			Ok(read) => read,
		};
		buffer.extend_from_slice(&chunk[..read]);
		if buffer.len() > protocol::MAX_REQUEST_BYTES {
			let _ = write_line(
				&writer,
				&protocol::encode_error("unknown", "Daemon broker request exceeds size limit"),
			)
			.await;
			break;
		}
		while let Some(index) = buffer.iter().position(|byte| *byte == b'\n') {
			let line: Vec<u8> = buffer.drain(..=index).collect();
			let line = &line[..line.len().saturating_sub(1)];
			if line.is_empty() {
				continue;
			}
			let Ok(line) = std::str::from_utf8(line) else {
				let _ = write_line(&writer, &protocol::encode_error("unknown", "Daemon broker request is not UTF-8")).await;
				continue;
			};
			let request = match protocol::parse_request(line) {
				Ok(request) => request,
				Err(error) => {
					let _ = write_line(&writer, &protocol::encode_error("unknown", &error)).await;
					continue;
				},
			};
			if !protocol::token_matches(&broker.token, &request.token) {
				let _ = write_line(
					&writer,
					&protocol::encode_error(&request.id, "Daemon broker authentication failed"),
				)
				.await;
				break 'connection;
			}
			if !authenticated {
				authenticated = true;
				broker.add_client();
			}
			let task_broker = Arc::clone(&broker);
			let task_writer = Arc::clone(&writer);
			let shutdown_after = request.operation.is_shutdown();
			let id = request.id.clone();
			let operation = request.operation;
			tokio::spawn(async move {
				let line = match task_broker.dispatch(&operation).await {
					Ok(result) => protocol::encode_ok(&id, &result),
					Err(error) => protocol::encode_error(&id, &error),
				};
				let _ = write_line(&task_writer, &line).await;
				if shutdown_after {
					task_broker.begin_shutdown();
				}
			});
		}
	}

	if authenticated {
		broker.remove_client();
	}
}

async fn write_line(writer: &Mutex<OwnedWriteHalf>, line: &str) -> std::io::Result<()> {
	let mut writer = writer.lock().await;
	writer.write_all(line.as_bytes()).await?;
	writer.flush().await
}

async fn idle_loop(broker: Arc<Broker>, cancel: CancellationToken) {
	loop {
		// Wait until no client is connected. The `Notify` future is enabled
		// before the count check, so the count->zero transition is always
		// observed even if it lands in the registration window.
		loop {
			let notified = broker.zero_clients.notified();
			tokio::pin!(notified);
			notified.as_mut().enable();
			if cancel.is_cancelled() {
				return;
			}
			if broker.clients.load(Ordering::SeqCst) == 0 {
				break;
			}
			tokio::select! {
				() = notified => {},
				() = cancel.cancelled() => return,
			}
		}
		tokio::select! {
			() = tokio::time::sleep(broker.idle_grace) => {},
			() = cancel.cancelled() => return,
		}
		if cancel.is_cancelled() || broker.clients.load(Ordering::SeqCst) > 0 {
			continue;
		}
		if broker.has_live_persistent().await {
			continue;
		}
		if client::has_live_presence(&broker.runtime_dir) {
			continue;
		}
		if broker.clients.load(Ordering::SeqCst) == 0 {
			broker.begin_shutdown();
			return;
		}
	}
}

const LEASE_LOCK_FILE: &str = "broker.lease.lock";

/// Publish lease bytes atomically: a fully-written 0600 temp file is renamed
/// onto the final name while the caller holds [`LeaseLock`], so readers only
/// ever see a complete record and no unlink/replace window exists (B2).
fn publish_lease(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
	let dir = path.parent().unwrap_or_else(|| Path::new("."));
	let temp = dir.join(format!(
		".{}.{}.{}.tmp",
		protocol::PID_FILE,
		std::process::id(),
		protocol::random_hex(6)?
	));
	protocol::write_private_new(&temp, bytes)?;
	let renamed = std::fs::rename(&temp, path);
	if renamed.is_err() {
		let _ = std::fs::remove_file(&temp);
	}
	renamed?;
	// Rename durability: fsync the containing directory.
	std::fs::File::open(dir)?.sync_all()
}

fn acquire_lease(runtime_dir: &Path) -> Result<Option<Lease>, String> {
	protocol::ensure_private_dir(runtime_dir)
		.map_err(|err| format!("cannot prepare daemon broker runtime dir: {err}"))?;
	let path = runtime_dir.join(protocol::PID_FILE);
	// Serialized read/check/replace: a live claimant is never unlinked, a stale
	// record is atomically replaced by rename, and a crashed holder releases the
	// kernel lock automatically.
	let _lock = LeaseLock::acquire(&runtime_dir.join(LEASE_LOCK_FILE))
		.map_err(|err| format!("cannot lock daemon broker lease: {err}"))?;
	let instance_id = protocol::random_hex(16).map_err(|err| err.to_string())?;
	let record = protocol::LeaseRecord {
		pid: std::process::id(),
		start_time: protocol::process_start_time(std::process::id()),
		instance_id: instance_id.clone(),
	};
	let bytes = serde_json::to_vec(&record).map_err(|err| err.to_string())?;
	let previous = protocol::read_private_file(&path)
		.ok()
		.and_then(|raw| serde_json::from_str::<protocol::LeaseRecord>(&raw).ok());
	if previous
		.as_ref()
		.is_some_and(|record| protocol::process_alive(record.pid, record.start_time))
	{
		return Ok(None);
	}
	publish_lease(&path, &bytes)
		.map_err(|err| format!("cannot acquire daemon broker lease: {err}"))?;
	Ok(Some(Lease { instance_id }))
}

fn release_lease(path: PathBuf, instance_id: &str) {
	let _lock = path
		.parent()
		.and_then(|dir| LeaseLock::acquire(&dir.join(LEASE_LOCK_FILE)).ok());
	let matches = protocol::read_private_file(&path)
		.ok()
		.and_then(|raw| serde_json::from_str::<protocol::LeaseRecord>(&raw).ok())
		.is_some_and(|record| record.instance_id == instance_id);
	if matches {
		let _ = std::fs::remove_file(&path);
	}
}

fn secure_endpoint(endpoint: &Path) -> Result<(), String> {
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		std::fs::set_permissions(endpoint, std::fs::Permissions::from_mode(0o600))
			.map_err(|err| format!("cannot chmod daemon broker socket: {err}"))?;
	}
	protocol::verify_owner(endpoint)
		.map_err(|err| format!("daemon broker socket ownership check failed: {err}"))
}

fn write_meta(runtime_dir: &Path, spec: &WireSpec, snapshot: &ProcessSnapshot) -> std::io::Result<()> {
	// `snapshot` (including its `startIdentity`) round-trips verbatim.
	let meta = MetaRecord { daemon: snapshot.clone(), spec: spec.clone() };
	let bytes = serde_json::to_vec(&meta)
		.map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
	write_meta_bytes(runtime_dir, &spec.name, &bytes)
}

fn write_meta_bytes(runtime_dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
	let dir = runtime_dir.join(protocol::DAEMONS_DIR).join(name);
	protocol::ensure_private_dir(&dir)?;
	atomic_write(&dir.join(protocol::META_FILE), bytes)
}

/// Persist manager transitions asynchronously as they are emitted. `Spawned`
/// is emitted before the readiness wait, but this loop may not have written the
/// record yet when the wait starts; metadata is guaranteed only once persisted,
/// so a broker death during ready wait or an auto-restart is reconciled from
/// list+describe rather than assumed on disk. Lagged consumers resubscribe
/// first, then reconcile via list+describe, per the manager's broadcast contract.
async fn transitions_loop(
	broker: Arc<Broker>,
	mut events: tokio::sync::broadcast::Receiver<ProcessEvent>,
	cancel: CancellationToken,
) {
	// Dedupe cache is task-local: no shared mutex can block lifecycle ops.
	let mut seen: HashMap<String, Vec<u8>> = HashMap::new();
	loop {
		tokio::select! {
			() = cancel.cancelled() => return,
			event = events.recv() => match event {
				Ok(event) => {
					let name = event.spec.name.clone();
					let wire = WireSpec::from_process_spec(&event.spec);
					let Ok(bytes) = serde_json::to_vec(&MetaRecord { daemon: event.snapshot, spec: wire }) else {
						continue;
					};
					if seen.get(&name).map(Vec::as_slice) != Some(bytes.as_slice())
						&& write_meta_bytes(&broker.runtime_dir, &name, &bytes).is_ok()
					{
						// Cache only after the fsynced commit succeeded; a failed
						// write (ENOSPC, permissions) must be retried on the next
						// event instead of being memoized as persisted.
						seen.insert(name.clone(), bytes.clone());
					}
				},
				Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
					// Resubscribe FIRST so no new transition is missed, then
					// reconcile the current authoritative state.
					events = broker.backend.events();
					seen.clear();
					broker.reconcile().await;
				},
				Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
			},
		}
	}
}

fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
	let suffix = protocol::random_hex(4)?;
	let stem = path.file_stem().unwrap_or_default().to_string_lossy();
	let temporary = path.with_file_name(format!("{stem}.{}.{suffix}.tmp", std::process::id()));
	let result = protocol::write_private_new(&temporary, bytes)
		.and_then(|()| std::fs::rename(&temporary, path))
		.and_then(|()| {
			// Rename durability: fsync the containing directory so the commit
			// survives a crash. Termux/Android supports directory fsync.
			let dir = path.parent().unwrap_or_else(|| Path::new("."));
			std::fs::File::open(dir)?.sync_all()
		});
	if result.is_err() {
		let _ = std::fs::remove_file(&temporary);
	}
	result
}

/// Terminal row for a start attempt that died before a pinned identity existed.
fn interrupted_snapshot(spec: &ProcessSpec, reason: &str) -> ProcessSnapshot {
	let now = protocol::now_ms();
	ProcessSnapshot {
		name: spec.name.clone(),
		id: format!("interrupted-{}", protocol::random_hex(8).unwrap_or_else(|_| "unknown".to_string())),
		state: ProcessState::Failed,
		pid: None,
		created_at: now,
		started_at: now,
		ready_at: None,
		exited_at: Some(now),
		exit_code: None,
		exit_reason: Some(reason.to_string()),
		restart_count: 0,
		output_bytes: 0,
		owner: None,
		ready_match: None,
		ready_pending: None,
		start_identity: None,
		persist: spec.persist,
		detached: spec.detached,
	}
}

/// Worker entry point used by the CLI hidden selector.
///
/// Takes its inputs explicitly: the CLI reads the three `OMP_DAEMON_*`
/// variables and removes them before any thread starts, so child processes
/// never inherit worker-only wiring.
pub async fn start(
	project_dir: PathBuf,
	runtime_dir: PathBuf,
	idle_grace: Duration,
) -> Result<(), String> {
	let backend = Arc::new(ManagerBackend::new(runtime_dir.join(protocol::DAEMONS_DIR)));
	let broker = Broker::new(project_dir, runtime_dir, backend, idle_grace)?;
	Arc::new(broker).serve().await
}
