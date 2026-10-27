//! Agent-runner seam.
//!
//! The binary registers the `pi-agent` implementation through [`install`]
//! (`runtime::PiAgentRunner`). A library caller that never registers one gets
//! `None` from [`runner`] and must fail with `agent runtime not linked` — there
//! is no stand-in that fabricates answers.

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use serde::Serialize;

use crate::{
	Result, approve::Approvals, catalog::Catalog, config::Config, output::OutputMode,
	prompts::ContextFile, skills::Skill,
};

pub const MISSING_RUNTIME: &str = "agent runtime not linked";

/// One streamed event of an agent turn.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
	MessageStart {
		role: String,
	},
	MessageUpdate {
		delta: String,
	},
	MessageEnd {
		message: pi_agent::Message,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		stop_reason: Option<String>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		api: Option<String>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		provider: Option<String>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		model: Option<String>,
	},
	ToolStart {
		id: String,
		tool: String,
		args: serde_json::Value,
	},
	ToolEnd {
		id: String,
		tool: String,
		result: String,
		error: Option<String>,
	},
	TurnEnd {
		message: pi_agent::Message,
		tool_results: Vec<pi_agent::ToolResult>,
		stop_reason: String,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		api: Option<String>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		provider: Option<String>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		model: Option<String>,
	},
	AgentEnd {
		messages: Vec<pi_agent::Message>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		api: Option<String>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		provider: Option<String>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		model: Option<String>,
	},
}

impl Event {
	pub fn message_end(message: pi_agent::Message) -> Self {
		let stop = if message.tool_calls.is_empty() { "stop" } else { "toolUse" };
		Self::MessageEnd {
			message,
			stop_reason: Some(stop.to_string()),
			api: None,
			provider: None,
			model: None,
		}
	}

	pub fn turn_end(
		message: pi_agent::Message,
		tool_results: Vec<pi_agent::ToolResult>,
		stop_reason: impl Into<String>,
	) -> Self {
		Self::TurnEnd {
			message,
			tool_results,
			stop_reason: stop_reason.into(),
			api: None,
			provider: None,
			model: None,
		}
	}

	pub fn agent_end(messages: Vec<pi_agent::Message>) -> Self {
		Self::AgentEnd { messages, api: None, provider: None, model: None }
	}
}

pub type EventStream = Box<dyn Iterator<Item = Result<Event>> + Send>;

/// Everything one turn needs. The runner owns retries, network and tools; the
/// CLI only resolves files into this request.
#[derive(Debug, Clone)]
pub struct RunRequest {
	pub prompt: String,
	pub parts: Vec<crate::session::ContentPart>,
	pub cwd: std::path::PathBuf,
	pub output_mode: OutputMode,
	pub model: Option<String>,
	pub provider: Option<String>,
	pub api_key: Option<String>,
	pub system_prompt: Option<String>,
	pub append_system_prompt: Option<String>,
	pub context_files: Vec<ContextFile>,
	pub skills: Vec<Skill>,
	/// Earlier turns of this session (`--continue`/`--session-id`), oldest first.
	pub history: Vec<crate::session::TurnRecord>,
	pub config: Config,
	pub catalog: Arc<Catalog>,
	pub approvals: Approvals,
	pub max_time: Option<Duration>,
	/// Assistant-turn budget handed to the agent loop.
	pub max_turns: usize,
	/// `false` when `--no-tools` was passed.
	pub tools_enabled: bool,
	/// Optional cancellation trigger from the consumer.
	pub cancellation_token: Option<pi_agent::CancellationToken>,
}

impl RunRequest {
	#[must_use]
	pub fn has_image(&self) -> bool {
		self.parts.iter().any(crate::session::ContentPart::is_image)
	}
}

pub trait AgentRunner: Send + Sync {
	fn run(&self, request: &RunRequest) -> Result<EventStream>;
}

static RUNNER: OnceLock<Box<dyn AgentRunner>> = OnceLock::new();

/// Registration hook for the `pi-agent` integration.
///
/// ```ignore
/// omp_cli::agent::install(Box::new(pi_agent::OmpRunner::new()));
/// ```
pub fn install(runner: Box<dyn AgentRunner>) -> std::result::Result<(), Box<dyn AgentRunner>> {
	RUNNER.set(runner)
}

#[must_use]
pub fn runner() -> Option<&'static dyn AgentRunner> {
	RUNNER.get().map(|boxed| &**boxed)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn event_json_matches_print_mode_shape() {
		let json = serde_json::to_string(&Event::MessageUpdate { delta: "hi".into() }).expect("json");
		assert_eq!(json, r#"{"type":"message_update","delta":"hi"}"#);
		let end = serde_json::to_string(&Event::TurnEnd {
			message: pi_agent::Message::assistant("ok"),
			tool_results: Vec::new(),
			stop_reason: "end_turn".into(),
			api: None,
			provider: None,
			model: None,
		})
		.expect("json");
		assert!(end.contains(r#""type":"turn_end""#));
		assert!(end.contains(r#""stop_reason":"end_turn""#));
	}

	#[test]
	fn no_runner_is_registered_by_default() {
		// Unit tests are compiled without the pi-agent integration.
		assert!(runner().is_none());
	}
}
