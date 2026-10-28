//! Hub tool: peer messaging plus supervisor-backed background-job control.
//!
//! Implemented ops: `jobs`, `cancel`, `wait`, `send`, `inbox`, `list`.
//! Process supervision (`start`, `ps`, `logs`, `stop`, `restart`, `describe`)
//! is rejected explicitly: this build has no launch broker.
//!
//! Ownership split mirrors the TypeScript hub: job inspection and control are
//! scoped to the caller's own children, while messaging has a flat namespace —
//! siblings may message each other. Receipts report only outcomes this build
//! can actually produce (`injected`/`failed`); there is no park/revive
//! lifecycle, so `woken` and `revived` are never claimed.

use std::{collections::HashSet, sync::Arc, time::Duration};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Tool, ToolError};
use crate::{
	mailbox::{
		DEFAULT_IRC_TIMEOUT_MS, IrcMessage, MAIN_AGENT_ID, format_incoming,
	},
	state::{AgentContext, ToolResult},
	supervisor::{CancelAttempt, ChildRecord, ChildStatus, Supervisor},
};

const SUPPORTED_OPS: &str = "jobs, cancel, wait, send, inbox, list";
const LABEL_MAX_CHARS: usize = 80;
const DEFAULT_HUB_LIST_LIMIT: usize = 32;
const MAX_HUB_LIST_LIMIT: usize = 100;
/// Floor of the upstream smart poll ladder; also the default job wait window.
const DEFAULT_JOB_WAIT_MS: u64 = 5_000;

#[derive(Clone)]
pub struct HubTool {
	supervisor: Arc<Supervisor>,
	/// Agent whose direct children this hub may inspect/cancel. `None` is the
	/// root agent (`Main`), which owns root-spawned jobs.
	owner: Option<String>,
}

impl HubTool {
	pub fn root(supervisor: Arc<Supervisor>) -> Self {
		Self { supervisor, owner: None }
	}

	pub fn with_owner(supervisor: Arc<Supervisor>, owner: Option<String>) -> Self {
		Self { supervisor, owner }
	}

	fn sender_id(&self) -> String {
		self.owner.clone().unwrap_or_else(|| MAIN_AGENT_ID.to_string())
	}

	/// Jobs owned by this caller (the agent that spawned them). Root owns jobs
	/// whose `parent_id` is `None`; a child owns its own direct children only.
	fn owned_jobs(&self) -> Vec<ChildRecord> {
		self
			.supervisor
			.children()
			.into_iter()
			.filter(|record| record.parent_id == self.owner)
			.collect()
	}

	/// All known peers except the caller (flat namespace).
	fn peer_records(&self) -> Vec<ChildRecord> {
		let sender = self.sender_id();
		self
			.supervisor
			.children()
			.into_iter()
			.filter(|record| record.id != sender)
			.collect()
	}

	/// All agents known to be running: supervisor children plus mailbox presence
	/// (root `Main` turns, child loops, hub waiters). Sorted and deduplicated.
	fn live_agent_ids(&self) -> Vec<String> {
		let mut ids = self.supervisor.running_agent_ids();
		ids.extend(self.supervisor.mailbox().active_agents());
		ids.sort_unstable();
		ids.dedup();
		ids
	}

	fn live_peers(&self, sender: &str) -> Vec<String> {
		self
			.live_agent_ids()
			.into_iter()
			.filter(|id| id != sender)
			.collect()
	}

	fn agent_running(&self, id: &str) -> bool {
		if id == MAIN_AGENT_ID {
			return self.supervisor.mailbox().is_agent_active(MAIN_AGENT_ID);
		}
		self.live_agent_ids().iter().any(|live| live == id)
	}

	fn peer_available(&self, sender: &str, from: Option<&str>) -> bool {
		match from {
			Some(from) => from != sender && self.agent_running(from),
			None => !self.live_peers(sender).is_empty(),
		}
	}

	fn no_peer_wait_result(&self, sender: &str) -> Result<ToolResult, ToolError> {
		let details = HubToolDetails {
			op: "wait".to_string(),
			from: Some(sender.to_string()),
			to: None,
			receipts: None,
			waited: None,
			inbox: None,
			peers: None,
			counts: None,
			jobs: Vec::new(),
			cancelled: None,
		};
		let mut result = details_result(&details)?;
		result.content = "No running peers to wait for.".to_string();
		Ok(result)
	}

	fn snapshot_and_consume(
		&self,
		records: Vec<ChildRecord>,
	) -> Vec<HubJobSnapshot> {
		let settled: Vec<String> = records
			.iter()
			.filter(|record| record.status != ChildStatus::Running)
			.map(|record| record.id.clone())
			.collect();
		let already = self.supervisor.consume_results(&settled);
		let mut snapshots = Vec::with_capacity(records.len());
		for record in records {
			let consumed = already.contains(&record.id);
			snapshots.push(snapshot(&record, consumed));
		}
		snapshots
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
pub struct IrcDeliveryReceipt {
	pub to: String,
	pub outcome: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubPeerInfo {
	pub id: String,
	pub display_name: String,
	pub kind: String,
	pub status: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub parent_id: Option<String>,
	pub unread: usize,
	pub last_activity: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubRosterCounts {
	pub running: usize,
	/// Settled agents. Unlike upstream `idle`, these are terminal in this
	/// build: they cannot be woken. Rows carry their real status.
	pub idle: usize,
	pub parked: usize,
	pub shown: usize,
	pub truncated: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubToolDetails {
	pub op: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub from: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub to: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub receipts: Option<Vec<IrcDeliveryReceipt>>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub waited: Option<IrcMessage>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub inbox: Option<Vec<IrcMessage>>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub peers: Option<Vec<HubPeerInfo>>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub counts: Option<HubRosterCounts>,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
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

fn job_status(record: &ChildRecord) -> &'static str {
	match record.status {
		ChildStatus::Running => "running",
		ChildStatus::Completed => "completed",
		ChildStatus::Failed => "failed",
		ChildStatus::Aborted
			if matches!(
				record.abort_reason.as_deref(),
				Some("cancelled by hub" | "aborted by parent")
			) =>
		{
			"cancelled"
		},
		ChildStatus::Aborted => "failed",
	}
}

fn snapshot(record: &ChildRecord, consumed: bool) -> HubJobSnapshot {
	let status = job_status(record);
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
			.filter(|_| record.status == ChildStatus::Completed && !consumed),
		error_text: if record.status == ChildStatus::Running {
			None
		} else {
			match record.status {
				ChildStatus::Failed => record.error.clone(),
				ChildStatus::Aborted => record.abort_reason.clone(),
				_ => None,
			}
			.filter(|_| !consumed)
		},
	}
}

fn format_duration(milliseconds: u64) -> String {
	if milliseconds < 1_000 {
		return format!("{milliseconds}ms");
	}
	let seconds = milliseconds as f64 / 1_000.0;
	if seconds < 60.0 {
		let rounded = (seconds * 10.0).round() / 10.0;
		if rounded.fract() == 0.0 {
			format!("{}s", rounded as u64)
		} else {
			format!("{rounded:.1}s")
		}
	} else {
		let minutes = (seconds / 60.0).floor() as u64;
		let rest = (seconds - minutes as f64 * 60.0).round() as u64;
		format!("{minutes}m {rest}s")
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

fn error_result(message: String, details: HubToolDetails) -> Result<ToolResult, ToolError> {
	let content = serde_json::to_string(&details).map_err(|err| {
		ToolError::ExecutionFailed(format!("Failed to serialize hub details: {err}"))
	})?;
	Ok(ToolResult {
		tool_call_id: String::new(),
		content: message,
		is_error: true,
		terminate: false,
		details: Some(json!({ "error": content })),
	})
}

fn cancelled_outcome(id: &str, attempt: CancelAttempt) -> HubCancelOutcome {
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

fn message_result(sender: &str, message: IrcMessage) -> Result<ToolResult, ToolError> {
	let details = HubToolDetails {
		op: "wait".to_string(),
		from: Some(sender.to_string()),
		to: None,
		receipts: None,
		waited: Some(message.clone()),
		inbox: None,
		peers: None,
		counts: None,
		jobs: Vec::new(),
		cancelled: None,
	};
	let content = serde_json::to_string(&details).map_err(|err| {
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

enum ReplyWait {
	Reply(IrcMessage),
	Timeout,
	Stopped,
	Interrupted,
}

#[async_trait]
impl Tool for HubTool {
	fn name(&self) -> &str {
		"hub"
	}

	fn description(&self) -> &str {
		"Peer messaging (`list`, `send`, `inbox`, `wait`) and background-job control (`jobs`, `cancel`, `wait`). Process supervision is not available in this build."
	}

	fn schema(&self) -> Value {
		json!({
			"type": "object",
			"properties": {
				"op": {
					"type": "string",
					"description": "Hub operation: send, wait, inbox, list, jobs, cancel (process supervision is not supported in this build)."
				},
				"to": { "type": "string", "description": "send: recipient agent id or \"all\"" },
				"message": { "type": "string", "description": "send: message body" },
				"replyTo": { "type": "string", "description": "send: message id being answered" },
				"await": { "type": "boolean", "description": "send: wait for the recipient's reply (invalid with to:\"all\")" },
				"from": { "type": "string", "description": "wait: only accept a message from this agent id" },
				"ids": {
					"type": "array",
					"items": { "type": "string" },
					"description": "wait: job ids to watch (omit = all running jobs); cancel: job ids to kill"
				},
				"timeoutMs": { "type": "number", "description": "wait/send await: timeout in milliseconds (0 waits indefinitely)" },
				"peek": { "type": "boolean", "description": "inbox: list messages without consuming them" },
				"status": { "type": "string", "description": "list: filter by running or a terminal status" },
				"limit": { "type": "integer", "description": "list: max peer rows; default 32, max 100" }
			},
			"required": ["op"]
		})
	}

	fn is_exclusive(&self) -> bool {
		false
	}

	async fn execute(&self, args: &Value, ctx: &AgentContext) -> Result<ToolResult, ToolError> {
		let obj = args
			.as_object()
			.ok_or_else(|| ToolError::InvalidArguments("Arguments must be an object".to_string()))?;
		let op = obj
			.get("op")
			.and_then(Value::as_str)
			.ok_or_else(|| ToolError::InvalidArguments("Missing 'op' argument".to_string()))?;

		match op {
			"jobs" => self.execute_jobs(),
			"cancel" => self.execute_cancel(obj).await,
			"wait" => self.execute_wait(obj, ctx).await,
			"send" => self.execute_send(obj, ctx).await,
			"inbox" => self.execute_inbox(obj),
			"list" => self.execute_list(obj),
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

impl HubTool {
	fn execute_jobs(&self) -> Result<ToolResult, ToolError> {
		let jobs = self.snapshot_and_consume(self.owned_jobs());
		let details = HubToolDetails {
			op: "jobs".to_string(),
			from: None,
			to: None,
			receipts: None,
			waited: None,
			inbox: None,
			peers: None,
			counts: None,
			jobs,
			cancelled: None,
		};
		let serialized = serde_json::to_string(&details).map_err(|err| {
			ToolError::ExecutionFailed(format!("Failed to serialize hub details: {err}"))
		})?;
		Ok(ToolResult {
			tool_call_id: String::new(),
			content: serialized,
			is_error: false,
			terminate: false,
			details: None,
		})
	}

	async fn execute_cancel(
		&self,
		obj: &serde_json::Map<String, Value>,
	) -> Result<ToolResult, ToolError> {
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
			if self.owner.as_deref() == Some(id) {
				cancelled.push(HubCancelOutcome {
					id: id.to_string(),
					status: "not_found".to_string(),
					message: "Cannot cancel yourself.".to_string(),
				});
				continue;
			}
			let owned = self
				.owned_jobs()
				.iter()
				.any(|record| record.id == id);
			if !owned {
				let known = self.supervisor.is_known_agent(id);
				let message = if known {
					format!("Background job {id} was not spawned by you.")
				} else {
					format!("Background job not found: {id}")
				};
				cancelled.push(HubCancelOutcome {
					id: id.to_string(),
					status: "not_found".to_string(),
					message,
				});
				continue;
			}
			let attempt = self
				.supervisor
				.cancel_child(id, self.owner.as_deref())
				.await;
			cancelled.push(cancelled_outcome(id, attempt));
		}
		let cancelled_ids: HashSet<&str> =
			cancelled.iter().map(|outcome| outcome.id.as_str()).collect();
		let records: Vec<ChildRecord> = self
			.owned_jobs()
			.into_iter()
			.filter(|record| cancelled_ids.contains(record.id.as_str()))
			.collect();
		let jobs = self.snapshot_and_consume(records);
		let details = HubToolDetails {
			op: "cancel".to_string(),
			from: None,
			to: None,
			receipts: None,
			waited: None,
			inbox: None,
			peers: None,
			counts: None,
			jobs,
			cancelled: Some(cancelled),
		};
		let serialized = serde_json::to_string(&details).map_err(|err| {
			ToolError::ExecutionFailed(format!("Failed to serialize hub details: {err}"))
		})?;
		Ok(ToolResult {
			tool_call_id: String::new(),
			content: serialized,
			is_error: false,
			terminate: false,
			details: None,
		})
	}

	async fn execute_wait(
		&self,
		obj: &serde_json::Map<String, Value>,
		ctx: &AgentContext,
	) -> Result<ToolResult, ToolError> {
		let sender = self.sender_id();
		let mailbox = self.supervisor.mailbox();
		let from = obj
			.get("from")
			.and_then(Value::as_str)
			.map(str::trim)
			.filter(|value| !value.is_empty())
			.map(str::to_string);

		if let Some(message) = mailbox.take(&sender, from.as_deref()) {
			return message_result(&sender, message);
		}

		let ids: Option<Vec<String>> = obj.get("ids").and_then(Value::as_array).map(|values| {
			values
				.iter()
				.filter_map(Value::as_str)
				.map(str::to_string)
				.collect()
		});
		let all_owned = self.owned_jobs();
		let watched: Vec<ChildRecord> = match &ids {
			Some(ids) => all_owned
				.into_iter()
				.filter(|record| ids.iter().any(|id| id == &record.id))
				.collect(),
			None => all_owned
				.into_iter()
				.filter(|record| record.status == ChildStatus::Running)
				.collect(),
		};
		if ids.is_some() && watched.is_empty() {
			let ids = ids.unwrap_or_default();
			let details = HubToolDetails {
				op: "wait".to_string(),
				from: Some(sender.clone()),
				to: None,
				receipts: None,
				waited: None,
				inbox: None,
				peers: None,
				counts: None,
				jobs: Vec::new(),
				cancelled: None,
			};
			let mut result = details_result(&details)?;
			result.content = format!("No matching jobs found for IDs: {}", ids.join(", "));
			result.is_error = false;
			return Ok(result);
		}
		if !watched.is_empty()
			&& watched
				.iter()
				.all(|record| record.status != ChildStatus::Running)
		{
			return self.job_snapshot_result("wait", watched);
		}

		let explicit_timeout = obj.get("timeoutMs").and_then(Value::as_u64);
		if watched.is_empty() {
			if let Some(from) = &from
				&& !self.agent_running(from)
			{
				return Err(ToolError::ExecutionFailed(format!(
					"IRC wait aborted: agent \"{from}\" is not running"
				)));
			}
			if !self.peer_available(&sender, from.as_deref()) {
				return self.no_peer_wait_result(&sender);
			}
			let timeout = explicit_timeout.unwrap_or(DEFAULT_IRC_TIMEOUT_MS);
			let (buffered, waiter) = mailbox.take_or_register(&sender, from.as_deref());
			if let Some(message) = buffered {
				return message_result(&sender, message);
			}
			let Some(mut waiter) = waiter else {
				return self.no_peer_wait_result(&sender);
			};
			let deadline = (timeout > 0)
				.then(|| tokio::time::Instant::now() + Duration::from_millis(timeout));
			let mut notified = Box::pin(self.supervisor.settle_signal());
			notified.as_mut().enable();
			let mut presence = mailbox.presence();
			presence.borrow_and_update();
			loop {
				// A peer-only wait must end when the last eligible peer exits;
				// presence bumps cover root turns and child loops, while settle
				// covers children tracked only by the supervisor.
				if !self.peer_available(&sender, from.as_deref()) {
					return self.no_peer_wait_result(&sender);
				}
				let receive = waiter.recv();
				tokio::pin!(receive);
				match deadline {
					Some(deadline) => {
						tokio::select! {
							_ = ctx.cancellation_token.cancelled() => {
								return Err(ToolError::ExecutionFailed("hub wait aborted".to_string()));
							},
							message = &mut receive => {
								if let Some(message) = message {
									return message_result(&sender, message);
								}
								return self.message_timeout_result(&sender, from.as_deref(), timeout);
							},
							_ = notified.as_mut() => {},
							_ = presence.changed() => {},
							_ = tokio::time::sleep_until(deadline) => {
								return self.message_timeout_result(&sender, from.as_deref(), timeout);
							},
						}
					},
					None => {
						tokio::select! {
							_ = ctx.cancellation_token.cancelled() => {
								return Err(ToolError::ExecutionFailed("hub wait aborted".to_string()));
							},
							message = &mut receive => {
								if let Some(message) = message {
									return message_result(&sender, message);
								}
								return self.message_timeout_result(&sender, from.as_deref(), timeout);
							},
							_ = notified.as_mut() => {},
							_ = presence.changed() => {},
						}
					},
				}
				notified = Box::pin(self.supervisor.settle_signal());
				notified.as_mut().enable();
				presence.borrow_and_update();
			}
		}

		let watched_ids: Vec<String> = watched.iter().map(|record| record.id.clone()).collect();
		let timeout = explicit_timeout.unwrap_or(DEFAULT_JOB_WAIT_MS);
		let (buffered, waiter) = mailbox.take_or_register(&sender, from.as_deref());
		if let Some(message) = buffered {
			return message_result(&sender, message);
		}
		let Some(mut waiter) = waiter else {
			return self.job_snapshot_result("wait", Vec::new());
		};
		let deadline =
			(timeout > 0).then(|| tokio::time::Instant::now() + Duration::from_millis(timeout));
		let mut notified = Box::pin(self.supervisor.settle_signal());
		notified.as_mut().enable();
		loop {
			// Re-read through the supervisor so a terminal transition between
			// the previous await and this check is never missed.
			let current: Vec<ChildRecord> = self
				.owned_jobs()
				.into_iter()
				.filter(|record| watched_ids.iter().any(|id| id == &record.id))
				.collect();
			if current
				.iter()
				.any(|record| record.status != ChildStatus::Running)
			{
				return self.job_snapshot_result("wait", current);
			}
			let receive = waiter.recv();
			tokio::pin!(receive);
			match deadline {
				Some(deadline) => {
					tokio::select! {
						_ = ctx.cancellation_token.cancelled() => {
							return Err(ToolError::ExecutionFailed("hub wait aborted".to_string()));
						},
						message = &mut receive => {
							if let Some(message) = message {
								return message_result(&sender, message);
							}
							return self.job_snapshot_result("wait", current);
						},
						_ = notified.as_mut() => {},
						_ = tokio::time::sleep_until(deadline) => {
							return self.job_snapshot_result("wait", current);
						},
					}
				},
				None => {
					tokio::select! {
						_ = ctx.cancellation_token.cancelled() => {
							return Err(ToolError::ExecutionFailed("hub wait aborted".to_string()));
						},
						message = &mut receive => {
							if let Some(message) = message {
								return message_result(&sender, message);
							}
							return self.job_snapshot_result("wait", current);
						},
						_ = notified.as_mut() => {},
					}
				},
			}
			// Re-arm before the next status check.
			notified = Box::pin(self.supervisor.settle_signal());
			notified.as_mut().enable();
		}
	}

	fn job_snapshot_result(
		&self,
		op: &str,
		records: Vec<ChildRecord>,
	) -> Result<ToolResult, ToolError> {
		let jobs = self.snapshot_and_consume(records);
		let details = HubToolDetails {
			op: op.to_string(),
			from: None,
			to: None,
			receipts: None,
			waited: None,
			inbox: None,
			peers: None,
			counts: None,
			jobs,
			cancelled: None,
		};
		let serialized = serde_json::to_string(&details).map_err(|err| {
			ToolError::ExecutionFailed(format!("Failed to serialize hub details: {err}"))
		})?;
		Ok(ToolResult {
			tool_call_id: String::new(),
			content: serialized,
			is_error: false,
			terminate: false,
			details: None,
		})
	}

	fn message_timeout_result(
		&self,
		sender: &str,
		from: Option<&str>,
		timeout: u64,
	) -> Result<ToolResult, ToolError> {
		let filter = from.map(|from| format!(" from {from}")).unwrap_or_default();
		let details = HubToolDetails {
			op: "wait".to_string(),
			from: Some(sender.to_string()),
			to: None,
			receipts: None,
			waited: None,
			inbox: None,
			peers: None,
			counts: None,
			jobs: Vec::new(),
			cancelled: None,
		};
		let mut result = details_result(&details)?;
		result.content = format!("No message{filter} within {}.", format_duration(timeout));
		Ok(result)
	}

	async fn execute_send(
		&self,
		obj: &serde_json::Map<String, Value>,
		ctx: &AgentContext,
	) -> Result<ToolResult, ToolError> {
		let sender = self.sender_id();
		let to = obj
			.get("to")
			.and_then(Value::as_str)
			.map(str::trim)
			.filter(|value| !value.is_empty());
		let message = obj
			.get("message")
			.and_then(Value::as_str)
			.map(str::trim)
			.filter(|value| !value.is_empty());
		let Some(to) = to else {
			return error_result(
				"`to` is required for op=\"send\".".to_string(),
				HubToolDetails {
					op: "send".to_string(),
					from: Some(sender),
					to: None,
					receipts: None,
					waited: None,
					inbox: None,
					peers: None,
					counts: None,
					jobs: Vec::new(),
					cancelled: None,
				},
			);
		};
		let Some(message) = message else {
			return error_result(
				"`message` is required for op=\"send\".".to_string(),
				HubToolDetails {
					op: "send".to_string(),
					from: Some(sender),
					to: Some(to.to_string()),
					receipts: None,
					waited: None,
					inbox: None,
					peers: None,
					counts: None,
					jobs: Vec::new(),
					cancelled: None,
				},
			);
		};
		if to == sender.as_str() {
			return error_result(
				"Cannot send a message to yourself.".to_string(),
				HubToolDetails {
					op: "send".to_string(),
					from: Some(sender),
					to: Some(to.to_string()),
					receipts: None,
					waited: None,
					inbox: None,
					peers: None,
					counts: None,
					jobs: Vec::new(),
					cancelled: None,
				},
			);
		}
		let is_broadcast = to == "all";
		let await_reply = obj.get("await").and_then(Value::as_bool).unwrap_or(false);
		if is_broadcast && await_reply {
			return error_result(
				"`await` is invalid with to:\"all\" — broadcasts have no single replier."
					.to_string(),
				HubToolDetails {
					op: "send".to_string(),
					from: Some(sender),
					to: Some(to.to_string()),
					receipts: None,
					waited: None,
					inbox: None,
					peers: None,
					counts: None,
					jobs: Vec::new(),
					cancelled: None,
				},
			);
		}
		let reply_to = obj.get("replyTo").and_then(Value::as_str);
		let mailbox = self.supervisor.mailbox();
		let targets: Vec<String> = if is_broadcast {
			self.live_peers(&sender)
		} else {
			vec![to.to_string()]
		};

		let mut receipts = Vec::with_capacity(targets.len());
		for target in &targets {
			let msg = IrcMessage::new(sender.clone(), target.clone(), message)
				.with_reply_to(reply_to);
			let known = self.supervisor.is_known_agent(target);
			let receipt = if mailbox.deliver_to_waiter(&msg) {
				IrcDeliveryReceipt {
					to: target.clone(),
					outcome: "injected".to_string(),
					error: None,
				}
			} else {
				match self
					.supervisor
					.inject_peer_message(target, format_incoming(&msg))
				{
					Ok(()) => IrcDeliveryReceipt {
						to: target.clone(),
						outcome: "injected".to_string(),
						error: None,
					},
					Err(error) => {
						// Failed live hand-off: buffer only when a later drain is
						// actually reachable (root turn boundary or a still-running
						// child waiting on `wait`/`inbox`). A terminal child cannot
						// be revived, so leave no unreachable mail behind.
						let reachable = known
							&& (target == MAIN_AGENT_ID
								|| self.agent_running(target));
						if reachable {
							mailbox.buffer(msg);
						}
						IrcDeliveryReceipt {
							to: target.clone(),
							outcome: "failed".to_string(),
							error: Some(error),
						}
					},
				}
			};
			receipts.push(receipt);
		}

		let delivered = receipts
			.iter()
			.filter(|receipt| receipt.outcome != "failed")
			.count();
		let mut lines = Vec::new();
		if targets.is_empty() {
			lines.push("No live peers to broadcast to.".to_string());
		} else if delivered == 0 {
			lines.push("No recipients received the message.".to_string());
		} else {
			lines.push(format!("Delivered to {delivered} peer(s):"));
		}
		for receipt in &receipts {
			lines.push(match &receipt.error {
				Some(error) => format!("- {}: failed — {error}", receipt.to),
				None => format!("- {}: {}", receipt.to, receipt.outcome),
			});
		}

		let mut waited = None;
		if await_reply && !is_broadcast && delivered > 0 {
			let timeout = obj
				.get("timeoutMs")
				.and_then(Value::as_u64)
				.unwrap_or(DEFAULT_IRC_TIMEOUT_MS);
			let target = to.to_string();
			match self.await_reply(&sender, &target, timeout, ctx).await? {
				ReplyWait::Reply(message) => {
					lines.push(String::new());
					lines.push(format!("Reply from {}:", message.from));
					lines.push(message.body.clone());
					waited = Some(message);
				},
				ReplyWait::Timeout => {
					lines.push(String::new());
					lines.push(format!(
						"No reply from {target} within {}. They may answer later — check `inbox` or `wait` again.",
						format_duration(timeout)
					));
				},
				ReplyWait::Stopped => {
					lines.push(String::new());
					lines.push(format!(
						"{target} stopped without replying. Check `inbox` or `wait` again for a later answer."
					));
				},
				ReplyWait::Interrupted => {
					lines.push(String::new());
					lines.push(format!(
						"Send delivered but the reply wait was interrupted before {target} answered. Check `inbox` or `wait` again after handling the interrupt."
					));
				},
			}
		}

		let details = HubToolDetails {
			op: "send".to_string(),
			from: Some(sender),
			to: Some(to.to_string()),
			receipts: Some(receipts),
			waited,
			inbox: None,
			peers: None,
			counts: None,
			jobs: Vec::new(),
			cancelled: None,
		};
		let details = serde_json::to_value(&details).map_err(|err| {
			ToolError::ExecutionFailed(format!("Failed to serialize hub details: {err}"))
		})?;
		Ok(ToolResult {
			tool_call_id: String::new(),
			content: lines.join("\n"),
			is_error: delivered == 0 && !targets.is_empty(),
			terminate: false,
			details: Some(details),
		})
	}

	async fn await_reply(
		&self,
		sender: &str,
		target: &str,
		timeout: u64,
		ctx: &AgentContext,
	) -> Result<ReplyWait, ToolError> {
		let mailbox = self.supervisor.mailbox();
		let (buffered, waiter) = mailbox.take_or_register(sender, Some(target));
		if let Some(message) = buffered {
			return Ok(ReplyWait::Reply(message));
		}
		let Some(mut waiter) = waiter else { return Ok(ReplyWait::Stopped) };
		let deadline =
			(timeout > 0).then(|| tokio::time::Instant::now() + Duration::from_millis(timeout));
		let mut notified = Box::pin(self.supervisor.settle_signal());
		notified.as_mut().enable();
		let mut presence = mailbox.presence();
		presence.borrow_and_update();
		loop {
			if !self.agent_running(target) {
				if let Some(message) = mailbox.take(sender, Some(target)) {
					return Ok(ReplyWait::Reply(message));
				}
				return Ok(ReplyWait::Stopped);
			}
			let receive = waiter.recv();
			tokio::pin!(receive);
			match deadline {
				Some(deadline) => {
					tokio::select! {
						_ = ctx.cancellation_token.cancelled() => return Ok(ReplyWait::Interrupted),
						message = &mut receive => {
							if let Some(message) = message {
								return Ok(ReplyWait::Reply(message));
							}
							return Ok(ReplyWait::Stopped);
						},
						_ = notified.as_mut() => {},
						_ = presence.changed() => {},
						_ = tokio::time::sleep_until(deadline) => {
							return Ok(if let Some(message) = mailbox.take(sender, Some(target)) {
								ReplyWait::Reply(message)
							} else {
								ReplyWait::Timeout
							});
						},
					}
				},
				None => {
					tokio::select! {
						_ = ctx.cancellation_token.cancelled() => return Ok(ReplyWait::Interrupted),
						message = &mut receive => {
							if let Some(message) = message {
								return Ok(ReplyWait::Reply(message));
							}
							return Ok(ReplyWait::Stopped);
						},
						_ = notified.as_mut() => {},
						_ = presence.changed() => {},
					}
				},
			}
			notified = Box::pin(self.supervisor.settle_signal());
			notified.as_mut().enable();
			presence.borrow_and_update();
		}
	}

	fn execute_inbox(
		&self,
		obj: &serde_json::Map<String, Value>,
	) -> Result<ToolResult, ToolError> {
		let sender = self.sender_id();
		let peek = obj.get("peek").and_then(Value::as_bool).unwrap_or(false);
		let messages = self.supervisor.mailbox().inbox(&sender, peek);
		let details = HubToolDetails {
			op: "inbox".to_string(),
			from: Some(sender),
			to: None,
			receipts: None,
			waited: None,
			inbox: Some(messages),
			peers: None,
			counts: None,
			jobs: Vec::new(),
			cancelled: None,
		};
		let serialized = serde_json::to_string(&details).map_err(|err| {
			ToolError::ExecutionFailed(format!("Failed to serialize hub details: {err}"))
		})?;
		Ok(ToolResult {
			tool_call_id: String::new(),
			content: serialized,
			is_error: false,
			terminate: false,
			details: None,
		})
	}

	fn execute_list(
		&self,
		obj: &serde_json::Map<String, Value>,
	) -> Result<ToolResult, ToolError> {
		let sender = self.sender_id();
		let status_filter = obj.get("status").and_then(Value::as_str);
		let limit = obj
			.get("limit")
			.and_then(Value::as_u64)
			.map(|value| {
				let value = value.max(1) as usize;
				value.min(MAX_HUB_LIST_LIMIT)
			})
			.unwrap_or(DEFAULT_HUB_LIST_LIMIT);
		let mailbox = self.supervisor.mailbox();
		let all = self.peer_records();
		let running = all
			.iter()
			.filter(|record| record.status == ChildStatus::Running)
			.count();
		let idle = all.len() - running;
		let mut selected: Vec<ChildRecord> = all
			.into_iter()
			.filter(|record| match status_filter {
				None => true,
				Some("running") => record.status == ChildStatus::Running,
				Some("idle") => record.status != ChildStatus::Running,
				Some("parked") => false,
				Some(other) => job_status(record) == other,
			})
			.collect();
		selected.sort_by(|left, right| {
			let left_running = left.status == ChildStatus::Running;
			let right_running = right.status == ChildStatus::Running;
			right_running
				.cmp(&left_running)
				.then(right.started_at.unwrap_or_default().cmp(&left.started_at.unwrap_or_default()))
		});
		let truncated = selected.len().saturating_sub(limit);
		selected.truncate(limit);
		let peers: Vec<HubPeerInfo> = selected
			.into_iter()
			.map(|record| HubPeerInfo {
				status: job_status(&record).to_string(),
				unread: mailbox.unread_count(&record.id),
				last_activity: record
					.started_at
					.map(|started| started + record.duration_ms.unwrap_or_default())
					.unwrap_or_else(now_ms),
				display_name: record.name,
				kind: "sub".to_string(),
				parent_id: record.parent_id,
				id: record.id,
			})
			.collect();
		let counts = HubRosterCounts {
			running,
			idle,
			parked: 0,
			shown: peers.len(),
			truncated,
		};
		let details = HubToolDetails {
			op: "list".to_string(),
			from: Some(sender),
			to: None,
			receipts: None,
			waited: None,
			inbox: None,
			peers: Some(peers),
			counts: Some(counts),
			jobs: Vec::new(),
			cancelled: None,
		};
		let serialized = serde_json::to_string(&details).map_err(|err| {
			ToolError::ExecutionFailed(format!("Failed to serialize hub details: {err}"))
		})?;
		Ok(ToolResult {
			tool_call_id: String::new(),
			content: serialized,
			is_error: false,
			terminate: false,
			details: None,
		})
	}
}
