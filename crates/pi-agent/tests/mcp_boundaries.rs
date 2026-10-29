//! Supported MCP consumer boundaries against real localhost servers:
//! cursor pagination, header precedence and session/protocol propagation,
//! idempotent reconnect after a retriable failure, origin-locked redirect
//! refusal, and closing while a stdio request is still pending.
//!
//! OAuth credential storage is rejected by config validation and is not
//! exercised here; that remains an explicit unsupported feature, not a gap
//! this file can paper over.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_agent::mcp::{
	MCPServerConfig, MCPHttpServerConfig, MCPStdioServerConfig, McpError, connect_to_server,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Clone)]
struct Seen {
	headers: String,
	body: Value,
}

struct Reply {
	status: u16,
	headers: Vec<(&'static str, String)>,
	body: String,
}

impl Reply {
	fn json(body: String) -> Self {
		Self {
			status: 200,
			headers: vec![("Content-Type", "application/json".to_string())],
			body,
		}
	}

	fn with_header(mut self, name: &'static str, value: &str) -> Self {
		self.headers.push((name, value.to_string()));
		self
	}
}

struct MockServer {
	url: String,
	seen: Arc<Mutex<Vec<Seen>>>,
	handle: tokio::task::JoinHandle<()>,
}

impl Drop for MockServer {
	fn drop(&mut self) {
		self.handle.abort();
	}
}

fn reason(status: u16) -> &'static str {
	match status {
		200 => "OK",
		302 => "Found",
		503 => "Service Unavailable",
		_ => "Unknown",
	}
}

async fn read_seen(socket: &mut tokio::net::TcpStream) -> Option<Seen> {
	let mut buf = Vec::new();
	let mut header_end = None;
	let mut chunk = [0u8; 1024];
	let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
	while header_end.is_none() && tokio::time::Instant::now() < deadline {
		let n = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut chunk))
			.await
			.ok()?
			.ok()?;
		if n == 0 {
			return None;
		}
		buf.extend_from_slice(&chunk[..n]);
		header_end = buf.windows(4).position(|w| w == b"\r\n\r\n");
	}
	let header_end = header_end?;
	let headers = String::from_utf8_lossy(&buf[..header_end]).into_owned();
	let content_length = headers
		.lines()
		.find_map(|line| {
			line.to_ascii_lowercase()
				.strip_prefix("content-length:")
				.map(|value| value.trim().parse::<usize>().unwrap_or(0))
		})
		.unwrap_or(0);
	let body_start = header_end + 4;
	let mut body = buf[body_start..].to_vec();
	while body.len() < content_length {
		let n = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut chunk))
			.await
			.ok()?
			.ok()?;
		if n == 0 {
			break;
		}
		body.extend_from_slice(&chunk[..n]);
	}
	Some(Seen { headers, body: serde_json::from_slice(&body).unwrap_or(Value::Null) })
}

async fn spawn_mock(
	handler: impl Fn(usize, &Seen) -> Reply + Send + Sync + 'static,
) -> MockServer {
	let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
	let addr = listener.local_addr().expect("addr");
	let seen = Arc::new(Mutex::new(Vec::new()));
	let handler = Arc::new(handler);
	let seen_for_server = Arc::clone(&seen);
	let handle = tokio::spawn(async move {
		loop {
			let Ok((mut socket, _)) = listener.accept().await else {
				return;
			};
			let handler = Arc::clone(&handler);
			let seen = Arc::clone(&seen_for_server);
			tokio::spawn(async move {
				let Some(request) = read_seen(&mut socket).await else {
					return;
				};
				let index = {
					let mut requests = seen.lock().unwrap();
					let index = requests.len();
					requests.push(request.clone());
					index
				};
				let reply = (*handler)(index, &request);
				let mut response = format!(
					"HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
					reply.status,
					reason(reply.status),
					reply.body.len()
				);
				for (name, value) in reply.headers {
					response.push_str(&format!("{name}: {value}\r\n"));
				}
				response.push_str("\r\n");
				response.push_str(&reply.body);
				let _ = socket.write_all(response.as_bytes()).await;
				let _ = socket.flush().await;
			});
		}
	});
	MockServer { url: format!("http://{addr}/mcp"), seen, handle }
}

fn rpc_result(body: &Value, result: Value) -> String {
	json!({
		"jsonrpc": "2.0",
		"id": body.get("id").cloned().unwrap_or(Value::Null),
		"result": result,
	})
	.to_string()
}

fn initialize_result() -> Value {
	json!({
		"protocolVersion": "2025-11-25",
		"capabilities": { "tools": {} },
		"serverInfo": { "name": "mock", "version": "1.0.0" },
	})
}

fn http_config(url: &str) -> MCPHttpServerConfig {
	MCPHttpServerConfig {
		url: url.to_string(),
		headers: HashMap::new(),
		timeout: Some(5_000),
		request_id_format: None,
		header_policy: None,
		auth: None,
		oauth: None,
	}
}

#[tokio::test]
async fn tools_list_follows_cursor_pages_in_order() {
	let server = spawn_mock(|_, seen| match seen.body.get("method").and_then(Value::as_str) {
		Some("initialize") => Reply::json(rpc_result(&seen.body, initialize_result()))
			.with_header("mcp-session-id", "sess-1"),
		Some("notifications/initialized") => Reply::json("{}".to_string()),
		Some("tools/list") => {
			let cursor = seen.body.get("params").and_then(|params| params.get("cursor"));
			if cursor.is_none() {
				Reply::json(rpc_result(
					&seen.body,
					json!({
						"tools": [{ "name": "alpha", "inputSchema": {} }],
						"nextCursor": "page-2",
					}),
				))
			} else {
				Reply::json(rpc_result(
					&seen.body,
					json!({ "tools": [{ "name": "beta", "inputSchema": {} }] }),
				))
			}
		},
		_ => Reply::json(rpc_result(&seen.body, json!({}))),
	})
	.await;

	let connection = connect_to_server("pages", MCPServerConfig::Http(http_config(&server.url)))
		.await
		.expect("connect");
	let tools = connection.list_tools().await.expect("paginated tools");
	let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
	assert_eq!(names, ["alpha", "beta"]);

	let requests = server.seen.lock().unwrap().clone();
	let list_calls: Vec<&Seen> = requests
		.iter()
		.filter(|seen| seen.body.get("method").and_then(Value::as_str) == Some("tools/list"))
		.collect();
	assert_eq!(list_calls.len(), 2, "one request per cursor page");
	assert_eq!(list_calls[0].body["params"], json!({}));
	assert_eq!(list_calls[1].body["params"]["cursor"], "page-2");

	connection.close().await.expect("close");
	connection.close().await.expect("close is idempotent for HTTP");
}

#[tokio::test]
async fn configured_headers_lose_to_generated_and_session_headers_follow() {
	let server = spawn_mock(|_, seen| match seen.body.get("method").and_then(Value::as_str) {
		Some("initialize") => Reply::json(rpc_result(&seen.body, initialize_result()))
			.with_header("mcp-session-id", "sess-1"),
		Some("notifications/initialized") => Reply::json("{}".to_string()),
		_ => Reply::json(rpc_result(&seen.body, json!({}))),
	})
	.await;

	let mut config = http_config(&server.url);
	config.headers = HashMap::from([
		("authorization".to_string(), "Bearer tok".to_string()),
		("x-api-key".to_string(), "k-1".to_string()),
		("Content-Type".to_string(), "text/plain".to_string()),
		("mcp-protocol-version".to_string(), "spoofed".to_string()),
	]);
	connect_to_server("headers", MCPServerConfig::Http(config))
		.await
		.expect("connect");

	let requests = server.seen.lock().unwrap().clone();
	let init_headers = requests[0].headers.to_ascii_lowercase();
	assert!(init_headers.contains("authorization: bearer tok"), "{init_headers}");
	assert!(init_headers.contains("x-api-key: k-1"), "{init_headers}");
	assert!(init_headers.contains("content-type: application/json"), "{init_headers}");
	assert!(!init_headers.contains("text/plain"), "generated content-type wins");
	assert!(
		!init_headers.contains("mcp-protocol-version"),
		"configured protocol version must be stripped before negotiation"
	);

	let notify_headers = requests[1].headers.to_ascii_lowercase();
	assert!(
		notify_headers.contains("mcp-session-id: sess-1"),
		"server session id must be replayed: {notify_headers}"
	);
	assert!(
		notify_headers.contains("mcp-protocol-version: 2025-11-25"),
		"negotiated protocol version must be sent after initialize: {notify_headers}"
	);
}

#[tokio::test]
async fn retriable_tools_list_failure_reconnects_and_retries_once() {
	let list_attempts = Arc::new(AtomicUsize::new(0));
	let attempts = Arc::clone(&list_attempts);
	let server = spawn_mock(move |_, seen| match seen.body.get("method").and_then(Value::as_str) {
		Some("initialize") => Reply::json(rpc_result(&seen.body, initialize_result())),
		Some("notifications/initialized") => Reply::json("{}".to_string()),
		Some("tools/list") => {
			let attempt = attempts.fetch_add(1, Ordering::SeqCst);
			if attempt == 0 {
				Reply {
					status: 503,
					headers: vec![("Content-Type", "text/plain".to_string())],
					body: "temporarily unavailable".to_string(),
				}
			} else {
				Reply::json(rpc_result(
					&seen.body,
					json!({ "tools": [{ "name": "after-retry", "inputSchema": {} }] }),
				))
			}
		},
		_ => Reply::json(rpc_result(&seen.body, json!({}))),
	})
	.await;

	let connection = connect_to_server("retry", MCPServerConfig::Http(http_config(&server.url)))
		.await
		.expect("connect");
	let tools = connection.list_tools().await.expect("retried tools");
	assert_eq!(tools.len(), 1);
	assert_eq!(tools[0].name, "after-retry");
	assert_eq!(list_attempts.load(Ordering::SeqCst), 2);

	let requests = server.seen.lock().unwrap().clone();
	let initialize_calls = requests
		.iter()
		.filter(|seen| seen.body.get("method").and_then(Value::as_str) == Some("initialize"))
		.count();
	assert_eq!(initialize_calls, 2, "reconnect must re-run the handshake");
}

#[tokio::test]
async fn origin_locked_redirect_is_refused_before_a_second_server_is_contacted() {
	let target = spawn_mock(|_, seen| Reply::json(rpc_result(&seen.body, json!({})))).await;
	let target_hits = Arc::clone(&target.seen);

	let redirector = spawn_mock(move |_, _| Reply {
		status: 302,
		headers: vec![("Location", target.url.clone())],
		body: String::new(),
	})
	.await;

	let mut config = http_config(&redirector.url);
	config.header_policy = Some("origin-locked".to_string());
	let err = connect_to_server("locked", MCPServerConfig::Http(config))
		.await
		.expect_err("cross-origin redirect must be refused");
	match err {
		McpError::Transport(message) => {
			assert!(
				message.contains("Refusing non-method-preserving redirect 302"),
				"{message}"
			);
		},
		other => panic!("expected transport refusal, got {other:?}"),
	}
	assert!(
		target_hits.lock().unwrap().is_empty(),
		"refused redirect must never reach the other origin"
	);
}

#[tokio::test]
async fn close_drains_a_pending_stdio_request_and_kills_the_child() {
	let dir = std::env::temp_dir();
	let unique = format!("omp-mcp-close-{}", std::process::id());
	let script = dir.join(format!("{unique}.py"));
	let pid_file = dir.join(format!("{unique}.pid"));
	let marker = dir.join(format!("{unique}.called"));
	std::fs::write(
		&script,
		r#"
import json, os, sys, time

open(sys.argv[1], "w").write(str(os.getpid()))
for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get("method")
    mid = msg.get("id")
    if method == "tools/call":
        open(sys.argv[2], "w").write("called")
        time.sleep(300)
        continue
    if mid is not None:
        result = {
            "protocolVersion": "2025-11-25",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "mock-close", "version": "1.0.0"},
        }
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "result": result}) + "\n")
        sys.stdout.flush()
"#,
	)
	.expect("write script");

	let config = MCPServerConfig::Stdio(MCPStdioServerConfig {
		command: "python3".to_string(),
		args: vec![
			script.to_string_lossy().into_owned(),
			pid_file.to_string_lossy().into_owned(),
			marker.to_string_lossy().into_owned(),
		],
		env: HashMap::new(),
		cwd: None,
		timeout: Some(5_000),
		request_id_format: None,
	});
	let connection = Arc::new(connect_to_server("close", config).await.expect("connect"));
	let pid: i32 = std::fs::read_to_string(&pid_file)
		.expect("pid file")
		.trim()
		.parse()
		.expect("pid");

	let caller = Arc::clone(&connection);
	let pending = tokio::spawn(async move { caller.call_tool("wait_forever", json!({})).await });
	let start = tokio::time::Instant::now();
	while !marker.exists() {
		assert!(
			start.elapsed() < Duration::from_secs(3),
			"server never observed tools/call"
		);
		tokio::time::sleep(Duration::from_millis(10)).await;
	}

	connection.close().await.expect("close");
	let result = tokio::time::timeout(Duration::from_secs(2), pending)
		.await
		.expect("pending request must settle on close")
		.expect("task");
	assert!(matches!(result, Err(McpError::Closed)), "{result:?}");

	let alive = std::process::Command::new("kill")
		.args(["-0", &pid.to_string()])
		.status()
		.is_ok_and(|status| status.success());
	assert!(!alive, "close must terminate the stdio child");

	let _ = std::fs::remove_file(&script);
	let _ = std::fs::remove_file(&pid_file);
	let _ = std::fs::remove_file(&marker);
}
