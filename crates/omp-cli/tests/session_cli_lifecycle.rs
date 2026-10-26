//! Behavioral and lifecycle tests for OMP session CLI surface:
//! - --resume known v3 session with local scripted SSE provider
//! - --continue fresh boundary
//! - v1 immutability
//! - printable typed turn persistence
//! - nonTTY bare --resume error

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcCommand, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use omp_cli::session::write_terminal_breadcrumb;

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_dir(prefix: &str) -> PathBuf {
	let unique = format!(
		"omp-lifecycle-{prefix}-{}-{}",
		std::process::id(),
		COUNTER.fetch_add(1, Ordering::Relaxed)
	);
	let dir = std::env::temp_dir().join(unique);
	fs::create_dir_all(&dir).expect("create temp dir");
	dir
}

fn omp() -> ProcCommand {
	ProcCommand::new(env!("CARGO_BIN_EXE_omp"))
}

struct MockSseServer {
	port: u16,
	stop_tx: mpsc::Sender<()>,
	handle: Option<thread::JoinHandle<()>>,
}

impl MockSseServer {
	fn start() -> Self {
		let listener = TcpListener::bind("127.0.0.1:0").expect("bind tcp");
		let port = listener.local_addr().expect("local addr").port();
		listener.set_nonblocking(true).expect("set nonblocking");
		let (stop_tx, stop_rx) = mpsc::channel::<()>();

		let handle = thread::spawn(move || loop {
			if stop_rx.try_recv().is_ok() {
				break;
			}
			match listener.accept() {
				Ok((mut stream, _)) => {
					let mut buf = [0u8; 4096];
					let mut total_read = 0;
					while total_read < buf.len() {
						match stream.read(&mut buf[total_read..]) {
							Ok(0) => break,
							Ok(n) => {
								total_read += n;
								if buf[..total_read].windows(4).any(|w| w == b"\r\n\r\n") {
									break;
								}
							},
							Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
								thread::sleep(Duration::from_millis(10));
							},
							Err(_) => break,
						}
					}

					let body = concat!(
						"data: {\"choices\":[{\"delta\":{\"content\":\"mock answer\"}}]}\n\n",
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
					thread::sleep(Duration::from_millis(20));
				},
				Err(_) => break,
			}
		});

		Self {
			port,
			stop_tx,
			handle: Some(handle),
		}
	}
}

impl Drop for MockSseServer {
	fn drop(&mut self) {
		let _ = self.stop_tx.send(());
		if let Some(h) = self.handle.take() {
			let _ = h.join();
		}
	}
}

fn setup_mock_agent_dir(dir: &Path, port: u16) {
	let config_yml = format!(
		"defaultModel: mock/mock-model\nproviders:\n  mock:\n    baseUrl: \"http://127.0.0.1:{port}/v1\"\n    api: openai-completions\n    apiKey: test-key\n"
	);
	fs::write(dir.join("config.yml"), config_yml).expect("write config.yml");

	let models_yml = format!(
		"providers:\n  mock:\n    baseUrl: \"http://127.0.0.1:{port}/v1\"\n    api: openai-completions\n    apiKey: test-key\n    models:\n      - id: mock-model\n        contextWindow: 128000\n"
	);
	fs::write(dir.join("models.yml"), models_yml).expect("write models.yml");
}

#[test]
fn test_resume_known_v3_session() {
	let server = MockSseServer::start();
	let temp = temp_dir("resume-v3");
	let agent_dir = temp.join("agent");
	let sessions_dir = temp.join("sessions");
	fs::create_dir_all(&agent_dir).expect("create agent dir");
	fs::create_dir_all(&sessions_dir).expect("create sessions dir");
	setup_mock_agent_dir(&agent_dir, server.port);

	let fixture_path =
		PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v3_session.jsonl");
	let session_id = "0195e342-9f7a-7b3b-8f3a-3c9f7a7b3b8f";
	let v3_filename = format!("2026-03-31T08-00-00-000Z_{session_id}.jsonl");
	let v3_file = sessions_dir.join(&v3_filename);
	fs::copy(&fixture_path, &v3_file).expect("copy v3 fixture");

	let output = omp()
		.env("PI_CODING_AGENT_DIR", agent_dir.to_str().unwrap())
		.args([
			"--session-dir",
			sessions_dir.to_str().unwrap(),
			"--resume",
			session_id,
			"--no-tools",
			"-p",
			"hello",
		])
		.output()
		.expect("run omp --resume");

	let stderr = String::from_utf8_lossy(&output.stderr);
	assert_eq!(
		output.status.code(),
		Some(0),
		"omp --resume must succeed exit 0: {stderr}"
	);

	let content = fs::read_to_string(&v3_file).expect("read resumed v3 file");
	let lines: Vec<&str> = content.lines().collect();
	// Original fixture had 3 lines (title, session, message), plus 2 appended (user, asst)
	assert!(
		lines.len() >= 5,
		"resumed file must have original entries and new entries, found {}",
		lines.len()
	);

	// Title changes may precede the first message in a valid v3 log.
	let orig_msg = lines
		.iter()
		.filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
		.find(|entry| entry["type"] == "message" && entry["message"]["role"] == "user")
		.expect("original user message");
	assert_eq!(orig_msg["message"]["role"], "user");

	// Appended assistant message
	let asst_entry: serde_json::Value =
		serde_json::from_str(lines[lines.len() - 1]).expect("parse asst message");
	assert_eq!(asst_entry["type"], "message");
	assert_eq!(asst_entry["message"]["role"], "assistant");
	assert_eq!(asst_entry["message"]["api"], "openai-completions");
	assert_eq!(asst_entry["message"]["provider"], "mock");
	assert_eq!(asst_entry["message"]["model"], "mock-model");
	assert_eq!(asst_entry["message"]["stopReason"], "stop");

	let _ = fs::remove_dir_all(temp);
}

#[test]
fn test_continue_fresh_boundary() {
	let server = MockSseServer::start();
	let temp = temp_dir("continue-fresh");
	let agent_dir = temp.join("agent");
	let sessions_dir = temp.join("sessions");
	let cwd = temp.join("project");
	fs::create_dir_all(&agent_dir).expect("create agent dir");
	fs::create_dir_all(&sessions_dir).expect("create sessions dir");
	fs::create_dir_all(&cwd).expect("create project dir");
	setup_mock_agent_dir(&agent_dir, server.port);

	let breadcrumb_file = temp.join("terminal-breadcrumb.json");
	let stale_session_file = sessions_dir.join(
		"2026-03-31T08-00-00-000Z_0195e342-9f7a-7b3b-8f3a-3c9f7a7b3b8f.jsonl",
	);

	// Fresh breadcrumb pointing to non-existent session file
	write_terminal_breadcrumb(&cwd, &stale_session_file, true, Some(&breadcrumb_file))
		.expect("write breadcrumb");

	let output = omp()
		.env("PI_CODING_AGENT_DIR", agent_dir.to_str().unwrap())
		.args([
			"--cwd",
			cwd.to_str().unwrap(),
			"--session-dir",
			sessions_dir.to_str().unwrap(),
			"--continue",
			"--no-tools",
			"-p",
			"test continue fresh",
		])
		.output()
		.expect("run omp --continue");

	let stderr = String::from_utf8_lossy(&output.stderr);
	assert_eq!(
		output.status.code(),
		Some(0),
		"omp --continue must succeed exit 0: {stderr}"
	);

	// Verify a fresh session file was created and written
	let files = fs::read_dir(&sessions_dir)
		.expect("read sessions dir")
		.flatten()
		.map(|e| e.path())
		.filter(|p| p.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
		.collect::<Vec<_>>();
	assert_eq!(
		files.len(),
		1,
		"exactly one fresh session file must exist in sessions dir"
	);

	let _ = fs::remove_dir_all(temp);
}

#[test]
fn test_v1_immutability() {
	let temp = temp_dir("v1-immutable");
	let server = MockSseServer::start();
	let agent_dir = temp.join("agent");
	fs::create_dir_all(&agent_dir).expect("create agent dir");
	setup_mock_agent_dir(&agent_dir, server.port);
	let fixture_path =
		PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v1_session.jsonl");
	let v1_file = temp.join("session.jsonl");
	fs::copy(&fixture_path, &v1_file).expect("copy v1 fixture");
	let initial_bytes = fs::read(&v1_file).expect("read initial bytes");

	let output = omp()
		.env("PI_CODING_AGENT_DIR", &agent_dir)
		.args([
			"--session-dir",
			temp.to_str().unwrap(),
			"--session-id",
			"session",
			"--no-tools",
			"-p",
			"try modifying v1",
		])
		.output()
		.expect("run omp on v1 session");

	let stderr = String::from_utf8_lossy(&output.stderr);
	assert_eq!(
		output.status.code(),
		Some(2),
		"modifying v1 session must exit with usage error 2: {stderr}"
	);
	assert!(
		stderr.contains("legacy v1 session log: read-only")
			|| stderr.contains("cannot append to legacy v1 session log"),
		"stderr must mention v1 read-only rejection: {stderr}"
	);

	let after_bytes = fs::read(&v1_file).expect("read after bytes");
	assert_eq!(
		initial_bytes, after_bytes,
		"v1 session file bytes must remain completely immutable"
	);

	let _ = fs::remove_dir_all(temp);
}

#[test]
fn test_printable_typed_turn_persistence() {
	let server = MockSseServer::start();
	let temp = temp_dir("turn-persist");
	let agent_dir = temp.join("agent");
	let sessions_dir = temp.join("sessions");
	fs::create_dir_all(&agent_dir).expect("create agent dir");
	fs::create_dir_all(&sessions_dir).expect("create sessions dir");
	setup_mock_agent_dir(&agent_dir, server.port);

	let output = omp()
		.env("PI_CODING_AGENT_DIR", agent_dir.to_str().unwrap())
		.args([
			"--session-dir",
			sessions_dir.to_str().unwrap(),
			"--session-id",
			"typed-turn-session",
			"--no-tools",
			"-p",
			"persistence check",
		])
		.output()
		.expect("run omp turn");

	let stderr = String::from_utf8_lossy(&output.stderr);
	assert_eq!(
		output.status.code(),
		Some(0),
		"omp turn must succeed exit 0: {stderr}"
	);

	let files = fs::read_dir(&sessions_dir)
		.expect("read session dir")
		.flatten()
		.map(|e| e.path())
		.filter(|p| p.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
		.collect::<Vec<_>>();
	assert!(!files.is_empty(), "must have written a session jsonl file");

	let content = fs::read_to_string(&files[0]).expect("read session file");
	let lines: Vec<&str> = content.lines().collect();
	assert!(
		lines.len() >= 4,
		"must have title slot, session header, user message, and assistant message: found {}",
		lines.len()
	);

	// User message line
	let user_entry: serde_json::Value =
		serde_json::from_str(lines[2]).expect("user entry json");
	assert_eq!(user_entry["type"], "message");
	assert_eq!(user_entry["message"]["role"], "user");
	assert!(user_entry["message"]["timestamp"].is_number());

	// Assistant message line
	let asst_entry: serde_json::Value =
		serde_json::from_str(lines[3]).expect("asst entry json");
	assert_eq!(asst_entry["type"], "message");
	assert_eq!(asst_entry["message"]["role"], "assistant");
	assert_eq!(asst_entry["message"]["api"], "openai-completions");
	assert_eq!(asst_entry["message"]["provider"], "mock");
	assert_eq!(asst_entry["message"]["model"], "mock-model");
	assert_eq!(asst_entry["message"]["stopReason"], "stop");
	assert!(asst_entry["message"]["timestamp"].is_number());

	let _ = fs::remove_dir_all(temp);
}

#[test]
fn test_nontty_bare_resume_error() {
	let output = omp()
		.arg("--resume")
		.stdin(Stdio::null())
		.output()
		.expect("run omp --resume bare");

	assert_eq!(
		output.status.code(),
		Some(2),
		"bare --resume in nonTTY must exit with usage error 2"
	);
	let stderr = String::from_utf8_lossy(&output.stderr);
	assert!(
		stderr.contains("cannot display session picker")
			|| stderr.contains("Non-interactive")
			|| stderr.contains("interactive terminal")
			|| stderr.contains("interactive TTY")
			|| stderr.contains("requires a TTY"),
		"stderr must explain non-interactive error, got: {stderr}"
	);
}
