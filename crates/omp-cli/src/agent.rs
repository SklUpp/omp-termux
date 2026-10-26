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
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
	MessageStart { role: String },
	MessageUpdate { delta: String },
	MessageEnd { text: String },
	ToolStart { id: String, tool: String, args: serde_json::Value },
	ToolEnd { id: String, tool: String, result: String, error: Option<String> },
	TurnEnd { stop_reason: String },
	AgentEnd { text: String },
}

pub type EventStream = Box<dyn Iterator<Item = Result<Event>> + Send>;

/// Everything one turn needs. The runner owns retries, network and tools; the
/// CLI only resolves files into this request.
#[derive(Debug, Clone)]
pub struct RunRequest {
	pub prompt: String,
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
		let end =
			serde_json::to_string(&Event::TurnEnd { stop_reason: "end_turn".into() }).expect("json");
		assert_eq!(end, r#"{"type":"turn_end","stop_reason":"end_turn"}"#);
	}

	#[test]
	fn no_runner_is_registered_by_default() {
		// Unit tests are compiled without the pi-agent integration.
		assert!(runner().is_none());
	}
}
