//! Two real `--no-session` prompts against a bounded mock SSE provider:
//! prompt 1 runs `todo init`, prompt 2 runs `todo view`. The view result must
//! carry the first prompt's phases even though no journal exists, and the run
//! must write no session files. `details.storage` stays `memory`.

use std::{
	fs,
	io::{Read, Write},
	net::{TcpListener, TcpStream},
	path::{Path, PathBuf},
	process::{Command as ProcCommand, Stdio},
	sync::{
		Arc, Mutex,
		atomic::{AtomicBool, AtomicU32, Ordering},
	},
	thread,
	time::{Duration, Instant},
};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_dir(prefix: &str) -> PathBuf {
	let dir = std::env::temp_dir().join(format!(
		"omp-todo-no-session-{prefix}-{}-{}",
		std::process::id(),
		COUNTER.fetch_add(1, Ordering::Relaxed)
	));
	fs::create_dir_all(&dir).expect("create temp dir");
	dir
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

/// Scripted provider: a user prompt mentioning `SETUP` gets a `todo init`
/// call, `CHECK` gets `todo view`, and any continuation (the last message is
/// the tool result) gets a final assistant text.
struct MockModel {
	port: u16,
	stop: Arc<AtomicBool>,
	requests: Arc<Mutex<Vec<serde_json::Value>>>,
	accept: Option<thread::JoinHandle<()>>,
}

impl MockModel {
	fn start() -> Self {
		let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
		let port = listener.local_addr().expect("addr").port();
		listener.set_nonblocking(true).expect("nonblocking");
		let stop = Arc::new(AtomicBool::new(false));
		let requests = Arc::new(Mutex::new(Vec::new()));

		let accept = {
			let stop = Arc::clone(&stop);
			let requests = Arc::clone(&requests);
			thread::spawn(move || {
				let deadline = Instant::now() + Duration::from_secs(30);
				while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
					match listener.accept() {
						Ok((mut stream, _)) => {
							let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
							let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
							let body = read_request(&mut stream);
							if let Ok(body) = serde_json::from_slice::<serde_json::Value>(&body) {
								requests.lock().expect("requests").push(body);
							}
							let response = respond(&body);
							let _ = stream.write_all(
								format!(
									"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
									response.len(),
									response
								)
								.as_bytes(),
							);
							let _ = stream.flush();
						},
						Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
							thread::sleep(Duration::from_millis(10));
						},
						Err(_) => break,
					}
				}
			})
		};

		Self { port, stop, requests, accept: Some(accept) }
	}
}

impl Drop for MockModel {
	fn drop(&mut self) {
		self.stop.store(true, Ordering::SeqCst);
		if let Some(handle) = self.accept.take() {
			let _ = handle.join();
		}
	}
}

fn read_request(stream: &mut TcpStream) -> Vec<u8> {
	let mut buf = Vec::new();
	let mut chunk = [0u8; 1024];
	let mut header_end = None;
	while header_end.is_none() {
		match stream.read(&mut chunk) {
			Ok(0) | Err(_) => return buf,
			Ok(n) => {
				buf.extend_from_slice(&chunk[..n]);
				header_end = buf.windows(4).position(|window| window == b"\r\n\r\n");
			},
		}
	}
	let header_end = header_end.expect("header terminator");
	let headers = String::from_utf8_lossy(&buf[..header_end]).into_owned();
	let content_length = headers
		.lines()
		.find_map(|line| {
			let (name, value) = line.split_once(':')?;
			name.trim()
				.eq_ignore_ascii_case("content-length")
				.then(|| value.trim().parse::<usize>().ok())?
		})
		.unwrap_or(0);
	let mut body = buf[header_end + 4..].to_vec();
	while body.len() < content_length {
		match stream.read(&mut chunk) {
			Ok(0) | Err(_) => break,
			Ok(n) => body.extend_from_slice(&chunk[..n]),
		}
	}
	body
}

fn respond(request: &[u8]) -> String {
	let Ok(body) = serde_json::from_slice::<serde_json::Value>(request) else {
		return text_body("ack");
	};
	let last = body
		.get("messages")
		.and_then(serde_json::Value::as_array)
		.and_then(|messages| messages.last());
	let Some(last) = last else {
		return text_body("ack");
	};
	if last.get("role").and_then(serde_json::Value::as_str) != Some("user") {
		return text_body("ack");
	}
	let text = message_text(last);
	if text.contains("SETUP") {
		tool_call_body(
			"call-init",
			"todo",
			&serde_json::json!({
				"op": "init",
				"list": [{"phase": "Plan", "items": ["seed task content"]}],
			}),
		)
	} else if text.contains("CHECK") {
		tool_call_body("call-view", "todo", &serde_json::json!({"op": "view"}))
	} else {
		text_body("ack")
	}
}

fn tool_call_body(id: &str, name: &str, args: &serde_json::Value) -> String {
	let arguments = serde_json::to_string(&args.to_string()).expect("encode arguments");
	format!(
		"data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"{id}\",\"type\":\"function\",\"function\":{{\"name\":\"{name}\",\"arguments\":{arguments}}}}}]}},\"finish_reason\":null}}]}}\n\ndata: [DONE]\n\n"
	)
}

fn text_body(text: &str) -> String {
	format!(
		"data: {{\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\ndata: [DONE]\n\n"
	)
}

fn message_text(message: &serde_json::Value) -> String {
	match message.get("content") {
		Some(serde_json::Value::String(text)) => text.clone(),
		Some(serde_json::Value::Array(parts)) => parts
			.iter()
			.filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
			.collect::<Vec<_>>()
			.join("\n"),
		_ => String::new(),
	}
}

#[test]
fn no_session_view_sees_phases_from_the_previous_prompt_without_a_log() {
	let server = MockModel::start();
	let temp = temp_dir("two-prompts");
	let agent_dir = temp.join("agent");
	let sessions_dir = temp.join("sessions");
	let share_dir = temp.join("share");
	let config_dir = temp.join("config");
	let cwd = temp.join("project");
	for dir in [&sessions_dir, &share_dir, &config_dir, &cwd] {
		fs::create_dir_all(dir).expect("fixture dir");
	}
	setup_agent_dir(&agent_dir, server.port);

	let output = ProcCommand::new(env!("CARGO_BIN_EXE_omp"))
		.env("PI_CODING_AGENT_DIR", &agent_dir)
		.env("PI_CONFIG_DIR", &config_dir)
		.env("OMP_SHARE_DIR", &share_dir)
		.env("PI_NO_TITLE", "1")
		.stdin(Stdio::null())
		.args(["--cwd", cwd.to_str().unwrap()])
		.args(["--session-dir", sessions_dir.to_str().unwrap()])
		.args(["--no-session", "--no-skills", "--tools", "todo", "--yolo"])
		.args(["--max-time", "20s", "--mode", "text", "-p"])
		.args(["SETUP the plan", "CHECK the plan"])
		.output()
		.expect("run omp with two prompts");

	let stderr = String::from_utf8_lossy(&output.stderr);
	assert_eq!(
		output.status.code(),
		Some(0),
		"both no-session prompts must succeed: {stderr}"
	);

	let requests = server.requests.lock().expect("requests").clone();
	assert_eq!(requests.len(), 4, "two prompts with one tool round-trip each");
	let last = requests.last().expect("view continuation request");
	let messages = last
		.get("messages")
		.and_then(serde_json::Value::as_array)
		.expect("messages");
	let result = messages.last().expect("view tool result");
	assert_eq!(result.get("role").and_then(serde_json::Value::as_str), Some("tool"));
	assert_eq!(
		result.get("name").and_then(serde_json::Value::as_str),
		Some("todo"),
		"last message must be the todo view result"
	);
	let result_text = message_text(result);
	assert!(
		result_text.contains("seed task content"),
		"todo view must see the previous prompt's phases without a journal: {result_text}"
	);
	assert!(
		!result_text.contains("Todo list is empty"),
		"restored phases must not degrade to an empty list: {result_text}"
	);

	assert_eq!(
		fs::read_dir(&sessions_dir).expect("sessions dir").count(),
		0,
		"--no-session must not write session files"
	);

	fs::remove_dir_all(temp).ok();
}
