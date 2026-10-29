//! Consumer-level tests for the RPC transport pieces that do not need a live
//! session: frame chunking round-trip, message paging cursors, host URI scheme
//! validation and stdin framing. Session/turn behaviour (streaming frames,
//! abort, EOF cleanup) needs the `RpcLaunch` wiring seam and lives in the
//! follow-up integration test described in `local://rpc-wiring.md`.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use omp_cli::rpc::{
	frame::{RpcFrameDecoder, RpcFrameEncoder, MAX_RPC_FRAME_BYTES},
	host_uris::HostUriBridge,
	input::{Inbound, Output, RpcInput},
	messages::{MessageSnapshot, page_messages},
	session::FrameSink,
	types::RpcHostUriSchemeDefinition,
};

#[test]
fn v2_chunks_round_trip_through_the_decoder() {
	let big = json!({
		"type": "notice",
		"message": "y".repeat(MAX_RPC_FRAME_BYTES + 64),
	});
	let mut encoder = RpcFrameEncoder::new();
	let lines = encoder.encode_frames(&big);
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
	gap["chunk"]["index"] = json!(9);
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
	let bridge = HostUriBridge::new(sink);
	let reserved = RpcHostUriSchemeDefinition {
		scheme:      "file".to_string(),
		description: None,
		writable:    Some(false),
		immutable:   Some(true),
	};
	let error = bridge.set_schemes(&[reserved]).expect_err("file:// is reserved");
	assert!(error.contains("reserved"), "{error}");

	let custom = RpcHostUriSchemeDefinition {
		scheme:      "notes".to_string(),
		description: Some("host notes".to_string()),
		writable:    Some(true),
		immutable:   None,
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
