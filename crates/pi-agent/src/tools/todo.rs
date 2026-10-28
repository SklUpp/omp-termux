//! Headless `todo` tool.
//!
//! Mirrors `packages/coding-agent/src/tools/todo.ts`: a single operation per
//! call, phased state, exact task-content targeting, and wholesale discard of a
//! failed batch. State lives on the tool instance for the duration of a run and
//! is restored by the CLI from the session's newest `user_todo_edit` custom
//! entry or successful `todo` tool result (`details.phases`), whichever appears
//! later in the branch — including `user_todo_edit` clears.

use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::state::{AgentContext, ToolResult};

use super::{Tool, ToolError, tool_result};

/// Phase name used by a flat `items` init when no `phase` is given.
const DEFAULT_INIT_PHASE: &str = "Tasks";

/// Custom session entry written by user todo edits (TS
/// `USER_TODO_EDIT_CUSTOM_TYPE`): `{customType, data: {phases}}`.
pub const USER_TODO_EDIT_CUSTOM_TYPE: &str = "user_todo_edit";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
	Pending,
	InProgress,
	Completed,
	Abandoned,
	Blocked,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TodoItem {
	pub content: String,
	pub status: TodoStatus,
	/// Blocker note while `status == Blocked`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub blocker: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TodoPhase {
	pub name: String,
	pub tasks: Vec<TodoItem>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TodoOp {
	Init,
	Start,
	Done,
	Rm,
	Drop,
	Block,
	Unblock,
	Append,
	View,
}

impl TodoOp {
	fn as_str(self) -> &'static str {
		match self {
			Self::Init => "init",
			Self::Start => "start",
			Self::Done => "done",
			Self::Rm => "rm",
			Self::Drop => "drop",
			Self::Block => "block",
			Self::Unblock => "unblock",
			Self::Append => "append",
			Self::View => "view",
		}
	}
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TodoCompletionTransition {
	pub phase: String,
	pub content: String,
}

#[derive(Debug, Clone, Deserialize)]
struct InitListEntry {
	phase: String,
	items: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct TodoParams {
	op: Option<TodoOp>,
	list: Option<Vec<InitListEntry>>,
	task: Option<String>,
	phase: Option<String>,
	items: Option<Vec<String>>,
	reason: Option<String>,
}

fn non_empty(value: &Option<String>) -> Option<&str> {
	value.as_deref().filter(|value| !value.is_empty())
}

fn find_task(phases: &[TodoPhase], content: &str) -> Option<(usize, usize)> {
	for (phase_index, phase) in phases.iter().enumerate() {
		if let Some(task_index) = phase.tasks.iter().position(|task| task.content == content) {
			return Some((phase_index, task_index));
		}
	}
	None
}

fn find_phase(phases: &[TodoPhase], name: &str) -> Option<usize> {
	phases.iter().position(|phase| phase.name == name)
}

fn resolve_task(
	phases: &[TodoPhase],
	content: Option<&str>,
	errors: &mut Vec<String>,
) -> Option<(usize, usize)> {
	let Some(content) = content else {
		errors.push("Missing task content".to_string());
		return None;
	};
	match find_task(phases, content) {
		Some(hit) => Some(hit),
		None => {
			if is_task_id(content) {
				errors.push(format!(
					"Task \"{content}\" not found. Tasks are referenced by content, not by IDs — pass the task's full text from the previous result."
				));
			} else {
				let total_tasks: usize = phases.iter().map(|phase| phase.tasks.len()).sum();
				let hint = if total_tasks == 0 {
					" (todo list is empty — was it replaced or not yet created?)"
				} else {
					""
				};
				errors.push(format!("Task \"{content}\" not found{hint}"));
			}
			None
		},
	}
}

fn is_task_id(content: &str) -> bool {
	let Some(rest) = content.strip_prefix("task-") else {
		return false;
	};
	!rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit())
}

fn resolve_phase(
	phases: &[TodoPhase],
	name: Option<&str>,
	errors: &mut Vec<String>,
) -> Option<usize> {
	let Some(name) = name else {
		errors.push("Missing phase name".to_string());
		return None;
	};
	let hit = find_phase(phases, name);
	if hit.is_none() {
		errors.push(format!("Phase \"{name}\" not found"));
	}
	hit
}

/// Resolved task targets for `done`/`drop`/`block`/`unblock`: one task, every
/// task in one phase, or every task in the list when neither target is given.
fn task_targets(
	phases: &[TodoPhase],
	params: &TodoParams,
	errors: &mut Vec<String>,
) -> Vec<(usize, usize)> {
	if let Some(task) = non_empty(&params.task) {
		return resolve_task(phases, Some(task), errors).into_iter().collect();
	}
	if let Some(phase) = non_empty(&params.phase) {
		return match resolve_phase(phases, Some(phase), errors) {
			Some(index) => (0..phases[index].tasks.len()).map(|task| (index, task)).collect(),
			None => Vec::new(),
		};
	}
	phases
		.iter()
		.enumerate()
		.flat_map(|(phase, value)| (0..value.tasks.len()).map(move |task| (phase, task)))
		.collect()
}

fn init_phases(params: &TodoParams, errors: &mut Vec<String>) -> Vec<TodoPhase> {
	let list = match &params.list {
		Some(list) => list.clone(),
		None => match &params.items {
			Some(items) if !items.is_empty() => vec![InitListEntry {
				phase: params.phase.clone().unwrap_or_else(|| DEFAULT_INIT_PHASE.to_string()),
				items: items.clone(),
			}],
			_ => {
				errors.push("Missing list for init operation".to_string());
				return Vec::new();
			},
		},
	};

	// Duplicate phase names or task contents would be permanently unaddressable
	// (every targeting op resolves the first match), so reject them up front.
	let mut seen_phases = HashSet::new();
	let mut seen_tasks = HashSet::new();
	for entry in &list {
		if !seen_phases.insert(entry.phase.clone()) {
			errors.push(format!("Duplicate phase \"{}\" in init list", entry.phase));
		}
		for content in &entry.items {
			if !seen_tasks.insert(content.clone()) {
				errors.push(format!("Duplicate task \"{content}\" in init list"));
			}
		}
	}

	list
		.into_iter()
		.map(|entry| TodoPhase {
			name: entry.phase,
			tasks: entry
				.items
				.into_iter()
				.map(|content| TodoItem { content, status: TodoStatus::Pending, blocker: None })
				.collect(),
		})
		.collect()
}

fn append_items(phases: &mut Vec<TodoPhase>, params: &TodoParams, errors: &mut Vec<String>) {
	let Some(phase_name) = non_empty(&params.phase) else {
		errors.push("Missing phase name for append operation".to_string());
		return;
	};
	let Some(items) = params.items.as_ref().filter(|items| !items.is_empty()) else {
		errors.push("Missing items for append operation".to_string());
		return;
	};

	// Validate the whole batch before mutating so a failing op reports every
	// duplicate and leaves nothing half-applied.
	let mut seen = HashSet::new();
	let mut has_duplicate = false;
	for content in items {
		if seen.contains(content) || find_task(phases, content).is_some() {
			errors.push(format!("Task \"{content}\" already exists"));
			has_duplicate = true;
		}
		seen.insert(content.clone());
	}
	if has_duplicate {
		return;
	}

	let phase_index = match find_phase(phases, phase_name) {
		Some(index) => index,
		None => {
			phases.push(TodoPhase { name: phase_name.to_string(), tasks: Vec::new() });
			phases.len() - 1
		},
	};
	for content in items {
		phases[phase_index].tasks.push(TodoItem {
			content: content.clone(),
			status: TodoStatus::Pending,
			blocker: None,
		});
	}
}

fn remove_tasks(phases: &mut Vec<TodoPhase>, params: &TodoParams, errors: &mut Vec<String>) {
	if let Some(task) = non_empty(&params.task) {
		match resolve_task(phases, Some(task), errors) {
			Some((phase, index)) => {
				phases[phase].tasks.remove(index);
			},
			None => {},
		}
		return;
	}
	if let Some(phase_name) = non_empty(&params.phase) {
		if let Some(index) = resolve_phase(phases, Some(phase_name), errors) {
			phases[index].tasks.clear();
		}
		return;
	}
	for phase in phases {
		phase.tasks.clear();
	}
}

fn normalize_blocker(reason: &Option<String>) -> Option<String> {
	let reason = reason.as_deref()?;
	let collapsed = reason.split_whitespace().collect::<Vec<_>>().join(" ");
	if collapsed.is_empty() { None } else { Some(collapsed) }
}

fn apply_entry(phases: &mut Vec<TodoPhase>, params: &TodoParams, errors: &mut Vec<String>) {
	match params.op {
		Some(TodoOp::Init) => {
			*phases = init_phases(params, errors);
		},
		Some(TodoOp::Start) => {
			let Some((phase, task)) = resolve_task(phases, non_empty(&params.task), errors) else {
				return;
			};
			for (phase_index, value) in phases.iter_mut().enumerate() {
				for (task_index, item) in value.tasks.iter_mut().enumerate() {
					if item.status == TodoStatus::InProgress && (phase_index != phase || task_index != task) {
						item.status = TodoStatus::Pending;
					}
				}
			}
			phases[phase].tasks[task].status = TodoStatus::InProgress;
			phases[phase].tasks[task].blocker = None;
		},
		Some(TodoOp::Done) => {
			for (phase, task) in task_targets(phases, params, errors) {
				phases[phase].tasks[task].status = TodoStatus::Completed;
				phases[phase].tasks[task].blocker = None;
			}
		},
		Some(TodoOp::Drop) => {
			for (phase, task) in task_targets(phases, params, errors) {
				phases[phase].tasks[task].status = TodoStatus::Abandoned;
				phases[phase].tasks[task].blocker = None;
			}
		},
		Some(TodoOp::Block) => {
			if non_empty(&params.task).is_none() && non_empty(&params.phase).is_none() {
				errors.push("block requires a task or phase target".to_string());
				return;
			}
			let reason = normalize_blocker(&params.reason);
			for (phase, task) in task_targets(phases, params, errors) {
				let item = &mut phases[phase].tasks[task];
				// Only actionable open work can be blocked: blocking a phase must
				// not reopen completed/abandoned tasks or erase finished progress.
				if !matches!(item.status, TodoStatus::Pending | TodoStatus::InProgress | TodoStatus::Blocked) {
					continue;
				}
				item.status = TodoStatus::Blocked;
				item.blocker = reason.clone();
			}
		},
		Some(TodoOp::Unblock) => {
			if non_empty(&params.task).is_none() && non_empty(&params.phase).is_none() {
				errors.push("unblock requires a task or phase target".to_string());
				return;
			}
			for (phase, task) in task_targets(phases, params, errors) {
				let item = &mut phases[phase].tasks[task];
				if item.status == TodoStatus::Blocked {
					item.status = TodoStatus::Pending;
					item.blocker = None;
				}
			}
		},
		Some(TodoOp::Rm) => remove_tasks(phases, params, errors),
		Some(TodoOp::Append) => append_items(phases, params, errors),
		Some(TodoOp::View) | None => {},
	}
}

fn normalize_in_progress_task(phases: &mut [TodoPhase]) {
	let ordered: Vec<(usize, usize)> = phases
		.iter()
		.enumerate()
		.flat_map(|(phase, value)| (0..value.tasks.len()).map(move |task| (phase, task)))
		.collect();
	if ordered.is_empty() {
		return;
	}

	let in_progress: Vec<(usize, usize)> = ordered
		.iter()
		.copied()
		.filter(|(phase, task)| phases[*phase].tasks[*task].status == TodoStatus::InProgress)
		.collect();
	if in_progress.len() > 1 {
		for (phase, task) in in_progress.iter().skip(1) {
			phases[*phase].tasks[*task].status = TodoStatus::Pending;
		}
	}
	if !in_progress.is_empty() {
		return;
	}

	if let Some((phase, task)) = ordered
		.into_iter()
		.find(|(phase, task)| phases[*phase].tasks[*task].status == TodoStatus::Pending)
	{
		phases[phase].tasks[task].status = TodoStatus::InProgress;
	}
}

fn completion_transitions(
	previous: &[TodoPhase],
	updated: &[TodoPhase],
) -> Vec<TodoCompletionTransition> {
	let previous_statuses: HashMap<(&str, &str), TodoStatus> = previous
		.iter()
		.flat_map(|phase| {
			phase
				.tasks
				.iter()
				.map(move |task| ((phase.name.as_str(), task.content.as_str()), task.status))
		})
		.collect();

	let mut transitions = Vec::new();
	for phase in updated {
		for task in &phase.tasks {
			if task.status != TodoStatus::Completed {
				continue;
			}
			match previous_statuses.get(&(phase.name.as_str(), task.content.as_str())) {
				Some(status) if *status != TodoStatus::Completed => transitions.push(
					TodoCompletionTransition {
						phase: phase.name.clone(),
						content: task.content.clone(),
					},
				),
				_ => {},
			}
		}
	}
	transitions
}

fn format_summary(phases: &[TodoPhase], errors: &[String], read_only: bool) -> String {
	let task_count: usize = phases.iter().map(|phase| phase.tasks.len()).sum();
	if task_count == 0 {
		if !errors.is_empty() {
			return format!("Errors: {}", errors.join("; "));
		}
		return if read_only {
			"Todo list is empty.".to_string()
		} else {
			"Todo list cleared.".to_string()
		};
	}

	let remaining: Vec<(&str, &TodoItem)> = phases
		.iter()
		.flat_map(|phase| {
			phase
				.tasks
				.iter()
				.filter(|task| {
					matches!(task.status, TodoStatus::Pending | TodoStatus::InProgress)
				})
				.map(move |task| (phase.name.as_str(), task))
		})
		.collect();

	let mut lines = Vec::new();
	if !errors.is_empty() {
		lines.push(format!("Errors: {}", errors.join("; ")));
	}
	if remaining.is_empty() {
		lines.push("Remaining items: none.".to_string());
	} else {
		lines.push(format!("Remaining items ({}):", remaining.len()));
		for (phase, task) in &remaining {
			lines.push(format!("  - {} [{}] ({})", task.content, status_label(task.status), phase));
		}
	}

	// Closed = completed + abandoned, mirroring the per-phase `done` count.
	let closed_all = phases
		.iter()
		.flat_map(|phase| &phase.tasks)
		.filter(|task| matches!(task.status, TodoStatus::Completed | TodoStatus::Abandoned))
		.count();
	let blocked_all = phases
		.iter()
		.flat_map(|phase| &phase.tasks)
		.filter(|task| task.status == TodoStatus::Blocked)
		.count();

	let current_index = phases
		.iter()
		.position(|phase| {
			phase
				.tasks
				.iter()
				.any(|task| matches!(task.status, TodoStatus::Pending | TodoStatus::InProgress))
		})
		.unwrap_or_else(|| phases.len() - 1);
	let current = &phases[current_index];
	let current_done = current
		.tasks
		.iter()
		.filter(|task| matches!(task.status, TodoStatus::Completed | TodoStatus::Abandoned))
		.count();
	let worked_ahead = phases.iter().enumerate().any(|(index, phase)| {
		index > current_index
			&& phase
				.tasks
				.iter()
				.any(|task| matches!(task.status, TodoStatus::Completed | TodoStatus::Abandoned))
	});

	let blocked_suffix = if blocked_all > 0 {
		format!(", {blocked_all} blocked")
	} else {
		String::new()
	};
	lines.push(format!(
		"Overall: {closed_all}/{task_count} done, {} open{blocked_suffix}.",
		remaining.len()
	));
	lines.push(format!(
		"Active phase {}/{} \"{}\" ({}/{}){}",
		current_index + 1,
		phases.len(),
		current.name,
		current_done,
		current.tasks.len(),
		if worked_ahead {
			" — earliest phase with open tasks; the in-progress pointer auto-advances to the earliest open task on each completion, so it can sit behind out-of-order work (nothing was un-completed)."
		} else {
			"."
		}
	));
	for phase in phases {
		lines.push(format!("  {}:", phase.name));
		for task in &phase.tasks {
			let checkbox = if task.status == TodoStatus::Completed { "[X]" } else { "[ ]" };
			let tag = match task.status {
				TodoStatus::InProgress => " (in progress)".to_string(),
				TodoStatus::Abandoned => " (dropped)".to_string(),
				TodoStatus::Blocked => match &task.blocker {
					Some(blocker) => format!(" (blocked: {blocker})"),
					None => " (blocked)".to_string(),
				},
				TodoStatus::Pending | TodoStatus::Completed => String::new(),
			};
			lines.push(format!("    - {checkbox} {}{tag}", task.content));
		}
	}
	lines.join("\n")
}

fn status_label(status: TodoStatus) -> &'static str {
	match status {
		TodoStatus::Pending => "pending",
		TodoStatus::InProgress => "in_progress",
		TodoStatus::Completed => "completed",
		TodoStatus::Abandoned => "abandoned",
		TodoStatus::Blocked => "blocked",
	}
}

/// Infer a missing `op` from an unambiguous payload shape.
fn infer_op(raw: &Value, has_existing_phases: bool) -> Option<TodoOp> {
	let list_non_empty = raw
		.get("list")
		.and_then(Value::as_array)
		.is_some_and(|list| !list.is_empty());
	if list_non_empty {
		return Some(TodoOp::Init);
	}
	let items_non_empty = raw
		.get("items")
		.and_then(Value::as_array)
		.is_some_and(|items| !items.is_empty());
	if items_non_empty {
		let phase_non_empty = raw
			.get("phase")
			.and_then(Value::as_str)
			.is_some_and(|phase| !phase.is_empty());
		if phase_non_empty {
			return Some(TodoOp::Append);
		}
		if !has_existing_phases {
			return Some(TodoOp::Init);
		}
	}
	None
}

fn validate_list(params: &TodoParams) -> Result<(), String> {
	if let Some(list) = &params.list
		&& list.iter().any(|entry| entry.items.is_empty())
	{
		return Err("Invalid todo arguments: each phase in `list` needs at least one task".to_string());
	}
	Ok(())
}

fn resolve_params(raw: &Value, has_existing_phases: bool) -> Result<TodoParams, String> {
	match serde_json::from_value::<TodoParams>(raw.clone()) {
		Ok(mut params) => {
			if params.op.is_none() {
				// `op: null` is an explicit bad value, not a missing field; only
				// repair when the key is absent (TS `raw.op === undefined`).
				if raw.get("op").is_some() {
					return Err("Invalid todo arguments: \"op\" must be a valid operation".to_string());
				}
				match infer_op(raw, has_existing_phases) {
					Some(op) => params.op = Some(op),
					None => {
						return Err("Invalid todo arguments: missing \"op\" parameter".to_string());
					},
				}
			}
			validate_list(&params)?;
			Ok(params)
		},
		Err(err) => {
			if raw.is_object() && raw.get("op").is_none() {
				if let Some(op) = infer_op(raw, has_existing_phases) {
					let mut repaired = raw.clone();
					repaired["op"] = json!(op);
					if let Ok(params) = serde_json::from_value::<TodoParams>(repaired) {
						validate_list(&params)?;
						return Ok(params);
					}
				}
			}
			Err(format!("Invalid todo arguments: {err}"))
		},
	}
}

fn apply_params(
	previous: &[TodoPhase],
	params: &TodoParams,
) -> (Vec<TodoPhase>, Vec<String>) {
	let mut phases = previous.to_vec();
	let mut errors = Vec::new();
	apply_entry(&mut phases, params, &mut errors);
	normalize_in_progress_task(&mut phases);
	(phases, errors)
}

/// Headless phased todo tool.
pub struct TodoTool {
	phases: RwLock<Vec<TodoPhase>>,
	storage: &'static str,
}

impl Default for TodoTool {
	fn default() -> Self {
		Self::new()
	}
}

impl TodoTool {
	pub fn new() -> Self {
		Self::restored(Vec::new(), false)
	}

	/// Restore phases from a resumed/forked session. `session_persisted` decides
	/// the `details.storage` label (`session` vs `memory`).
	pub fn restored(phases: Vec<TodoPhase>, session_persisted: bool) -> Self {
		Self {
			phases: RwLock::new(phases),
			storage: if session_persisted { "session" } else { "memory" },
		}
	}
}

#[async_trait]
impl Tool for TodoTool {
	fn name(&self) -> &str {
		"todo"
	}

	fn description(&self) -> &str {
		"Track multi-step work with a phased todo list. One operation per call: \
		 init|start|done|rm|drop|block|unblock|append|view. Call init first with \
		 list=[{phase,items}]; deep tasks are split into phases, not one flat list. \
		 Keep exactly one task in_progress and mark tasks done as you finish them; \
		 block only when external input is required. Tasks are addressed by exact \
		 content, never by IDs, so keep task content and phase names stable."
	}

	fn schema(&self) -> Value {
		json!({
			"type": "object",
			"properties": {
				"op": {
					"type": "string",
					"enum": ["init", "start", "done", "rm", "drop", "block", "unblock", "append", "view"],
					"description": "operation to apply"
				},
				"list": {
					"type": "array",
					"description": "phased task list (init)",
					"items": {
						"type": "object",
						"properties": {
							"phase": { "type": "string", "description": "phase name" },
							"items": {
								"type": "array",
								"items": { "type": "string", "description": "task content" },
								"minItems": 1,
								"description": "tasks for this phase"
							}
						},
						"required": ["phase", "items"]
					}
				},
				"task": { "type": "string", "description": "task content" },
				"phase": { "type": "string", "description": "phase name" },
				"items": {
					"type": "array",
					"items": { "type": "string", "description": "task content" },
					"description": "tasks for single-phase init or append"
				},
				"reason": { "type": "string", "description": "blocker note (block op)" }
			},
			"required": ["op"],
			"description": "apply a single todo operation"
		})
	}

	fn is_exclusive(&self) -> bool {
		true
	}

	async fn execute(&self, args: &Value, ctx: &AgentContext) -> Result<ToolResult, ToolError> {
		if ctx.cancellation_token.is_cancelled() {
			return Ok(tool_result("Todo update cancelled", true));
		}

		let mut guard = self
			.phases
			.write()
			.map_err(|_| ToolError::ExecutionFailed("todo lock poisoned".to_string()))?;
		let previous = guard.clone();

		let params = match resolve_params(args, !previous.is_empty()) {
			Ok(params) => params,
			Err(message) => {
				return Ok(ToolResult {
					tool_call_id: String::new(),
					content: message,
					is_error: true,
					terminate: false,
					details: Some(json!({
						"phases": &previous,
						"storage": self.storage,
					})),
				});
			},
		};
		let op = params.op.unwrap_or(TodoOp::View);
		// Pure-view calls are reads: no normalization, no state write.
		let read_only = op == TodoOp::View;
		let (updated, errors) = if read_only {
			(Vec::new(), Vec::new())
		} else {
			apply_params(&previous, &params)
		};

		// A batch with any error is discarded wholesale: persisting a half-applied
		// batch makes the natural retry hit "already exists" for the ops that did
		// land. State and rendered summary stay at previous.
		let failed = !errors.is_empty();
		let changed = !read_only && !failed;
		let completed = if changed {
			completion_transitions(&previous, &updated)
		} else {
			Vec::new()
		};
		let effective = if changed { &updated } else { &previous };

		let mut details = json!({
			"op": op.as_str(),
			"phases": effective,
			"storage": self.storage,
		});
		if !completed.is_empty() {
			details["completedTasks"] = serde_json::to_value(&completed)
				.map_err(|err| ToolError::ExecutionFailed(format!("cannot serialize todo details: {err}")))?;
		}
		let content = format_summary(effective, &errors, read_only);
		if changed {
			*guard = updated;
		}

		Ok(ToolResult {
			tool_call_id: String::new(),
			content,
			is_error: failed,
			terminate: false,
			details: Some(details),
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::state::AgentContext;

	async fn run(tool: &TodoTool, args: Value) -> ToolResult {
		tool.execute(&args, &AgentContext::new(std::env::temp_dir()))
			.await
			.expect("todo executes")
	}

	fn phases(result: &ToolResult) -> Vec<TodoPhase> {
		let details = result.details.as_ref().expect("details present");
		serde_json::from_value(details["phases"].clone()).expect("phases shape")
	}

	fn task<'a>(phases: &'a [TodoPhase], content: &str) -> &'a TodoItem {
		phases
			.iter()
			.flat_map(|phase| &phase.tasks)
			.find(|item| item.content == content)
			.expect("task exists")
	}

	#[tokio::test]
	async fn init_normalizes_and_returns_structured_phases() {
		let tool = TodoTool::new();
		let result = run(
			&tool,
			json!({
				"op": "init",
				"list": [
					{"phase": "Foundation", "items": ["Scaffold", "Wire"]},
					{"phase": "Verification", "items": ["Run tests"]}
				]
			}),
		)
		.await;

		assert!(!result.is_error);
		let phases = phases(&result);
		assert_eq!(phases.len(), 2);
		assert_eq!(phases[0].tasks[0].status, TodoStatus::InProgress);
		assert_eq!(phases[0].tasks[1].status, TodoStatus::Pending);
		assert_eq!(phases[1].tasks[0].status, TodoStatus::Pending);
		assert!(result.content.contains("Active phase 1/2 \"Foundation\" (0/2)"));
		let details = result.details.as_ref().expect("details");
		assert_eq!(details["op"], "init");
		assert_eq!(details["storage"], "memory");
	}

	#[tokio::test]
	async fn start_done_and_phase_targets_progress_atomically() {
		let tool = TodoTool::new();
		run(
			&tool,
			json!({"op": "init", "list": [
				{"phase": "Foundation", "items": ["Scaffold", "Wire"]},
				{"phase": "Verification", "items": ["Run tests"]}
			]}),
		)
		.await;

		let started = run(&tool, json!({"op": "start", "task": "Wire"})).await;
		assert!(!started.is_error);
		let phases_after_start = phases(&started);
		assert_eq!(task(&phases_after_start, "Scaffold").status, TodoStatus::Pending);
		assert_eq!(task(&phases_after_start, "Wire").status, TodoStatus::InProgress);

		let done = run(&tool, json!({"op": "done", "task": "Wire"})).await;
		assert!(!done.is_error);
		let phases_after_done = phases(&done);
		assert_eq!(task(&phases_after_done, "Wire").status, TodoStatus::Completed);
		assert_eq!(task(&phases_after_done, "Scaffold").status, TodoStatus::InProgress);
		assert_eq!(
			done.details.as_ref().expect("details")["completedTasks"],
			json!([{"phase": "Foundation", "content": "Wire"}])
		);

		let done_phase = run(&tool, json!({"op": "done", "phase": "Foundation"})).await;
		assert!(!done_phase.is_error);
		let finished = phases(&done_phase);
		assert!(finished[0].tasks.iter().all(|item| item.status == TodoStatus::Completed));
		assert_eq!(finished[1].tasks[0].status, TodoStatus::InProgress);
		assert!(done_phase.content.contains("Overall: 2/3 done, 1 open."));
	}

	#[tokio::test]
	async fn flat_init_and_unambiguous_missing_op_are_repaired() {
		let tool = TodoTool::new();
		let init = run(&tool, json!({"items": ["A", "B"]})).await;
		assert!(!init.is_error);
		let state = phases(&init);
		assert_eq!(state[0].name, "Tasks");
		assert_eq!(init.details.as_ref().expect("details")["op"], "init");

		let append = run(&tool, json!({"phase": "Tasks", "items": ["C"]})).await;
		assert!(!append.is_error);
		assert_eq!(phases(&append)[0].tasks.len(), 3);

		let ambiguous = run(&tool, json!({"task": "A"})).await;
		assert!(ambiguous.is_error);
		assert!(ambiguous.content.starts_with("Invalid todo arguments:"));
	}

	#[tokio::test]
	async fn failed_operation_discards_the_whole_batch() {
		let tool = TodoTool::new();
		run(&tool, json!({"op": "init", "items": ["A", "B"]})).await;

		let failed = run(&tool, json!({"op": "done", "task": "missing"})).await;
		assert!(failed.is_error);
		assert!(failed.content.contains("Task \"missing\" not found"));
		assert!(failed.content.contains("Errors: "));
		let unchanged = phases(&failed);
		assert_eq!(unchanged[0].tasks.len(), 2);
		assert_eq!(task(&unchanged, "A").status, TodoStatus::InProgress);
		assert_eq!(task(&unchanged, "B").status, TodoStatus::Pending);

		let id_hint = run(&tool, json!({"op": "start", "task": "task-1"})).await;
		assert!(id_hint.is_error);
		assert!(id_hint.content.contains("referenced by content, not by IDs"));

		let empty_hint = run(&TodoTool::new(), json!({"op": "done", "task": "anything"})).await;
		assert!(empty_hint.is_error);
		assert!(empty_hint.content.contains("todo list is empty"));
	}

	#[tokio::test]
	async fn append_rejects_duplicates_before_mutating() {
		let tool = TodoTool::new();
		run(&tool, json!({"op": "init", "list": [{"phase": "Tasks", "items": ["A"]}]})).await;

		let duplicate = run(&tool, json!({"op": "append", "phase": "Tasks", "items": ["B", "B"]})).await;
		assert!(duplicate.is_error);
		assert!(duplicate.content.contains("Task \"B\" already exists"));
		assert_eq!(phases(&duplicate)[0].tasks.len(), 1);

		let fresh = run(&tool, json!({"op": "append", "phase": "Later", "items": ["C"]})).await;
		assert!(!fresh.is_error);
		let state = phases(&fresh);
		assert_eq!(state.len(), 2);
		assert_eq!(state[1].name, "Later");
	}

	#[tokio::test]
	async fn block_never_promotes_and_unblock_returns_to_pending() {
		let tool = TodoTool::new();
		run(&tool, json!({"op": "init", "items": ["A", "B"]})).await;

		let blocked = run(&tool, json!({"op": "block", "task": "A", "reason": "  needs\n input  "})).await;
		assert!(!blocked.is_error);
		let state = phases(&blocked);
		assert_eq!(task(&state, "A").status, TodoStatus::Blocked);
		assert_eq!(task(&state, "A").blocker.as_deref(), Some("needs input"));
		assert_eq!(task(&state, "B").status, TodoStatus::InProgress);
		assert!(blocked.content.contains("1 blocked"));

		let missing_target = run(&tool, json!({"op": "block"})).await;
		assert!(missing_target.is_error);
		assert!(missing_target.content.contains("block requires a task or phase target"));

		let unblocked = run(&tool, json!({"op": "unblock", "task": "A"})).await;
		assert!(!unblocked.is_error);
		assert_eq!(task(&phases(&unblocked), "A").status, TodoStatus::Pending);

		run(&tool, json!({"op": "done", "task": "A"})).await;
		let closed = run(&tool, json!({"op": "block", "phase": "Tasks"})).await;
		assert!(!closed.is_error);
		assert_eq!(task(&phases(&closed), "A").status, TodoStatus::Completed);
	}

	#[tokio::test]
	async fn start_done_and_drop_clear_blocker_note() {
		let tool = TodoTool::new();
		run(&tool, json!({"op": "init", "items": ["A", "B", "C"]})).await;

		run(&tool, json!({"op": "block", "task": "A", "reason": "waiting"})).await;
		let started = run(&tool, json!({"op": "start", "task": "A"})).await;
		assert_eq!(task(&phases(&started), "A").status, TodoStatus::InProgress);
		assert!(task(&phases(&started), "A").blocker.is_none());

		run(&tool, json!({"op": "block", "task": "B", "reason": "waiting"})).await;
		let done = run(&tool, json!({"op": "done", "task": "B"})).await;
		assert_eq!(task(&phases(&done), "B").status, TodoStatus::Completed);
		assert!(task(&phases(&done), "B").blocker.is_none());

		run(&tool, json!({"op": "block", "task": "C", "reason": "waiting"})).await;
		let dropped = run(&tool, json!({"op": "drop", "task": "C"})).await;
		assert_eq!(task(&phases(&dropped), "C").status, TodoStatus::Abandoned);
		assert!(task(&phases(&dropped), "C").blocker.is_none());
	}

	#[tokio::test]
	async fn blocked_sole_task_has_no_in_progress_pointer() {
		let tool = TodoTool::new();
		run(&tool, json!({"op": "init", "items": ["Only"]})).await;
		let blocked = run(&tool, json!({"op": "block", "task": "Only"})).await;
		assert!(!blocked.is_error);
		let phases = phases(&blocked);
		assert_eq!(task(&phases, "Only").status, TodoStatus::Blocked);
		assert!(phases.iter().flat_map(|phase| &phase.tasks).all(|item| item.status != TodoStatus::InProgress));
	}

	#[tokio::test]
	async fn rm_targets_task_phase_or_entire_list() {
		let tool = TodoTool::new();
		run(
			&tool,
			json!({"op": "init", "list": [
				{"phase": "One", "items": ["A", "B"]},
				{"phase": "Two", "items": ["C"]}
			]}),
		)
		.await;

		let one = run(&tool, json!({"op": "rm", "task": "B"})).await;
		assert!(!one.is_error);
		let phases_one = phases(&one);
		assert_eq!(phases_one[0].tasks.len(), 1);
		assert_eq!(phases_one[1].tasks.len(), 1);

		let phase = run(&tool, json!({"op": "rm", "phase": "One"})).await;
		assert!(!phase.is_error);
		let phases_phase = phases(&phase);
		assert!(phases_phase[0].tasks.is_empty());
		assert_eq!(phases_phase[1].tasks.len(), 1);

		let all = run(&tool, json!({"op": "rm"})).await;
		assert!(!all.is_error);
		assert!(phases(&all).iter().all(|phase| phase.tasks.is_empty()));
	}

	#[tokio::test]
	async fn view_is_read_only_and_skips_normalization() {
		let state = vec![TodoPhase {
			name: "Saved".to_string(),
			tasks: vec![TodoItem {
				content: "resume me".to_string(),
				status: TodoStatus::Pending,
				blocker: None,
			}],
		}];
		let tool = TodoTool::restored(state, true);
		let viewed = run(&tool, json!({"op": "view"})).await;
		assert!(!viewed.is_error);
		assert_eq!(task(&phases(&viewed), "resume me").status, TodoStatus::Pending);
		assert_eq!(viewed.details.as_ref().expect("details")["storage"], "session");

		let empty = run(&TodoTool::new(), json!({"op": "view"})).await;
		assert_eq!(empty.content, "Todo list is empty.");
	}

	#[tokio::test]
	async fn schema_and_op_specific_validation_errors_are_reported() {
		let tool = TodoTool::new();
		let missing_list = run(&tool, json!({"op": "init"})).await;
		assert!(missing_list.is_error);
		assert_eq!(missing_list.content, "Errors: Missing list for init operation");

		let empty_entry = run(
			&tool,
			json!({"op": "init", "list": [{"phase": "Empty", "items": []}]}),
		)
		.await;
		assert!(empty_entry.is_error);
		assert!(empty_entry.content.starts_with("Invalid todo arguments:"));

		let duplicate = run(
			&tool,
			json!({"op": "init", "list": [{"phase": "P", "items": ["A", "A"]}]}),
		)
		.await;
		assert!(duplicate.is_error);
		assert!(duplicate.content.contains("Duplicate task \"A\" in init list"));

		let missing_append = run(&tool, json!({"op": "append", "phase": "P"})).await;
		assert!(missing_append.is_error);
		assert!(missing_append.content.contains("Missing items for append operation"));

		let unknown_phase = run(&tool, json!({"op": "done", "phase": "Nope"})).await;
		assert!(unknown_phase.is_error);
		assert!(unknown_phase.content.contains("Phase \"Nope\" not found"));

		let cancelled = {
			let ctx = AgentContext::new(std::env::temp_dir());
			ctx.cancellation_token.cancel();
			tool.execute(&json!({"op": "view"}), &ctx).await.expect("cancelled executes")
		};
		assert!(cancelled.is_error);
		assert_eq!(cancelled.content, "Todo update cancelled");
	}
}
