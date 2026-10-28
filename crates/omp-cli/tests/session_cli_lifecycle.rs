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
	let mut cmd = ProcCommand::new(env!("CARGO_BIN_EXE_omp"));
	cmd.stdin(Stdio::null());
	cmd
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

#[test]
fn test_cli_file_arg_does_not_enter_repl_and_runs_turn() {
	let server = MockSseServer::start();
	let temp = temp_dir("file-arg-repl");
	let agent_dir = temp.join("agent");
	let sessions_dir = temp.join("sessions");
	let work_dir = temp.join("work");
	fs::create_dir_all(&agent_dir).expect("create agent dir");
	fs::create_dir_all(&sessions_dir).expect("create sessions dir");
	fs::create_dir_all(&work_dir).expect("create work dir");
	setup_mock_agent_dir(&agent_dir, server.port);

	let file_path = work_dir.join("sample.txt");
	fs::write(&file_path, "sample text content").expect("write sample file");

	let output = omp()
		.env("PI_CODING_AGENT_DIR", agent_dir.to_str().unwrap())
		.args([
			"--session-dir",
			sessions_dir.to_str().unwrap(),
			"--session-id",
			"file-arg-session",
			"--no-tools",
			"--cwd",
			work_dir.to_str().unwrap(),
			"@sample.txt",
		])
		.output()
		.expect("run omp @sample.txt");

	let stderr = String::from_utf8_lossy(&output.stderr);
	assert_eq!(
		output.status.code(),
		Some(0),
		"omp @sample.txt must not enter REPL or fail; exit 0 expected: {stderr}"
	);

	let files = fs::read_dir(&sessions_dir)
		.expect("read session dir")
		.flatten()
		.map(|e| e.path())
		.filter(|p| p.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
		.collect::<Vec<_>>();
	assert!(!files.is_empty(), "must write session log");

	let content = fs::read_to_string(&files[0]).expect("read session file");
	let lines: Vec<&str> = content.lines().collect();
	assert!(lines.len() >= 4, "must have recorded turn: found {}", lines.len());

	let user_entry: serde_json::Value = serde_json::from_str(lines[2]).expect("user entry json");
	assert_eq!(user_entry["type"], "message");
	assert_eq!(user_entry["message"]["role"], "user");
	let user_text = user_entry["message"]["content"].as_str().expect("user content string");
	assert!(
		user_text.contains("<file name=\"") && user_text.contains("sample.txt\">"),
		"user prompt must format <file name=...>: got {user_text}"
	);
	assert!(
		user_text.contains("sample text content"),
		"user prompt must contain file content: got {user_text}"
	);

	let _ = fs::remove_dir_all(temp);
}

// File-only invocation (`omp -p @file`) with empty/closed stdin executes the prompt directly
// without blocking for input.
#[test]
fn test_cli_file_arg_print_mode_does_not_block_on_empty_stdin() {
	let server = MockSseServer::start();
	let temp = temp_dir("file-arg-print");
	let agent_dir = temp.join("agent");
	let sessions_dir = temp.join("sessions");
	let work_dir = temp.join("work");
	fs::create_dir_all(&agent_dir).expect("create agent dir");
	fs::create_dir_all(&sessions_dir).expect("create sessions dir");
	fs::create_dir_all(&work_dir).expect("create work dir");
	setup_mock_agent_dir(&agent_dir, server.port);

	let file_path = work_dir.join("code.rs");
	fs::write(&file_path, "fn hello() {}").expect("write code file");

	let output = omp()
		.env("PI_CODING_AGENT_DIR", agent_dir.to_str().unwrap())
		.args([
			"--session-dir",
			sessions_dir.to_str().unwrap(),
			"--session-id",
			"file-arg-print-session",
			"--no-tools",
			"--cwd",
			work_dir.to_str().unwrap(),
			"-p",
			"@code.rs",
		])
		.output()
		.expect("run omp -p @code.rs");

	let stderr = String::from_utf8_lossy(&output.stderr);
	assert_eq!(
		output.status.code(),
		Some(0),
		"omp -p @code.rs must succeed exit 0: {stderr}"
	);
	let stdout = String::from_utf8_lossy(&output.stdout);
	assert!(
		stdout.contains("mock answer"),
		"print output must contain mock answer, got: {stdout}"
	);

	let _ = fs::remove_dir_all(temp);
}

#[test]
fn test_cli_file_arg_combined_with_prompt_message() {
	let server = MockSseServer::start();
	let temp = temp_dir("file-arg-combined");
	let agent_dir = temp.join("agent");
	let sessions_dir = temp.join("sessions");
	let work_dir = temp.join("work");
	fs::create_dir_all(&agent_dir).expect("create agent dir");
	fs::create_dir_all(&sessions_dir).expect("create sessions dir");
	fs::create_dir_all(&work_dir).expect("create work dir");
	setup_mock_agent_dir(&agent_dir, server.port);

	let file_path = work_dir.join("data.txt");
	fs::write(&file_path, "first file payload").expect("write data file");

	let output = omp()
		.env("PI_CODING_AGENT_DIR", agent_dir.to_str().unwrap())
		.args([
			"--session-dir",
			sessions_dir.to_str().unwrap(),
			"--session-id",
			"file-arg-combined-session",
			"--no-tools",
			"--cwd",
			work_dir.to_str().unwrap(),
			"@data.txt",
			"explain this payload",
		])
		.output()
		.expect("run omp @data.txt prompt");

	let stderr = String::from_utf8_lossy(&output.stderr);
	assert_eq!(output.status.code(), Some(0), "exit 0: {stderr}");

	let files = fs::read_dir(&sessions_dir)
		.expect("read session dir")
		.flatten()
		.map(|e| e.path())
		.filter(|p| p.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
		.collect::<Vec<_>>();
	assert!(!files.is_empty(), "must write session log");

	let content = fs::read_to_string(&files[0]).expect("read session file");
	let lines: Vec<&str> = content.lines().collect();
	let user_entry: serde_json::Value = serde_json::from_str(lines[2]).expect("user entry json");
	let user_text = user_entry["message"]["content"].as_str().expect("user content string");
	assert!(
		user_text.contains("<file name=\"") && user_text.contains("first file payload"),
		"must contain file payload block"
	);
	assert!(
		user_text.contains("explain this payload"),
		"must contain prompt message"
	);

	let _ = fs::remove_dir_all(temp);
}

#[test]
fn test_cli_file_arg_missing_file_fails_honestly() {
	let temp = temp_dir("missing-file");
	let agent_dir = temp.join("agent");
	let work_dir = temp.join("work");
	fs::create_dir_all(&agent_dir).expect("create agent dir");
	fs::create_dir_all(&work_dir).expect("create work dir");
	setup_mock_agent_dir(&agent_dir, 9999);

	let output = omp()
		.env("PI_CODING_AGENT_DIR", agent_dir.to_str().unwrap())
		.args([
			"--cwd",
			work_dir.to_str().unwrap(),
			"@does_not_exist_12345.txt",
		])
		.output()
		.expect("run omp with missing file");

	assert_eq!(
		output.status.code(),
		Some(1),
		"missing file arg must exit 1 (EXIT_FAILURE)"
	);
	let stderr = String::from_utf8_lossy(&output.stderr);
	assert!(
		stderr.contains("Error: File not found:") && stderr.contains("does_not_exist_12345.txt"),
		"stderr must report File not found error, got: {stderr}"
	);

	let _ = fs::remove_dir_all(temp);
}


#[test]
fn test_cli_file_arg_svg_treated_as_text_matching_ts() {
	let server = MockSseServer::start();
	let temp = temp_dir("svg-file");
	let agent_dir = temp.join("agent");
	let sessions_dir = temp.join("sessions");
	let work_dir = temp.join("work");
	fs::create_dir_all(&agent_dir).expect("create agent dir");
	fs::create_dir_all(&sessions_dir).expect("create sessions dir");
	fs::create_dir_all(&work_dir).expect("create work dir");
	setup_mock_agent_dir(&agent_dir, server.port);

	let svg_path = work_dir.join("logo.svg");
	let svg_content = "<svg xmlns=\"http://www.w3.org/2000/svg\"><circle r=\"5\"/></svg>";
	fs::write(&svg_path, svg_content).expect("write svg");

	let output = omp()
		.env("PI_CODING_AGENT_DIR", agent_dir.to_str().unwrap())
		.args([
			"--session-dir",
			sessions_dir.to_str().unwrap(),
			"--session-id",
			"svg-turn-session",
			"--no-tools",
			"--cwd",
			work_dir.to_str().unwrap(),
			"@logo.svg",
		])
		.output()
		.expect("run omp with svg file");

	let stderr = String::from_utf8_lossy(&output.stderr);
	assert_eq!(
		output.status.code(),
		Some(0),
		"svg file arg must be processed as text, exit 0: {stderr}"
	);

	let files = fs::read_dir(&sessions_dir)
		.expect("read session dir")
		.flatten()
		.map(|e| e.path())
		.filter(|p| p.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
		.collect::<Vec<_>>();
	assert!(!files.is_empty(), "must write session log");

	let content = fs::read_to_string(&files[0]).expect("read session file");
	let lines: Vec<&str> = content.lines().collect();
	let user_entry: serde_json::Value = serde_json::from_str(lines[2]).expect("user entry json");
	let user_text = user_entry["message"]["content"].as_str().expect("user content string");
	assert!(
		user_text.contains("<file name=\"") && user_text.contains("logo.svg\">"),
		"user prompt must format <file name=...>: got {user_text}"
	);
	assert!(
		user_text.contains(svg_content),
		"user prompt must contain raw svg xml: got {user_text}"
	);

	let _ = fs::remove_dir_all(temp);
}

#[test]
fn test_cli_delayed_piped_stdin_producer_with_file_and_prompt_order() {
	let server = MockSseServer::start();
	let temp = temp_dir("delayed-stdin");
	let agent_dir = temp.join("agent");
	let sessions_dir = temp.join("sessions");
	let work_dir = temp.join("work");
	fs::create_dir_all(&agent_dir).expect("create agent dir");
	fs::create_dir_all(&sessions_dir).expect("create sessions dir");
	fs::create_dir_all(&work_dir).expect("create work dir");
	setup_mock_agent_dir(&agent_dir, server.port);

	let file1 = work_dir.join("f1.txt");
	fs::write(&file1, "content1").expect("write f1");
	let file2 = work_dir.join("f2.txt");
	fs::write(&file2, "content2").expect("write f2");

	let mut child = omp()
		.env("PI_CODING_AGENT_DIR", agent_dir.to_str().unwrap())
		.args([
			"--session-dir",
			sessions_dir.to_str().unwrap(),
			"--session-id",
			"delayed-session",
			"--no-tools",
			"--cwd",
			work_dir.to_str().unwrap(),
			"-p",
			"@f1.txt",
			"@f2.txt",
			"prompt_tail",
		])
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.expect("spawn omp with piped stdin");

	let mut stdin = child.stdin.take().expect("child stdin pipe");
	let producer = thread::spawn(move || {
		// Delayed producer: simulate slow pipe writer producing data after delay.
		thread::sleep(Duration::from_millis(50));
		stdin.write_all(b"delayed_stdin_header\n").expect("write delayed stdin");
		// Explicit close (EOF) according to Unix pipe semantics and TS contract (main.ts:199-221).
		drop(stdin);
	});

	let output = child.wait_with_output().expect("wait with output");
	producer.join().expect("join producer thread");

	assert_eq!(output.status.code(), Some(0));

	let files = fs::read_dir(&sessions_dir)
		.expect("read session dir")
		.flatten()
		.map(|e| e.path())
		.filter(|p| p.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
		.collect::<Vec<_>>();
	assert!(!files.is_empty(), "must write session log");

	let content = fs::read_to_string(&files[0]).expect("read session file");
	let lines: Vec<&str> = content.lines().collect();
	let user_entry: serde_json::Value = serde_json::from_str(lines[2]).expect("user entry json");
	let user_text = user_entry["message"]["content"].as_str().expect("user content string");

	let pos_stdin = user_text.find("delayed_stdin_header").expect("stdin pos");
	let pos_f1 = user_text.find("f1.txt").expect("f1 pos");
	let pos_f2 = user_text.find("f2.txt").expect("f2 pos");
	let pos_prompt = user_text.find("prompt_tail").expect("prompt pos");

	assert!(
		pos_stdin < pos_f1 && pos_f1 < pos_f2 && pos_f2 < pos_prompt,
		"order must be delayed_stdin < f1 < f2 < prompt: got {user_text}"
	);

	let _ = fs::remove_dir_all(temp);
}
#[test]
fn test_cli_bare_piped_stdin_without_flags_runs_autoprint_turn() {
	let server = MockSseServer::start();
	let temp = temp_dir("bare-piped-stdin");
	let agent_dir = temp.join("agent");
	let sessions_dir = temp.join("sessions");
	let work_dir = temp.join("work");
	fs::create_dir_all(&agent_dir).expect("create agent dir");
	fs::create_dir_all(&sessions_dir).expect("create sessions dir");
	fs::create_dir_all(&work_dir).expect("create work dir");
	setup_mock_agent_dir(&agent_dir, server.port);

	let mut child = omp()
		.env("PI_CODING_AGENT_DIR", agent_dir.to_str().unwrap())
		.args([
			"--session-dir",
			sessions_dir.to_str().unwrap(),
			"--session-id",
			"bare-piped-session",
			"--no-tools",
			"--cwd",
			work_dir.to_str().unwrap(),
		])
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.expect("spawn omp with bare piped stdin");

	if let Some(mut stdin) = child.stdin.take() {
		stdin.write_all(b"bare piped prompt question\n").expect("write piped stdin");
		drop(stdin);
	}

	let output = child.wait_with_output().expect("wait with output");
	assert_eq!(output.status.code(), Some(0));

	let stdout = String::from_utf8_lossy(&output.stdout);
	assert!(stdout.contains("mock answer"), "must print mock answer in print mode: {stdout}");

	let files = fs::read_dir(&sessions_dir)
		.expect("read session dir")
		.flatten()
		.map(|e| e.path())
		.filter(|p| p.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
		.collect::<Vec<_>>();
	assert!(!files.is_empty(), "must write session log");

	let content = fs::read_to_string(&files[0]).expect("read session file");
	let lines: Vec<&str> = content.lines().collect();
	let user_entry: serde_json::Value = serde_json::from_str(lines[2]).expect("user entry json");
	let user_text = user_entry["message"]["content"].as_str().expect("user content string");
	assert!(user_text.contains("bare piped prompt question"), "log must contain piped prompt: {user_text}");

	let _ = fs::remove_dir_all(temp);
}
#[test]
fn test_cli_piped_stdin_to_explicit_repl_subcommand() {
	let server = MockSseServer::start();
	let temp = temp_dir("piped-repl");
	let agent_dir = temp.join("agent");
	let sessions_dir = temp.join("sessions");
	let work_dir = temp.join("work");
	fs::create_dir_all(&agent_dir).expect("create agent dir");
	fs::create_dir_all(&sessions_dir).expect("create sessions dir");
	fs::create_dir_all(&work_dir).expect("create work dir");
	setup_mock_agent_dir(&agent_dir, server.port);

	let mut child = omp()
		.env("PI_CODING_AGENT_DIR", agent_dir.to_str().unwrap())
		.args([
			"--session-dir",
			sessions_dir.to_str().unwrap(),
			"--session-id",
			"piped-repl-session",
			"--no-tools",
			"--cwd",
			work_dir.to_str().unwrap(),
			"repl",
		])
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.expect("spawn omp repl with piped stdin");

	if let Some(mut stdin) = child.stdin.take() {
		stdin.write_all(b"repl piped question\n/exit\n").expect("write repl pipe");
		drop(stdin);
	}

	let output = child.wait_with_output().expect("wait with output");
	assert_eq!(output.status.code(), Some(0));

	let files = fs::read_dir(&sessions_dir)
		.expect("read session dir")
		.flatten()
		.map(|e| e.path())
		.filter(|p| p.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
		.collect::<Vec<_>>();
	assert!(!files.is_empty(), "must write session log");

	let content = fs::read_to_string(&files[0]).expect("read session file");
	let lines: Vec<&str> = content.lines().collect();
	assert!(lines.len() >= 4, "must record turn in repl: found {}", lines.len());

	let user_entry: serde_json::Value = serde_json::from_str(lines[2]).expect("user entry json");
	let user_text = user_entry["message"]["content"].as_str().expect("user content string");
	assert!(
		user_text.contains("repl piped question"),
		"repl must process prompt from stdin: {user_text}"
	);

	let _ = fs::remove_dir_all(temp);
}

#[test]
fn test_cli_message_order_stdin_files_prompts() {
	let server = MockSseServer::start();
	let temp = temp_dir("order-test");
	let agent_dir = temp.join("agent");
	let sessions_dir = temp.join("sessions");
	let work_dir = temp.join("work");
	fs::create_dir_all(&agent_dir).expect("create agent dir");
	fs::create_dir_all(&sessions_dir).expect("create sessions dir");
	fs::create_dir_all(&work_dir).expect("create work dir");
	setup_mock_agent_dir(&agent_dir, server.port);

	let file1 = work_dir.join("f1.txt");
	fs::write(&file1, "content1").expect("write f1");
	let file2 = work_dir.join("f2.txt");
	fs::write(&file2, "content2").expect("write f2");

	let stdin_path = work_dir.join("stdin.txt");
	fs::write(&stdin_path, "stdin_header\n").expect("write stdin file");

	let output = omp()
		.env("PI_CODING_AGENT_DIR", agent_dir.to_str().unwrap())
		.args([
			"--session-dir",
			sessions_dir.to_str().unwrap(),
			"--session-id",
			"order-session",
			"--no-tools",
			"--cwd",
			work_dir.to_str().unwrap(),
			"-p",
			"@f1.txt",
			"@f2.txt",
			"prompt_tail",
		])
		.stdin(Stdio::from(fs::File::open(&stdin_path).expect("open stdin file")))
		.output()
		.expect("run omp with redirected stdin");
	assert_eq!(output.status.code(), Some(0));

	let files = fs::read_dir(&sessions_dir)
		.expect("read session dir")
		.flatten()
		.map(|e| e.path())
		.filter(|p| p.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
		.collect::<Vec<_>>();
	assert!(!files.is_empty(), "must write session log");

	let content = fs::read_to_string(&files[0]).expect("read session file");
	let lines: Vec<&str> = content.lines().collect();
	let user_entry: serde_json::Value = serde_json::from_str(lines[2]).expect("user entry json");
	let user_text = user_entry["message"]["content"].as_str().expect("user content string");

	let pos_stdin = user_text.find("stdin_header").expect("stdin pos");
	let pos_f1 = user_text.find("f1.txt").expect("f1 pos");
	let pos_f2 = user_text.find("f2.txt").expect("f2 pos");
	let pos_prompt = user_text.find("prompt_tail").expect("prompt pos");

	assert!(
		pos_stdin < pos_f1 && pos_f1 < pos_f2 && pos_f2 < pos_prompt,
		"order must be stdin < f1 < f2 < prompt: got {user_text}"
	);

	let _ = fs::remove_dir_all(temp);
}

#[test]
fn test_cli_file_arg_oversize_skips_content() {
	let server = MockSseServer::start();
	let temp = temp_dir("oversize-file");
	let agent_dir = temp.join("agent");
	let sessions_dir = temp.join("sessions");
	let work_dir = temp.join("work");
	fs::create_dir_all(&agent_dir).expect("create agent dir");
	fs::create_dir_all(&sessions_dir).expect("create sessions dir");
	fs::create_dir_all(&work_dir).expect("create work dir");
	setup_mock_agent_dir(&agent_dir, server.port);

	let oversize_path = work_dir.join("big.txt");
	let chunk = vec![b'A'; 1024 * 1024];
	{
		let mut f = fs::File::create(&oversize_path).expect("create big file");
		for _ in 0..5 {
			f.write_all(&chunk).expect("write chunk");
		}
		f.write_all(&vec![b'A'; 512 * 1024]).expect("write last half mb");
	}

	let output = omp()
		.env("PI_CODING_AGENT_DIR", agent_dir.to_str().unwrap())
		.args([
			"--session-dir",
			sessions_dir.to_str().unwrap(),
			"--session-id",
			"oversize-session",
			"--no-tools",
			"--cwd",
			work_dir.to_str().unwrap(),
			"@big.txt",
		])
		.output()
		.expect("run omp with oversize file");

	assert_eq!(output.status.code(), Some(0));
	let stderr = String::from_utf8_lossy(&output.stderr);
	assert!(
		stderr.contains("Warning: Skipping file contents (too large:"),
		"stderr must contain skipping warning: {stderr}"
	);

	let files = fs::read_dir(&sessions_dir)
		.expect("read session dir")
		.flatten()
		.map(|e| e.path())
		.filter(|p| p.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
		.collect::<Vec<_>>();
	assert!(!files.is_empty(), "must write session log");

	let content = fs::read_to_string(&files[0]).expect("read session file");
	let lines: Vec<&str> = content.lines().collect();
	let user_entry: serde_json::Value = serde_json::from_str(lines[2]).expect("user entry json");
	let user_text = user_entry["message"]["content"].as_str().expect("user content string");
	assert!(
		user_text.contains("<file name=\"") && user_text.contains("(skipped: too large,"),
		"must contain skipped block: {user_text}"
	);

	let _ = fs::remove_dir_all(temp);
}

#[test]
fn test_cli_file_arg_fifo_rejected_nonblocking() {
	let temp = temp_dir("fifo-file");
	let agent_dir = temp.join("agent");
	let work_dir = temp.join("work");
	fs::create_dir_all(&agent_dir).expect("create agent dir");
	fs::create_dir_all(&work_dir).expect("create work dir");
	setup_mock_agent_dir(&agent_dir, 9999);

	let fifo_path = work_dir.join("test.fifo");
	let mkfifo_status = ProcCommand::new("mkfifo")
		.arg(&fifo_path)
		.status();
	if mkfifo_status.is_err() || !mkfifo_status.unwrap().success() {
		use std::ffi::CString;
		unsafe extern "C" {
			fn mkfifo(path: *const std::os::raw::c_char, mode: std::os::raw::c_uint) -> std::os::raw::c_int;
		}
		let c_path = CString::new(fifo_path.to_str().unwrap()).unwrap();
		let ret = unsafe { mkfifo(c_path.as_ptr(), 0o600) };
		if ret != 0 {
			let _ = fs::remove_dir_all(temp);
			return;
		}
	}

	let output = omp()
		.env("PI_CODING_AGENT_DIR", agent_dir.to_str().unwrap())
		.args([
			"--cwd",
			work_dir.to_str().unwrap(),
			"@test.fifo",
		])
		.output()
		.expect("run omp with fifo");

	assert_eq!(
		output.status.code(),
		Some(1),
		"fifo must exit 1"
	);
	let stderr = String::from_utf8_lossy(&output.stderr);
	assert!(
		stderr.contains("not a regular file"),
		"stderr must report not a regular file, got: {stderr}"
	);

	let _ = fs::remove_file(&fifo_path);
	let _ = fs::remove_dir_all(temp);
}

#[test]
fn test_cli_file_arg_invalid_utf8_lossy_decoded() {
	let server = MockSseServer::start();
	let temp = temp_dir("invalid-utf8");
	let agent_dir = temp.join("agent");
	let sessions_dir = temp.join("sessions");
	let work_dir = temp.join("work");
	fs::create_dir_all(&agent_dir).expect("create agent dir");
	fs::create_dir_all(&sessions_dir).expect("create sessions dir");
	fs::create_dir_all(&work_dir).expect("create work dir");
	setup_mock_agent_dir(&agent_dir, server.port);

	let invalid_path = work_dir.join("corrupt.txt");
	fs::write(&invalid_path, b"hello \xff\xfe world").expect("write invalid utf8 file");

	let output = omp()
		.env("PI_CODING_AGENT_DIR", agent_dir.to_str().unwrap())
		.args([
			"--session-dir",
			sessions_dir.to_str().unwrap(),
			"--session-id",
			"invalid-utf8-session",
			"--no-tools",
			"--cwd",
			work_dir.to_str().unwrap(),
			"@corrupt.txt",
		])
		.output()
		.expect("run omp with invalid utf8 file");

	assert_eq!(output.status.code(), Some(0));

	let files = fs::read_dir(&sessions_dir)
		.expect("read session dir")
		.flatten()
		.map(|e| e.path())
		.filter(|p| p.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
		.collect::<Vec<_>>();
	assert!(!files.is_empty(), "must write session log");

	let content = fs::read_to_string(&files[0]).expect("read session file");
	let lines: Vec<&str> = content.lines().collect();
	let user_entry: serde_json::Value = serde_json::from_str(lines[2]).expect("user entry json");
	let user_text = user_entry["message"]["content"].as_str().expect("user content string");
	assert!(
		user_text.contains("hello \u{fffd}\u{fffd} world"),
		"must contain lossy decoded string with replacement characters: {user_text}"
	);

	let _ = fs::remove_dir_all(temp);
}
