//! Behavioral tests for `--thinking` and the root model `:level` suffix.
//!
//! Every case drives the real `omp` binary against a mock chat-completions
//! endpoint and asserts what reached the wire: `reasoning_effort` is present
//! exactly for concrete levels, `off`/`auto` never invent an effort, an
//! explicit flag wins over the model suffix, and an invalid flag only warns.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcCommand, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_dir(prefix: &str) -> PathBuf {
	let unique = format!(
		"omp-thinking-{prefix}-{}-{}",
		std::process::id(),
		COUNTER.fetch_add(1, Ordering::Relaxed)
	);
	let dir = std::env::temp_dir().join(unique);
	fs::create_dir_all(&dir).expect("create temp dir");
	dir
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

		let handle = thread::spawn(move || {
			loop {
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
							let header = String::from_utf8_lossy(&buf[..pos]);
							let len: usize = header
								.lines()
								.find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
								.and_then(|l| l.split(':').nth(1))
								.and_then(|v| v.trim().parse().ok())
								.unwrap_or(0);
							while buf.len() < pos + 4 + len {
								match stream.read(&mut temp) {
									Ok(0) => break,
									Ok(n) => buf.extend_from_slice(&temp[..n]),
									Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
										thread::sleep(Duration::from_millis(5));
									},
									Err(_) => break,
								}
							}
							if buf.len() >= pos + 4 + len {
								let body = &buf[pos + 4..pos + 4 + len];
								if let Ok(json) = serde_json::from_slice::<serde_json::Value>(body) {
									reqs_clone.lock().expect("lock").push(json);
								}
							}
						}
						let body = concat!(
							"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n",
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
			}
		});

		Self {
			port,
			requests,
			stop_tx,
			handle: Some(handle),
		}
	}

	fn body(&self, index: usize) -> serde_json::Value {
		self
			.requests
			.lock()
			.expect("lock")
			.get(index)
			.cloned()
			.expect("mock request recorded")
	}

	fn stop(mut self) {
		let _ = self.stop_tx.send(());
		if let Some(handle) = self.handle.take() {
			let _ = handle.join();
		}
	}
}

fn setup(port: u16, base: &Path) -> (PathBuf, PathBuf) {
	let agent_dir = base.join("agent");
	let cwd = base.join("work");
	fs::create_dir_all(&agent_dir).expect("agent dir");
	fs::create_dir_all(&cwd).expect("cwd");
	let config = format!(
		"defaultModel: mock/vision\nproviders:\n  mock:\n    baseUrl: \"http://127.0.0.1:{port}/v1\"\n    api: openai-completions\n    apiKey: test-key\n"
	);
	fs::write(agent_dir.join("config.yml"), config).expect("write config.yml");
	let models = format!(
		"providers:\n  mock:\n    baseUrl: \"http://127.0.0.1:{port}/v1\"\n    api: openai-completions\n    apiKey: test-key\n    models:\n      - id: vision\n        contextWindow: 128000\n        input: [\"text\", \"image\"]\n      - id: \"qwen:7b\"\n        contextWindow: 128000\n"
	);
	fs::write(agent_dir.join("models.yml"), models).expect("write models.yml");
	(agent_dir, cwd)
}

fn run_omp(agent_dir: &Path, cwd: &Path, extra: &[&str]) -> Output {
	let mut cmd = ProcCommand::new(env!("CARGO_BIN_EXE_omp"));
	cmd.env("PI_CODING_AGENT_DIR", agent_dir);
	cmd.stdin(Stdio::null());
	cmd.args([
		"--cwd",
		cwd.to_str().expect("utf8 cwd"),
		"--no-session",
		"--no-tools",
		"--no-title",
		"--mode",
		"text",
	]);
	cmd.args(extra);
	cmd.arg("say hi");
	let out = cmd.output().expect("run omp");
	assert!(
		out.status.success(),
		"omp failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	out
}

/// `--thinking` wins over a concrete `:level` already on `--model`.
#[test]
fn explicit_thinking_flag_overrides_model_suffix() {
	let server = MockWireServer::start();
	let base = temp_dir("flag-wins");
	let (agent_dir, cwd) = setup(server.port, &base);
	run_omp(&agent_dir, &cwd, &["--model", "mock/vision:high", "--thinking", "low"]);
	let body = server.body(0);
	assert_eq!(body["model"], "vision");
	assert_eq!(body["reasoning_effort"], "low");
	server.stop();
}

/// Suffix abbreviations normalize to the provider-facing full level.
#[test]
fn model_suffix_maps_abbreviations_to_full_effort() {
	let server = MockWireServer::start();
	let base = temp_dir("suffix-abbrev");
	let (agent_dir, cwd) = setup(server.port, &base);
	run_omp(&agent_dir, &cwd, &["--model", "mock/vision:med"]);
	let body = server.body(0);
	assert_eq!(body["model"], "vision");
	assert_eq!(body["reasoning_effort"], "medium");
	server.stop();
}

/// `auto` means "let the endpoint decide": no effort is sent, and the suffix
/// still comes off the model id.
#[test]
fn auto_never_invents_an_effort() {
	let server = MockWireServer::start();
	let base = temp_dir("auto");
	let (agent_dir, cwd) = setup(server.port, &base);
	run_omp(&agent_dir, &cwd, &["--model", "mock/vision:high", "--thinking", "auto"]);
	let body = server.body(0);
	assert_eq!(body["model"], "vision");
	assert!(
		body.get("reasoning_effort").is_none(),
		"auto must not send an effort: {body}"
	);
	server.stop();
}

/// `off` disables reasoning without forwarding `reasoning_effort: "off"`.
#[test]
fn off_never_sends_the_off_selector() {
	let server = MockWireServer::start();
	let base = temp_dir("off");
	let (agent_dir, cwd) = setup(server.port, &base);
	run_omp(&agent_dir, &cwd, &["--model", "mock/vision:high", "--thinking", "off"]);
	let body = server.body(0);
	assert_eq!(body["model"], "vision");
	assert!(
		body.get("reasoning_effort").is_none(),
		"off must not send an effort: {body}"
	);
	server.stop();
}

/// Invalid flag: warning only, no failure, model suffix still applies.
#[test]
fn invalid_thinking_warns_and_falls_back() {
	let server = MockWireServer::start();
	let base = temp_dir("invalid");
	let (agent_dir, cwd) = setup(server.port, &base);
	let out = run_omp(
		&agent_dir,
		&cwd,
		&["--model", "mock/vision:high", "--thinking", "bogus"],
	);
	let stderr = String::from_utf8_lossy(&out.stderr);
	assert!(
		stderr.contains("Invalid thinking level passed to --thinking"),
		"expected warning, got: {stderr}"
	);
	let body = server.body(0);
	assert_eq!(body["model"], "vision");
	assert_eq!(body["reasoning_effort"], "high");
	server.stop();
}

/// `:auto` on the model pattern is the upstream sentinel, not part of the id.
#[test]
fn auto_model_suffix_strips_without_effort() {
	let server = MockWireServer::start();
	let base = temp_dir("auto-suffix");
	let (agent_dir, cwd) = setup(server.port, &base);
	run_omp(&agent_dir, &cwd, &["--model", "mock/vision:auto"]);
	let body = server.body(0);
	assert_eq!(body["model"], "vision");
	assert!(
		body.get("reasoning_effort").is_none(),
		":auto must not send an effort: {body}"
	);
	server.stop();
}

/// Colon-bearing ids survive; only a known trailing level is stripped.
#[test]
fn provider_model_colons_are_preserved() {
	let server = MockWireServer::start();
	let base = temp_dir("colon-id");
	let (agent_dir, cwd) = setup(server.port, &base);
	run_omp(&agent_dir, &cwd, &["--model", "mock/qwen:7b"]);
	let plain = server.body(0);
	assert_eq!(plain["model"], "qwen:7b");
	assert!(plain.get("reasoning_effort").is_none());

	run_omp(&agent_dir, &cwd, &["--model", "mock/qwen:7b:min"]);
	let suffixed = server.body(1);
	assert_eq!(suffixed["model"], "qwen:7b");
	assert_eq!(suffixed["reasoning_effort"], "minimal");
	server.stop();
}
