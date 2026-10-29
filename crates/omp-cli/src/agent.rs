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
///
/// Usage accounting rides on the embedded `pi_agent::Message`:
/// [`Event::MessageEnd`] fires exactly once per provider request, so its
/// `message.usage` is that request's report and is the only tick to sum.
/// [`Event::TurnEnd`] and [`Event::AgentEnd`] repeat the same messages (a loop
/// turn may hold several model calls), so accounting must never add them to the
/// `MessageEnd` totals. `usage: None` means the provider reported nothing.
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
	/// Partial tool output while it still runs (TS `tool_execution_update`).
	ToolUpdate {
		id: String,
		tool: String,
		args: serde_json::Value,
		partial: serde_json::Value,
	},
	ToolEnd {
		id: String,
		tool: String,
		result: String,
		error: Option<String>,
		/// Validated image parts the provider request carries for this result.
		#[serde(default, skip_serializing_if = "Vec::is_empty")]
		images: Vec<pi_agent::ContentPart>,
		/// Structured result details (TS `AgentToolResult.details`).
		#[serde(default, skip_serializing_if = "Option::is_none")]
		details: Option<serde_json::Value>,
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
	/// A user message accepted from the steering channel, exactly as it entered
	/// the loop's history. Emitted after acceptance, so the session can persist
	/// it at that position and mirror it on the wire.
	UserSteer {
		parts: Vec<pi_agent::ContentPart>,
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
	/// Provider-side sticky session id (`--provider-session-id`).
	pub provider_session_id: Option<String>,
	/// Effective prompt-cache identity for this run (`--prompt-cache-key`, else
	/// a key inherited from the resumed/forked session header).
	pub prompt_cache_key: Option<String>,
	pub system_prompt: Option<String>,
	pub append_system_prompt: Option<String>,
	pub context_files: Vec<ContextFile>,
	pub skills: Vec<Skill>,
	/// Earlier turns of this session (`--continue`/`--session-id`), oldest first.
	pub history: Vec<crate::session::TurnRecord>,
	/// Exact provider-ready branch context from the session (`branch_summary`
	/// and retained rewind reports included). When set it replaces `history`
	/// as the request prefix; the live runner appends the new user message.
	pub history_messages: Option<Vec<pi_agent::Message>>,
	/// Shared checkpoint controller when `checkpoint.enabled`; tool guards and
	/// the session owner must see the same state within a run.
	pub checkpoint_controller: Option<std::sync::Arc<pi_agent::tools::CheckpointController>>,
	/// Per-turn persistence/rewind handshake channel.
	pub turn_ack: Option<std::sync::Arc<pi_agent::TurnAck>>,
	/// Todo phases restored from the session branch (TS
	/// `getLatestTodoPhasesFromEntries`): the newest `user_todo_edit` custom
	/// entry or successful `todo` tool result, whichever comes last in the log.
	pub todo_phases: Vec<pi_agent::tools::todo::TodoPhase>,
	/// Whether turns are written to a session log; tools report
	/// `details.storage` as `session` instead of `memory`.
	pub session_persisted: bool,
	/// Session log path when persistence is enabled. Eval derives its artifact
	/// directory from this file and exposes it to kernels.
	pub session_file: Option<std::path::PathBuf>,
	/// Stable session identity for eval kernel/artifact scoping. `None` lets
	/// the runner generate one process-stable id for in-memory sessions.
	pub session_id: Option<String>,
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
	/// Host-provided tools mounted into the per-turn registry after every
	/// builtin/MCP/ask/checkpoint/eval tool is in place. A name that is already
	/// registered — or repeated in this batch — is a hard error, never a silent
	/// replacement of a builtin.
	pub extra_tools: Vec<ExtraTool>,
	/// Session internal URL router: the RPC host `scheme://` handlers plus any
	/// scheme the runner installs. Shared with child agents so a child resolves
	/// exactly what the root does; `None` disables every non-filesystem source.
	pub internal_urls: Option<std::sync::Arc<pi_agent::tools::internal_urls::InternalUrlRouter>>,
	/// Immutable URL scope for this run (session file + `local://` root). The
	/// runner copies it into each tool call, so concurrent turns and children
	/// never share or overwrite one.
	pub url_scope: Option<std::sync::Arc<pi_agent::tools::internal_urls::UrlScope>>,
	/// Subagent event observer the runner subscribes once on its lazily built
	/// supervisor; a replaced supervisor reuses the same handle.
	pub subagent_observer: Option<ObserverHandle>,
	/// Directory child transcripts are written to; `None` leaves
	/// `get_subagent_messages` without a file to page.
	pub child_session_root: Option<std::path::PathBuf>,
}

/// Observer handle that keeps [`RunRequest`]'s derived `Debug` intact: the
/// trait object itself is not printable, the handle names only that one was
/// installed.
#[derive(Clone)]
pub struct ObserverHandle(pub std::sync::Arc<dyn pi_agent::supervisor::SubagentObserver>);

impl std::fmt::Debug for ObserverHandle {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("ObserverHandle")
	}
}

/// One host tool handed to the runner.
///
/// `Arc<dyn Tool>` is neither `Debug` nor name-aware at the field level, so the
/// wrapper supplies both: `RunRequest` stays `Debug` and a conflict message can
/// name the tool.
#[derive(Clone)]
pub struct ExtraTool(pub std::sync::Arc<dyn pi_agent::tools::Tool>);

impl std::fmt::Debug for ExtraTool {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_tuple("ExtraTool").field(&self.0.name()).finish()
	}
}

impl From<std::sync::Arc<dyn pi_agent::tools::Tool>> for ExtraTool {
	fn from(tool: std::sync::Arc<dyn pi_agent::tools::Tool>) -> Self {
		Self(tool)
	}
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
