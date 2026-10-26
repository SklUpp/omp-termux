use std::{collections::HashMap, path::PathBuf};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// Role of a message in context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
	System,
	User,
	Assistant,
	Tool,
}

/// A tool invocation request by model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
	pub id: String,
	pub name: String,
	pub arguments: serde_json::Value,
}

/// A tool execution response.
///
/// Tools fill `content`/`is_error` and leave `tool_call_id` empty; the scheduler
/// stamps the id of the call it ran, so a tool cannot mislabel its own result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
	pub tool_call_id: String,
	pub content: String,
	#[serde(default)]
	pub is_error: bool,
}

/// Completion status of an assistant turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
	EndTurn,
	ToolUse,
	Aborted,
	Error(String),
}

/// A single message in the agent dialog history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
	pub role: Role,
	pub content: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub name: Option<String>,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub tool_calls: Vec<ToolCall>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub tool_call_id: Option<String>,
	#[serde(default, skip_serializing_if = "std::ops::Not::not")]
	pub is_error: bool,
}

impl Message {
	pub fn system(content: impl Into<String>) -> Self {
		Self {
			role: Role::System,
			content: content.into(),
			name: None,
			tool_calls: Vec::new(),
			tool_call_id: None,
			is_error: false,
		}
	}

	pub fn user(content: impl Into<String>) -> Self {
		Self {
			role: Role::User,
			content: content.into(),
			name: None,
			tool_calls: Vec::new(),
			tool_call_id: None,
			is_error: false,
		}
	}

	pub fn assistant(content: impl Into<String>) -> Self {
		Self {
			role: Role::Assistant,
			content: content.into(),
			name: None,
			tool_calls: Vec::new(),
			tool_call_id: None,
			is_error: false,
		}
	}

	pub fn assistant_with_tools(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
		Self {
			role: Role::Assistant,
			content: content.into(),
			name: None,
			tool_calls,
			tool_call_id: None,
			is_error: false,
		}
	}

	pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
		Self {
			role: Role::Tool,
			content: content.into(),
			name: None,
			tool_calls: Vec::new(),
			tool_call_id: Some(tool_call_id.into()),
			is_error: false,
		}
	}

	pub fn tool_with_status(
		tool_call_id: impl Into<String>,
		name: impl Into<String>,
		content: impl Into<String>,
		is_error: bool,
	) -> Self {
		Self {
			role: Role::Tool,
			content: content.into(),
			name: Some(name.into()),
			tool_calls: Vec::new(),
			tool_call_id: Some(tool_call_id.into()),
			is_error,
		}
	}
}

/// Agent configuration parameters.
///
/// Everything frequently edited (system prompt, model id, limits) is supplied by
/// the caller from config/data files, never baked into this crate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
	pub max_turns: usize,
	pub context_limit: usize,
	pub system_prompt: Option<String>,
	pub cwd: PathBuf,
}

/// Shared context accessible to tools and execution loop.
#[derive(Debug, Clone)]
pub struct AgentContext {
	pub cwd: PathBuf,
	pub env: HashMap<String, String>,
	pub cancellation_token: CancellationToken,
}

impl AgentContext {
	pub fn new(cwd: PathBuf) -> Self {
		Self { cwd, env: std::env::vars().collect(), cancellation_token: CancellationToken::new() }
	}

	pub fn with_cancellation(cwd: PathBuf, cancellation_token: CancellationToken) -> Self {
		Self { cwd, env: std::env::vars().collect(), cancellation_token }
	}
}

/// Persistent state of an agent session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentState {
	pub messages: Vec<Message>,
	pub total_turns: usize,
}

/// Result of a single turn execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnResult {
	pub message: Message,
	pub tool_results: Vec<ToolResult>,
	pub is_completed: bool,
}
