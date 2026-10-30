//! JSONL transport framing for RPC mode.
//!
//! Protocol v1 sends one logical frame per physical line and enforces a 1 MiB
//! line ceiling by shrinking oversized frames. Protocol v2 (`negotiate_protocol`)
//! chunks a logical frame that still exceeds the ceiling into `rpc_chunk` lines
//! (256 KiB payloads, 64 MiB reassembled ceiling). Frame *counts*, chunk ids and
//! error text mirror `packages/coding-agent/src/modes/rpc/rpc-frame.ts`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use pi_agent::state::is_valid_base64_payload;
use serde_json::{Map, Value, json};

/// Maximum UTF-8 size of one newline-delimited RPC frame, including the newline.
pub const MAX_RPC_FRAME_BYTES: usize = 1024 * 1024;
/// Maximum UTF-8 size of one logical frame reassembled by protocol v2.
pub const MAX_RPC_REASSEMBLED_BYTES: usize = 64 * 1024 * 1024;
/// Raw payload bytes carried by one `rpc_chunk` frame.
pub const RPC_CHUNK_PAYLOAD_BYTES: usize = 256 * 1024;

const STRING_ELISION_RESERVE: usize = 80;
const METADATA_STRING_CAP: usize = 1024;
const MAX_CHUNK_ID_CHARS: usize = 128;

struct ShrinkPass {
	string_cap:   usize,
	array_limit:  usize,
	object_limit: usize,
}

/// Progressive reduction applied to an oversized frame, exactly as upstream.
const SHRINK_PASSES: [ShrinkPass; 7] = [
	ShrinkPass { string_cap: 256 * 1024, array_limit: 512, object_limit: 512 },
	ShrinkPass { string_cap: 64 * 1024, array_limit: 256, object_limit: 256 },
	ShrinkPass { string_cap: 16 * 1024, array_limit: 128, object_limit: 128 },
	ShrinkPass { string_cap: 4 * 1024, array_limit: 64, object_limit: 64 },
	ShrinkPass { string_cap: 1024, array_limit: 32, object_limit: 32 },
	ShrinkPass { string_cap: 256, array_limit: 8, object_limit: 16 },
	ShrinkPass { string_cap: 64, array_limit: 1, object_limit: 8 },
];

fn serialized_frame_bytes(json: &str) -> usize {
	json.len() + 1
}

fn hash_payload(payload: &str) -> u64 {
	let mut hasher = DefaultHasher::new();
	payload.hash(&mut hasher);
	hasher.finish()
}

fn shrink_string(value: &str, cap: usize) -> String {
	if value.chars().count() <= cap {
		return value.to_string();
	}
	let head_length = cap.saturating_sub(STRING_ELISION_RESERVE);
	let head: String = value.chars().take(head_length).collect();
	let elided = value.chars().count() - head.chars().count();
	format!("{head}\n…[{elided} chars elided for RPC frame]")
}

fn shrink_value(value: &Value, pass: &ShrinkPass) -> Value {
	match value {
		Value::String(text) => Value::String(shrink_string(text, pass.string_cap)),
		Value::Array(items) => {
			let keep = items.len().min(pass.array_limit);
			let mut output: Vec<Value> = items
				.iter()
				.take(keep)
				.map(|item| shrink_value(item, pass))
				.collect();
			if keep < items.len() {
				output.push(Value::String(format!(
					"…[{} items elided for RPC frame]",
					items.len() - keep
				)));
			}
			Value::Array(output)
		},
		Value::Object(map) => {
			let mut output = Map::new();
			for (index, (key, item)) in map.iter().enumerate() {
				if index >= pass.object_limit {
					output.insert(
						"rpcFrameElidedKeys".to_string(),
						json!(map.len().saturating_sub(pass.object_limit)),
					);
					break;
				}
				output.insert(key.clone(), shrink_value(item, pass));
			}
			Value::Object(output)
		},
		other => other.clone(),
	}
}

/// Slice an `agent_end` payload to the messages the client has not streamed yet.
fn compact_terminal_frame(frame: &Value, streamed_messages: &[Value]) -> Value {
	let Some(object) = frame.as_object() else {
		return frame.clone();
	};
	if object.get("type").and_then(Value::as_str) != Some("agent_end") {
		return frame.clone();
	}
	let Some(messages) = object.get("messages").and_then(Value::as_array) else {
		return frame.clone();
	};
	let mut streamed = 0usize;
	let limit = streamed_messages.len().min(messages.len());
	while streamed < limit && streamed_messages[streamed] == messages[streamed] {
		streamed += 1;
	}
	let mut compacted = object.clone();
	compacted.insert(
		"messages".to_string(),
		Value::Array(messages[streamed..].to_vec()),
	);
	compacted.insert("messageCount".to_string(), json!(messages.len()));
	Value::Object(compacted)
}

fn overflow_frame(frame: &Value) -> Value {
	let Some(object) = frame.as_object() else {
		return json!({ "type": "rpc_frame_error", "error": "RPC frame exceeded the transport limit" });
	};
	match object.get("type").and_then(Value::as_str) {
		Some("response") => json!({
			"id": object
				.get("id")
				.and_then(Value::as_str)
				.map(|id| shrink_string(id, METADATA_STRING_CAP)),
			"type": "response",
			"command": object
				.get("command")
				.and_then(Value::as_str)
				.map(|command| shrink_string(command, METADATA_STRING_CAP))
				.unwrap_or_else(|| "unknown".to_string()),
			"success": false,
			"error": "RPC response exceeded the transport limit",
		}),
		Some("agent_end") => json!({
			"type": "agent_end",
			"messages": [],
			"messageCount": object.get("messageCount").and_then(Value::as_u64).unwrap_or(0),
		}),
		_ => json!({
			"type": "rpc_frame_error",
			"originalType": object
				.get("type")
				.and_then(Value::as_str)
				.map(|kind| shrink_string(kind, METADATA_STRING_CAP)),
			"error": "RPC frame exceeded the transport limit",
		}),
	}
}

fn encode_frame_from_json(
	frame: &Value,
	mut encoded: String,
	streamed_messages: &[Value],
) -> String {
	if serialized_frame_bytes(&encoded) <= MAX_RPC_FRAME_BYTES {
		return format!("{encoded}\n");
	}
	if frame.get("type").and_then(Value::as_str) == Some("response") {
		return format!("{}\n", overflow_frame(frame));
	}

	let compacted = compact_terminal_frame(frame, streamed_messages);
	encoded = compacted.to_string();
	if serialized_frame_bytes(&encoded) <= MAX_RPC_FRAME_BYTES {
		return format!("{encoded}\n");
	}
	for pass in &SHRINK_PASSES {
		encoded = shrink_value(&compacted, pass).to_string();
		if serialized_frame_bytes(&encoded) <= MAX_RPC_FRAME_BYTES {
			return format!("{encoded}\n");
		}
	}
	format!("{}\n", overflow_frame(&compacted))
}

/// Serialize one complete frame while enforcing the transport ceiling.
#[must_use]
pub fn encode_frame(frame: &Value) -> String {
	let encoded = frame.to_string();
	encode_frame_from_json(frame, encoded, &[])
}

/// Physical lines produced for one logical frame.
///
/// Chunked results are produced lazily from the serialized payload so a
/// near-limit frame never materializes its full base64 transport at once.
pub enum FrameLines {
	Single(Option<String>),
	Chunked {
		payload:   Vec<u8>,
		chunk_id:  String,
		index:     usize,
		count:     usize,
		byte_size: usize,
	},
}

impl Iterator for FrameLines {
	type Item = String;

	fn next(&mut self) -> Option<String> {
		match self {
			Self::Single(line) => line.take(),
			Self::Chunked { payload, chunk_id, index, count, byte_size } => {
				if *index >= *count {
					return None;
				}
				let start = *index * RPC_CHUNK_PAYLOAD_BYTES;
				let end = (start + RPC_CHUNK_PAYLOAD_BYTES).min(payload.len());
				let chunk = json!({
					"type": "rpc_chunk",
					"chunkId": chunk_id,
					"index": *index,
					"count": *count,
					"byteLength": byte_size,
					"data": base64_encode(&payload[start..end]),
				});
				*index += 1;
				Some(format!("{chunk}\n"))
			},
		}
	}
}

fn chunk_lines(payload: String, chunk_id: String) -> FrameLines {
	let byte_size = payload.len();
	let count = byte_size.div_ceil(RPC_CHUNK_PAYLOAD_BYTES);
	FrameLines::Chunked { payload: payload.into_bytes(), chunk_id, index: 0, count, byte_size }
}

/// No legal encoding exists for this payload. Report it instead of truncating;
/// a terminal `agent_end` keeps its own count so the client never sees a
/// fabricated zero or a second turn terminator.
fn reassembled_overflow_lines(frame: &Value) -> String {
	let error = json!({
		"type": "rpc_frame_error",
		"error": "RPC frame exceeded the reassembled transport limit",
	});
	let mut text = format!("{error}\n");
	if frame.get("type").and_then(Value::as_str) == Some("agent_end") {
		let count = frame
			.get("messageCount")
			.and_then(Value::as_u64)
			.or_else(|| frame.get("messages").and_then(Value::as_array).map(|m| m.len() as u64));
		// Only a real count may terminate the turn; an unreadable `agent_end`
		// stays an explicit frame error rather than a fabricated success.
		if let Some(count) = count {
			let terminator = overflow_frame(&json!({
				"type": "agent_end",
				"messageCount": count,
				"willContinue": false,
			}));
			text.push_str(&format!("{terminator}\n"));
		}
	}
	text
}

/// Stateful encoder that remembers which messages the client already streamed
/// and suppresses consecutive exact-equal logical frames.
pub struct RpcFrameEncoder {
	streamed_messages: Vec<Value>,
	protocol_version:  u8,
	chunk_counter:     u64,
	/// Hash of the last delivered logical frame payload (`Value::to_string`).
	last_frame_hash:   Option<u64>,
}

impl Default for RpcFrameEncoder {
	fn default() -> Self {
		Self::new()
	}
}

impl RpcFrameEncoder {
	#[must_use]
	pub fn new() -> Self {
		Self {
			streamed_messages: Vec::new(),
			protocol_version: 1,
			chunk_counter: 0,
			last_frame_hash: None,
		}
	}

	#[must_use]
	pub fn protocol_version(&self) -> u8 {
		self.protocol_version
	}

	pub fn set_protocol_version(&mut self, version: u8) -> Result<(), String> {
		if version != 1 && version != 2 {
			return Err(format!("Unsupported RPC protocol version: {version}"));
		}
		self.protocol_version = version;
		Ok(())
	}

	/// Encode one logical frame. The returned iterable MUST be consumed exactly
	/// once; encoder bookkeeping already ran. Consecutive frames whose payload
	/// serializes identically are collapsed to an empty iterator.
	pub fn encode_frames(&mut self, frame: &Value) -> FrameLines {
		if frame.get("type").and_then(Value::as_str) == Some("agent_start") {
			self.streamed_messages.clear();
			self.last_frame_hash = None;
		}
		let encoded = frame.to_string();
		let payload_hash = hash_payload(&encoded);
		if self.last_frame_hash == Some(payload_hash) {
			return FrameLines::Single(None);
		}
		let mut lines = if self.protocol_version == 2
			&& serialized_frame_bytes(&encoded) > MAX_RPC_FRAME_BYTES
		{
			let compacted = compact_terminal_frame(frame, &self.streamed_messages);
			let compacted_json =
				if compacted == *frame { encoded.clone() } else { compacted.to_string() };
			if serialized_frame_bytes(&compacted_json) > MAX_RPC_REASSEMBLED_BYTES {
				FrameLines::Single(Some(reassembled_overflow_lines(frame)))
			} else if serialized_frame_bytes(&compacted_json) > MAX_RPC_FRAME_BYTES {
				self.chunk_counter += 1;
				chunk_lines(compacted_json, format!("rpc-{}", self.chunk_counter))
			} else {
				FrameLines::Single(Some(format!("{compacted_json}\n")))
			}
		} else {
			FrameLines::Single(Some(encode_frame_from_json(
				frame,
				encoded,
				&self.streamed_messages,
			)))
		};

		match frame.get("type").and_then(Value::as_str) {
			Some("message_end") => {
				// Track the message the client actually received: for a shrunk or
				// single-line frame that is the encoded body, not the source frame.
				let snapshot = match (&mut lines, frame.get("message")) {
					(FrameLines::Single(Some(line)), _) => {
						serde_json::from_str::<Value>(line.trim_end())
							.ok()
							.filter(|parsed| parsed.get("type").and_then(Value::as_str) == Some("message_end"))
							.and_then(|parsed| parsed.get("message").cloned())
					},
					_ => frame.get("message").cloned(),
				};
				if let Some(message) = snapshot {
					self.streamed_messages.push(message);
				}
			},
			Some("agent_end") if frame.get("willContinue").and_then(Value::as_bool) != Some(true) => {
				self.streamed_messages.clear();
			},
			_ => {},
		}
		self.last_frame_hash = Some(payload_hash);
		lines
	}

	/// Convenience for tests and small frames.
	#[must_use]
	pub fn encode(&mut self, frame: &Value) -> String {
		self.encode_frames(frame).collect()
	}
}

struct PendingChunks {
	chunk_id:   String,
	count:      usize,
	byte_length: usize,
	next_index: usize,
	chunks:     Vec<Vec<u8>>,
	received:   usize,
}

/// Reassembles protocol v2 `rpc_chunk` sequences, one parsed JSONL line at a time.
#[derive(Default)]
pub struct RpcFrameDecoder {
	pending: Option<PendingChunks>,
}

impl RpcFrameDecoder {
	/// Feed one parsed line. Returns the logical frame once it is complete.
	pub fn push(&mut self, value: Value) -> Result<Option<Value>, String> {
		if value.get("type").and_then(Value::as_str) != Some("rpc_chunk") {
			if self.pending.is_some() {
				return Err("rpc chunk sequence interrupted".to_string());
			}
			if !value.is_object() {
				return Err("rpc frame must be an object".to_string());
			}
			return Ok(Some(value));
		}

		let object = value.as_object().ok_or("rpc frame must be an object")?;
		let chunk_id = object
			.get("chunkId")
			.and_then(Value::as_str)
			.ok_or("invalid rpc chunk metadata")?;
		if chunk_id.is_empty() || chunk_id.chars().count() > MAX_CHUNK_ID_CHARS {
			return Err("invalid rpc chunk metadata".to_string());
		}
		let index = safe_integer(object.get("index")).ok_or("invalid rpc chunk metadata")?;
		let count = safe_integer(object.get("count")).ok_or("invalid rpc chunk metadata")?;
		let byte_length = safe_integer(object.get("byteLength")).ok_or("invalid rpc chunk metadata")?;
		let max_count = MAX_RPC_REASSEMBLED_BYTES.div_ceil(RPC_CHUNK_PAYLOAD_BYTES);
		if count < 2
			|| count > max_count
			|| index >= count
			|| byte_length < MAX_RPC_FRAME_BYTES
			|| byte_length > MAX_RPC_REASSEMBLED_BYTES
		{
			return Err("invalid rpc chunk metadata".to_string());
		}
		let data = object
			.get("data")
			.and_then(Value::as_str)
			.ok_or("invalid rpc chunk data")?;
		if data.len() > RPC_CHUNK_PAYLOAD_BYTES / 3 * 4 + 4 {
			return Err("rpc chunk payload exceeds the transport limit".to_string());
		}
		let bytes = decode_base64_strict(data)?;
		if bytes.len() > RPC_CHUNK_PAYLOAD_BYTES {
			return Err("rpc chunk payload exceeds the transport limit".to_string());
		}

		if self.pending.is_none() {
			if index != 0 {
				return Err("rpc chunk sequence must start at index 0".to_string());
			}
			self.pending = Some(PendingChunks {
				chunk_id: chunk_id.to_string(),
				count,
				byte_length,
				next_index: 0,
				chunks: Vec::new(),
				received: 0,
			});
		}
		let pending = self.pending.as_mut().ok_or("rpc chunk state lost")?;
		if pending.chunk_id != chunk_id
			|| pending.count != count
			|| pending.byte_length != byte_length
			|| pending.next_index != index
		{
			return Err("rpc chunk sequence mismatch".to_string());
		}
		pending.received = pending.received.saturating_add(bytes.len());
		if pending.received > pending.byte_length {
			return Err("rpc chunk sequence exceeds declared length".to_string());
		}
		pending.chunks.push(bytes);
		pending.next_index += 1;
		if pending.next_index < pending.count {
			return Ok(None);
		}
		if pending.received != pending.byte_length {
			return Err("rpc chunk sequence length mismatch".to_string());
		}
		let pending = self.pending.take().ok_or("rpc chunk state lost")?;
		let mut joined = Vec::with_capacity(pending.byte_length);
		for chunk in pending.chunks {
			joined.extend_from_slice(&chunk);
		}
		let text = String::from_utf8(joined).map_err(|_| "rpc frame is not valid UTF-8".to_string())?;
		let frame: Value = serde_json::from_str(&text).map_err(|error| format!("invalid rpc frame JSON: {error}"))?;
		if !frame.is_object() {
			return Err("rpc frame must be an object".to_string());
		}
		Ok(Some(frame))
	}

	/// True while a chunk sequence is partially received.
	#[must_use]
	pub fn is_pending(&self) -> bool {
		self.pending.is_some()
	}
}

fn safe_integer(value: Option<&Value>) -> Option<usize> {
	let number = value?;
	if let Some(raw) = number.as_u64() {
		return usize::try_from(raw).ok();
	}
	None
}

const BASE64_ALPHABET: &[u8; 64] =
	b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 (RFC 4648, padded) used by `rpc_chunk` payloads.
#[must_use]
pub fn base64_encode(data: &[u8]) -> String {
	let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
	for chunk in data.chunks(3) {
		let b0 = u32::from(chunk[0]);
		let b1 = chunk.get(1).copied().map(u32::from);
		let b2 = chunk.get(2).copied().map(u32::from);
		let triple = (b0 << 16) | (b1.unwrap_or(0) << 8) | b2.unwrap_or(0);
		out.push(BASE64_ALPHABET[(triple >> 18) as usize & 0x3F] as char);
		out.push(BASE64_ALPHABET[(triple >> 12) as usize & 0x3F] as char);
		out.push(if b1.is_some() {
			BASE64_ALPHABET[(triple >> 6) as usize & 0x3F] as char
		} else {
			'='
		});
		out.push(if b2.is_some() {
			BASE64_ALPHABET[triple as usize & 0x3F] as char
		} else {
			'='
		});
	}
	out
}

fn base64_value(byte: u8) -> Option<u32> {
	match byte {
		b'A'..=b'Z' => Some(u32::from(byte - b'A')),
		b'a'..=b'z' => Some(u32::from(byte - b'a') + 26),
		b'0'..=b'9' => Some(u32::from(byte - b'0') + 52),
		b'+' => Some(62),
		b'/' => Some(63),
		_ => None,
	}
}

/// Strict standard-base64 decode: alphabet, padding and canonical pad bits.
pub fn decode_base64_strict(data: &str) -> Result<Vec<u8>, String> {
	if !is_valid_base64_payload(data) {
		return Err("invalid rpc chunk data".to_string());
	}
	let bytes = data.as_bytes();
	let pad = if bytes.ends_with(b"==") {
		2
	} else if bytes.ends_with(b"=") {
		1
	} else {
		0
	};
	let data_end = bytes.len() - pad;
	let mut out = Vec::with_capacity(data_end / 4 * 3 + 3);
	let mut index = 0;
	while index < data_end {
		let remaining = data_end - index;
		let b0 = base64_value(bytes[index]).ok_or("invalid rpc chunk data")?;
		let b1 = base64_value(bytes[index + 1]).ok_or("invalid rpc chunk data")?;
		let b2 = if remaining > 2 {
			base64_value(bytes[index + 2]).ok_or("invalid rpc chunk data")?
		} else {
			0
		};
		let b3 = if remaining > 3 {
			base64_value(bytes[index + 3]).ok_or("invalid rpc chunk data")?
		} else {
			0
		};
		let triple = (b0 << 18) | (b1 << 12) | (b2 << 6) | b3;
		out.push((triple >> 16) as u8);
		if remaining > 2 {
			out.push((triple >> 8) as u8);
		}
		if remaining > 3 {
			out.push(triple as u8);
		}
		index += 4;
	}
	Ok(out)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn big_text(bytes: usize) -> String {
		"x".repeat(bytes)
	}

	#[test]
	fn consecutive_identical_frames_are_collapsed() {
		let mut encoder = RpcFrameEncoder::new();
		let frame = json!({ "type": "notice", "message": "same" });
		let first: Vec<String> = encoder.encode_frames(&frame).collect();
		assert_eq!(first.len(), 1);
		assert!(encoder.encode_frames(&frame).next().is_none());
		assert!(encoder.encode_frames(&frame).next().is_none());
	}

	#[test]
	fn alternating_identical_runs_redeliver_after_change() {
		let mut encoder = RpcFrameEncoder::new();
		let a = json!({ "type": "notice", "message": "A" });
		let b = json!({ "type": "notice", "message": "B" });
		let delivered: Vec<String> = [a.clone(), a.clone(), b.clone(), b.clone(), a]
			.into_iter()
			.flat_map(|frame| encoder.encode_frames(&frame))
			.collect();
		assert_eq!(
			delivered,
			vec![
				"{\"type\":\"notice\",\"message\":\"A\"}\n".to_string(),
				"{\"type\":\"notice\",\"message\":\"B\"}\n".to_string(),
				"{\"type\":\"notice\",\"message\":\"A\"}\n".to_string(),
			]
		);
	}

	#[test]
	fn agent_start_resets_consecutive_dedupe() {
		let mut encoder = RpcFrameEncoder::new();
		let frame = json!({ "type": "notice", "message": "same" });
		assert_eq!(encoder.encode_frames(&frame).count(), 1);
		assert!(encoder.encode_frames(&frame).next().is_none());
		let _: Vec<String> = encoder.encode_frames(&json!({ "type": "agent_start" })).collect();
		assert_eq!(encoder.encode_frames(&frame).count(), 1);
	}
	#[test]
	fn small_frame_is_one_line() {
		let mut encoder = RpcFrameEncoder::new();
		let lines: Vec<String> = encoder.encode_frames(&json!({ "type": "turn_start" })).collect();
		assert_eq!(lines, vec!["{\"type\":\"turn_start\"}\n".to_string()]);
		assert_eq!(encoder.protocol_version(), 1);
	}

	#[test]
	fn oversized_response_becomes_overflow_error() {
		let mut encoder = RpcFrameEncoder::new();
		let frame = json!({
			"id": "r1",
			"type": "response",
			"command": "get_messages",
			"success": true,
			"data": { "messages": big_text(MAX_RPC_FRAME_BYTES + 10) },
		});
		let lines: Vec<String> = encoder.encode_frames(&frame).collect();
		assert_eq!(lines.len(), 1);
		let parsed: Value = serde_json::from_str(lines[0].trim_end()).expect("json");
		assert_eq!(parsed["type"], json!("response"));
		assert_eq!(parsed["success"], json!(false));
		assert_eq!(parsed["id"], json!("r1"));
		assert_eq!(parsed["error"], json!("RPC response exceeded the transport limit"));
	}

	#[test]
	fn v2_chunks_and_decodes_round_trip() {
		let mut encoder = RpcFrameEncoder::new();
		encoder.set_protocol_version(2).expect("v2");
		let payload = big_text(MAX_RPC_FRAME_BYTES + 1024);
		let frame = json!({ "type": "notice", "level": "info", "message": payload });
		let lines: Vec<String> = encoder.encode_frames(&frame).collect();
		assert!(lines.len() >= 3, "chunked into {}", lines.len());
		assert!(lines.iter().all(|line| serialized_frame_bytes(line.trim_end()) <= MAX_RPC_FRAME_BYTES));

		let mut decoder = RpcFrameDecoder::default();
		let mut decoded = None;
		for line in &lines {
			let value: Value = serde_json::from_str(line.trim_end()).expect("json");
			if let Some(frame) = decoder.push(value).expect("decode") {
				decoded = Some(frame);
			}
		}
		let decoded = decoded.expect("frame reassembled");
		assert_eq!(decoded["type"], json!("notice"));
		assert_eq!(decoded["message"].as_str().expect("message").len(), payload.len());
	}

	#[test]
	fn v2_compacts_agent_end_against_streamed_messages() {
		let mut encoder = RpcFrameEncoder::new();
		encoder.set_protocol_version(2).expect("v2");
		// The oversized streamed message is what pushes the raw `agent_end`
		// over the frame bound; compaction drops it and keeps the small tail.
		let assistant = json!({
			"role": "assistant",
			"content": big_text(MAX_RPC_FRAME_BYTES + 10),
			"timestamp": 1
		});
		let _: Vec<String> = encoder
			.encode_frames(&json!({ "type": "message_end", "message": assistant.clone() }))
			.collect();
		let mut messages = vec![assistant];
		messages.push(json!({ "role": "toolResult", "content": [], "timestamp": 2 }));
		let lines: Vec<String> = encoder
			.encode_frames(&json!({ "type": "agent_end", "messages": messages }))
			.collect();
		assert_eq!(lines.len(), 1, "compacted frame must fit one line");
		let parsed: Value = serde_json::from_str(lines[0].trim_end()).expect("json");
		assert_eq!(parsed["type"], json!("agent_end"));
		assert_eq!(parsed["messages"].as_array().expect("messages").len(), 1);
		assert_eq!(parsed["messages"][0]["role"], json!("toolResult"));
		assert_eq!(parsed["messageCount"], json!(2));
	}

	#[test]
	fn decoder_rejects_bad_chunk_sequences() {
		let mut decoder = RpcFrameDecoder::default();
		let error = decoder
			.push(json!({
				"type": "rpc_chunk",
				"chunkId": "c",
				"index": 1,
				"count": 2,
				"byteLength": MAX_RPC_FRAME_BYTES,
				"data": base64_encode(b"hello"),
			}))
			.expect_err("must start at index 0");
		assert_eq!(error, "rpc chunk sequence must start at index 0");

		let mut decoder = RpcFrameDecoder::default();
		let error = decoder
			.push(json!({
				"type": "rpc_chunk",
				"chunkId": "c",
				"index": 0,
				"count": 2,
				"byteLength": MAX_RPC_FRAME_BYTES,
				"data": "not base64!",
			}))
			.expect_err("invalid data");
		assert_eq!(error, "invalid rpc chunk data");

		let mut decoder = RpcFrameDecoder::default();
		assert!(decoder.push(json!({ "type": "turn_start" })).expect("frame").is_some());
		assert!(decoder.push(json!([])).is_err());
	}

	#[test]
	fn decoder_rejects_interrupted_sequence() {
		let mut decoder = RpcFrameDecoder::default();
		let chunk = json!({
			"type": "rpc_chunk",
			"chunkId": "c",
			"index": 0,
			"count": 2,
			"byteLength": MAX_RPC_FRAME_BYTES,
			"data": base64_encode(&vec![b'a'; RPC_CHUNK_PAYLOAD_BYTES]),
		});
		assert!(decoder.push(chunk).expect("first chunk").is_none());
		assert_eq!(
			decoder.push(json!({ "type": "turn_start" })).expect_err("interrupted"),
			"rpc chunk sequence interrupted"
		);
	}

	#[test]
	fn base64_round_trips_and_rejects_non_canonical() {
		for bytes in [b"f".as_slice(), b"fo", b"foo", b"foobar", &[0u8, 255, 1, 128]] {
			let encoded = base64_encode(bytes);
			assert_eq!(decode_base64_strict(&encoded).expect("decode"), bytes.to_vec());
		}
		assert!(decode_base64_strict("Zg=").is_err());
		assert!(decode_base64_strict("Zh==").is_err());
		assert!(decode_base64_strict("!!").is_err());
	}

	#[test]
	fn shrink_keeps_strings_and_arrays_bounded() {
		let value = json!({
			"type": "notice",
			"message": big_text(MAX_RPC_FRAME_BYTES),
			"items": (0..1000).collect::<Vec<_>>(),
		});
		let shrunk = shrink_value(&value, &SHRINK_PASSES[6]);
		let encoded = shrunk.to_string();
		assert!(serialized_frame_bytes(&encoded) < MAX_RPC_FRAME_BYTES);
		assert_eq!(shrunk["items"].as_array().expect("items").len(), 2);
	}
}
