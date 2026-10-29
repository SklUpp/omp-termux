//! Host seam for eval bridge calls.
//!
//! The parent owns registry, supervisor, model, session, and settings
//! integration. Every capability the runtimes reach (tools, completion,
//! subagents, fan-out width, budget, artifact spill, background delivery) is an
//! explicit required method here; the eval module never fabricates a result.

use async_trait::async_trait;
use serde_json::{Map, Value};

use crate::{
	state::{AgentContext, ToolResult},
	tools::ToolError,
};

use super::types::EvalStatusEvent;

/// Synthetic bridge name for `completion()`.
pub const EVAL_COMPLETION_BRIDGE_NAME: &str = "__completion__";
/// Synthetic bridge name for `agent()`.
pub const EVAL_AGENT_BRIDGE_NAME: &str = "__agent__";
/// Synthetic bridge name for `parallel()`/`pipeline()` width discovery.
pub const EVAL_CONCURRENCY_BRIDGE_NAME: &str = "__concurrency__";
/// Synthetic bridge name for `budget`.
pub const EVAL_BUDGET_BRIDGE_NAME: &str = "__budget__";
/// Field injected into bridged tool args, matching the JS/Ruby/Python preludes.
pub const INTENT_FIELD: &str = "i";
/// Tools that only work as direct tool-result messages.
pub const BRIDGE_REJECTED_TOOLS: &[&str] = &["checkpoint", "rewind"];

/// Resolve a model-invisible bridge call.
///
/// Cancellation contract: every async host future MUST resolve once
/// `ctx.cancellation_token` fires. Dispatch is not wrapped in a bridge timeout;
/// the cell watchdog stays paused and `agent()` keeps the external-abort shield
/// raised until the future returns. A host call that ignores cancellation
/// therefore stalls the eval cell indefinitely.
#[async_trait]
pub trait EvalHost: Send + Sync {
	/// Execute a normal registered session tool. `ctx` carries cwd, env, the
	/// turn cancellation token, and approval surface.
	async fn call_tool(&self, name: &str, args: Value, ctx: &AgentContext) -> Result<ToolResult, ToolError>;

	/// Stateless, tool-free one-shot completion. Returns the prelude value shape
	/// (`{"text": ...}` or parsed structured data).
	async fn completion(&self, args: Value, ctx: &AgentContext) -> Result<Value, String>;

	/// Run one structured subagent through the parent's supervisor.
	async fn agent(&self, args: Value, ctx: &AgentContext) -> Result<Value, String>;

	/// Live Goal Mode token budget snapshot.
	async fn budget(&self, args: Value, ctx: &AgentContext) -> Result<Value, String>;

	/// Current fan-out width for `parallel()`/`pipeline()` (`task.maxConcurrency`;
	/// `0` means unbounded).
	fn concurrency(&self, args: &Value) -> Result<Value, String>;

	/// Persist full cell output as an artifact. `None` means artifacts are not
	/// available for this session.
	async fn write_artifact(&self, kind: &str, bytes: &[u8]) -> Result<Option<ArtifactRef>, String>;

	/// Enqueue a finished background cell for exactly-once delivery at a turn
	/// boundary. The parent wires this to its existing result sink/supervisor.
	/// Returns `false` when background delivery is unavailable, in which case the
	/// cell must settle in the foreground.
	fn enqueue_background_result(&self, job_id: &str, text: String) -> bool;
}

/// Stored artifact identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRef {
	pub id: String,
	pub path: Option<String>,
}

/// Host seam that mounts an eval tool into a registry.
///
/// Root runs bind once after the final registry exists; child registries bind
/// from [`crate::supervisor::Supervisor::child_registry_unbound`] before the
/// allowlist filter (so `eval` is filtered by internal key) and again after
/// `rebind_session_tools`, so the host's tool-call view sees the child's fresh
/// session tools. `dispose_owner` releases retained child kernels.
#[async_trait]
pub trait EvalToolBinder: Send + Sync {
	fn bind(&self, registry: &mut crate::tools::registry::ToolRegistry, owner: Option<&str>);

	/// Rebind `owner`'s eval host with the exact provider its agent loop
	/// resolved. Children must override the binder's session provider so
	/// `completion(model = "default")` answers with the child model, not the
	/// root model. The default keeps the binder's own provider.
	fn bind_with_provider(
		&self,
		registry: &mut crate::tools::registry::ToolRegistry,
		owner: Option<&str>,
		_provider: Option<std::sync::Arc<dyn crate::provider::ModelProvider>>,
	) {
		self.bind(registry, owner);
	}

	async fn dispose_owner(&self, _owner: &str) {}
}

/// Stable status summary for one bridged tool call (TypeScript
/// `summarizeToolResult`). Error results include a 500-character slice.
pub fn summarize_tool_result(name: &str, args: &Value, result: &ToolResult) -> EvalStatusEvent {
	let record = args.as_object();
	let details = result.details.as_ref().and_then(Value::as_object);
	// Upstream reports foreground command execution as `op: "run"`. Only include
	// count/code/matches when the Rust tool actually emitted those detail keys;
	// grep/glob/bash currently return `details: None`, so no numbers are invented.
	let op = if name == "bash" { "run" } else { name };
	let mut event = EvalStatusEvent::new(op);
	let mut field = |key: &str, value: Value| {
		event.fields.insert(key.to_string(), value);
	};
	match name {
		"read" => {
			if let Some(path) = record.and_then(|record| record.get("path")) {
				field("path", path.clone());
			}
			field("chars", Value::from(result.content.len()));
			field("preview", Value::String(result.content.chars().take(500).collect()));
		},
		"write" => {
			if let Some(path) = record.and_then(|record| record.get("path")) {
				field("path", path.clone());
			}
			let chars = record
				.and_then(|record| record.get("content"))
				.and_then(Value::as_str)
				.map_or(0, str::len);
			field("chars", Value::from(chars));
		},
		"grep" => {
			for key in ["pattern", "path"] {
				if let Some(value) = record.and_then(|record| record.get(key)) {
					field(key, value.clone());
				}
			}
			if let Some(count) = details.and_then(|details| details.get("matchCount")) {
				field("count", count.clone());
			}
		},
		"glob" => {
			if let Some(pattern) = record.and_then(|record| record.get("pattern")) {
				field("pattern", pattern.clone());
			}
			if let Some(count) = details.and_then(|details| details.get("fileCount")) {
				field("count", count.clone());
			}
			if let Some(files) = details.and_then(|details| details.get("files")).and_then(Value::as_array) {
				field("matches", Value::Array(files.iter().take(20).cloned().collect()));
			}
		},
		"bash" => {
			if let Some(command) = record.and_then(|record| record.get("command")) {
				field("cmd", command.clone());
			}
			if let Some(code) = details.and_then(|details| details.get("exitCode")) {
				field("code", code.clone());
			}
			field("output", Value::String(result.content.chars().take(500).collect()));
		},
		_ => {
			field("chars", Value::from(result.content.len()));
		},
	}
	if result.is_error {
		event.fields.insert("hasError".to_string(), Value::Bool(true));
		event
			.fields
			.insert("error".to_string(), Value::String(result.content.chars().take(500).collect()));
	}
	event
}

/// Reject bridge-only tools that require a direct tool-result message.
pub fn reject_bridged_tool(name: &str) -> Option<ToolError> {
	BRIDGE_REJECTED_TOOLS.contains(&name).then(|| {
		ToolError::ExecutionFailed(format!(
			"`{name}` cannot run through the eval bridge; call the direct `{name}` tool."
		))
	})
}

/// Normalize bridged arguments and stamp the intent field.
pub fn normalize_bridge_args(args: Value, intent: &str) -> Value {
	match args {
		Value::Object(mut record) => {
			if !record.contains_key(INTENT_FIELD) {
				record.insert(INTENT_FIELD.to_string(), Value::String(intent.to_string()));
			}
			Value::Object(record)
		},
		other => other,
	}
}

/// Object-shaped tool value returned to a kernel bridge caller.
pub fn tool_value(result: ToolResult, images: Vec<Value>) -> Value {
	let mut value = Map::new();
	value.insert("text".to_string(), Value::String(result.content));
	if let Some(details) = result.details {
		value.insert("details".to_string(), details);
	}
	if !images.is_empty() {
		value.insert("images".to_string(), Value::Array(images));
	}
	if result.is_error {
		value.insert("hasError".to_string(), Value::Bool(true));
	}
	Value::Object(value)
}
