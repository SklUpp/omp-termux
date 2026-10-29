//! Subagent observability for RPC: snapshots, event frames and transcript paging.
//!
//! Mirrors `packages/coding-agent/src/modes/rpc/rpc-subagents.ts`. Payloads are
//! the native task-observer shapes (`id`, `index`, `agent`, `status`, ...), so
//! the registry works unchanged once the native supervisor publishes them.
//! This build has no observability event bus yet — see [`event_bus_prerequisite`].

use std::{
	collections::{BTreeMap, BTreeSet},
	io::{Read, Seek, SeekFrom},
	path::Path,
	sync::{
		Arc, Mutex, MutexGuard,
	},
};

use serde_json::{Value, json};

use super::{
	session::{FrameSink, Prerequisite},
	types::SubagentSubscriptionLevel,
};

const MAX_RETAINED_TRANSCRIPT_REFERENCES: usize = 256;

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
}

impl SubagentRegistry {
	#[must_use]
	pub fn new() -> Self {
		Self::default()
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
			task: existing.as_ref().and_then(|s| s.task.clone()),
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

/// The native seam subagent frames still need: an observability event bus.
#[must_use]
pub fn event_bus_prerequisite() -> Prerequisite {
	Prerequisite::new(
		"subagent_events_unavailable",
		"subagent observability is unavailable: the native supervisor publishes no lifecycle/progress \
		 event bus and does not expose child records to the session owner (native seam: \
		 task-observer event bus + ChildRecord.session_file; RPC then feeds SubagentRegistry)",
	)
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
}
