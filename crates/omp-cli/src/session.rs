//! Append-only JSONL session log: one file per session, readable back for
//! `--continue`.
//!
//! Line shapes (the same format is written and read):
//! ```json
//! {"type":"header","version":1,"id":"…","timestamp":"…","cwd":"…","model":null,"provider":null}
//! {"type":"turn","prompt":"…","assistant":"…","stop_reason":"end_turn","tool_calls":[{"id":"…","name":"read","arguments":{},"result":"…","is_error":false}]}
//! ```
//! The only clock read is the header timestamp, so a log round-trips byte-stable
//! apart from it.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::agent::Event;
use crate::{dirs, output};

/// Directory under the agent dir that holds CLI session logs.
pub const SESSIONS_SUBDIR: &str = "sessions/omp-cli";
const LOG_VERSION: u32 = 1;
const LOG_EXTENSION: &str = "jsonl";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionHeader {
	pub version: u32,
	pub id: String,
	pub timestamp: String,
	pub cwd: String,
	#[serde(default)]
	pub model: Option<String>,
	#[serde(default)]
	pub provider: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallRecord {
	pub id: String,
	pub name: String,
	pub arguments: serde_json::Value,
	pub result: String,
	pub is_error: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TurnRecord {
	pub prompt: String,
	#[serde(default)]
	pub assistant: String,
	#[serde(default)]
	pub stop_reason: String,
	#[serde(default)]
	pub tool_calls: Vec<ToolCallRecord>,
}

/// Builds a [`TurnRecord`] from the runner's event stream.
#[derive(Debug, Default)]
pub struct TurnBuilder {
	record: TurnRecord,
	pending: BTreeMap<String, usize>,
}

impl TurnBuilder {
	#[must_use]
	pub fn new(prompt: impl Into<String>) -> Self {
		Self {
			record: TurnRecord { prompt: prompt.into(), ..TurnRecord::default() },
			pending: BTreeMap::new(),
		}
	}

	pub fn observe(&mut self, event: &Event) {
		match event {
			Event::MessageEnd { text } | Event::AgentEnd { text } => {
				self.record.assistant = text.clone();
			},
			Event::TurnEnd { stop_reason } => self.record.stop_reason = stop_reason.clone(),
			Event::ToolStart { id, tool, args } => {
				self
					.pending
					.insert(id.clone(), self.record.tool_calls.len());
				self.record.tool_calls.push(ToolCallRecord {
					id: id.clone(),
					name: tool.clone(),
					arguments: args.clone(),
					result: String::new(),
					is_error: false,
				});
			},
			Event::ToolEnd { id, result, error, .. } => {
				if let Some(index) = self.pending.remove(id)
					&& let Some(call) = self.record.tool_calls.get_mut(index)
				{
					call.result = result.clone();
					call.is_error = error.is_some();
				}
			},
			_ => {},
		}
	}

	#[must_use]
	pub fn finish(self) -> TurnRecord {
		self.record
	}
}

/// Where and how to persist a session.
#[derive(Debug, Clone)]
pub struct SessionOptions {
	/// `false` for `--no-session`: nothing is written or read.
	pub enabled: bool,
	pub cwd: PathBuf,
	pub model: Option<String>,
	pub provider: Option<String>,
	/// `--session-dir`; defaults to `~/.omp/agent/sessions/omp-cli`.
	pub dir: Option<PathBuf>,
	/// `--session-id`.
	pub id: Option<String>,
	/// `--continue`: resume the newest log in `dir` (by mtime).
	pub continue_latest: bool,
}

impl SessionOptions {
	#[must_use]
	pub fn default_dir() -> PathBuf {
		dirs::agent_dir().join(SESSIONS_SUBDIR)
	}
}

/// Session log handle. Failures are collected as warnings so a broken log never
/// changes the answer or the exit code.
#[derive(Debug, Default)]
pub struct SessionStore {
	enabled: bool,
	id: String,
	file: Option<PathBuf>,
	header: Option<SessionHeader>,
	history: Vec<TurnRecord>,
	warnings: Vec<String>,
}

impl SessionStore {
	/// Open (or resume) a session. Never fails: IO problems disable persistence
	/// and surface through [`take_warnings`](Self::take_warnings).
	pub fn open(options: &SessionOptions) -> Self {
		if !options.enabled {
			return Self::default();
		}
		let dir = options
			.dir
			.clone()
			.unwrap_or_else(SessionOptions::default_dir);
		let mut store = Self { enabled: false, ..Self::default() };
		if let Err(err) = fs::create_dir_all(&dir) {
			store.warn(format!("cannot create session directory {}: {err}", dir.display()));
			return store;
		}

		let mut id = options.id.clone();
		if options.continue_latest {
			match latest_session(&dir) {
				Ok(Some((latest, _))) => id = Some(latest),
				Ok(None) => {
					store.warn(format!("no previous session in {}; starting a new one", dir.display()))
				},
				Err(err) => store.warn(err),
			}
		}
		let id = id.unwrap_or_else(output::session_id);
		if id.contains(['/', '\\']) || id.is_empty() {
			store.warn(format!("invalid session id `{id}`; session persistence disabled"));
			return store;
		}
		let file = dir.join(format!("{id}.{LOG_EXTENSION}"));

		if file.is_file() {
			let existing = fs::read_to_string(&file)
				.map_err(|err| format!("cannot read session log {}: {err}", file.display()));
			match existing.and_then(|text| parse_log(&file, &text)) {
				Ok((header, history)) => {
					store.id = id;
					store.header = Some(header);
					store.history = history;
					store.file = Some(file);
					store.enabled = true;
					return store;
				},
				Err(err) => {
					// Never append to a log we cannot parse: that would corrupt it.
					store.warn(format!("{err}; session persistence disabled"));
					return store;
				},
			}
		}

		let header = SessionHeader {
			version: LOG_VERSION,
			id: id.clone(),
			timestamp: output::iso8601(SystemTime::now()),
			cwd: options.cwd.display().to_string(),
			model: options.model.clone(),
			provider: options.provider.clone(),
		};
		if let Err(err) =
			append_line(&file, &HeaderLine { kind: "header".to_string(), header: header.clone() })
		{
			store.warn(format!("{err}; session persistence disabled"));
			return store;
		}
		store.id = id;
		store.header = Some(header);
		store.file = Some(file);
		store.enabled = true;
		store
	}

	#[must_use]
	pub fn enabled(&self) -> bool {
		self.enabled
	}

	#[must_use]
	pub fn id(&self) -> &str {
		&self.id
	}

	#[must_use]
	pub fn file(&self) -> Option<&Path> {
		self.file.as_deref()
	}

	#[must_use]
	pub fn header(&self) -> Option<&SessionHeader> {
		self.header.as_ref()
	}

	#[must_use]
	pub fn history(&self) -> &[TurnRecord] {
		&self.history
	}

	pub fn take_warnings(&mut self) -> Vec<String> {
		std::mem::take(&mut self.warnings)
	}

	/// Append one finished turn. The in-memory history grows even when the disk
	/// write fails, so a single session stays coherent.
	pub fn append_turn(&mut self, turn: &TurnRecord) -> Result<(), String> {
		if !self.enabled {
			return Ok(());
		}
		self.history.push(turn.clone());
		let Some(file) = self.file.clone() else {
			return Ok(());
		};
		append_line(&file, &TurnLine { kind: "turn".to_string(), turn: turn.clone() })
	}

	fn warn(&mut self, message: String) {
		self.warnings.push(message);
	}
}

/// Newest `*.jsonl` in `dir`, by mtime (ties broken by path).
fn latest_session(dir: &Path) -> Result<Option<(String, PathBuf)>, String> {
	let entries = fs::read_dir(dir)
		.map_err(|err| format!("cannot read session directory {}: {err}", dir.display()))?;
	let mut newest: Option<(SystemTime, String, PathBuf)> = None;
	for entry in entries {
		let entry = entry.map_err(|err| format!("cannot read session directory entry: {err}"))?;
		let path = entry.path();
		if path.extension().and_then(|extension| extension.to_str()) != Some(LOG_EXTENSION) {
			continue;
		}
		let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
			continue;
		};
		let modified = entry
			.metadata()
			.and_then(|metadata| metadata.modified())
			.unwrap_or(SystemTime::UNIX_EPOCH);
		let replace = match &newest {
			None => true,
			Some((time, _, _)) => modified > *time,
		};
		if replace {
			newest = Some((modified, stem.to_string(), path));
		}
	}
	Ok(newest.map(|(_, id, path)| (id, path)))
}

fn parse_log(file: &Path, text: &str) -> Result<(SessionHeader, Vec<TurnRecord>), String> {
	let mut header = None;
	let mut turns = Vec::new();
	for (index, line) in text.lines().enumerate() {
		if line.trim().is_empty() {
			continue;
		}
		let value: serde_json::Value = serde_json::from_str(line).map_err(|err| {
			format!("invalid session log {} line {}: {err}", file.display(), index + 1)
		})?;
		let kind = value
			.get("type")
			.and_then(serde_json::Value::as_str)
			.unwrap_or_default()
			.to_string();
		let invalid = |err: serde_json::Error| {
			format!("invalid session log {} line {}: {err}", file.display(), index + 1)
		};
		match kind.as_str() {
			"header" => {
				let parsed: HeaderLine = serde_json::from_value(value).map_err(invalid)?;
				header = Some(parsed.header);
			},
			"turn" => {
				let parsed: TurnLine = serde_json::from_value(value).map_err(invalid)?;
				turns.push(parsed.turn);
			},
			other => {
				return Err(format!(
					"unknown session record `{other}` in {} line {}",
					file.display(),
					index + 1
				));
			},
		}
	}
	let header = header.ok_or_else(|| format!("session log {} has no header", file.display()))?;
	Ok((header, turns))
}

fn append_line(file: &Path, line: &impl Serialize) -> Result<(), String> {
	let encoded =
		serde_json::to_string(line).map_err(|err| format!("cannot encode session record: {err}"))?;
	let mut handle = OpenOptions::new()
		.create(true)
		.append(true)
		.open(file)
		.map_err(|err| format!("cannot open session log {}: {err}", file.display()))?;
	handle
		.write_all(format!("{encoded}\n").as_bytes())
		.map_err(|err| format!("cannot write session log {}: {err}", file.display()))
}

#[derive(Debug, Serialize, Deserialize)]
struct HeaderLine {
	#[serde(rename = "type")]
	kind: String,
	#[serde(flatten)]
	header: SessionHeader,
}

#[derive(Debug, Serialize, Deserialize)]
struct TurnLine {
	#[serde(rename = "type")]
	kind: String,
	#[serde(flatten)]
	turn: TurnRecord,
}

#[cfg(test)]
mod tests {
	use std::time::{Duration, UNIX_EPOCH};

	use serde_json::json;

	use super::*;

	static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

	fn temp_dir() -> PathBuf {
		let unique = format!(
			"omp-cli-session-{}-{}",
			std::process::id(),
			COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
		);
		let dir = std::env::temp_dir().join(unique);
		fs::create_dir_all(&dir).expect("temp dir");
		dir
	}

	fn options(dir: PathBuf, id: Option<&str>) -> SessionOptions {
		SessionOptions {
			enabled: true,
			cwd: PathBuf::from("/work"),
			model: Some("local/llama3".to_string()),
			provider: Some("local".to_string()),
			dir: Some(dir),
			id: id.map(str::to_string),
			continue_latest: false,
		}
	}

	fn turn(prompt: &str, assistant: &str) -> TurnRecord {
		TurnRecord {
			prompt: prompt.to_string(),
			assistant: assistant.to_string(),
			stop_reason: "end_turn".to_string(),
			tool_calls: vec![ToolCallRecord {
				id: "call-1".to_string(),
				name: "read".to_string(),
				arguments: json!({"path": "a.txt"}),
				result: "contents".to_string(),
				is_error: false,
			}],
		}
	}

	#[test]
	fn jsonl_round_trip_preserves_header_and_turns() {
		let dir = temp_dir();
		let first = turn("fix", "done");
		{
			let mut store = SessionStore::open(&options(dir.clone(), Some("s1")));
			assert!(store.enabled());
			assert_eq!(store.header().expect("header").id, "s1");
			store.append_turn(&first).expect("append");
			assert!(store.take_warnings().is_empty());
		}
		let mut reopened = SessionStore::open(&options(dir.clone(), Some("s1")));
		assert!(reopened.enabled());
		assert_eq!(reopened.history().to_vec(), vec![first.clone()]);
		let second = TurnRecord {
			prompt: "again".to_string(),
			assistant: "ok".to_string(),
			stop_reason: "end_turn".to_string(),
			tool_calls: Vec::new(),
		};
		reopened.append_turn(&second).expect("append");
		drop(reopened);

		let resumed = SessionStore::open(&options(dir.clone(), Some("s1")));
		assert_eq!(resumed.history().to_vec(), vec![first, second]);
		assert_eq!(resumed.header().expect("header").cwd, "/work");
		fs::remove_dir_all(dir).ok();
	}

	#[test]
	fn continue_picks_the_newest_log_by_mtime() {
		let dir = temp_dir();
		let mut older = SessionStore::open(&options(dir.clone(), Some("older")));
		older
			.append_turn(&turn("old", "old answer"))
			.expect("append");
		drop(older);
		let mut newer = SessionStore::open(&options(dir.clone(), Some("newer")));
		newer
			.append_turn(&turn("new", "new answer"))
			.expect("append");
		drop(newer);

		let older_path = dir.join("older.jsonl");
		let newer_path = dir.join("newer.jsonl");
		let base = UNIX_EPOCH + Duration::from_secs(1_000);
		fs::File::options()
			.write(true)
			.open(&older_path)
			.expect("open older")
			.set_modified(base)
			.expect("mtime older");
		fs::File::options()
			.write(true)
			.open(&newer_path)
			.expect("open newer")
			.set_modified(base + Duration::from_secs(10))
			.expect("mtime newer");

		let mut resumed = SessionStore::open(&SessionOptions {
			continue_latest: true,
			..options(dir.clone(), None)
		});
		assert!(resumed.enabled());
		assert_eq!(resumed.id(), "newer");
		assert_eq!(resumed.history().to_vec(), vec![turn("new", "new answer")]);
		assert!(resumed.take_warnings().is_empty());
		fs::remove_dir_all(dir).ok();
	}

	#[test]
	fn no_session_writes_nothing() {
		let dir = temp_dir().join("never-created");
		let mut store =
			SessionStore::open(&SessionOptions { enabled: false, ..options(dir.clone(), Some("s1")) });
		assert!(!store.enabled());
		store
			.append_turn(&turn("hi", "there"))
			.expect("append is a no-op");
		assert!(!dir.exists());
		fs::remove_dir_all(dir.parent().expect("parent")).ok();
	}

	#[test]
	fn unreadable_directory_is_a_warning_not_a_failure() {
		let dir = temp_dir();
		let blocker = dir.join("blocker");
		fs::write(&blocker, "not a directory").expect("write blocker");
		let mut store = SessionStore::open(&SessionOptions {
			dir: Some(blocker.clone()),
			..options(dir.clone(), Some("s1"))
		});
		assert!(!store.enabled());
		assert!(!store.take_warnings().is_empty());
		store
			.append_turn(&turn("hi", "there"))
			.expect("append is a no-op");
		fs::remove_dir_all(dir).ok();
	}

	#[test]
	fn builder_captures_tools_and_final_text() {
		let mut builder = TurnBuilder::new("fix the build");
		builder.observe(&Event::MessageStart { role: "assistant".to_string() });
		builder.observe(&Event::ToolStart {
			id: "call-1".to_string(),
			tool: "read".to_string(),
			args: json!({"path": "a.txt"}),
		});
		builder.observe(&Event::ToolEnd {
			id: "call-1".to_string(),
			tool: "read".to_string(),
			result: "contents".to_string(),
			error: None,
		});
		builder.observe(&Event::MessageEnd { text: "done".to_string() });
		builder.observe(&Event::TurnEnd { stop_reason: "end_turn".to_string() });
		builder.observe(&Event::AgentEnd { text: "done".to_string() });
		let record = builder.finish();
		assert_eq!(record.prompt, "fix the build");
		assert_eq!(record.assistant, "done");
		assert_eq!(record.stop_reason, "end_turn");
		assert_eq!(record.tool_calls.len(), 1);
		assert_eq!(record.tool_calls[0].result, "contents");
		assert!(!record.tool_calls[0].is_error);
	}
}
