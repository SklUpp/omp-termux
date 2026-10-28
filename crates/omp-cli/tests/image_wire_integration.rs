//! Behavioral integration tests for CLI @image and prompt @file mentions:
//! - Content detection via magic byte sniffing (PNG, JPEG, GIF, WebP)
//! - CLI positional `@image` argument maps to multipart text and `image_url` wire parts
//! - Prompt mentions `@image` map to `<file path="...">` and `image_url` wire parts
//! - Session persistence stores ordered text and image parts in JSONL session log
//! - Session resume preserves turn history images and text without loss on subsequent wire turns
//! - Explicit text-only models fail closed without corrupting session logs
//! - Bounded limits (25 MiB) and invalid files are rejected safely

use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcCommand, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use omp_cli::session::SessionStore;

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_dir(prefix: &str) -> PathBuf {
	let unique = format!(
		"omp-img-int-{prefix}-{}-{}",
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
						"data: {\"choices\":[{\"delta\":{\"content\":\"Recognized the visual content\"}}]}\n\n",
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

// 1x1 transparent PNG
const TINY_PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01\x00\x00\x00\x01\x08\x06\x00\x00\x00\x1f\x15c4\x00\x00\x00\nIDATx\x9cc\x00\x01\x00\x00\x05\x00\x01\r\n-\xb4\x00\x00\x00\x00IEND\xaeB`\x82";

#[test]
fn test_cli_positional_image_wire_and_session_resume() {
	let server = MockWireServer::start();
	let base = temp_dir("cli-image-resume");
	let (agent_dir, session_dir, cwd) = setup_dirs(&base, server.port);

	let img_path = cwd.join("screenshot.png");
	fs::write(&img_path, TINY_PNG).expect("write png");

	// 1. First turn: pass @screenshot.png and prompt
	let mut cmd = omp();
	cmd.env("PI_CODING_AGENT_DIR", &agent_dir);
	cmd.args([
		"--cwd",
		cwd.to_str().unwrap(),
		"--session-dir",
		session_dir.to_str().unwrap(),
		"--session-id",
		"session-img-1",
		"--mode",
		"text",
		"--no-tools",
		"@screenshot.png",
		"please describe this image",
	]);
	let out = cmd.output().expect("run turn 1");
	assert!(
		out.status.success(),
		"turn 1 must succeed: {}",
		String::from_utf8_lossy(&out.stderr)
	);

	// Check wire request
	let reqs = server.requests.lock().expect("lock").clone();
	assert_eq!(reqs.len(), 1, "mock server must have received 1 request");
	let messages = reqs[0]["messages"].as_array().expect("messages array");
	assert_eq!(messages.len(), 1);
	let content_parts = messages[0]["content"].as_array().expect("content parts");
	assert_eq!(content_parts.len(), 2, "must have text part and image_url part");

	assert_eq!(content_parts[0]["type"], "text");
	let text = content_parts[0]["text"].as_str().unwrap();
	assert!(text.contains("<file name="));
	assert!(text.contains("please describe this image"));

	assert_eq!(content_parts[1]["type"], "image_url");
	let url = content_parts[1]["image_url"]["url"].as_str().unwrap();
	assert!(url.starts_with("data:image/png;base64,"));

	// Check session file on disk
	let session_files = fs::read_dir(&session_dir)
		.expect("read session dir")
		.flatten()
		.map(|e| e.path())
		.filter(|p| {
			p.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
				&& p.file_stem()
					.and_then(|s| s.to_str())
					.map_or(false, |s| s.ends_with("session-img-1"))
		})
		.collect::<Vec<_>>();
	assert_eq!(session_files.len(), 1, "session file must exist");
	let log_path = &session_files[0];
	assert!(log_path.is_file(), "session file must exist");
	let log_content = fs::read_to_string(log_path).expect("read session log");
	assert!(log_content.contains("\"type\":\"image\""));
	assert!(log_content.contains("\"mimeType\":\"image/png\""));

	// 2. Second turn: resume session and verify turn 1 image wire replay
	let mut resume_cmd = omp();
	resume_cmd.env("PI_CODING_AGENT_DIR", &agent_dir);
	resume_cmd.args([
		"--cwd",
		cwd.to_str().unwrap(),
		"--session-dir",
		session_dir.to_str().unwrap(),
		"--resume",
		"session-img-1",
		"--mode",
		"text",
		"--no-tools",
		"what colors did you see?",
	]);
	let res_out = resume_cmd.output().expect("run turn 2 resume");
	assert!(
		res_out.status.success(),
		"turn 2 resume must succeed: {}",
		String::from_utf8_lossy(&res_out.stderr)
	);

	let reqs2 = server.requests.lock().expect("lock").clone();
	assert_eq!(reqs2.len(), 2, "mock server must have received 2 requests total");
	let resumed_messages = reqs2[1]["messages"].as_array().expect("resumed messages");
	assert!(resumed_messages.len() >= 3, "resumed history must carry previous turns");

	// Message 0 is the historical user turn: must still have the image part!
	let turn1_content = resumed_messages[0]["content"].as_array().expect("turn 1 parts");
	assert!(
		turn1_content.iter().any(|p| p["type"] == "image_url"),
		"resumed turn 1 must preserve image_url without loss"
	);

	server.stop();
	let _ = fs::remove_dir_all(&base);
}

#[test]
fn test_cli_prompt_mentions_wire_transfer() {
	let server = MockWireServer::start();
	let base = temp_dir("prompt-mention-wire");
	let (agent_dir, session_dir, cwd) = setup_dirs(&base, server.port);

	let img_path = cwd.join("chart.png");
	fs::write(&img_path, TINY_PNG).expect("write chart");

	let mut cmd = omp();
	cmd.env("PI_CODING_AGENT_DIR", &agent_dir);
	cmd.args([
		"--cwd",
		cwd.to_str().unwrap(),
		"--session-dir",
		session_dir.to_str().unwrap(),
		"--no-session",
		"--mode",
		"text",
		"--no-tools",
		"Please analyze @chart.png carefully",
	]);
	let out = cmd.output().expect("run prompt mention");
	assert!(
		out.status.success(),
		"prompt mention must succeed: {}",
		String::from_utf8_lossy(&out.stderr)
	);

	let reqs = server.requests.lock().expect("lock").clone();
	assert_eq!(reqs.len(), 1);
	let messages = reqs[0]["messages"].as_array().expect("messages");
	let parts = messages[0]["content"].as_array().expect("content parts");

	assert!(parts.iter().any(|p| p["type"] == "image_url"));
	let text_part = parts.iter().find(|p| p["type"] == "text").expect("text part");
	let text = text_part["text"].as_str().unwrap();
	assert!(text.contains("<file path=\"chart.png\">"));
	assert!(text.contains("Please analyze @chart.png carefully"));

	server.stop();
	let _ = fs::remove_dir_all(&base);
}

#[test]
fn test_text_only_model_explicitly_fails_closed_on_image() {
	let server = MockWireServer::start();
	let base = temp_dir("text-only-fail");
	let (agent_dir, session_dir, cwd) = setup_dirs(&base, server.port);

	let img_path = cwd.join("photo.png");
	fs::write(&img_path, TINY_PNG).expect("write photo");

	let mut cmd = omp();
	cmd.env("PI_CODING_AGENT_DIR", &agent_dir);
	cmd.args([
		"--cwd",
		cwd.to_str().unwrap(),
		"--session-dir",
		session_dir.to_str().unwrap(),
		"--model",
		"mock/text-only",
		"--mode",
		"text",
		"--no-tools",
		"@photo.png",
		"describe",
	]);
	let out = cmd.output().expect("run text only model");
	assert!(
		!out.status.success(),
		"text-only model must fail when image is passed"
	);
	let stderr = String::from_utf8_lossy(&out.stderr);
	assert!(
		stderr.contains("does not support image input"),
		"stderr must state text-only model does not support image input: {stderr}"
	);

	// Ensure no requests were sent to wire
	let reqs = server.requests.lock().expect("lock").clone();
	assert_eq!(reqs.len(), 0, "no wire request must be sent on rejection");

	server.stop();
	let _ = fs::remove_dir_all(&base);
}

#[test]
fn test_missing_or_oversize_image_file_handling() {
	let server = MockWireServer::start();
	let base = temp_dir("missing-oversize");
	let (agent_dir, session_dir, cwd) = setup_dirs(&base, server.port);

	// 1. Missing file: fast fail
	let mut missing_cmd = omp();
	missing_cmd.env("PI_CODING_AGENT_DIR", &agent_dir);
	missing_cmd.args([
		"--cwd",
		cwd.to_str().unwrap(),
		"--session-dir",
		session_dir.to_str().unwrap(),
		"--mode",
		"text",
		"@nonexistent.png",
	]);
	let missing_out = missing_cmd.output().expect("run missing");
	assert!(!missing_out.status.success());
	let stderr = String::from_utf8_lossy(&missing_out.stderr);
	assert!(stderr.contains("Error: File not found:"));

	// 2. Oversize file (>25 MiB): warning, text block, no image wire payload
	let huge_path = cwd.join("huge.png");
	let mut f = File::create(&huge_path).expect("create huge");
	f.write_all(b"\x89PNG\r\n\x1a\n").expect("write png magic");
	f.set_len(26 * 1024 * 1024).expect("set 26MB length");
	drop(f);

	let mut huge_cmd = omp();
	huge_cmd.env("PI_CODING_AGENT_DIR", &agent_dir);
	huge_cmd.args([
		"--cwd",
		cwd.to_str().unwrap(),
		"--session-dir",
		session_dir.to_str().unwrap(),
		"--no-session",
		"--mode",
		"text",
		"--no-tools",
		"@huge.png",
		"analyze",
	]);
	let huge_out = huge_cmd.output().expect("run huge");
	assert!(huge_out.status.success());
	let huge_stderr = String::from_utf8_lossy(&huge_out.stderr);
	assert!(huge_stderr.contains("Warning: Skipping file contents (too large:"));

	let reqs = server.requests.lock().expect("lock").clone();
	assert_eq!(reqs.len(), 1);
	let messages = reqs[0]["messages"].as_array().expect("messages");
	let content = messages[0]["content"].as_str().expect("must be pure text without image parts");
	assert!(content.contains("(skipped: too large,"));

	server.stop();
	let _ = fs::remove_dir_all(&base);
}

#[test]
fn test_positional_file_contents_do_not_trigger_file_mentions() {
	let server = MockWireServer::start();
	let base = temp_dir("no-scan-contents");
	let (agent_dir, session_dir, cwd) = setup_dirs(&base, server.port);

	let secret_png = cwd.join("secret.png");
	fs::write(&secret_png, TINY_PNG).expect("write secret png");

	// Text file contains `@secret.png` within its text content
	let text_file = cwd.join("doc.txt");
	fs::write(&text_file, "Document mentions @secret.png in body").expect("write doc.txt");

	let mut cmd = omp();
	cmd.env("PI_CODING_AGENT_DIR", &agent_dir);
	cmd.args([
		"--cwd",
		cwd.to_str().unwrap(),
		"--session-dir",
		session_dir.to_str().unwrap(),
		"--no-session",
		"--mode",
		"text",
		"--no-tools",
		"@doc.txt",
		"summarize the text",
	]);
	let out = cmd.output().expect("run no scan");
	assert!(out.status.success());

	let reqs = server.requests.lock().expect("lock").clone();
	assert_eq!(reqs.len(), 1);
	let messages = reqs[0]["messages"].as_array().expect("messages");
	// Since no images should be attached from inside @doc.txt, content should be pure text!
	let content = messages[0]["content"].as_str().expect("must be pure string, no image_url");
	assert!(content.contains("Document mentions @secret.png in body"));
	assert!(!content.contains("<file path=\"secret.png\">"));

	server.stop();
	let _ = fs::remove_dir_all(&base);
}
#[test]
fn test_json_mode_agent_end_outputs_typed_image_parts() {
	let server = MockWireServer::start();
	let base = temp_dir("json-mode-agent-end");
	let (agent_dir, session_dir, cwd) = setup_dirs(&base, server.port);

	let img_path = cwd.join("photo.png");
	fs::write(&img_path, TINY_PNG).expect("write photo");

	let mut cmd = omp();
	cmd.env("PI_CODING_AGENT_DIR", &agent_dir);
	cmd.args([
		"--cwd",
		cwd.to_str().unwrap(),
		"--session-dir",
		session_dir.to_str().unwrap(),
		"--no-session",
		"--mode",
		"json",
		"--no-tools",
		"@photo.png",
		"describe image",
	]);
	let out = cmd.output().expect("run json mode");
	assert!(out.status.success(), "run json mode must succeed: {}", String::from_utf8_lossy(&out.stderr));

	let stdout_str = String::from_utf8_lossy(&out.stdout);
	let agent_end_line = stdout_str
		.lines()
		.find(|l| l.contains(r#""type":"agent_end""#))
		.expect("agent_end frame in stdout");
	let ae: serde_json::Value = serde_json::from_str(agent_end_line).expect("parse agent_end JSON");
	let messages = ae["messages"].as_array().expect("messages in agent_end");
	assert!(!messages.is_empty(), "agent_end must contain messages");

	let user_msg = &messages[0];
	assert_eq!(user_msg["role"], "user");
	let content_parts = user_msg["content"].as_array().expect("user content parts");
	assert_eq!(content_parts.len(), 2, "must have text part and image part in order");
	assert_eq!(content_parts[0]["type"], "text");
	assert_eq!(content_parts[1]["type"], "image");
	assert_eq!(content_parts[1]["mimeType"], "image/png");
	assert!(content_parts[1]["data"].as_str().unwrap().starts_with("iVBORw0KGgo"));

	server.stop();
	let _ = fs::remove_dir_all(&base);
}
