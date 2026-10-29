//! Consumer boundaries for the OpenAI-compatible provider:
//! - streamed tool-call assembly across duplicate/out-of-order indices and
//!   missing ids or names;
//! - SSE framing termination on `[DONE]`, EOF tails, transport errors and
//!   malformed payloads;
//! - HTTP error-body bounds, protocol failures and forced `tool_choice`
//!   selection that must fail before any request leaves the client.
//!
//! The wire shape of a valid forced choice and the live loop escalation are
//! covered in `agent_core.rs`; this file only adds the remaining boundaries.

use std::time::Duration;

use futures::StreamExt;
use pi_agent::{
	Message, ModelProvider, OpenAiCompatibleProvider, ProviderError, StreamAssembler, StreamChunk,
	ToolCallDelta, ToolChoice, parse_sse_line, sse_stream,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn delta(
	index: usize,
	id: Option<&str>,
	name: Option<&str>,
	arguments_delta: Option<&str>,
) -> ToolCallDelta {
	ToolCallDelta {
		index,
		id: id.map(str::to_string),
		name: name.map(str::to_string),
		arguments_delta: arguments_delta.map(str::to_string),
	}
}

fn chunk(content: Option<&str>, deltas: Vec<ToolCallDelta>) -> StreamChunk {
	StreamChunk {
		content: content.map(str::to_string),
		tool_call_deltas: deltas,
		usage: None,
		is_done: false,
	}
}

async fn read_request(socket: &mut TcpStream) -> Option<(String, Value)> {
	let mut buf = Vec::new();
	let mut header_end = None;
	let mut tmp = [0u8; 1024];
	let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
	while header_end.is_none() && tokio::time::Instant::now() < deadline {
		let n = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut tmp))
			.await
			.ok()?
			.ok()?;
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
	let body_start = header_end + 4;
	let mut body = buf[body_start..].to_vec();
	while body.len() < content_length {
		let n = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut tmp))
			.await
			.ok()?
			.ok()?;
		if n == 0 {
			break;
		}
		body.extend_from_slice(&tmp[..n]);
	}
	Some((headers, serde_json::from_slice(&body).unwrap_or(Value::Null)))
}

fn response(status: &str, content_type: &str, body: &str) -> Vec<u8> {
	format!(
		"HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
		body.len()
	)
	.into_bytes()
}

async fn serve_responses(responses: Vec<Vec<u8>>) -> String {
	let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
	let addr = listener.local_addr().expect("addr");
	tokio::spawn(async move {
		for response in responses {
			let Ok((mut socket, _)) = listener.accept().await else {
				return;
			};
			let _ = read_request(&mut socket).await;
			let _ = socket.write_all(&response).await;
			let _ = socket.flush().await;
		}
	});
	format!("http://{addr}")
}

#[test]
fn assembler_merges_duplicate_indices_and_sorts_out_of_order_calls() {
	let mut assembler = StreamAssembler::new();
	assembler.push(&chunk(
		None,
		vec![delta(2, None, Some("third"), Some("{}"))],
	));
	assembler.push(&chunk(
		Some("A"),
		vec![delta(0, Some("call_a"), Some("read"), Some("{\"path\":\"a"))],
	));
	assembler.push(&chunk(
		Some("B"),
		vec![delta(0, Some("call_a2"), None, Some(".txt\"}"))],
	));
	assembler.push(&chunk(
		Some("C"),
		vec![delta(1, Some("call_b"), Some("bash"), Some("echo hi"))],
	));

	let message = assembler.finish().expect("assembled turn");
	assert_eq!(message.content, "ABC");
	assert_eq!(message.tool_calls.len(), 3);

	// Index 0 was emitted twice: one call, later id wins, arguments joined.
	assert_eq!(message.tool_calls[0].id, "call_a2");
	assert_eq!(message.tool_calls[0].name, "read");
	assert_eq!(message.tool_calls[0].arguments, json!({ "path": "a.txt" }));

	// Non-JSON arguments stay verbatim instead of being dropped.
	assert_eq!(message.tool_calls[1].id, "call_b");
	assert_eq!(message.tool_calls[1].arguments, json!("echo hi"));

	// Missing ids fall back to the index; empty arguments become an object.
	assert_eq!(message.tool_calls[2].id, "call_2");
	assert_eq!(message.tool_calls[2].name, "third");
	assert_eq!(message.tool_calls[2].arguments, json!({}));
}

#[tokio::test]
async fn sse_stream_stops_at_done_and_flushes_a_tail_without_newline() {
	let done_body = futures::stream::iter(vec![
		Ok::<_, String>(b"data: {\"choices\":[{\"delta\":{\"content\":\"first\"}}]}\n\n".to_vec()),
		Ok(b"data: [DONE]\n\n".to_vec()),
		Err("body polled after [DONE]".to_string()),
	]);
	let mut stream = sse_stream(done_body);
	assert_eq!(stream.next().await.expect("first").expect("ok").content.as_deref(), Some("first"));
	assert!(stream.next().await.expect("done").expect("ok").is_done);
	assert!(stream.next().await.is_none(), "the body must not be polled after [DONE]");

	let tail_body = futures::stream::iter(vec![Ok::<_, String>(
		b"data: {\"choices\":[{\"delta\":{\"content\":\"tail\"}}]}".to_vec(),
	)]);
	let mut stream = sse_stream(tail_body);
	assert_eq!(stream.next().await.expect("tail").expect("ok").content.as_deref(), Some("tail"));
	assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn sse_stream_surfaces_transport_and_serialization_errors_then_stops() {
	let transport_body = futures::stream::iter(vec![
		Ok::<_, String>(b"data: {\"choices\":[{\"delta\":{\"content\":\"before\"}}]}\n\n".to_vec()),
		Err("socket reset".to_string()),
		Ok(b"data: {\"choices\":[{\"delta\":{\"content\":\"after\"}}]}\n\n".to_vec()),
	]);
	let mut stream = sse_stream(transport_body);
	assert_eq!(stream.next().await.expect("chunk").expect("ok").content.as_deref(), Some("before"));
	let err = stream.next().await.expect("error").expect_err("transport error");
	match err {
		ProviderError::Network(message) => assert!(message.contains("socket reset"), "{message}"),
		other => panic!("expected network error, got {other:?}"),
	}
	assert!(stream.next().await.is_none(), "stream must terminate after a transport error");

	let malformed_body = futures::stream::iter(vec![
		Ok::<_, String>(b"data: {not json}\n\n".to_vec()),
		Ok(b"data: {\"choices\":[{\"delta\":{\"content\":\"after\"}}]}\n\n".to_vec()),
	]);
	let mut stream = sse_stream(malformed_body);
	assert!(matches!(
		stream.next().await.expect("error").expect_err("malformed"),
		ProviderError::Serialization(_)
	));
	assert!(stream.next().await.is_none(), "stream must terminate after a parse error");
}

#[tokio::test]
async fn http_stream_error_body_is_bounded_and_status_surfaces() {
	let long_body = "x".repeat(5_000);
	let url = serve_responses(vec![response("429 Too Many Requests", "text/plain", &long_body)]).await;
	let provider = OpenAiCompatibleProvider::new(url, "key", "model").expect("provider");

	let err = match provider.stream(&[Message::user("hi")], &[], &ToolChoice::Auto).await {
		Err(err) => err,
		Ok(_) => panic!("429 must fail before a stream is returned"),
	};
	match err {
		ProviderError::HttpStatus(429, body) => {
			assert_eq!(body.chars().count(), 2048, "error body must be capped");
		},
		other => panic!("expected HttpStatus(429), got {other:?}"),
	}
}

#[tokio::test]
async fn complete_reports_empty_choices_and_invalid_json_as_protocol_errors() {
	let url = serve_responses(vec![
		response("200 OK", "application/json", r#"{"choices":[]}"#),
		response("200 OK", "application/json", r#"{"choices":"not-an-array"}"#),
	])
	.await;
	let provider = OpenAiCompatibleProvider::new(url, "key", "model").expect("provider");

	let empty = provider
		.complete(&[Message::user("hi")], &[], &ToolChoice::Auto)
		.await
		.expect_err("empty choices");
	match empty {
		ProviderError::Protocol(message) => assert!(message.contains("no choices"), "{message}"),
		other => panic!("expected protocol error, got {other:?}"),
	}

	let invalid = provider
		.complete(&[Message::user("hi")], &[], &ToolChoice::Auto)
		.await
		.expect_err("invalid body");
	match invalid {
		ProviderError::Protocol(message) => {
			assert!(message.contains("invalid chat completion body"), "{message}");
			assert!(message.contains(r#"{"choices":"not-an-array"}"#), "{message}");
		},
		other => panic!("expected protocol error, got {other:?}"),
	}
}

#[tokio::test]
async fn forced_tool_choice_requires_an_offered_function_shaped_tool() {
	let provider = OpenAiCompatibleProvider::new("http://127.0.0.1:9", "key", "model")
		.expect("provider");
	// Same name, but neither entry is a function tool: a forced selection must
	// not treat a lookalike as offered.
	let tools = vec![
		json!({ "type": "other", "function": { "name": "write" } }),
		json!({ "type": "function", "name": "write" }),
	];
	let choice = ToolChoice::Tool { name: "write".to_string() };
	let err = provider
		.complete(&[Message::user("hi")], &tools, &choice)
		.await
		.expect_err("lookalike tool must be rejected");
	match err {
		ProviderError::InvalidToolChoice(message) => {
			assert!(message.contains("not offered"), "{message}");
		},
		other => panic!("expected InvalidToolChoice, got {other:?}"),
	}
}

#[tokio::test]
async fn dropping_a_live_stream_closes_the_transport() {
	let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
	let addr = listener.local_addr().expect("addr");
	let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
	tokio::spawn(async move {
		let (mut socket, _) = listener.accept().await.expect("accept");
		let _ = read_request(&mut socket).await;
		let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";
		let _ = socket.write_all(headers.as_bytes()).await;
		let event = "data: {\"choices\":[{\"delta\":{\"content\":\"first\"}}]}\n\n";
		let chunk = format!("{:x}\r\n{event}\r\n", event.len());
		let _ = socket.write_all(chunk.as_bytes()).await;
		let _ = socket.flush().await;
		// The client must close this connection when it drops the stream.
		let mut tmp = [0u8; 64];
		let _ = socket.read(&mut tmp).await;
		let _ = closed_tx.send(());
	});

	let provider =
		OpenAiCompatibleProvider::new(format!("http://{addr}"), "key", "model").expect("provider");
	let mut stream = provider
		.stream(&[Message::user("hi")], &[], &ToolChoice::Auto)
		.await
		.expect("stream");
	let first = stream.next().await.expect("chunk").expect("ok");
	assert_eq!(first.content.as_deref(), Some("first"));
	drop(stream);

	tokio::time::timeout(Duration::from_secs(2), closed_rx)
		.await
		.expect("server must observe the closed stream")
		.expect("closed signal");
}

#[test]
fn sse_line_parser_ignores_non_data_fields() {
	assert!(parse_sse_line("event: message").is_none());
	assert!(parse_sse_line("id: 1").is_none());
	assert!(parse_sse_line(": keep-alive").is_none());
}

async fn serve_capturing(
	response: Vec<u8>,
) -> (String, tokio::sync::oneshot::Receiver<(String, Value)>) {
	let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
	let addr = listener.local_addr().expect("addr");
	let (tx, rx) = tokio::sync::oneshot::channel();
	tokio::spawn(async move {
		let Ok((mut socket, _)) = listener.accept().await else {
			return;
		};
		let Some((headers, body)) = read_request(&mut socket).await else {
			return;
		};
		let _ = socket.write_all(&response).await;
		let _ = socket.flush().await;
		let _ = tx.send((headers, body));
	});
	(format!("http://{addr}"), rx)
}

#[tokio::test]
async fn forced_tool_choice_serializes_openai_function_shape_on_the_wire() {
	let (url, captured) = serve_capturing(response(
		"200 OK",
		"text/event-stream",
		"data: [DONE]\n\n",
	))
	.await;
	let provider =
		OpenAiCompatibleProvider::new(url, "sk-wire", "model").expect("provider");
	let tools = vec![
		json!({ "type": "function", "function": { "name": "read", "parameters": { "type": "object" } } }),
	];
	let mut stream = provider
		.stream(
			&[Message::user("hi")],
			&tools,
			&ToolChoice::Tool { name: "read".to_string() },
		)
		.await
		.expect("forced stream");
	while let Some(item) = stream.next().await {
		item.expect("stream chunk");
	}

	let (headers, body) = captured.await.expect("captured request");
	assert_eq!(
		body["tool_choice"],
		json!({ "type": "function", "function": { "name": "read" } }),
		"forced selection must use the OpenAI function shape"
	);
	assert_eq!(body["tools"], json!(tools));
	assert!(
		headers.to_ascii_lowercase().contains("authorization: bearer sk-wire"),
		"key must travel in the request header only: {headers}"
	);
}

#[tokio::test]
async fn provider_errors_never_echo_the_api_key() {
	let url = serve_responses(vec![response(
		"500 Internal Server Error",
		"text/plain",
		"upstream unavailable",
	)])
	.await;
	let provider = OpenAiCompatibleProvider::new(url, "sk-secret-boundary", "model")
		.expect("provider");
	let err = provider
		.complete(&[Message::user("hi")], &[], &ToolChoice::Auto)
		.await
		.expect_err("500 must fail");
	let rendered = format!("{err} {err:?}");
	assert!(
		!rendered.contains("sk-secret-boundary"),
		"API key must not appear in provider errors: {rendered}"
	);
	assert!(matches!(err, ProviderError::HttpStatus(500, _)), "{err:?}");
}
