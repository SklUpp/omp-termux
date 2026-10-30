//! Checkpoint/rewind host: binds the tool state machine to the session tree.
//!
//! Upstream anchors: `packages/coding-agent/src/session/agent-session.ts`
//! (`#checkpointState`, `#pendingRewindReport`, `#applyRewind`,
//! `#rehydrateCheckpointRewindState`) and
//! `packages/coding-agent/src/session/session-manager.ts`
//! (`branchWithSummary`, `appendCustomMessageEntry`).
//!
//! Lifecycle:
//! 1. A successful `checkpoint` tool result is persisted → [`CheckpointHost::note_persisted_tool_results`]
//!    records the goal, the persisted result entry id, the timestamp, and the
//!    message count at that point.
//! 2. The `rewind` tool only validates and returns a report; its persisted
//!    result records the report as pending.
//! 3. At turn end the caller invokes [`CheckpointHost::apply_pending_rewind`],
//!    which appends a `branch_summary` at the checkpoint entry (or root when it
//!    no longer resolves), appends the hidden retained `rewind-report`
//!    `custom_message`, moves the leaf, and returns the rebuilt branch history.
//!    Abandoned JSONL entries stay on disk untouched, so nothing is lost and
//!    usage/cost data on retained messages is preserved verbatim.
//! 4. On resume/tree navigation [`CheckpointHost::rehydrate`] rebuilds either
//!    the open checkpoint or the completed-rewind guard from the active branch.
//!
//! `main.rs` owns the wiring: it registers the pair, captures persistent or
//! no-session results at `Event::TurnEnd`, and answers that event's
//! [`pi_agent::TurnAck`] with the rebuilt branch context.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use pi_agent::InMemoryRewind;
use pi_agent::tools::checkpoint::{
	CHECKPOINT_ACTIVE_NOTICE, CHECKPOINT_ACTIVE_REMINDER_TYPE, CHECKPOINT_TOOL_NAME,
	CheckpointController, CheckpointTool, REWIND_REPORT_CUSTOM_TYPE, REWIND_TOOL_NAME, RewindTool,
	now_iso8601, render_branch_summary_context, render_rewind_report,
};
use pi_agent::{Message, ToolResult};

use crate::session::{MessageEntry, SessionEntry, SessionStore, TurnRecord};

/// Apply a no-session rewind to a conversation list that has no leading system
/// message. `InMemoryRewind::keep` counts the live messages (system included),
/// so the mirror keeps one fewer entry. The summary/report appended here are
/// what the next prompt replays.
pub fn apply_in_memory_rewind(messages: &mut Vec<Message>, rewind: &InMemoryRewind) {
	// `keep` is a live-loop count (compaction/steering can shift it), so prefer
	// the checkpoint batch's tool-call id in this uncompacted mirror; sibling
	// results of that assistant message stay, so no call is left unanswered.
	let keep = rewind
		.checkpoint_tool_call_id
		.as_deref()
		.and_then(|id| pi_agent::tool_batch_end(messages, id))
		.unwrap_or_else(|| {
			pi_agent::repair_tool_batch_boundary(messages, rewind.keep.saturating_sub(1))
		})
		.min(messages.len());
	messages.truncate(keep);
	if !rewind.summary.is_empty() {
		messages.push(Message::user(rewind.summary.as_str()));
	}
	if !rewind.report.is_empty() {
		messages.push(Message::system(rewind.report.as_str()));
	}
}

/// Result of the deferred turn-end branch cut.
#[derive(Debug, Clone, PartialEq)]
pub struct RewindApplied {
	/// Trimmed report retained for the rest of the branch.
	pub report: String,
	pub started_at: String,
	pub rewound_at: String,
	pub branch_summary_id: String,
	pub report_entry_id: String,
	/// The checkpoint entry disappeared from the tree; upstream logs a warning
	/// and branches from root instead of failing.
	pub fell_back_to_root: bool,
	/// Active-branch `TurnRecord` view after the cut (what the next request
	/// should send instead of the pre-rewind history).
	pub history: Vec<TurnRecord>,
}

/// Session-side owner of the checkpoint state machine.
#[derive(Debug, Clone, Default)]
pub struct CheckpointHost {
	controller: Arc<CheckpointController>,
	/// A no-session cut was handed to the loop for the current settle but the
	/// turn has not committed yet (rollback keeps the pending report).
	in_memory_rewind_applied: Arc<Mutex<bool>>,
}

impl CheckpointHost {
	#[must_use]
	pub fn new() -> Self {
		Self::default()
	}

	#[must_use]
	pub fn controller(&self) -> &Arc<CheckpointController> {
		&self.controller
	}

	#[must_use]
	pub fn checkpoint_tool(&self) -> CheckpointTool {
		CheckpointTool::new(Arc::clone(&self.controller))
	}

	#[must_use]
	pub fn rewind_tool(&self) -> RewindTool {
		RewindTool::new(Arc::clone(&self.controller))
	}

	#[must_use]
	pub fn checkpoint_state(&self) -> Option<pi_agent::tools::checkpoint::CheckpointState> {
		self.controller.checkpoint_state()
	}

	#[must_use]
	pub fn last_completed_rewind(&self) -> Option<pi_agent::tools::checkpoint::CompletedRewindState> {
		self.controller.last_completed_rewind()
	}

	/// Pre-yield guard: the model still owes a `rewind`. The caller injects
	/// [`Self::yield_warning`] as a developer message and continues the turn
	/// (upstream `#enforceRewindBeforeYield`).
	#[must_use]
	pub fn needs_rewind_before_yield(&self) -> bool {
		self.controller.needs_rewind_before_yield()
	}

	#[must_use]
	pub fn yield_warning(&self) -> Option<&'static str> {
		self.controller.yield_warning()
	}

	/// Yield guard: when a checkpoint is open and the assistant produced no
	/// tool calls, return the live messages plus the developer warning so the
	/// loop runs another turn instead of ending. `None` leaves the turn alone.
	///
	/// Parity with upstream `#enforceRewindBeforeYield`: exactly one warning per
	/// yielding settle; repeats are bounded by the agent loop's turn budget (the
	/// warning disappears as soon as `rewind` records its report).
	#[must_use]
	pub fn yield_continuation(
		&self,
		assistant: &Message,
		messages: &[Message],
	) -> Option<Vec<Message>> {
		if !assistant.tool_calls.is_empty() || !self.needs_rewind_before_yield() {
			return None;
		}
		let warning = self.yield_warning()?;
		let mut continuation = messages.to_vec();
		continuation.push(Message::system(warning));
		Some(continuation)
	}

	/// Rebuild runtime state from the active persisted branch (resume, session
	/// switch, tree navigation).
	pub fn rehydrate(&self, store: &SessionStore) {
		let entries: Vec<Value> = store
			.branch_entries()
			.into_iter()
			.filter_map(|entry| serde_json::to_value(entry).ok())
			.collect();
		self.controller.rehydrate(&entries);
	}

	#[must_use]
	pub fn pending_rewind(&self) -> Option<pi_agent::tools::checkpoint::PendingRewind> {
		self.controller.pending_rewind()
	}

	/// Capture successful `checkpoint`/`rewind` tool results that were just
	/// persisted. Call after `SessionStore::append_messages`; the count for a
	/// checkpoint is its own position in the log, not the end of the batch.
	/// Replayed batches whose checkpoint already has a retained report are
	/// ignored, so a cancelled/redriven stream never recuts the branch (F2).
	pub fn note_persisted_tool_results(&self, store: &SessionStore, entries: &[MessageEntry]) -> bool {
		let mut captured = false;
		for entry in entries {
			let message = &entry.message;
			if message.role != "toolResult" || message.is_error == Some(true) {
				continue;
			}
			let details = message.extra.get("details");
			match message.tool_name.as_deref().unwrap_or_default() {
				CHECKPOINT_TOOL_NAME => {
					if checkpoint_already_rewound(store, &entry.id) {
						continue;
					}
					let goal = details
						.and_then(|details| details.get("goal"))
						.and_then(Value::as_str)
						.unwrap_or_default();
					let started_at = details
						.and_then(|details| details.get("startedAt"))
						.and_then(Value::as_str)
						.unwrap_or(&entry.timestamp);
					let count = store
						.messages()
						.iter()
						.position(|message| message.id == entry.id)
						.map_or_else(|| store.messages().len(), |index| index + 1);
					let _ = self.controller.note_checkpoint(goal, started_at, count);
					self.controller.backfill_checkpoint_entry_id(entry.id.clone());
					if let Some(tool_call_id) = message.tool_call_id.clone() {
						self
							.controller
							.backfill_checkpoint_tool_call_id(tool_call_id);
					}
					self.abandon_in_memory_rewind();
					captured = true;
				},
				REWIND_TOOL_NAME => {
					if let Some(report) = tool_report(details, &message.content.text()) {
						self.controller.note_rewind_tool_result(&report);
					}
				},
				_ => {},
			}
		}
		captured
	}

	/// Persist the transient `checkpoint-active-reminder` custom message right
	/// after the checkpoint result (upstream `#checkpointActiveReminder`).
	/// Non-displayed, so UIs skip it; branch rebuilds still replay it to the
	/// model. It is abandoned by the later rewind cut.
	pub fn persist_active_reminder(&self, store: &mut SessionStore) -> Result<(), String> {
		let Some(state) = self.checkpoint_state() else {
			return Ok(());
		};
		let Some(_checkpoint_id) = state.checkpoint_entry_id.as_deref() else {
			return Ok(());
		};
		// The reminder is parented on the batch's last message, not necessarily
		// the checkpoint result, so match its details instead of `parentId`.
		let exists = store.branch_entries().iter().any(|entry| {
			entry.kind == "custom_message"
				&& entry.extra.get("customType").and_then(Value::as_str)
					== Some(CHECKPOINT_ACTIVE_REMINDER_TYPE)
				&& entry
					.extra
					.get("details")
					.and_then(|details| details.get("goal"))
					.and_then(Value::as_str)
					== Some(state.goal.as_str())
				&& entry
					.extra
					.get("details")
					.and_then(|details| details.get("startedAt"))
					.and_then(Value::as_str)
					== Some(state.started_at.as_str())
		});
		if exists {
			return Ok(());
		}
		store.append_custom_message(
			CHECKPOINT_ACTIVE_REMINDER_TYPE,
			CHECKPOINT_ACTIVE_NOTICE,
			false,
			json!({ "goal": state.goal, "startedAt": state.started_at }),
		)?;
		Ok(())
	}

	/// Capture results for sessions without persistence (F1). `tool_names` maps
	/// tool-call id to name; `turn_start` is the message count before the
	/// assistant message of this turn, so a checkpoint keeps its exact
	/// in-memory position.
	pub fn note_unpersisted_tool_results(
		&self,
		tool_results: &[ToolResult],
		tool_names: &std::collections::HashMap<String, String>,
		turn_start: usize,
	) -> bool {
		let mut captured = false;
		for (index, result) in tool_results.iter().enumerate() {
			if result.is_error {
				continue;
			}
			let name = tool_names
				.get(&result.tool_call_id)
				.map(String::as_str)
				.unwrap_or_default();
			match name {
				CHECKPOINT_TOOL_NAME => {
					let details = result.details.as_ref();
					let goal = details
						.and_then(|details| details.get("goal"))
						.and_then(Value::as_str)
						.unwrap_or_default();
					let started_at = details
						.and_then(|details| details.get("startedAt"))
						.and_then(Value::as_str)
						.unwrap_or_default();
					if !started_at.is_empty() {
						let count = turn_start + index + 2;
						let _ = self.controller.note_checkpoint(goal, started_at, count);
						self
							.controller
							.backfill_checkpoint_tool_call_id(result.tool_call_id.clone());
						self.abandon_in_memory_rewind();
						captured = true;
					}
				},
				REWIND_TOOL_NAME => {
					if let Some(report) = tool_report(result.details.as_ref(), &result.content) {
						self.controller.note_rewind_tool_result(&report);
					}
				},
				_ => {},
			}
		}
		captured
	}

	/// The transient active-checkpoint notice for no-session continuations.
	#[must_use]
	pub fn active_notice(&self) -> Option<&'static str> {
		self.checkpoint_state().map(|_| CHECKPOINT_ACTIVE_NOTICE)
	}

	/// No-session rewind: return the cut the loop/mirror should apply. The
	/// controller stays pending until [`Self::finish_in_memory_rewind`], so a
	/// failed settle can [`Self::abandon_in_memory_rewind`] without losing it.
	/// `None` when no report is pending or this settle already applied it.
	#[must_use]
	pub fn apply_pending_rewind_in_memory(&self) -> Option<InMemoryRewind> {
		let mut applied = self.lock_applied();
		if *applied {
			return None;
		}
		let pending = self.controller.pending_rewind()?;
		*applied = true;
		Some(InMemoryRewind {
			// The mirror cuts by checkpoint tool-call id, never by a stale
			// live-loop position; zero only applies if that id is absent.
			keep: 0,
			checkpoint_tool_call_id: pending.checkpoint_tool_call_id,
			summary: render_branch_summary_context(&pending.report),
			report: render_rewind_report(&pending.report),
		})
	}

	/// Commit a no-session cut after the turn settled successfully. Returns
	/// whether a cut was pending (the rewind turn is then abandoned).
	pub fn finish_in_memory_rewind(&self) -> bool {
		if !std::mem::take(&mut *self.lock_applied()) {
			return false;
		}
		self.controller.complete_rewind(&now_iso8601()).is_some()
	}

	/// Roll back an uncommitted no-session cut: the checkpoint stays open and
	/// the unsettled report is dropped so the yield guard applies again.
	pub fn abandon_in_memory_rewind(&self) {
		*self.lock_applied() = false;
		self.controller.clear_pending_rewind_report();
	}

	fn lock_applied(&self) -> std::sync::MutexGuard<'_, bool> {
		self
			.in_memory_rewind_applied
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
	}

	/// Apply the deferred cut at turn end, if a `rewind` reported. The branch
	/// summary and retained report are appended; the leaf follows them, and
	/// abandoned entries remain in the JSONL log.
	pub fn apply_pending_rewind(
		&self,
		store: &mut SessionStore,
	) -> Result<Option<RewindApplied>, String> {
		let Some(pending) = self.controller.pending_rewind() else {
			return Ok(None);
		};
		// Persistence disabled: the loop owns the conversation, so the caller
		// must use `apply_pending_rewind_in_memory` instead of wiping context.
		if !store.enabled() {
			return Ok(None);
		}
		let (branch_from, fell_back_to_root) = match pending.checkpoint_entry_id.as_deref() {
			Some(id) if store.has_entry(id) => (Some(id.to_string()), false),
			Some(_) => (None, true),
			None => (None, false),
		};
		let summary_details = json!({ "startedAt": pending.started_at });
		// Retry after a failed report append resumes from the summary already on
		// disk instead of duplicating it (F3).
		let existing_summary = store
			.branch_entries()
			.last()
			.filter(|entry| {
				entry.kind == "branch_summary"
					&& entry.extra.get("summary").and_then(Value::as_str) == Some(pending.report.as_str())
					&& entry
						.extra
						.get("details")
						.and_then(|details| details.get("startedAt"))
						.and_then(Value::as_str)
						== Some(pending.started_at.as_str())
			})
			.map(|entry| entry.id.clone());
		let branch_summary_id = match existing_summary {
			Some(id) => id,
			None => store.branch_with_summary(
				branch_from.as_deref(),
				&pending.report,
				summary_details,
			)?,
		};
		let rewound_at = now_iso8601();
		let report_entry_id = store.append_custom_message(
			REWIND_REPORT_CUSTOM_TYPE,
			&render_rewind_report(&pending.report),
			false,
			json!({
				"report": pending.report,
				"startedAt": pending.started_at,
				"rewoundAt": rewound_at,
			}),
		)?;
		let completed = self
			.controller
			.complete_rewind(&rewound_at)
			.ok_or_else(|| "checkpoint state disappeared while applying rewind".to_string())?;
		let history = store.branch_history()?;
		Ok(Some(RewindApplied {
			report: completed.report,
			started_at: completed.started_at,
			rewound_at: completed.rewound_at,
			branch_summary_id,
			report_entry_id,
			fell_back_to_root,
			history,
		}))
	}

	/// Provider-ready messages for the active branch: rebuilt turns plus
	/// `branch_summary` (user) and retained `rewind-report` (system/developer)
	/// guidance, in branch order. This is what replaces both the live message
	/// array and the next request's history after a rewind or on resume.
	pub fn context_messages(store: &SessionStore) -> Result<Vec<Message>, String> {
		let by_id: std::collections::HashMap<&str, &MessageEntry> = store
			.messages()
			.iter()
			.map(|message| (message.id.as_str(), message))
			.collect();
		let mut messages = Vec::new();
		let mut run: Vec<MessageEntry> = Vec::new();
		let mut emitted: std::collections::HashSet<&str> = std::collections::HashSet::new();
		let branch = store.branch_entries();
		for (index, entry) in branch.iter().enumerate() {
			if entry.kind == "message" {
				let Some(message) = by_id.get(entry.id.as_str()) else {
					return Err(format!(
						"session branch references missing message entry {}",
						entry.id
					));
				};
				if !emitted.insert(message.id.as_str()) {
					continue;
				}
				if message.message.role == "developer" {
					// Host-injected guidance is not a user turn: keep it verbatim
					// so later requests still carry the plan reminder.
					flush_run(&mut run, &mut messages)?;
					let text = message.message.content.text();
					if !text.trim().is_empty() {
						messages.push(Message::developer(text));
					}
					continue;
				}
				run.push((*message).clone());
				// Same assistant batch, sibling results are not branch
				// ancestors; replay them right after the call message.
				if message.message.role == "assistant" {
					let parts = message.message.content.parts();
					let call_ids: std::collections::HashSet<String> = parts
						.iter()
						.filter(|part| part.kind == "toolCall")
						.filter_map(|part| {
							part.extra
								.get("id")
								.and_then(Value::as_str)
								.map(str::to_string)
						})
						.collect();
					for sibling in store.messages().iter().filter(|sibling| {
						sibling.message.role == "toolResult"
							&& sibling
								.message
								.tool_call_id
								.as_deref()
								.is_some_and(|id| call_ids.contains(id))
					}) {
						if emitted.insert(sibling.id.as_str()) {
							run.push(sibling.clone());
						}
					}
				}
				continue;
			}
			flush_run(&mut run, &mut messages)?;
			match entry.kind.as_str() {
				"branch_summary" => {
					let summary = entry
						.extra
						.get("summary")
						.and_then(Value::as_str)
						.filter(|summary| !summary.is_empty());
					if let Some(summary) = summary {
						messages.push(Message::user(render_branch_summary_context(summary)));
					}
				},
				// Upstream replays every valid custom message; rewind reports are
				// just one custom type. Keep their order among the turns.
				"custom_message" => {
					let custom_type = entry.extra.get("customType").and_then(Value::as_str);
					// A reminder whose checkpoint was already cut is history;
					// replaying it would claim the checkpoint is still active.
					let superseded = custom_type == Some(CHECKPOINT_ACTIVE_REMINDER_TYPE)
						&& branch[index + 1..].iter().any(|later| {
							later.kind == "custom_message"
								&& later
									.extra
									.get("customType")
									.and_then(Value::as_str)
									== Some(REWIND_REPORT_CUSTOM_TYPE)
						});
					if !superseded
						&& let Some(content) = custom_message_text(entry)
						&& !content.is_empty()
					{
						messages.push(Message::system(content));
					}
				},
				_ => {},
			}
		}
		flush_run(&mut run, &mut messages)?;
		Ok(messages)
	}
}

fn tool_report(details: Option<&Value>, content: &str) -> Option<String> {
	let report = match details.and_then(|details| details.get("report")).and_then(Value::as_str) {
		Some(report) if !report.trim().is_empty() => report.trim().to_string(),
		_ => content.trim().to_string(),
	};
	(!report.is_empty()).then_some(report)
}

/// A checkpoint entry already followed by a retained rewind report on the
/// active branch has been cut; replaying its result must not cut again (F2).
fn checkpoint_already_rewound(store: &SessionStore, checkpoint_id: &str) -> bool {
	let branch = store.branch_entries();
	let Some(position) = branch.iter().position(|entry| entry.id == checkpoint_id) else {
		return false;
	};
	branch[position + 1..].iter().any(|entry| {
		entry.kind == "custom_message"
			&& entry
				.extra
				.get("customType")
				.and_then(Value::as_str)
				== Some(REWIND_REPORT_CUSTOM_TYPE)
	})
}

fn flush_run(run: &mut Vec<MessageEntry>, messages: &mut Vec<Message>) -> Result<(), String> {
	if run.is_empty() {
		return Ok(());
	}
	let turns = crate::session::derive_turn_history(run)?;
	run.clear();
	messages.extend(crate::runtime::history_messages(&turns).map_err(|err| err.to_string())?);
	Ok(())
}

fn custom_message_text(entry: &SessionEntry) -> Option<String> {
	match entry.extra.get("content")? {
		Value::String(text) => Some(text.clone()),
		Value::Array(parts) => Some(
			parts
				.iter()
				.filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
				.filter_map(|part| part.get("text").and_then(Value::as_str))
				.collect::<Vec<_>>()
				.join("\n"),
		),
		_ => None,
	}
}

#[cfg(test)]
mod tests {
	use std::fs;
	use std::path::PathBuf;

	use serde_json::json;

	use super::*;
	use crate::session::{MessageContent, MessageEntry, SessionMessage, SessionOptions};

	static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

	fn temp_dir() -> PathBuf {
		let unique = format!(
			"omp-cli-checkpoint-{}-{}",
			std::process::id(),
			COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
		);
		let dir = std::env::temp_dir().join(unique);
		fs::create_dir_all(&dir).expect("temp dir");
		dir
	}

	fn options(dir: PathBuf, id: &str) -> SessionOptions {
		SessionOptions {
			enabled: true,
			cwd: PathBuf::from("/work"),
			model: Some("local/llama3".to_string()),
			provider: Some("local".to_string()),
			dir: Some(dir),
			id: Some(id.to_string()),
			continue_latest: false,
			parent_session: None,
			no_title: true,
		}
	}

	const TS: &str = "2026-10-29T00:00:00.000Z";

	fn user(id: &str, parent: Option<&str>, text: &str) -> MessageEntry {
		MessageEntry::new(
			id,
			parent.map(str::to_string),
			TS,
			SessionMessage::user(MessageContent::Text(text.to_string()), 1),
		)
	}

	fn assistant(id: &str, parent: Option<&str>, text: &str, call: (&str, &str)) -> MessageEntry {
		let mut parts = Vec::new();
		if !text.is_empty() {
			parts.push(crate::session::ContentPart::text(text));
		}
		parts.push(crate::session::ContentPart {
			kind: "toolCall".to_string(),
			text: None,
			data: None,
			mime_type: None,
			extra: serde_json::Map::from_iter([
				("id".to_string(), json!(call.0)),
				("name".to_string(), json!(call.1)),
				("arguments".to_string(), json!({})),
			]),
		});
		let message = SessionMessage::assistant(
			MessageContent::Parts(parts),
			"api",
			"local",
			"llama3",
			None,
			"toolUse",
			2,
		);
		MessageEntry::new(id, parent.map(str::to_string), TS, message)
	}

	fn tool_result(
		id: &str,
		parent: &str,
		call_id: &str,
		tool: &str,
		text: &str,
		details: Option<Value>,
	) -> MessageEntry {
		let mut message = SessionMessage::tool_result(
			call_id,
			tool,
			MessageContent::Text(text.to_string()),
			false,
			3,
		);
		if let Some(details) = details {
			message.extra.insert("details".to_string(), details);
		}
		MessageEntry::new(id, Some(parent.to_string()), TS, message)
	}

	fn checkpoint_turn(store: &mut SessionStore, host: &CheckpointHost, prefix: &str) {
		let call = format!("{prefix}-call");
		let user_id = format!("{prefix}-user");
		let asst_id = format!("{prefix}-asst");
		let result_id = format!("{prefix}-result");
		let mut assistant_entry = assistant(&asst_id, Some(&user_id), "", (&call, "checkpoint"));
		assistant_entry.message.usage = Some(crate::session::Usage {
			input: 11,
			output: 7,
			total_tokens: 18,
			cost: Some(crate::session::Cost {
				total: 0.5,
				..crate::session::Cost::default()
			}),
			..crate::session::Usage::default()
		});
		let entries = vec![
			user(&user_id, store.leaf_id(), "investigate"),
			assistant_entry,
			tool_result(
				&result_id,
				&asst_id,
				&call,
				"checkpoint",
				"Checkpoint: map it\nFinish exploration and formulate findings.",
				Some(json!({ "goal": "map it", "startedAt": TS })),
			),
		];
		store.append_messages(&entries).expect("append checkpoint turn");
		host.note_persisted_tool_results(store, &entries);
		host.persist_active_reminder(store).expect("reminder");
		// A later turn end while the checkpoint is open must not duplicate it.
		host.persist_active_reminder(store).expect("reminder again");
	}

	#[tokio::test]
	async fn lifecycle_rewinds_to_checkpoint_and_rehydrates_retained_report() {
		let dir = temp_dir();
		let host = CheckpointHost::new();
		let mut store = SessionStore::open(&options(dir.clone(), "lifecycle"));
		assert!(store.enabled(), "store: {:?}", store.corrupt_reason());

		checkpoint_turn(&mut store, &host, "cp");
		let reminders = store
			.branch_entries()
			.iter()
			.filter(|entry| {
				entry.extra.get("customType").and_then(Value::as_str)
					== Some(CHECKPOINT_ACTIVE_REMINDER_TYPE)
			})
			.count();
		assert_eq!(reminders, 1, "reminder must be idempotent");
		let checkpoint_state = host.checkpoint_state().expect("active checkpoint");
		assert_eq!(checkpoint_state.goal, "map it");
		assert_eq!(checkpoint_state.checkpoint_entry_id.as_deref(), Some("cp-result"));
		assert!(host.needs_rewind_before_yield());

		// Exploratory investigation after the checkpoint: this path is abandoned.
		let explore = vec![
			user("ex-user", store.leaf_id(), "explore"),
			assistant("ex-asst", Some("ex-user"), "", ("ex-call", "read")),
			tool_result("ex-result", "ex-asst", "ex-call", "read", "contents", None),
		];
		store.append_messages(&explore).expect("append exploration");

		// Rewind result is persisted, then applied at turn end.
		let rewind = vec![
			user("rw-user", store.leaf_id(), "wrap up"),
			assistant("rw-asst", Some("rw-user"), "", ("rw-call", "rewind")),
			tool_result(
				"rw-result",
				"rw-asst",
				"rw-call",
				"rewind",
				"Rewind requested.\nReport captured for context replacement.",
				Some(json!({ "report": "concise findings", "rewound": true })),
			),
		];
		store.append_messages(&rewind).expect("append rewind request");
		host.note_persisted_tool_results(&store, &rewind);
		assert!(!host.needs_rewind_before_yield());

		let before: Vec<String> = store
			.entries()
			.iter()
			.map(|entry| serde_json::to_string(entry).expect("serialize entry"))
			.collect();
		let applied = host
			.apply_pending_rewind(&mut store)
			.expect("apply")
			.expect("pending rewind");
		assert_eq!(applied.report, "concise findings");
		assert!(!applied.fell_back_to_root);
		assert_eq!(store.leaf_id(), Some(applied.report_entry_id.as_str()));
		assert_eq!(host.checkpoint_state(), None);

		// Abandoned entries stay in memory and on disk, byte-identical.
		let after: Vec<String> = store
			.entries()
			.iter()
			.filter(|entry| before.iter().any(|line| line.contains(entry.id.as_str())))
			.map(|entry| serde_json::to_string(entry).expect("serialize entry"))
			.collect();
		for line in &before {
			assert!(after.contains(line), "prior entry changed or missing: {line}");
		}
		assert!(store.branch_messages().iter().all(|entry| entry.id != "ex-result"));
		let retained = store
			.branch_messages()
			.into_iter()
			.find(|entry| entry.id == "cp-asst")
			.expect("retained checkpoint turn");
		let usage = retained.message.usage.as_ref().expect("usage preserved");
		assert_eq!(usage.input, 11);
		assert_eq!(usage.cost.as_ref().expect("cost").total, 0.5);

		let branch_kinds: Vec<&str> = store
			.branch_entries()
			.iter()
			.map(|entry| entry.kind.as_str())
			.collect();
		assert_eq!(branch_kinds.last().copied(), Some("custom_message"));
		assert_eq!(branch_kinds[branch_kinds.len() - 2], "branch_summary");

		// Rebuilt context: retained report is present, exploration is not.
		let context = CheckpointHost::context_messages(&store).expect("context");
		let joined = context
			.iter()
			.map(|message| message.content.as_str())
			.collect::<Vec<_>>()
			.join("\n");
		assert!(joined.contains("Checkpoint called and rewound"));
		assert!(joined.contains("concise findings"));
		assert!(joined.contains("Branch-return summary:"));
		assert!(!joined.contains("contents"), "abandoned exploration leaked into context");

		// Completed guard survives resume.
		drop(store);
		let reopened = SessionStore::open(&options(dir.clone(), "lifecycle"));
		assert!(reopened.enabled());
		let resumed = CheckpointHost::new();
		resumed.rehydrate(&reopened);
		assert_eq!(resumed.checkpoint_state(), None);
		let completed = resumed.last_completed_rewind().expect("completed");
		assert_eq!(completed.report, "concise findings");
		let rewind = resumed.rewind_tool();
		use pi_agent::Tool as _;
		let context = pi_agent::AgentContext::new(std::env::temp_dir());
		let error = rewind
			.execute(&json!({ "report": "again" }), &context)
			.await
			.expect_err("completed guard");
		assert_eq!(
			error.to_string(),
			"Checkpoint already completed; continue from the retained rewind report instead of calling rewind again."
		);
		assert!(!applied.history.iter().any(|turn| turn.prompt == "explore"));
		assert!(!applied.history.iter().any(|turn| turn.prompt == "wrap up"));

		fs::remove_dir_all(dir).ok();
	}

	#[test]
	fn yield_guard_continues_until_rewind_and_skips_tool_turns() {
		let host = CheckpointHost::new();
		let _ = host
			.controller()
			.note_checkpoint("goal", TS, 3);
		let live = vec![Message::system("system prompt"), Message::user("investigate")];

		let continuation = host
			.yield_continuation(&Message::assistant("done"), &live)
			.expect("warning continuation");
		assert_eq!(continuation.len(), live.len() + 1);
		assert!(continuation.last().expect("warning").content.contains("You MUST call rewind"));
		assert_eq!(continuation[0], live[0], "system head preserved");

		let with_tool = Message::assistant_with_tools(
			"",
			vec![pi_agent::ToolCall {
				id: "call".to_string(),
				name: "read".to_string(),
				arguments: json!({}),
			}],
		);
		assert!(host.yield_continuation(&with_tool, &live).is_none());

		assert!(host.controller().note_rewind_tool_result("report"));
		assert!(host.yield_continuation(&Message::assistant("done"), &live).is_none());
	}

	#[test]
	fn unfinished_checkpoint_rehydrates_and_missing_entry_falls_back_to_root() {
		let dir = temp_dir();
		let host = CheckpointHost::new();
		let mut store = SessionStore::open(&options(dir.clone(), "resume"));
		checkpoint_turn(&mut store, &host, "cp");
		drop(store);

		let mut reopened = SessionStore::open(&options(dir.clone(), "resume"));
		let resumed = CheckpointHost::new();
		resumed.rehydrate(&reopened);
		let state = resumed.checkpoint_state().expect("checkpoint restored");
		assert_eq!(state.checkpoint_entry_id.as_deref(), Some("cp-result"));
		assert!(resumed.needs_rewind_before_yield());

		// A checkpoint entry that no longer resolves falls back to root.
		resumed.controller().clear();
		let _ = resumed
			.controller()
			.note_checkpoint("map it", TS, reopened.messages().len());
		resumed
			.controller()
			.backfill_checkpoint_entry_id("missing-entry");
		resumed.controller().note_rewind_tool_result("fallback report");

		let applied = resumed
			.apply_pending_rewind(&mut reopened)
			.expect("apply")
			.expect("pending");
		assert!(applied.fell_back_to_root);
		let branch = reopened.branch_entries();
		assert_eq!(branch.len(), 2, "root fallback keeps only summary + report");
		assert_eq!(branch[0].kind, "branch_summary");
		assert_eq!(branch[0].parent_id, None);
		assert_eq!(branch[1].kind, "custom_message");

		fs::remove_dir_all(dir).ok();
	}

	#[test]
	fn in_memory_rewind_survives_next_prompt_without_persistence() {
		let mut branch = vec![
			Message::user("investigate"),
			Message::assistant_with_tools(
				"",
				vec![pi_agent::ToolCall {
					id: "cp-call".to_string(),
					name: "checkpoint".to_string(),
					arguments: json!({ "goal": "map it" }),
				}],
			),
			Message::tool_with_status(
				"cp-call",
				"checkpoint",
				"Checkpoint: map it\nFinish exploration and formulate findings.",
				false,
			),
			Message::user("explore"),
			Message::assistant("secret contents"),
		];
		let rewind = InMemoryRewind {
			keep: 0,
			checkpoint_tool_call_id: Some("cp-call".to_string()),
			summary: render_branch_summary_context("map it"),
			report: render_rewind_report("concise findings"),
		};
		apply_in_memory_rewind(&mut branch, &rewind);
		// The loop runs one more assistant step after the cut.
		branch.push(Message::assistant("rewound answer"));

		let joined = branch
			.iter()
			.map(|message| message.content.as_str())
			.collect::<Vec<_>>()
			.join("\n");
		assert!(joined.contains("concise findings"));
		assert!(!joined.contains("secret contents"));

		// The next prompt replays this branch prefix and appends one user turn.
		let mut next_prompt = branch.clone();
		next_prompt.push(Message::user("next question"));
		assert_eq!(
			next_prompt
				.iter()
				.filter(|message| message.content == "next question")
				.count(),
			1
		);
		assert_eq!(
			next_prompt[branch.len() - 1].content,
			"rewound answer",
			"previous answer must survive into the next prompt"
		);
	}

	#[test]
	fn in_memory_rewind_keeps_sibling_results_of_checkpoint_batch() {
		let mut branch = vec![
			Message::user("investigate"),
			Message::assistant_with_tools(
				"",
				vec![
					pi_agent::ToolCall {
						id: "cp-call".to_string(),
						name: "checkpoint".to_string(),
						arguments: json!({ "goal": "map it" }),
					},
					pi_agent::ToolCall {
						id: "read-call".to_string(),
						name: "read".to_string(),
						arguments: json!({}),
					},
				],
			),
			Message::tool_with_status(
				"cp-call",
				"checkpoint",
				"Checkpoint: map it\nFinish exploration and formulate findings.",
				false,
			),
			Message::tool_with_status("read-call", "read", "sibling contents", false),
			Message::assistant("secret contents"),
		];
		let rewind = InMemoryRewind {
			keep: 0,
			checkpoint_tool_call_id: Some("cp-call".to_string()),
			summary: render_branch_summary_context("map it"),
			report: render_rewind_report("concise findings"),
		};
		apply_in_memory_rewind(&mut branch, &rewind);
		let joined = branch
			.iter()
			.map(|message| message.content.as_str())
			.collect::<Vec<_>>()
			.join("\n");
		assert!(joined.contains("sibling contents"), "sibling result kept");
		assert!(joined.contains("concise findings"));
		assert!(!joined.contains("secret contents"));
	}

	#[test]
	fn same_batch_checkpoint_and_rewind_drops_active_notice() {
		let dir = temp_dir();
		let host = CheckpointHost::new();
		let mut store = SessionStore::open(&options(dir.clone(), "same-batch"));
		let entries = vec![
			user("sb-user", None, "checkpoint and rewind"),
			assistant("sb-cp-asst", Some("sb-user"), "", ("sb-cp", "checkpoint")),
			tool_result(
				"sb-cp-result",
				"sb-cp-asst",
				"sb-cp",
				"checkpoint",
				"Checkpoint: map it\nFinish exploration and formulate findings.",
				Some(json!({ "goal": "map it", "startedAt": "2026-10-29T00:00:00.000Z" })),
			),
			assistant("sb-rw-asst", Some("sb-cp-result"), "", ("sb-rw", "rewind")),
			tool_result(
				"sb-rw-result",
				"sb-rw-asst",
				"sb-rw",
				"rewind",
				"Rewind requested.",
				Some(json!({ "report": "same batch findings", "rewound": true })),
			),
		];
		store.append_messages(&entries).expect("batch");
		assert!(host.note_persisted_tool_results(&store, &entries));
		host.persist_active_reminder(&mut store).expect("reminder");
		assert!(host.apply_pending_rewind(&mut store).expect("apply").is_some());
		let context = CheckpointHost::context_messages(&store).expect("context");
		let text = context
			.iter()
			.map(|message| message.content.as_str())
			.collect::<Vec<_>>()
			.join("\n");
		assert!(text.contains("same batch findings"));
		assert!(
			!text.contains(CHECKPOINT_ACTIVE_NOTICE),
			"completed cut must not replay the active reminder"
		);
		fs::remove_dir_all(dir).ok();
	}

	#[test]
	fn batched_checkpoint_siblings_survive_rewind_branch() {
		let dir = temp_dir();
		let host = CheckpointHost::new();
		let mut store = SessionStore::open(&options(dir.clone(), "batch-siblings"));
		let mut batch = assistant("bs-asst", Some("bs-user"), "", ("bs-cp", "checkpoint"));
		if let crate::session::MessageContent::Parts(parts) = &mut batch.message.content {
			parts.push(crate::session::ContentPart {
				kind: "toolCall".to_string(),
				text: None,
				data: None,
				mime_type: None,
				extra: serde_json::Map::from_iter([
					("id".to_string(), json!("bs-read")),
					("name".to_string(), json!("read")),
					("arguments".to_string(), json!({})),
				]),
			});
		}
		let entries = vec![
			user("bs-user", None, "checkpoint and read"),
			batch,
			tool_result(
				"bs-cp-result",
				"bs-asst",
				"bs-cp",
				"checkpoint",
				"Checkpoint: map it\nFinish exploration and formulate findings.",
				None,
			),
			tool_result(
				"bs-read-result",
				"bs-cp-result",
				"bs-read",
				"read",
				"sibling contents",
				None,
			),
		];
		store.append_messages(&entries).expect("batch");
		assert!(host.note_persisted_tool_results(&store, &entries));
		assert!(host.controller().note_rewind_tool_result("concise findings"));
		assert!(host.apply_pending_rewind(&mut store).expect("apply").is_some());
		let context = CheckpointHost::context_messages(&store).expect("context");
		let text = context
			.iter()
			.map(|message| message.content.as_str())
			.collect::<Vec<_>>()
			.join("\n");
		assert!(text.contains("sibling contents"), "sibling result kept");
		assert!(text.contains("concise findings"));
		let history = store.branch_history().expect("history");
		let call = history
			.iter()
			.flat_map(|turn| &turn.tool_calls)
			.find(|call| call.id == "bs-read")
			.expect("read call");
		assert_eq!(call.result, "sibling contents");
		assert!(!call.is_error);
		fs::remove_dir_all(dir).ok();
	}

	#[test]
	fn abandoned_in_memory_cut_keeps_pending_report() {
		let host = CheckpointHost::new();
		host
			.controller()
			.note_checkpoint("map it", "2026-10-29T00:00:00.000Z", 1);
		assert!(host.controller().note_rewind_tool_result("findings"));
		assert!(host.apply_pending_rewind_in_memory().is_some());
		host.abandon_in_memory_rewind();
		assert!(host.controller().checkpoint_state().is_some(), "checkpoint stays open");
		assert!(host.controller().pending_rewind().is_none(), "unsettled report dropped");
		assert!(host.controller().note_rewind_tool_result("findings again"));
		assert!(
			host.apply_pending_rewind_in_memory().is_some(),
			"retry after rollback must recut"
		);
		assert!(host.finish_in_memory_rewind());
		assert!(host.controller().pending_rewind().is_none());
		assert!(host.controller().last_completed_rewind().is_some());
	}

	#[test]
	fn incoherent_parent_chain_falls_back_to_journal_order() {
		let dir = temp_dir();
		let mut store = SessionStore::open(&options(dir.clone(), "chain"));
		store
			.append_messages(&[
				user("chain-1", None, "one"),
				user("chain-2", None, "two"),
			])
			.expect("append");
		assert!(store.branch_chain_warning().is_some());
		assert_eq!(store.branch_entries().len(), 2);
		assert!(store.has_branch_context());
		assert_eq!(store.branch_messages().len(), 2);
		fs::remove_dir_all(dir).ok();
	}
}
