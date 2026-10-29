//! Client half of the broker-shared LSP mux.
//!
//! Port of `packages/coding-agent/src/lsp/mux/daemon.ts`. Ensures the
//! per-project mux daemon is running under the existing daemon broker, dials
//! its socket, performs the [`MUX_CONNECT_METHOD`] handshake, and returns the
//! raw socket halves. Every failure returns `None` so the caller falls back to
//! a process-local spawn; LSP never regresses on broker trouble.

use std::{
	collections::{BTreeMap, HashMap},
	path::{Path, PathBuf},
	sync::{Arc, LazyLock},
	time::Duration,
};

use serde_json::{Value, json};
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt},
	net::{UnixStream, unix::{OwnedReadHalf, OwnedWriteHalf}},
	sync::Mutex,
	time::{Instant, sleep, timeout},
};

use crate::{
	lsp::framing::MessageFramer,
	process_broker::{
		client::BrokerClient,
		protocol::{self, WireSpec},
	},
	tools::process::{ReadySpec, RestartPolicy},
};

use super::{
	DAEMON_NAME, MUX_CONNECT_METHOD, MUX_PING_METHOD, MUX_PING_RESULT, MuxConnectParams,
	MuxConnectResult, PROJECT_DIR_ENV, READY_PATTERN, SOCKET_ENV, WORKER_ARG, endpoint_for_project,
	frame,
};

const CONNECT_TIMEOUT: Duration = Duration::from_millis(3_000);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(10_000);
const PROBE_TIMEOUT: Duration = Duration::from_millis(1_500);
const READY_TIMEOUT: Duration = Duration::from_millis(15_000);

/// Connected, handshaken mux link.
pub struct SharedTransport {
	pub reader: OwnedReadHalf,
	pub writer: OwnedWriteHalf,
	/// Bytes received after the handshake response.
	pub leftover: Vec<u8>,
	pub pid: u32,
}

/// Dial the project mux, starting it under the broker when necessary.
///
/// Returns `None` on any shared-path failure (no broker, no socket, handshake
/// error) so callers spawn a private server instead.
pub async fn connect(project_dir: &Path, params: &MuxConnectParams) -> Option<SharedTransport> {
	let endpoint = endpoint_for_project(project_dir).ok()?;
	if !probe(&endpoint).await {
		ensure_daemon(project_dir, &endpoint).await;
	}
	dial(&endpoint, params).await
}

async fn dial(endpoint: &Path, params: &MuxConnectParams) -> Option<SharedTransport> {
	let stream = timeout(CONNECT_TIMEOUT, UnixStream::connect(endpoint))
		.await
		.ok()?
		.ok()?;
	let (mut reader, mut writer) = stream.into_split();
	let request = json!({
		"jsonrpc": "2.0",
		"id": "connect:1",
		"method": MUX_CONNECT_METHOD,
		"params": params,
	});
	writer.write_all(frame(&request).as_bytes()).await.ok()?;
	writer.flush().await.ok()?;
	let (result, leftover) = timeout(HANDSHAKE_TIMEOUT, read_handshake(&mut reader))
		.await
		.ok()??;
	let parsed = serde_json::from_value::<MuxConnectResult>(result).ok()?;
	Some(SharedTransport {
		reader,
		writer,
		leftover,
		pid: parsed.pid.unwrap_or(0),
	})
}

/// Read messages until the handshake response arrives; return its result and
/// any bytes that followed it.
async fn read_handshake(reader: &mut OwnedReadHalf) -> Option<(Value, Vec<u8>)> {
	let mut framer = MessageFramer::new();
	let mut buffer = [0u8; 8 * 1024];
	loop {
		let count = reader.read(&mut buffer).await.ok()?;
		if count == 0 {
			return None;
		}
		framer.push(&buffer[..count]);
		let messages = framer.drain(|_| {}).ok()?;
		for text in messages {
			let Ok(message) = serde_json::from_str::<Value>(&text) else {
				continue;
			};
			if message.get("id").and_then(Value::as_str) != Some("connect:1") {
				continue;
			}
			if message.get("error").is_some() {
				return None;
			}
			return Some((message.get("result").cloned().unwrap_or(Value::Null), framer.remainder()));
		}
	}
}

/// True when a mux answers the ping handshake at `endpoint`.
async fn probe(endpoint: &Path) -> bool {
	let Ok(Ok(stream)) = timeout(PROBE_TIMEOUT, UnixStream::connect(endpoint)).await else {
		return false;
	};
	let (mut reader, mut writer) = stream.into_split();
	let request = json!({ "jsonrpc": "2.0", "id": "probe", "method": MUX_PING_METHOD, "params": null });
	if writer.write_all(frame(&request).as_bytes()).await.is_err() {
		return false;
	}
	let mut framer = MessageFramer::new();
	let mut buffer = [0u8; 4 * 1024];
	let deadline = Instant::now() + PROBE_TIMEOUT;
	while Instant::now() < deadline {
		let remaining = deadline.saturating_duration_since(Instant::now());
		let Ok(Ok(count)) = timeout(remaining, reader.read(&mut buffer)).await else {
			return false;
		};
		if count == 0 {
			return false;
		}
		framer.push(&buffer[..count]);
		let Ok(messages) = framer.drain(|_| {}) else {
			return false;
		};
		for text in messages {
			if let Ok(message) = serde_json::from_str::<Value>(&text)
				&& message.get("result").and_then(Value::as_str) == Some(MUX_PING_RESULT)
			{
				return true;
			}
		}
	}
	false
}

/// Start the mux worker through the daemon broker and wait until it answers.
async fn ensure_daemon(project_dir: &Path, endpoint: &Path) {
	if probe(endpoint).await {
		return;
	}
	let Some(broker) = broker_for(project_dir).await else {
		return;
	};
	let Ok(project) = project_dir.canonicalize() else {
		return;
	};
	let Ok(application) = std::env::current_exe() else {
		return;
	};
	let spec = WireSpec {
		name: DAEMON_NAME.to_string(),
		application: application.to_string_lossy().into_owned(),
		args: vec![WORKER_ARG.to_string()],
		env: BTreeMap::from([
			(SOCKET_ENV.to_string(), endpoint.to_string_lossy().into_owned()),
			(
				PROJECT_DIR_ENV.to_string(),
				project.to_string_lossy().into_owned(),
			),
		]),
		full_env: false,
		cwd: project.to_string_lossy().into_owned(),
		pty: false,
		ready: Some(ReadySpec {
			log: Some(READY_PATTERN.trim_end().to_string()),
			port: None,
			host: None,
			timeout_ms: u64::try_from(READY_TIMEOUT.as_millis()).unwrap_or(15_000),
		}),
		restart: RestartPolicy::No,
		persist: false,
		detached: false,
	};
	// A concurrent start may already own the name; the probe loop below adopts it.
	let _ = broker.start(spec, Some("lsp-mux")).await;
	let deadline = Instant::now() + READY_TIMEOUT;
	while Instant::now() < deadline {
		if probe(endpoint).await {
			return;
		}
		sleep(Duration::from_millis(100)).await;
	}
}

/// Broker clients double as the presence lease keeping the mux's broker alive.
static BROKERS: LazyLock<Mutex<HashMap<PathBuf, Arc<BrokerClient>>>> =
	LazyLock::new(|| Mutex::new(HashMap::new()));

async fn broker_for(project_dir: &Path) -> Option<Arc<BrokerClient>> {
	let project = protocol::canonical_project_dir(project_dir);
	if let Some(client) = BROKERS.lock().await.get(&project) {
		return Some(client.clone());
	}
	let client = BrokerClient::connect(&project).await.ok()?;
	let mut brokers = BROKERS.lock().await;
	Some(
		brokers
			.entry(project)
			.or_insert(client)
			.clone(),
	)
}
