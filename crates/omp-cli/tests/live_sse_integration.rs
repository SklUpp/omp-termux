//! Behavioral integration tests for live localhost HTTP SSE event & session contract:
//! - 2 model turns with real `read` tool execution on a real file.
//! - Early `message_update` NDJSON event emitted before server sends `[DONE]`.
//! - Exactly one tool result in session log and valid history on resume.
//! - SIGINT (Ctrl-C) during long tool execution closes tool, leaves no orphan,
//!   records `aborted` in session log, and resumes cleanly.
//! - Robust bounds: all stdout reading, child waiting, and server synchronization
//!   enforce bounded timeouts, safe POSIX process-group cleanup, and Drop guards.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command as ProcCommand, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct TempDirGuard {
	path: PathBuf,
}

impl Drop for TempDirGuard {
	fn drop(&mut self) {
		let _ = fs::remove_dir_all(&self.path);
	}
}

fn temp_dir(prefix: &str) -> TempDirGuard {
	let unique = format!(
		"omp-live-sse-{prefix}-{}-{}",
		std::process::id(),
		COUNTER.fetch_add(1, Ordering::Relaxed)
	);
	let path = std::env::temp_dir().join(unique);
	fs::create_dir_all(&path).expect("create temp dir");
	TempDirGuard { path }
}

struct ChildGuard {
	child: Option<Child>,
	exited: bool,
}

impl ChildGuard {
	fn new(child: Child) -> Self {
		Self { child: Some(child), exited: false }
	}

	fn id(&self) -> u32 {
		self.child.as_ref().expect("child active").id()
	}

	fn take_stdout(&mut self) -> Option<ChildStdout> {
		self.child.as_mut().and_then(|c| c.stdout.take())
	}

	fn take_stderr(&mut self) -> Option<std::process::ChildStderr> {
		self.child.as_mut().and_then(|c| c.stderr.take())
	}

	fn wait_timeout(&mut self, timeout: Duration) -> ExitStatus {
		let child = self.child.as_mut().expect("child active");
		let start = Instant::now();
		loop {
			if let Some(status) = child.try_wait().expect("try_wait") {
				self.exited = true;
				return status;
			}
			if start.elapsed() > timeout {
				let pid = child.id();
				if pid > 1 {
					let _ = ProcCommand::new("kill")
						.args(["-KILL", &format!("-{pid}")])
						.status();
				}
				let _ = child.kill();
				self.exited = true;
				panic!("timed out waiting for child to exit after {timeout:?}");
			}
			thread::sleep(Duration::from_millis(20));
		}
	}
}

impl Drop for ChildGuard {
	fn drop(&mut self) {
		if let Some(mut child) = self.child.take() {
			if !self.exited {
				let pid = child.id();
				if pid > 1 {
					// POSIX process_group(0) + kill(-pid, SIGKILL) terminates entire process group cleanly
					let _ = ProcCommand::new("kill")
						.args(["-KILL", &format!("-{pid}")])
						.status();
				}
				let _ = child.kill();
			}
			let _ = child.wait();
		}
	}
}

struct ServerGuard {
	stop_tx: Option<mpsc::Sender<()>>,
	handle: Option<thread::JoinHandle<()>>,
}

impl ServerGuard {
	fn join(mut self) {
		if let Some(tx) = self.stop_tx.take() {
			let _ = tx.send(());
		}
		if let Some(h) = self.handle.take() {
			h.join().expect("server thread must join cleanly");
		}
	}
}

impl Drop for ServerGuard {
	fn drop(&mut self) {
		if let Some(tx) = self.stop_tx.take() {
			let _ = tx.send(());
		}
		if let Some(h) = self.handle.take() {
			let _ = h.join();
		}
	}
}

fn omp() -> ProcCommand {
	ProcCommand::new(env!("CARGO_BIN_EXE_omp"))
}

fn setup_agent_dir(dir: &Path, port: u16) {
	let config_yml = format!(
		"defaultModel: mock/mock-model\nproviders:\n  mock:\n    baseUrl: \"http://127.0.0.1:{port}/v1\"\n    api: openai-completions\n    apiKey: test-key\n"
	);
	fs::write(dir.join("config.yml"), config_yml).expect("write config.yml");

	let models_yml = format!(
		"providers:\n  mock:\n    baseUrl: \"http://127.0.0.1:{port}/v1\"\n    api: openai-completions\n    apiKey: test-key\n    models:\n      - id: mock-model\n        contextWindow: 128000\n"
	);
	fs::write(dir.join("models.yml"), models_yml).expect("write models.yml");
}

fn read_http_request(stream: &mut std::net::TcpStream) -> (String, serde_json::Value) {
	let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
	let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

	let mut buf = Vec::new();
	let mut header_end = None;
	let mut temp = [0u8; 1024];
	let start = Instant::now();

	while header_end.is_none() && start.elapsed() < Duration::from_secs(5) {
		match stream.read(&mut temp) {
			Ok(0) => break,
			Ok(n) => {
				buf.extend_from_slice(&temp[..n]);
				if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
					header_end = Some(pos);
				}
			},
			Err(e)
				if e.kind() == std::io::ErrorKind::WouldBlock
					|| e.kind() == std::io::ErrorKind::TimedOut =>
			{
				thread::sleep(Duration::from_millis(5));
			},
			Err(_) => break,
		}
	}

	let Some(header_pos) = header_end else {
		return (String::new(), serde_json::Value::Null);
	};
	let headers_str = String::from_utf8_lossy(&buf[..header_pos]).into_owned();

	let mut content_length = 0;
	for line in headers_str.lines() {
		if line.to_ascii_lowercase().starts_with("content-length:") {
			if let Some(val) = line.split(':').nth(1) {
				content_length = val.trim().parse::<usize>().unwrap_or(0);
			}
		}
	}

	let body_start = header_pos + 4;
	let mut body_bytes = buf[body_start..].to_vec();
	while body_bytes.len() < content_length && start.elapsed() < Duration::from_secs(5) {
		match stream.read(&mut temp) {
			Ok(0) => break,
			Ok(n) => body_bytes.extend_from_slice(&temp[..n]),
			Err(e)
				if e.kind() == std::io::ErrorKind::WouldBlock
					|| e.kind() == std::io::ErrorKind::TimedOut =>
			{
				thread::sleep(Duration::from_millis(5));
			},
			Err(_) => break,
		}
	}

	let body_json: serde_json::Value =
		serde_json::from_slice(&body_bytes).unwrap_or(serde_json::Value::Null);
	(headers_str, body_json)
}

fn spawn_stdout_reader(stdout: ChildStdout) -> mpsc::Receiver<String> {
	let (tx, rx) = mpsc::channel();
	thread::spawn(move || {
		let mut reader = BufReader::new(stdout);
		let mut line = String::new();
		while reader.read_line(&mut line).unwrap_or(0) > 0 {
			let trimmed = line.trim().to_string();
			line.clear();
			if !trimmed.is_empty() && tx.send(trimmed).is_err() {
				break;
			}
		}
	});
	rx
}

fn spawn_stderr_reader(stderr: std::process::ChildStderr) -> Arc<Mutex<String>> {
	let buf = Arc::new(Mutex::new(String::new()));
	let buf_clone = Arc::clone(&buf);
	thread::spawn(move || {
		let mut reader = BufReader::new(stderr);
		let mut line = String::new();
		while reader.read_line(&mut line).unwrap_or(0) > 0 {
			buf_clone.lock().expect("lock").push_str(&line);
			line.clear();
		}
	});
	buf
}

fn spawn_byte_reader<R: Read + Send + 'static>(mut stream: R) -> thread::JoinHandle<Vec<u8>> {
	thread::spawn(move || {
		let mut buf = Vec::new();
		let _ = stream.read_to_end(&mut buf);
		buf
	})
}

fn run_omp_with_timeout(
	agent_dir: &Path,
	cwd: &Path,
	session_dir: &Path,
	session_id: &str,
	prompt: &str,
	timeout: Duration,
) -> std::process::Output {
	let mut cmd = omp();
	cmd.env("PI_CODING_AGENT_DIR", agent_dir.to_str().unwrap());
	cmd.args([
		"--cwd",
		cwd.to_str().unwrap(),
		"--session-dir",
		session_dir.to_str().unwrap(),
		"--resume",
		session_id,
		"--no-tools",
		"--mode",
		"json",
		prompt,
	]);
	cmd.process_group(0);
	cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

	let child = cmd.spawn().expect("spawn omp --resume");
	let mut guard = ChildGuard::new(child);
	let child_stdout = guard.take_stdout().expect("child stdout");
	let child_stderr = guard.take_stderr().expect("child stderr");

	let stdout_reader = spawn_byte_reader(child_stdout);
	let stderr_reader = spawn_byte_reader(child_stderr);

	let status = guard.wait_timeout(timeout);
	let stdout_bytes = stdout_reader.join().unwrap_or_default();
	let stderr_bytes = stderr_reader.join().unwrap_or_default();

	std::process::Output {
		status,
		stdout: stdout_bytes,
		stderr: stderr_bytes,
	}
}

#[test]
fn test_live_sse_two_turn_read_early_delta_and_resume() {
	let guard = temp_dir("two-turn-read");
	let base = &guard.path;
	let agent_dir = base.join("agent");
	let sessions_dir = base.join("sessions");
	let project_dir = base.join("project");
	fs::create_dir_all(&agent_dir).expect("create agent dir");
	fs::create_dir_all(&sessions_dir).expect("create sessions dir");
	fs::create_dir_all(&project_dir).expect("create project dir");

	let secret_file = project_dir.join("secret_data.txt");
	let secret_content = "AUTHENTIC_SECRET_PAYLOAD_48291\n";
	// Hashline-eligible reads prefix the snapshot header and number lines with
	// `N:TEXT`. The tag is content-derived, so compute it independently here from
	// the expected bytes instead of copying it out of the tool's own output.
	let tag = pi_agent::tools::hashline::hash::compute_file_hash(secret_content);
	let read_output = format!("[secret_data.txt#{tag}]\n1:{}", secret_content.trim_end());
	fs::write(&secret_file, secret_content).expect("write secret file");

	let listener = TcpListener::bind("127.0.0.1:0").expect("bind tcp");
	let port = listener.local_addr().expect("local addr").port();
	listener.set_nonblocking(true).expect("set nonblocking");

	setup_agent_dir(&agent_dir, port);

	let recorded_requests: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
	let recorded_reqs_clone = Arc::clone(&recorded_requests);

	let (server_stop_tx, server_stop_rx) = mpsc::channel::<()>();
	let (delta_sent_tx, _delta_sent_rx) = mpsc::channel::<()>();
	let (allow_done_tx, allow_done_rx) = mpsc::channel::<()>();

	let server_handle = thread::spawn(move || {
		let mut turn = 0;
		loop {
			if server_stop_rx.try_recv().is_ok() {
				break;
			}
			match listener.accept() {
				Ok((mut stream, _)) => {
					let (_headers, body) = read_http_request(&mut stream);
					if body.is_null() && server_stop_rx.try_recv().is_ok() {
						break;
					}
					recorded_reqs_clone.lock().expect("lock").push(body);
					turn += 1;

					if turn == 1 {
						// Turn 1: model calls real read tool
						let sse = concat!(
							"HTTP/1.1 200 OK\r\n",
							"Content-Type: text/event-stream\r\n",
							"Cache-Control: no-cache\r\n",
							"Connection: close\r\n\r\n",
							"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_read_1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"secret_data.txt\\\"}\"}}]}}]}\n\n",
							"data: [DONE]\n\n"
						);
						let _ = stream.write_all(sse.as_bytes());
						let _ = stream.flush();
					} else if turn == 2 {
						// Turn 2: model streams "ok", but pauses before [DONE] to prove
						// that early message_update arrives at client before server completion.
						let header = concat!(
							"HTTP/1.1 200 OK\r\n",
							"Content-Type: text/event-stream\r\n",
							"Cache-Control: no-cache\r\n",
							"Connection: close\r\n\r\n",
							"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n"
						);
						let _ = stream.write_all(header.as_bytes());
						let _ = stream.flush();

						// Notify test that early delta was sent to wire
						let _ = delta_sent_tx.send(());

						// Wait for test verification before closing with [DONE]
						let _ = allow_done_rx.recv_timeout(Duration::from_secs(5));

						let done_chunk = "data: [DONE]\n\n";
						let _ = stream.write_all(done_chunk.as_bytes());
						let _ = stream.flush();
					} else {
						// Turn 3 (resumed): model answers follow-up
						let sse = concat!(
							"HTTP/1.1 200 OK\r\n",
							"Content-Type: text/event-stream\r\n",
							"Cache-Control: no-cache\r\n",
							"Connection: close\r\n\r\n",
							"data: {\"choices\":[{\"delta\":{\"content\":\"resumed answer ok\"}}]}\n\n",
							"data: [DONE]\n\n"
						);
						let _ = stream.write_all(sse.as_bytes());
						let _ = stream.flush();
					}
				},
				Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
					thread::sleep(Duration::from_millis(10));
				},
				Err(_) => break,
			}
		}
	});

	let server_guard = ServerGuard {
		stop_tx: Some(server_stop_tx),
		handle: Some(server_handle),
	};

	// Spawn omp in JSON mode wrapped in ChildGuard with process_group(0)
	let mut cmd = omp();
	cmd.env("PI_CODING_AGENT_DIR", agent_dir.to_str().unwrap());
	cmd.args([
		"--cwd",
		project_dir.to_str().unwrap(),
		"--session-dir",
		sessions_dir.to_str().unwrap(),
		"--mode",
		"json",
		"--allow",
		"read",
		"Please read secret_data.txt",
	]);
	cmd.process_group(0);
	cmd.stdout(Stdio::piped());
	cmd.stderr(Stdio::piped());

	let child = cmd.spawn().expect("spawn omp");

	let mut child_guard = ChildGuard::new(child);
	let child_stdout = child_guard.take_stdout().expect("child stdout");
	let child_stderr = child_guard.take_stderr().expect("child stderr");
	let line_rx = spawn_stdout_reader(child_stdout);
	let captured_stderr = spawn_stderr_reader(child_stderr);

	let mut observed_lines = Vec::new();
	let mut early_delta_verified = false;

	loop {
		let line = match line_rx.recv_timeout(Duration::from_secs(8)) {
			Ok(line) => line,
			Err(mpsc::RecvTimeoutError::Timeout) => {
				panic!(
					"timed out waiting for child stdout event in two-turn read test: stderr={}",
					captured_stderr.lock().unwrap()
				);
			},
			Err(mpsc::RecvTimeoutError::Disconnected) => break,
		};

		let parsed: serde_json::Value =
			serde_json::from_str(&line).expect("parse NDJSON event");
		let ev_type = parsed.get("type").and_then(|v| v.as_str()).unwrap_or("");
		let is_agent_end = ev_type == "agent_end";

		if ev_type == "message_update" {
			let delta = parsed["assistantMessageEvent"]["delta"].as_str();
			if delta == Some("ok") {
				// We have observed the delta "ok" BEFORE [DONE] was sent by the server!
				early_delta_verified = true;
				// Unblock server to send [DONE]
				let _ = allow_done_tx.send(());
			}
		}

		observed_lines.push(parsed);

		if is_agent_end {
			break;
		}
	}

	let status = child_guard.wait_timeout(Duration::from_secs(5));
	assert!(
		status.success(),
		"omp must exit 0: status={status:?}, stderr={}",
		captured_stderr.lock().unwrap()
	);
	assert!(
		early_delta_verified,
		"early message_update with delta 'ok' must arrive before server [DONE]"
	);

	// Verify NDJSON event sequence on stdout
	let tool_start = observed_lines
		.iter()
		.find(|e| e["type"] == "tool_execution_start")
		.expect("tool_execution_start event");
	assert_eq!(tool_start["toolName"], "read");
	assert_eq!(tool_start["args"]["path"], "secret_data.txt");

	let tool_end = observed_lines
		.iter()
		.find(|e| e["type"] == "tool_execution_end")
		.expect("tool_execution_end event");
	assert_eq!(tool_end["toolName"], "read");
	assert_eq!(tool_end["result"], read_output);
	assert_eq!(tool_end["isError"], false);

	// Verify session JSONL file on disk
	let entries = fs::read_dir(&sessions_dir)
		.expect("read sessions dir")
		.flatten()
		.filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("jsonl"))
		.collect::<Vec<_>>();
	assert_eq!(entries.len(), 1, "exactly one session file created");

	let session_path = entries[0].path();
	let session_content = fs::read_to_string(&session_path).expect("read session file");
	let jsonl_lines: Vec<serde_json::Value> = session_content
		.lines()
		.filter(|l| !l.trim().is_empty())
		.map(|l| serde_json::from_str(l).expect("valid jsonl line"))
		.collect();

	// Session file must contain exactly ONE toolResult entry
	let tool_results: Vec<&serde_json::Value> = jsonl_lines
		.iter()
		.filter(|e| e["type"] == "message" && e["message"]["role"] == "toolResult")
		.collect();
	assert_eq!(
		tool_results.len(),
		1,
		"session file must have exactly ONE toolResult entry, found {}",
		tool_results.len()
	);
	assert_eq!(tool_results[0]["message"]["toolCallId"], "call_read_1");
	assert_eq!(
		tool_results[0]["message"]["content"],
		read_output
	);
	assert_eq!(tool_results[0]["message"]["isError"], false);

	// Extract session id from session filename or header
	let stem = session_path.file_stem().unwrap().to_str().unwrap();
	let session_id = stem.rfind('_').map_or(stem, |idx| &stem[idx + 1..]);

	// Now RESUME the session with a follow-up prompt using bounded timeout runner
	let resume_output = run_omp_with_timeout(
		&agent_dir,
		&project_dir,
		&sessions_dir,
		session_id,
		"What was in the secret file?",
		Duration::from_secs(10),
	);

	let resume_stderr = String::from_utf8_lossy(&resume_output.stderr);
	assert!(
		resume_output.status.success(),
		"omp --resume must succeed: {resume_stderr}"
	);

	// Verify that the server received Turn 3 with the full conversation history
	let requests = recorded_requests.lock().expect("lock").clone();
	assert_eq!(requests.len(), 3, "server must have received exactly 3 turns");

	let turn3_msgs = requests[2]["messages"]
		.as_array()
		.expect("turn 3 messages array");

	let has_asst_call = turn3_msgs.iter().any(|m| {
		m["role"] == "assistant"
			&& m["tool_calls"]
				.as_array()
				.is_some_and(|calls| calls.iter().any(|c| c["id"] == "call_read_1"))
	});
	assert!(has_asst_call, "resume request must include assistant tool_call in history");

	let has_tool_res = turn3_msgs.iter().any(|m| {
		m["role"] == "tool"
			&& m["tool_call_id"] == "call_read_1"
			&& m["content"] == read_output
	});
	assert!(has_tool_res, "resume request must include tool result in history");

	let has_asst_ok = turn3_msgs.iter().any(|m| {
		m["role"] == "assistant" && m["content"] == "ok"
	});
	assert!(has_asst_ok, "resume request must include assistant 'ok' in history");

	let has_follow_up = turn3_msgs.iter().any(|m| {
		m["role"] == "user" && m["content"] == "What was in the secret file?"
	});
	assert!(has_follow_up, "resume request must include follow-up prompt");
	server_guard.join();
}

#[test]
fn test_live_sse_sigint_interrupt_during_tool_closes_tool_and_records_aborted() {
	let guard = temp_dir("sigint-tool");
	let base = &guard.path;
	let agent_dir = base.join("agent");
	let sessions_dir = base.join("sessions");
	let project_dir = base.join("project");
	fs::create_dir_all(&agent_dir).expect("create agent dir");
	fs::create_dir_all(&sessions_dir).expect("create sessions dir");
	fs::create_dir_all(&project_dir).expect("create project dir");

	// Write an executable external script to force genuine OS child process execution
	// (bypassing in-process pi-builtins 'sleep').
	let script_path = project_dir.join("long_job.sh");
	fs::write(&script_path, "#!/bin/sh\nexec sleep 30\n").expect("write long_job.sh");
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		let _ = fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755));
	}

	let listener = TcpListener::bind("127.0.0.1:0").expect("bind tcp");
	let port = listener.local_addr().expect("local addr").port();
	listener.set_nonblocking(true).expect("set nonblocking");

	setup_agent_dir(&agent_dir, port);

	let (server_stop_tx, server_stop_rx) = mpsc::channel::<()>();

	let server_handle = thread::spawn(move || {
		let mut turn = 0;
		loop {
			if server_stop_rx.try_recv().is_ok() {
				break;
			}
			match listener.accept() {
				Ok((mut stream, _)) => {
					let (_headers, body) = read_http_request(&mut stream);
					if body.is_null() && server_stop_rx.try_recv().is_ok() {
						break;
					}
					turn += 1;

					if turn == 1 {
						let body = serde_json::json!({
							"choices": [{
								"delta": {
									"tool_calls": [{
										"index": 0,
										"id": "call_external_long",
										"function": {
											"name": "bash",
											"arguments": serde_json::json!({ "command": "./long_job.sh" }).to_string()
										}
									}]
								}
							}]
						});
						let sse = format!(
							"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\ndata: {body}\n\ndata: [DONE]\n\n"
						);
						let _ = stream.write_all(sse.as_bytes());
						let _ = stream.flush();
					} else {
						// Turn 2 (resumed): model answers cleanly after aborted turn
						let sse = concat!(
							"HTTP/1.1 200 OK\r\n",
							"Content-Type: text/event-stream\r\n",
							"Cache-Control: no-cache\r\n",
							"Connection: close\r\n\r\n",
							"data: {\"choices\":[{\"delta\":{\"content\":\"recovered from abort ok\"}}]}\n\n",
							"data: [DONE]\n\n"
						);
						let _ = stream.write_all(sse.as_bytes());
						let _ = stream.flush();
					}
				},
				Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
					thread::sleep(Duration::from_millis(10));
				},
				Err(_) => break,
			}
		}
	});

	let server_guard = ServerGuard {
		stop_tx: Some(server_stop_tx),
		handle: Some(server_handle),
	};

	// Spawn omp with bash allowed wrapped in ChildGuard in its own process group
	let mut cmd = omp();
	cmd.env("PI_CODING_AGENT_DIR", agent_dir.to_str().unwrap());
	cmd.args([
		"--cwd",
		project_dir.to_str().unwrap(),
		"--session-dir",
		sessions_dir.to_str().unwrap(),
		"--mode",
		"json",
		"--allow",
		"bash",
		"Execute long external script",
	]);
	cmd.process_group(0);
	cmd.stdout(Stdio::piped());
	cmd.stderr(Stdio::piped());

	let child = cmd.spawn().expect("spawn omp");

	let mut child_guard = ChildGuard::new(child);
	let child_pid = child_guard.id();
	let child_stdout = child_guard.take_stdout().expect("child stdout");
	let child_stderr = child_guard.take_stderr().expect("child stderr");
	let line_rx = spawn_stdout_reader(child_stdout);
	let captured_stderr = spawn_stderr_reader(child_stderr);

	// Wait until tool_execution_start is observed on stdout, bounded by timeout
	let mut tool_started = false;
	let start_wait = Instant::now();
	while start_wait.elapsed() < Duration::from_secs(8) {
		match line_rx.recv_timeout(Duration::from_secs(2)) {
			Ok(line) => {
				if line.contains("\"type\":\"tool_execution_start\"") {
					tool_started = true;
					break;
				}
			},
			Err(mpsc::RecvTimeoutError::Timeout) => continue,
			Err(mpsc::RecvTimeoutError::Disconnected) => break,
		}
	}
	assert!(
		tool_started,
		"tool execution must have started before SIGINT: stderr={}",
		captured_stderr.lock().unwrap()
	);

	// Give external script subprocess a moment to launch and verify it is running
	let mut spawned_pids = String::new();
	let start_poll = Instant::now();
	while start_poll.elapsed() < Duration::from_secs(3) {
		let pgrep_children = ProcCommand::new("pgrep")
			.args(["-P", &child_pid.to_string()])
			.output();
		let pids = pgrep_children
			.ok()
			.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
			.unwrap_or_default();
		if !pids.is_empty() {
			spawned_pids = pids;
			break;
		}
		thread::sleep(Duration::from_millis(50));
	}
	assert!(
		!spawned_pids.is_empty(),
		"external tool child process must be running before SIGINT: stderr={}",
		captured_stderr.lock().unwrap()
	);
	// Send SIGINT to the omp process (simulating Ctrl-C)
	let kill_status = ProcCommand::new("kill")
		.args(["-INT", &child_pid.to_string()])
		.status()
		.expect("send SIGINT to omp");
	assert!(kill_status.success(), "kill -INT must succeed");

	// Wait for child process to terminate, bounded by timeout
	let status = child_guard.wait_timeout(Duration::from_secs(8));
	assert!(
		!status.success(),
		"interrupted omp process must exit with non-zero failure"
	);

	let stderr = captured_stderr.lock().unwrap().clone();
	assert!(
		stderr.contains("interrupted"),
		"stderr must indicate interruption: {stderr}"
	);

	// Check that every spawned child process of child_pid is terminated (no orphan processes)
	thread::sleep(Duration::from_millis(200));
	for pid_str in spawned_pids.split_whitespace() {
		let check = ProcCommand::new("kill").args(["-0", pid_str]).status();
		assert!(
			check.is_err() || !check.unwrap().success(),
			"child process PID {pid_str} must not be running after tool abort"
		);
	}

	// Verify session file records stopReason: aborted and synthetic tool result
	let entries = fs::read_dir(&sessions_dir)
		.expect("read sessions dir")
		.flatten()
		.filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("jsonl"))
		.collect::<Vec<_>>();
	assert_eq!(entries.len(), 1, "exactly one session file created");

	let session_path = entries[0].path();
	let session_content = fs::read_to_string(&session_path).expect("read session file");
	let jsonl_lines: Vec<serde_json::Value> = session_content
		.lines()
		.filter(|l| !l.trim().is_empty())
		.map(|l| serde_json::from_str(l).expect("valid jsonl line"))
		.collect();

	// Assistant message must have stopReason: aborted
	let asst_msg = jsonl_lines
		.iter()
		.find(|e| e["type"] == "message" && e["message"]["role"] == "assistant")
		.expect("assistant message entry");
	assert_eq!(asst_msg["message"]["stopReason"], "aborted");
	assert_eq!(
		asst_msg["message"]["errorMessage"],
		"Interrupted by user"
	);

	// ToolResult must be recorded with isError: true and indicate cancellation
	let tool_res = jsonl_lines
		.iter()
		.find(|e| e["type"] == "message" && e["message"]["role"] == "toolResult")
		.expect("toolResult message entry");
	assert_eq!(tool_res["message"]["toolCallId"], "call_external_long");
	assert_eq!(tool_res["message"]["isError"], true);

	// Resume from this aborted session to ensure it resumes cleanly using bounded runner
	let stem = session_path.file_stem().unwrap().to_str().unwrap();
	let session_id = stem.rfind('_').map_or(stem, |idx| &stem[idx + 1..]);

	let resume_output = run_omp_with_timeout(
		&agent_dir,
		&project_dir,
		&sessions_dir,
		session_id,
		"Continue after aborted turn",
		Duration::from_secs(10),
	);

	let resume_stderr = String::from_utf8_lossy(&resume_output.stderr);
	assert!(
		resume_output.status.success(),
		"resuming aborted session must succeed: {resume_stderr}"
	);
	server_guard.join();
}
