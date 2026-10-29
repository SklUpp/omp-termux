//! Behavioral regression tests for typed toolResult image preservation and replay:
//! - v3 session log preserves ordered text and image parts in toolResult messages
//! - Replay formats OpenAI Chat wire with grouped tool responses followed by a single user image message
//! - Consecutive parallel tool results group their images without intermediate user messages
//! - Image-only tool results use "(see attached image)" text placeholder on the tool message
//! - Aborted turns with tool result images replay correctly
//! - Malformed/external image URLs fail closed before any HTTP request is issued
//! - Legacy string-only tool results replay cleanly without synthetic image messages

use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcCommand, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use omp_cli::session::{ContentPart, SessionOptions, SessionStore, ToolCallRecord, TurnRecord};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_dir(prefix: &str) -> PathBuf {
	let unique = format!(
		"omp-tool-img-{prefix}-{}-{}",
		std::process::id(),
		COUNTER.fetch_add(1, Ordering::Relaxed)
	);
	let dir = std::env::temp_dir().join(unique);
	fs::create_dir_all(&dir).expect("create temp dir");
	dir
}

fn omp() -> ProcCommand {
	let mut cmd = ProcCommand::new(env!("CARGO_BIN_EXE_omp"));
	cmd.stdin(Stdio::null());
	cmd
}

struct MockWireServer {
	port: u16,
	requests: Arc<Mutex<Vec<serde_json::Value>>>,
	stop_tx: mpsc::Sender<()>,
	handle: Option<thread::JoinHandle<()>>,
}

impl MockWireServer {
	fn start() -> Self {
		let listener = TcpListener::bind("127.0.0.1:0").expect("bind tcp");
		let port = listener.local_addr().expect("local addr").port();
		listener.set_nonblocking(true).expect("set nonblocking");
		let (stop_tx, stop_rx) = mpsc::channel::<()>();
		let requests = Arc::new(Mutex::new(Vec::new()));
		let reqs_clone = Arc::clone(&requests);

		let handle = thread::spawn(move || loop {
			if stop_rx.try_recv().is_ok() {
				break;
			}
			match listener.accept() {
				Ok((mut stream, _)) => {
					let mut buf = Vec::new();
					let mut temp = [0u8; 4096];
					let mut header_pos = None;

					while header_pos.is_none() {
						match stream.read(&mut temp) {
							Ok(0) => break,
							Ok(n) => {
								buf.extend_from_slice(&temp[..n]);
								if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
									header_pos = Some(pos);
								}
							},
							Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
								thread::sleep(Duration::from_millis(5));
							},
							Err(_) => break,
						}
					}

					if let Some(pos) = header_pos {
						let header_str = String::from_utf8_lossy(&buf[..pos]);
						let content_len: usize = header_str
							.lines()
							.find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
							.and_then(|l| l.split(':').nth(1))
							.and_then(|v| v.trim().parse().ok())
							.unwrap_or(0);

						while buf.len() < pos + 4 + content_len {
							match stream.read(&mut temp) {
								Ok(0) => break,
								Ok(n) => buf.extend_from_slice(&temp[..n]),
								Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
									thread::sleep(Duration::from_millis(5));
								},
								Err(_) => break,
							}
						}

						if buf.len() >= pos + 4 + content_len {
							let body = &buf[pos + 4..pos + 4 + content_len];
							if let Ok(json) = serde_json::from_slice::<serde_json::Value>(body) {
								reqs_clone.lock().expect("lock").push(json);
							}
						}
					}

					let body = concat!(
						"data: {\"choices\":[{\"delta\":{\"content\":\"Understood tool results\"}}]}\n\n",
						"data: [DONE]\n\n"
					);
					let response = format!(
						"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
						body.len(),
						body
					);
					let _ = stream.write_all(response.as_bytes());
					let _ = stream.flush();
				},
				Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
					thread::sleep(Duration::from_millis(15));
				},
				Err(_) => break,
			}
		});

		Self {
			port,
			requests,
			stop_tx,
			handle: Some(handle),
		}
	}

	fn stop(mut self) {
		let _ = self.stop_tx.send(());
		if let Some(h) = self.handle.take() {
			let _ = h.join();
		}
	}
}

fn setup_dirs(base: &Path, port: u16) -> (PathBuf, PathBuf, PathBuf) {
	let agent_dir = base.join("agent");
	let session_dir = base.join("sessions");
	let cwd = base.join("work");
	fs::create_dir_all(&agent_dir).expect("agent dir");
	fs::create_dir_all(&session_dir).expect("session dir");
	fs::create_dir_all(&cwd).expect("cwd");

	let config_yml = format!(
		"defaultModel: mock/vision\nproviders:\n  mock:\n    baseUrl: \"http://127.0.0.1:{port}/v1\"\n    api: openai-completions\n    apiKey: test-key\n"
	);
	fs::write(agent_dir.join("config.yml"), config_yml).expect("write config.yml");

	let models_yml = format!(
		"providers:\n  mock:\n    baseUrl: \"http://127.0.0.1:{port}/v1\"\n    api: openai-completions\n    apiKey: test-key\n    models:\n      - id: vision\n        contextWindow: 128000\n        input: [\"text\", \"image\"]\n      - id: text-only\n        contextWindow: 128000\n        input: [\"text\"]\n"
	);
	fs::write(agent_dir.join("models.yml"), models_yml).expect("write models.yml");

	(agent_dir, session_dir, cwd)
}

#[test]
fn test_cli_tool_result_image_replay_to_openai_chat_wire() {
	let server = MockWireServer::start();
	let base = temp_dir("tool-img-replay");
	let (agent_dir, session_dir, cwd) = setup_dirs(&base, server.port);

	// 1. Establish session with parallel tool calls returning images and text
	let session_id = "sess-tool-img-1";
	{
		let mut store = SessionStore::open(&SessionOptions {
			enabled: true,
			cwd: cwd.clone(),
			model: Some("mock/vision".to_string()),
			provider: Some("mock".to_string()),
			dir: Some(session_dir.clone()),
			id: Some(session_id.to_string()),
			continue_latest: false,
			parent_session: None,
			no_title: false,
		});
		assert!(store.enabled());

		let turn = TurnRecord {
			prompt: "capture scene and verify metrics".to_string(),
			assistant: "running capture tools".to_string(),
			stop_reason: "stop".to_string(),
			tool_calls: vec![
				ToolCallRecord {
					id: "call-cam-1".to_string(),
					name: "camera".to_string(),
					arguments: serde_json::json!({"mode": "high-res"}),
					result: "Photo 1 captured successfully".to_string(),
					is_error: false,
					result_parts: vec![
						ContentPart::text("Photo 1 captured successfully"),
						ContentPart::image("aW1nMQ==", "image/png"),
					],
					details: None,
				},
				ToolCallRecord {
					id: "call-cam-2".to_string(),
					name: "camera".to_string(),
					arguments: serde_json::json!({"mode": "infrared"}),
					result: "Photo 2 captured successfully".to_string(),
					is_error: false,
					result_parts: vec![
						ContentPart::text("Photo 2 captured successfully"),
						ContentPart::image("aW1nMg==", "image/jpeg"),
					],
					details: None,
				},
				ToolCallRecord {
					id: "call-diag-3".to_string(),
					name: "diag".to_string(),
					arguments: serde_json::json!({}),
					result: "all systems green".to_string(),
					is_error: false,
					result_parts: vec![ContentPart::text("all systems green")],
					details: None,
				},
			],
			parts: Vec::new(),
		};
		store.append_turn(&turn).expect("append turn");
	}

	// 2. Resume session via CLI and verify wire message structure
	let mut cmd = omp();
	cmd.env("PI_CODING_AGENT_DIR", &agent_dir);
	cmd.args([
		"--cwd",
		cwd.to_str().unwrap(),
		"--session-dir",
		session_dir.to_str().unwrap(),
		"--resume",
		session_id,
		"--mode",
		"text",
		"--no-tools",
		"summarize diagnostic findings",
	]);
	let out = cmd.output().expect("run resume command");
	assert!(
		out.status.success(),
		"resume must succeed: {}",
		String::from_utf8_lossy(&out.stderr)
	);

	let reqs = server.requests.lock().expect("lock").clone();
	assert_eq!(reqs.len(), 1, "mock server must have received 1 request");
	let messages = reqs[0]["messages"].as_array().expect("messages array");

	// Validate message sequence:
	// - Message 0: User turn 1 prompt
	// - Message 1: Assistant with tool_calls
	// - Messages 2-4: Consecutive tool results (text content only, NO image_url!)
	// - Message 5: Single hoisted User image message with both images
	// - Message 6: User turn 2 prompt
	assert_eq!(messages.len(), 7, "expected 7 messages on wire: {:?}", messages);

	assert_eq!(messages[0]["role"], "user");
	assert_eq!(messages[0]["content"], "capture scene and verify metrics");

	assert_eq!(messages[1]["role"], "assistant");
	let tool_calls = messages[1]["tool_calls"].as_array().expect("tool_calls");
	assert_eq!(tool_calls.len(), 3);

	// Tool call 1
	assert_eq!(messages[2]["role"], "tool");
	assert_eq!(messages[2]["tool_call_id"], "call-cam-1");
	assert_eq!(messages[2]["content"], "Photo 1 captured successfully");

	// Tool call 2
	assert_eq!(messages[3]["role"], "tool");
	assert_eq!(messages[3]["tool_call_id"], "call-cam-2");
	assert_eq!(messages[3]["content"], "Photo 2 captured successfully");

	// Tool call 3
	assert_eq!(messages[4]["role"], "tool");
	assert_eq!(messages[4]["tool_call_id"], "call-diag-3");
	assert_eq!(messages[4]["content"], "all systems green");

	// Hoisted user message with all images
	assert_eq!(messages[5]["role"], "user");
	let image_parts = messages[5]["content"].as_array().expect("image content parts");
	assert_eq!(image_parts.len(), 3); // 1 header text + 2 images
	assert_eq!(image_parts[0]["type"], "text");
	assert_eq!(image_parts[0]["text"], "Attached image(s) from tool result:");

	assert_eq!(image_parts[1]["type"], "image_url");
	assert_eq!(image_parts[1]["image_url"]["url"], "data:image/png;base64,aW1nMQ==");

	assert_eq!(image_parts[2]["type"], "image_url");
	assert_eq!(image_parts[2]["image_url"]["url"], "data:image/jpeg;base64,aW1nMg==");

	// Turn 2 prompt
	assert_eq!(messages[6]["role"], "user");
	assert_eq!(messages[6]["content"], "summarize diagnostic findings");

	server.stop();
}

#[test]
fn test_cli_image_only_tool_result_uses_placeholder_on_wire() {
	let server = MockWireServer::start();
	let base = temp_dir("img-only-placeholder");
	let (agent_dir, session_dir, cwd) = setup_dirs(&base, server.port);

	let session_id = "sess-img-only";
	{
		let mut store = SessionStore::open(&SessionOptions {
			enabled: true,
			cwd: cwd.clone(),
			model: Some("mock/vision".to_string()),
			provider: Some("mock".to_string()),
			dir: Some(session_dir.clone()),
			id: Some(session_id.to_string()),
			continue_latest: false,
			parent_session: None,
			no_title: false,
		});
		assert!(store.enabled());

		let turn = TurnRecord {
			prompt: "snap".to_string(),
			assistant: "ok".to_string(),
			stop_reason: "stop".to_string(),
			tool_calls: vec![ToolCallRecord {
				id: "call-only-img".to_string(),
				name: "camera".to_string(),
				arguments: serde_json::json!({}),
				result: String::new(),
				is_error: false,
				result_parts: vec![ContentPart::image("aW1nMQ==", "image/png")],
				details: None,
			}],
			parts: Vec::new(),
		};
		store.append_turn(&turn).expect("append turn");
	}

	let mut cmd = omp();
	cmd.env("PI_CODING_AGENT_DIR", &agent_dir);
	cmd.args([
		"--cwd",
		cwd.to_str().unwrap(),
		"--session-dir",
		session_dir.to_str().unwrap(),
		"--resume",
		session_id,
		"--mode",
		"text",
		"--no-tools",
		"next",
	]);
	let out = cmd.output().expect("run resume command");
	assert!(
		out.status.success(),
		"resume command failed: status={:?}\nstderr:\n{}",
		out.status,
		String::from_utf8_lossy(&out.stderr)
	);

	let reqs = server.requests.lock().expect("lock").clone();
	assert_eq!(reqs.len(), 1);
	let messages = reqs[0]["messages"].as_array().expect("messages");

	// Tool message has "(see attached image)" text placeholder
	assert_eq!(messages[2]["role"], "tool");
	assert_eq!(messages[2]["content"], "(see attached image)");

	// Hoisted user message contains the image
	assert_eq!(messages[3]["role"], "user");
	let parts = messages[3]["content"].as_array().expect("parts");
	assert_eq!(parts.len(), 2);
	assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,aW1nMQ==");

	server.stop();
}

#[test]
fn test_cli_aborted_tool_turn_with_image_replays_properly() {
	let server = MockWireServer::start();
	let base = temp_dir("aborted-tool-img");
	let (agent_dir, session_dir, cwd) = setup_dirs(&base, server.port);

	let session_id = "sess-aborted-img";
	{
		let mut store = SessionStore::open(&SessionOptions {
			enabled: true,
			cwd: cwd.clone(),
			model: Some("mock/vision".to_string()),
			provider: Some("mock".to_string()),
			dir: Some(session_dir.clone()),
			id: Some(session_id.to_string()),
			continue_latest: false,
			parent_session: None,
			no_title: false,
		});
		assert!(store.enabled());

		let turn = TurnRecord {
			prompt: "start camera and interrupt".to_string(),
			assistant: "started".to_string(),
			stop_reason: "aborted".to_string(),
			tool_calls: vec![ToolCallRecord {
				id: "call-aborted-1".to_string(),
				name: "camera".to_string(),
				arguments: serde_json::json!({}),
				result: "partial preview".to_string(),
				is_error: true,
				result_parts: vec![
					ContentPart::text("partial preview"),
					ContentPart::image("aW1nMQ==", "image/png"),
				],
				details: None,
			}],
			parts: Vec::new(),
		};
		store.append_turn(&turn).expect("append turn");
	}

	let mut cmd = omp();
	cmd.env("PI_CODING_AGENT_DIR", &agent_dir);
	cmd.args([
		"--cwd",
		cwd.to_str().unwrap(),
		"--session-dir",
		session_dir.to_str().unwrap(),
		"--resume",
		session_id,
		"--mode",
		"text",
		"--no-tools",
		"continue work",
	]);
	let out = cmd.output().expect("run resume command");
	assert!(
		out.status.success(),
		"resume command failed: status={:?}\nstderr:\n{}",
		out.status,
		String::from_utf8_lossy(&out.stderr)
	);

	let reqs = server.requests.lock().expect("lock").clone();
	assert_eq!(reqs.len(), 1);
	let messages = reqs[0]["messages"].as_array().expect("messages");

	assert_eq!(messages[2]["role"], "tool");
	assert_eq!(messages[2]["content"], "partial preview");
	assert_eq!(messages[3]["role"], "user");
	let parts = messages[3]["content"].as_array().expect("parts");
	assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,aW1nMQ==");

	server.stop();
}

#[test]
fn test_cli_malformed_tool_result_image_fails_closed_before_wire() {
	let server = MockWireServer::start();
	let base = temp_dir("bad-tool-img");
	let (agent_dir, session_dir, cwd) = setup_dirs(&base, server.port);

	let session_id = "sess-bad-img";
	let log_path = session_dir.join(format!("2026-10-27T10-00-00-000Z_{session_id}.jsonl"));

	// Malformed log with external URL image inside toolResult
	let log = concat!(
		r#"{"type":"session","version":3,"id":"sess-bad-img","timestamp":"2026-10-27T10:00:00.000Z","cwd":"/work","model":"mock/vision","provider":"mock"}"#,
		"\n",
		r#"{"type":"message","id":"u1","parentId":null,"timestamp":"2026-10-27T10:00:01.000Z","message":{"role":"user","content":"run"}}"#,
		"\n",
		r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-10-27T10:00:02.000Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"c1","name":"cam","arguments":{}}],"stop_reason":"toolUse"}}"#,
		"\n",
		r#"{"type":"message","id":"t1","parentId":"a1","timestamp":"2026-10-27T10:00:03.000Z","message":{"role":"toolResult","tool_call_id":"c1","tool_name":"cam","content":[{"type":"image","data":"http://evil.com/leak.png","mimeType":"image/png"}]}}"#,
		"\n"
	);
	fs::write(&log_path, log).expect("write bad log");
	let bytes_before = fs::read(&log_path).expect("read before");
	let mut cmd = omp();
	cmd.env("PI_CODING_AGENT_DIR", &agent_dir);
	cmd.args([
		"--cwd",
		cwd.to_str().unwrap(),
		"--session-dir",
		session_dir.to_str().unwrap(),
		"--resume",
		session_id,
		"--mode",
		"text",
		"--no-tools",
		"next",
	]);
	let out = cmd.output().expect("run resume command");
	assert!(!out.status.success(), "must fail closed on corrupt/malformed image in session log");

	let reqs = server.requests.lock().expect("lock").clone();
	assert_eq!(reqs.len(), 0, "mock server must receive 0 requests when session resume fails closed");

	let bytes_after = fs::read(&log_path).expect("read after");
	assert_eq!(bytes_before, bytes_after, "session log must remain byte-for-byte unmodified");
	server.stop();
}

#[test]
fn test_cli_legacy_text_tool_session_replays_without_user_image_insertion() {
	let server = MockWireServer::start();
	let base = temp_dir("legacy-txt-tool");
	let (agent_dir, session_dir, cwd) = setup_dirs(&base, server.port);

	let session_id = "sess-legacy-txt";
	{
		let mut store = SessionStore::open(&SessionOptions {
			enabled: true,
			cwd: cwd.clone(),
			model: Some("mock/vision".to_string()),
			provider: Some("mock".to_string()),
			dir: Some(session_dir.clone()),
			id: Some(session_id.to_string()),
			continue_latest: false,
			parent_session: None,
			no_title: false,
		});
		assert!(store.enabled());

		let turn = TurnRecord {
			prompt: "read config".to_string(),
			assistant: "reading".to_string(),
			stop_reason: "stop".to_string(),
			tool_calls: vec![ToolCallRecord {
				id: "call-txt-1".to_string(),
				name: "read_file".to_string(),
				arguments: serde_json::json!({"path": "config.txt"}),
				result: "port=8080".to_string(),
				is_error: false,
				result_parts: Vec::new(),
				details: None,
			}],
			parts: Vec::new(),
		};
		store.append_turn(&turn).expect("append turn");
	}

	let mut cmd = omp();
	cmd.env("PI_CODING_AGENT_DIR", &agent_dir);
	cmd.args([
		"--cwd",
		cwd.to_str().unwrap(),
		"--session-dir",
		session_dir.to_str().unwrap(),
		"--resume",
		session_id,
		"--mode",
		"text",
		"--no-tools",
		"proceed",
	]);
	let out = cmd.output().expect("run resume command");
	assert!(
		out.status.success(),
		"resume command failed: status={:?}\nstderr:\n{}",
		out.status,
		String::from_utf8_lossy(&out.stderr)
	);

	let reqs = server.requests.lock().expect("lock").clone();
	assert_eq!(reqs.len(), 1);
	let messages = reqs[0]["messages"].as_array().expect("messages");

	// Exactly 4 messages: User turn 1, Assistant, Tool result, User turn 2
	assert_eq!(messages.len(), 4);
	assert_eq!(messages[2]["role"], "tool");
	assert_eq!(messages[2]["content"], "port=8080");
	assert_eq!(messages[3]["role"], "user");
	assert_eq!(messages[3]["content"], "proceed");

	server.stop();
}
