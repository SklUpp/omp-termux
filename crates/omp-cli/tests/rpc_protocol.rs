//! Consumer-level tests for the RPC transport: frame chunking round-trip,
//! message paging cursors, host URI scheme validation, stdin framing, plus
//! live-session integration tests that drive [`RpcMode`] against the real
//! `PiAgentRunner` and a localhost OpenAI-compatible SSE model: prompt
//! streaming, abort, queued follow-up, EOF drain and `abort_bash`.

use std::{
	io::{BufRead, Read, Write},
	net::{TcpListener, TcpStream},
	sync::{
		Arc, Mutex,
		mpsc::{self, Receiver, Sender},
	},
	thread,
	time::{Duration, Instant},
};

use serde_json::{json, Value};

use omp_cli::{
	approve::ApprovalMode,
	catalog::Catalog,
	config::{Config, ProviderConfig},
	prompts::Prompts,
	rpc::{
		frame::{MAX_RPC_FRAME_BYTES, RpcFrameDecoder, RpcFrameEncoder},
		host_uris::HostUriBridge,
		input::{Inbound, Output, RpcInput},
		messages::{MessageSnapshot, page_messages},
		mode::RpcMode,
		session::{FrameSink, RpcLaunch},
		types::RpcHostUriSchemeDefinition,
	},
	runtime::PiAgentRunner,
	session::SessionOptions,
};

#[test]
fn v2_chunks_round_trip_through_the_decoder() {
	let big = json!({
		"type": "notice",
		"message": "y".repeat(MAX_RPC_FRAME_BYTES + 64),
	});
	let mut encoder = RpcFrameEncoder::new();
	encoder.set_protocol_version(2).expect("v2");
	let lines: Vec<String> = encoder.encode_frames(&big).collect();
	assert!(lines.len() > 1, "oversized frame must be chunked");

	let mut decoder = RpcFrameDecoder::default();
	let mut assembled = Vec::new();
	for line in &lines {
		let value: Value = serde_json::from_str(line).expect("chunk line is JSON");
		if let Some(frame) = decoder.push(value).expect("valid chunk sequence") {
			assembled.push(frame);
		}
	}
	assert_eq!(assembled, vec![big]);

	// A chunk sequence with a hole must be rejected, not silently reassembled.
	let mut decoder = RpcFrameDecoder::default();
	let first: Value = serde_json::from_str(&lines[0]).expect("chunk line is JSON");
	assert!(decoder.push(first).expect("first chunk accepted").is_none());
	let second: Value = serde_json::from_str(&lines[1]).expect("chunk line is JSON");
	let mut gap = second.clone();
	gap["index"] = json!(9);
	assert!(decoder.push(gap).is_err(), "out-of-order chunk must fail");
}

#[test]
fn message_paging_cursor_continues_and_rejects_a_changed_snapshot() {
	let messages: Vec<Value> = (0..5)
		.map(|index| json!({ "role": "user", "content": format!("m{index}") }))
		.collect();
	let snapshot = MessageSnapshot {
		session_id:    "s1".to_string(),
		leaf_id:       Some("leaf".to_string()),
		message_count: messages.len(),
	};
	let first = page_messages(&messages, &snapshot, (None, Some(2))).expect("first page");
	assert_eq!(first.messages.len(), 2);
	let cursor = first.next_cursor.clone().expect("cursor for the next page");

	let second = page_messages(&messages, &snapshot, (Some(&cursor), Some(2))).expect("second page");
	assert_eq!(second.messages.len(), 2);
	assert_ne!(first.messages, second.messages);

	// A snapshot that no longer matches the live list must fail closed.
	let stale = MessageSnapshot { message_count: 4, ..snapshot };
	assert!(page_messages(&messages, &stale, (Some(&cursor), Some(2))).is_err());
}

#[test]
fn reserved_host_uri_schemes_are_rejected_before_registration() {
	let sink: Arc<dyn FrameSink> = Arc::new(RecordingSink::default());
	let bridge = Arc::new(HostUriBridge::new(sink));
	let reserved = RpcHostUriSchemeDefinition {
		scheme:      "security".to_string(),
		description: None,
		writable:    false,
		immutable:   true,
	};
	let error = bridge.set_schemes(&[reserved]).expect_err("security:// is reserved");
	assert!(error.contains("reserved"), "{error}");

	let custom = RpcHostUriSchemeDefinition {
		scheme:      "notes".to_string(),
		description: Some("host notes".to_string()),
		writable:    true,
		immutable:   false,
	};
	let schemes = bridge.set_schemes(&[custom]).expect("custom scheme accepted");
	assert_eq!(schemes, vec!["notes".to_string()]);
}

#[test]
fn stdin_reader_skips_blank_lines_and_reports_garbage() {
	let mut input = RpcInput::new(std::io::Cursor::new(
		b"\n   \n{\"type\":\"get_state\",\"id\":\"1\"}\nnot json\n".to_vec(),
	));
	let first = input.next().expect("io").expect("line");
	match first {
		Ok(Inbound::Command(value)) => assert_eq!(value["type"], "get_state"),
		other => panic!("expected a command, got {other:?}"),
	}
	let second = input.next().expect("io").expect("line");
	match second {
		Ok(_) => panic!("garbage must not parse as a command"),
		Err(error) => assert!(error.starts_with("Failed to parse command"), "{error}"),
	}
	assert!(input.next().expect("io").is_none(), "EOF is reported");
}

#[test]
fn writer_keeps_frame_order_and_flips_to_v2_after_negotiation() {
	#[derive(Clone, Default)]
	struct Buffer(Arc<Mutex<Vec<u8>>>);

	impl std::io::Write for Buffer {
		fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
			self.0.lock().expect("buffer").extend_from_slice(data);
			Ok(data.len())
		}

		fn flush(&mut self) -> std::io::Result<()> {
			Ok(())
		}
	}

	let buffer = Buffer::default();
	let output = Output::spawn(buffer.clone());
	output.send(json!({ "type": "ready", "protocolVersion": 1 }));
	output.send(json!({
		"id": "1",
		"type": "response",
		"command": "negotiate_protocol",
		"success": true,
		"data": { "protocolVersion": 2 },
	}));
	output.send(json!({ "type": "notice", "message": "z".repeat(MAX_RPC_FRAME_BYTES + 8) }));
	assert_eq!(output.finish(), None);

	let text = String::from_utf8(buffer.0.lock().expect("buffer").clone()).expect("utf8");
	let lines: Vec<&str> = text.lines().collect();
	assert_eq!(lines[0], r#"{"type":"ready","protocolVersion":1}"#);
	assert!(lines[1].contains(r#""command":"negotiate_protocol""#));
	assert!(lines[2].contains(r#""type":"rpc_chunk""#));
	assert!(
		lines
			.iter()
			.all(|line| line.len() < MAX_RPC_FRAME_BYTES),
		"no line may exceed the advertised frame bound"
	);
}

#[derive(Default)]
struct RecordingSink(Mutex<Vec<Value>>);

impl FrameSink for RecordingSink {
	fn send(&self, frame: Value) {
		self.0.lock().expect("sink").push(frame);
	}
}

// ------------------------------------------------------------ live RPC mode --

type SharedBytes = Arc<Mutex<Vec<u8>>>;

struct SharedWriter(SharedBytes);

impl Write for SharedWriter {
	fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
		self.0.lock().expect("writer").extend_from_slice(data);
		Ok(data.len())
	}

	fn flush(&mut self) -> std::io::Result<()> {
		Ok(())
	}
}

/// Stdin substitute: blocks for the next chunk and reports EOF when the test
/// drops its sender.
struct ChannelReader {
	rx:  Receiver<Vec<u8>>,
	buf: Vec<u8>,
	pos: usize,
	eof: bool,
}

impl ChannelReader {
	fn new(rx: Receiver<Vec<u8>>) -> Self {
		Self { rx, buf: Vec::new(), pos: 0, eof: false }
	}
}

impl Read for ChannelReader {
	fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
		let available = self.fill_buf()?;
		let len = available.len().min(out.len());
		out[..len].copy_from_slice(&available[..len]);
		self.consume(len);
		Ok(len)
	}
}

impl BufRead for ChannelReader {
	fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
		while self.pos >= self.buf.len() {
			if self.eof {
				return Ok(&[]);
			}
			match self.rx.recv() {
				Ok(chunk) => {
					self.buf = chunk;
					self.pos = 0;
				},
				Err(_) => {
					self.eof = true;
					return Ok(&[]);
				},
			}
		}
		Ok(&self.buf[self.pos..])
	}

	fn consume(&mut self, amount: usize) {
		self.pos += amount;
	}
}

fn parse_frames(bytes: &[u8]) -> Vec<Value> {
	let text = String::from_utf8_lossy(bytes);
	let Some(last_newline) = text.rfind('\n') else {
		return Vec::new();
	};
	text[..=last_newline]
		.lines()
		.filter(|line| !line.trim().is_empty())
		.filter_map(|line| serde_json::from_str(line).ok())
		.collect()
}

struct Harness {
	tx:     Mutex<Option<Sender<Vec<u8>>>>,
	out:    SharedBytes,
	handle: Option<thread::JoinHandle<omp_cli::Result<()>>>,
}

impl Harness {
	fn start(launch: RpcLaunch) -> Self {
		let (tx, rx) = mpsc::channel();
		let out = Arc::new(Mutex::new(Vec::new()));
		let reader = ChannelReader::new(rx);
		let writer = SharedWriter(Arc::clone(&out));
		let handle = thread::Builder::new()
			.name("rpc-test-mode".to_string())
			.spawn(move || RpcMode::run(launch, reader, writer))
			.expect("spawn rpc mode");
		Self { tx: Mutex::new(Some(tx)), out, handle: Some(handle) }
	}

	fn send(&self, frame: Value) {
		let guard = self.tx.lock().expect("tx");
		guard
			.as_ref()
			.expect("stdin still open")
			.send(format!("{frame}\n").into_bytes())
			.expect("send frame");
	}

	fn close_stdin(&self) {
		self.tx.lock().expect("tx").take();
	}

	fn frames(&self) -> Vec<Value> {
		parse_frames(&self.out.lock().expect("out"))
	}

	fn wait_for(&self, mut done: impl FnMut(&[Value]) -> bool, timeout: Duration) -> Vec<Value> {
		let deadline = Instant::now() + timeout;
		loop {
			let frames = self.frames();
			if done(&frames) {
				return frames;
			}
			assert!(
				Instant::now() < deadline,
				"timed out waiting for RPC frame; got {frames:#?}"
			);
			thread::sleep(Duration::from_millis(10));
		}
	}

	fn wait_frame(&self, kind: &str, timeout: Duration) -> Vec<Value> {
		self.wait_for(
			|frames| frames.iter().any(|frame| frame["type"] == kind),
			timeout,
		)
	}

	fn join(mut self) -> omp_cli::Result<()> {
		self.close_stdin();
		self
			.handle
			.take()
			.expect("handle")
			.join()
			.expect("rpc mode thread")
	}
}

impl Drop for Harness {
	fn drop(&mut self) {
		self.close_stdin();
		if let Some(handle) = self.handle.take() {
			let _ = handle.join();
		}
	}
}

/// Wait until the streamed turn is journaled as `aborted`.
///
/// `abort_and_prompt` retires the live turn's token inside the run lock and only
/// then writes its response, so this frame proves the old turn can no longer
/// complete with `stop`. Releasing a paused model script before it is a race the
/// fixture — not the mode — decides, which is exactly what made the replacement
/// test flaky.
fn wait_aborted(harness: &Harness) {
	harness.wait_for(
		|frames| {
			frames.iter().any(|frame| {
				frame["type"] == "message_end" && frame["message"]["stopReason"] == "aborted"
			})
		},
		timeout(),
	);
}

struct StreamScript {
	text:  String,
	/// When present the server holds the response open until the test signals.
	pause: Option<Receiver<()>>,
}

fn spawn_model(scripts: Vec<StreamScript>) -> u16 {
	spawn_model_recording(scripts).0
}

/// [`spawn_model`] plus the user prompt of every request the mock served.
///
/// The scripted answer text is positional, so it cannot identify the prompt that
/// asked for it: a test asserting *which* prompt reached the provider has to read
/// the request.
fn spawn_model_recording(scripts: Vec<StreamScript>) -> (u16, Arc<Mutex<Vec<String>>>) {
	let prompts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
	let recorded = Arc::clone(&prompts);
	let listener = TcpListener::bind("127.0.0.1:0").expect("bind model");
	let port = listener.local_addr().expect("addr").port();
	thread::spawn(move || {
		listener
			.set_nonblocking(false)
			.expect("blocking listener");
		for script in scripts {
			let Ok((mut stream, _)) = listener.accept() else {
				return;
			};
			stream
				.set_read_timeout(Some(Duration::from_secs(5)))
				.expect("read timeout");
			let Some(request) = read_request(&mut stream) else {
				continue;
			};
			if let Some(prompt) = user_prompt(&request) {
				recorded
					.lock()
					.unwrap_or_else(std::sync::PoisonError::into_inner)
					.push(prompt);
			}
			let body = serde_json::to_string(&script.text).expect("json text");
			let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
			            Cache-Control: no-cache\r\nConnection: close\r\n\r\n";
			let _ = stream.write_all(head.as_bytes());
			let _ = stream.write_all(
				format!(
					"data: {{\"id\":\"chatcmpl-test\",\"object\":\"chat.completion.chunk\",\
					 \"choices\":[{{\"index\":0,\"delta\":{{\"content\":{body}}},\
					 \"finish_reason\":null}}]}}\n\n"
				)
				.as_bytes(),
			);
			let _ = stream.flush();
			if let Some(pause) = script.pause {
				let _ = pause.recv_timeout(Duration::from_secs(5));
			}
			let _ = stream.write_all(b"data: [DONE]\n\n");
			let _ = stream.flush();
		}
	});
	(port, prompts)
}

/// Text of the last `user` message in an OpenAI-compatible request body.
fn user_prompt(request: &[u8]) -> Option<String> {
	let body_start = request.windows(4).position(|window| window == b"\r\n\r\n")? + 4;
	let value: Value = serde_json::from_slice(&request[body_start..]).ok()?;
	let messages = value.get("messages")?.as_array()?;
	let last = messages
		.iter()
		.rev()
		.find(|message| message.get("role").and_then(Value::as_str) == Some("user"))?;
	let content = last.get("content")?;
	if let Some(text) = content.as_str() {
		return Some(text.to_string());
	}
	Some(
		content
			.as_array()?
			.iter()
			.filter_map(|part| part.get("text").and_then(Value::as_str))
			.collect::<Vec<_>>()
			.join("\n"),
	)
}

/// Read headers and body so the client never sees a reset mid-request.
fn read_request(stream: &mut TcpStream) -> Option<Vec<u8>> {
	let mut buffer = Vec::new();
	let mut chunk = [0u8; 4096];
	loop {
		let read = stream.read(&mut chunk).ok()?;
		if read == 0 {
			return None;
		}
		buffer.extend_from_slice(&chunk[..read]);
		let Some(head_end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") else {
			if buffer.len() > 64 * 1024 {
				return None;
			}
			continue;
		};
		let head = String::from_utf8_lossy(&buffer[..head_end]).to_ascii_lowercase();
		let content_length = head
			.lines()
			.find_map(|line| line.strip_prefix("content-length:"))
			.and_then(|value| value.trim().parse::<usize>().ok())
			.unwrap_or(0);
		let body_start = head_end + 4;
		while buffer.len() < body_start + content_length {
			let read = stream.read(&mut chunk).ok()?;
			if read == 0 {
				break;
			}
			buffer.extend_from_slice(&chunk[..read]);
		}
		return Some(buffer);
	}
}

fn launch(port: u16, model: &str, provider: &str) -> RpcLaunch {
	let mut config = Config::default();
	config.providers.insert(
		provider.to_string(),
		ProviderConfig {
			base_url: Some(format!("http://127.0.0.1:{port}/v1")),
			api_key:  Some("sk-test".to_string()),
			..Default::default()
		},
	);
	let cwd = std::env::temp_dir();
	RpcLaunch {
		runner: Arc::new(PiAgentRunner::new().expect("runner")),
		config,
		cwd: cwd.clone(),
		prompts: Prompts::default(),
		skills: Vec::new(),
		catalog: Arc::new(Catalog::default()),
		approval_mode: ApprovalMode::Yolo,
		allow: Vec::new(),
		model: Some(model.to_string()),
		provider: Some(provider.to_string()),
		api_key: Some("sk-test".to_string()),
		thinking: None,
		max_time: Some(Duration::from_secs(10)),
		max_turns: 2,
		tools_enabled: false,
		prompt_cache_key: None,
		provider_session_id: None,
		store_options: SessionOptions {
			enabled: false,
			cwd,
			model: None,
			provider: None,
			dir: None,
			id: None,
			continue_latest: false,
			parent_session: None,
			no_title: true,
		},
		steer: None,
	}
}

fn timeout() -> Duration {
	Duration::from_secs(10)
}

#[test]
fn prompt_streams_events_and_agent_end_matches_messages() {
	let port = spawn_model(vec![StreamScript { text: "hello".to_string(), pause: None }]);
	let harness = Harness::start(launch(port, "local/mock-model", "local"));
	harness.send(json!({ "type": "prompt", "id": "p1", "message": "hi" }));
	let frames = harness.wait_frame("agent_end", timeout());

	let index = |kind: &str| {
		frames
			.iter()
			.position(|frame| frame["type"] == kind)
			.unwrap_or_else(|| panic!("missing {kind}: {frames:#?}"))
	};
	assert!(index("turn_start") < index("message_start"));
	assert!(index("message_start") < index("message_update"));
	assert!(index("message_update") < index("message_end"));
	assert!(index("message_end") < index("turn_end"));
	assert!(index("turn_end") < index("agent_end"));
	assert_eq!(frames[0]["type"], "ready");
	assert!(index("agent_start") < index("turn_start"));

	let message_end = &frames[index("message_end")];
	assert_eq!(message_end["message"]["stopReason"], "stop");
	let update = &frames[index("message_update")];
	assert_eq!(update["assistantMessageEvent"]["delta"], "hello");

	let agent_end = &frames[index("agent_end")];
	assert_eq!(agent_end["willContinue"], false);
	let messages = agent_end["messages"].as_array().expect("messages").clone();
	assert_eq!(messages.len(), 2);
	assert_eq!(messages[0]["role"], "user");

	let prompt_response = frames
		.iter()
		.find(|frame| frame["id"] == "p1")
		.expect("prompt response");
	assert_eq!(prompt_response["success"], true);
	assert_eq!(prompt_response["command"], "prompt");

	harness.send(json!({ "type": "get_messages", "id": "m1" }));
	let frames = harness.wait_for(
		|frames| frames.iter().any(|frame| frame["id"] == "m1"),
		timeout(),
	);
	let response = frames.iter().find(|frame| frame["id"] == "m1").expect("get_messages");
	assert_eq!(response["data"]["messages"], Value::Array(messages));
}

#[test]
fn abort_mid_stream_marks_message_aborted_and_clears_streaming() {
	let (pause_tx, pause_rx) = mpsc::channel();
	let port = spawn_model(vec![StreamScript {
		text:  "partial".to_string(),
		pause: Some(pause_rx),
	}]);
	let harness = Harness::start(launch(port, "local/mock-model", "local"));
	harness.send(json!({ "type": "prompt", "id": "p1", "message": "hi" }));
	harness.wait_frame("message_update", timeout());
	harness.send(json!({ "type": "abort", "id": "a1" }));
	let frames = harness.wait_frame("agent_end", timeout());
	let aborted = frames
		.iter()
		.find(|frame| {
			frame["type"] == "message_end" && frame["message"]["stopReason"] == "aborted"
		})
		.unwrap_or_else(|| panic!("no aborted message_end: {frames:#?}"));
	assert_eq!(aborted["message"]["role"], "assistant");

	harness.send(json!({ "type": "get_state", "id": "s1" }));
	let frames = harness.wait_for(
		|frames| frames.iter().any(|frame| frame["id"] == "s1"),
		timeout(),
	);
	let state = frames.iter().find(|frame| frame["id"] == "s1").expect("state");
	assert_eq!(state["data"]["isStreaming"], false);
	drop(pause_tx);
}

#[test]
fn follow_up_queued_while_streaming_runs_in_the_same_agent_run() {
	let (pause_tx, pause_rx) = mpsc::channel();
	let port = spawn_model(vec![
		StreamScript { text: "first".to_string(), pause: Some(pause_rx) },
		StreamScript { text: "second".to_string(), pause: None },
	]);
	let harness = Harness::start(launch(port, "local/mock-model", "local"));
	harness.send(json!({ "type": "prompt", "id": "p1", "message": "one" }));
	harness.wait_frame("message_update", timeout());
	harness.send(json!({ "type": "follow_up", "id": "f1", "message": "two" }));
	pause_tx.send(()).expect("release first response");
	let frames = harness.wait_frame("agent_end", timeout());

	let message_ends: Vec<&Value> = frames
		.iter()
		.filter(|frame| frame["type"] == "message_end")
		.collect();
	assert_eq!(message_ends.len(), 2, "{frames:#?}");
	assert_eq!(message_ends[0]["message"]["stopReason"], "stop");
	assert_eq!(message_ends[1]["message"]["stopReason"], "stop");
	let updates: Vec<String> = frames
		.iter()
		.filter(|frame| frame["type"] == "message_update")
		.map(|frame| frame["assistantMessageEvent"]["delta"].as_str().unwrap_or("").to_string())
		.collect();
	assert!(updates.iter().any(|delta| delta == "first"));
	assert!(updates.iter().any(|delta| delta == "second"));
	assert_eq!(
		frames.iter().filter(|frame| frame["type"] == "agent_start").count(),
		1
	);
	assert_eq!(
		frames.iter().filter(|frame| frame["type"] == "agent_end").count(),
		1
	);
	let follow_up = frames.iter().find(|frame| frame["id"] == "f1").expect("follow_up response");
	assert_eq!(follow_up["success"], true);
}

#[test]
fn abort_and_prompt_while_streaming_runs_the_replacement_as_a_new_turn() {
	let (pause_tx, pause_rx) = mpsc::channel();
	let port = spawn_model(vec![
		StreamScript { text: "old".to_string(), pause: Some(pause_rx) },
		StreamScript { text: "replacement".to_string(), pause: None },
	]);
	let harness = Harness::start(launch(port, "local/mock-model", "local"));
	harness.send(json!({ "type": "prompt", "id": "p1", "message": "one" }));
	harness.wait_frame("message_update", timeout());
	harness.send(json!({ "type": "abort_and_prompt", "id": "ap1", "message": "two" }));
	// The abort is applied before the release, so the paused script cannot win
	// the race by completing the old turn with `stop`.
	wait_aborted(&harness);
	pause_tx.send(()).expect("release first response");
	let frames = harness.wait_frame("agent_end", timeout());

	let response = frames
		.iter()
		.find(|frame| frame["id"] == "ap1")
		.expect("abort_and_prompt response");
	assert_eq!(response["success"], true);
	assert_eq!(response["command"], "abort_and_prompt");

	// The aborted turn is journaled as `aborted`, and the replacement runs as a
	// real new turn: its text can only appear when a second provider request was
	// actually made (the mock serves the scripts in order).
	let message_ends: Vec<&Value> = frames
		.iter()
		.filter(|frame| frame["type"] == "message_end")
		.collect();
	assert_eq!(message_ends.len(), 2, "{frames:#?}");
	assert_eq!(message_ends[0]["message"]["stopReason"], "aborted");
	assert_eq!(message_ends[1]["message"]["stopReason"], "stop");
	let deltas: Vec<String> = frames
		.iter()
		.filter(|frame| frame["type"] == "message_update")
		.map(|frame| frame["assistantMessageEvent"]["delta"].as_str().unwrap_or("").to_string())
		.collect();
	assert!(deltas.iter().any(|delta| delta == "replacement"), "{deltas:?}");
	// Same agent run: one start, one end, no extra run for the replacement.
	assert_eq!(
		frames.iter().filter(|frame| frame["type"] == "agent_start").count(),
		1
	);
	assert_eq!(
		frames.iter().filter(|frame| frame["type"] == "agent_end").count(),
		1
	);
}

#[test]
fn abort_and_prompt_discards_the_queued_follow_ups_of_the_aborted_run() {
	let (pause_tx, pause_rx) = mpsc::channel();
	// The mock answers in request order, so a script named after the discarded
	// follow-up could never distinguish the cases: with a correct run only two
	// requests exist, and the second would answer with that script's text whatever
	// prompt it carried. The scripts therefore name the expected run (aborted
	// `old`, then the replacement) and the prompt itself is asserted below.
	let (port, prompts) = spawn_model_recording(vec![
		StreamScript { text: "old".to_string(), pause: Some(pause_rx) },
		StreamScript { text: "replacement".to_string(), pause: None },
	]);
	let harness = Harness::start(launch(port, "local/mock-model", "local"));
	harness.send(json!({ "type": "prompt", "id": "p1", "message": "one" }));
	harness.wait_frame("message_update", timeout());
	harness.send(json!({ "type": "follow_up", "id": "f1", "message": "stale" }));
	harness.send(json!({ "type": "abort_and_prompt", "id": "ap1", "message": "two" }));
	// Same barrier as the replacement test: the aborted turn must be journaled
	// before the paused script is released, or `stops` below is a coin flip.
	wait_aborted(&harness);
	pause_tx.send(()).expect("release first response");
	let frames = harness.wait_frame("agent_end", timeout());

	let deltas: Vec<String> = frames
		.iter()
		.filter(|frame| frame["type"] == "message_update")
		.map(|frame| frame["assistantMessageEvent"]["delta"].as_str().unwrap_or("").to_string())
		.collect();
	assert!(deltas.iter().any(|delta| delta == "replacement"), "{deltas:?}");
	// The queued follow-up belonged to the aborted run, so the provider saw exactly
	// two prompts and the second one is the replacement — the assertion the scripted
	// answer text cannot make.
	let prompts = prompts
		.lock()
		.unwrap_or_else(std::sync::PoisonError::into_inner)
		.clone();
	assert_eq!(prompts.len(), 2, "{prompts:?}");
	assert!(prompts[0].contains("one"), "{prompts:?}");
	assert!(
		prompts[1].contains("two"),
		"the replacement prompt reached the provider: {prompts:?}"
	);
	assert!(
		!prompts[1].contains("stale"),
		"the aborted run's queued follow-up was sent to the provider: {prompts:?}"
	);

	let stops: Vec<String> = frames
		.iter()
		.filter(|frame| frame["type"] == "message_end")
		.map(|frame| frame["message"]["stopReason"].as_str().unwrap_or("").to_string())
		.collect();
	assert_eq!(stops, vec!["aborted".to_string(), "stop".to_string()], "{frames:#?}");
}

#[test]
fn plain_abort_at_the_boundary_discards_the_replacement() {
	let (pause_tx, pause_rx) = mpsc::channel();
	let port = spawn_model(vec![
		StreamScript { text: "old".to_string(), pause: Some(pause_rx) },
		StreamScript { text: "replacement".to_string(), pause: None },
	]);
	let harness = Harness::start(launch(port, "local/mock-model", "local"));
	harness.send(json!({ "type": "prompt", "id": "p1", "message": "one" }));
	harness.wait_frame("message_update", timeout());
	harness.send(json!({ "type": "abort_and_prompt", "id": "ap1", "message": "two" }));
	// A plain abort supersedes the replacement. Whether it lands before or after
	// the worker takes it, the replacement must never complete: before the
	// provider call the run state stops it, after it the abort cancels its token.
	harness.send(json!({ "type": "abort", "id": "a1" }));
	let frames = harness.wait_frame("agent_end", timeout());
	drop(pause_tx);

	let completed_replacement = frames.iter().any(|frame| {
		frame["type"] == "message_end"
			&& frame["message"]["stopReason"] == "stop"
			&& frame["message"]["content"].to_string().contains("replacement")
	});
	assert!(!completed_replacement, "a plain abort let the replacement complete: {frames:#?}");
}

#[test]
fn eof_after_abort_and_prompt_never_completes_the_replacement() {
	let (pause_tx, pause_rx) = mpsc::channel();
	let port = spawn_model(vec![
		StreamScript { text: "old".to_string(), pause: Some(pause_rx) },
		StreamScript { text: "replacement".to_string(), pause: None },
	]);
	let harness = Harness::start(launch(port, "local/mock-model", "local"));
	harness.send(json!({ "type": "prompt", "id": "p1", "message": "one" }));
	harness.wait_frame("message_update", timeout());
	harness.send(json!({ "type": "abort_and_prompt", "id": "ap1", "message": "two" }));
	// EOF while the aborted turn is still draining. Whether the worker reaches
	// the replacement before EOF or after is a genuine race in both upstream and
	// here; what must hold in every interleaving is that shutdown never lets the
	// replacement finish.
	harness.close_stdin();
	let frames = harness.wait_frame("agent_end", timeout());
	let result = harness.join();
	assert!(result.is_ok(), "rpc run must exit cleanly: {result:?}");
	drop(pause_tx);

	let completed_replacement = frames.iter().any(|frame| {
		frame["type"] == "message_end"
			&& frame["message"]["stopReason"] == "stop"
			&& frame["message"]["content"]
				.to_string()
				.contains("replacement")
	});
	assert!(!completed_replacement, "EOF let the replacement complete: {frames:#?}");
	assert_eq!(
		frames.iter().filter(|frame| frame["type"] == "agent_start").count(),
		1
	);
}

#[test]
fn eof_drains_the_streaming_turn_and_exits_clean() {
	let (_pause_tx, pause_rx) = mpsc::channel();
	let port = spawn_model(vec![StreamScript {
		text:  "bye".to_string(),
		pause: Some(pause_rx),
	}]);
	let harness = Harness::start(launch(port, "local/mock-model", "local"));
	harness.send(json!({ "type": "prompt", "id": "p1", "message": "hi" }));
	harness.wait_frame("message_update", timeout());
	// EOF while the turn is still streaming: `run` must abort, drain and return.
	let result = harness.join();
	assert!(result.is_ok(), "rpc run must exit cleanly: {result:?}");
}

#[test]
fn abort_bash_cancels_the_running_command() {
	let port = spawn_model(Vec::new());
	let harness = Harness::start(launch(port, "local/mock-model", "local"));
	harness.send(json!({
		"type": "bash",
		"id": "b1",
		"command": "sleep 30; echo never",
	}));
	harness.send(json!({ "type": "abort_bash", "id": "b2" }));
	let frames = harness.wait_for(
		|frames| {
			frames.iter().any(|frame| {
				frame["id"] == "b1" && frame["data"]["cancelled"] == true
			})
		},
		timeout(),
	);
	let bash = frames.iter().find(|frame| frame["id"] == "b1").expect("bash result");
	assert_eq!(bash["success"], true);
	assert_eq!(bash["data"]["cancelled"], true);
	assert!(
		!bash["data"]["output"].as_str().unwrap_or("").contains("never"),
		"the command must not finish after abort"
	);
}

#[test]
fn fast_mode_accepts_both_directions_where_the_provider_supports_it() {
	let port = spawn_model(Vec::new());
	let harness = Harness::start(launch(port, "openai/gpt-5", "openai"));
	harness.send(json!({ "type": "set_fast_mode", "id": "f1", "enabled": true }));
	let frames = harness.wait_for(
		|frames| frames.iter().any(|frame| frame["id"] == "f1"),
		timeout(),
	);
	let response = frames.iter().find(|frame| frame["id"] == "f1").expect("fast on");
	assert_eq!(response["success"], true);
	assert_eq!(response["data"]["enabled"], true);
	assert_eq!(response["data"]["active"], true);

	harness.send(json!({ "type": "set_fast_mode", "id": "f2", "enabled": false }));
	let frames = harness.wait_for(
		|frames| frames.iter().any(|frame| frame["id"] == "f2"),
		timeout(),
	);
	let response = frames.iter().find(|frame| frame["id"] == "f2").expect("fast off");
	assert_eq!(response["success"], true);
	assert_eq!(response["data"]["enabled"], false);
	assert_eq!(response["data"]["active"], false);
}

#[test]
fn fast_mode_true_is_rejected_for_models_without_a_tier_knob() {
	let port = spawn_model(Vec::new());
	let harness = Harness::start(launch(port, "local/mock-model", "local"));
	harness.send(json!({ "type": "set_fast_mode", "id": "f1", "enabled": true }));
	let frames = harness.wait_for(
		|frames| frames.iter().any(|frame| frame["id"] == "f1"),
		timeout(),
	);
	let response = frames.iter().find(|frame| frame["id"] == "f1").expect("fast");
	assert_eq!(response["success"], false);
	assert_eq!(response["error"], "Fast mode is unavailable for the current model.");
}
