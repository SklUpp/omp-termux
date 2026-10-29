//! Real-binary boundaries for interruption and persistence:
//! - SIGINT while a provider stream is live exits non-zero and leaves an
//!   `aborted` assistant turn in the session log;
//! - `--max-time` during a stalled stream surfaces the documented timeout and
//!   persists an `aborted` assistant turn (partial text is not required: the
//!   deadline may land before any delta is assembled);
//! - `--no-session` writes nothing;
//! - `--api-key` never leaks to stdout, stderr, or the session log;
//! - `--fork` creates a new identity and leaves the source untouched.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command as ProcCommand, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_dir(prefix: &str) -> PathBuf {
	let dir = std::env::temp_dir().join(format!(
		"omp-cli-boundary-{prefix}-{}-{}",
		std::process::id(),
		COUNTER.fetch_add(1, Ordering::Relaxed)
	));
	fs::create_dir_all(&dir).expect("create temp dir");
	dir
}

#[derive(Clone, Copy)]
enum ServerMode {
	Complete,
	HoldAfterPartial,
}

struct MockServer {
	port: u16,
	stop: Arc<AtomicBool>,
	auth: Arc<Mutex<Option<String>>>,
	accept: Option<thread::JoinHandle<()>>,
	workers: Arc<Mutex<Vec<thread::JoinHandle<()>>>>,
}

impl MockServer {
	fn start(mode: ServerMode) -> Self {
		let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
		let port = listener.local_addr().expect("addr").port();
		listener.set_nonblocking(true).expect("nonblocking");
		let stop = Arc::new(AtomicBool::new(false));
		let auth = Arc::new(Mutex::new(None));
		let workers = Arc::new(Mutex::new(Vec::new()));

		let accept = {
			let stop = Arc::clone(&stop);
			let auth = Arc::clone(&auth);
			let workers = Arc::clone(&workers);
			thread::spawn(move || {
				while !stop.load(Ordering::SeqCst) {
					match listener.accept() {
						Ok((mut stream, _)) => {
							let stop = Arc::clone(&stop);
							let auth = Arc::clone(&auth);
							let worker = thread::spawn(move || {
								serve(&mut stream, mode, &stop, &auth)
							});
							workers.lock().unwrap().push(worker);
						},
						Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
							thread::sleep(Duration::from_millis(5));
						},
						Err(_) => break,
					}
				}
			})
		};

		Self { port, stop, auth, accept: Some(accept), workers }
	}
}

fn serve(stream: &mut TcpStream, mode: ServerMode, stop: &AtomicBool, auth: &Mutex<Option<String>>) {
	let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
	let mut buf = Vec::new();
	let mut chunk = [0u8; 1024];
	let mut header_end = None;
	while header_end.is_none() {
		match stream.read(&mut chunk) {
			Ok(0) | Err(_) => break,
			Ok(n) => {
				buf.extend_from_slice(&chunk[..n]);
				header_end = buf.windows(4).position(|window| window == b"\r\n\r\n");
			},
		}
	}
	let headers = String::from_utf8_lossy(&buf).into_owned();
	for line in headers.lines() {
		if let Some((name, value)) = line.split_once(':')
			&& name.trim().eq_ignore_ascii_case("authorization")
		{
			*auth.lock().unwrap() = Some(value.trim().to_string());
		}
	}

	match mode {
		ServerMode::Complete => {
			let body = concat!(
				"data: {\"choices\":[{\"delta\":{\"content\":\"boundary answer\"}}]}\n\n",
				"data: [DONE]\n\n"
			);
			let response = format!(
				"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
				body.len(),
				body
			);
			let _ = stream.write_all(response.as_bytes());
			let _ = stream.flush();
		},
		ServerMode::HoldAfterPartial => {
			let _ = stream.write_all(
				b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
			);
			let event = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n";
			let _ = write!(stream, "{:x}\r\n{event}\r\n", event.len());
			let _ = stream.flush();
			let start = Instant::now();
			while !stop.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(15) {
				thread::sleep(Duration::from_millis(20));
			}
		},
	}
}

impl Drop for MockServer {
	fn drop(&mut self) {
		self.stop.store(true, Ordering::SeqCst);
		if let Some(handle) = self.accept.take() {
			let _ = handle.join();
		}
		for worker in self.workers.lock().unwrap().drain(..) {
			let _ = worker.join();
		}
	}
}

fn setup_agent_dir(dir: &Path, port: u16) {
	fs::create_dir_all(dir).expect("agent dir");
	fs::write(
		dir.join("config.yml"),
		format!(
			"defaultModel: mock/mock-model\nproviders:\n  mock:\n    baseUrl: \"http://127.0.0.1:{port}/v1\"\n    api: openai-completions\n    apiKey: test-key\n"
		),
	)
	.expect("config.yml");
	fs::write(
		dir.join("models.yml"),
		format!(
			"providers:\n  mock:\n    baseUrl: \"http://127.0.0.1:{port}/v1\"\n    api: openai-completions\n    apiKey: test-key\n    models:\n      - id: mock-model\n        contextWindow: 128000\n"
		),
	)
	.expect("models.yml");
}

struct Fixture {
	base: PathBuf,
	agent_dir: PathBuf,
	sessions_dir: PathBuf,
	share_dir: PathBuf,
}

impl Fixture {
	fn new(prefix: &str, port: u16) -> Self {
		let base = temp_dir(prefix);
		let agent_dir = base.join("agent");
		let sessions_dir = base.join("sessions");
		let share_dir = base.join("share");
		fs::create_dir_all(&sessions_dir).expect("sessions dir");
		fs::create_dir_all(&share_dir).expect("share dir");
		fs::create_dir_all(base.join("config")).expect("config dir");
		setup_agent_dir(&agent_dir, port);
		Self { base, agent_dir, sessions_dir, share_dir }
	}

	fn command(&self) -> ProcCommand {
		let mut command = ProcCommand::new(env!("CARGO_BIN_EXE_omp"));
		command
			.env("PI_CODING_AGENT_DIR", &self.agent_dir)
			.env("PI_CONFIG_DIR", self.base.join("config"))
			.env("OMP_SHARE_DIR", &self.share_dir)
			.env("PI_NO_TITLE", "1")
			.stdin(Stdio::null());
		command
	}

	fn base_args(&self) -> Vec<String> {
		vec![
			"--session-dir".to_string(),
			self.sessions_dir.to_string_lossy().into_owned(),
			"--no-tools".to_string(),
			"--mode".to_string(),
			"json".to_string(),
		]
	}

	fn session_entries(&self) -> Vec<serde_json::Value> {
		let mut files: Vec<PathBuf> = fs::read_dir(&self.sessions_dir)
			.expect("session dir")
			.flatten()
			.map(|entry| entry.path())
			.filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
			.collect();
		files.sort();
		assert_eq!(files.len(), 1, "expected exactly one session log: {files:?}");
		fs::read_to_string(&files[0])
			.expect("session log")
			.lines()
			.filter(|line| !line.trim().is_empty())
			.map(|line| serde_json::from_str(line).expect("valid session jsonl"))
			.collect()
	}
}

fn wait_timeout(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
	let start = Instant::now();
	loop {
		if let Some(status) = child.try_wait().expect("try_wait") {
			return Some(status);
		}
		if start.elapsed() > timeout {
			let _ = child.kill();
			return None;
		}
		thread::sleep(Duration::from_millis(20));
	}
}

#[test]
fn sigint_during_live_provider_stream_records_aborted_turn() {
	let server = MockServer::start(ServerMode::HoldAfterPartial);
	let fixture = Fixture::new("sigint-provider", server.port);

	let mut command = fixture.command();
	command
		.args(fixture.base_args())
		.arg("-p")
		.arg("interrupt the provider")
		.stdout(Stdio::piped())
		.stderr(Stdio::piped());
	let mut child = command.spawn().expect("spawn omp");
	let stdout = child.stdout.take().expect("stdout");
	let (line_tx, line_rx) = mpsc::channel::<String>();
	let reader = thread::spawn(move || {
		for line in BufReader::new(stdout).lines().map_while(Result::ok) {
			if line_tx.send(line).is_err() {
				break;
			}
		}
	});

	let mut saw_partial = false;
	let wait_start = Instant::now();
	while wait_start.elapsed() < Duration::from_secs(8) {
		match line_rx.recv_timeout(Duration::from_secs(2)) {
			Ok(line) => {
				if line.contains("message_update") || line.contains("partial") {
					saw_partial = true;
					break;
				}
			},
			Err(mpsc::RecvTimeoutError::Timeout) => continue,
			Err(mpsc::RecvTimeoutError::Disconnected) => break,
		}
	}
	assert!(saw_partial, "provider delta must reach stdout before SIGINT");

	let pid = child.id();
	let signal = ProcCommand::new("kill").args(["-INT", &pid.to_string()]).status();
	assert!(signal.expect("kill -INT").success(), "SIGINT must be delivered");

	let timeout = Duration::from_secs(8);
	let status = wait_timeout(&mut child, timeout).expect("interrupted omp must exit");
	assert!(!status.success(), "interrupted omp must exit non-zero");
	let mut stderr = String::new();
	child.stderr.take().expect("stderr").read_to_string(&mut stderr).expect("read stderr");
	assert!(stderr.contains("interrupted"), "stderr must say interrupted: {stderr}");
	let _ = reader.join();

	let entries = fixture.session_entries();
	let assistant = entries
		.iter()
		.find(|entry| entry["type"] == "message" && entry["message"]["role"] == "assistant")
		.expect("assistant entry");
	assert_eq!(assistant["message"]["stopReason"], "aborted");
	assert_eq!(assistant["message"]["errorMessage"], "Interrupted by user");
	assert!(
		assistant.to_string().contains("partial"),
		"the partial provider text must survive: {assistant}"
	);
}

#[test]
fn max_time_during_stalled_provider_stream_reports_timeout_and_keeps_history() {
	let server = MockServer::start(ServerMode::HoldAfterPartial);
	let fixture = Fixture::new("max-time", server.port);

	let mut command = fixture.command();
	command
		.args(fixture.base_args())
		.args(["--max-time", "1"])
		.arg("-p")
		.arg("stall until the deadline")
		.stdout(Stdio::piped())
		.stderr(Stdio::piped());
	let mut child = command.spawn().expect("spawn omp");
	let status = wait_timeout(&mut child, Duration::from_secs(15))
		.expect("--max-time must terminate the stalled turn");
	let mut stderr = String::new();
	child.stderr.take().expect("stderr").read_to_string(&mut stderr).expect("read stderr");
	assert_eq!(status.code(), Some(1), "timeout is a failure exit");
	assert!(stderr.contains("max time exceeded"), "stderr was {stderr}");

	let entries = fixture.session_entries();
	let assistant = entries
		.iter()
		.find(|entry| entry["type"] == "message" && entry["message"]["role"] == "assistant")
		.expect("assistant entry");
	assert_eq!(assistant["message"]["stopReason"], "aborted");
}

#[test]
fn no_session_never_writes_a_log() {
	let server = MockServer::start(ServerMode::Complete);
	let fixture = Fixture::new("no-session", server.port);

	let output = fixture
		.command()
		.args(fixture.base_args())
		.arg("--no-session")
		.arg("-p")
		.arg("ephemeral")
		.output()
		.expect("run omp");
	assert!(
		output.status.success(),
		"stderr: {}",
		String::from_utf8_lossy(&output.stderr)
	);
	assert!(String::from_utf8_lossy(&output.stdout).contains("boundary answer"));
	assert_eq!(
		fs::read_dir(&fixture.sessions_dir).expect("sessions dir").count(),
		0,
		"--no-session must not create a session log"
	);
}

#[test]
fn api_key_never_leaks_to_output_or_session_log() {
	let server = MockServer::start(ServerMode::Complete);
	let fixture = Fixture::new("api-key", server.port);
	let secret = "super-secret-boundary-key";

	let output = fixture
		.command()
		.args(fixture.base_args())
		.args(["--api-key", secret])
		.arg("-p")
		.arg("say something")
		.output()
		.expect("run omp");
	assert!(
		output.status.success(),
		"stderr: {}",
		String::from_utf8_lossy(&output.stderr)
	);
	let expected_auth = format!("Bearer {secret}");
	assert_eq!(
		server.auth.lock().unwrap().as_deref(),
		Some(expected_auth.as_str()),
		"the key must still reach the provider as bearer auth"
	);

	let mut everything = String::from_utf8_lossy(&output.stdout).into_owned();
	everything.push_str(&String::from_utf8_lossy(&output.stderr));
	for entry in fs::read_dir(&fixture.sessions_dir).expect("sessions dir").flatten() {
		if let Ok(content) = fs::read_to_string(entry.path()) {
			everything.push_str(&content);
		}
	}
	assert!(
		!everything.contains(secret),
		"API key must not appear in stdout, stderr, or session logs"
	);
}

#[test]
fn fork_creates_a_new_identity_and_leaves_the_source_untouched() {
	let server = MockServer::start(ServerMode::Complete);
	let fixture = Fixture::new("fork", server.port);
	let source =
		PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v3_session.jsonl");
	let before = fs::read(&source).expect("read source fixture");
	let source_id = "01a178ce-3463-7504-8047-ca4fab78a16b";

	let output = fixture
		.command()
		.args(fixture.base_args())
		.arg("--fork")
		.arg(&source)
		.args(["-p", "forked turn"])
		.output()
		.expect("run omp --fork");
	assert!(
		output.status.success(),
		"stderr: {}",
		String::from_utf8_lossy(&output.stderr)
	);

	assert_eq!(fs::read(&source).expect("re-read source"), before, "fork must not rewrite the source");
	let files: Vec<PathBuf> = fs::read_dir(&fixture.sessions_dir)
		.expect("sessions dir")
		.flatten()
		.map(|entry| entry.path())
		.filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
		.collect();
	assert_eq!(files.len(), 1, "fork must create exactly one new log");
	let new_file = &files[0];
	let entries = fixture.session_entries();
	let header = entries
		.iter()
		.find(|entry| entry["type"] == "session" || entry["type"] == "header")
		.expect("session header");
	let new_id = header["id"].as_str().expect("new id");
	assert_ne!(new_id, source_id, "fork identity must differ from the source");
	assert_eq!(header["parentSession"], source_id);
	assert!(
		new_file.to_string_lossy().contains(new_id),
		"fork filename must carry its new id: {}",
		new_file.display()
	);
	assert!(
		entries.iter().any(|entry| entry["message"]["content"]
			.to_string()
			.contains("forked turn")),
		"the forked turn must be appended to the new log"
	);
}
