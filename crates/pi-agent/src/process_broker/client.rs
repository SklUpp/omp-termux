//! Client side of the daemon broker: connect to (or auto-spawn) the hidden
//! worker for one project scope, authenticate with the 0600 token, and speak
//! bounded newline-JSON over the private socket.
//!
//! Transport is deliberately **per request**: every operation opens a fresh
//! authenticated connection, exchanges exactly one frame, and closes it. That
//! makes cancellation safe (a timed-out request drops its socket, so no stale
//! reply can be mistaken for the next request) and keeps a long `wait`/`follow`
//! from blocking `stop`/`send`/`arm_timeout` (B1/B5). Presence files are the
//! only cross-request state; persistent children live in the broker, not here.
//!
//! Anchors: `packages/coding-agent/src/launch/client.ts`, `presence.ts`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use super::protocol::{self, Operation, WaitTarget, WireSpec};
use crate::tools::process::{
	LogOptions, LogsOutcome, ProcessBackend, ProcessError, ProcessSnapshot, ProcessSpec, Signal,
	StartOutcome, WaitOutcome, WaitRequest,
};

/// Connect probe timeout per attempt.
const CONNECT_PROBE_MS: u64 = 250;
/// Overall budget to obtain a first reachable broker (spawn/startup).
const CONNECT_BUDGET_MS: u64 = 10_000;
const CONNECT_RETRY_MS: u64 = 50;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartResult {
	pub daemon: ProcessSnapshot,
	pub ready_timed_out: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitResult {
	pub daemon: ProcessSnapshot,
	#[serde(default)]
	pub matched: Option<String>,
	pub timed_out: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DescribeResult {
	pub daemon: ProcessSnapshot,
	pub spec: protocol::PublicSpec,
}

/// One authenticated client identity for a project-scoped broker.
///
/// Holds no socket: operations are independent. Dropping the client removes its
/// presence marker; the broker decides idle shutdown afterwards.
pub struct BrokerClient {
	project_dir: PathBuf,
	runtime_dir: PathBuf,
	endpoint: PathBuf,
	token: String,
	spawn: bool,
	next_id: AtomicU64,
	/// Removed when the client drops; keeps the broker alive between calls.
	_presence: Presence,
}

impl std::fmt::Debug for BrokerClient {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter.write_str("BrokerClient")
	}
}

impl BrokerClient {
	/// Connect to the broker for `project_dir`, spawning the hidden worker when
	/// no live broker owns the scope.
	pub async fn connect(project_dir: &Path) -> Result<Arc<Self>, String> {
		let project = protocol::canonical_project_dir(project_dir);
		let logical_root = protocol::runtime_root();
		let logical_dir = logical_root.join(protocol::scope_key(&project));
		let runtime = protocol::physical_runtime_dir_with(&logical_root, &project)?;
		if runtime != logical_dir {
			// Long logical root: the worker must be able to prove it is binding
			// the same scope before it creates the socket.
			protocol::ensure_scope(&runtime, &logical_root, &project).map_err(|err| {
				format!("cannot prepare daemon broker scope {}: {err}", runtime.display())
			})?;
		}
		Self::open(&project, &runtime, true).await
	}

	/// Connect (or spawn) using an explicit runtime directory.
	pub async fn connect_in(project_dir: &Path, runtime_dir: &Path) -> Result<Arc<Self>, String> {
		Self::open(project_dir, runtime_dir, true).await
	}

	/// Connect to a broker that must already exist (tests, diagnostics).
	pub async fn connect_existing(project_dir: &Path, runtime_dir: &Path) -> Result<Arc<Self>, String> {
		Self::open(project_dir, runtime_dir, false).await
	}

	async fn open(project_dir: &Path, runtime_dir: &Path, spawn: bool) -> Result<Arc<Self>, String> {
		let project = protocol::canonical_project_dir(project_dir);
		protocol::ensure_private_dir(runtime_dir).map_err(|err| {
			format!("cannot prepare daemon broker runtime dir {}: {err}", runtime_dir.display())
		})?;
		let endpoint = protocol::endpoint(runtime_dir)?;
		let token = protocol::ensure_token(runtime_dir)
			.map_err(|err| format!("cannot initialize daemon broker token: {err}"))?;
		let presence = Presence::register(runtime_dir)
			.map_err(|err| format!("cannot register daemon broker presence: {err}"))?;
		let client = Self {
			project_dir: project,
			runtime_dir: runtime_dir.to_path_buf(),
			endpoint,
			token,
			spawn,
			next_id: AtomicU64::new(0),
			_presence: presence,
		};
		// Fail early (and spawn a worker) instead of deferring to the first op.
		let deadline = Instant::now() + Duration::from_millis(CONNECT_BUDGET_MS);
		client.connect_stream(deadline).await.map_err(|err| {
			format!("cannot connect to daemon broker at {}: {err}", client.endpoint.display())
		})?;
		Ok(Arc::new(client))
	}

	/// Open one authenticated connection, starting a worker if allowed.
	async fn connect_stream(&self, deadline: Instant) -> Result<UnixStream, String> {
		let mut spawn_attempts = 0u8;
		loop {
			if let Some(stream) = try_connect(&self.endpoint).await {
				return Ok(stream);
			}
			if self.spawn && spawn_attempts < 3 && !lease_live(&self.runtime_dir) {
				spawn_worker(&self.project_dir, &self.runtime_dir)?;
				spawn_attempts += 1;
			}
			if Instant::now() >= deadline {
				return Err("no broker socket became available".to_string());
			}
			tokio::time::sleep(Duration::from_millis(CONNECT_RETRY_MS)).await;
		}
	}

	/// Send one authenticated operation on its own connection and decode the
	/// result envelope. A timeout/drop retires the whole connection (B5).
	async fn request(&self, operation: Operation) -> Result<Value, String> {
		let timeout = operation.timeout();
		let id = format!("{}-{}", std::process::id(), self.next_id.fetch_add(1, Ordering::Relaxed));
		let line = protocol::encode_request(&id, &self.token, operation)?;
		if line.len() > protocol::MAX_REQUEST_BYTES {
			return Err(format!(
				"daemon broker request exceeds the {} byte limit",
				protocol::MAX_REQUEST_BYTES
			));
		}
		let deadline = Instant::now() + timeout;
		let exchange = async {
			let stream = self.connect_stream(deadline).await?;
			let (read_half, mut write_half) = stream.into_split();
			write_half
				.write_all(line.as_bytes())
				.await
				.map_err(|err| format!("daemon broker write failed: {err}"))?;
			write_half
				.flush()
				.await
				.map_err(|err| format!("daemon broker write failed: {err}"))?;
			let mut reader = BufReader::new(read_half);
			let reply = read_bounded_line(&mut reader).await?;
			let text =
				std::str::from_utf8(&reply).map_err(|_| "daemon broker reply is not UTF-8".to_string())?;
			protocol::parse_reply(text.trim_end())
		};
		let reply = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), exchange)
			.await
			.map_err(|_| format!("daemon broker request timed out after {}ms", timeout.as_millis()))??;
		if reply.id != id {
			return Err("daemon broker reply id mismatch".to_string());
		}
		if reply.ok {
			Ok(reply.result.unwrap_or(Value::Null))
		} else {
			Err(reply.error.unwrap_or_else(|| "daemon broker request failed".to_string()))
		}
	}

	pub async fn ping(&self) -> Result<String, String> {
		#[derive(Deserialize)]
		#[serde(rename_all = "camelCase")]
		struct Ping {
			project_dir: String,
		}
		decode::<Ping>(self.request(Operation::Ping).await?).map(|reply| reply.project_dir)
	}

	pub async fn start(&self, spec: WireSpec, owner: Option<&str>) -> Result<StartResult, String> {
		let result = self
			.request(Operation::Start { spec, owner: owner.map(str::to_string) })
			.await?;
		decode(result)
	}

	/// Start from the hub's [`ProcessSpec`] (complete environment travels
	/// privately and is used verbatim by the broker).
	pub async fn start_spec(&self, spec: &ProcessSpec, owner: Option<&str>) -> Result<StartResult, String> {
		self.start(WireSpec::from_process_spec(spec), owner).await
	}

	pub async fn list(&self, owner: Option<&str>) -> Result<Vec<ProcessSnapshot>, String> {
		#[derive(Deserialize)]
		struct List {
			daemons: Vec<ProcessSnapshot>,
		}
		decode::<List>(self.request(Operation::List { owner: owner.map(str::to_string) }).await?)
			.map(|reply| reply.daemons)
	}

	pub async fn logs(
		&self,
		name: &str,
		options: LogOptions,
		owner: Option<&str>,
	) -> Result<LogsOutcome, String> {
		let result = self
			.request(Operation::Logs {
				name: name.to_string(),
				lines: options.lines,
				head: options.head,
				grep: options.grep,
				follow: options.follow,
				cursor: options.cursor,
				timeout_ms: options.timeout_ms,
				owner: owner.map(str::to_string),
			})
			.await?;
		decode(result)
	}

	pub async fn wait(
		&self,
		name: &str,
		target: WaitTarget,
		pattern: Option<&str>,
		timeout_ms: u64,
		owner: Option<&str>,
	) -> Result<WaitResult, String> {
		let result = self
			.request(Operation::Wait {
				name: name.to_string(),
				target,
				pattern: pattern.map(str::to_string),
				timeout_ms,
				owner: owner.map(str::to_string),
			})
			.await?;
		decode(result)
	}

	pub async fn send(
		&self,
		name: &str,
		data: Option<&str>,
		signal: Option<Signal>,
		owner: Option<&str>,
	) -> Result<ProcessSnapshot, String> {
		let result = self
			.request(Operation::Send {
				name: name.to_string(),
				data: data.map(str::to_string),
				signal,
				owner: owner.map(str::to_string),
			})
			.await?;
		snapshot(result)
	}

	pub async fn stop(
		&self,
		name: &str,
		timeout_ms: u64,
		owner: Option<&str>,
	) -> Result<ProcessSnapshot, String> {
		let result = self
			.request(Operation::Stop {
				name: name.to_string(),
				timeout_ms,
				owner: owner.map(str::to_string),
			})
			.await?;
		snapshot(result)
	}

	pub async fn restart(&self, name: &str, owner: Option<&str>) -> Result<ProcessSnapshot, String> {
		let result = self
			.request(Operation::Restart { name: name.to_string(), owner: owner.map(str::to_string) })
			.await?;
		snapshot(result)
	}

	/// Arm the broker-side background deadline for a daemon.
	pub async fn arm_timeout(
		&self,
		name: &str,
		timeout_ms: u64,
		owner: Option<&str>,
	) -> Result<(), String> {
		self
			.request(Operation::Timeout {
				name: name.to_string(),
				timeout_ms,
				owner: owner.map(str::to_string),
			})
			.await
			.map(|_| ())
	}

	pub async fn describe(&self, name: &str, owner: Option<&str>) -> Result<DescribeResult, String> {
		let result = self
			.request(Operation::Describe { name: name.to_string(), owner: owner.map(str::to_string) })
			.await?;
		decode(result)
	}

	/// Explicitly stop the broker. Broker-owned `persist`/`detached` children
	/// keep running; the next broker generation adopts `detached` and retires
	/// non-detached orphans only after their pinned identity matches.
	pub async fn shutdown(&self) -> Result<(), String> {
		self.request(Operation::Shutdown).await.map(|_| ())
	}
}

/// [`ProcessBackend`] adapter that routes every call through the broker.
///
/// This is the only seam the supervisor/hub needs: it can hold
/// `Arc<dyn ProcessBackend>` and never name a broker type. `persist`/`detached`
/// are genuinely cross-exit here because the broker process owns the children.
pub struct BrokerProcessBackend {
	client: Arc<BrokerClient>,
	async_enabled: AtomicBool,
	/// Per-client background sequence. The PID prefix keeps `bash-*` names
	/// unique across concurrent CLI processes sharing one broker scope.
	name_seq: AtomicU64,
}

impl BrokerProcessBackend {
	/// Connect or auto-spawn the broker and wrap it as a backend.
	pub async fn connect(project_dir: &Path) -> Result<Arc<Self>, String> {
		let client = BrokerClient::connect(project_dir).await?;
		Ok(Arc::new(Self { client, async_enabled: AtomicBool::new(true), name_seq: AtomicU64::new(0) }))
	}

	/// Connect or spawn using an explicit runtime directory.
	pub async fn connect_in(project_dir: &Path, runtime_dir: &Path) -> Result<Arc<Self>, String> {
		let client = BrokerClient::connect_in(project_dir, runtime_dir).await?;
		Ok(Arc::new(Self { client, async_enabled: AtomicBool::new(true), name_seq: AtomicU64::new(0) }))
	}

	/// Wrap an already connected client (tests, explicit scopes).
	#[must_use]
	pub fn new(client: Arc<BrokerClient>) -> Self {
		Self { client, async_enabled: AtomicBool::new(true), name_seq: AtomicU64::new(0) }
	}

	#[must_use]
	pub fn client(&self) -> &Arc<BrokerClient> {
		&self.client
	}
}

fn failed(error: String) -> ProcessError {
	ProcessError::Failed(error)
}

#[async_trait]
impl ProcessBackend for BrokerProcessBackend {
	async fn start(self: Arc<Self>, spec: ProcessSpec, owner: Option<String>) -> Result<StartOutcome, ProcessError> {
		self
			.client
			.start(WireSpec::from_process_spec(&spec), owner.as_deref())
			.await
			.map(|reply| StartOutcome { snapshot: reply.daemon, ready_timed_out: reply.ready_timed_out })
			.map_err(failed)
	}

	async fn list(self: Arc<Self>, owner: Option<String>) -> Result<Vec<ProcessSnapshot>, ProcessError> {
		self.client.list(owner.as_deref()).await.map_err(failed)
	}

	async fn logs(
		self: Arc<Self>,
		name: String,
		options: LogOptions,
		owner: Option<String>,
	) -> Result<LogsOutcome, ProcessError> {
		self.client.logs(&name, options, owner.as_deref()).await.map_err(failed)
	}

	async fn wait(
		self: Arc<Self>,
		name: String,
		request: WaitRequest,
		timeout_ms: u64,
		owner: Option<String>,
	) -> Result<WaitOutcome, ProcessError> {
		let target = if request.for_exit { WaitTarget::Exit } else { WaitTarget::Ready };
		self
			.client
			.wait(&name, target, request.pattern.as_deref(), timeout_ms, owner.as_deref())
			.await
			.map(|reply| WaitOutcome { snapshot: reply.daemon, matched: reply.matched, timed_out: reply.timed_out })
			.map_err(failed)
	}

	async fn send(
		self: Arc<Self>,
		name: String,
		data: Option<String>,
		signal: Option<Signal>,
		owner: Option<String>,
	) -> Result<ProcessSnapshot, ProcessError> {
		self.client.send(&name, data.as_deref(), signal, owner.as_deref()).await.map_err(failed)
	}

	async fn stop(
		self: Arc<Self>,
		name: String,
		timeout_ms: u64,
		owner: Option<String>,
	) -> Result<ProcessSnapshot, ProcessError> {
		self.client.stop(&name, timeout_ms, owner.as_deref()).await.map_err(failed)
	}

	async fn restart(self: Arc<Self>, name: String, owner: Option<String>) -> Result<ProcessSnapshot, ProcessError> {
		self.client.restart(&name, owner.as_deref()).await.map_err(failed)
	}

	async fn describe(
		self: Arc<Self>,
		name: String,
		owner: Option<String>,
	) -> Result<(ProcessSnapshot, ProcessSpec), ProcessError> {
		let reply = self.client.describe(&name, owner.as_deref()).await.map_err(failed)?;
		// The wire reply is redacted by design; env stays broker-private.
		let public = reply.spec;
		let spec = ProcessSpec {
			name: public.name,
			application: public.application,
			args: public.args,
			env: HashMap::new(),
			cwd: public.cwd,
			pty: public.pty,
			ready: public.ready,
			restart: public.restart,
			persist: public.persist,
			detached: public.detached,
		};
		Ok((reply.daemon, spec))
	}

	fn next_background_name(&self) -> String {
		format!(
			"bash-{}-{}",
			std::process::id(),
			self.name_seq.fetch_add(1, Ordering::SeqCst)
		)
	}

	fn async_enabled(&self) -> bool {
		self.async_enabled.load(Ordering::SeqCst)
	}

	fn set_async_enabled(&self, enabled: bool) {
		self.async_enabled.store(enabled, Ordering::SeqCst);
	}

	fn arm_timeout(self: Arc<Self>, name: String, owner: Option<String>, timeout_ms: u64) {
		// Fire-and-forget on its own connection; the broker-owned manager
		// records the timeout in its own exit-reason vocabulary.
		tokio::spawn(async move {
			let _ = self.client.arm_timeout(&name, timeout_ms, owner.as_deref()).await;
		});
	}
}

/// Read one newline-terminated reply without ever allocating more than
/// [`protocol::MAX_REQUEST_BYTES`]: bytes are accumulated from `fill_buf` only
/// after the bound check, so a hostile/runaway broker connection cannot exhaust
/// memory before the check (B5). Oversize kills this dedicated connection.
async fn read_bounded_line(reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>) -> Result<Vec<u8>, String> {
	let mut reply: Vec<u8> = Vec::new();
	loop {
		let available = reader
			.fill_buf()
			.await
			.map_err(|err| format!("daemon broker read failed: {err}"))?;
		if available.is_empty() {
			return Err("daemon broker closed the connection".to_string());
		}
		if let Some(position) = available.iter().position(|byte| *byte == b'\n') {
			let take = position + 1;
			if reply.len() + take > protocol::MAX_REQUEST_BYTES {
				return Err("daemon broker reply exceeds the size limit".to_string());
			}
			reply.extend_from_slice(&available[..take]);
			reader.consume(take);
			return Ok(reply);
		}
		if reply.len() + available.len() > protocol::MAX_REQUEST_BYTES {
			return Err("daemon broker reply exceeds the size limit".to_string());
		}
		reply.extend_from_slice(available);
		let consumed = available.len();
		reader.consume(consumed);
	}
}

fn decode<T: DeserializeOwned>(value: Value) -> Result<T, String> {
	serde_json::from_value(value).map_err(|err| format!("invalid daemon broker reply: {err}"))
}

fn snapshot(value: Value) -> Result<ProcessSnapshot, String> {
	#[derive(Deserialize)]
	struct SnapshotReply {
		daemon: ProcessSnapshot,
	}
	decode::<SnapshotReply>(value).map(|reply| reply.daemon)
}

async fn try_connect(endpoint: &Path) -> Option<UnixStream> {
	protocol::verify_socket(endpoint).ok()?;
	tokio::time::timeout(Duration::from_millis(CONNECT_PROBE_MS), UnixStream::connect(endpoint))
		.await
		.ok()?
		.ok()
}

fn lease_live(runtime_dir: &Path) -> bool {
	protocol::read_private_file(&runtime_dir.join(protocol::PID_FILE))
		.ok()
		.and_then(|raw| serde_json::from_str::<protocol::LeaseRecord>(&raw).ok())
		.is_some_and(|record| protocol::process_alive(record.pid, record.start_time))
}

fn spawn_worker(project_dir: &Path, runtime_dir: &Path) -> Result<(), String> {
	let executable =
		std::env::current_exe().map_err(|err| format!("cannot resolve the omp executable: {err}"))?;
	let mut command = std::process::Command::new(executable);
	command
		.arg(protocol::WORKER_ARG)
		.env(protocol::PROJECT_DIR_ENV, project_dir)
		.env(protocol::RUNTIME_DIR_ENV, runtime_dir)
		.env(protocol::IDLE_GRACE_ENV, protocol::DEFAULT_IDLE_GRACE_MS.to_string())
		.stdin(Stdio::null())
		.stdout(Stdio::null())
		.stderr(Stdio::null());
	#[cfg(unix)]
	{
		// Own process group: terminal signals and CLI exit do not take the broker
		// (or the children it owns) down. No wait: the child is intentionally
		// detached; init reaps it after this CLI exits.
		use std::os::unix::process::CommandExt;
		command.process_group(0);
	}
	command
		.spawn()
		.map_err(|err| format!("cannot start daemon broker worker: {err}"))?;
	Ok(())
}

/// Presence marker (`clients/<pid>-<random>.json`, 0600) that lets an idle
/// broker distinguish "between two CLI invocations" from "nobody needs me".
struct Presence {
	path: PathBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PresenceRecord {
	pid: u32,
	#[serde(default)]
	start_time: Option<u64>,
	started_at: u64,
}

impl Presence {
	fn register(runtime_dir: &Path) -> std::io::Result<Self> {
		let dir = runtime_dir.join(protocol::CLIENTS_DIR);
		protocol::ensure_private_dir(&dir)?;
		sweep_presence(&dir);
		let name = format!("{}-{}.json", std::process::id(), protocol::random_hex(8)?);
		let pid = std::process::id();
		let record = PresenceRecord {
			pid,
			start_time: protocol::process_start_time(pid),
			started_at: protocol::now_ms(),
		};
		let bytes = serde_json::to_vec(&record)?;
		let path = dir.join(name);
		protocol::write_private_new(&path, &bytes)?;
		Ok(Self { path })
	}
}

impl Drop for Presence {
	fn drop(&mut self) {
		let _ = std::fs::remove_file(&self.path);
	}
}

fn read_presence(path: &Path) -> Option<PresenceRecord> {
	protocol::read_private_file(path)
		.ok()
		.and_then(|raw| serde_json::from_str::<PresenceRecord>(&raw).ok())
}

fn presence_live(record: &PresenceRecord) -> bool {
	protocol::process_alive(record.pid, record.start_time)
}

fn presence_files(dir: &Path) -> Vec<PathBuf> {
	let Ok(entries) = std::fs::read_dir(dir) else {
		return Vec::new();
	};
	entries
		.flatten()
		.map(|entry| entry.path())
		.filter(|path| path.extension().and_then(|extension| extension.to_str()) == Some("json"))
		.collect()
}

fn sweep_presence(dir: &Path) {
	for path in presence_files(dir) {
		if read_presence(&path).is_none_or(|record| !presence_live(&record)) {
			let _ = std::fs::remove_file(path);
		}
	}
}

/// True while another live client process owns a presence marker.
pub(crate) fn has_live_presence(runtime_dir: &Path) -> bool {
	let dir = runtime_dir.join(protocol::CLIENTS_DIR);
	let mut live = false;
	for path in presence_files(&dir) {
		match read_presence(&path) {
			Some(record) if presence_live(&record) => live = true,
			_ => {
				let _ = std::fs::remove_file(path);
			},
		}
	}
	live
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn dead_presence_is_pruned_and_live_is_seen() {
		let root = std::env::temp_dir().join(format!("omp-broker-presence-{}", protocol::random_hex(8).unwrap()));
		let dir = root.join(protocol::CLIENTS_DIR);
		protocol::ensure_private_dir(&dir).unwrap();
		let mut exited = std::process::Command::new("sh").arg("-c").arg(":").spawn().unwrap();
		let dead_pid = exited.id();
		let _ = exited.wait();
		let dead = PresenceRecord { pid: dead_pid, start_time: protocol::process_start_time(dead_pid), started_at: 0 };
		protocol::write_private_new(&dir.join("dead.json"), &serde_json::to_vec(&dead).unwrap()).unwrap();
		assert!(!has_live_presence(&root));
		assert!(!dir.join("dead.json").exists());
		let live = PresenceRecord {
			pid: std::process::id(),
			start_time: protocol::process_start_time(std::process::id()),
			started_at: protocol::now_ms(),
		};
		protocol::write_private_new(&dir.join("live.json"), &serde_json::to_vec(&live).unwrap()).unwrap();
		assert!(has_live_presence(&root));
		let _ = std::fs::remove_dir_all(root);
	}

	#[test]
	fn generated_background_names_are_valid_and_unique() {
		let backend = BrokerProcessBackend {
			client: Arc::new(unsafe_placeholder_client()),
			async_enabled: AtomicBool::new(true),
			name_seq: AtomicU64::new(0),
		};
		let first = backend.next_background_name();
		let second = backend.next_background_name();
		assert_ne!(first, second);
	}

	/// Name generation never touches the client, so tests can stub it.
	fn unsafe_placeholder_client() -> BrokerClient {
		BrokerClient {
			project_dir: PathBuf::new(),
			runtime_dir: PathBuf::new(),
			endpoint: PathBuf::new(),
			token: String::new(),
			spawn: false,
			next_id: AtomicU64::new(0),
			_presence: Presence { path: PathBuf::new() },
		}
	}
}
