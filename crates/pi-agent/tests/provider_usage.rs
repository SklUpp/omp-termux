//! Provider-reported token usage end to end at the provider/loop boundary:
//! - a real `stream_options.include_usage` trailer (empty `choices`) reaches
//!   the finalized assistant message;
//! - OpenAI/DeepSeek/Vertex cache-read spellings normalize into disjoint
//!   counters without double counting;
//! - repeated usage snapshots replace, never add;
//! - a host that reports nothing leaves `usage: None` instead of zeros;
//! - a request that fails after reporting usage keeps that usage exactly once.

use std::sync::{
	Arc,
	atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use futures::{StreamExt, stream};
use pi_agent::{
	AgentConfig, AgentEvent, AgentLoop, AgentState, ChunkStream, Message, ModelProvider,
	OpenAiCompatibleProvider, OpenAiUsage, ProviderError, StreamAssembler, StreamChunk, TokenUsage,
	ToolChoice, ToolRegistry, TurnStatus, normalize_usage, parse_sse_line,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> TokenUsage {
	TokenUsage {
		input,
		output,
		cache_read,
		cache_write,
		total_tokens: input + output + cache_read + cache_write,
		reasoning_tokens: None,
	}
}

fn sse_response(events: &[&str]) -> Vec<u8> {
	let body = events
		.iter()
		.map(|event| format!("data: {event}\n\n"))
		.collect::<String>();
	format!(
		"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
		body.len()
	)
	.into_bytes()
}

fn json_response(body: &str) -> Vec<u8> {
	format!(
		"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
		body.len()
	)
	.into_bytes()
}

async fn read_request(socket: &mut TcpStream) -> Option<Value> {
	let mut buf = Vec::new();
	let mut header_end = None;
	let mut tmp = [0u8; 1024];
	while header_end.is_none() {
		let n = socket.read(&mut tmp).await.ok()?;
		if n == 0 {
			return None;
		}
		buf.extend_from_slice(&tmp[..n]);
		header_end = buf.windows(4).position(|w| w == b"\r\n\r\n");
	}
	let header_end = header_end?;
	let headers = String::from_utf8_lossy(&buf[..header_end]).into_owned();
	let content_length = headers
		.lines()
		.find_map(|line| {
			line.to_ascii_lowercase()
				.strip_prefix("content-length:")
				.map(|value| value.trim().parse::<usize>().unwrap_or(0))
		})
		.unwrap_or(0);
	let mut body = buf[header_end + 4..].to_vec();
	while body.len() < content_length {
		let n = socket.read(&mut tmp).await.ok()?;
		if n == 0 {
			break;
		}
		body.extend_from_slice(&tmp[..n]);
	}
	serde_json::from_slice(&body).ok()
}

/// Serve one response per request and hand every request body back.
async fn serve(responses: Vec<Vec<u8>>) -> (String, tokio::sync::mpsc::Receiver<Value>) {
	let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
	let addr = listener.local_addr().expect("addr");
	let (tx, rx) = tokio::sync::mpsc::channel(responses.len().max(1));
	tokio::spawn(async move {
		for response in responses {
			let Ok((mut socket, _)) = listener.accept().await else {
				return;
			};
			let body = read_request(&mut socket).await.unwrap_or(Value::Null);
			let _ = tx.send(body).await;
			let _ = socket.write_all(&response).await;
			let _ = socket.flush().await;
		}
	});
	(format!("http://{addr}"), rx)
}

fn drained_calls(rx: &mut tokio::sync::mpsc::Receiver<Value>) -> Value {
	rx.try_recv().expect("captured request")
}

#[tokio::test]
async fn streamed_usage_only_trailer_attaches_the_request_snapshot() {
	let (url, mut calls) = serve(vec![sse_response(&[
		r#"{"choices":[{"delta":{"content":"hi"}}]}"#,
		r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
		r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,"prompt_tokens_details":{"cached_tokens":40}}}"#,
		"[DONE]",
	])])
	.await;
	let provider = OpenAiCompatibleProvider::new(url, "sk-test", "model").expect("provider");
	let mut chunks = provider
		.stream(&[Message::user("hi")], &[], &ToolChoice::Auto)
		.await
		.expect("stream");

	let mut assembler = StreamAssembler::new();
	while let Some(chunk) = chunks.next().await {
		assembler.push(&chunk.expect("chunk"));
	}
	let message = assembler.finish().expect("finish");

	assert_eq!(message.content, "hi");
	assert_eq!(
		message.usage,
		Some(usage(60, 20, 40, 0)),
		"input excludes cache read; the trailer's counters land once"
	);
	let request = drained_calls(&mut calls);
	assert_eq!(
		request["stream_options"],
		json!({ "include_usage": true }),
		"streamed requests must ask for the usage trailer"
	);
}

#[tokio::test]
async fn duplicate_usage_snapshots_are_replaced_not_added() {
	let (url, _calls) = serve(vec![sse_response(&[
		r#"{"choices":[{"delta":{"content":"hi"},"usage":{"prompt_tokens":10,"completion_tokens":5}}]}"#,
		r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#,
		"[DONE]",
	])])
	.await;
	let provider = OpenAiCompatibleProvider::new(url, "sk-test", "model").expect("provider");
	let mut chunks = provider
		.stream(&[Message::user("hi")], &[], &ToolChoice::Auto)
		.await
		.expect("stream");

	let mut assembler = StreamAssembler::new();
	while let Some(chunk) = chunks.next().await {
		assembler.push(&chunk.expect("chunk"));
	}

	assert_eq!(
		assembler.finish().expect("finish").usage,
		Some(usage(10, 5, 0, 0)),
		"repeated cumulative snapshots must not double count"
	);
}

#[tokio::test]
async fn two_streamed_requests_report_separate_usage() {
	let (url, _calls) = serve(vec![
		sse_response(&[
			r#"{"choices":[{"delta":{"content":"one"}}]}"#,
			r#"{"choices":[],"usage":{"prompt_tokens":50,"completion_tokens":10}}"#,
			"[DONE]",
		]),
		sse_response(&[
			r#"{"choices":[{"delta":{"content":"two"}}]}"#,
			r#"{"choices":[],"usage":{"prompt_tokens":70,"completion_tokens":30,"prompt_tokens_details":{"cached_tokens":20}}}"#,
			"[DONE]",
		]),
	])
	.await;
	let provider = OpenAiCompatibleProvider::new(url, "sk-test", "model").expect("provider");

	let mut messages = Vec::new();
	for _ in 0..2 {
		let mut chunks = provider
			.stream(&[Message::user("hi")], &[], &ToolChoice::Auto)
			.await
			.expect("stream");
		let mut assembler = StreamAssembler::new();
		while let Some(chunk) = chunks.next().await {
			assembler.push(&chunk.expect("chunk"));
		}
		messages.push(assembler.finish().expect("finish"));
	}

	assert_eq!(messages[0].usage, Some(usage(50, 10, 0, 0)));
	assert_eq!(messages[1].usage, Some(usage(50, 30, 20, 0)));
	let input: u64 = messages.iter().filter_map(|m| m.usage).map(|u| u.input).sum();
	let output: u64 = messages.iter().filter_map(|m| m.usage).map(|u| u.output).sum();
	assert_eq!((input, output), (100, 40), "each response is counted once");
}

#[tokio::test]
async fn non_stream_completion_attaches_reported_usage() {
	let (url, mut calls) = serve(vec![json_response(
		r#"{"choices":[{"message":{"content":"ok"}}],"usage":{"prompt_tokens":50,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":10}}}"#,
	)])
	.await;
	let provider = OpenAiCompatibleProvider::new(url, "sk-test", "model").expect("provider");
	let message = provider
		.complete(&[Message::user("hi")], &[], &ToolChoice::Auto)
		.await
		.expect("complete");

	assert_eq!(message.usage, Some(usage(40, 5, 10, 0)));
	let request = drained_calls(&mut calls);
	assert!(
		request.get("stream_options").is_none(),
		"non-streaming requests carry no stream_options: {request}"
	);
}

#[tokio::test]
async fn missing_usage_stays_unknown() {
	let (url, _calls) = serve(vec![sse_response(&[
		r#"{"choices":[{"delta":{"content":"hi"}}]}"#,
		"[DONE]",
	])])
	.await;
	let provider = OpenAiCompatibleProvider::new(url, "sk-test", "model").expect("provider");
	let mut chunks = provider
		.stream(&[Message::user("hi")], &[], &ToolChoice::Auto)
		.await
		.expect("stream");

	let mut assembler = StreamAssembler::new();
	while let Some(chunk) = chunks.next().await {
		assembler.push(&chunk.expect("chunk"));
	}

	assert_eq!(
		assembler.finish().expect("finish").usage,
		None,
		"an unreported request must stay unknown, not zero-filled"
	);
}

#[tokio::test]
async fn dialect_without_streaming_usage_omits_stream_options() {
	let (url, mut calls) = serve(vec![sse_response(&["[DONE]"])]).await;
	let provider = OpenAiCompatibleProvider::new(url, "sk-test", "model")
		.expect("provider")
		.with_usage_in_streaming(false);
	let mut chunks = provider
		.stream(&[Message::user("hi")], &[], &ToolChoice::Auto)
		.await
		.expect("stream");
	while let Some(chunk) = chunks.next().await {
		chunk.expect("chunk");
	}

	let request = drained_calls(&mut calls);
	assert!(
		request.get("stream_options").is_none(),
		"dialects that reject the field must not receive it: {request}"
	);
}

#[test]
fn cache_read_spellings_and_deepseek_pair_normalize_without_double_count() {
	let openai = normalize_usage(
		&serde_json::from_value::<OpenAiUsage>(json!({
			"prompt_tokens": 100,
			"completion_tokens": 20,
			"prompt_tokens_details": { "cached_tokens": 40 },
		}))
		.expect("openai shape"),
	);
	assert_eq!(openai, usage(60, 20, 40, 0));

	let vertex = normalize_usage(
		&serde_json::from_value::<OpenAiUsage>(json!({
			"prompt_tokens": 100,
			"completion_tokens": 20,
			"cachedContentTokenCount": 25,
		}))
		.expect("vertex shape"),
	);
	assert_eq!(vertex, usage(75, 20, 25, 0));

	// DeepSeek reports the write side as miss tokens, already inside prompt_tokens.
	let deepseek = normalize_usage(
		&serde_json::from_value::<OpenAiUsage>(json!({
			"prompt_tokens": 100,
			"completion_tokens": 5,
			"prompt_cache_hit_tokens": 30,
			"prompt_cache_miss_tokens": 70,
		}))
		.expect("deepseek shape"),
	);
	assert_eq!(deepseek, usage(70, 5, 30, 0));

	// An explicit write counter is charged as cache write, not input.
	let anthropic_style = normalize_usage(
		&serde_json::from_value::<OpenAiUsage>(json!({
			"prompt_tokens": 100,
			"completion_tokens": 5,
			"prompt_tokens_details": { "cached_tokens": 30, "cache_write_tokens": 20 },
		}))
		.expect("write shape"),
	);
	assert_eq!(anthropic_style, usage(50, 5, 30, 20));

	// Reasoning tokens are preserved (subset of output) and never invented.
	let reasoning = normalize_usage(
		&serde_json::from_value::<OpenAiUsage>(json!({
			"prompt_tokens": 100,
			"completion_tokens": 20,
			"completion_tokens_details": { "reasoning_tokens": 7 },
		}))
		.expect("reasoning shape"),
	);
	assert_eq!(reasoning, TokenUsage { reasoning_tokens: Some(7), ..usage(100, 20, 0, 0) });
	let no_reasoning = normalize_usage(
		&serde_json::from_value::<OpenAiUsage>(json!({
			"prompt_tokens": 100,
			"completion_tokens": 20,
			"completion_tokens_details": { "reasoning_tokens": 0 },
		}))
		.expect("zero reasoning shape"),
	);
	assert_eq!(no_reasoning.reasoning_tokens, None, "zero means unreported, not a value");
}

#[test]
fn malformed_usage_counters_never_fail_the_stream() {
	let Some(Ok(chunk)) = parse_sse_line(
		r#"data: {"choices":[],"usage":{"prompt_tokens":"lots","completion_tokens":null}}"#,
	) else {
		panic!("usage payload with unparsable counters must still parse");
	};
	assert_eq!(
		chunk.usage,
		Some(usage(0, 0, 0, 0)),
		"a present-but-empty report is a report of nothing, not a stream error"
	);

	let Some(Ok(chunk)) = parse_sse_line(
		r#"data: {"choices":[{"delta":{},"usage":{"prompt_tokens":9,"completion_tokens":1}}]}"#,
	) else {
		panic!("choice-level usage must parse");
	};
	assert_eq!(chunk.usage, Some(usage(9, 1, 0, 0)));
}

struct FailingAfterUsageProvider {
	usage: TokenUsage,
	stream_calls: AtomicUsize,
}

#[async_trait]
impl ModelProvider for FailingAfterUsageProvider {
	async fn complete(
		&self,
		_messages: &[Message],
		_tools: &[Value],
		_tool_choice: &ToolChoice,
	) -> Result<Message, ProviderError> {
		Err(ProviderError::Protocol(
			"complete() must not run when an event sink is attached".to_string(),
		))
	}

	async fn stream(
		&self,
		_messages: &[Message],
		_tools: &[Value],
		_tool_choice: &ToolChoice,
	) -> Result<ChunkStream, ProviderError> {
		self.stream_calls.fetch_add(1, Ordering::SeqCst);
		let usage = self.usage;
		let items: Vec<Result<StreamChunk, ProviderError>> = vec![
			Ok(StreamChunk {
				content: Some("partial".to_string()),
				tool_call_deltas: Vec::new(),
				usage: Some(usage),
				is_done: false,
			}),
			Err(ProviderError::Network("connection dropped".to_string())),
		];
		let stream: ChunkStream = Box::pin(stream::iter(items));
		Ok(stream)
	}
}

struct TempDir {
	path: std::path::PathBuf,
}

impl TempDir {
	fn new(label: &str) -> Self {
		static NEXT: AtomicUsize = AtomicUsize::new(0);
		let unique = format!(
			"pi-agent-usage-{label}-{}-{}",
			std::process::id(),
			NEXT.fetch_add(1, Ordering::SeqCst)
		);
		let path = std::env::temp_dir().join(unique);
		std::fs::create_dir_all(&path).expect("create temp dir");
		Self { path }
	}
}

impl Drop for TempDir {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.path);
	}
}

fn config(cwd: &std::path::Path) -> AgentConfig {
	AgentConfig {
		max_turns: 2,
		context_limit: 100_000,
		system_prompt: Some("test system prompt".to_string()),
		cwd: cwd.to_path_buf(),
	}
}

/// One report travels on both `MessageEnd` and `TurnEnd` of the same request;
/// consumers account once, and the failed request is not dropped.
#[tokio::test]
async fn failed_request_keeps_its_reported_usage_once() {
	let dir = TempDir::new("failed-usage");
	let provider = Arc::new(FailingAfterUsageProvider {
		usage: usage(30, 4, 6, 0),
		stream_calls: AtomicUsize::new(0),
	});
	let agent_provider: Arc<dyn ModelProvider> = provider.clone();
	let agent = AgentLoop::new(
		config(&dir.path),
		agent_provider,
		ToolRegistry::with_builtins(),
	);
	let mut state = AgentState::default();
	let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(16);

	let result = agent.run_dialog_with("hi", &mut state, None, Some(events_tx)).await;
	assert!(result.is_err(), "the transport error must surface");

	let mut events = Vec::new();
	while let Some(event) = events_rx.recv().await {
		events.push(event);
	}

	let message_ends: Vec<&Message> = events
		.iter()
		.filter_map(|event| match event {
			AgentEvent::MessageEnd { message, status } => {
				assert!(matches!(status, TurnStatus::Error(_)), "status: {status:?}");
				Some(message)
			},
			_ => None,
		})
		.collect();
	assert_eq!(message_ends.len(), 1, "exactly one request was attempted");
	assert_eq!(message_ends[0].usage, Some(usage(30, 4, 6, 0)));

	let turn_ends: Vec<&Message> = events
		.iter()
		.filter_map(|event| match event {
			AgentEvent::TurnEnd { message, status, .. } => {
				assert!(matches!(status, TurnStatus::Error(_)), "status: {status:?}");
				Some(message)
			},
			_ => None,
		})
		.collect();
	assert_eq!(turn_ends.len(), 1);
	assert_eq!(
		turn_ends[0].usage,
		message_ends[0].usage,
		"TurnEnd repeats the same request snapshot; summing both would double count"
	);
	assert_eq!(provider.stream_calls.load(Ordering::SeqCst), 1);
}
