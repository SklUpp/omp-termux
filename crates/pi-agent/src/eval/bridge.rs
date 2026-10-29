//! Loopback HTTP bridge used by the Python/Ruby/Julia preludes and the JS VM.
//!
//! The endpoint is `127.0.0.1` only, requires a per-process bearer token, bounds
//! request headers/bodies, and compares the token in constant time. The token is
//! passed to a kernel through its spawn environment and is never logged.

use std::{
	collections::HashMap,
	sync::{
		Arc, Mutex,
		atomic::{AtomicU64, Ordering},
	},
	time::Duration,
};

use serde::Deserialize;
use serde_json::{Map, Value};
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt},
	net::{TcpListener, TcpStream},
	sync::mpsc,
	task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::state::AgentContext;

use super::{
	host::{
		EVAL_AGENT_BRIDGE_NAME, EVAL_BUDGET_BRIDGE_NAME, EVAL_COMPLETION_BRIDGE_NAME,
		EVAL_CONCURRENCY_BRIDGE_NAME, EvalHost, normalize_bridge_args, reject_bridged_tool,
		summarize_tool_result, tool_value,
	},
	types::{EvalError, EvalStatusEvent},
	util,
};

/// Synthetic status op: a bridge call left the cell runtime.
pub const EVAL_TIMEOUT_PAUSE_OP: &str = "timeout-pause";
/// Synthetic status op: control returned to the cell runtime.
pub const EVAL_TIMEOUT_RESUME_OP: &str = "timeout-resume";
/// Maximum accepted request head.
const MAX_HEAD_BYTES: usize = 16 * 1024;
/// Maximum accepted JSON body.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
/// Idle keep-alive bound for one loopback connection.
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(60);
/// Sleep window for an idle accept loop before checking shutdown.
const ACCEPT_POLL: Duration = Duration::from_millis(200);

#[derive(Clone)]
struct Registration {
	host: Arc<dyn EvalHost>,
	ctx: AgentContext,
	intent: String,
	cancellation: CancellationToken,
	status: mpsc::UnboundedSender<EvalStatusEvent>,
}

#[derive(Clone)]
pub struct BridgeEndpoint {
	pub url: String,
	pub token: String,
}

impl std::fmt::Debug for BridgeEndpoint {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		// Never render the token.
		formatter.debug_struct("BridgeEndpoint").field("url", &self.url).finish_non_exhaustive()
	}
}

/// Running bridge server.
pub struct Bridge {
	endpoint: BridgeEndpoint,
	registrations: Arc<Mutex<HashMap<String, Registration>>>,
	accept: Mutex<Option<JoinHandle<()>>>,
	shutdown: CancellationToken,
	connections: Arc<AtomicU64>,
}

impl Bridge {
	pub async fn start() -> Result<Self, EvalError> {
		let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
		let address = listener.local_addr()?;
		let token = util::random_token();
		let endpoint = BridgeEndpoint { url: format!("http://{address}"), token: token.clone() };
		let registrations = Arc::new(Mutex::new(HashMap::new()));
		let shutdown = CancellationToken::new();
		let connections = Arc::new(AtomicU64::new(0));
		let accept = tokio::spawn(accept_loop(
			listener,
			token,
			Arc::clone(&registrations),
			shutdown.clone(),
			Arc::clone(&connections),
		));
		Ok(Self {
			endpoint,
			registrations,
			accept: Mutex::new(Some(accept)),
			shutdown,
			connections,
		})
	}

	pub fn endpoint(&self) -> &BridgeEndpoint {
		&self.endpoint
	}

	pub fn connection_count(&self) -> u64 {
		self.connections.load(Ordering::SeqCst)
	}

	/// Register one execution. The returned receiver carries host-side status
	/// events (including timeout pause/resume controls) for the cell.
	pub fn register(
		&self,
		session_id: &str,
		run_id: &str,
		host: Arc<dyn EvalHost>,
		ctx: AgentContext,
		intent: impl Into<String>,
		cancellation: CancellationToken,
	) -> mpsc::UnboundedReceiver<EvalStatusEvent> {
		let (status, receiver) = mpsc::unbounded_channel();
		let registration = Registration {
			host,
			ctx,
			intent: intent.into(),
			cancellation,
			status,
		};
		self
			.registrations
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
			.insert(registration_key(session_id, run_id), registration);
		receiver
	}

	pub fn unregister(&self, session_id: &str, run_id: &str) {
		self
			.registrations
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
			.remove(&registration_key(session_id, run_id));
	}

	pub async fn shutdown(&self) {
		self.shutdown.cancel();
		let accept = self.accept.lock().unwrap_or_else(|poison| poison.into_inner()).take();
		if let Some(accept) = accept {
			let _ = tokio::time::timeout(Duration::from_secs(1), accept).await;
		}
		self
			.registrations
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
			.clear();
	}
}

impl Drop for Bridge {
	fn drop(&mut self) {
		self.shutdown.cancel();
		if let Some(accept) = self.accept.lock().unwrap_or_else(|poison| poison.into_inner()).take() {
			accept.abort();
		}
	}
}

fn registration_key(session_id: &str, run_id: &str) -> String {
	format!("{session_id}:{run_id}")
}

async fn accept_loop(
	listener: TcpListener,
	token: String,
	registrations: Arc<Mutex<HashMap<String, Registration>>>,
	shutdown: CancellationToken,
	connections: Arc<AtomicU64>,
) {
	loop {
		let accepted = tokio::select! {
			() = shutdown.cancelled() => break,
			result = listener.accept() => result,
		};
		match accepted {
			Ok((stream, _)) => {
				connections.fetch_add(1, Ordering::SeqCst);
				let token = token.clone();
				let registrations = Arc::clone(&registrations);
				tokio::spawn(async move {
					let _ = tokio::time::timeout(
						CONNECTION_TIMEOUT,
						handle_connection(stream, token, registrations),
					)
					.await;
				});
			},
			Err(_) => {
				// A transient accept error (EMFILE) must not spin; back off.
				tokio::time::sleep(ACCEPT_POLL).await;
			},
		}
	}
}

#[derive(Debug, Deserialize)]
struct RequestBody {
	#[serde(default)]
	session: String,
	#[serde(default)]
	run: String,
	#[serde(default)]
	name: String,
	#[serde(default)]
	args: Value,
}

async fn handle_connection(
	mut stream: TcpStream,
	token: String,
	registrations: Arc<Mutex<HashMap<String, Registration>>>,
) {
	let Some((method, path, headers, leftover)) = read_head(&mut stream).await else {
		let _ = write_response(&mut stream, 400, "text/plain", b"bad request").await;
		return;
	};
	if method != "POST" || path != "/v1/tool" {
		let _ = write_response(&mut stream, 404, "text/plain", b"not found").await;
		return;
	}
	let Some(authorization) = headers.get("authorization") else {
		let _ = write_response(&mut stream, 403, "text/plain", b"forbidden").await;
		return;
	};
	let presented = authorization.strip_prefix("Bearer ").unwrap_or("");
	if !constant_time_eq(presented.as_bytes(), token.as_bytes()) {
		let _ = write_response(&mut stream, 403, "text/plain", b"forbidden").await;
		return;
	}
	let Some(length) = headers.get("content-length").and_then(|value| value.parse::<usize>().ok()) else {
		let _ = write_response_json(&mut stream, 400, &failure("Missing Content-Length")).await;
		return;
	};
	if length > MAX_BODY_BYTES {
		let _ = write_response_json(&mut stream, 413, &failure("Request body too large")).await;
		return;
	}
	let mut body = leftover;
	if body.len() > length {
		body.truncate(length);
	}
	if body.len() < length {
		let mut rest = vec![0u8; length - body.len()];
		if stream.read_exact(&mut rest).await.is_err() {
			let _ = write_response_json(&mut stream, 400, &failure("Incomplete body")).await;
			return;
		}
		body.extend_from_slice(&rest);
	}
	let Ok(request) = serde_json::from_slice::<RequestBody>(&body) else {
		let _ = write_response_json(&mut stream, 400, &failure("Invalid JSON body")).await;
		return;
	};
	if request.session.is_empty() || request.run.is_empty() || request.name.is_empty() {
		let _ = write_response_json(&mut stream, 400, &failure("Missing session/run/name")).await;
		return;
	}
	let registration = registrations
		.lock()
		.unwrap_or_else(|poison| poison.into_inner())
		.get(&registration_key(&request.session, &request.run))
		.cloned();
	let Some(registration) = registration else {
		let _ = write_response_json(
			&mut stream,
			200,
			&failure(&format!(
				"No active eval bridge session: {}",
				registration_key(&request.session, &request.run)
			)),
		)
		.await;
		return;
	};
	if registration.cancellation.is_cancelled() {
		let _ = write_response_json(&mut stream, 200, &failure("eval cell was interrupted")).await;
		return;
	}
	if let Some(error) = reject_bridged_tool(&request.name) {
		let _ = write_response_json(&mut stream, 200, &failure(&error.to_string())).await;
		return;
	}
	let result = dispatch(&registration, request.name.as_str(), request.args).await;
	match result {
		Ok(value) => {
			let _ = write_response_json(&mut stream, 200, &serde_json::json!({"ok": true, "value": value})).await;
		},
		Err(error) => {
			let _ = write_response_json(&mut stream, 200, &failure(&error)).await;
		},
	}
}

fn failure(message: &str) -> Value {
	serde_json::json!({"ok": false, "error": message})
}

async fn dispatch(registration: &Registration, name: &str, args: Value) -> Result<Value, String> {
	match name {
		EVAL_COMPLETION_BRIDGE_NAME => {
			emit(registration, EvalStatusEvent::new(EVAL_TIMEOUT_PAUSE_OP));
			let result = registration.host.completion(args, &registration.ctx).await;
			emit(registration, EvalStatusEvent::new(EVAL_TIMEOUT_RESUME_OP));
			result
		},
		EVAL_AGENT_BRIDGE_NAME => {
			emit(registration, EvalStatusEvent::new(EVAL_TIMEOUT_PAUSE_OP));
			let result = registration.host.agent(args, &registration.ctx).await;
			emit(registration, EvalStatusEvent::new(EVAL_TIMEOUT_RESUME_OP));
			result
		},
		EVAL_BUDGET_BRIDGE_NAME => registration.host.budget(args, &registration.ctx).await,
		EVAL_CONCURRENCY_BRIDGE_NAME => registration.host.concurrency(&args),
		_ => {
			let args = normalize_bridge_args(args, &registration.intent);
			let result = registration
				.host
				.call_tool(name, args.clone(), &registration.ctx)
				.await
				.map_err(|error| error.to_string())?;
			let event = summarize_tool_result(name, &args, &result);
			emit(registration, event);
			let images = result
				.details
				.as_ref()
				.and_then(|details| details.get("images"))
				.and_then(Value::as_array)
				.map(|images| images.clone())
				.unwrap_or_default();
			Ok(tool_value(result, images))
		},
	}
}

fn emit(registration: &Registration, event: EvalStatusEvent) {
	// A dropped receiver means the cell settled; the event is no longer useful.
	let _ = registration.status.send(event);
}

async fn read_head(
	stream: &mut TcpStream,
) -> Option<(String, String, HashMap<String, String>, Vec<u8>)> {
	let mut buffer = Vec::with_capacity(1024);
	let mut chunk = [0u8; 1024];
	let head_end = loop {
		let read = stream.read(&mut chunk).await.ok()?;
		if read == 0 {
			return None;
		}
		buffer.extend_from_slice(&chunk[..read]);
		if let Some(offset) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
			break offset + 4;
		}
		if buffer.len() > MAX_HEAD_BYTES {
			return None;
		}
	};
	let leftover = buffer.split_off(head_end);
	let text = String::from_utf8(buffer).ok()?;
	let mut lines = text.split("\r\n");
	let request_line = lines.next()?;
	let mut parts = request_line.split_whitespace();
	let method = parts.next()?.to_string();
	let path = parts.next()?.to_string();
	let mut headers = HashMap::new();
	for line in lines {
		if line.is_empty() {
			break;
		}
		let (key, value) = line.split_once(':')?;
		headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
	}
	Some((method, path, headers, leftover))
}

async fn write_response_json(stream: &mut TcpStream, status: u16, value: &Value) {
	let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{\"ok\":false}".to_vec());
	let _ = write_response(stream, status, "application/json", &body).await;
}

async fn write_response(
	stream: &mut TcpStream,
	status: u16,
	content_type: &str,
	body: &[u8],
) -> std::io::Result<()> {
	let reason = match status {
		200 => "OK",
		400 => "Bad Request",
		403 => "Forbidden",
		404 => "Not Found",
		413 => "Payload Too Large",
		_ => "Error",
	};
	let head = format!(
		"HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
		body.len()
	);
	stream.write_all(head.as_bytes()).await?;
	stream.write_all(body).await?;
	stream.flush().await
}

/// Length-independent comparison of the presented token.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
	let mut difference = left.len() ^ right.len();
	for index in 0..left.len().max(right.len()) {
		let a = left.get(index).copied().unwrap_or(0);
		let b = right.get(index).copied().unwrap_or(0);
		difference |= usize::from(a ^ b);
	}
	difference == 0
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn constant_time_compare_rejects_prefix_and_length_changes() {
		assert!(constant_time_eq(b"secret", b"secret"));
		assert!(!constant_time_eq(b"secret", b"secrez"));
		assert!(!constant_time_eq(b"secret", b"secret2"));
		assert!(!constant_time_eq(b"", b"x"));
	}
}
