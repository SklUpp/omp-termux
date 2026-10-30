//! One conversion from native agent events to TS-shaped RPC frames.
//!
//! Both the main session (`mode::EventFrames`) and subagent observers map the
//! same native events onto the wire, so the frame builders live here instead of
//! being written twice. Frame shapes follow the TS protocol: an assistant
//! `message_update` carries the message snapshot plus `assistantMessageEvent`,
//! and `tool_execution_end` carries a `content` block array, not a bare string.

use pi_agent::{ContentPart, Message, ToolResult};
use serde_json::{Value, json};

/// Epoch milliseconds, as the protocol's message timestamps.
pub fn now_ms() -> u64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.unwrap_or_default()
		.as_millis() as u64
}

/// One content block in TS `AgentMessage` shape.
pub fn content_block(part: &ContentPart) -> Value {
	match part {
		ContentPart::Text { text } => json!({ "type": "text", "text": text }),
		ContentPart::Thinking { thinking } => {
			json!({ "type": "thinking", "thinking": thinking })
		},
		ContentPart::Image { data, mime_type } => {
			json!({ "type": "image", "data": data, "mimeType": mime_type })
		},
	}
}

/// Content blocks for a list of parts.
pub fn content_blocks(parts: &[ContentPart]) -> Vec<Value> {
	parts.iter().map(content_block).collect()
}

/// `message_start` frame.
pub fn message_start(role: &str, timestamp_ms: u64) -> Value {
	json!({
		"type": "message_start",
		"message": { "role": role, "content": [], "timestamp": timestamp_ms },
	})
}

/// The TS `UserMessage` object for a list of parts.
pub fn user_message(parts: &[ContentPart], timestamp_ms: u64) -> Value {
	json!({
		"role": "user",
		"content": content_blocks(parts),
		"timestamp": timestamp_ms,
	})
}

/// `message_end` frame for a user message built from parts.
pub fn message_end_user(parts: &[ContentPart], timestamp_ms: u64) -> Value {
	json!({ "type": "message_end", "message": user_message(parts, timestamp_ms) })
}

/// `message_start` + `message_end` pair for an accepted user message.
///
/// TS `emitInputMessages` pushes the *same* fully-populated message into both
/// frames (only the assistant stream starts empty and accumulates), so the host
/// can reconcile queue acceptance against actual injection from either frame.
pub fn user_message_pair(parts: &[ContentPart], timestamp_ms: u64) -> [Value; 2] {
	let message = user_message(parts, timestamp_ms);
	[
		json!({ "type": "message_start", "message": message.clone() }),
		json!({ "type": "message_end", "message": message }),
	]
}

/// Assistant message snapshot in TS `AssistantMessage` shape.
pub fn assistant_message(
	message: &Message,
	api: &str,
	provider: &str,
	model: &str,
	stop_reason: &str,
	timestamp_ms: u64,
) -> Value {
	let mut content = content_blocks(&message.parts);
	if content.is_empty() && !message.content.is_empty() {
		content.push(json!({ "type": "text", "text": message.content }));
	}
	let usage = message.usage.as_ref().map_or(Value::Null, |usage| {
		json!({
			"input": usage.input,
			"output": usage.output,
			"cacheRead": usage.cache_read,
			"cacheWrite": usage.cache_write,
			"totalTokens": usage.total_tokens,
		})
	});
	json!({
		"role": "assistant",
		"content": content,
		"api": api,
		"provider": provider,
		"model": model,
		"usage": usage,
		"stopReason": stop_reason,
		"timestamp": timestamp_ms,
	})
}

/// `message_update` frame: delta plus the current snapshot and the streamed
/// `assistantMessageEvent` the TS host reads.
pub fn message_update(
	message: &Message,
	delta: Option<&str>,
	api: &str,
	provider: &str,
	model: &str,
	stop_reason: &str,
	timestamp_ms: u64,
) -> Value {
	let snapshot = assistant_message(message, api, provider, model, stop_reason, timestamp_ms);
	let event = match delta {
		Some(delta) => json!({ "type": "text_delta", "delta": delta }),
		None => json!({ "type": "done" }),
	};
	json!({
		"type": "message_update",
		"message": snapshot,
		"assistantMessageEvent": event,
	})
}

/// `message_update` frame for a public reasoning delta. Same un-gated JSON path
/// as `text_delta`: the flag gates only the text render, never this stream.
pub fn thinking_update(
	message: &Message,
	delta: &str,
	api: &str,
	provider: &str,
	model: &str,
	stop_reason: &str,
	timestamp_ms: u64,
) -> Value {
	let snapshot = assistant_message(message, api, provider, model, stop_reason, timestamp_ms);
	json!({
		"type": "message_update",
		"message": snapshot,
		"assistantMessageEvent": { "type": "thinking_delta", "delta": delta },
	})
}

/// `message_end` frame for an already-shaped assistant message.
pub fn message_end(message: Value, timestamp_ms: u64) -> Value {
	let mut value = message;
	if let Some(map) = value.as_object_mut() {
		map.insert("timestamp".to_string(), json!(timestamp_ms));
	}
	json!({ "type": "message_end", "message": value })
}

/// `tool_execution_start` frame.
pub fn tool_start(id: &str, tool: &str, args: &Value) -> Value {
	json!({
		"type": "tool_execution_start",
		"toolCallId": id,
		"toolName": tool,
		"args": args,
	})
}

/// `tool_execution_update` frame: partial output of a running tool.
pub fn tool_update(id: &str, tool: &str, args: &Value, partial: &Value) -> Value {
	json!({
		"type": "tool_execution_update",
		"toolCallId": id,
		"toolName": tool,
		"args": args,
		"partialResult": partial,
	})
}

/// `tool_execution_end` frame with the TS `AgentToolResult`.
///
/// `details` is optional in the TS result and is omitted entirely when absent,
/// so a host that sent structured details gets them back verbatim.
pub fn tool_end(
	id: &str,
	tool: &str,
	text: &str,
	images: &[ContentPart],
	details: Option<&Value>,
	is_error: bool,
) -> Value {
	let mut blocks = vec![json!({ "type": "text", "text": text })];
	blocks.extend(content_blocks(images));
	let mut result = json!({ "content": blocks });
	if let Some(details) = details {
		result["details"] = details.clone();
	}
	json!({
		"type": "tool_execution_end",
		"toolCallId": id,
		"toolName": tool,
		"result": result,
		"isError": is_error,
	})
}

/// TS `ToolResultMessage`: the flat entry a `turn_end.toolResults` array holds.
///
/// Distinct from the `tool_execution_end` frame: the frame wraps the result in
/// `{ type, toolCallId, toolName, result, isError }`, while a turn carries whole
/// messages (`role`, `content` blocks, `timestamp`). Both are built from the same
/// native result so they can never disagree on text, images, details or status.
pub fn tool_result_message(
	result: &ToolResult,
	name: &str,
	images: &[ContentPart],
	timestamp_ms: u64,
) -> Value {
	let mut blocks = Vec::with_capacity(images.len() + 1);
	if !result.content.is_empty() {
		blocks.push(json!({ "type": "text", "text": result.content }));
	}
	blocks.extend(content_blocks(images));
	let mut message = json!({
		"role": "toolResult",
		"toolCallId": result.tool_call_id,
		"toolName": name,
		"content": blocks,
		"isError": result.is_error,
		"timestamp": timestamp_ms,
	});
	if let Some(details) = &result.details {
		message["details"] = details.clone();
	}
	message
}

/// `tool_execution_end` frame built from a native tool result, forwarding its
/// own `details`.
pub fn tool_end_result(result: &ToolResult, name: &str, images: &[ContentPart]) -> Value {
	tool_end(
		&result.tool_call_id,
		name,
		&result.content,
		images,
		result.details.as_ref(),
		result.is_error,
	)
}

#[cfg(test)]
mod tests {
	use super::*;
	use pi_agent::TokenUsage;

	fn text_block(text: &str) -> Value {
		json!({ "type": "text", "text": text })
	}

	fn image_block(data: &str, mime: &str) -> Value {
		json!({ "type": "image", "data": data, "mimeType": mime })
	}

	fn image(data: &str, mime: &str) -> ContentPart {
		ContentPart::Image { data: data.to_string(), mime_type: mime.to_string() }
	}

	fn assistant(parts: Vec<ContentPart>, content: &str) -> Message {
		let mut message = Message::assistant(content);
		message.parts = parts;
		message
	}

	#[test]
	fn user_pair_sends_the_same_full_message_in_both_frames() {
		let parts = vec![ContentPart::text("look"), image("AAAA", "image/png")];
		let [start, end] = user_message_pair(&parts, 7);

		assert_eq!(start["type"], "message_start");
		assert_eq!(end["type"], "message_end");
		assert_eq!(start["message"], end["message"], "queue acceptance and injection must agree");
		assert_eq!(start["message"]["role"], "user");
		assert_eq!(start["message"]["timestamp"], 7);
		assert_eq!(
			start["message"]["content"],
			json!([text_block("look"), image_block("AAAA", "image/png")])
		);

		let stream = message_start("assistant", 9);
		assert_eq!(stream["message"]["role"], "assistant");
		assert_eq!(stream["message"]["content"], json!([]), "only the assistant stream starts empty");
		assert_eq!(stream["message"]["timestamp"], 9);
	}

	#[test]
	fn message_update_carries_the_delta_and_the_snapshot() {
		let message = assistant(vec![ContentPart::text("hel")], "hel");
		let frame = message_update(
			&message,
			Some("hel"),
			"openai-completions",
			"qwen-portal",
			"coder-model",
			"stop",
			11,
		);

		assert_eq!(frame["type"], "message_update");
		assert_eq!(frame["assistantMessageEvent"], json!({ "type": "text_delta", "delta": "hel" }));

		let snapshot = &frame["message"];
		assert_eq!(snapshot["role"], "assistant");
		assert_eq!(snapshot["content"], json!([text_block("hel")]));
		assert_eq!(snapshot["api"], "openai-completions");
		assert_eq!(snapshot["provider"], "qwen-portal");
		assert_eq!(snapshot["model"], "coder-model");
		assert_eq!(snapshot["stopReason"], "stop");
		assert_eq!(snapshot["timestamp"], 11);
		assert!(snapshot["usage"].is_null(), "a silent provider is null, never zero counters");
	}

	#[test]
	fn message_update_without_a_delta_closes_the_stream() {
		let message = assistant(vec![ContentPart::text("done")], "done");
		let frame = message_update(&message, None, "", "", "", "toolUse", 12);

		assert_eq!(frame["assistantMessageEvent"], json!({ "type": "done" }));
		assert!(
			frame["assistantMessageEvent"].get("delta").is_none(),
			"no delta on a terminal update"
		);
		assert_eq!(
			frame["message"]["stopReason"], "toolUse",
			"canonical stop reasons pass through verbatim"
		);
	}

	#[test]
	fn assistant_content_prefers_parts_and_falls_back_to_the_text_field() {
		let from_parts = assistant_message(
			&assistant(vec![ContentPart::text("parts win")], "ignored"),
			"",
			"",
			"",
			"endTurn",
			0,
		);
		assert_eq!(
			from_parts["content"],
			json!([text_block("parts win")]),
			"parts and text never duplicate"
		);

		let legacy =
			assistant_message(&assistant(Vec::new(), "legacy text"), "", "", "", "endTurn", 0);
		assert_eq!(
			legacy["content"],
			json!([text_block("legacy text")]),
			"text-only messages still serialize"
		);

		let empty = assistant_message(&assistant(Vec::new(), ""), "", "", "", "endTurn", 0);
		assert_eq!(empty["content"], json!([]), "an empty message stays empty");
	}

	#[test]
	fn usage_uses_the_camel_case_token_counters_only() {
		let mut message = assistant(vec![ContentPart::text("x")], "x");
		message.usage = Some(TokenUsage {
			input: 3,
			output: 2,
			cache_read: 1,
			cache_write: 4,
			total_tokens: 10,
			..TokenUsage::default()
		});

		let frame = assistant_message(&message, "", "", "", "endTurn", 0);
		assert_eq!(
			frame["usage"],
			json!({ "input": 3, "output": 2, "cacheRead": 1, "cacheWrite": 4, "totalTokens": 10 })
		);
		assert!(
			frame["usage"].get("reasoningTokens").is_none(),
			"only the TS counters reach the host"
		);
	}

	#[test]
	fn tool_frames_echo_the_call_identity_and_args() {
		let args = json!({ "path": "a.txt" });
		let start = tool_start("call-1", "read", &args);
		assert_eq!(
			start,
			json!({ "type": "tool_execution_start", "toolCallId": "call-1", "toolName": "read", "args": args })
		);

		let update = tool_update("call-1", "read", &args, &json!({ "bytes": 3 }));
		assert_eq!(update["type"], "tool_execution_update");
		assert_eq!(update["toolCallId"], "call-1");
		assert_eq!(update["toolName"], "read");
		assert_eq!(update["args"], args);
		assert_eq!(update["partialResult"], json!({ "bytes": 3 }));
	}

	#[test]
	fn tool_end_holds_text_then_images_and_omits_absent_details() {
		let images = vec![image("AAAA", "image/png")];
		let frame = tool_end("call-1", "read", "file body", &images, None, false);

		assert_eq!(frame["type"], "tool_execution_end");
		assert_eq!(frame["toolCallId"], "call-1");
		assert_eq!(frame["toolName"], "read");
		assert_eq!(frame["isError"], false);
		assert_eq!(
			frame["result"]["content"],
			json!([text_block("file body"), image_block("AAAA", "image/png")])
		);
		assert!(frame["result"].get("details").is_none(), "absent details stay absent, never null");

		let with_details =
			tool_end("call-1", "read", "file body", &[], Some(&json!({ "lines": 2 })), true);
		assert_eq!(with_details["result"]["details"], json!({ "lines": 2 }));
		assert_eq!(with_details["isError"], true);
	}

	#[test]
	fn tool_end_result_and_the_turn_entry_agree_on_content_status_and_details() {
		let result = ToolResult {
			tool_call_id: "call-7".to_string(),
			content: "exit 1".to_string(),
			is_error: true,
			terminate: false,
			details: Some(json!({ "code": 1 })),
		};

		let frame = tool_end_result(&result, "bash", &[]);
		assert_eq!(frame["type"], "tool_execution_end");
		assert_eq!(frame["toolCallId"], "call-7");
		assert_eq!(frame["result"]["content"], json!([text_block("exit 1")]));
		assert_eq!(frame["result"]["details"], json!({ "code": 1 }));
		assert_eq!(frame["isError"], true);

		let entry = tool_result_message(&result, "bash", &[], 21);
		assert_eq!(entry["role"], "toolResult");
		assert_eq!(entry["toolCallId"], "call-7");
		assert_eq!(entry["toolName"], "bash");
		assert_eq!(entry["content"], frame["result"]["content"], "both shapes carry the same blocks");
		assert_eq!(entry["isError"], frame["isError"]);
		assert_eq!(entry["details"], frame["result"]["details"]);
		assert_eq!(entry["timestamp"], 21);
	}

	#[test]
	fn tool_result_message_drops_an_empty_text_block() {
		let result = ToolResult {
			tool_call_id: "call-2".to_string(),
			content: String::new(),
			is_error: false,
			terminate: false,
			details: None,
		};
		let entry = tool_result_message(&result, "screenshot", &[image("AAAA", "image/png")], 1);

		assert_eq!(
			entry["content"],
			json!([image_block("AAAA", "image/png")]),
			"no empty text block first"
		);
		assert!(entry.get("details").is_none());
		assert_eq!(entry["isError"], false);
	}

	#[test]
	fn message_end_stamps_the_timestamp_inside_the_message() {
		let shaped = assistant_message(
			&assistant(vec![ContentPart::text("final")], "final"),
			"",
			"",
			"",
			"endTurn",
			0,
		);
		let frame = message_end(shaped, 42);

		let mut keys: Vec<&str> = frame
			.as_object()
			.expect("frame object")
			.keys()
			.map(String::as_str)
			.collect();
		keys.sort_unstable();
		assert_eq!(keys, ["message", "type"], "the frame adds no fields of its own");
		assert_eq!(frame["message"]["timestamp"], 42, "the stamp replaces the stream placeholder");
		assert_eq!(frame["message"]["content"], json!([text_block("final")]));

		let passthrough = message_end(Value::Null, 1);
		assert!(passthrough["message"].is_null(), "a non-object message is forwarded, not a panic");
	}
}
