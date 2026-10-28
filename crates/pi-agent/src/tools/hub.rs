//! Hub tool: supervisor-backed job inspection and cancellation.
//!
//! This is a deliberately small subset of the TypeScript hub. Only `jobs` and
//! `cancel` are implemented; peer messaging and process supervision are
//! rejected explicitly. Delegated work runs synchronously inside a `task` tool
//! call, so `hub` observes records and can abort a child that is still running
//! (for example from a concurrently scheduled tool call), but it is not a
//! background scheduler and cannot revive completed work.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Tool, ToolError};
use crate::{
	state::{AgentContext, ToolResult},
	supervisor::{CancelAttempt, ChildRecord, ChildStatus, Supervisor},
};

const SUPPORTED_OPS: &str = "jobs, cancel";
const LABEL_MAX_CHARS: usize = 80;

#[derive(Clone)]
pub struct HubTool {
	supervisor: Arc<Supervisor>,
	/// Agent whose direct children this hub may observe/cancel. `None` is the
	/// root agent and sees the whole supervisor tree.
	owner: Option<String>,
}

impl HubTool {
	pub fn root(supervisor: Arc<Supervisor>) -> Self {
		Self { supervisor, owner: None }
	}

	pub fn with_owner(supervisor: Arc<Supervisor>, owner: Option<String>) -> Self {
		Self { supervisor, owner }
	}

	fn visible(&self) -> Vec<ChildRecord> {
		self
			.supervisor
			.children()
			.into_iter()
			.filter(|record| {
				self
					.owner
					.as_deref()
					.is_none_or(|owner| record.parent_id.as_deref() == Some(owner))
			})
			.collect()
	}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubJobSnapshot {
	pub id: String,
	/// Model-supplied label; not unique, never accepted by `hub cancel`.
	pub name: String,
	#[serde(rename = "type")]
	pub kind: String,
	pub status: String,
	pub label: String,
	pub duration_ms: u64,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub result_text: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub error_text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubCancelOutcome {
	pub id: String,
	pub status: String,
	pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubToolDetails {
	pub op: String,
	pub jobs: Vec<HubJobSnapshot>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub cancelled: Option<Vec<HubCancelOutcome>>,
}

fn now_ms() -> u64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|duration| duration.as_millis() as u64)
		.unwrap_or_default()
}

fn label(task: &str) -> String {
	let first = task.lines().next().unwrap_or("").trim();
	let mut out: String = first.chars().take(LABEL_MAX_CHARS).collect();
	if first.chars().count() > LABEL_MAX_CHARS {
		out.push('…');
	}
	if out.is_empty() { "(no description)".to_string() } else { out }
}

fn snapshot(record: &ChildRecord) -> HubJobSnapshot {
	let status = match record.status {
		ChildStatus::Running => "running",
		ChildStatus::Completed => "completed",
		ChildStatus::Failed => "failed",
		// Operator/parent cancellation is distinguishable from a child that
		// aborted with an error (`yield {result:{error}}`): only the former is a
		// cancellation; an error abort is a task failure.
		ChildStatus::Aborted
			if matches!(
				record.abort_reason.as_deref(),
				Some("cancelled by hub" | "aborted by parent")
			) =>
		{
			"cancelled"
		},
		ChildStatus::Aborted => "failed",
	};
	let duration_ms = match record.status {
		ChildStatus::Running => record
			.started_at
			.map(|started| now_ms().saturating_sub(started))
			.unwrap_or_default(),
		_ => record.duration_ms.unwrap_or_default(),
	};
	HubJobSnapshot {
		id: record.id.clone(),
		name: record.name.clone(),
		kind: "task".to_string(),
		status: status.to_string(),
		label: label(&record.task),
		duration_ms,
		result_text: record
			.output
			.clone()
			.filter(|_| record.status == ChildStatus::Completed),
		error_text: if record.status == ChildStatus::Aborted {
			record.abort_reason.clone()
		} else {
			record.error.clone()
		},
	}
}

fn details_result(details: &HubToolDetails) -> Result<ToolResult, ToolError> {
	let content = serde_json::to_string(details).map_err(|err| {
		ToolError::ExecutionFailed(format!("Failed to serialize hub details: {err}"))
	})?;
	Ok(ToolResult {
		tool_call_id: String::new(),
		content,
		is_error: false,
		terminate: false,
		details: None,
	})
}

fn cancel_message(id: &str, attempt: CancelAttempt) -> HubCancelOutcome {
	let (status, message) = match attempt {
		CancelAttempt::Cancelled => ("cancelled", format!("Cancelled background job {id}.")),
		CancelAttempt::NotFound => ("not_found", format!("Background job not found: {id}")),
		CancelAttempt::AlreadyCompleted => (
			"already_completed",
			format!("Background job {id} is already settled."),
		),
	};
	HubCancelOutcome { id: id.to_string(), status: status.to_string(), message }
}

#[async_trait]
impl Tool for HubTool {
	fn name(&self) -> &str {
		"hub"
	}

	fn description(&self) -> &str {
		"Supervisor-backed child inspection. Supported ops: `jobs` (records from this process) and `cancel` (abort a still-running child); all other hub ops are rejected in this build. Delegation is synchronous, so this is not a background job scheduler."
	}

	fn schema(&self) -> Value {
		json!({
			"type": "object",
			"properties": {
				"op": {
					"type": "string",
					"description": "Hub operation. This build supports `jobs` and `cancel` only."
				},
				"ids": {
					"type": "array",
					"items": { "type": "string" },
					"description": "cancel: supervisor job ids to abort (use `jobs` `id`; names are non-unique labels). Unknown ids are reported per id."
				}
			},
			"required": ["op"]
		})
	}

	fn is_exclusive(&self) -> bool {
		false
	}

	async fn execute(&self, args: &Value, _ctx: &AgentContext) -> Result<ToolResult, ToolError> {
		let obj = args
			.as_object()
			.ok_or_else(|| ToolError::InvalidArguments("Arguments must be an object".to_string()))?;
		let op = obj
			.get("op")
			.and_then(Value::as_str)
			.ok_or_else(|| ToolError::InvalidArguments("Missing 'op' argument".to_string()))?;

		match op {
			"jobs" => {
				let details = HubToolDetails {
					op: "jobs".to_string(),
					jobs: self.visible().iter().map(snapshot).collect(),
					cancelled: None,
				};
				details_result(&details)
			},
			"cancel" => {
				let ids = obj.get("ids").and_then(Value::as_array).ok_or_else(|| {
					ToolError::InvalidArguments("`ids` is required for op=\"cancel\".".to_string())
				})?;
				if ids.is_empty() {
					return Err(ToolError::InvalidArguments(
						"`ids` must contain at least one job id".to_string(),
					));
				}
				let mut cancelled = Vec::with_capacity(ids.len());
				for id in ids {
					let id = id.as_str().ok_or_else(|| {
						ToolError::InvalidArguments("`ids` must be an array of strings".to_string())
					})?;
					let attempt = self
						.supervisor
						.cancel_child(id, self.owner.as_deref())
						.await;
					cancelled.push(cancel_message(id, attempt));
				}
				let jobs: Vec<HubJobSnapshot> = {
					let cancelled_ids: Vec<&str> =
						cancelled.iter().map(|outcome| outcome.id.as_str()).collect();
					self
						.visible()
						.iter()
						.filter(|record| cancelled_ids.contains(&record.id.as_str()))
						.map(snapshot)
						.collect()
				};
				let details = HubToolDetails {
					op: "cancel".to_string(),
					jobs,
					cancelled: Some(cancelled),
				};
				details_result(&details)
			},
			other => {
				let message = format!(
					"hub op `{other}` is not supported in this build; supported ops: {SUPPORTED_OPS}"
				);
				Ok(ToolResult {
					tool_call_id: String::new(),
					content: json!({ "error": message, "op": other, "jobs": [] }).to_string(),
					is_error: true,
					terminate: false,
					details: None,
				})
			},
		}
	}
}
