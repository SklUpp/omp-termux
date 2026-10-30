//! Subagent observability for RPC: snapshots, event frames and transcript paging.
//!
//! Mirrors `packages/coding-agent/src/modes/rpc/rpc-subagents.ts`. Payloads are
//! the native task-observer shapes (`id`, `index`, `agent`, `status`, ...).
//! [`SubagentRegistry::observer`] builds the handle the runner subscribes to
//! the live supervisor once (through `RunRequest.subagent_observer`), so the
//! commands report the live snapshots.

use std::{
	collections::{BTreeMap, BTreeSet},
	io::{Read, Seek, SeekFrom},
	path::Path,
	sync::{
		Arc, Mutex, MutexGuard,
		atomic::{AtomicBool, Ordering},
	},
};

use pi_agent::{
	AgentEvent, Message, TurnStatus,
	supervisor::{SubagentEvent, SubagentObserver},
};
use serde_json::{Value, json};

use super::{
	event_wire,
	session::FrameSink,
	types::SubagentSubscriptionLevel,
};

const MAX_RETAINED_TRANSCRIPT_REFERENCES: usize = 256;

/// Bridges the native supervisor's observer bus into this registry.
///
/// The payloads it builds are the same shapes `handle_lifecycle` /
/// `handle_progress` already parse, so the registry stays transport-agnostic.
/// One handle is created per RPC mode and handed to the runner through
/// `RunRequest.subagent_observer`; it forwards nothing while the registry is
/// detached.
struct RegistryObserver {
	registry: Arc<SubagentRegistry>,
	sink:     Arc<dyn FrameSink>,
	/// Streamed assistant text per child id: TS `message_update` carries the
	/// accumulated message, not just the delta. Cleared when the turn closes.
	text:     Mutex<BTreeMap<String, String>>,
}

impl SubagentObserver for RegistryObserver {
	fn observe(&self, event: &SubagentEvent) {
		if !self.registry.is_attached() {
			return;
		}
		match event {
			SubagentEvent::Lifecycle(lifecycle) => {
				let mut payload = json!({
					"id": lifecycle.id,
					"index": lifecycle.index,
					"agent": lifecycle.agent,
					"agentSource": lifecycle.agent_source,
					"task": lifecycle.task,
					"status": lifecycle.status.as_str(),
				});
				if let Some(session_file) = &lifecycle.session_file {
					payload["sessionFile"] = json!(session_file);
				}
				if let Some(parent) = &lifecycle.parent_tool_call_id {
					payload["parentToolCallId"] = json!(parent);
				}
				if lifecycle.detached {
					payload["detached"] = json!(true);
				}
				if let Some(error) = &lifecycle.error {
					payload["error"] = json!(error);
				}
				if let Some(reason) = &lifecycle.abort_reason {
					payload["abortReason"] = json!(reason);
				}
				if let Some(duration_ms) = lifecycle.duration_ms {
					payload["durationMs"] = json!(duration_ms);
				}
				self.registry.handle_lifecycle(&payload, self.sink.as_ref());
			},
			SubagentEvent::Progress(progress) => {
				let mut payload = json!({
					"index": progress.index,
					"agent": progress.agent,
					"agentSource": progress.agent_source,
					"task": progress.task,
					"progress": {
						"id": progress.id,
						"status": progress.status,
						"description": progress.description,
					},
				});
				if let Some(parent) = &progress.parent_tool_call_id {
					payload["parentToolCallId"] = json!(parent);
				}
				if let Some(session_file) = &progress.session_file {
					payload["sessionFile"] = json!(session_file);
				}
				if progress.detached {
					payload["detached"] = json!(true);
				}
				self.registry.handle_progress(&payload, self.sink.as_ref());
			},
			SubagentEvent::Event { id, event } => {
				let mut buffers = self.text.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
				let text = buffers.entry(id.clone()).or_default();
				for mapped in agent_event_frames(event, text) {
					let payload = json!({ "id": id, "event": mapped });
					self.registry.handle_event(&payload, self.sink.as_ref());
				}
			},
		}
	}
}

/// Map a native child-loop event onto the RPC session-event frames it must
/// produce, through the shared [`event_wire`] builders the main session also
/// uses — so a child frame cannot drift from a root frame.
///
/// The mapping is exhaustive over the loop's `AgentEvent`: a new variant is a
/// compile error here until it gets real TS semantics. Accepted steering becomes
/// the TS input-message pair ([`event_wire::user_message_pair`], mirroring
/// `emitInputMessages` in `packages/agent/src/agent-loop.ts:967-971`): a child has
/// no host-visible history, so this pair is its only signal that the instruction
/// was accepted. Native `api`/`provider`/`model` are unknown for a child (the
/// native message does not carry them), so they are emitted empty rather than
/// invented.
fn agent_event_frames(event: &AgentEvent, text: &mut String) -> Vec<Value> {
	let now = event_wire::now_ms();
	match event {
		AgentEvent::MessageUpdate { delta } => {
			text.push_str(delta);
			let snapshot = Message::assistant(text.clone());
			vec![event_wire::message_update(&snapshot, Some(delta), "", "", "", "", now)]
		},
		AgentEvent::MessageEnd { message, status } => vec![event_wire::message_end(
			event_wire::assistant_message(message, "", "", "", stop_reason(status), now),
			now,
		)],
		AgentEvent::ToolStart { id, name, arguments } => {
			vec![event_wire::tool_start(id, name, arguments)]
		},
		AgentEvent::ToolUpdate { id, name, args, partial } => {
			vec![event_wire::tool_update(id, name, args, partial)]
		},
		AgentEvent::ToolEnd { id, name, result, is_error, images, details } => {
			vec![event_wire::tool_end(id, name, result, images, details.as_ref(), *is_error)]
		},
		AgentEvent::TurnEnd { message, tool_results, status } => {
			// TS `turn_end.toolResults` is a list of `ToolResultMessage` values,
			// not `tool_execution_end` frames: each entry is flat
			// (`role`/`toolCallId`/`toolName`/`content`/`details`/`isError`/
			// `timestamp`). The image blocks are the *same validated* parts the
			// loop builds for the provider request (`details.images`), so the turn
			// list carries real images in `content`, and `details` still travels
			// verbatim as the host audit payload (`details.content` is never read
			// as parts). A rejected payload already failed closed upstream, so the
			// default here cannot hide a bad image from `tool_execution_end`.
			let tool_results: Vec<Value> = tool_results
				.iter()
				.map(|result| {
					let name = message
						.tool_calls
						.iter()
						.find(|call| call.id == result.tool_call_id)
						.map_or("", |call| call.name.as_str());
					// An invalid image payload must never become a silent empty
					// success: mirror the loop's batch behaviour exactly — mark the
					// result failed, append the same diagnostic, attach no parts.
					let mut result = result.clone();
					let images = match pi_agent::tool_image_parts_of(&result) {
						Ok(images) => images,
						Err(error) => {
							result.is_error = true;
							result.content = format!(
								"{}\n\n[image payload rejected: {error}]",
								result.content
							);
							Vec::new()
						},
					};
					event_wire::tool_result_message(&result, name, &images, now)
				})
				.collect();
			let frame = json!({
				"type": "turn_end",
				"message": event_wire::assistant_message(
					message,
					"",
					"",
					"",
					stop_reason(status),
					now,
				),
				"toolResults": tool_results,
			});
			text.clear();
			vec![frame]
		},
		AgentEvent::UserSteer { parts } => event_wire::user_message_pair(parts, now).to_vec(),
		// The child has no host-visible history and the root accounts usage from
		// its own goal runtime, so the mapper has no frame to emit.
		AgentEvent::BackgroundResult { .. } => Vec::new(),
	}
}

/// TS `stopReason` of a native turn status (the words
/// `session::canonical_stop_reason` normalizes to).
fn stop_reason(status: &TurnStatus) -> &'static str {
	match status {
		TurnStatus::EndTurn => "stop",
		TurnStatus::ToolUse => "toolUse",
		TurnStatus::Aborted => "aborted",
		TurnStatus::Error(_) => "error",
	}
}

/// Snapshot of one running subagent (`RpcSubagentSnapshot`).
#[derive(Debug, Clone, PartialEq)]
pub struct SubagentSnapshot {
	pub id:               String,
	pub index:            Option<i64>,
	pub agent:            Option<String>,
	pub agent_source:     Option<String>,
	pub description:      Option<String>,
	pub status:           String,
	pub task:             Option<String>,
	pub assignment:       Option<String>,
	pub session_file:     Option<String>,
	pub parent_tool_call_id: Option<String>,
	pub last_update:      u64,
	pub progress:         Option<Value>,
}

impl SubagentSnapshot {
	#[must_use]
	pub fn to_wire(&self) -> Value {
		let mut value = json!({
			"id": self.id,
			"status": self.status,
			"lastUpdate": self.last_update,
		});
		if let Some(index) = self.index {
			value["index"] = json!(index);
		}
		if let Some(agent) = &self.agent {
			value["agent"] = json!(agent);
		}
		if let Some(agent_source) = &self.agent_source {
			value["agentSource"] = json!(agent_source);
		}
		if let Some(description) = &self.description {
			value["description"] = json!(description);
		}
		if let Some(task) = &self.task {
			value["task"] = json!(task);
		}
		if let Some(assignment) = &self.assignment {
			value["assignment"] = json!(assignment);
		}
		if let Some(session_file) = &self.session_file {
			value["sessionFile"] = json!(session_file);
		}
		if let Some(parent) = &self.parent_tool_call_id {
			value["parentToolCallId"] = json!(parent);
		}
		if let Some(progress) = &self.progress {
			value["progress"] = progress.clone();
		}
		value
	}
}

/// Latest transcript offsets for one subagent session file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptSlice {
	pub session_file: String,
	pub from_byte:    usize,
	pub next_byte:    usize,
	pub reset:        bool,
	pub entries:      Vec<Value>,
	pub messages:     Vec<Value>,
}

impl TranscriptSlice {
	#[must_use]
	pub fn to_wire(&self) -> Value {
		json!({
			"sessionFile": self.session_file,
			"fromByte": self.from_byte,
			"nextByte": self.next_byte,
			"reset": self.reset,
			"entries": self.entries,
			"messages": self.messages,
		})
	}
}

/// Read appended transcript bytes from a subagent session log.
///
/// Only complete lines are parsed; `nextByte` points at the last newline so a
/// partially written record is re-read on the next call. A `fromByte` beyond
/// the file size restarts at zero and reports `reset`.
pub fn read_transcript(session_file: &str, from_byte: i64) -> Result<TranscriptSlice, String> {
	let mut start = usize::try_from(from_byte.max(0)).unwrap_or(0);
	let path = Path::new(session_file);
	let mut file = match std::fs::File::open(path) {
		Ok(file) => file,
		Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
			return Ok(TranscriptSlice {
				session_file: session_file.to_string(),
				from_byte: start,
				next_byte: start,
				reset: false,
				entries: Vec::new(),
				messages: Vec::new(),
			});
		},
		Err(err) => return Err(format!("cannot read subagent transcript: {err}")),
	};
	let size = file
		.metadata()
		.map_err(|err| format!("cannot read subagent transcript: {err}"))?
		.len();
	let size = usize::try_from(size).unwrap_or(usize::MAX);
	let mut reset = false;
	if start > size {
		start = 0;
		reset = true;
	}
	let mut text = String::new();
	if start < size {
		file
			.seek(SeekFrom::Start(start as u64))
			.map_err(|err| format!("cannot read subagent transcript: {err}"))?;
		let mut bytes = Vec::new();
		file
			.read_to_end(&mut bytes)
			.map_err(|err| format!("cannot read subagent transcript: {err}"))?;
		text = String::from_utf8_lossy(&bytes).into_owned();
	}
	let complete = match text.rfind('\n') {
		Some(index) => &text[..=index],
		None => "",
	};
	let entries: Vec<Value> = complete
		.lines()
		.filter(|line| !line.trim().is_empty())
		.filter_map(|line| serde_json::from_str::<Value>(line).ok())
		.collect();
	let messages = entries
		.iter()
		.filter(|entry| entry.get("type").and_then(Value::as_str) == Some("message"))
		.filter_map(|entry| entry.get("message").cloned())
		.collect();
	let next_byte = start + complete.len();
	Ok(TranscriptSlice {
		session_file: session_file.to_string(),
		from_byte: start,
		next_byte,
		reset,
		entries,
		messages,
	})
}

/// Ids of lifecycle/progress events already seen, so late frames are ignored.
#[derive(Default)]
pub struct SubagentRegistry {
	subagents: Mutex<BTreeMap<String, SubagentSnapshot>>,
	transcript_files: Mutex<BTreeMap<String, String>>,
	stale: Mutex<BTreeSet<String>>,
	level: Mutex<SubagentSubscriptionLevel>,
	/// Set once a supervisor observer is installed; the `get_subagents` /
	/// subscription commands report the mount prerequisite without it.
	attached: AtomicBool,
}

impl SubagentRegistry {
	#[must_use]
	pub fn new() -> Self {
		Self::default()
	}

	/// Observer handle for `RunRequest.subagent_observer`. One per RPC mode:
	/// the runner subscribes it with `Supervisor::subscribe_once` as soon as the
	/// lazily built supervisor exists. Events are forwarded only while the
	/// registry stays attached ([`Self::detach`] stops them).
	pub fn observer(
		self: &Arc<Self>,
		sink: Arc<dyn FrameSink>,
	) -> Arc<dyn SubagentObserver> {
		self.attached.store(true, Ordering::SeqCst);
		Arc::new(RegistryObserver {
			registry: Arc::clone(self),
			sink,
			text: Mutex::new(BTreeMap::new()),
		})
	}

	/// Stop forwarding supervisor events (shutdown). The runner may keep the
	/// handle; it becomes a no-op.
	pub fn detach(&self) {
		self.attached.store(false, Ordering::SeqCst);
	}

	/// Whether a supervisor observer is installed.
	#[must_use]
	pub fn is_attached(&self) -> bool {
		self.attached.load(Ordering::SeqCst)
	}

	/// Drop every live snapshot and mark its id stale, so late lifecycle or
	/// progress frames for a previous session are ignored (`clear()` upstream).
	/// Transcript references stay, so finished logs remain pageable.
	pub fn clear(&self) {
		let live: Vec<String> = self.lock_subagents().keys().cloned().collect();
		let files: Vec<String> = self.lock_transcripts().keys().cloned().collect();
		{
			let mut stale = self.lock_stale();
			for id in live.iter().chain(files.iter()) {
				stale.insert(id.clone());
			}
			while stale.len() > MAX_RETAINED_TRANSCRIPT_REFERENCES {
				let Some(oldest) = stale.iter().next().cloned() else {
					break;
				};
				stale.remove(&oldest);
			}
		}
		self.lock_subagents().clear();
	}

	pub fn set_subscription_level(&self, level: SubagentSubscriptionLevel) {
		*self.lock_level() = level;
	}

	#[must_use]
	pub fn subscription_level(&self) -> SubagentSubscriptionLevel {
		*self.lock_level()
	}

	/// Snapshots ordered by `index` then `id`.
	#[must_use]
	pub fn snapshots(&self) -> Vec<SubagentSnapshot> {
		let mut values: Vec<SubagentSnapshot> =
			self.lock_subagents().values().cloned().collect();
		values.sort_by(|a, b| {
			a.index
				.unwrap_or(i64::MAX)
				.cmp(&b.index.unwrap_or(i64::MAX))
				.then_with(|| a.id.cmp(&b.id))
		});
		values
	}

	#[must_use]
	pub fn snapshots_wire(&self) -> Value {
		json!({ "subagents": self.snapshots().iter().map(SubagentSnapshot::to_wire).collect::<Vec<_>>() })
	}

	/// Apply one lifecycle payload (`subagent_lifecycle`).
	pub fn handle_lifecycle(&self, payload: &Value, sink: &dyn FrameSink) {
		let Some(id) = payload.get("id").and_then(Value::as_str) else {
			return;
		};
		let status = payload.get("status").and_then(Value::as_str).unwrap_or("started");
		let existing = self.lock_subagents().get(id).cloned();
		if let Some(existing) = &existing {
			if !same_owner(payload, existing) {
				return;
			}
		} else if status != "started" {
			return;
		}
		if status == "started" {
			self.lock_stale().remove(id);
		}
		let session_file = payload
			.get("sessionFile")
			.and_then(Value::as_str)
			.map(str::to_string)
			.or_else(|| existing.as_ref().and_then(|snapshot| snapshot.session_file.clone()));
		let snapshot = SubagentSnapshot {
			id: id.to_string(),
			index: payload.get("index").and_then(Value::as_i64).or(existing.as_ref().and_then(|s| s.index)),
			agent: payload.get("agent").and_then(Value::as_str).map(str::to_string),
			agent_source: payload
				.get("agentSource")
				.and_then(Value::as_str)
				.map(str::to_string),
			description: payload
				.get("description")
				.and_then(Value::as_str)
				.map(str::to_string)
				.or_else(|| existing.as_ref().and_then(|s| s.description.clone())),
			status: if status == "started" { "running".to_string() } else { status.to_string() },
			task: payload
				.get("task")
				.and_then(Value::as_str)
				.map(str::to_string)
				.or_else(|| existing.as_ref().and_then(|s| s.task.clone())),
			assignment: existing.as_ref().and_then(|s| s.assignment.clone()),
			session_file: session_file.clone(),
			parent_tool_call_id: payload
				.get("parentToolCallId")
				.and_then(Value::as_str)
				.map(str::to_string)
				.or_else(|| existing.as_ref().and_then(|s| s.parent_tool_call_id.clone())),
			last_update: now_ms(),
			progress: existing.as_ref().and_then(|s| s.progress.clone()),
		};
		self.remember_transcript(id, session_file.as_deref());
		if status == "started" {
			self.lock_subagents().insert(id.to_string(), snapshot);
		} else {
			self.lock_subagents().remove(id);
		}
		if self.subscription_level() != SubagentSubscriptionLevel::Off {
			sink.send(json!({ "type": "subagent_lifecycle", "payload": payload }));
		}
	}

	/// Apply one progress payload (`subagent_progress`).
	pub fn handle_progress(&self, payload: &Value, sink: &dyn FrameSink) {
		let Some(progress) = payload.get("progress") else {
			return;
		};
		let Some(id) = progress.get("id").and_then(Value::as_str) else {
			return;
		};
		if self.lock_stale().contains(id) {
			return;
		}
		let Some(existing) = self.lock_subagents().get(id).cloned() else {
			return;
		};
		if !same_owner(payload, &existing) {
			return;
		}
		let session_file = payload
			.get("sessionFile")
			.and_then(Value::as_str)
			.map(str::to_string)
			.or_else(|| existing.session_file.clone());
		self.remember_transcript(id, session_file.as_deref());
		let snapshot = SubagentSnapshot {
			id: id.to_string(),
			index: payload.get("index").and_then(Value::as_i64).or(existing.index),
			agent: payload.get("agent").and_then(Value::as_str).map(str::to_string),
			agent_source: payload
				.get("agentSource")
				.and_then(Value::as_str)
				.map(str::to_string),
			description: progress
				.get("description")
				.and_then(Value::as_str)
				.map(str::to_string)
				.or(existing.description.clone()),
			status: progress
				.get("status")
				.and_then(Value::as_str)
				.unwrap_or(&existing.status)
				.to_string(),
			task: payload.get("task").and_then(Value::as_str).map(str::to_string),
			assignment: payload.get("assignment").and_then(Value::as_str).map(str::to_string),
			session_file,
			parent_tool_call_id: payload
				.get("parentToolCallId")
				.and_then(Value::as_str)
				.map(str::to_string)
				.or(existing.parent_tool_call_id.clone()),
			last_update: now_ms(),
			progress: Some(progress.clone()),
		};
		self.lock_subagents().insert(id.to_string(), snapshot);
		if self.subscription_level() != SubagentSubscriptionLevel::Off {
			sink.send(json!({ "type": "subagent_progress", "payload": payload }));
		}
	}

	/// Forward one raw agent event frame when subscribed to `events`.
	pub fn handle_event(&self, payload: &Value, sink: &dyn FrameSink) {
		let Some(id) = payload.get("id").and_then(Value::as_str) else {
			return;
		};
		if self.lock_stale().contains(id) {
			return;
		}
		if self.subscription_level() != SubagentSubscriptionLevel::Events {
			return;
		}
		sink.send(json!({ "type": "subagent_event", "payload": payload }));
	}

	/// Resolve which transcript file a selector names.
	pub fn resolve_session_file(
		&self,
		subagent_id: Option<&str>,
		session_file: Option<&str>,
	) -> Result<String, String> {
		if let Some(id) = subagent_id {
			let known = self
				.lock_subagents()
				.get(id)
				.and_then(|snapshot| snapshot.session_file.clone())
				.or_else(|| self.lock_transcripts().get(id).cloned());
			return known.ok_or_else(|| {
				format!("Unknown subagent or session file unavailable: {id}")
			});
		}
		if let Some(session_file) = session_file {
			if self.has_transcript_file(session_file) {
				return Ok(session_file.to_string());
			}
			return Err("Unknown subagent session file".to_string());
		}
		Err("get_subagent_messages requires subagentId or sessionFile".to_string())
	}

	fn has_transcript_file(&self, session_file: &str) -> bool {
		self
			.lock_subagents()
			.values()
			.any(|snapshot| snapshot.session_file.as_deref() == Some(session_file))
			|| self
				.lock_transcripts()
				.values()
				.any(|known| known == session_file)
	}

	fn remember_transcript(&self, id: &str, session_file: Option<&str>) {
		let Some(session_file) = session_file else {
			return;
		};
		let mut files = self.lock_transcripts();
		files.remove(id);
		files.insert(id.to_string(), session_file.to_string());
		while files.len() > MAX_RETAINED_TRANSCRIPT_REFERENCES {
			let oldest = files.keys().next().cloned();
			let Some(oldest) = oldest else {
				break;
			};
			files.remove(&oldest);
		}
	}

	fn lock_subagents(&self) -> MutexGuard<'_, BTreeMap<String, SubagentSnapshot>> {
		self.subagents.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
	}

	fn lock_transcripts(&self) -> MutexGuard<'_, BTreeMap<String, String>> {
		self.transcript_files.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
	}

	fn lock_stale(&self) -> MutexGuard<'_, BTreeSet<String>> {
		self.stale.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
	}

	fn lock_level(&self) -> MutexGuard<'_, SubagentSubscriptionLevel> {
		self.level.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
	}
}

fn same_owner(payload: &Value, snapshot: &SubagentSnapshot) -> bool {
	if let (Some(payload_parent), Some(snapshot_parent)) = (
		payload.get("parentToolCallId").and_then(Value::as_str),
		snapshot.parent_tool_call_id.as_deref(),
	) {
		return payload_parent == snapshot_parent;
	}
	if let (Some(payload_file), Some(snapshot_file)) = (
		payload.get("sessionFile").and_then(Value::as_str),
		snapshot.session_file.as_deref(),
	) {
		return payload_file == snapshot_file;
	}
	true
}

fn now_ms() -> u64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.unwrap_or_default()
		.as_millis() as u64
}

/// Unused marker kept so the registry can be shared with another transport.
pub type SharedRegistry = Arc<SubagentRegistry>;

#[cfg(test)]
mod tests {
	use super::*;

	#[derive(Default)]
	struct CollectSink {
		frames: Mutex<Vec<Value>>,
	}

	impl FrameSink for CollectSink {
		fn send(&self, frame: Value) {
			self.frames.lock().expect("frames").push(frame);
		}
	}

	/// The single frame a single-frame mapping produces; asserts the count so an
	/// accidental extra frame cannot slip past the per-variant assertions.
	fn single(event: &AgentEvent) -> Value {
		let mut text = String::new();
		let mut frames = agent_event_frames(event, &mut text);
		assert_eq!(frames.len(), 1, "expected one frame for {event:?}");
		frames.remove(0)
	}

	impl CollectSink {
		fn types(&self) -> Vec<String> {
			self
				.frames
				.lock()
				.expect("frames")
				.iter()
				.filter_map(|frame| frame.get("type").and_then(Value::as_str).map(str::to_string))
				.collect()
		}
	}

	fn tempfile(name: &str) -> std::path::PathBuf {
		let dir = std::env::temp_dir().join(format!("omp-rpc-subagents-{}", std::process::id()));
		std::fs::create_dir_all(&dir).expect("dir");
		dir.join(name)
	}

	#[test]
	fn lifecycle_start_progress_and_terminal_are_tracked() {
		let registry = SubagentRegistry::new();
		let sink = CollectSink::default();
		registry.set_subscription_level(SubagentSubscriptionLevel::Progress);
		registry.handle_lifecycle(
			&json!({
				"id": "s1",
				"index": 2,
				"agent": "scout",
				"status": "started",
				"sessionFile": "/tmp/s1.jsonl",
			}),
			&sink,
		);
		let snapshots = registry.snapshots();
		assert_eq!(snapshots.len(), 1);
		assert_eq!(snapshots[0].status, "running");
		assert_eq!(snapshots[0].index, Some(2));
		assert_eq!(sink.types(), vec!["subagent_lifecycle".to_string()]);

		registry.handle_progress(
			&json!({
				"index": 2,
				"agent": "scout",
				"progress": { "id": "s1", "status": "running", "description": "scanning" },
			}),
			&sink,
		);
		let snapshots = registry.snapshots();
		assert_eq!(snapshots[0].description.as_deref(), Some("scanning"));
		assert!(snapshots[0].progress.is_some());
		assert_eq!(sink.types(), vec!["subagent_lifecycle".to_string(), "subagent_progress".to_string()]);

		registry.handle_lifecycle(&json!({ "id": "s1", "status": "completed" }), &sink);
		assert!(registry.snapshots().is_empty());
	}

	#[test]
	fn progress_without_lifecycle_is_ignored_and_events_respect_level() {
		let registry = SubagentRegistry::new();
		let sink = CollectSink::default();
		registry.handle_progress(
			&json!({ "progress": { "id": "ghost", "status": "running" } }),
			&sink,
		);
		assert!(registry.snapshots().is_empty());
		assert!(sink.types().is_empty());

		registry.handle_event(&json!({ "id": "s1", "event": "x" }), &sink);
		assert!(sink.types().is_empty());
		registry.set_subscription_level(SubagentSubscriptionLevel::Events);
		registry.handle_event(&json!({ "id": "s1", "event": "x" }), &sink);
		assert_eq!(sink.types(), vec!["subagent_event".to_string()]);
	}

	#[test]
	fn transcript_reads_only_complete_lines_and_resets() {
		let path = tempfile("transcript.jsonl");
		let _ = std::fs::remove_file(&path);
		let first = json!({ "type": "message", "message": { "role": "user", "content": "hi" } });
		std::fs::write(&path, format!("{first}\n{{\"type\":\"message\"")).expect("write");

		let slice = read_transcript(path.to_str().expect("path"), 0).expect("read");
		assert_eq!(slice.entries.len(), 1);
		assert_eq!(slice.messages.len(), 1);
		assert_eq!(slice.messages[0]["role"], json!("user"));
		assert_eq!(slice.next_byte, first.to_string().len() + 1);
		assert!(!slice.reset);

		std::fs::write(&path, format!("{first}\n{{\"type\":\"message\"}}\n")).expect("rewrite");
		let slice = read_transcript(path.to_str().expect("path"), slice.next_byte as i64).expect("read");
		assert_eq!(slice.entries.len(), 1);
		assert_eq!(slice.entries[0]["type"], json!("message"));

		let slice = read_transcript(path.to_str().expect("path"), 10_000).expect("reset");
		assert!(slice.reset);
		assert_eq!(slice.from_byte, 0);
		assert_eq!(slice.entries.len(), 2);

		let missing = read_transcript("/nonexistent/omp-rpc-subagent.jsonl", 5).expect("missing");
		assert_eq!(missing.from_byte, 5);
		assert_eq!(missing.next_byte, 5);
		assert!(missing.entries.is_empty());
	}

	#[test]
	fn session_file_resolution_matches_upstream_errors() {
		let registry = SubagentRegistry::new();
		assert_eq!(
			registry.resolve_session_file(None, None).expect_err("selector"),
			"get_subagent_messages requires subagentId or sessionFile"
		);
		assert_eq!(
			registry.resolve_session_file(Some("s1"), None).expect_err("unknown"),
			"Unknown subagent or session file unavailable: s1"
		);
		assert_eq!(
			registry.resolve_session_file(None, Some("/tmp/x.jsonl")).expect_err("unknown file"),
			"Unknown subagent session file"
		);
		let sink = CollectSink::default();
		registry.handle_lifecycle(
			&json!({ "id": "s1", "status": "started", "sessionFile": "/tmp/x.jsonl" }),
			&sink,
		);
		assert_eq!(
			registry.resolve_session_file(Some("s1"), None).expect("known"),
			"/tmp/x.jsonl"
		);
		assert_eq!(
			registry
				.resolve_session_file(None, Some("/tmp/x.jsonl"))
				.expect("known file"),
			"/tmp/x.jsonl"
		);
	}

	fn started_event(id: &str, session_file: Option<&str>, detached: bool) -> SubagentEvent {
		SubagentEvent::Lifecycle(pi_agent::supervisor::SubagentLifecycle {
			id:                  id.to_string(),
			index:               1,
			agent:               "scout".to_string(),
			agent_source:        "builtin".to_string(),
			task:                "look".to_string(),
			status:              pi_agent::supervisor::SubagentLifecycleStatus::Started,
			session_file:        session_file.map(str::to_string),
			parent_tool_call_id: None,
			detached,
			error:               None,
			abort_reason:        None,
			duration_ms:         None,
		})
	}

	/// The supervisor observer bridge turns real native events into the frames
	/// the transport already speaks: started, one progress, one terminal.
	#[test]
	fn observer_bridge_emits_lifecycle_progress_and_terminal() {
		let registry = Arc::new(SubagentRegistry::new());
		registry.set_subscription_level(SubagentSubscriptionLevel::Progress);
		let sink = Arc::new(CollectSink::default());
		let observer = registry.observer(Arc::clone(&sink) as Arc<dyn FrameSink>);

		observer.observe(&started_event("s1", Some("/tmp/s1.jsonl"), true));
		assert_eq!(registry.snapshots().len(), 1);
		assert_eq!(registry.snapshots()[0].status, "running");

		observer.observe(&SubagentEvent::Progress(pi_agent::supervisor::SubagentProgress {
			id:                  "s1".to_string(),
			index:               1,
			agent:               "scout".to_string(),
			agent_source:        "builtin".to_string(),
			task:                "look".to_string(),
			parent_tool_call_id: None,
			session_file:        Some("/tmp/s1.jsonl".to_string()),
			detached:            true,
			status:              "running".to_string(),
			description:         Some("scanning".to_string()),
		}));
		observer.observe(&SubagentEvent::Lifecycle(pi_agent::supervisor::SubagentLifecycle {
			id:                  "s1".to_string(),
			index:               1,
			agent:               "scout".to_string(),
			agent_source:        "builtin".to_string(),
			task:                "look".to_string(),
			status:              pi_agent::supervisor::SubagentLifecycleStatus::Completed,
			session_file:        Some("/tmp/s1.jsonl".to_string()),
			parent_tool_call_id: None,
			detached:            true,
			error:               None,
			abort_reason:        None,
			duration_ms:         Some(4),
		}));

		assert_eq!(
			sink.types(),
			vec![
				"subagent_lifecycle".to_string(),
				"subagent_progress".to_string(),
				"subagent_lifecycle".to_string(),
			]
		);
		assert!(registry.snapshots().is_empty(), "terminal drops the live entry");
		assert_eq!(
			registry.resolve_session_file(Some("s1"), None).expect("retained"),
			"/tmp/s1.jsonl"
		);
	}

	/// Real child-loop events reach the sink only at `events` level, and the
	/// payload keeps the native event shape (never a re-synthesized summary).
	#[test]
	fn observer_bridge_emits_real_agent_events_only_at_events_level() {
		use pi_agent::AgentEvent;

		let registry = Arc::new(SubagentRegistry::new());
		let sink = Arc::new(CollectSink::default());
		let observer = registry.observer(Arc::clone(&sink) as Arc<dyn FrameSink>);
		let event = SubagentEvent::Event {
			id:    "task-1".to_string(),
			event: AgentEvent::ToolStart {
				id:        "call-9".to_string(),
				name:      "grep".to_string(),
				arguments: json!({ "pattern": "x" }),
			},
		};

		registry.set_subscription_level(SubagentSubscriptionLevel::Progress);
		observer.observe(&event);
		assert!(sink.types().is_empty(), "progress level drops raw events");

		registry.set_subscription_level(SubagentSubscriptionLevel::Events);
		observer.observe(&event);
		assert_eq!(sink.types(), vec!["subagent_event".to_string()]);
		let frames = sink.frames.lock().expect("frames").clone();
		assert_eq!(frames[0]["payload"]["id"], json!("task-1"));
		assert_eq!(frames[0]["payload"]["event"]["type"], json!("tool_execution_start"));
		assert_eq!(frames[0]["payload"]["event"]["toolCallId"], json!("call-9"));
		assert_eq!(frames[0]["payload"]["event"]["toolName"], json!("grep"));
	}

	#[test]
	fn agent_event_frames_match_the_session_event_names() {
		use pi_agent::{ContentPart, Message, TurnStatus};

		let message = Message::assistant("hi");
		let tool_result = pi_agent::ToolResult {
			tool_call_id: "call-1".to_string(),
			content:      "ok".to_string(),
			is_error:     false,
			terminate:    false,
			details:      Some(json!({
				// Model-facing parts the loop validates; these must reach
				// `content` as real image blocks.
				"images": [{ "data": "aGk=", "mimeType": "image/png" }],
				// Host audit payload: forwarded verbatim, never read as parts.
				"content": [{ "type": "image", "data": "aGk=", "mimeType": "image/png" }],
			})),
		};
		let rejected = pi_agent::ToolResult {
			tool_call_id: "call-2".to_string(),
			content:      "bad".to_string(),
			is_error:     false,
			terminate:    false,
			// Missing `mimeType`: the shared validator rejects the payload.
			details:      Some(json!({ "images": [{ "data": "aGk=" }] })),
		};
		let update = single(&AgentEvent::MessageUpdate { delta: "he".to_string() });
		assert_eq!(update["type"], json!("message_update"));
		// TS `message_update` carries the message snapshot *and* the streamed
		// event; a client must not have to accumulate deltas itself.
		assert_eq!(update["message"]["role"], json!("assistant"));
		assert_eq!(update["message"]["content"][0]["text"], json!("he"));
		assert_eq!(
			update["assistantMessageEvent"],
			json!({ "type": "text_delta", "delta": "he" })
		);
		assert_eq!(
			single(&AgentEvent::MessageEnd { message: message.clone(), status: TurnStatus::EndTurn })["type"],
			json!("message_end")
		);
		assert_eq!(
			single(&AgentEvent::ToolStart {
				id:        "call-1".to_string(),
				name:      "read".to_string(),
				arguments: json!({}),
			}),
			json!({
				"type": "tool_execution_start",
				"toolCallId": "call-1",
				"toolName": "read",
				"args": {},
			})
		);
		assert_eq!(
			single(&AgentEvent::ToolUpdate {
				id:      "call-1".to_string(),
				name:    "bash".to_string(),
				args:    json!({ "command": "ls" }),
				partial: json!({ "output": "a" }),
			}),
			json!({
				"type": "tool_execution_update",
				"toolCallId": "call-1",
				"toolName": "bash",
				"args": { "command": "ls" },
				"partialResult": { "output": "a" },
			})
		);
		let end = single(&AgentEvent::ToolEnd {
			id:       "call-1".to_string(),
			name:     "read".to_string(),
			result:   "body".to_string(),
			is_error: true,
			images:   vec![ContentPart::Image {
				data:      "aGk=".to_string(),
				mime_type: "image/png".to_string(),
			}],
			details:  Some(json!({ "bytes": 4 })),
		});
		assert_eq!(end["type"], json!("tool_execution_end"));
		assert_eq!(end["result"]["content"][0]["text"], json!("body"));
		// Images the model sees are on the wire, not dropped.
		assert_eq!(end["result"]["content"][1]["type"], json!("image"));
		assert_eq!(end["result"]["details"]["bytes"], json!(4));
		assert_eq!(end["isError"], json!(true));

		let turn_message = Message::assistant_with_tools(
			"hi",
			vec![pi_agent::ToolCall {
				id:        "call-1".to_string(),
				name:      "read".to_string(),
				arguments: json!({}),
			}],
		);
		let turn_end = single(&AgentEvent::TurnEnd {
			message:      turn_message,
			tool_results: vec![tool_result, rejected],
			status:       TurnStatus::ToolUse,
		});
		assert_eq!(turn_end["type"], json!("turn_end"));
		assert_eq!(turn_end["message"]["role"], json!("assistant"));
		// The turn status is not hardcoded: a tool-use turn says `toolUse`.
		assert_eq!(turn_end["message"]["stopReason"], json!("toolUse"));
		// TS `turn_end.toolResults` is a `ToolResultMessage`, not a
		// `tool_execution_end` frame.
		let result = &turn_end["toolResults"][0];
		assert_eq!(result["role"], json!("toolResult"));
		assert_eq!(result["toolCallId"], json!("call-1"));
		assert_eq!(result["toolName"], json!("read"));
		assert_eq!(result["isError"], json!(false));
		assert!(result["timestamp"].as_u64().is_some());
		assert_eq!(result["content"][0]["type"], json!("text"));
		assert_eq!(result["content"][0]["text"], json!("ok"));
		// Real image block in `content`, from the validated `details.images`.
		assert_eq!(result["content"][1]["type"], json!("image"));
		assert_eq!(result["content"][1]["data"], json!("aGk="));
		assert_eq!(result["content"][1]["mimeType"], json!("image/png"));
		// Audit payload survives in `details`, untouched.
		assert_eq!(result["details"]["content"][0]["type"], json!("image"));
		assert_eq!(result["details"]["content"][0]["mimeType"], json!("image/png"));
		// A rejected image payload is an explicit failure, not silent success.
		let rejected = &turn_end["toolResults"][1];
		assert_eq!(rejected["isError"], json!(true));
		let blocks = rejected["content"].as_array().expect("blocks");
		assert_eq!(blocks.len(), 1);
		assert!(
			blocks[0]["text"].as_str().unwrap_or_default().contains("[image payload rejected:"),
			"{}",
			blocks[0]["text"]
		);

		let steer = {
			let mut text = String::new();
			agent_event_frames(
				&AgentEvent::UserSteer {
					parts: vec![
						ContentPart::Text { text: "go".to_string() },
						ContentPart::Image {
							data:      "aGk=".to_string(),
							mime_type: "image/png".to_string(),
						},
					],
				},
				&mut text,
			)
		};
		assert_eq!(steer.len(), 2);
		assert_eq!(steer[0]["type"], json!("message_start"));
		assert_eq!(steer[1]["type"], json!("message_end"));
		// TS `emitInputMessages`: both frames carry the same full message.
		assert_eq!(steer[0]["message"], steer[1]["message"]);
		assert_eq!(steer[1]["message"]["role"], json!("user"));
		assert_eq!(steer[1]["message"]["content"][0]["text"], json!("go"));
		assert_eq!(steer[1]["message"]["content"][1]["type"], json!("image"));
		assert_eq!(
			steer[1]["message"]["content"][1]["mimeType"],
			json!("image/png")
		);
	}

	/// `clear()` drops live entries and marks their ids stale, so events from a
	/// previous session are ignored instead of resurrecting snapshots.
	#[test]
	fn clear_ignores_late_events_from_the_previous_session() {
		let registry = SubagentRegistry::new();
		let sink = CollectSink::default();
		registry.set_subscription_level(SubagentSubscriptionLevel::Progress);
		registry.handle_lifecycle(
			&json!({ "id": "s1", "status": "started", "sessionFile": "/tmp/s1.jsonl" }),
			&sink,
		);
		assert_eq!(registry.snapshots().len(), 1);
		let before = sink.types().len();

		registry.clear();
		assert!(registry.snapshots().is_empty());
		assert_eq!(
			registry.resolve_session_file(Some("s1"), None).expect("retained transcript"),
			"/tmp/s1.jsonl"
		);

		registry.handle_progress(
			&json!({ "progress": { "id": "s1", "status": "running" } }),
			&sink,
		);
		registry.handle_lifecycle(&json!({ "id": "s1", "status": "completed" }), &sink);
		assert!(registry.snapshots().is_empty());
		assert_eq!(sink.types().len(), before, "stale ids emit nothing");
	}
}
