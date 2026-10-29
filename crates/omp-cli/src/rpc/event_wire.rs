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
