//! Hub tool: peer messaging, supervisor-backed background-job control, and
//! OS-process supervision.
//!
//! Implemented ops: `jobs`, `cancel`, `wait`, `send`, `inbox`, `list`,
//! `start`, `ps`, `logs`, `stop`, `restart`, `describe`. Process ops run on the
//! root-lifetime [`crate::tools::process::ProcessManager`]; `wait` and `send`
//! dispatch to a process when `name` is supplied and to jobs/messages
//! otherwise.
//!
//! Ownership split mirrors the TypeScript hub: job inspection and control are
//! scoped to the caller's own children, while messaging has a flat namespace —
//! siblings may message each other. Process visibility hides other agents'
//! processes from a child; root sees everything, and stdin/signals/stop/
//! restart are owner-scoped. Receipts report only outcomes this build can
//! actually produce (`injected`/`failed`); there is no park/revive lifecycle,
//! so `woken` and `revived` are never claimed.

use std::{collections::HashSet, sync::Arc, time::Duration};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::process::{
	LogOptions, ProcessBackend, ProcessError, ProcessSnapshot, ProcessSpec, ProcessState, ReadySpec,
	RestartPolicy, Signal, WaitRequest, is_terminal_state,
};
use super::{Tool, ToolError};
use crate::{
	mailbox::{
		DEFAULT_IRC_TIMEOUT_MS, IrcMessage, MAIN_AGENT_ID, format_incoming,
	},
	state::{AgentContext, ToolResult},
	supervisor::{CancelAttempt, ChildRecord, ChildStatus, Supervisor},
};

const SUPPORTED_OPS: &str =
	"jobs, cancel, wait, send, inbox, list, start, ps, logs, stop, restart, describe";
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
	/// Process backend snapshot taken when this tool was built. Children must
	/// use the same scoped backend as their `bash` tool, not a later global
	/// swap; `None` falls back to the supervisor's current backend.
	processes: Option<Arc<dyn ProcessBackend>>,
	/// Backend-visible owner label (runner-namespaced), separate from UI ids.
	backend_owner: Option<String>,
}

impl HubTool {
	pub fn root(supervisor: Arc<Supervisor>) -> Self {
		Self {
			supervisor,
			owner: None,
			processes: None,
			backend_owner: None,
		}
	}

	pub fn with_owner(supervisor: Arc<Supervisor>, owner: Option<String>) -> Self {
		Self {
			supervisor,
			owner,
			processes: None,
			backend_owner: None,
		}
	}

	/// Bind the exact process backend and owner label this hub's process ops
	/// must use, mirroring the sibling `BashTool`.
	#[must_use]
	pub fn with_processes(
		mut self,
		processes: Arc<dyn ProcessBackend>,
		backend_owner: Option<String>,
	) -> Self {
		self.processes = Some(processes);
		self.backend_owner = backend_owner;
		self
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
		"Peer messaging (`list`, `send`, `inbox`, `wait`), background-job control (`jobs`, `cancel`, `wait`), and OS process supervision (`start`, `ps`, `logs`, `stop`, `restart`, `describe`)."
	}

	fn schema(&self) -> Value {
		json!({
			"type": "object",
			"properties": {
				"op": {
					"type": "string",
					"description": "Hub operation: send, wait, inbox, list, jobs, cancel, start, ps, logs, stop, restart, describe."
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
				"limit": { "type": "integer", "description": "list: max peer rows; default 32, max 100" },
				"name": { "type": "string", "description": "Process ops: supervised process name (send/wait switch to the process when present)" },
				"application": { "type": "string", "description": "start: executable to launch (PATH-resolved)" },
				"args": { "type": "array", "items": { "type": "string" }, "description": "start: argv passed to application" },
				"env": { "type": "object", "additionalProperties": { "type": "string" }, "description": "start: extra environment variables merged over the session env" },
				"cwd": { "type": "string", "description": "start: working directory (absolute, ~, or relative to the agent cwd)" },
				"pty": { "type": "boolean", "description": "start: allocate a PTY (default true)" },
				"ready": {
					"type": "object",
					"description": "start: readiness conditions; every supplied condition must pass",
					"properties": {
						"log": { "type": "string", "description": "Regex over sanitized output" },
						"port": { "type": "integer", "description": "TCP port that must accept a connection" },
						"host": { "type": "string", "description": "Port probe host; default 127.0.0.1" },
						"timeout": { "type": "number", "description": "Readiness deadline in seconds; default 30, clamped to 0.05-3600" }
					}
				},
				"restart": { "type": "string", "enum": ["no", "on-failure", "always"], "description": "start: restart policy after an unexpected exit" },
				"persist": { "type": "boolean", "description": "start: keep the process alive when the client exits (daemon broker backend only; rejected by the in-process runner)" },
				"detached": { "type": "boolean", "description": "start: detached supervision across client exits, implies persist (daemon broker backend only)" },
				"lines": { "type": "integer", "description": "logs: output lines to return; default 100, max 1000" },
				"head": { "type": "boolean", "description": "logs: read from the start of the log instead of the tail" },
				"grep": { "type": "string", "description": "logs: regex filter applied per line" },
				"follow": { "type": "boolean", "description": "logs: wait for output newer than cursor before reading" },
				"cursor": { "type": "integer", "description": "logs: output cursor from a previous call" },
				"for": { "type": "string", "enum": ["ready", "exit"], "description": "wait: condition to wait for (default exit)" },
				"pattern": { "type": "string", "description": "wait: regex over sanitized output" },
				"text": { "type": "string", "description": "send: stdin text for the process" },
				"enter": { "type": "boolean", "description": "send: append ENTER to text (default true)" },
				"keys": { "type": "array", "items": { "type": "string" }, "description": "send: named keys (ENTER, TAB, ESCAPE, CTRL_C, CTRL_D, UP, DOWN, RIGHT, LEFT)" },
				"signal": { "type": "string", "enum": ["SIGINT", "SIGTERM", "SIGHUP", "SIGQUIT", "SIGKILL"], "description": "send: signal the process tree" },
				"timeout": { "type": "number", "description": "logs/wait/stop: timeout in seconds" }
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
			"wait" => {
				if has_process_name(obj) {
					self.execute_process_wait(obj, ctx).await
				} else {
					self.execute_wait(obj, ctx).await
				}
			},
			"send" => {
				if has_process_name(obj) {
					self.execute_process_send(obj).await
				} else {
					self.execute_send(obj, ctx).await
				}
			},
			"inbox" => self.execute_inbox(obj),
			"list" => self.execute_list(obj),
			"start" => self.execute_process_start(obj, ctx).await,
			"ps" => self.execute_process_list().await,
			"logs" => self.execute_process_logs(obj, ctx).await,
			"stop" => self.execute_process_stop(obj).await,
			"restart" => self.execute_process_restart(obj).await,
			"describe" => self.execute_process_describe(obj).await,
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

/// Structured launch details (the `LaunchToolDetails` subset this build fills).
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ProcessDetails {
	op: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	daemon: Option<ProcessSnapshot>,
	#[serde(skip_serializing_if = "Option::is_none")]
	daemons: Option<Vec<ProcessSnapshot>>,
	#[serde(skip_serializing_if = "Option::is_none")]
	spec: Option<ProcessSpec>,
	#[serde(skip_serializing_if = "Option::is_none")]
	cursor: Option<u64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	timed_out: Option<bool>,
	#[serde(skip_serializing_if = "Option::is_none")]
	state: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	matched: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	ready_timed_out: Option<bool>,
}

impl ProcessDetails {
	fn new(op: &str) -> Self {
		Self {
			op: op.to_string(),
			daemon: None,
			daemons: None,
			spec: None,
			cursor: None,
			timed_out: None,
			state: None,
			matched: None,
			ready_timed_out: None,
		}
	}
}

fn process_details_result(
	content: String,
	details: ProcessDetails,
) -> Result<ToolResult, ToolError> {
	let details = serde_json::to_value(&details)
		.map_err(|error| ToolError::ExecutionFailed(format!("Failed to serialize process details: {error}")))?;
	Ok(ToolResult {
		tool_call_id: String::new(),
		content,
		is_error: false,
		terminate: false,
		details: Some(details),
	})
}

fn process_invalid(message: impl Into<String>) -> ToolError {
	ToolError::InvalidArguments(message.into())
}

fn process_tool_error(error: ProcessError) -> ToolError {
	match error {
		ProcessError::Invalid(message) => ToolError::InvalidArguments(message),
		ProcessError::Failed(message) => ToolError::ExecutionFailed(message),
	}
}

fn has_process_name(obj: &serde_json::Map<String, Value>) -> bool {
	obj
		.get("name")
		.and_then(Value::as_str)
		.is_some_and(|name| !name.trim().is_empty())
}

fn required_process_name<'a>(obj: &'a serde_json::Map<String, Value>) -> Result<&'a str, ToolError> {
	obj
		.get("name")
		.and_then(Value::as_str)
		.map(str::trim)
		.filter(|name| !name.is_empty())
		.ok_or_else(|| process_invalid("This process op requires 'name'"))
}

fn process_bool(
	obj: &serde_json::Map<String, Value>,
	key: &str,
	default: bool,
) -> Result<bool, ToolError> {
	match obj.get(key) {
		None | Some(Value::Null) => Ok(default),
		Some(value) => value
			.as_bool()
			.ok_or_else(|| process_invalid(format!("'{key}' must be a boolean"))),
	}
}

fn process_u64(
	obj: &serde_json::Map<String, Value>,
	key: &str,
) -> Result<Option<u64>, ToolError> {
	match obj.get(key) {
		None | Some(Value::Null) => Ok(None),
		Some(value) => match value.as_u64() {
			Some(number) => Ok(Some(number)),
			None => match value.as_f64() {
				Some(number) if number.is_finite() && number >= 0.0 => Ok(Some(number.floor() as u64)),
				_ => Err(process_invalid(format!("'{key}' must be a non-negative integer"))),
			},
		},
	}
}

fn process_optional_string(
	obj: &serde_json::Map<String, Value>,
	key: &str,
) -> Result<Option<String>, ToolError> {
	match obj.get(key) {
		None | Some(Value::Null) => Ok(None),
		Some(value) => value
			.as_str()
			.map(|text| Some(text.to_string()))
			.ok_or_else(|| process_invalid(format!("'{key}' must be a string"))),
	}
}

/// Seconds→milliseconds with the TypeScript launch bounds (`0.05..=3600`).
fn process_timeout_ms(
	obj: &serde_json::Map<String, Value>,
	fallback_seconds: f64,
) -> Result<u64, ToolError> {
	let seconds = match obj.get("timeout").filter(|value| !value.is_null()) {
		None => fallback_seconds,
		Some(value) => {
			let seconds = value
				.as_f64()
				.ok_or_else(|| process_invalid("'timeout' must be a number of seconds"))?;
			if !seconds.is_finite() {
				return Err(process_invalid("'timeout' must be a finite number"));
			}
			seconds
		},
	};
	Ok((seconds.clamp(0.05, 3_600.0) * 1_000.0).round() as u64)
}

fn process_args(obj: &serde_json::Map<String, Value>) -> Result<Vec<String>, ToolError> {
	match obj.get("args") {
		None | Some(Value::Null) => Ok(Vec::new()),
		Some(Value::Array(items)) => items
			.iter()
			.map(|item| {
				item.as_str()
					.map(str::to_string)
					.ok_or_else(|| process_invalid("'args' must be an array of strings"))
			})
			.collect(),
		Some(_) => Err(process_invalid("'args' must be an array of strings")),
	}
}

fn process_restart(obj: &serde_json::Map<String, Value>) -> Result<RestartPolicy, ToolError> {
	match obj.get("restart") {
		None | Some(Value::Null) => Ok(RestartPolicy::No),
		Some(value) => match value.as_str() {
			Some("no") => Ok(RestartPolicy::No),
			Some("on-failure") => Ok(RestartPolicy::OnFailure),
			Some("always") => Ok(RestartPolicy::Always),
			_ => Err(process_invalid(
				"'restart' must be \"no\", \"on-failure\", or \"always\"",
			)),
		},
	}
}

fn parse_process_ready(value: Option<&Value>) -> Result<Option<ReadySpec>, ToolError> {
	let Some(value) = value.filter(|value| !value.is_null()) else {
		return Ok(None);
	};
	let Some(object) = value.as_object() else {
		return Err(process_invalid("'ready' must be an object"));
	};
	let log = match object.get("log") {
		None | Some(Value::Null) => None,
		Some(value) => Some(
			value
				.as_str()
				.ok_or_else(|| process_invalid("ready.log must be a string"))?
				.to_string(),
		)
		.filter(|pattern| !pattern.trim().is_empty()),
	};
	let port = match object.get("port") {
		None | Some(Value::Null) => None,
		Some(value) => {
			let port = value
				.as_u64()
				.filter(|port| (1..=65_535).contains(port))
				.ok_or_else(|| process_invalid("ready.port must be an integer from 1 to 65535"))?;
			Some(port as u16)
		},
	};
	let host = match object.get("host") {
		None | Some(Value::Null) => None,
		Some(value) => Some(
			value
				.as_str()
				.ok_or_else(|| process_invalid("ready.host must be a string"))?
				.to_string(),
		),
	};
	let timeout_ms = match object.get("timeout").filter(|value| !value.is_null()) {
		None => 30_000,
		Some(value) => {
			let seconds = value
				.as_f64()
				.ok_or_else(|| process_invalid("ready.timeout must be a number of seconds"))?;
			if !seconds.is_finite() {
				return Err(process_invalid("ready.timeout must be a finite number"));
			}
			(seconds.clamp(0.05, 3_600.0) * 1_000.0).round() as u64
		},
	};
	if log.is_none() && port.is_none() {
		return Err(process_invalid("ready requires log or port"));
	}
	Ok(Some(ReadySpec { log, port, host, timeout_ms }))
}

fn hub_now_ms() -> u64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|duration| duration.as_millis() as u64)
		.unwrap_or_default()
}

fn daemon_label(daemon: &ProcessSnapshot) -> String {
	let pid = daemon.pid.map(|pid| format!(" pid={pid}")).unwrap_or_default();
	let exit = daemon
		.exit_code
		.map(|code| format!(" exit={code}"))
		.unwrap_or_default();
	let end = daemon.exited_at.unwrap_or_else(hub_now_ms);
	let flags = if daemon.detached {
		" detached"
	} else if daemon.persist {
		" persistent"
	} else {
		""
	};
	format!(
		"{}: {}{pid}{exit} uptime={} restarts={}{flags}",
		daemon.name,
		daemon.state.as_str(),
		format_duration(end.saturating_sub(daemon.started_at)),
		daemon.restart_count
	)
}

fn ready_pending_summary(daemon: &ProcessSnapshot, ready: Option<&ReadySpec>) -> Vec<String> {
	let mut parts = Vec::new();
	for condition in daemon.ready_pending.clone().unwrap_or_default() {
		if condition == "log" {
			parts.push(match ready.and_then(|ready| ready.log.as_deref()) {
				Some(pattern) => format!("log pattern /{pattern}/ never matched"),
				None => "the log pattern never matched".to_string(),
			});
		} else {
			parts.push(match ready.and_then(|ready| ready.port) {
				Some(port) => format!(
					"port {port} on {} never accepted connections",
					ready.and_then(|ready| ready.host.as_deref()).unwrap_or("127.0.0.1")
				),
				None => "the port never accepted connections".to_string(),
			});
		}
	}
	parts
}

fn restart_label(policy: RestartPolicy) -> &'static str {
	match policy {
		RestartPolicy::No => "no",
		RestartPolicy::OnFailure => "on-failure",
		RestartPolicy::Always => "always",
	}
}

fn launch_key_input(key: &str) -> Option<&'static str> {
	match key {
		"ENTER" => Some("\r"),
		"TAB" => Some("\t"),
		"ESCAPE" => Some("\u{1b}"),
		"CTRL_C" => Some("\u{3}"),
		"CTRL_D" => Some("\u{4}"),
		"UP" => Some("\u{1b}[A"),
		"DOWN" => Some("\u{1b}[B"),
		"RIGHT" => Some("\u{1b}[C"),
		"LEFT" => Some("\u{1b}[D"),
		_ => None,
	}
}

impl HubTool {
	fn processes(&self) -> Arc<dyn ProcessBackend> {
		self.processes
			.clone()
			.unwrap_or_else(|| self.supervisor.processes())
	}

	async fn execute_process_start(
		&self,
		obj: &serde_json::Map<String, Value>,
		ctx: &AgentContext,
	) -> Result<ToolResult, ToolError> {
		let name = required_process_name(obj)?.to_string();
		let application = obj
			.get("application")
			.and_then(Value::as_str)
			.map(str::trim)
			.filter(|application| !application.is_empty())
			.ok_or_else(|| process_invalid("start requires application"))?;
		let cwd_arg = match obj.get("cwd") {
			None | Some(Value::Null) => None,
			Some(Value::String(cwd)) => Some(cwd.as_str()),
			Some(_) => return Err(process_invalid("'cwd' must be a string")),
		};
		let cwd = super::resolve_bash_cwd(cwd_arg, ctx).map_err(ToolError::InvalidArguments)?;
		let env = super::bash_env(obj.get("env"), &ctx.env)?;
		let detached = process_bool(obj, "detached", false)?;
		let spec = ProcessSpec {
			name,
			application: application.to_string(),
			args: process_args(obj)?,
			env,
			cwd: cwd.to_string_lossy().into_owned(),
			pty: if detached {
				false
			} else {
				process_bool(obj, "pty", true)?
			},
			ready: parse_process_ready(obj.get("ready"))?,
			restart: process_restart(obj)?,
			persist: process_bool(obj, "persist", false)? || detached,
			detached,
		};
		let ready_seconds = spec.ready.as_ref().map_or(30.0, |ready| ready.timeout_ms as f64 / 1_000.0);
		let outcome = self
			.processes()
			.start(spec.clone(), self.backend_owner.clone())
			.await
			.map_err(process_tool_error)?;
		let daemon = outcome.snapshot;
		let mut lines = vec![format!(
			"{} {}",
			if daemon.state == ProcessState::Failed {
				"Failed to launch"
			} else {
				"Started"
			},
			daemon_label(&daemon)
		)];
		if daemon.state == ProcessState::Failed
			&& let Some(reason) = daemon.exit_reason.as_deref()
		{
			lines.push(format!("Reason: {reason}"));
		}
		if let Some(matched) = daemon.ready_match.as_deref() {
			lines.push(format!("Ready log matched: {matched}"));
		}
		if outcome.ready_timed_out {
			let pending = ready_pending_summary(&daemon, spec.ready.as_ref());
			let cause = if pending.is_empty() {
				String::new()
			} else {
				format!(": {}", pending.join("; "))
			};
			lines.push(format!(
				"NOT ready — readiness timed out after {ready_seconds}s{cause}. The process is still running (state: {}); follow its logs or stop it.",
				daemon.state.as_str()
			));
		} else if spec.ready.is_some() && daemon.ready_at.is_none() && is_terminal_state(daemon.state) {
			lines.push("Process exited before readiness was observed.".to_string());
		}
		process_details_result(
			lines.join("\n"),
			ProcessDetails {
				daemon: Some(daemon),
				ready_timed_out: Some(outcome.ready_timed_out),
				..ProcessDetails::new("start")
			},
		)
	}

	async fn execute_process_list(&self) -> Result<ToolResult, ToolError> {
		let daemons = self
			.processes()
			.list(self.backend_owner.clone())
			.await
			.map_err(process_tool_error)?;
		let content = if daemons.is_empty() {
			"No processes.".to_string()
		} else {
			daemons
				.iter()
				.map(|daemon| format!("- {}", daemon_label(daemon)))
				.collect::<Vec<_>>()
				.join("\n")
		};
		process_details_result(
			content,
			ProcessDetails { daemons: Some(daemons), ..ProcessDetails::new("list") },
		)
	}

	async fn execute_process_logs(
		&self,
		obj: &serde_json::Map<String, Value>,
		ctx: &AgentContext,
	) -> Result<ToolResult, ToolError> {
		let name = required_process_name(obj)?;
		let lines = process_u64(obj, "lines")?
			.unwrap_or(100)
			.clamp(1, 1_000) as usize;
		let options = LogOptions {
			lines,
			head: process_bool(obj, "head", false)?,
			grep: process_optional_string(obj, "grep")?,
			follow: process_bool(obj, "follow", false)?,
			cursor: process_u64(obj, "cursor")?,
			timeout_ms: process_timeout_ms(obj, 30.0)?,
		};
		let outcome = tokio::select! {
			result = self.processes().logs(name.to_string(), options, self.backend_owner.clone()) => {
				result.map_err(process_tool_error)?
			},
			_ = ctx.cancellation_token.cancelled() => {
				return Err(ToolError::Aborted("hub logs was cancelled".to_string()));
			},
		};
		let newline = if !outcome.text.is_empty() && !outcome.text.ends_with('\n') {
			"\n"
		} else {
			""
		};
		let follow = if outcome.timed_out { "; follow timed out" } else { "" };
		let content = format!(
			"{}{}[{}: {}; cursor={}{follow}]",
			outcome.text,
			newline,
			outcome.name,
			outcome.state.as_str(),
			outcome.cursor
		);
		process_details_result(
			content,
			ProcessDetails {
				cursor: Some(outcome.cursor),
				timed_out: Some(outcome.timed_out),
				state: Some(outcome.state.as_str().to_string()),
				..ProcessDetails::new("logs")
			},
		)
	}

	async fn execute_process_wait(
		&self,
		obj: &serde_json::Map<String, Value>,
		ctx: &AgentContext,
	) -> Result<ToolResult, ToolError> {
		let name = required_process_name(obj)?;
		let for_exit = match obj.get("for") {
			None | Some(Value::Null) => true,
			Some(value) => match value.as_str() {
				Some("exit") => true,
				Some("ready") => false,
				_ => return Err(process_invalid("'for' must be \"ready\" or \"exit\"")),
			},
		};
		let request = WaitRequest {
			for_exit,
			pattern: process_optional_string(obj, "pattern")?,
		};
		let timeout_ms = process_timeout_ms(obj, 30.0)?;
		let outcome = tokio::select! {
			result = self.processes().wait(name.to_string(), request, timeout_ms, self.backend_owner.clone()) => {
				result.map_err(process_tool_error)?
			},
			_ = ctx.cancellation_token.cancelled() => {
				return Err(ToolError::Aborted("hub wait was cancelled".to_string()));
			},
		};
		let mut lines = vec![daemon_label(&outcome.snapshot)];
		if let Some(matched) = outcome.matched.as_deref() {
			lines.push(format!("Matched: {matched}"));
		}
		if outcome.timed_out {
			let pending = ready_pending_summary(&outcome.snapshot, None);
			lines.push(if pending.is_empty() {
				"Wait timed out.".to_string()
			} else {
				format!("Wait timed out (still waiting on: {}).", pending.join("; "))
			});
		}
		process_details_result(
			lines.join("\n"),
			ProcessDetails {
				daemon: Some(outcome.snapshot),
				matched: outcome.matched,
				timed_out: Some(outcome.timed_out),
				..ProcessDetails::new("wait")
			},
		)
	}

	async fn execute_process_send(
		&self,
		obj: &serde_json::Map<String, Value>,
	) -> Result<ToolResult, ToolError> {
		let name = required_process_name(obj)?;
		let text = process_optional_string(obj, "text")?.unwrap_or_default();
		let mut data = text.clone();
		if !text.is_empty() && process_bool(obj, "enter", true)? {
			data.push('\r');
		}
		match obj.get("keys") {
			None | Some(Value::Null) => {},
			Some(Value::Array(keys)) => {
				for raw in keys {
					let raw = raw
						.as_str()
						.ok_or_else(|| process_invalid("'keys' must be an array of strings"))?;
					let key = raw.trim().to_uppercase();
					let input = launch_key_input(&key)
						.ok_or_else(|| process_invalid(format!("Unsupported launch key {raw}")))?;
					data.push_str(input);
				}
			},
			Some(_) => return Err(process_invalid("'keys' must be an array of strings")),
		}
		let signal = match obj.get("signal") {
			None | Some(Value::Null) => None,
			Some(value) => {
				let raw = value
					.as_str()
					.ok_or_else(|| process_invalid("'signal' must be a string"))?;
				Some(
					Signal::parse(raw)
						.ok_or_else(|| process_invalid(format!("Unsupported signal {raw}")))?,
				)
			},
		};
		let data = if data.is_empty() { None } else { Some(data) };
		let daemon = self
			.processes()
			.send(name.to_string(), data, signal, self.backend_owner.clone())
			.await
			.map_err(process_tool_error)?;
		process_details_result(
			format!("Sent input to {}", daemon_label(&daemon)),
			ProcessDetails { daemon: Some(daemon), ..ProcessDetails::new("send") },
		)
	}

	async fn execute_process_stop(
		&self,
		obj: &serde_json::Map<String, Value>,
	) -> Result<ToolResult, ToolError> {
		let name = required_process_name(obj)?;
		let daemon = self
			.processes()
			.stop(name.to_string(), process_timeout_ms(obj, 5.0)?, self.backend_owner.clone())
			.await
			.map_err(process_tool_error)?;
		process_details_result(
			format!("Stopped {}", daemon_label(&daemon)),
			ProcessDetails { daemon: Some(daemon), ..ProcessDetails::new("stop") },
		)
	}

	async fn execute_process_restart(
		&self,
		obj: &serde_json::Map<String, Value>,
	) -> Result<ToolResult, ToolError> {
		let name = required_process_name(obj)?;
		let daemon = self
			.processes()
			.restart(name.to_string(), self.backend_owner.clone())
			.await
			.map_err(process_tool_error)?;
		process_details_result(
			format!("Restarted {}", daemon_label(&daemon)),
			ProcessDetails { daemon: Some(daemon), ..ProcessDetails::new("restart") },
		)
	}

	async fn execute_process_describe(
		&self,
		obj: &serde_json::Map<String, Value>,
	) -> Result<ToolResult, ToolError> {
		let name = required_process_name(obj)?;
		let (daemon, spec) = self
			.processes()
			.describe(name.to_string(), self.backend_owner.clone())
			.await
			.map_err(process_tool_error)?;
		let command = std::iter::once(spec.application.as_str())
			.chain(spec.args.iter().map(String::as_str))
			.collect::<Vec<_>>()
			.join(" ");
		let content = format!(
			"{}\nCommand: {command}\nCwd: {}\nPTY: {}; restart={}; persist={}; detached={}",
			daemon_label(&daemon),
			spec.cwd,
			spec.pty,
			restart_label(spec.restart),
			spec.persist,
			spec.detached
		);
		process_details_result(
			content,
			ProcessDetails {
				daemon: Some(daemon),
				spec: Some(spec),
				..ProcessDetails::new("describe")
			},
		)
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

#[cfg(test)]
mod backend_snapshot_tests {
	use std::sync::Arc;

	use async_trait::async_trait;

	use super::*;
	use crate::{
		provider::{ChunkStream, ModelProvider, ProviderError, ToolChoice},
		state::Message,
		supervisor::DelegationLimits,
		tools::{process::ProcessManager, registry::ToolRegistry},
	};

	struct QuietProvider;

	#[async_trait]
	impl ModelProvider for QuietProvider {
		async fn complete(
			&self,
			_messages: &[Message],
			_tools: &[serde_json::Value],
			_tool_choice: &ToolChoice,
		) -> Result<Message, ProviderError> {
			Err(ProviderError::Protocol("unused".to_string()))
		}

		async fn stream(
			&self,
			_messages: &[Message],
			_tools: &[serde_json::Value],
			_tool_choice: &ToolChoice,
		) -> Result<ChunkStream, ProviderError> {
			Err(ProviderError::Protocol("unused".to_string()))
		}
	}

	#[test]
	fn hub_process_ops_use_the_bound_backend_snapshot() {
		let supervisor = Supervisor::with_selection(
			ToolRegistry::with_builtins(),
			Arc::new(QuietProvider),
			None,
			DelegationLimits::default(),
			true,
			true,
		);
		let snapshot: Arc<dyn ProcessBackend> =
			Arc::new(ProcessManager::new(std::env::temp_dir().join("omp-hub-test")));
		let tool = HubTool::root(Arc::clone(&supervisor))
			.with_processes(Arc::clone(&snapshot), None);
		assert!(Arc::ptr_eq(&tool.processes(), &snapshot));
		let replacement: Arc<dyn ProcessBackend> =
			Arc::new(ProcessManager::new(std::env::temp_dir().join("omp-hub-test-2")));
		supervisor.set_processes(replacement);
		assert!(
			Arc::ptr_eq(&tool.processes(), &snapshot),
			"child hub must keep the spawn-time backend"
		);
	}
}
