//! `checkpoint`/`rewind`: conversation-only context checkpointing.
//!
//! Upstream anchors: `packages/coding-agent/src/tools/checkpoint.ts`,
//! `packages/coding-agent/src/session/agent-session.ts`
//! (`#checkpointState`/`#pendingRewindReport`/`#applyRewind`),
//! `packages/coding-agent/src/session/checkpoint-entries.ts`.
//!
//! A checkpoint records only conversation metadata: the goal, the persisted
//! checkpoint tool-result entry id, the message count at that point, and a
//! timestamp. There is **no** git, worktree, or filesystem snapshot. `rewind`
//! only requests the cut; the session layer defers the actual branch to
//! turn-end, appends a `branch_summary` plus a retained `rewind-report`
//! custom message, and moves the persisted leaf back to the checkpoint entry.
//! Abandoned JSONL entries stay in the log untouched.
//!
//! State lives in an [`CheckpointController`] shared by both tools and the
//! session layer. A successful `checkpoint` records the live checkpoint as
//! soon as the tool succeeds (entry id backfilled later), so an immediate
//! `rewind` sees it; the session layer then calls
//! [`CheckpointController::note_checkpoint`] again with the exact message count
//! and persisted entry id, and
//! [`CheckpointController::note_rewind_tool_result`] once the rewind result is
//! persisted. The session owner captures those results and answers the loop's
//! turn-end [`crate::TurnAck`] with replacement messages; this module never
//! touches persistence.

use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Tool, ToolError};
use crate::state::{AgentContext, ToolResult};

/// Tool names, also used by the session layer to recognise results.
pub const CHECKPOINT_TOOL_NAME: &str = "checkpoint";
pub const REWIND_TOOL_NAME: &str = "rewind";
/// Persisted custom-message type carrying the retained rewind report.
pub const REWIND_REPORT_CUSTOM_TYPE: &str = "rewind-report";
/// Persisted transient custom-message type nudging the next turn to `rewind`.
pub const CHECKPOINT_ACTIVE_REMINDER_TYPE: &str = "checkpoint-active-reminder";

/// Model-facing description, mirroring `prompts/tools/checkpoint.md`.
pub const CHECKPOINT_DESCRIPTION: &str = "Context checkpoint: before exploratory work; later `rewind`, retaining only concise report.

Use for investigations with many intermediate tool calls (`read`/`grep`/`glob`/`lsp`/etc.) to minimize subsequent context cost.

Rules:
- MUST `rewind` before yielding after starting a checkpoint.
- NEVER `checkpoint` while another checkpoint active.

Typical flow:
1. `checkpoint(goal: …)`
2. Exploratory work
3. `rewind(report: …)` with concise findings

After `rewind`: intermediate checkpoint messages removed from active context; replaced by report.";

/// Model-facing description, mirroring `prompts/tools/rewind.md`.
pub const REWIND_DESCRIPTION: &str =
	"End the active checkpoint; rewind context to it, replacing intermediate exploration with your report.";

/// Transient notice appended after a successful checkpoint tool result.
pub const CHECKPOINT_ACTIVE_NOTICE: &str = "<system-notice>
Exploration checkpoint active.
- MUST `rewind` with findings once exploration is done.
- MUST `rewind` before yielding.
</system-notice>";

/// Developer warning injected when the agent yields with an open checkpoint.
pub const REWIND_BEFORE_YIELD_WARNING: &str = "<system-warning>
You are in an active checkpoint. You MUST call rewind with your investigation findings before yielding. Do NOT yield without completing the checkpoint.
</system-warning>";

/// Marker inside the persisted retained report content (upstream
/// `checkpoint-entries.ts#reportFromRewindReportContent`).
const REPORT_MARKER: &str = "\nReport:\n";

/// Active checkpoint boundary (upstream `CheckpointState`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointState {
	/// Investigation goal echoed by `checkpoint`; never trimmed.
	pub goal: String,
	/// `AgentState.messages.len()` right after the checkpoint tool result.
	pub checkpoint_message_count: usize,
	/// Persisted checkpoint tool-result entry id; `None` until captured.
	pub checkpoint_entry_id: Option<String>,
	/// Provider tool-call id of the successful checkpoint result. No-session
	/// rewinds locate the cut in the uncompacted mirror by this id instead of a
	/// live message count.
	#[serde(default)]
	pub tool_call_id: Option<String>,
	pub started_at: String,
}

/// Completed rewind retained for the rest of the branch (upstream
/// `CompletedRewindState`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletedRewindState {
	pub report: String,
	pub started_at: String,
	pub rewound_at: String,
}

/// Snapshot handed to the session layer so it can perform the deferred cut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRewind {
	pub checkpoint_entry_id: Option<String>,
	pub checkpoint_tool_call_id: Option<String>,
	pub report: String,
	pub started_at: String,
}

#[derive(Debug, Default)]
struct Inner {
	checkpoint: Option<CheckpointState>,
	pending_report: Option<String>,
	completed: Option<CompletedRewindState>,
}

/// Shared checkpoint/rewind state machine.
#[derive(Debug, Default)]
pub struct CheckpointController {
	inner: Mutex<Inner>,
}

impl CheckpointController {
	#[must_use]
	pub fn new() -> Self {
		Self::default()
	}

	fn lock(&self) -> MutexGuard<'_, Inner> {
		self.inner.lock().unwrap_or_else(|poison| poison.into_inner())
	}

	#[must_use]
	pub fn checkpoint_state(&self) -> Option<CheckpointState> {
		self.lock().checkpoint.clone()
	}

	#[must_use]
	pub fn last_completed_rewind(&self) -> Option<CompletedRewindState> {
		self.lock().completed.clone()
	}

	#[must_use]
	pub fn pending_rewind_report(&self) -> Option<String> {
		self.lock().pending_report.clone()
	}

	/// Whether an open checkpoint exists without a captured rewind report.
	/// Drives the pre-yield guard (upstream `#enforceRewindBeforeYield`).
	#[must_use]
	pub fn needs_rewind_before_yield(&self) -> bool {
		let inner = self.lock();
		inner.checkpoint.is_some() && inner.pending_report.is_none()
	}

	/// Capture a successful `checkpoint` result. Clears any pending report and
	/// the retained completed rewind, exactly like upstream's success event.
	#[must_use]
	pub fn note_checkpoint(
		&self,
		goal: impl Into<String>,
		started_at: impl Into<String>,
		checkpoint_message_count: usize,
	) -> CheckpointState {
		let state = CheckpointState {
			goal: goal.into(),
			checkpoint_message_count,
			checkpoint_entry_id: None,
			tool_call_id: None,
			started_at: started_at.into(),
		};
		let mut inner = self.lock();
		inner.checkpoint = Some(state.clone());
		inner.pending_report = None;
		inner.completed = None;
		state
	}

	/// Backfill the persisted entry id after the tool-result entry is written.
	/// Returns `false` when no checkpoint is active.
	pub fn backfill_checkpoint_entry_id(&self, entry_id: impl Into<String>) -> bool {
		let mut inner = self.lock();
		match &mut inner.checkpoint {
			Some(state) => {
				state.checkpoint_entry_id = Some(entry_id.into());
				true
			},
			None => false,
		}
	}

	/// Backfill the provider tool-call id used by no-session mirrors.
	pub fn backfill_checkpoint_tool_call_id(&self, tool_call_id: impl Into<String>) -> bool {
		let mut inner = self.lock();
		match &mut inner.checkpoint {
			Some(state) => {
				state.tool_call_id = Some(tool_call_id.into());
				true
			},
			None => false,
		}
	}

	/// Capture a successful `rewind` result report. Ignored without an active
	/// checkpoint or for an empty report.
	pub fn note_rewind_tool_result(&self, report: &str) -> bool {
		let report = report.trim();
		if report.is_empty() {
			return false;
		}
		let mut inner = self.lock();
		if inner.checkpoint.is_none() {
			return false;
		}
		inner.pending_report = Some(report.to_string());
		true
	}

	/// Drop a captured report without completing (rollback of an unsettled
	/// no-session rewind). The active checkpoint stays open.
	pub fn clear_pending_rewind_report(&self) {
		self.lock().pending_report = None;
	}

	/// Snapshot the deferred cut. Does not clear state: the caller completes it
	/// via [`Self::complete_rewind`] once the branch and report entry persist.
	#[must_use]
	pub fn pending_rewind(&self) -> Option<PendingRewind> {
		let inner = self.lock();
		let checkpoint = inner.checkpoint.as_ref()?;
		Some(PendingRewind {
			checkpoint_entry_id: checkpoint.checkpoint_entry_id.clone(),
			checkpoint_tool_call_id: checkpoint.tool_call_id.clone(),
			report: inner.pending_report.clone()?,
			started_at: checkpoint.started_at.clone(),
		})
	}

	/// Finish the deferred cut: retain the completed report and clear the active
	/// checkpoint plus pending report.
	#[must_use]
	pub fn complete_rewind(&self, rewound_at: impl Into<String>) -> Option<CompletedRewindState> {
		let mut inner = self.lock();
		let checkpoint = inner.checkpoint.take()?;
		let report = inner.pending_report.take()?;
		let completed = CompletedRewindState {
			report,
			started_at: checkpoint.started_at,
			rewound_at: rewound_at.into(),
		};
		inner.completed = Some(completed.clone());
		Some(completed)
	}

	/// Drop all runtime checkpoint state (session switch/reset).
	pub fn clear(&self) {
		let mut inner = self.lock();
		inner.checkpoint = None;
		inner.pending_report = None;
		inner.completed = None;
	}

	/// Warning text to inject before yielding, if a checkpoint is still open.
	#[must_use]
	pub fn yield_warning(&self) -> Option<&'static str> {
		self.needs_rewind_before_yield().then_some(REWIND_BEFORE_YIELD_WARNING)
	}

	/// Rebuild state from the active persisted branch (upstream
	/// `#rehydrateCheckpointRewindState`). Entries are the serialized
	/// `SessionEntry` values on the branch, oldest first.
	pub fn rehydrate(&self, entries: &[Value]) {
		let mut completed: Option<CompletedRewindState> = None;
		let mut pending: Option<CheckpointState> = None;
		let mut message_count = 0usize;

		for entry in entries {
			if entry.get("type").and_then(Value::as_str) == Some("message") {
				message_count += 1;
			}
			if let Some(started_at) = checkpoint_started_at(entry) {
				completed = None;
				pending = Some(CheckpointState {
					goal: checkpoint_goal(entry).unwrap_or_default(),
					checkpoint_message_count: message_count,
					checkpoint_entry_id: entry
						.get("id")
						.and_then(Value::as_str)
						.filter(|id| !id.is_empty())
						.map(str::to_string),
					tool_call_id: None,
					started_at,
				});
				continue;
			}
			if let Some(rewind) = completed_rewind_from_entry(entry) {
				completed = Some(rewind);
				pending = None;
			}
		}

		let mut inner = self.lock();
		inner.checkpoint = pending;
		inner.pending_report = None;
		inner.completed = if inner.checkpoint.is_some() { None } else { completed };
	}
}

/// Render the retained rewind report content (upstream
/// `prompts/system/rewind-report.md`).
#[must_use]
pub fn render_rewind_report(report: &str) -> String {
	format!("Checkpoint called and rewound. Report retained below. Need explore again → new `checkpoint`.\n\nReport:\n{report}")
}

/// Render a `branch_summary` entry as LLM context (upstream
/// `agent/compaction/prompts/branch-summary-context.md`).
#[must_use]
pub fn render_branch_summary_context(summary: &str) -> String {
	format!("Branch-return summary:\n\n<summary>\n{summary}\n</summary>")
}

/// Extract the report body from persisted `rewind-report` content.
#[must_use]
pub fn report_from_rewind_report_content(content: &str) -> String {
	match content.rfind(REPORT_MARKER) {
		Some(index) => content[index + REPORT_MARKER.len()..].trim().to_string(),
		None => content.trim().to_string(),
	}
}

fn message_property<'a>(entry: &'a Value, key: &str) -> Option<&'a Value> {
	entry.get("message").and_then(|message| message.get(key))
}

fn successful_tool_result(entry: &Value, tool: &str) -> bool {
	entry.get("type").and_then(Value::as_str) == Some("message")
		&& message_property(entry, "role").and_then(Value::as_str) == Some("toolResult")
		&& message_property(entry, "isError").and_then(Value::as_bool) != Some(true)
		&& message_property(entry, "toolName").and_then(Value::as_str) == Some(tool)
}

fn checkpoint_details(entry: &Value) -> Option<&Value> {
	message_property(entry, "details")
}

/// `startedAt` represented by a successful checkpoint entry, if any
/// (upstream `checkpointStartedAtFromEntry`).
fn checkpoint_started_at(entry: &Value) -> Option<String> {
	if !successful_tool_result(entry, CHECKPOINT_TOOL_NAME) {
		return None;
	}
	if let Some(started_at) = checkpoint_details(entry)
		.and_then(|details| details.get("startedAt"))
		.and_then(Value::as_str)
	{
		return Some(started_at.to_string());
	}
	entry
		.get("timestamp")
		.and_then(Value::as_str)
		.map(str::to_string)
}

fn checkpoint_goal(entry: &Value) -> Option<String> {
	checkpoint_details(entry)
		.and_then(|details| details.get("goal"))
		.and_then(Value::as_str)
		.map(str::to_string)
}

/// Restore a completed rewind from a persisted `rewind-report` entry
/// (upstream `completedRewindFromEntry`).
fn completed_rewind_from_entry(entry: &Value) -> Option<CompletedRewindState> {
	if entry.get("type").and_then(Value::as_str) != Some("custom_message")
		|| entry.get("customType").and_then(Value::as_str) != Some(REWIND_REPORT_CUSTOM_TYPE)
	{
		return None;
	}
	let details = entry.get("details")?;
	let started_at = details.get("startedAt").and_then(Value::as_str)?;
	let rewound_at = details.get("rewoundAt").and_then(Value::as_str)?;
	let report = details
		.get("report")
		.and_then(Value::as_str)
		.map(str::trim)
		.filter(|report| !report.is_empty())
		.map(str::to_string)
		.unwrap_or_else(|| report_from_rewind_report_content(&content_text(entry.get("content"))));
	if report.is_empty() {
		return None;
	}
	Some(CompletedRewindState {
		report,
		started_at: started_at.to_string(),
		rewound_at: rewound_at.to_string(),
	})
}

/// String form of a persisted custom-message `content` field.
fn content_text(content: Option<&Value>) -> String {
	match content {
		Some(Value::String(text)) => text.clone(),
		Some(Value::Array(parts)) => parts
			.iter()
			.filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
			.filter_map(|part| part.get("text").and_then(Value::as_str))
			.collect::<Vec<_>>()
			.join("\n"),
		_ => String::new(),
	}
}

/// RFC 3339 UTC timestamp with millisecond precision, matching the
/// `omp-cli` formatter without a chrono dependency.
#[must_use]
pub fn now_iso8601() -> String {
	let duration = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.unwrap_or_default();
	let seconds = duration.as_secs();
	let days = i64::try_from(seconds / 86_400).unwrap_or(0);
	let seconds_of_day = seconds % 86_400;
	let (year, month, day) = civil_from_days(days);
	format!(
		"{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
		seconds_of_day / 3_600,
		(seconds_of_day % 3_600) / 60,
		seconds_of_day % 60,
		duration.subsec_millis(),
	)
}

/// Howard Hinnant's days-to-civil conversion.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
	let z = days + 719_468;
	let era = z.div_euclid(146_097);
	let day_of_era = z.rem_euclid(146_097);
	let year_of_era =
		(day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
	let year = year_of_era + era * 400;
	let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
	let month_prime = (5 * day_of_year + 2) / 153;
	let day = u32::try_from(day_of_year - (153 * month_prime + 2) / 5 + 1).unwrap_or(1);
	let month = u32::try_from(if month_prime < 10 {
		month_prime + 3
	} else {
		month_prime - 9
	})
	.unwrap_or(1);
	let year = if month <= 2 { year + 1 } else { year };
	(year, month, day)
}

fn strict_object<'a>(
	args: &'a Value,
	tool: &str,
	allowed: &str,
) -> Result<&'a serde_json::Map<String, Value>, ToolError> {
	let obj = args
		.as_object()
		.ok_or_else(|| ToolError::InvalidArguments("Arguments must be an object".to_string()))?;
	for key in obj.keys() {
		if key != allowed {
			return Err(ToolError::InvalidArguments(format!(
				"Unknown parameter '{key}': {tool} accepts only '{allowed}'"
			)));
		}
	}
	Ok(obj)
}

/// `checkpoint(goal)` — marks the current conversation position.
pub struct CheckpointTool {
	controller: std::sync::Arc<CheckpointController>,
}

impl CheckpointTool {
	#[must_use]
	pub fn new(controller: std::sync::Arc<CheckpointController>) -> Self {
		Self { controller }
	}
}

#[async_trait]
impl Tool for CheckpointTool {
	fn name(&self) -> &str {
		CHECKPOINT_TOOL_NAME
	}

	fn description(&self) -> &str {
		CHECKPOINT_DESCRIPTION
	}

	fn schema(&self) -> Value {
		json!({
			"type": "object",
			"properties": {
				"goal": { "type": "string", "description": "investigation goal" }
			},
			"required": ["goal"],
			"additionalProperties": false
		})
	}

	/// Serialized so two `checkpoint` calls in one batch cannot both observe an
	/// empty state and both succeed.
	fn is_exclusive(&self) -> bool {
		true
	}

	async fn execute(&self, args: &Value, _ctx: &AgentContext) -> Result<ToolResult, ToolError> {
		let obj = strict_object(args, CHECKPOINT_TOOL_NAME, "goal")?;
		let goal = obj
			.get("goal")
			.and_then(Value::as_str)
			.ok_or_else(|| ToolError::InvalidArguments("Missing required parameter 'goal'".to_string()))?;
		if self.controller.checkpoint_state().is_some() {
			return Err(ToolError::Rejected("Checkpoint already active.".to_string()));
		}
		let started_at = now_iso8601();
		// Upstream sets the active checkpoint from the tool result as soon as it
		// is produced, so a `rewind` in the very next step finds it. The message
		// count and persisted entry id are backfilled by the session host once
		// the tool-result entry is written.
		let _ = self.controller.note_checkpoint(goal, started_at.clone(), 0);
		Ok(ToolResult {
			tool_call_id: String::new(),
			content: format!("Checkpoint: {goal}\nFinish exploration and formulate findings."),
			is_error: false,
			terminate: false,
			details: Some(json!({ "goal": goal, "startedAt": started_at })),
		})
	}
}

/// `rewind(report)` — requests the deferred context cut.
pub struct RewindTool {
	controller: std::sync::Arc<CheckpointController>,
}

impl RewindTool {
	#[must_use]
	pub fn new(controller: std::sync::Arc<CheckpointController>) -> Self {
		Self { controller }
	}
}

#[async_trait]
impl Tool for RewindTool {
	fn name(&self) -> &str {
		REWIND_TOOL_NAME
	}

	fn description(&self) -> &str {
		REWIND_DESCRIPTION
	}

	fn schema(&self) -> Value {
		json!({
			"type": "object",
			"properties": {
				"report": { "type": "string", "description": "investigation findings" }
			},
			"required": ["report"],
			"additionalProperties": false
		})
	}

	fn is_exclusive(&self) -> bool {
		true
	}

	async fn execute(&self, args: &Value, _ctx: &AgentContext) -> Result<ToolResult, ToolError> {
		let obj = strict_object(args, REWIND_TOOL_NAME, "report")?;
		if self.controller.checkpoint_state().is_none() {
			if self.controller.last_completed_rewind().is_some() {
				return Err(ToolError::Rejected(
					"Checkpoint already completed; continue from the retained rewind report instead of calling rewind again."
						.to_string(),
				));
			}
			return Err(ToolError::Rejected(
				"No active checkpoint. Create a checkpoint before calling rewind.".to_string(),
			));
		}
		let report = obj
			.get("report")
			.and_then(Value::as_str)
			.map(str::trim)
			.ok_or_else(|| ToolError::InvalidArguments("Missing required parameter 'report'".to_string()))?;
		if report.is_empty() {
			return Err(ToolError::Rejected("Report cannot be empty.".to_string()));
		}
		Ok(ToolResult {
			tool_call_id: String::new(),
			content: "Rewind requested.\nReport captured for context replacement.".to_string(),
			is_error: false,
			terminate: false,
			details: Some(json!({ "report": report, "rewound": true })),
		})
	}
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;

	use serde_json::json;

	use super::*;

	fn controller() -> Arc<CheckpointController> {
		Arc::new(CheckpointController::new())
	}

	fn checkpoint_entry(id: &str, goal: &str, started_at: &str) -> Value {
		json!({
			"type": "message",
			"id": id,
			"parentId": null,
			"timestamp": started_at,
			"message": {
				"role": "toolResult",
				"toolName": "checkpoint",
				"content": "Checkpoint: goal\nFinish exploration and formulate findings.",
				"details": { "goal": goal, "startedAt": started_at }
			}
		})
	}

	async fn run(tool: &dyn Tool, args: Value) -> Result<ToolResult, ToolError> {
		tool.execute(&args, &AgentContext::new(std::env::temp_dir())).await
	}

	#[tokio::test]
	async fn checkpoint_tool_returns_goal_and_rejects_duplicate() {
		let host = controller();
		let tool = CheckpointTool::new(Arc::clone(&host));
		let result = run(&tool, json!({ "goal": "map the retry path" })).await.expect("checkpoint");
		assert_eq!(result.content, "Checkpoint: map the retry path\nFinish exploration and formulate findings.");
		let details = result.details.expect("details");
		assert_eq!(details["goal"], "map the retry path");
		assert!(details["startedAt"].as_str().is_some_and(|value| value.ends_with('Z')));

		// State is live immediately; the session host backfills count and entry id
		// after the tool result is persisted.
		let state = host.checkpoint_state().expect("synchronous capture");
		assert_eq!(state.goal, "map the retry path");
		assert_eq!(state.checkpoint_message_count, 0);
		assert_eq!(state.checkpoint_entry_id, None);
		let _ = host.note_checkpoint("map the retry path", "2026-10-29T00:00:00.000Z", 8);
		assert!(host.backfill_checkpoint_entry_id("entry-1"));
		let error = run(&tool, json!({ "goal": "second" })).await.expect_err("duplicate");
		assert!(matches!(error, ToolError::Rejected(message) if message == "Checkpoint already active."));
	}

	#[tokio::test]
	async fn rewind_works_immediately_after_checkpoint_without_persistence() {
		let host = controller();
		let checkpoint = CheckpointTool::new(Arc::clone(&host));
		let rewind = RewindTool::new(Arc::clone(&host));
		run(&checkpoint, json!({ "goal": "immediate" })).await.expect("checkpoint");
		let result = run(&rewind, json!({ "report": "found it" })).await.expect("rewind");
		assert_eq!(result.details, Some(json!({ "report": "found it", "rewound": true })));
	}

	#[tokio::test]
	async fn rewind_tool_distinguishes_missing_empty_and_completed() {
		let host = controller();
		let tool = RewindTool::new(Arc::clone(&host));

		let error = run(&tool, json!({ "report": "findings" })).await.expect_err("none");
		assert!(matches!(error, ToolError::Rejected(message) if message == "No active checkpoint. Create a checkpoint before calling rewind."));

		let _ = host.note_checkpoint("goal", "2026-10-29T00:00:00.000Z", 4);
		let error = run(&tool, json!({ "report": "   " })).await.expect_err("empty");
		assert!(matches!(error, ToolError::Rejected(message) if message == "Report cannot be empty."));

		let result = run(&tool, json!({ "report": "  concise findings  " })).await.expect("rewind");
		assert_eq!(result.content, "Rewind requested.\nReport captured for context replacement.");
		assert_eq!(
			result.details,
			Some(json!({ "report": "concise findings", "rewound": true }))
		);

		assert!(host.note_rewind_tool_result("concise findings"));
		let pending = host.pending_rewind().expect("pending");
		assert_eq!(pending.report, "concise findings");
		assert!(!host.needs_rewind_before_yield(), "a captured report satisfies the guard");
		let completed = host.complete_rewind("2026-10-29T00:01:00.000Z").expect("completed");
		assert_eq!(completed.report, "concise findings");

		let error = run(&tool, json!({ "report": "again" })).await.expect_err("completed");
		assert!(matches!(error, ToolError::Rejected(message)
			if message == "Checkpoint already completed; continue from the retained rewind report instead of calling rewind again."));
	}

	#[test]
	fn rehydrate_prefers_most_recent_checkpoint_or_report() {
		let host = CheckpointController::new();
		let checkpoint = checkpoint_entry("c1", "goal", "2026-10-29T00:00:00.000Z");
		let report = json!({
			"type": "custom_message",
			"id": "r1",
			"parentId": "c1",
			"timestamp": "2026-10-29T00:01:00.000Z",
			"customType": REWIND_REPORT_CUSTOM_TYPE,
			"content": render_rewind_report("retained findings"),
			"display": false,
			"details": {
				"report": "retained findings",
				"startedAt": "2026-10-29T00:00:00.000Z",
				"rewoundAt": "2026-10-29T00:01:00.000Z"
			}
		});

		host.rehydrate(&[checkpoint.clone()]);
		let state = host.checkpoint_state().expect("open checkpoint restored");
		assert_eq!(state.checkpoint_entry_id.as_deref(), Some("c1"));
		assert_eq!(state.checkpoint_message_count, 1);
		assert_eq!(state.started_at, "2026-10-29T00:00:00.000Z");

		host.clear();
		host.rehydrate(&[checkpoint, report]);
		assert!(host.checkpoint_state().is_none());
		let completed = host.last_completed_rewind().expect("completed restored");
		assert_eq!(completed.report, "retained findings");
		assert_eq!(completed.rewound_at, "2026-10-29T00:01:00.000Z");
	}

	#[test]
	fn rewind_report_content_round_trips_through_marker() {
		let content = render_rewind_report("line one\nline two");
		assert_eq!(report_from_rewind_report_content(&content), "line one\nline two");
		assert_eq!(report_from_rewind_report_content("bare report"), "bare report");
	}
}
