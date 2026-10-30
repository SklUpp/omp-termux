//! Behavioral tests for `--smol`, `--slow`, and `--no-lsp`.
//!
//! Each case drives the real `omp` binary against a mock chat-completions
//! endpoint and asserts the model that reached the wire: a CLI role flag
//! replaces the configured `modelRoles` entry for `--model @role`, the flag
//! wins over `PI_SMOL_MODEL`/`PI_SLOW_MODEL` (an explicit empty flag suppresses
//! the env var and keeps the configured entry), and `--no-lsp` closes the
//! existing `lsp.enabled` gate that rejects an explicit `--tools lsp`.

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
		"omp-role-flags-{prefix}-{}-{}",
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
		"defaultModel: mock/default\nmodelRoles:\n  smol: mock/configured\n  slow: \
		 mock/configured-slow\nproviders:\n  mock:\n    baseUrl: \"http://127.0.0.1:{port}/v1\"\n    \
		 api: openai-completions\n    apiKey: test-key\n"
	);
	fs::write(agent_dir.join("config.yml"), config).expect("write config.yml");
	let models = format!(
		"providers:\n  mock:\n    baseUrl: \"http://127.0.0.1:{port}/v1\"\n    api: \
		 openai-completions\n    apiKey: test-key\n    models:\n      - id: default\n        \
		 contextWindow: 128000\n      - id: configured\n        contextWindow: 128000\n      - id: \
		 configured-slow\n        contextWindow: 128000\n      - id: cli-fast\n        \
		 contextWindow: 128000\n      - id: cli-slow\n        contextWindow: 128000\n      - id: \
		 other\n        contextWindow: 128000\n      - id: \
		 env-smol\n        contextWindow: 128000\n"
	);
	fs::write(agent_dir.join("models.yml"), models).expect("write models.yml");
	(agent_dir, cwd)
}

fn run_omp(agent_dir: &Path, cwd: &Path, envs: &[(&str, &str)], extra: &[&str]) -> Output {
	let mut cmd = ProcCommand::new(env!("CARGO_BIN_EXE_omp"));
	cmd.env("PI_CODING_AGENT_DIR", agent_dir);
	for (key, value) in envs {
		cmd.env(key, value);
	}
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
	cmd.output().expect("run omp")
}

fn run_omp_ok(agent_dir: &Path, cwd: &Path, envs: &[(&str, &str)], extra: &[&str]) -> Output {
	let out = run_omp(agent_dir, cwd, envs, extra);
	assert!(
		out.status.success(),
		"omp failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	out
}

/// `--smol`/`--slow` replace the configured role for the `@role` alias, and a
/// comma-separated pattern still resolves through the existing resolver.
#[test]
fn role_flags_override_configured_roles_for_model_alias() {
	let server = MockWireServer::start();
	let base = temp_dir("flags");
	let (agent_dir, cwd) = setup(server.port, &base);
	run_omp_ok(
		&agent_dir,
		&cwd,
		&[],
		&["--smol", "mock/cli-fast", "--model", "@smol"],
	);
	assert_eq!(server.body(0)["model"], "cli-fast");

	run_omp_ok(
		&agent_dir,
		&cwd,
		&[],
		&["--slow", "mock/other, mock/cli-slow", "--model", "@slow"],
	);
	assert_eq!(server.body(1)["model"], "other", "comma pattern list keeps its order");
	server.stop();
}

/// The flag is the nullish winner over the env var; an explicit empty flag
/// keeps the configured role and still suppresses the env fallback.
#[test]
fn cli_role_flags_beat_env_and_empty_flag_blocks_env() {
	let server = MockWireServer::start();
	let base = temp_dir("precedence");
	let (agent_dir, cwd) = setup(server.port, &base);
	let envs = [("PI_SMOL_MODEL", "mock/env-smol")];

	run_omp_ok(&agent_dir, &cwd, &envs, &["--smol", "mock/cli-fast", "--model", "@smol"]);
	assert_eq!(server.body(0)["model"], "cli-fast", "CLI must win over the env var");

	run_omp_ok(&agent_dir, &cwd, &envs, &["--smol", "", "--model", "@smol"]);
	assert_eq!(
		server.body(1)["model"],
		"configured",
		"an explicit empty flag keeps the configured role and blocks the env fallback"
	);

	run_omp_ok(&agent_dir, &cwd, &envs, &["--model", "@smol"]);
	assert_eq!(server.body(2)["model"], "env-smol", "env fills the role when the flag is absent");
	server.stop();
}

/// `--no-lsp` closes the existing `lsp.enabled` gate: an explicit `lsp` tool
/// now fails with the disabled message instead of mounting the tool.
#[test]
fn no_lsp_flag_disables_explicit_lsp_tool() {
	let base = temp_dir("no-lsp");
	let (agent_dir, cwd) = setup(0, &base);
	let mut cmd = ProcCommand::new(env!("CARGO_BIN_EXE_omp"));
	cmd.env("PI_CODING_AGENT_DIR", &agent_dir);
	cmd.stdin(Stdio::null());
	cmd.args([
		"--cwd",
		cwd.to_str().expect("utf8 cwd"),
		"--no-session",
		"--no-title",
		"--mode",
		"text",
		"--no-lsp",
		"--tools",
		"lsp",
		"--model",
		"mock/default",
	]);
	cmd.arg("say hi");
	let out = cmd.output().expect("run omp");
	assert!(!out.status.success(), "--no-lsp must reject an explicit lsp tool");
	let stderr = String::from_utf8_lossy(&out.stderr);
	assert!(
		stderr.contains("disabled by `lsp.enabled`"),
		"expected the disabled-lsp error, got: {stderr}"
	);
}
