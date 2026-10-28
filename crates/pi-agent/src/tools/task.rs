use std::{
	collections::HashMap,
	sync::Arc,
	time::Instant,
};

use async_trait::async_trait;
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::{Tool, ToolError};
use crate::{
	loop_module::{AgentError, AgentLoop, SteeringEvent},
	state::{AgentConfig, AgentContext, AgentState, Message, ToolResult, TurnResult},
	supervisor::{ChildGuard, Supervisor, cap_result_text},
};

#[derive(Clone)]
pub struct TaskTool {
	pub supervisor: Arc<Supervisor>,
	pub depth: i32,
	/// Agent id that owns the children this tool spawns (`None` at the root).
	pub parent_id: Option<String>,
	pub semaphore: Arc<Semaphore>,
}

impl TaskTool {
	pub fn new(supervisor: Arc<Supervisor>, depth: i32) -> Self {
		Self::with_parent(supervisor, depth, None)
	}

	pub fn with_parent(
		supervisor: Arc<Supervisor>,
		depth: i32,
		parent_id: Option<String>,
	) -> Self {
		let limits = supervisor.limits();
		let permits = if limits.max_concurrency == 0 {
			Semaphore::MAX_PERMITS
		} else {
			limits.max_concurrency
		};
		Self {
			supervisor,
			depth,
			parent_id,
			semaphore: Arc::new(Semaphore::new(permits)),
		}
	}

	pub fn with_semaphore(supervisor: Arc<Supervisor>, depth: i32, semaphore: Arc<Semaphore>) -> Self {
		Self {
			supervisor,
			depth,
			parent_id: None,
			semaphore,
		}
	}

	async fn run_single_task(
		&self,
		index: usize,
		item: TaskItemSpec,
		ctx: &AgentContext,
	) -> SingleResult {
		let start = Instant::now();
		let child_name = item.name.clone();
		let Some((child_id, child_cancel)) = self.supervisor.reserve_child(
			&child_name,
			index,
			&item.agent,
			&item.task,
			self.parent_id.as_deref(),
		) else {
			return SingleResult {
				index,
				id: child_name.clone(),
				name: child_name,
				agent: "task".to_string(),
				agent_source: "bundled".to_string(),
				task: item.task,
				exit_code: 1,
				output: "(no output)".to_string(),
				stderr: String::new(),
				truncated: false,
				duration_ms: 0,
				tokens: 0,
				requests: 0,
				error: Some(format!(
					"result capacity exhausted ({} outstanding); wait for or consume finished jobs first",
					crate::supervisor::MAX_OUTSTANDING_RESULTS
				)),
				aborted: None,
				abort_reason: None,
				extracted_tool_data: None,
			};
		};
		let mut reservation = ReservationGuard::new(
			self.supervisor.clone(),
			child_id.clone(),
			child_cancel.clone(),
		);

		let _permit = match tokio::select! {
			biased;
			_ = child_cancel.cancelled() => Err("cancelled by hub"),
			_ = ctx.cancellation_token.cancelled() => Err("aborted by parent"),
			permit = self.semaphore.acquire() => permit.map_err(|_| "aborted by parent"),
		} {
			Ok(permit) => permit,
			Err(reason) => {
				self.supervisor.mark_aborted(&child_id, reason);
				self.supervisor.unregister_active_child(&child_id);
				return SingleResult {
					index,
					id: child_id,
					name: child_name,
					agent: "task".to_string(),
					agent_source: "bundled".to_string(),
					task: item.task,
					exit_code: 1,
					output: "(no output)".to_string(),
					stderr: String::new(),
					truncated: false,
					duration_ms: start.elapsed().as_millis() as u64,
					tokens: 0,
					requests: 0,
					error: None,
					aborted: Some(true),
					abort_reason: Some(reason.to_string()),
					extracted_tool_data: None,
				};
			}
		};

		// A permit can be granted at the same moment the reservation is cancelled.
		if child_cancel.is_cancelled() {
			self.supervisor.mark_aborted(&child_id, "cancelled by hub");
			self.supervisor.unregister_active_child(&child_id);
			return SingleResult {
				index,
				id: child_id,
				name: child_name,
				agent: "task".to_string(),
				agent_source: "bundled".to_string(),
				task: item.task,
				exit_code: 1,
				output: "(no output)".to_string(),
				stderr: String::new(),
				truncated: false,
				duration_ms: start.elapsed().as_millis() as u64,
				tokens: 0,
				requests: 0,
				error: None,
				aborted: Some(true),
				abort_reason: Some("cancelled by hub".to_string()),
				extracted_tool_data: None,
			};
		}

		let (steering_tx, steering_rx) = tokio::sync::mpsc::channel(16);

		let child_config = AgentConfig {
			max_turns: 20,
			context_limit: 100_000,
			system_prompt: None,
			cwd: ctx.cwd.clone(),
		};

		let child_provider = self.supervisor.provider();
		let child_approver = self.supervisor.child_approver();
		let child_registry = self.supervisor.child_registry(self.depth + 1, Some(&child_id));

		let full_prompt = if let Some(context) = &item.context {
			format!("{context}\n\n{}", item.task)
		} else {
			item.task.clone()
		};

		let child_cancel_for_loop = child_cancel.clone();
		let mut task_handle = tokio::spawn(async move {
			let child_loop = match child_approver {
				Some(approver) => AgentLoop::with_approver(
					child_config,
					child_provider,
					child_registry,
					approver,
				),
				None => AgentLoop::new(child_config, child_provider, child_registry),
			};
			let mut child_state = AgentState::default();
			child_loop
				.run_dialog_message_with_cancel(
					Message::user(full_prompt),
					&mut child_state,
					Some(steering_rx),
					None,
					child_cancel_for_loop,
				)
				.await
		});

		if !self.supervisor.register_active_child(
			&child_id,
			steering_tx.clone(),
			task_handle.abort_handle(),
			child_cancel.clone(),
		) {
			// The reservation was cancelled between the permit check and this
			// registration; never let the already-spawned loop run unowned.
			child_cancel.cancel();
			task_handle.abort();
			let _ = task_handle.await;
			self.supervisor.mark_aborted(&child_id, "cancelled by hub");
			self.supervisor.unregister_active_child(&child_id);
			return SingleResult {
				index,
				id: child_id,
				name: child_name,
				agent: "task".to_string(),
				agent_source: "bundled".to_string(),
				task: item.task,
				exit_code: 1,
				output: "(no output)".to_string(),
				stderr: String::new(),
				truncated: false,
				duration_ms: start.elapsed().as_millis() as u64,
				tokens: 0,
				requests: 0,
				error: None,
				aborted: Some(true),
				abort_reason: Some("cancelled by hub".to_string()),
				extracted_tool_data: None,
			};
		}

		let mut guard = ChildGuard::new(
			task_handle.abort_handle(),
			steering_tx.clone(),
			self.supervisor.clone(),
			child_id.clone(),
			child_cancel.clone(),
		);
		// The running task is now owned by `ChildGuard`; no await occurred since
		// `register_active_child`, so nothing can drop this future in the gap.
		reservation.disarm();

		let outcome = tokio::select! {
			_ = ctx.cancellation_token.cancelled() => {
				child_cancel.cancel();
				let _ = steering_tx.try_send(SteeringEvent::Interrupt);
				None
			}
			res = &mut task_handle => Some(join_dialog_result(res)),
		};
		// Await outside the select so only one branch borrows the handle.
		let outcome = match outcome {
			Some(outcome) => outcome,
			None => match tokio::time::timeout(CHILD_CANCEL_GRACE, &mut task_handle).await {
				Ok(joined) => join_dialog_result(joined),
				Err(_) => {
					guard.handle.abort();
					let _ = task_handle.await;
					Err(AgentError::Interrupted)
				},
			},
		};

		guard.defuse();
		self.supervisor.unregister_active_child(&child_id);

		let duration_ms = start.elapsed().as_millis() as u64;
		let outstanding_id = child_id.clone();

		let result = match outcome {
			Ok(turn_results) => {
				let requests = turn_results.len();

				let (output, output_truncated) = cap_result_text(
					&turn_results
						.iter()
						.rev()
						.find_map(|turn| {
							let trimmed = turn.message.content.trim();
							if !trimmed.is_empty() {
								Some(trimmed.to_string())
							} else {
								None
							}
						})
						.unwrap_or_else(|| "(no output)".to_string()),
				);

				let mut extracted_tool_data = None;
				for turn in &turn_results {
					for tr in &turn.tool_results {
						if tr.terminate {
							if let Ok(val) = serde_json::from_str::<Value>(&tr.content) {
								let mut map = HashMap::new();
								map.insert("yield".to_string(), vec![val]);
								extracted_tool_data = Some(map);
							}
							break;
						}
					}
					if extracted_tool_data.is_some() {
						break;
					}
				}

				let mut is_aborted = false;
				let mut abort_reason = None;
				let is_error = false;
				let error_msg = None;

				if let Some(map) = &extracted_tool_data {
					if let Some(yields) = map.get("yield") {
						for y in yields {
							if let Some(status) = y.get("status").and_then(Value::as_str) {
								if status == "aborted" {
									is_aborted = true;
									if let Some(err) = y.get("error").and_then(Value::as_str) {
										abort_reason = Some(err.to_string());
									}
								}
							}
						}
					}
				}

				let exit_code = if is_aborted || is_error { 1 } else { 0 };

				if is_aborted {
					self.supervisor.mark_aborted(
						&child_id,
						abort_reason.as_deref().unwrap_or("aborted by yield"),
					);
				} else {
					// A child that yields carries its real result in the terminal
					// tool payload, not in assistant text. Surface that data in the
					// hub snapshot instead of "(no output)".
					let record_output = extracted_tool_data
						.as_ref()
						.and_then(|map| map.get("yield"))
						.and_then(|yields| yields.first())
						.and_then(|yielded| yielded.get("data"))
						.map(Value::to_string)
						.unwrap_or_else(|| output.clone());
					self
						.supervisor
						.mark_completed(&child_id, duration_ms, &record_output);
				}

				SingleResult {
					index,
					id: child_id,
					name: child_name,
					agent: "task".to_string(),
					agent_source: "bundled".to_string(),
					task: item.task,
					exit_code,
					output,
					stderr: String::new(),
					truncated: output_truncated,
					duration_ms,
					tokens: 0,
					requests,
					error: error_msg,
					aborted: if is_aborted { Some(true) } else { None },
					abort_reason,
					extracted_tool_data,
				}
			},
			Err(AgentError::Interrupted) => {
				self.supervisor.mark_aborted(&child_id, "aborted by parent");
				SingleResult {
					index,
					id: child_id,
					name: child_name,
					agent: "task".to_string(),
					agent_source: "bundled".to_string(),
					task: item.task,
					exit_code: 1,
					output: "(no output)".to_string(),
					stderr: String::new(),
					truncated: false,
					duration_ms,
					tokens: 0,
					requests: 0,
					error: None,
					aborted: Some(true),
					abort_reason: Some("aborted by parent".to_string()),
					extracted_tool_data: None,
				}
			},
			Err(err) => {
				let err_msg = err.to_string();
				self.supervisor.mark_failed(&child_id, duration_ms, &err_msg);
				SingleResult {
					index,
					id: child_id,
					name: child_name,
					agent: "task".to_string(),
					agent_source: "bundled".to_string(),
					task: item.task,
					exit_code: 1,
					output: "(no output)".to_string(),
					stderr: err_msg.clone(),
					truncated: false,
					duration_ms,
					tokens: 0,
					requests: 0,
					error: Some(err_msg),
					aborted: None,
					abort_reason: None,
					extracted_tool_data: None,
				}
			},
		};
		self.supervisor.release_outstanding(&outstanding_id);
		result
	}

	/// Reserve and detach one background child. The returned id is the real
	/// supervisor identity and doubles as the job id, so `hub jobs/cancel/wait`
	/// address it immediately, before the child acquires a permit or runs.
	fn spawn_background_task(
		&self,
		index: usize,
		item: TaskItemSpec,
		ctx: &AgentContext,
	) -> Result<String, String> {
		let child_name = item.name.clone();
		let Some((child_id, child_cancel)) = self.supervisor.reserve_child(
			&child_name,
			index,
			&item.agent,
			&item.task,
			self.parent_id.as_deref(),
		) else {
			return Err(format!(
				"{child_name}: result capacity exhausted ({} outstanding); wait for or consume finished jobs first",
				crate::supervisor::MAX_OUTSTANDING_RESULTS
			));
		};
		let (steering_tx, steering_rx) = tokio::sync::mpsc::channel(16);
		let (start_tx, start_rx) = tokio::sync::oneshot::channel::<()>();
		let supervisor = self.supervisor.clone();
		// Per-`TaskTool` (per-session) permits: nested spawns use their own
		// semaphore, so a parent holding a permit while waiting for descendants
		// cannot deadlock when every parent permit is occupied.
		let semaphore = self.semaphore.clone();
		let depth = self.depth;
		let cwd = ctx.cwd.clone();
		let task_id = child_id.clone();
		let cancel = child_cancel.clone();
		let mut lifecycle = SettlementGuard::new(supervisor.clone(), task_id.clone());

		let handle = tokio::spawn(async move {
			// Wait until the reservation is registered as active, so an early
			// cancel can abort this task before it ever reaches the model.
			if start_rx.await.is_err() {
				supervisor.mark_aborted(&task_id, "cancelled before start");
				supervisor.unregister_active_child(&task_id);
				return;
			}
			let start = Instant::now();
			let permit = tokio::select! {
				biased;
				_ = cancel.cancelled() => None,
				permit = semaphore.clone().acquire_owned() => permit.ok(),
			};
			let Some(_permit) = permit else {
				supervisor.mark_aborted(&task_id, "cancelled by hub");
				supervisor.unregister_active_child(&task_id);
				return;
			};
			if cancel.is_cancelled() {
				supervisor.mark_aborted(&task_id, "cancelled by hub");
				supervisor.unregister_active_child(&task_id);
				return;
			}
			let outcome = run_background_child(
				supervisor.clone(),
				depth,
				task_id.clone(),
				item,
				cwd,
				steering_rx,
				cancel.clone(),
			)
			.await;
			supervisor.unregister_active_child(&task_id);
			settle_background_result(supervisor, task_id, start, outcome);
			lifecycle.disarm();
		});

		if !self.supervisor.register_active_child(
			&child_id,
			steering_tx,
			handle.abort_handle(),
			child_cancel.clone(),
		) {
			child_cancel.cancel();
			handle.abort();
			self.supervisor.mark_aborted(&child_id, "cancelled by hub");
			self.supervisor.unregister_active_child(&child_id);
			return Err(format!("{child_name}: cancelled before start"));
		}
		let _ = start_tx.send(());
		Ok(child_id)
	}
}

/// Build a child loop with fresh read/edit state, its own result sink, and its
/// own peer inbox. Runs detached from the parent turn so REPL turns do not kill
/// a background job.
async fn run_background_child(
	supervisor: Arc<Supervisor>,
	depth: i32,
	child_id: String,
	item: TaskItemSpec,
	cwd: std::path::PathBuf,
	steering_rx: tokio::sync::mpsc::Receiver<SteeringEvent>,
	cancel: CancellationToken,
) -> Result<Vec<TurnResult>, AgentError> {
	let child_config = AgentConfig {
		max_turns: 20,
		context_limit: 100_000,
		system_prompt: None,
		cwd,
	};
	let child_provider = supervisor.provider();
	let child_approver = supervisor.child_approver();
	let child_registry = supervisor.child_registry(depth + 1, Some(&child_id));
	let full_prompt = if let Some(context) = &item.context {
		format!("{context}\n\n{}", item.task)
	} else {
		item.task.clone()
	};
	let child_loop = match child_approver {
		Some(approver) => {
			AgentLoop::with_approver(child_config, child_provider, child_registry, approver)
		},
		None => AgentLoop::new(child_config, child_provider, child_registry),
	};
	let child_loop = child_loop
		.with_background_results(supervisor.owner_results_sink(Some(&child_id)))
		.with_peer_inbox(supervisor.mailbox(), child_id.clone());
	let mut child_state = AgentState::default();
	child_loop
		.run_dialog_message_with_cancel(
			Message::user(full_prompt),
			&mut child_state,
			Some(steering_rx),
			None,
			cancel,
		)
		.await
}

/// Summarize a finished child: final assistant text, abort flag/reason, and a
/// terminal `yield` payload when one exists.
fn summarize_turns(turns: &[TurnResult]) -> (String, bool, Option<String>) {
	let output = turns
		.iter()
		.rev()
		.find_map(|turn| {
			let trimmed = turn.message.content.trim();
			if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
		})
		.unwrap_or_else(|| "(no output)".to_string());
	let mut aborted = false;
	let mut reason = None;
	for turn in turns {
		for result in &turn.tool_results {
			if result.terminate {
				if let Ok(value) = serde_json::from_str::<Value>(&result.content)
					&& value.get("status").and_then(Value::as_str) == Some("aborted")
				{
					aborted = true;
					reason = value
						.get("error")
						.and_then(Value::as_str)
						.map(str::to_string);
				}
			}
		}
	}
	(output, aborted, reason)
}

fn settle_background_result(
	supervisor: Arc<Supervisor>,
	id: String,
	start: Instant,
	outcome: Result<Vec<TurnResult>, AgentError>,
) {
	let duration_ms = start.elapsed().as_millis() as u64;
	match outcome {
		Ok(turns) => {
			let (output, aborted, reason) = summarize_turns(&turns);
			let (output, _) = cap_result_text(&output);
			if aborted {
				supervisor.mark_aborted(
					&id,
					reason.as_deref().unwrap_or("aborted by yield"),
				);
				return;
			}
			let envelope =
				task_result_envelope(&id, "task", "completed", duration_ms, &output);
			supervisor.mark_completed(&id, duration_ms, &envelope);
			supervisor.enqueue_delivery(&id, envelope);
		},
		Err(AgentError::Interrupted) => {
			supervisor.mark_aborted(&id, "aborted");
		},
		Err(error) => {
			let message = error.to_string();
			let envelope =
				task_result_envelope(&id, "task", "failed (exit 1)", duration_ms, &message);
			supervisor.mark_failed(&id, duration_ms, &envelope);
			supervisor.enqueue_delivery(&id, envelope);
		},
	}
}

/// `<task-result>` envelope from `prompts/tools/task-summary.md`. No
/// `agent://`/`history://` pointer is emitted: this build has no transcript
/// resolver, so a link would be fake.
fn task_result_envelope(
	id: &str,
	agent: &str,
	status: &str,
	duration_ms: u64,
	output: &str,
) -> String {
	format!(
		"<task-result id=\"{id}\" agent=\"{agent}\" status=\"{status}\" duration=\"{}\">\n<output>\n{output}\n</output>\n</task-result>",
		format_duration_ms(duration_ms)
	)
}

fn format_duration_ms(milliseconds: u64) -> String {
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

fn coordination_hint(ids: &[String]) -> String {
	let dm = if ids.len() == 1 {
		format!(
			"DM `{}` via `hub` send to coordinate while it runs; use `hub` only to inspect (`jobs`), wait, or cancel a stuck task.",
			ids[0]
		)
	} else {
		"DM these ids via `hub` send to coordinate while they run; use `hub` only to inspect (`jobs`), wait, or cancel a stuck task by id.".to_string()
	};
	format!("{dm}\n{}", TASK_ASYNC_CONTRACT)
}

/// Upstream `prompts/tools/task-async-contract.md`, verbatim.
const TASK_ASYNC_CONTRACT: &str = "No polling needed.\n\nSettled-job inspection: `hub jobs` | `hub wait` delivers its snapshot → no duplicate `async-result`.\n\n`completed`: subagent yielded successfully; claimed artifacts unverified.";

/// Owns a queued reservation until the spawned `ChildGuard` takes over. If the
/// batch future is dropped while waiting for a semaphore permit, this drop still
/// settles the record and releases the reservation instead of leaking a
/// `Running` id forever.
struct ReservationGuard {
	supervisor: Arc<Supervisor>,
	id:         String,
	token:      tokio_util::sync::CancellationToken,
	armed:      bool,
}

impl ReservationGuard {
	fn new(
		supervisor: Arc<Supervisor>,
		id: String,
		token: tokio_util::sync::CancellationToken,
	) -> Self {
		Self { supervisor, id, token, armed: true }
	}

	/// Called only after `ChildGuard` owns the running task; no await may sit
	/// between registration and this handoff.
	fn disarm(&mut self) {
		self.armed = false;
	}
}

impl Drop for ReservationGuard {
	fn drop(&mut self) {
		if !self.armed {
			return;
		}
		self.token.cancel();
		self.supervisor.mark_aborted(&self.id, "aborted by parent");
		self.supervisor.unregister_active_child(&self.id);
	}
}

/// Lifecycle guard for a detached background child. A panic inside the child
/// future unwinds past the normal unregister/settle path; this guard then
/// settles the record as aborted and frees the admission slot (unless a body
/// was already queued). Disarmed only after normal settlement.
struct SettlementGuard {
	supervisor: Arc<Supervisor>,
	id:         String,
	armed:      bool,
}

impl SettlementGuard {
	fn new(supervisor: Arc<Supervisor>, id: String) -> Self {
		Self { supervisor, id, armed: true }
	}

	fn disarm(&mut self) {
		self.armed = false;
	}
}

impl Drop for SettlementGuard {
	fn drop(&mut self) {
		if !self.armed {
			return;
		}
		if !self.supervisor.mark_aborted(&self.id, "panicked before settlement") {
			self.supervisor.release_outstanding_if_undelivered(&self.id);
		}
		self.supervisor.unregister_active_child(&self.id);
	}
}

/// How long a cancelled child may take to unwind cooperative tool cleanup
/// (the shell's own graceful-kill window) before it is hard-aborted.
const CHILD_CANCEL_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

fn join_dialog_result(
	joined: Result<Result<Vec<crate::state::TurnResult>, AgentError>, tokio::task::JoinError>,
) -> Result<Vec<crate::state::TurnResult>, AgentError> {
	match joined {
		Ok(dialog_res) => dialog_res,
		Err(join_err) => {
			if join_err.is_cancelled() {
				Err(AgentError::Interrupted)
			} else {
				Err(AgentError::Internal(join_err.to_string()))
			}
		},
	}
}

const UNSUPPORTED_FIELDS: &[&str] = &["outputSchema", "schemaMode", "effort", "isolated"];

fn check_unsupported_fields(obj: &serde_json::Map<String, Value>) -> Result<(), ToolError> {
	for &field in UNSUPPORTED_FIELDS {
		if obj.contains_key(field) {
			return Err(ToolError::InvalidArguments(format!(
				"'{field}' is not supported in this build"
			)));
		}
	}
	Ok(())
}

fn validate_name(name: &str) -> Result<(), ToolError> {
	if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
		Ok(())
	} else {
		Err(ToolError::InvalidArguments(format!(
			"Invalid name '{name}': must match [A-Za-z0-9_-]+"
		)))
	}
}

fn validate_agent(val: Option<&Value>) -> Result<(), ToolError> {
	if let Some(agent_val) = val {
		match agent_val.as_str() {
			Some("task") => Ok(()),
			Some(other) => Err(ToolError::InvalidArguments(format!(
				"Unsupported agent '{other}': only 'task' is supported in this build"
			))),
			None => Err(ToolError::InvalidArguments(
				"'agent' must be a string".to_string(),
			)),
		}
	} else {
		Ok(())
	}
}

struct TaskItemSpec {
	name: String,
	agent: String,
	task: String,
	context: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SingleResult {
	pub index: usize,
	/// Supervisor-generated identity, used for `hub cancel`.
	pub id: String,
	/// Model-supplied display label (may repeat across children).
	pub name: String,
	pub agent: String,
	pub agent_source: String,
	pub task: String,
	pub exit_code: i32,
	pub output: String,
	pub stderr: String,
	pub truncated: bool,
	pub duration_ms: u64,
	pub tokens: u64,
	pub requests: usize,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub error: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub aborted: Option<bool>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub abort_reason: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub extracted_tool_data: Option<HashMap<String, Vec<Value>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AsyncTaskState {
	pub state: String,
	pub job_id: String,
	#[serde(rename = "type")]
	pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskToolDetails {
	pub project_agents_dir: Option<String>,
	pub results: Vec<SingleResult>,
	pub total_duration_ms: u64,
	/// Model-facing spawn summary for a background call.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub message: Option<String>,
	/// Present only for a background call that returned before the jobs ran.
	#[serde(rename = "async", skip_serializing_if = "Option::is_none")]
	pub async_state: Option<AsyncTaskState>,
}

#[async_trait]
impl Tool for TaskTool {
	fn name(&self) -> &str {
		"task"
	}

	fn description(&self) -> &str {
		"Spawn one or more subagents to perform tasks."
	}

	fn schema(&self) -> Value {
		json!({
			"type": "object",
			"properties": {
				"task": { "type": "string", "description": "The task instructions for a single agent." },
				"name": { "type": "string", "description": "Stable agent name." },
				"agent": { "type": "string", "description": "Agent type to spawn." },
				"context": { "type": "string", "description": "Shared background context for batch tasks." },
				"tasks": {
					"type": "array",
					"description": "Batch of subagent tasks.",
					"items": {
						"type": "object",
						"properties": {
							"name": { "type": "string" },
							"agent": { "type": "string" },
							"task": { "type": "string" }
						},
						"required": ["task"]
					}
				}
			}
		})
	}

	fn is_exclusive(&self) -> bool {
		false
	}

	async fn execute(&self, args: &Value, ctx: &AgentContext) -> Result<ToolResult, ToolError> {
		let limits = self.supervisor.limits();
		if limits.max_depth >= 0 && self.depth >= limits.max_depth {
			return Err(ToolError::ExecutionFailed(format!(
				"Cannot spawn another agent at task depth {}; maximum depth is {}.",
				self.depth, limits.max_depth
			)));
		}

		let obj = args
			.as_object()
			.ok_or_else(|| ToolError::InvalidArguments("Arguments must be an object".to_string()))?;

		check_unsupported_fields(obj)?;

		let items = if obj.contains_key("tasks") {
			let tasks_arr = obj
				.get("tasks")
				.and_then(Value::as_array)
				.ok_or_else(|| ToolError::InvalidArguments("'tasks' must be an array".to_string()))?;

			if tasks_arr.is_empty() {
				return Err(ToolError::InvalidArguments(
					"'tasks' array cannot be empty".to_string(),
				));
			}

			let context = obj.get("context").and_then(Value::as_str).map(|s| s.to_string());

			let mut items = Vec::with_capacity(tasks_arr.len());
			for item_val in tasks_arr {
				let item_obj = item_val.as_object().ok_or_else(|| {
					ToolError::InvalidArguments("Each item in 'tasks' must be an object".to_string())
				})?;

				check_unsupported_fields(item_obj)?;
				validate_agent(item_obj.get("agent"))?;

				let task_str = item_obj
					.get("task")
					.and_then(Value::as_str)
					.ok_or_else(|| ToolError::InvalidArguments("Missing 'task' in tasks item".to_string()))?;

				let name = if let Some(n) = item_obj.get("name").and_then(Value::as_str) {
					validate_name(n)?;
					n.to_string()
				} else {
					format!("task-{}", self.supervisor.next_seq())
				};

				items.push(TaskItemSpec {
					name,
					agent: "task".to_string(),
					task: task_str.to_string(),
					context: context.clone(),
				});
			}
			items
		} else {
			let task_str = obj
				.get("task")
				.and_then(Value::as_str)
				.ok_or_else(|| ToolError::InvalidArguments("Missing 'task' argument".to_string()))?;

			validate_agent(obj.get("agent"))?;

			let name = if let Some(n) = obj.get("name").and_then(Value::as_str) {
				validate_name(n)?;
				n.to_string()
			} else {
				format!("task-{}", self.supervisor.next_seq())
			};

			let context = obj.get("context").and_then(Value::as_str).map(|s| s.to_string());

			vec![TaskItemSpec {
				name,
				agent: "task".to_string(),
				task: task_str.to_string(),
				context,
			}]
		};

		// Bound the batch before allocating records or spawning any child: an
		// oversized batch is rejected as a whole with an explicit capacity error
		// instead of silently queueing an unbounded backlog of result bodies.
		let capacity = self.supervisor.outstanding_capacity();
		if items.len() > capacity {
			return Err(ToolError::ExecutionFailed(format!(
				"task result capacity exhausted: {} item(s) requested, {} slot(s) available; wait for or consume finished jobs first",
				items.len(),
				capacity
			)));
		}

		if self.supervisor.background_ready() {
			let mut started = Vec::new();
			let mut failures = Vec::new();
			for (index, item) in items.into_iter().enumerate() {
				match self.spawn_background_task(index, item, ctx) {
					Ok(id) => started.push(id),
					Err(error) => failures.push(error),
				}
			}

			if started.is_empty() {
				let message = if failures.is_empty() {
					"Failed to start background task job".to_string()
				} else {
					format!(
						"Failed to start background task job{}: {}",
						if failures.len() == 1 { "" } else { "s" },
						failures.join("; ")
					)
				};
				return Ok(ToolResult {
					tool_call_id: String::new(),
					content: message,
					is_error: false,
					terminate: false,
					details: None,
				});
			}

			let schedule_failure_summary = if failures.is_empty() {
				String::new()
			} else {
				format!(
					" Failed to schedule {} spawn{}: {}.",
					failures.len(),
					if failures.len() == 1 { "" } else { "s" },
					failures.join("; ")
				)
			};
			let coordination_hint = coordination_hint(&started);
			let content = if started.len() == 1 {
				format!(
					"Spawned agent `{}` (job `{}`). Its result auto-delivers on yield unless a settled `hub jobs`/`wait` snapshot consumes it first. {coordination_hint}",
					started[0], started[0]
				)
			} else {
				let listing = started
					.iter()
					.map(|id| format!("- `{id}` (job `{id}`)"))
					.collect::<Vec<_>>()
					.join("\n");
				format!(
					"Spawned {} background agents.{} Each result auto-delivers on yield unless a settled `hub jobs`/`wait` snapshot consumes it first.\n{listing}\n{coordination_hint}",
					started.len(),
					schedule_failure_summary
				)
			};
			let details = TaskToolDetails {
				project_agents_dir: None,
				results: Vec::new(),
				total_duration_ms: 0,
				async_state: Some(AsyncTaskState {
					state: "running".to_string(),
					job_id: started[0].clone(),
					kind: "task".to_string(),
				}),
				message: Some(content),
			};
			let serialized = serde_json::to_string(&details).map_err(|e| {
				ToolError::ExecutionFailed(format!("Failed to serialize TaskToolDetails: {e}"))
			})?;
			return Ok(ToolResult {
				tool_call_id: String::new(),
				content: serialized,
				is_error: false,
				terminate: false,
				details: None,
			});
		}

		let start_total = Instant::now();
		let futures = items
			.into_iter()
			.enumerate()
			.map(|(index, item)| self.run_single_task(index, item, ctx));

		let results = join_all(futures).await;
		let total_duration_ms = start_total.elapsed().as_millis() as u64;

		let details = TaskToolDetails {
			project_agents_dir: None,
			results,
			total_duration_ms,
			message: None,
			async_state: None,
		};

		let content = serde_json::to_string(&details).map_err(|e| {
			ToolError::ExecutionFailed(format!("Failed to serialize TaskToolDetails: {e}"))
		})?;

		Ok(ToolResult {
			tool_call_id: String::new(),
			content,
			is_error: false,
			terminate: false,
			details: None,
		})
	}
}
