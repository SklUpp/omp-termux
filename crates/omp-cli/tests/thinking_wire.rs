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

/// Default reply body: one content delta, then the done frame.
const DEFAULT_SSE: &str =
	"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";

struct MockWireServer {
	port: u16,
	requests: Arc<Mutex<Vec<serde_json::Value>>>,
	stop_tx: mpsc::Sender<()>,
	handle: Option<thread::JoinHandle<()>>,
}

impl MockWireServer {
	fn start() -> Self {
		Self::start_with(DEFAULT_SSE)
	}

	/// Same server, answering with a caller-supplied SSE body.
	fn start_with(sse_body: &str) -> Self {
		let sse_body = sse_body.to_string();
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
						let body = sse_body.as_str();
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

/// Provider stream that reports a public reasoning block before the answer.
/// Two chunks, each carrying the block in a different public field
/// (`reasoning_content`, then `reasoning_text`), so both decoders are exercised.
const REASONING_SSE: &str = concat!(
	"data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"weigh\"}}]}\n\n",
	"data: {\"choices\":[{\"delta\":{\"reasoning_text\":\" options\"}}]}\n\n",
	"data: {\"choices\":[{\"delta\":{\"content\":\"answer\"}}]}\n\n",
	"data: [DONE]\n\n"
);

/// Run the binary with exactly the given arguments (no forced mode or session).
fn run_omp_raw(agent_dir: &Path, cwd: &Path, args: &[&str]) -> Output {
	let mut cmd = ProcCommand::new(env!("CARGO_BIN_EXE_omp"));
	cmd.env("PI_CODING_AGENT_DIR", agent_dir);
	cmd.stdin(Stdio::null());
	cmd.args(["--cwd", cwd.to_str().expect("utf8 cwd")]);
	cmd.args(args);
	let out = cmd.output().expect("run omp");
	assert!(
		out.status.success(),
		"omp failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	out
}

/// NDJSON events with volatile fields (timestamps, generated ids) removed so two
/// runs of the same stream compare equal.
fn stable_events(stdout: &[u8]) -> Vec<serde_json::Value> {
	fn scrub(value: &mut serde_json::Value) {
		match value {
			serde_json::Value::Object(map) => {
				for volatile in ["timestamp", "id", "sessionId"] {
					map.remove(volatile);
				}
				for nested in map.values_mut() {
					scrub(nested);
				}
			},
			serde_json::Value::Array(items) => {
				for item in items {
					scrub(item);
				}
			},
			_ => {},
		}
	}

	String::from_utf8_lossy(stdout)
		.lines()
		.filter(|line| !line.trim().is_empty())
		.map(|line| {
			let mut value: serde_json::Value =
				serde_json::from_str(line).expect("json mode emits NDJSON only");
			scrub(&mut value);
			value
		})
		.collect()
}

/// Rebuild streamed blocks strictly by `contentIndex`, the way a host does.
///
/// Asserts each block keeps one kind and indices are contiguous from zero, so
/// this fails if the answer and the reasoning are folded into one index.
fn blocks_by_index(events: &[serde_json::Value]) -> Vec<(String, String)> {
	let mut blocks: Vec<Option<(String, String)>> = Vec::new();
	for event in events {
		let delta = &event["assistantMessageEvent"];
		let kind = match delta["type"].as_str() {
			Some("text_delta") => "text",
			Some("thinking_delta") => "thinking",
			_ => continue,
		};
		let index = delta["contentIndex"]
			.as_u64()
			.expect("streamed delta must carry contentIndex") as usize;
		assert!(
			index <= blocks.len(),
			"contentIndex {index} is not contiguous: {event}"
		);
		if blocks.len() == index {
			blocks.push(None);
		}
		let slot = blocks[index]
			.get_or_insert_with(|| (kind.to_string(), String::new()));
		assert_eq!(slot.0, kind, "one block must not mix kinds: {event}");
		slot.1.push_str(delta["delta"].as_str().unwrap_or_default());
	}
	blocks
		.into_iter()
		.map(|block| block.expect("no empty contentIndex"))
		.collect()
}

/// Assistant blocks in the finalized message, in message order.
fn final_assistant_blocks(events: &[serde_json::Value]) -> Vec<(String, String)> {
	let agent_end = events
		.iter()
		.rev()
		.find(|event| event["type"] == "agent_end")
		.expect("agent_end must be emitted");
	let message = agent_end["messages"]
		.as_array()
		.expect("agent_end.messages")
		.iter()
		.rev()
		.find(|message| message["role"] == "assistant")
		.expect("final assistant message");
	message["content"]
		.as_array()
		.expect("assistant content blocks")
		.iter()
		.map(|block| {
			let kind = block["type"].as_str().unwrap_or_default().to_string();
			let text = block["thinking"]
				.as_str()
				.or(block["text"].as_str())
				.unwrap_or_default();
			(kind, text.to_string())
		})
		.collect()
}

/// Real binary against a real SSE endpoint: public provider reasoning stays
/// apart from the answer in the text render (gated by `--print-thoughts`), in
/// the JSON stream (ungated), in the session log (once), and in the next
/// outbound request (never).
#[test]
fn public_reasoning_stays_apart_and_never_replays() {
	let server = MockWireServer::start_with(REASONING_SSE);
	let base = temp_dir("reasoning-stream");
	let (agent_dir, cwd) = setup(server.port, &base);
	let session_dir = base.join("sessions");
	fs::create_dir_all(&session_dir).expect("session dir");
	let session_dir_arg = session_dir.to_str().expect("utf8 session dir").to_string();

	// Default text mode: only the answer reaches stdout.
	let plain = run_omp_raw(
		&agent_dir,
		&cwd,
		&["--no-session", "--no-tools", "--no-title", "--mode", "text", "say hi"],
	);
	assert_eq!(String::from_utf8_lossy(&plain.stdout).trim(), "answer");

	// `--print-thoughts`: reasoning renders before the answer, still separate.
	let thoughts = run_omp_raw(
		&agent_dir,
		&cwd,
		&[
			"--no-session",
			"--no-tools",
			"--no-title",
			"--mode",
			"text",
			"--print-thoughts",
			"say hi",
		],
	);
	assert_eq!(
		String::from_utf8_lossy(&thoughts.stdout).trim(),
		"weigh options\nanswer"
	);

	// JSON mode is flag-independent: the same event stream either way.
	let json_off = run_omp_raw(
		&agent_dir,
		&cwd,
		&["--no-session", "--no-tools", "--no-title", "--mode", "json", "say hi"],
	);
	let json_on = run_omp_raw(
		&agent_dir,
		&cwd,
		&[
			"--no-session",
			"--no-tools",
			"--no-title",
			"--mode",
			"json",
			"--print-thoughts",
			"say hi",
		],
	);
	let off = stable_events(&json_off.stdout);
	let on = stable_events(&json_on.stdout);
	assert!(!off.is_empty(), "json mode must emit events");
	let deltas = |events: &[serde_json::Value], kind: &str| -> String {
		events
			.iter()
			.filter(|event| event["assistantMessageEvent"]["type"].as_str() == Some(kind))
			.filter_map(|event| event["assistantMessageEvent"]["delta"].as_str())
			.collect()
	};
	assert_eq!(deltas(&off, "thinking_delta"), "weigh options");
	assert_eq!(deltas(&off, "text_delta"), "answer");
	// A host rebuilding by `contentIndex` must get two clean blocks, in the
	// streamed order, and the finalized message must agree with that order.
	let expected = vec![
		("thinking".to_string(), "weigh options".to_string()),
		("text".to_string(), "answer".to_string()),
	];
	assert_eq!(blocks_by_index(&off), expected);
	assert_eq!(final_assistant_blocks(&off), expected);
	assert_eq!(off, on, "the print flag must not change the JSON stream");

	// Persisted turn: one thinking block beside the answer, never inside it.
	run_omp_raw(
		&agent_dir,
		&cwd,
		&[
			"--session-dir",
			session_dir_arg.as_str(),
			"--session-id",
			"reasoning-turn",
			"--no-tools",
			"--no-title",
			"--mode",
			"text",
			"say hi",
		],
	);
	let log_path = fs::read_dir(&session_dir)
		.expect("read session dir")
		.flatten()
		.map(|entry| entry.path())
		.find(|path| {
			path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
				&& path
					.file_stem()
					.and_then(|stem| stem.to_str())
					.map_or(false, |stem| stem.ends_with("reasoning-turn"))
		})
		.expect("session log must exist");
	let log = fs::read_to_string(&log_path).expect("read session log");
	assert_eq!(
		log.matches("weigh options").count(),
		1,
		"reasoning must persist exactly once: {log}"
	);
	assert!(
		log.contains(r#"{"type":"thinking","thinking":"weigh options"}"#),
		"thinking must persist as a typed part: {log}"
	);
	assert!(
		log.contains(r#"{"type":"text","text":"answer"}"#),
		"answer must persist unchanged: {log}"
	);

	// Resumed turn replays the history but never the reasoning.
	run_omp_raw(
		&agent_dir,
		&cwd,
		&[
			"--session-dir",
			session_dir_arg.as_str(),
			"--resume",
			"reasoning-turn",
			"--no-tools",
			"--no-title",
			"--mode",
			"text",
			"and again",
		],
	);
	let requests = server.requests.lock().expect("lock").clone();
	assert!(requests.len() >= 2, "resume must issue a request");
	for body in &requests {
		assert!(
			!body.to_string().contains("weigh options"),
			"public reasoning must never be replayed outbound: {body}"
		);
	}
	server.stop();
}

/// Provider stream that sends the answer first and the reasoning afterwards, the
/// order some OpenAI-compatible servers use for tool-ish turns.
const TEXT_FIRST_SSE: &str = concat!(
	"data: {\"choices\":[{\"delta\":{\"content\":\"answer\"}}]}\n\n",
	"data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"weigh\"}}]}\n\n",
	"data: {\"choices\":[{\"delta\":{\"reasoning_text\":\" options\"}}]}\n\n",
	"data: [DONE]\n\n"
);

/// Answer before reasoning: indices, text render, persisted parts and the
/// resumed turn all keep the streamed order instead of regrouping to
/// thinking-first.
#[test]
fn text_first_reasoning_keeps_block_order_on_resume() {
	let server = MockWireServer::start_with(TEXT_FIRST_SSE);
	let base = temp_dir("reasoning-text-first");
	let (agent_dir, cwd) = setup(server.port, &base);
	let session_dir = base.join("sessions");
	fs::create_dir_all(&session_dir).expect("session dir");
	let session_dir_arg = session_dir.to_str().expect("utf8 session dir").to_string();

	// Answer only by default; `--print-thoughts` renders in message order, so the
	// answer still comes first.
	let plain = run_omp_raw(
		&agent_dir,
		&cwd,
		&["--no-session", "--no-tools", "--no-title", "--mode", "text", "say hi"],
	);
	assert_eq!(String::from_utf8_lossy(&plain.stdout).trim(), "answer");
	let thoughts = run_omp_raw(
		&agent_dir,
		&cwd,
		&[
			"--session-dir",
			session_dir_arg.as_str(),
			"--session-id",
			"text-first-turn",
			"--no-tools",
			"--no-title",
			"--mode",
			"text",
			"--print-thoughts",
			"say hi",
		],
	);
	assert_eq!(
		String::from_utf8_lossy(&thoughts.stdout).trim(),
		"answer\nweigh options"
	);

	let expected = vec![
		("text".to_string(), "answer".to_string()),
		("thinking".to_string(), "weigh options".to_string()),
	];

	// Persisted turn keeps answer-first block order, each block once.
	let log_path = fs::read_dir(&session_dir)
		.expect("read session dir")
		.flatten()
		.map(|entry| entry.path())
		.find(|path| {
			path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
				&& path
					.file_stem()
					.and_then(|stem| stem.to_str())
					.map_or(false, |stem| stem.ends_with("text-first-turn"))
		})
		.expect("session log must exist");
	let log = fs::read_to_string(&log_path).expect("read session log");
	assert_eq!(log.matches("weigh options").count(), 1, "{log}");
	let text_at = log
		.find(r#"{"type":"text","text":"answer"}"#)
		.expect("answer part must persist");
	let thinking_at = log
		.find(r#"{"type":"thinking","thinking":"weigh options"}"#)
		.expect("thinking part must persist");
	assert!(
		text_at < thinking_at,
		"persisted parts must keep streamed order: {log}"
	);

	// Resumed turn: history replays in the recorded order and the new stream is
	// still indexed from zero with one kind per block.
	let resumed = run_omp_raw(
		&agent_dir,
		&cwd,
		&[
			"--session-dir",
			session_dir_arg.as_str(),
			"--resume",
			"text-first-turn",
			"--no-tools",
			"--no-title",
			"--mode",
			"json",
			"and again",
		],
	);
	let events = stable_events(&resumed.stdout);
	assert_eq!(blocks_by_index(&events), expected);
	assert_eq!(final_assistant_blocks(&events), expected);
	for body in server.requests.lock().expect("lock").iter() {
		assert!(
			!body.to_string().contains("weigh options"),
			"public reasoning must never be replayed outbound: {body}"
		);
	}
	server.stop();
}
