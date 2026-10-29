//! RPC wire types shared with `packages/coding-agent/src/modes/rpc/rpc-types.ts`.
//!
//! The TypeScript file is the contract: every command name, field spelling and
//! frame shape here mirrors it, so an existing RPC client can drive this binary
//! without a translation layer. Frames whose payload is an agent-defined
//! structure (models, messages, todos) travel as [`serde_json::Value`] and are
//! built from real native state at the emit site.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Version announced by the `ready` frame.
pub const PROTOCOL_VERSION: u8 = 1;
/// Versions this build can serve: v1 JSONL, v2 chunked JSONL.
pub const SUPPORTED_PROTOCOL_VERSIONS: [u8; 2] = [1, 2];

/// Image content accepted by `prompt`/`steer`/`follow_up`/`abort_and_prompt`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcImage {
	pub data:      String,
	pub mime_type: String,
}

/// `prompt.streamingBehavior`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StreamingBehavior {
	Steer,
	FollowUp,
}

/// `set_steering_mode` / `set_follow_up_mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum QueueMode {
	#[serde(rename = "all")]
	All,
	#[serde(rename = "one-at-a-time")]
	OneAtATime,
}

impl QueueMode {
	#[must_use]
	pub fn as_str(self) -> &'static str {
		match self {
			Self::All => "all",
			Self::OneAtATime => "one-at-a-time",
		}
	}
}

/// `set_interrupt_mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InterruptMode {
	Immediate,
	Wait,
}

impl InterruptMode {
	#[must_use]
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Immediate => "immediate",
			Self::Wait => "wait",
		}
	}
}

/// `set_subagent_subscription`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubagentSubscriptionLevel {
	Off,
	Progress,
	Events,
}

impl Default for SubagentSubscriptionLevel {
	/// Upstream `#subscriptionLevel = "off"` (`rpc-subagents.ts:113`).
	fn default() -> Self {
		Self::Off
	}
}

impl SubagentSubscriptionLevel {
	#[must_use]
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Off => "off",
			Self::Progress => "progress",
			Self::Events => "events",
		}
	}
}

/// One host tool definition from `set_host_tools`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcHostToolDefinition {
	pub name:        String,
	#[serde(default)]
	pub label:       Option<String>,
	#[serde(default)]
	pub description: String,
	#[serde(default)]
	pub parameters:  Value,
	#[serde(default)]
	pub hidden:      bool,
	#[serde(default)]
	pub load_mode:   Option<String>,
}

/// One host URI scheme from `set_host_uri_schemes`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcHostUriSchemeDefinition {
	pub scheme:      String,
	#[serde(default)]
	pub description: Option<String>,
	#[serde(default)]
	pub writable:    bool,
	#[serde(default)]
	pub immutable:   bool,
}

/// Every command accepted on stdin (`RpcCommand` upstream).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RpcCommand {
	// Protocol
	#[serde(rename_all = "camelCase")]
	NegotiateProtocol {
		#[serde(default)]
		id:               Option<String>,
		protocol_version: i64,
	},

	// Prompting
	#[serde(rename_all = "camelCase")]
	Prompt {
		#[serde(default)]
		id:                Option<String>,
		message:           String,
		#[serde(default)]
		images:            Option<Vec<RpcImage>>,
		#[serde(default)]
		streaming_behavior: Option<StreamingBehavior>,
	},
	#[serde(rename_all = "camelCase")]
	Steer {
		#[serde(default)]
		id:      Option<String>,
		message: String,
		#[serde(default)]
		images:  Option<Vec<RpcImage>>,
	},
	#[serde(rename_all = "camelCase")]
	FollowUp {
		#[serde(default)]
		id:      Option<String>,
		message: String,
		#[serde(default)]
		images:  Option<Vec<RpcImage>>,
	},
	Abort {
		#[serde(default)]
		id: Option<String>,
	},
	#[serde(rename_all = "camelCase")]
	AbortAndPrompt {
		#[serde(default)]
		id:      Option<String>,
		message: String,
		#[serde(default)]
		images:  Option<Vec<RpcImage>>,
	},
	#[serde(rename_all = "camelCase")]
	NewSession {
		#[serde(default)]
		id:             Option<String>,
		#[serde(default)]
		parent_session: Option<String>,
	},

	// State
	GetState {
		#[serde(default)]
		id: Option<String>,
	},
	#[serde(rename_all = "camelCase")]
	SetFastMode {
		#[serde(default)]
		id:      Option<String>,
		enabled: bool,
	},
	GetAvailableCommands {
		#[serde(default)]
		id: Option<String>,
	},
	SetTodos {
		#[serde(default)]
		id:     Option<String>,
		phases: Vec<pi_agent::tools::todo::TodoPhase>,
	},
	#[serde(rename_all = "camelCase")]
	SetHostTools {
		#[serde(default)]
		id:    Option<String>,
		tools: Vec<RpcHostToolDefinition>,
	},
	#[serde(rename_all = "camelCase")]
	SetHostUriSchemes {
		#[serde(default)]
		id:      Option<String>,
		schemes: Vec<RpcHostUriSchemeDefinition>,
	},
	#[serde(rename_all = "camelCase")]
	SetSubagentSubscription {
		#[serde(default)]
		id:    Option<String>,
		level: SubagentSubscriptionLevel,
	},
	GetSubagents {
		#[serde(default)]
		id: Option<String>,
	},
	#[serde(rename_all = "camelCase")]
	GetSubagentMessages {
		#[serde(default)]
		id:          Option<String>,
		#[serde(default)]
		subagent_id: Option<String>,
		#[serde(default)]
		session_file: Option<String>,
		#[serde(default)]
		from_byte:   Option<i64>,
	},

	// Model
	#[serde(rename_all = "camelCase")]
	SetModel {
		#[serde(default)]
		id:       Option<String>,
		provider: String,
		model_id: String,
	},
	CycleModel {
		#[serde(default)]
		id: Option<String>,
	},
	GetAvailableModels {
		#[serde(default)]
		id: Option<String>,
	},

	// Thinking
	#[serde(rename_all = "camelCase")]
	SetThinkingLevel {
		#[serde(default)]
		id:    Option<String>,
		level: String,
	},
	CycleThinkingLevel {
		#[serde(default)]
		id: Option<String>,
	},

	// Queue modes
	#[serde(rename_all = "camelCase")]
	SetSteeringMode {
		#[serde(default)]
		id:   Option<String>,
		mode: QueueMode,
	},
	#[serde(rename_all = "camelCase")]
	SetFollowUpMode {
		#[serde(default)]
		id:   Option<String>,
		mode: QueueMode,
	},
	#[serde(rename_all = "camelCase")]
	SetInterruptMode {
		#[serde(default)]
		id:   Option<String>,
		mode: InterruptMode,
	},

	// Compaction
	#[serde(rename_all = "camelCase")]
	Compact {
		#[serde(default)]
		id:                  Option<String>,
		#[serde(default)]
		custom_instructions: Option<String>,
	},
	#[serde(rename_all = "camelCase")]
	SetAutoCompaction {
		#[serde(default)]
		id:      Option<String>,
		enabled: bool,
	},

	// Retry
	#[serde(rename_all = "camelCase")]
	SetAutoRetry {
		#[serde(default)]
		id:      Option<String>,
		enabled: bool,
	},
	AbortRetry {
		#[serde(default)]
		id: Option<String>,
	},

	// Bash
	Bash {
		#[serde(default)]
		id:      Option<String>,
		command: String,
	},
	AbortBash {
		#[serde(default)]
		id: Option<String>,
	},

	// Session
	GetSessionStats {
		#[serde(default)]
		id: Option<String>,
	},
	#[serde(rename_all = "camelCase")]
	ExportHtml {
		#[serde(default)]
		id:          Option<String>,
		#[serde(default)]
		output_path: Option<String>,
	},
	#[serde(rename_all = "camelCase")]
	SwitchSession {
		#[serde(default)]
		id:           Option<String>,
		session_path: String,
	},
	#[serde(rename_all = "camelCase")]
	Branch {
		#[serde(default)]
		id:       Option<String>,
		entry_id: String,
	},
	GetBranchMessages {
		#[serde(default)]
		id: Option<String>,
	},
	GetLastAssistantText {
		#[serde(default)]
		id: Option<String>,
	},
	#[serde(rename_all = "camelCase")]
	SetSessionName {
		#[serde(default)]
		id:   Option<String>,
		name: String,
	},
	#[serde(rename_all = "camelCase")]
	Handoff {
		#[serde(default)]
		id:                  Option<String>,
		#[serde(default)]
		custom_instructions: Option<String>,
	},

	// Messages
	GetMessages {
		#[serde(default)]
		id: Option<String>,
	},
	#[serde(rename_all = "camelCase")]
	GetMessagesPage {
		#[serde(default)]
		id:     Option<String>,
		#[serde(default)]
		cursor: Option<String>,
		#[serde(default)]
		limit:  Option<i64>,
	},

	// Login
	GetLoginProviders {
		#[serde(default)]
		id: Option<String>,
	},
	#[serde(rename_all = "camelCase")]
	Login {
		#[serde(default)]
		id:          Option<String>,
		provider_id: String,
	},
}

impl RpcCommand {
	/// Correlation id echoed on the response, when the client sent one.
	#[must_use]
	pub fn id(&self) -> Option<&str> {
		match self {
			Self::NegotiateProtocol { id, .. }
			| Self::Prompt { id, .. }
			| Self::Steer { id, .. }
			| Self::FollowUp { id, .. }
			| Self::Abort { id }
			| Self::AbortAndPrompt { id, .. }
			| Self::NewSession { id, .. }
			| Self::GetState { id }
			| Self::SetFastMode { id, .. }
			| Self::GetAvailableCommands { id }
			| Self::SetTodos { id, .. }
			| Self::SetHostTools { id, .. }
			| Self::SetHostUriSchemes { id, .. }
			| Self::SetSubagentSubscription { id, .. }
			| Self::GetSubagents { id }
			| Self::GetSubagentMessages { id, .. }
			| Self::SetModel { id, .. }
			| Self::CycleModel { id }
			| Self::GetAvailableModels { id }
			| Self::SetThinkingLevel { id, .. }
			| Self::CycleThinkingLevel { id }
			| Self::SetSteeringMode { id, .. }
			| Self::SetFollowUpMode { id, .. }
			| Self::SetInterruptMode { id, .. }
			| Self::Compact { id, .. }
			| Self::SetAutoCompaction { id, .. }
			| Self::SetAutoRetry { id, .. }
			| Self::AbortRetry { id }
			| Self::Bash { id, .. }
			| Self::AbortBash { id }
			| Self::GetSessionStats { id }
			| Self::ExportHtml { id, .. }
			| Self::SwitchSession { id, .. }
			| Self::Branch { id, .. }
			| Self::GetBranchMessages { id }
			| Self::GetLastAssistantText { id }
			| Self::SetSessionName { id, .. }
			| Self::Handoff { id, .. }
			| Self::GetMessages { id }
			| Self::GetMessagesPage { id, .. }
			| Self::GetLoginProviders { id }
			| Self::Login { id, .. } => id.as_deref(),
		}
	}

	/// Wire command name (`RpcCommand["type"]`).
	#[must_use]
	pub fn kind(&self) -> &'static str {
		match self {
			Self::NegotiateProtocol { .. } => "negotiate_protocol",
			Self::Prompt { .. } => "prompt",
			Self::Steer { .. } => "steer",
			Self::FollowUp { .. } => "follow_up",
			Self::Abort { .. } => "abort",
			Self::AbortAndPrompt { .. } => "abort_and_prompt",
			Self::NewSession { .. } => "new_session",
			Self::GetState { .. } => "get_state",
			Self::SetFastMode { .. } => "set_fast_mode",
			Self::GetAvailableCommands { .. } => "get_available_commands",
			Self::SetTodos { .. } => "set_todos",
			Self::SetHostTools { .. } => "set_host_tools",
			Self::SetHostUriSchemes { .. } => "set_host_uri_schemes",
			Self::SetSubagentSubscription { .. } => "set_subagent_subscription",
			Self::GetSubagents { .. } => "get_subagents",
			Self::GetSubagentMessages { .. } => "get_subagent_messages",
			Self::SetModel { .. } => "set_model",
			Self::CycleModel { .. } => "cycle_model",
			Self::GetAvailableModels { .. } => "get_available_models",
			Self::SetThinkingLevel { .. } => "set_thinking_level",
			Self::CycleThinkingLevel { .. } => "cycle_thinking_level",
			Self::SetSteeringMode { .. } => "set_steering_mode",
			Self::SetFollowUpMode { .. } => "set_follow_up_mode",
			Self::SetInterruptMode { .. } => "set_interrupt_mode",
			Self::Compact { .. } => "compact",
			Self::SetAutoCompaction { .. } => "set_auto_compaction",
			Self::SetAutoRetry { .. } => "set_auto_retry",
			Self::AbortRetry { .. } => "abort_retry",
			Self::Bash { .. } => "bash",
			Self::AbortBash { .. } => "abort_bash",
			Self::GetSessionStats { .. } => "get_session_stats",
			Self::ExportHtml { .. } => "export_html",
			Self::SwitchSession { .. } => "switch_session",
			Self::Branch { .. } => "branch",
			Self::GetBranchMessages { .. } => "get_branch_messages",
			Self::GetLastAssistantText { .. } => "get_last_assistant_text",
			Self::SetSessionName { .. } => "set_session_name",
			Self::Handoff { .. } => "handoff",
			Self::GetMessages { .. } => "get_messages",
			Self::GetMessagesPage { .. } => "get_messages_page",
			Self::GetLoginProviders { .. } => "get_login_providers",
			Self::Login { .. } => "login",
		}
	}
}

/// One `{type:"response"}` frame.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RpcResponse {
	#[serde(skip_serializing_if = "Option::is_none")]
	pub id:      Option<String>,
	#[serde(rename = "type")]
	pub kind:    &'static str,
	pub command: String,
	pub success: bool,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub data:    Option<Value>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub error:   Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub code:    Option<String>,
}

impl RpcResponse {
	#[must_use]
	pub fn success(id: Option<&str>, command: &str) -> Self {
		Self {
			id: id.map(str::to_string),
			kind: "response",
			command: command.to_string(),
			success: true,
			data: None,
			error: None,
			code: None,
		}
	}

	#[must_use]
	pub fn success_data(id: Option<&str>, command: &str, data: Value) -> Self {
		Self { data: Some(data), ..Self::success(id, command) }
	}

	#[must_use]
	pub fn error(id: Option<&str>, command: &str, message: impl Into<String>) -> Self {
		Self {
			id: id.map(str::to_string),
			kind: "response",
			command: command.to_string(),
			success: false,
			data: None,
			error: Some(message.into()),
			code: None,
		}
	}

	#[must_use]
	pub fn error_code(
		id: Option<&str>,
		command: &str,
		message: impl Into<String>,
		code: &str,
	) -> Self {
		Self { code: Some(code.to_string()), ..Self::error(id, command, message) }
	}

	/// JSON object for the encoder. Serialization of this shape cannot fail.
	#[must_use]
	pub fn into_value(self) -> Value {
		serde_json::to_value(self).unwrap_or_else(|_| {
			json!({
				"type": "response",
				"command": "unknown",
				"success": false,
				"error": "RPC response could not be serialized",
			})
		})
	}
}

/// `ready` frame sent before any command is read.
#[must_use]
pub fn ready_frame(max_frame_bytes: usize, max_reassembled_bytes: usize) -> Value {
	json!({
		"type": "ready",
		"protocolVersion": PROTOCOL_VERSION,
		"supportedProtocolVersions": SUPPORTED_PROTOCOL_VERSIONS,
		"maxFrameBytes": max_frame_bytes,
		"maxReassembledFrameBytes": max_reassembled_bytes,
	})
}

/// `RpcSessionState` payload for `get_state`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcSessionState {
	#[serde(skip_serializing_if = "Option::is_none")]
	pub model:                    Option<Value>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub thinking_level:           Option<String>,
	pub is_streaming:             bool,
	pub is_compacting:            bool,
	pub steering_mode:            &'static str,
	pub follow_up_mode:           &'static str,
	pub interrupt_mode:           &'static str,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub session_file:             Option<String>,
	pub session_id:               String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub session_name:             Option<String>,
	pub auto_compaction_enabled:  bool,
	pub queued_message_count:     usize,
	pub todo_phases:              Value,
	pub fast_mode_enabled:        bool,
	pub tokens_per_second:        Option<f64>,
	pub fast_mode_active:         bool,
	pub message_count:            usize,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub system_prompt:            Option<Vec<String>>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub dump_tools:               Option<Vec<Value>>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub context_usage:            Option<Value>,
}

/// `extension_ui_response` frame (side-channel, overtakes the serial queue).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RpcExtensionUiResponse {
	pub id:        String,
	#[serde(default)]
	pub value:     Option<String>,
	#[serde(default)]
	pub confirmed: Option<bool>,
	#[serde(default)]
	pub cancelled: bool,
	#[serde(default)]
	pub timed_out: Option<bool>,
}

/// `host_tool_result` frame.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RpcHostToolResult {
	pub id:     String,
	pub result: Value,
	#[serde(default, rename = "isError")]
	pub is_error: bool,
}

/// `host_tool_update` frame.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RpcHostToolUpdate {
	pub id:             String,
	#[serde(rename = "partialResult")]
	pub partial_result: Value,
}

/// `host_uri_result` frame.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcHostUriResult {
	pub id:           String,
	#[serde(default)]
	pub content:      Option<String>,
	#[serde(default)]
	pub content_type: Option<String>,
	#[serde(default)]
	pub notes:        Option<Vec<String>>,
	#[serde(default)]
	pub immutable:    Option<bool>,
	#[serde(default)]
	pub is_error:     bool,
	#[serde(default)]
	pub error:        Option<String>,
}

/// Frames that must overtake the serialized command queue.
#[derive(Debug, Clone, PartialEq)]
pub enum RpcControlFrame {
	ExtensionUi(RpcExtensionUiResponse),
	HostToolResult(RpcHostToolResult),
	HostToolUpdate(RpcHostToolUpdate),
	HostUriResult(RpcHostUriResult),
}

/// Parse a stdin frame as a side-channel frame, mirroring the upstream guards
/// (`isRpcExtensionUIResponse`, `isRpcHostToolResult`, `isRpcHostToolUpdate`,
/// `isRpcHostUriResult`).
#[must_use]
pub fn parse_control_frame(value: &Value) -> Option<RpcControlFrame> {
	let Some(kind) = value.get("type").and_then(Value::as_str) else {
		return None;
	};
	match kind {
		"extension_ui_response" => {
			let id = value.get("id").and_then(Value::as_str)?;
			if id.is_empty() {
				return None;
			}
			serde_json::from_value::<RpcExtensionUiResponse>(value.clone())
				.ok()
				.map(RpcControlFrame::ExtensionUi)
		},
		"host_tool_result" => {
			let frame = serde_json::from_value::<RpcHostToolResult>(value.clone()).ok()?;
			is_agent_tool_result(&frame.result).then_some(RpcControlFrame::HostToolResult(frame))
		},
		"host_tool_update" => {
			let frame = serde_json::from_value::<RpcHostToolUpdate>(value.clone()).ok()?;
			is_agent_tool_result(&frame.partial_result).then_some(RpcControlFrame::HostToolUpdate(frame))
		},
		"host_uri_result" => serde_json::from_value::<RpcHostUriResult>(value.clone())
			.ok()
			.map(RpcControlFrame::HostUriResult),
		_ => None,
	}
}

/// `AgentToolResult` guard: a record whose `content` is an array.
#[must_use]
pub fn is_agent_tool_result(value: &Value) -> bool {
	value
		.as_object()
		.and_then(|object| object.get("content"))
		.is_some_and(Value::is_array)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn command_field_spellings_match_the_typescript_contract() {
		let command: RpcCommand = serde_json::from_value(json!({
			"id": "c1",
			"type": "prompt",
			"message": "hi",
			"streamingBehavior": "followUp",
			"images": [{ "type": "image", "data": "aGk=", "mimeType": "image/png" }],
		}))
		.expect("prompt parses");
		let RpcCommand::Prompt { id, message, images, streaming_behavior } = command else {
			panic!("wrong variant");
		};
		assert_eq!(id.as_deref(), Some("c1"));
		assert_eq!(message, "hi");
		assert_eq!(streaming_behavior, Some(StreamingBehavior::FollowUp));
		assert_eq!(
			images.expect("images"),
			[RpcImage { data: "aGk=".to_string(), mime_type: "image/png".to_string() }]
		);
	}

	#[test]
	fn camel_case_command_fields_parse() {
		let command: RpcCommand = serde_json::from_value(json!({
			"type": "get_subagent_messages",
			"subagentId": "s1",
			"sessionFile": "/tmp/a.jsonl",
			"fromByte": 12,
		}))
		.expect("parses");
		let RpcCommand::GetSubagentMessages { subagent_id, session_file, from_byte, .. } = command else {
			panic!("wrong variant");
		};
		assert_eq!(subagent_id.as_deref(), Some("s1"));
		assert_eq!(session_file.as_deref(), Some("/tmp/a.jsonl"));
		assert_eq!(from_byte, Some(12));
	}

	#[test]
	fn queue_mode_rejects_unknown_values() {
		assert!(serde_json::from_value::<QueueMode>(json!("one-at-a-time")).is_ok());
		assert!(serde_json::from_value::<QueueMode>(json!("sometimes")).is_err());
	}

	#[test]
	fn response_omits_absent_fields() {
		let value = RpcResponse::success(Some("1"), "abort").into_value();
		assert_eq!(value, json!({
			"id": "1",
			"type": "response",
			"command": "abort",
			"success": true,
		}));
		let value = RpcResponse::error(None, "parse", "bad").into_value();
		assert_eq!(value["success"], json!(false));
		assert!(value.get("id").is_none());
	}

	#[test]
	fn control_frame_guards_mirror_upstream() {
		assert!(matches!(
			parse_control_frame(&json!({ "type": "extension_ui_response", "id": "u1", "value": "x" })),
			Some(RpcControlFrame::ExtensionUi(_))
		));
		// `host_tool_result` needs an AgentToolResult-shaped payload.
		assert!(parse_control_frame(&json!({
			"type": "host_tool_result",
			"id": "h1",
			"result": { "content": [] }
		}))
		.is_some());
		assert!(parse_control_frame(&json!({
			"type": "host_tool_result",
			"id": "h1",
			"result": "nope"
		}))
		.is_none());
		assert!(parse_control_frame(&json!({ "type": "prompt", "message": "x" })).is_none());
	}

	#[test]
	fn every_command_type_is_recognised() {
		let types = [
			"negotiate_protocol", "prompt", "steer", "follow_up", "abort", "abort_and_prompt",
			"new_session", "get_state", "set_fast_mode", "get_available_commands", "set_todos",
			"set_host_tools", "set_host_uri_schemes", "set_subagent_subscription", "get_subagents",
			"get_subagent_messages", "set_model", "cycle_model", "get_available_models",
			"set_thinking_level", "cycle_thinking_level", "set_steering_mode", "set_follow_up_mode",
			"set_interrupt_mode", "compact", "set_auto_compaction", "set_auto_retry", "abort_retry",
			"bash", "abort_bash", "get_session_stats", "export_html", "switch_session", "branch",
			"get_branch_messages", "get_last_assistant_text", "set_session_name", "handoff",
			"get_messages", "get_messages_page", "get_login_providers", "login",
		];
		// The TypeScript union has 42 members; the guard is the count, so a new
		// upstream command cannot be silently dropped here.
		assert_eq!(types.len(), 42);
		for kind in types {
			let mut frame = json!({ "type": kind });
			let level = if kind == "set_subagent_subscription" { "events" } else { "high" };
			let mode = if kind == "set_interrupt_mode" { "immediate" } else { "all" };
			// Fill the required fields the command needs to deserialize.
			for (field, value) in [
				("message", json!("m")),
				("command", json!("echo hi")),
				("name", json!("n")),
				("provider", json!("p")),
				("modelId", json!("m")),
				("providerId", json!("p")),
				("sessionPath", json!("/tmp/s.jsonl")),
				("entryId", json!("e")),
				("level", json!(level)),
				("mode", json!(mode)),
				("enabled", json!(false)),
				("phases", json!([])),
				("tools", json!([])),
				("schemes", json!([])),
				("protocolVersion", json!(2)),
			] {
				frame[field] = value;
			}
			assert!(
				serde_json::from_value::<RpcCommand>(frame.clone()).is_ok(),
				"{kind} must deserialize: {frame}"
			);
		}
	}
}
