//! Foreign session import readers: discovery and conversion of Claude Code and
//! OpenAI Codex session transcripts into independent session structures for OMP.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::{Error, Result, dirs};

/// External coding-agent session source supported by OMP imports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForeignSessionSource {
	Claude,
	Codex,
}

impl ForeignSessionSource {
	#[must_use]
	pub const fn name(&self) -> &'static str {
		match self {
			Self::Claude => "Claude",
			Self::Codex => "Codex",
		}
	}
}

impl std::fmt::Display for ForeignSessionSource {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(self.name())
	}
}

impl std::str::FromStr for ForeignSessionSource {
	type Err = Error;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		match s.to_ascii_lowercase().as_str() {
			"claude" => Ok(Self::Claude),
			"codex" => Ok(Self::Codex),
			_ => Err(Error::new(format!("Unknown foreign session source: {s}"))),
		}
	}
}

/// Lightweight source metadata used to choose a foreign session before loading its transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForeignSessionInfo {
	pub source: ForeignSessionSource,
	pub id: String,
	pub path: PathBuf,
	pub cwd: PathBuf,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub title: Option<String>,
	pub created_ms: u64,
	pub modified_ms: u64,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub message_count: Option<usize>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub first_message: Option<String>,
}

impl ForeignSessionInfo {
	#[must_use]
	pub fn created_iso(&self) -> String {
		format_iso_timestamp(self.created_ms)
	}

	#[must_use]
	pub fn modified_iso(&self) -> String {
		format_iso_timestamp(self.modified_ms)
	}
}

/// A loaded foreign session containing transformed entries and provenance metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoadedForeignSession {
	pub source: ForeignSessionSource,
	pub id: String,
	pub cwd: PathBuf,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub title: Option<String>,
	pub entries: Vec<Value>,
}

impl LoadedForeignSession {
	/// Returns every message object contained in session `message` entries.
	#[must_use]
	pub fn messages(&self) -> Vec<Value> {
		self
			.entries
			.iter()
			.filter(|e| e.get("type").and_then(Value::as_str) == Some("message"))
			.filter_map(|e| e.get("message").cloned())
			.collect()
	}

	/// Returns the role of each message in the session transcript.
	#[must_use]
	pub fn message_roles(&self) -> Vec<String> {
		self
			.messages()
			.iter()
			.filter_map(|m| m.get("role").and_then(Value::as_str).map(String::from))
			.collect()
	}

	/// Creates the standard `foreign_session_import` breadcrumb entry matching TypeScript OMP.
	#[must_use]
	pub fn provenance_entry(&self, source_info: &ForeignSessionInfo) -> Value {
		let timestamp = if source_info.created_ms > 0 {
			source_info.created_iso()
		} else if let Some(first_ts) = self
			.entries
			.first()
			.and_then(|e| e.get("timestamp"))
			.and_then(Value::as_str)
		{
			first_ts.to_string()
		} else {
			format_iso_timestamp(
				std::time::SystemTime::now()
					.duration_since(std::time::UNIX_EPOCH)
					.map(|d| d.as_millis() as u64)
					.unwrap_or(0),
			)
		};
		let id = format!("import-{}", source_info.id);
		json!({
			"type": "custom",
			"id": id,
			"parentId": null,
			"timestamp": timestamp,
			"customType": "foreign_session_import",
			"data": {
				"source": source_info.source.to_string().to_ascii_lowercase(),
				"sourceId": source_info.id,
				"sourcePath": source_info.path.display().to_string(),
				"sourceCwd": source_info.cwd.display().to_string(),
			}
		})
	}
}

/// Lists and converts sessions owned by another coding agent.
pub trait ForeignSessionStore {
	fn source(&self) -> ForeignSessionSource;
	fn list(&self) -> Result<Vec<ForeignSessionInfo>>;
	fn load(&self, info: &ForeignSessionInfo) -> Result<LoadedForeignSession>;
}

/// Creates a store for a foreign session source.
#[must_use]
pub fn create_foreign_session_store(
	source: ForeignSessionSource,
	custom_root: Option<&Path>,
) -> Box<dyn ForeignSessionStore> {
	match source {
		ForeignSessionSource::Claude => {
			if let Some(root) = custom_root {
				Box::new(ClaudeSessionStore::new(root))
			} else {
				Box::new(ClaudeSessionStore::default())
			}
		},
		ForeignSessionSource::Codex => {
			if let Some(root) = custom_root {
				Box::new(CodexSessionStore::new(root))
			} else {
				Box::new(CodexSessionStore::default())
			}
		},
	}
}

// ─────────────────────────────────────────────────────────────────────────────
// Claude Session Store
// ─────────────────────────────────────────────────────────────────────────────

const CLAUDE_CWD_PREFIX_BYTES: u64 = 64 * 1024;

/// Claude Code encodes project cwds by replacing non-alphanumeric chars with `-`.
#[must_use]
pub fn encode_project_dir(project: &str) -> String {
	project
		.chars()
		.map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
		.collect()
}

#[derive(Debug, Clone)]
pub struct ClaudeSessionStore {
	root: PathBuf,
}

impl Default for ClaudeSessionStore {
	fn default() -> Self {
		let (config_dir, _) = resolve_claude_paths(None);
		Self { root: config_dir }
	}
}

impl ClaudeSessionStore {
	#[must_use]
	pub fn new(root: impl Into<PathBuf>) -> Self {
		Self { root: root.into() }
	}

	#[must_use]
	pub fn root(&self) -> &Path {
		&self.root
	}

	pub fn list(&self) -> Result<Vec<ForeignSessionInfo>> {
		let history = read_claude_history_index(&self.root.join("history.jsonl"));
		let files = find_claude_project_files(&self.root);
		let mut sessions = Vec::with_capacity(files.len());

		for (file_path, project_cwd) in files {
			let Ok(meta) = fs::metadata(&file_path) else {
				continue;
			};
			let id = file_path
				.file_stem()
				.and_then(|s| s.to_str())
				.unwrap_or("")
				.to_string();
			if id.is_empty() {
				continue;
			}

			let mtime_ms = file_mtime_ms(&meta);
			let ctime_ms = file_birth_or_ctime_ms(&meta, mtime_ms);
			let indexed = history.get(&id);

			let created_ms = indexed.map_or(ctime_ms, |idx| idx.created);
			let modified_ms = indexed.map_or(mtime_ms, |idx| idx.modified).max(mtime_ms);

			let cwd_str = indexed
				.and_then(|idx| idx.cwd.clone())
				.or_else(|| recorded_claude_cwd(&file_path))
				.unwrap_or(project_cwd);
			let cwd = PathBuf::from(cwd_str);

			sessions.push(ForeignSessionInfo {
				source: ForeignSessionSource::Claude,
				id,
				path: file_path,
				cwd,
				title: None,
				created_ms,
				modified_ms,
				message_count: indexed.map(|idx| idx.message_count),
				first_message: indexed.and_then(|idx| idx.first_message.clone()),
			});
		}

		sessions.sort_by(|left, right| {
			right
				.modified_ms
				.cmp(&left.modified_ms)
				.then_with(|| left.path.cmp(&right.path))
		});

		Ok(sessions)
	}

	pub fn load(&self, info: &ForeignSessionInfo) -> Result<LoadedForeignSession> {
		if info.source != ForeignSessionSource::Claude {
			return Err(Error::new(format!(
				"Cannot load {} session with ClaudeSessionStore",
				info.source
			)));
		}

		let meta = fs::metadata(&info.path)
			.map_err(|e| Error::new(format!("Unable to read Claude session {}: {e}", info.id)))?;

		let records = collect_foreign_json_records(&info.path)
			.map_err(|e| Error::new(format!("Unable to read Claude session {}: {e}", info.id)))?;

		if records.is_empty() && meta.len() > 0 {
			return Err(Error::new(format!(
				"Claude session {} contains no readable records",
				info.id
			)));
		}

		let mut source_parents: HashMap<String, Option<String>> = HashMap::new();
		let mut source_cwd: Option<String> = None;
		let mut source_title: Option<String> = None;
		let mut ai_title: Option<String> = None;

		for record in &records {
			if let Some(uuid) = string_field(&record.value, "uuid") {
				let parent_uuid = string_field(&record.value, "parentUuid");
				source_parents.insert(uuid.to_string(), parent_uuid.map(String::from));
			}
			if source_cwd.is_none() {
				if let Some(cwd) = string_field(&record.value, "cwd") {
					source_cwd = Some(cwd.to_string());
				}
			}
			match string_field(&record.value, "type") {
				Some("custom-title") => {
					if let Some(title) = string_field(&record.value, "customTitle") {
						source_title = Some(title.to_string());
					}
				},
				Some("ai-title") => {
					if let Some(title) = string_field(&record.value, "aiTitle") {
						ai_title = Some(title.to_string());
					}
				},
				_ => {},
			}
		}

		let mut entries = Vec::new();
		let mut source_tails: HashMap<String, String> = HashMap::new();
		let mut used_ids: HashSet<String> = HashSet::new();
		let mut tool_names: HashMap<String, String> = HashMap::new();
		let mut last_model: Option<String> = None;
		let mut synthetic: usize = 0;
		let file_mtime = file_mtime_ms(&meta);

		for record in &records {
			let converted = convert_claude_record(&record.value, file_mtime, &mut tool_names);
			if converted.is_empty() {
				continue;
			}

			let source_uuid = string_field(&record.value, "uuid")
				.map_or_else(|| format!("line-{}", record.line), String::from);
			let mut parent_id = resolve_parent_chain(
				string_field(&record.value, "parentUuid"),
				&source_parents,
				&source_tails,
			);
			let timestamp = iso_timestamp_from_val(record.value.get("timestamp"), file_mtime);

			if string_field(&record.value, "type") == Some("assistant")
				&& let Some(msg) = record.value.get("message")
				&& let Some(model) = string_field(msg, "model")
				&& last_model.as_deref() != Some(model)
			{
				let id = unique_entry_id(&format!("claude-{source_uuid}-model"), &mut used_ids);
				entries.push(json!({
					"type": "model_change",
					"id": id,
					"parentId": parent_id,
					"timestamp": timestamp,
					"model": format!("anthropic/{model}")
				}));
				parent_id = Some(id);
				last_model = Some(model.to_string());
			}

			for item in converted {
				synthetic += 1;
				let id = unique_entry_id(
					&format!("claude-{source_uuid}-{}-{synthetic}", item.suffix),
					&mut used_ids,
				);
				entries.push(json!({
					"type": "message",
					"id": id,
					"parentId": parent_id,
					"timestamp": timestamp,
					"message": item.message
				}));
				parent_id = Some(id);
			}

			if let Some(pid) = parent_id {
				source_tails.insert(source_uuid, pid);
			}
		}

		let title = source_title.or(ai_title).or_else(|| info.title.clone());
		let cwd = source_cwd.map_or_else(|| info.cwd.clone(), PathBuf::from);

		Ok(LoadedForeignSession {
			source: ForeignSessionSource::Claude,
			id: info.id.clone(),
			cwd,
			title,
			entries,
		})
	}
}

impl ForeignSessionStore for ClaudeSessionStore {
	fn source(&self) -> ForeignSessionSource {
		ForeignSessionSource::Claude
	}

	fn list(&self) -> Result<Vec<ForeignSessionInfo>> {
		self.list()
	}

	fn load(&self, info: &ForeignSessionInfo) -> Result<LoadedForeignSession> {
		self.load(info)
	}
}

// ─────────────────────────────────────────────────────────────────────────────
// Codex Session Store
// ─────────────────────────────────────────────────────────────────────────────

/// Discovery result for Codex SQLite index database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexSqliteDiscovery {
	NotFound,
	Available { path: PathBuf, version: u32 },
}

#[derive(Debug, Clone)]
pub struct CodexSessionStore {
	root: PathBuf,
}

impl Default for CodexSessionStore {
	fn default() -> Self {
		let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
		Self { root: home.join(".codex") }
	}
}

impl CodexSessionStore {
	#[must_use]
	pub fn new(root: impl Into<PathBuf>) -> Self {
		Self { root: root.into() }
	}

	#[must_use]
	pub fn root(&self) -> &Path {
		&self.root
	}

	/// Inspects Codex root for SQLite state databases.
	#[must_use]
	pub fn sqlite_discovery(&self) -> CodexSqliteDiscovery {
		let Some((db_path, version)) = state_database_path(&self.root) else {
			return CodexSqliteDiscovery::NotFound;
		};
		CodexSqliteDiscovery::Available { path: db_path, version }
	}

	/// Queries Codex state database directly using read-only connection.
	pub fn list_from_sqlite(&self) -> Result<Vec<ForeignSessionInfo>> {
		let (db_path, _) = state_database_path(&self.root).ok_or_else(|| {
			Error::new(format!("No state_*.sqlite database found in {}", self.root.display()))
		})?;
		query_codex_sqlite_threads(&db_path, &self.root)
	}

	/// Lists Codex sessions: prefers SQLite state database when present and non-empty;
	/// falls back to JSONL rollout scan on schema mismatch or missing SQLite.
	pub fn list(&self) -> Result<Vec<ForeignSessionInfo>> {
		if let Some((db_path, _)) = state_database_path(&self.root) {
			if let Ok(sessions) = query_codex_sqlite_threads(&db_path, &self.root) {
				if !sessions.is_empty() {
					return Ok(sessions);
				}
			}
		}

		let index = load_codex_index(&self.root);
		let roots = ["sessions", ".sessions", "archived_sessions"]
			.iter()
			.map(|sub| self.root.join(sub))
			.collect::<Vec<_>>();
		let files = rollout_files(&roots);

		let mut sessions = Vec::new();
		for file_path in files {
			let Some(first) = first_foreign_json_record(&file_path) else {
				continue;
			};
			if string_field(&first, "type") != Some("session_meta") {
				continue;
			}
			let Some(payload) = first.get("payload") else {
				continue;
			};

			let id = string_field(payload, "id").map_or_else(|| rollout_id(&file_path), String::from);
			let Some(cwd_str) = string_field(payload, "cwd") else {
				continue;
			};
			let cwd = PathBuf::from(cwd_str);

			let Ok(meta) = fs::metadata(&file_path) else {
				continue;
			};
			let mtime_ms = file_mtime_ms(&meta);
			let ctime_ms = file_birth_or_ctime_ms(&meta, mtime_ms);
			let indexed = index.get(&id);

			let created_ms = payload
				.get("timestamp")
				.map_or(ctime_ms, |v| parse_timestamp_ms(v, ctime_ms));

			let modified_ms = indexed
				.map_or(mtime_ms, |idx| parse_iso_timestamp_ms(&idx.updated_at).unwrap_or(mtime_ms));

			let title = indexed.map(|idx| idx.thread_name.clone());

			sessions.push(ForeignSessionInfo {
				source: ForeignSessionSource::Codex,
				id,
				path: file_path,
				cwd,
				title,
				created_ms,
				modified_ms,
				message_count: None,
				first_message: None,
			});
		}

		sessions.sort_by(|left, right| {
			right
				.modified_ms
				.cmp(&left.modified_ms)
				.then_with(|| left.id.cmp(&right.id))
		});

		Ok(sessions)
	}

	pub fn load(&self, info: &ForeignSessionInfo) -> Result<LoadedForeignSession> {
		if info.source != ForeignSessionSource::Codex {
			return Err(Error::new(format!(
				"Cannot load {} session with CodexSessionStore",
				info.source
			)));
		}

		let records = collect_foreign_json_records(&info.path).map_err(|e| {
			Error::new(format!(
				"Unable to read Codex session {} at {}: {e}",
				info.id,
				info.path.display()
			))
		})?;

		if records.is_empty() {
			return Err(Error::new(format!(
				"Codex session {} at {} is empty or malformed",
				info.id,
				info.path.display()
			)));
		}

		let meta_record = records.iter().find(|r| {
			string_field(&r.value, "type") == Some("session_meta")
				&& r.value.get("payload").is_some_and(Value::is_object)
		});

		let session_cwd = meta_record
			.and_then(|r| r.value.get("payload"))
			.and_then(|p| string_field(p, "cwd"))
			.map_or_else(|| info.cwd.clone(), PathBuf::from);

		let canonical = canonical_texts(&records);
		let mut converted: Vec<CodexConvertedItem> = Vec::new();
		let mut tool_names: HashMap<String, String> = HashMap::new();
		let mut model = "codex".to_string();
		let mut fallback_timestamp = info.created_ms;
		let mut title = info.title.clone();

		for record in &records {
			let ts = record
				.value
				.get("timestamp")
				.map_or(fallback_timestamp, |v| parse_timestamp_ms(v, fallback_timestamp));
			fallback_timestamp = fallback_timestamp.saturating_add(1).max(ts);

			let Some(payload) = record.value.get("payload").and_then(Value::as_object) else {
				continue;
			};

			let rec_type = string_field(&record.value, "type");
			let mut item: Option<CodexConvertedItem> = None;

			match rec_type {
				Some("turn_context") => {
					if let Some(next_model) = string_field_obj(payload, "model") {
						if next_model != model {
							model = next_model.to_string();
							item = Some(CodexConvertedItem::Model { model: model.clone(), timestamp: ts });
						}
					}
				},
				Some("response_item") => {
					item = converted_response_item(payload, ts, &model, &mut tool_names);
				},
				Some("event_msg") => {
					item = converted_event(payload, ts, &model, &canonical, &mut tool_names);
				},
				Some("compacted") => {
					let source_summary = string_field_obj(payload, "message")
						.map(str::trim)
						.filter(|s| !s.is_empty());
					let raw_repl = payload
						.get("replacement_history")
						.and_then(Value::as_array)
						.filter(|arr| arr.iter().all(Value::is_object));

					if source_summary.is_some() || raw_repl.is_some() {
						let compaction_item = raw_repl.and_then(|arr| {
							arr.iter().rev().find(|candidate| {
								let is_compaction = string_field(candidate, "type") == Some("compaction")
									&& candidate
										.get("encrypted_content")
										.is_some_and(Value::is_string);
								let is_summary =
									string_field(candidate, "type") == Some("compaction_summary");
								is_compaction || is_summary
							})
						});

						let summary = source_summary.unwrap_or("Context compacted by Codex.");
						item = Some(CodexConvertedItem::Compaction {
							summary: summary.to_string(),
							replacement_history: raw_repl.cloned(),
							compaction_item: compaction_item.cloned(),
							timestamp: ts,
						});
					}
				},
				_ => {},
			}

			let Some(conv_item) = item else {
				continue;
			};

			match conv_item {
				CodexConvertedItem::Rollback { turns } => {
					rollback_codex_records(&mut converted, turns);
				},
				CodexConvertedItem::Title { title: new_title } => {
					title = Some(new_title);
				},
				CodexConvertedItem::Pair { primary, following } => {
					converted.push(*primary);
					converted.push(*following);
				},
				other => {
					converted.push(other);
				},
			}
		}

		let mut entries = Vec::new();
		let mut parent_id: Option<String> = None;
		let mut ordinal: usize = 0;

		for item in converted {
			ordinal += 1;
			let id = format!("codex-{}", to_base36(ordinal));
			let item_ts = item.timestamp();
			let timestamp = format_iso_timestamp(item_ts);

			match item {
				CodexConvertedItem::Message { message, .. } => {
					entries.push(json!({
						"type": "message",
						"id": id,
						"parentId": parent_id,
						"timestamp": timestamp,
						"message": message
					}));
					parent_id = Some(id);
				},
				CodexConvertedItem::Model { model: item_model, .. } => {
					entries.push(json!({
						"type": "model_change",
						"id": id,
						"parentId": parent_id,
						"timestamp": timestamp,
						"model": format!("openai-codex/{item_model}")
					}));
					parent_id = Some(id);
				},
				CodexConvertedItem::Compaction {
					summary,
					replacement_history,
					compaction_item,
					..
				} => {
					let preserve_data = replacement_history.map(|rh| {
						let mut remote = json!({
							"provider": "openai-codex",
							"replacementHistory": rh
						});
						if let Some(ci) = compaction_item {
							remote["compactionItem"] = ci;
						}
						json!({ "openaiRemoteCompaction": remote })
					});

					let mut comp_entry = json!({
						"type": "compaction",
						"id": id,
						"parentId": parent_id,
						"timestamp": timestamp,
						"summary": summary,
						"shortSummary": "Imported Codex compaction",
						"firstKeptEntryId": id,
						"tokensBefore": 0
					});
					if let Some(pd) = preserve_data {
						comp_entry["preserveData"] = pd;
					}

					entries.push(comp_entry);
					parent_id = Some(id);
				},
				_ => {},
			}
		}

		Ok(LoadedForeignSession {
			source: ForeignSessionSource::Codex,
			id: info.id.clone(),
			cwd: session_cwd,
			title,
			entries,
		})
	}
}

impl ForeignSessionStore for CodexSessionStore {
	fn source(&self) -> ForeignSessionSource {
		ForeignSessionSource::Codex
	}

	fn list(&self) -> Result<Vec<ForeignSessionInfo>> {
		self.list()
	}

	fn load(&self, info: &ForeignSessionInfo) -> Result<LoadedForeignSession> {
		self.load(info)
	}
}

// ─────────────────────────────────────────────────────────────────────────────
// Codex SQLite Query Helper
// ─────────────────────────────────────────────────────────────────────────────

fn query_codex_sqlite_threads(db_path: &Path, root: &Path) -> Result<Vec<ForeignSessionInfo>> {
	let conn = Connection::open_with_flags(
		db_path,
		OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
	)
	.map_err(|e| Error::new(format!("Failed to open Codex database {}: {e}", db_path.display())))?;

	let mut stmt = conn
		.prepare(
			"SELECT id, rollout_path, created_at, updated_at, cwd, title, first_user_message FROM threads",
		)
		.map_err(|e| Error::new(format!("Failed to prepare query on {}: {e}", db_path.display())))?;

	let rows = stmt
		.query_map([], |row| {
			let id: String = row.get(0)?;
			let rollout_path_str: String = row.get(1)?;
			let created_at: Option<i64> = row.get(2)?;
			let updated_at: Option<i64> = row.get(3)?;
			let cwd: String = row.get(4)?;
			let title: Option<String> = row.get(5)?;
			let first_user_message: Option<String> = row.get(6)?;

			Ok((id, rollout_path_str, created_at, updated_at, cwd, title, first_user_message))
		})
		.map_err(|e| Error::new(format!("Failed to execute query on {}: {e}", db_path.display())))?;

	let mut sessions = Vec::new();
	for row_res in rows {
		let Ok((id, rollout_path_str, created_at, updated_at, cwd, title, first_msg)) = row_res
		else {
			continue;
		};
		if id.is_empty() || rollout_path_str.is_empty() || cwd.is_empty() {
			continue;
		}

		let rollout_path = PathBuf::from(&rollout_path_str);
		let path = if rollout_path.is_absolute() {
			rollout_path
		} else {
			root.join(rollout_path)
		};

		let modified_ms = epoch_to_millis(updated_at, 0);
		let created_ms = epoch_to_millis(created_at, modified_ms);

		sessions.push(ForeignSessionInfo {
			source: ForeignSessionSource::Codex,
			id,
			path,
			cwd: PathBuf::from(cwd),
			title,
			created_ms,
			modified_ms,
			message_count: None,
			first_message: first_msg,
		});
	}

	sessions.sort_by(|left, right| {
		right
			.modified_ms
			.cmp(&left.modified_ms)
			.then_with(|| left.id.cmp(&right.id))
	});

	Ok(sessions)
}

fn epoch_to_millis(val: Option<i64>, fallback: u64) -> u64 {
	match val {
		Some(n) if n >= 0 => {
			let u = n as u64;
			if u < 10_000_000_000 { u * 1000 } else { u }
		},
		_ => fallback,
	}
}

// ─────────────────────────────────────────────────────────────────────────────
// Tolerant Foreign JSONL Record Reader
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ForeignJsonRecord {
	pub value: Value,
	pub line: usize,
}

pub fn collect_foreign_json_records(path: &Path) -> Result<Vec<ForeignJsonRecord>> {
	let file = File::open(path)?;
	let reader = BufReader::new(file);
	let mut records = Vec::new();
	let mut line_num = 0;

	for line_res in reader.lines() {
		line_num += 1;
		let Ok(line) = line_res else {
			continue;
		};
		let trimmed = line.trim();
		if trimmed.is_empty() {
			continue;
		}
		if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(trimmed) {
			records.push(ForeignJsonRecord { value: Value::Object(map), line: line_num });
		}
	}

	Ok(records)
}

fn first_foreign_json_record(path: &Path) -> Option<Value> {
	let file = File::open(path).ok()?;
	let reader = BufReader::new(file);

	for line_res in reader.lines() {
		let line = line_res.ok()?;
		let trimmed = line.trim();
		if trimmed.is_empty() {
			continue;
		}
		if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(trimmed) {
			return Some(Value::Object(map));
		}
	}
	None
}

// ─────────────────────────────────────────────────────────────────────────────
// Claude Helpers
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct ClaudeHistoryMetadata {
	created: u64,
	modified: u64,
	first_message: Option<String>,
	cwd: Option<String>,
	message_count: usize,
}

struct ConvertedClaudeMessage {
	message: Value,
	suffix: String,
}

pub fn resolve_claude_paths(home_override: Option<&Path>) -> (PathBuf, PathBuf) {
	if let Ok(override_path) = std::env::var("CLAUDE_CONFIG_DIR") {
		let trimmed = override_path.trim();
		if !trimmed.is_empty() {
			let config_dir = PathBuf::from(trimmed);
			let config_file = config_dir.join(".claude.json");
			return (config_dir, config_file);
		}
	}

	let home = home_override
		.map_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")), Path::to_path_buf);
	let config_dir = home.join(".claude");
	let config_file = home.join(".claude.json");
	(config_dir, config_file)
}

fn recorded_claude_cwd(file_path: &Path) -> Option<String> {
	let file = File::open(file_path).ok()?;
	let reader = BufReader::new(file.take(CLAUDE_CWD_PREFIX_BYTES));

	for line_res in reader.lines() {
		let Ok(line) = line_res else {
			continue;
		};
		let trimmed = line.trim();
		if trimmed.is_empty() {
			continue;
		}
		if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(trimmed) {
			if let Some(cwd) = string_field_obj(&map, "cwd") {
				return Some(cwd.to_string());
			}
		}
	}
	None
}

fn read_claude_history_index(file: &Path) -> HashMap<String, ClaudeHistoryMetadata> {
	let mut index = HashMap::new();
	let Ok(records) = collect_foreign_json_records(file) else {
		return index;
	};

	for record in records {
		let val = &record.value;
		let id = string_field(val, "sessionId").or_else(|| string_field(val, "session_id"));
		let timestamp = val
			.get("timestamp")
			.or_else(|| val.get("ts"))
			.map_or(0, |v| parse_timestamp_ms(v, 0));

		let Some(id) = id else {
			continue;
		};
		if timestamp == 0 {
			continue;
		}

		let raw_text = string_field(val, "display").or_else(|| string_field(val, "text"));
		let text = raw_text.and_then(clean_preview);
		let cwd = string_field(val, "project").map(String::from);

		if let Some(existing) = index.get_mut(id) {
			if timestamp < existing.created {
				existing.created = timestamp;
				if text.is_some() {
					existing.first_message.clone_from(&text);
				}
			}
			existing.modified = existing.modified.max(timestamp);
			if existing.first_message.is_none() && text.is_some() {
				existing.first_message.clone_from(&text);
			}
			existing.message_count += 1;
			if existing.cwd.is_none() && cwd.is_some() {
				existing.cwd.clone_from(&cwd);
			}
		} else {
			index.insert(
				id.to_string(),
				ClaudeHistoryMetadata {
					created: timestamp,
					modified: timestamp,
					first_message: text,
					cwd,
					message_count: 1,
				},
			);
		}
	}

	index
}

fn read_registered_claude_projects(root: &Path) -> Vec<String> {
	let (config_dir, config_file) = resolve_claude_paths(None);
	let config_path = if root == config_dir {
		config_file
	} else {
		root
			.parent()
			.map_or_else(|| PathBuf::from(".claude.json"), |p| p.join(".claude.json"))
	};

	let Ok(data) = fs::read_to_string(&config_path) else {
		return Vec::new();
	};
	let Ok(parsed) = serde_json::from_str::<Value>(&data) else {
		return Vec::new();
	};
	let Some(projects) = parsed.get("projects").and_then(Value::as_object) else {
		return Vec::new();
	};

	projects
		.keys()
		.filter(|k| Path::new(k).is_absolute())
		.cloned()
		.collect()
}

fn claude_project_cwd(encoded: &str, registered: &[String]) -> String {
	for reg in registered {
		if encode_project_dir(reg) == encoded {
			return reg.clone();
		}
	}
	if !encoded.starts_with('-') {
		return encoded.to_string();
	}
	encoded.replace('-', "/")
}

fn find_claude_project_files(root: &Path) -> Vec<(PathBuf, String)> {
	let registered = read_registered_claude_projects(root);
	let mut files = Vec::new();

	for container_name in ["projects", ".projects"] {
		let container = root.join(container_name);
		let Ok(entries) = fs::read_dir(&container) else {
			continue;
		};

		for project_entry in entries.flatten() {
			let Ok(meta) = project_entry.metadata() else {
				continue;
			};
			if !meta.is_dir() {
				continue;
			}
			let project_name = project_entry.file_name().to_string_lossy().to_string();
			let cwd = claude_project_cwd(&project_name, &registered);
			let proj_dir = project_entry.path();

			let Ok(session_entries) = fs::read_dir(&proj_dir) else {
				continue;
			};
			for sess_entry in session_entries.flatten() {
				let sess_path = sess_entry.path();
				if sess_path.is_file()
					&& sess_path
						.extension()
						.and_then(|s| s.to_str())
						.is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
				{
					files.push((sess_path, cwd.clone()));
				}
			}
		}
	}

	files
}

fn convert_claude_record(
	record: &Value,
	fallback_timestamp: u64,
	tool_names: &mut HashMap<String, String>,
) -> Vec<ConvertedClaudeMessage> {
	let rec_type = string_field(record, "type");
	if rec_type != Some("user") && rec_type != Some("assistant") {
		return Vec::new();
	}
	if record.get("isSidechain").is_some_and(|v| v == true)
		|| record.get("isMeta").is_some_and(|v| v == true)
	{
		return Vec::new();
	}

	let Some(msg) = record.get("message").and_then(Value::as_object) else {
		return Vec::new();
	};

	let timestamp = record
		.get("timestamp")
		.map_or(fallback_timestamp, |v| parse_timestamp_ms(v, fallback_timestamp));

	if rec_type == Some("assistant") {
		let content = claude_assistant_content(msg.get("content"), tool_names);
		let error_msg = string_field(record, "error");
		if content.is_empty() && error_msg.is_none() {
			return Vec::new();
		}

		let model = string_field_obj(msg, "model").unwrap_or("unknown");
		let usage = claude_usage(msg.get("usage"));

		let is_api_error = record.get("isApiErrorMessage").is_some_and(|v| v == true);
		let stop_reason = if is_api_error {
			"error"
		} else {
			claude_stop_reason(msg.get("stop_reason"))
		};

		let mut assistant_msg = json!({
			"role": "assistant",
			"content": content,
			"api": "anthropic-messages",
			"provider": "anthropic",
			"model": model,
			"usage": usage,
			"stopReason": stop_reason,
			"timestamp": timestamp,
		});

		if let Some(resp_id) = string_field_obj(msg, "id") {
			assistant_msg["responseId"] = json!(resp_id);
		}
		if let Some(err) = error_msg {
			assistant_msg["errorMessage"] = json!(err);
		}
		if let Some(status) = record.get("apiErrorStatus").and_then(Value::as_f64) {
			if status.is_finite() {
				assistant_msg["errorStatus"] = json!(status as i64);
			}
		}

		return vec![ConvertedClaudeMessage {
			message: assistant_msg,
			suffix: "message".to_string(),
		}];
	}

	// User turn: parse interleaved user text/images and tool_results
	let raw_content = msg.get("content");
	if let Some(arr) = raw_content.and_then(Value::as_array) {
		let mut results = Vec::new();
		let mut user_parts = Vec::new();
		let mut content_start = 0;

		for (index, block) in arr.iter().enumerate() {
			if let Some(part) = claude_user_part(block) {
				if user_parts.is_empty() {
					content_start = index;
				}
				user_parts.push(part);
				continue;
			}

			if string_field(block, "type") != Some("tool_result") {
				continue;
			}
			let Some(call_id) = string_field(block, "tool_use_id") else {
				continue;
			};

			if !user_parts.is_empty() {
				results.push(ConvertedClaudeMessage {
					message: json!({
						"role": "user",
						"content": user_parts.clone(),
						"timestamp": timestamp,
					}),
					suffix: format!("user-{content_start}"),
				});
				user_parts.clear();
			}

			let tool_name = tool_names.get(call_id).map_or("unknown", String::as_str);
			let content = claude_tool_result_content(block.get("content"));
			let is_error = block.get("is_error").is_some_and(|v| v == true);

			results.push(ConvertedClaudeMessage {
				message: json!({
					"role": "toolResult",
					"toolCallId": call_id,
					"toolName": tool_name,
					"content": content,
					"isError": is_error,
					"timestamp": timestamp,
				}),
				suffix: format!("tool-{index}"),
			});
		}

		if !user_parts.is_empty() {
			results.push(ConvertedClaudeMessage {
				message: json!({
					"role": "user",
					"content": user_parts,
					"timestamp": timestamp,
				}),
				suffix: format!("user-{content_start}"),
			});
		}

		if !results.is_empty() {
			return results;
		}
	}

	let user_content = claude_user_content(raw_content);
	let Some(content) = user_content else {
		return Vec::new();
	};

	vec![ConvertedClaudeMessage {
		message: json!({
			"role": "user",
			"content": content,
			"timestamp": timestamp,
		}),
		suffix: "message".to_string(),
	}]
}

fn claude_image_content(value: &Value) -> Option<Value> {
	if string_field(value, "type") != Some("image") {
		return None;
	}
	let source = value.get("source")?;
	let data = string_field(source, "data")?;
	let media_type = string_field(source, "media_type")?;
	Some(json!({
		"type": "image",
		"data": data,
		"mimeType": media_type
	}))
}

fn claude_user_part(value: &Value) -> Option<Value> {
	if string_field(value, "type") == Some("text") {
		if let Some(txt) = string_field(value, "text") {
			return Some(json!({ "type": "text", "text": txt }));
		}
	}
	claude_image_content(value)
}

fn claude_user_content(value: Option<&Value>) -> Option<Value> {
	let val = value?;
	if let Some(s) = val.as_str() {
		if s.is_empty() {
			return None;
		}
		return Some(json!(s));
	}
	let arr = val.as_array()?;
	let mut content = Vec::new();
	for block in arr {
		if let Some(part) = claude_user_part(block) {
			content.push(part);
		}
	}
	if content.is_empty() {
		None
	} else {
		Some(json!(content))
	}
}

fn claude_tool_result_content(value: Option<&Value>) -> Value {
	let Some(val) = value else {
		return json!([]);
	};
	if let Some(s) = val.as_str() {
		return json!([{ "type": "text", "text": s }]);
	}
	if let Some(arr) = val.as_array() {
		let mut content = Vec::new();
		for block in arr {
			if let Some(part) = claude_user_part(block) {
				content.push(part);
			}
		}
		return json!(content);
	}
	json!([])
}

fn claude_assistant_content(
	value: Option<&Value>,
	tool_names: &mut HashMap<String, String>,
) -> Vec<Value> {
	let Some(arr) = value.and_then(Value::as_array) else {
		return Vec::new();
	};
	let mut content = Vec::new();

	for block in arr {
		match string_field(block, "type") {
			Some("text") => {
				if let Some(txt) = string_field(block, "text") {
					content.push(json!({ "type": "text", "text": txt }));
				}
			},
			Some("thinking") => {
				if let Some(thinking) = string_field(block, "thinking") {
					let mut part = json!({ "type": "thinking", "thinking": thinking });
					if let Some(sig) = string_field(block, "signature") {
						part["thinkingSignature"] = json!(sig);
					}
					content.push(part);
				}
			},
			Some("tool_use") => {
				let id = string_field(block, "id");
				let name = string_field(block, "name");
				if let (Some(id), Some(name)) = (id, name) {
					let args = block
						.get("input")
						.filter(|v| v.is_object())
						.cloned()
						.unwrap_or_else(|| json!({}));
					content.push(json!({
						"type": "toolCall",
						"id": id,
						"name": name,
						"arguments": args
					}));
					tool_names.insert(id.to_string(), name.to_string());
				}
			},
			_ => {},
		}
	}

	content
}

fn claude_usage(value: Option<&Value>) -> Value {
	let Some(val) = value else {
		return json!({
			"input": 0,
			"output": 0,
			"cacheRead": 0,
			"cacheWrite": 0,
			"totalTokens": 0,
			"cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 }
		});
	};

	let input = number_field(val, "input_tokens");
	let output = number_field(val, "output_tokens");
	let cache_read = number_field(val, "cache_read_input_tokens");
	let cache_write = number_field(val, "cache_creation_input_tokens");
	let total = input + output + cache_read + cache_write;

	json!({
		"input": input,
		"output": output,
		"cacheRead": cache_read,
		"cacheWrite": cache_write,
		"totalTokens": total,
		"cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 }
	})
}

fn claude_stop_reason(value: Option<&Value>) -> &'static str {
	match value.and_then(Value::as_str) {
		Some("tool_use") => "toolUse",
		Some("max_tokens") => "length",
		Some("end_turn" | "stop_sequence" | "pause_turn") => "stop",
		None => "stop",
		_ => "error",
	}
}

fn resolve_parent_chain<'a>(
	mut cursor: Option<&'a str>,
	source_parents: &'a HashMap<String, Option<String>>,
	source_tails: &HashMap<String, String>,
) -> Option<String> {
	let mut seen = HashSet::new();
	while let Some(c) = cursor {
		if !seen.insert(c.to_string()) {
			break;
		}
		if let Some(retained) = source_tails.get(c) {
			return Some(retained.clone());
		}
		cursor = source_parents.get(c).and_then(|opt| opt.as_deref());
	}
	None
}

// ─────────────────────────────────────────────────────────────────────────────
// Codex Helpers
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct CodexIndexRow {
	thread_name: String,
	updated_at: String,
}

enum CodexConvertedItem {
	Message {
		message: Value,
		timestamp: u64,
	},
	Model {
		model: String,
		timestamp: u64,
	},
	Compaction {
		summary: String,
		replacement_history: Option<Vec<Value>>,
		compaction_item: Option<Value>,
		timestamp: u64,
	},
	Pair {
		primary: Box<CodexConvertedItem>,
		following: Box<CodexConvertedItem>,
	},
	Title {
		title: String,
	},
	Rollback {
		turns: usize,
	},
}

impl CodexConvertedItem {
	fn timestamp(&self) -> u64 {
		match self {
			Self::Message { timestamp, .. }
			| Self::Model { timestamp, .. }
			| Self::Compaction { timestamp, .. } => *timestamp,
			Self::Pair { primary, .. } => primary.timestamp(),
			Self::Title { .. } | Self::Rollback { .. } => 0,
		}
	}
}

struct CanonicalCodexTexts {
	users: HashSet<String>,
	assistants: HashSet<String>,
	tool_calls: HashSet<String>,
}

fn state_database_path(root: &Path) -> Option<(PathBuf, u32)> {
	let entries = fs::read_dir(root).ok()?;
	let mut candidates = Vec::new();

	for entry in entries.flatten() {
		let name = entry.file_name().to_string_lossy().to_string();
		if let Some(rest) = name.strip_prefix("state_") {
			if let Some(ver_str) = rest.strip_suffix(".sqlite") {
				if let Ok(ver) = ver_str.parse::<u32>() {
					candidates.push((entry.path(), ver));
				}
			}
		}
	}

	candidates.sort_by(|left, right| right.1.cmp(&left.1));
	candidates.into_iter().next()
}

fn load_codex_index(root: &Path) -> HashMap<String, CodexIndexRow> {
	let mut index = HashMap::new();
	let path = root.join("session_index.jsonl");
	let Ok(records) = collect_foreign_json_records(&path) else {
		return index;
	};

	for record in records {
		let val = &record.value;
		let id = string_field(val, "id");
		let name = string_field(val, "thread_name");
		let updated = string_field(val, "updated_at");

		if let (Some(id), Some(name), Some(updated)) = (id, name, updated) {
			index.insert(
				id.to_string(),
				CodexIndexRow { thread_name: name.to_string(), updated_at: updated.to_string() },
			);
		}
	}

	index
}

fn rollout_files(directories: &[PathBuf]) -> Vec<PathBuf> {
	let mut files = Vec::new();
	for dir in directories {
		collect_rollouts_recursive(dir, &mut files);
	}
	files
}

fn collect_rollouts_recursive(dir: &Path, acc: &mut Vec<PathBuf>) {
	let Ok(entries) = fs::read_dir(dir) else {
		return;
	};
	for entry in entries.flatten() {
		let path = entry.path();
		if path.is_dir() {
			collect_rollouts_recursive(&path, acc);
		} else if path.is_file()
			&& path
				.extension()
				.and_then(|s| s.to_str())
				.is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
		{
			acc.push(path);
		}
	}
}

fn rollout_id(file_path: &Path) -> String {
	let stem = file_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
	let candidate = stem.strip_prefix("rollout-").unwrap_or(stem);
	if is_uuid_string(candidate) {
		candidate.to_string()
	} else {
		stem.to_string()
	}
}

fn is_uuid_string(s: &str) -> bool {
	let bytes = s.as_bytes();
	if bytes.len() != 36 {
		return false;
	}
	for (i, &b) in bytes.iter().enumerate() {
		if i == 8 || i == 13 || i == 18 || i == 23 {
			if b != b'-' {
				return false;
			}
		} else if !b.is_ascii_hexdigit() {
			return false;
		}
	}
	true
}

fn image_from_data_url(value: &str, detail: Option<&str>) -> Option<Value> {
	let rest = value.strip_prefix("data:")?;
	let (mime, payload) = rest.split_once(";base64,")?;
	let mut obj = json!({
		"type": "image",
		"mimeType": mime,
		"data": payload
	});
	if let Some(d) = detail {
		if matches!(d, "auto" | "low" | "high" | "original") {
			obj["detail"] = json!(d);
		}
	}
	Some(obj)
}

fn codex_response_content(value: Option<&Value>) -> Vec<Value> {
	let Some(arr) = value.and_then(Value::as_array) else {
		return Vec::new();
	};
	let mut content = Vec::new();

	for item in arr {
		let Some(obj) = item.as_object() else {
			continue;
		};
		let item_type = string_field_obj(obj, "type");
		let text = string_field_obj(obj, "text");

		if matches!(item_type, Some("input_text" | "output_text" | "text"))
			&& let Some(txt) = text
		{
			content.push(json!({ "type": "text", "text": txt }));
			continue;
		}

		if item_type == Some("input_image")
			&& let Some(url) = string_field_obj(obj, "image_url")
		{
			let detail = string_field_obj(obj, "detail");
			if let Some(img) = image_from_data_url(url, detail) {
				content.push(img);
			}
		}
	}

	content
}

fn codex_text_from_content(value: Option<&Value>) -> String {
	let parts = codex_response_content(value);
	let mut res = String::new();
	for part in parts {
		if part.get("type").and_then(Value::as_str) == Some("text")
			&& let Some(txt) = part.get("text").and_then(Value::as_str)
		{
			res.push_str(txt);
		}
	}
	res
}

fn codex_tool_arguments(value: Option<&Value>) -> Value {
	let Some(val) = value else {
		return json!({});
	};
	if val.is_object() {
		return val.clone();
	}
	if let Some(s) = val.as_str() {
		if let Ok(parsed) = serde_json::from_str::<Value>(s) {
			if parsed.is_object() {
				return parsed;
			}
		}
		return json!({ "input": s });
	}
	json!({ "input": val })
}

fn codex_tool_output_content(value: Option<&Value>) -> Vec<Value> {
	let structured = codex_response_content(value);
	if !structured.is_empty() {
		return structured;
	}
	let Some(val) = value else {
		return Vec::new();
	};
	if let Some(s) = val.as_str() {
		return vec![json!({ "type": "text", "text": s })];
	}
	vec![json!({ "type": "text", "text": val.to_string() })]
}

fn codex_reasoning_content(payload: &Map<String, Value>) -> Vec<Value> {
	let mut parts = Vec::new();
	for key in ["summary", "content"] {
		if let Some(arr) = payload.get(key).and_then(Value::as_array) {
			for item in arr {
				if let Some(txt) = string_field(item, "text") {
					parts.push(json!({ "type": "thinking", "thinking": txt }));
				}
			}
		}
	}
	parts
}

fn codex_assistant_message(
	content: Vec<Value>,
	timestamp: u64,
	model: &str,
	stop_reason: &str,
) -> Value {
	json!({
		"role": "assistant",
		"content": content,
		"api": "openai-codex-responses",
		"provider": "openai-codex",
		"model": model,
		"usage": {
			"input": 0,
			"output": 0,
			"cacheRead": 0,
			"cacheWrite": 0,
			"totalTokens": 0,
			"cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 }
		},
		"stopReason": stop_reason,
		"timestamp": timestamp,
	})
}

fn canonical_texts(records: &[ForeignJsonRecord]) -> CanonicalCodexTexts {
	let mut users = HashSet::new();
	let mut assistants = HashSet::new();
	let mut tool_calls = HashSet::new();

	for record in records {
		if string_field(&record.value, "type") != Some("response_item") {
			continue;
		}
		let Some(payload) = record.value.get("payload").and_then(Value::as_object) else {
			continue;
		};

		let call_id =
			string_field_obj(payload, "call_id").or_else(|| string_field_obj(payload, "id"));
		if let Some(cid) = call_id
			&& string_field_obj(payload, "type").is_some_and(|t| t.contains("call"))
		{
			tool_calls.insert(cid.to_string());
		}

		match string_field_obj(payload, "type") {
			Some("message") => {
				let text = codex_text_from_content(payload.get("content"));
				if !text.is_empty() {
					match string_field_obj(payload, "role") {
						Some("user") => {
							users.insert(text);
						},
						Some("assistant") => {
							assistants.insert(text);
						},
						_ => {},
					}
				}
			},
			Some("reasoning") => {
				for part in codex_reasoning_content(payload) {
					if let Some(th) = part.get("thinking").and_then(Value::as_str) {
						assistants.insert(th.to_string());
					}
				}
			},
			_ => {},
		}
	}

	CanonicalCodexTexts { users, assistants, tool_calls }
}

fn converted_response_item(
	payload: &Map<String, Value>,
	timestamp: u64,
	model: &str,
	tool_names: &mut HashMap<String, String>,
) -> Option<CodexConvertedItem> {
	let item_type = string_field_obj(payload, "type");
	match item_type {
		Some("message") => {
			let role = string_field_obj(payload, "role")?;
			let content = codex_response_content(payload.get("content"));
			if content.is_empty() {
				return None;
			}
			if role == "user" {
				Some(CodexConvertedItem::Message {
					message: json!({
						"role": "user",
						"content": content,
						"timestamp": timestamp,
					}),
					timestamp,
				})
			} else if role == "assistant" {
				Some(CodexConvertedItem::Message {
					message: codex_assistant_message(content, timestamp, model, "stop"),
					timestamp,
				})
			} else {
				None
			}
		},
		Some("reasoning") => {
			let content = codex_reasoning_content(payload);
			if content.is_empty() {
				None
			} else {
				Some(CodexConvertedItem::Message {
					message: codex_assistant_message(content, timestamp, model, "stop"),
					timestamp,
				})
			}
		},
		Some("function_call" | "custom_tool_call") => {
			let call_id =
				string_field_obj(payload, "call_id").or_else(|| string_field_obj(payload, "id"))?;
			let name = string_field_obj(payload, "name")?;
			tool_names.insert(call_id.to_string(), name.to_string());

			let is_custom = item_type == Some("custom_tool_call");
			let args_val = if is_custom {
				payload.get("input")
			} else {
				payload.get("arguments")
			};
			let mut call_obj = json!({
				"type": "toolCall",
				"id": call_id,
				"name": name,
				"arguments": codex_tool_arguments(args_val)
			});
			if is_custom {
				call_obj["customWireName"] = json!(name);
			}

			Some(CodexConvertedItem::Message {
				message: codex_assistant_message(vec![call_obj], timestamp, model, "toolUse"),
				timestamp,
			})
		},
		Some("function_call_output" | "custom_tool_call_output") => {
			let call_id = string_field_obj(payload, "call_id")?;
			let name = tool_names.get(call_id).map_or("unknown", String::as_str);
			let content = codex_tool_output_content(payload.get("output"));

			Some(CodexConvertedItem::Message {
				message: json!({
					"role": "toolResult",
					"toolCallId": call_id,
					"toolName": name,
					"content": content,
					"isError": false,
					"timestamp": timestamp,
				}),
				timestamp,
			})
		},
		Some("web_search_call" | "tool_search_call") => {
			let call_id =
				string_field_obj(payload, "call_id").or_else(|| string_field_obj(payload, "id"))?;
			let is_web = item_type == Some("web_search_call");
			let name = if is_web { "web_search" } else { "tool_search" };
			tool_names.insert(call_id.to_string(), name.to_string());

			let input_val = if is_web {
				payload.get("action")
			} else {
				payload.get("arguments")
			};
			let call_obj = json!({
				"type": "toolCall",
				"id": call_id,
				"name": name,
				"arguments": codex_tool_arguments(input_val)
			});

			Some(CodexConvertedItem::Message {
				message: codex_assistant_message(vec![call_obj], timestamp, model, "toolUse"),
				timestamp,
			})
		},
		Some("tool_search_output") => {
			let call_id = string_field_obj(payload, "call_id")?;
			let name = tool_names
				.get(call_id)
				.map_or("tool_search", String::as_str);
			let content = codex_tool_output_content(payload.get("tools"));
			let is_error = string_field_obj(payload, "status") == Some("failed");

			Some(CodexConvertedItem::Message {
				message: json!({
					"role": "toolResult",
					"toolCallId": call_id,
					"toolName": name,
					"content": content,
					"isError": is_error,
					"timestamp": timestamp,
				}),
				timestamp,
			})
		},
		_ => None,
	}
}

fn converted_event(
	payload: &Map<String, Value>,
	timestamp: u64,
	model: &str,
	canonical: &CanonicalCodexTexts,
	tool_names: &mut HashMap<String, String>,
) -> Option<CodexConvertedItem> {
	match string_field_obj(payload, "type") {
		Some("user_message") => {
			let text = string_field_obj(payload, "message")?;
			if text.is_empty() || canonical.users.contains(text) {
				return None;
			}
			Some(CodexConvertedItem::Message {
				message: json!({
					"role": "user",
					"content": text,
					"timestamp": timestamp,
				}),
				timestamp,
			})
		},
		Some("agent_message") => {
			let text = string_field_obj(payload, "message")?;
			if text.is_empty() || canonical.assistants.contains(text) {
				return None;
			}
			Some(CodexConvertedItem::Message {
				message: codex_assistant_message(
					vec![json!({ "type": "text", "text": text })],
					timestamp,
					model,
					"stop",
				),
				timestamp,
			})
		},
		Some("agent_reasoning") => {
			let text = string_field_obj(payload, "text")?;
			if text.is_empty() || canonical.assistants.contains(text) {
				return None;
			}
			Some(CodexConvertedItem::Message {
				message: codex_assistant_message(
					vec![json!({ "type": "thinking", "thinking": text })],
					timestamp,
					model,
					"stop",
				),
				timestamp,
			})
		},
		Some("dynamic_tool_call_request") => {
			let call_id =
				string_field_obj(payload, "callId").or_else(|| string_field_obj(payload, "call_id"))?;
			let name = string_field_obj(payload, "tool")?;
			if canonical.tool_calls.contains(call_id) {
				return None;
			}
			tool_names.insert(call_id.to_string(), name.to_string());

			let call = json!({
				"type": "toolCall",
				"id": call_id,
				"name": name,
				"arguments": codex_tool_arguments(payload.get("arguments"))
			});

			Some(CodexConvertedItem::Message {
				message: codex_assistant_message(vec![call], timestamp, model, "toolUse"),
				timestamp,
			})
		},
		Some("dynamic_tool_call_response") => {
			let call_id =
				string_field_obj(payload, "call_id").or_else(|| string_field_obj(payload, "callId"))?;
			if canonical.tool_calls.contains(call_id) {
				return None;
			}
			let name = tool_names
				.get(call_id)
				.map(String::as_str)
				.or_else(|| string_field_obj(payload, "tool"))
				.unwrap_or("unknown");
			let error = string_field_obj(payload, "error");
			let is_error = error.is_some() || payload.get("success").is_some_and(|v| v == false);

			let content = if let Some(err) = error {
				vec![json!({ "type": "text", "text": err })]
			} else {
				codex_tool_output_content(payload.get("content_items"))
			};

			Some(CodexConvertedItem::Message {
				message: json!({
					"role": "toolResult",
					"toolCallId": call_id,
					"toolName": name,
					"content": content,
					"isError": is_error,
					"timestamp": timestamp,
				}),
				timestamp,
			})
		},
		Some("web_search_end") => {
			let call_id = string_field_obj(payload, "call_id")?;
			let name = tool_names
				.get(call_id)
				.cloned()
				.unwrap_or_else(|| "web_search".to_string());

			let res_content =
				codex_tool_output_content(payload.get("results").or_else(|| payload.get("query")));
			let result_msg = json!({
				"role": "toolResult",
				"toolCallId": call_id,
				"toolName": name,
				"content": res_content,
				"isError": false,
				"timestamp": timestamp,
			});

			if canonical.tool_calls.contains(call_id) || tool_names.contains_key(call_id) {
				Some(CodexConvertedItem::Message { message: result_msg, timestamp })
			} else {
				tool_names.insert(call_id.to_string(), name.to_string());
				let call_input = payload.get("action").or_else(|| payload.get("query"));
				let call = json!({
					"type": "toolCall",
					"id": call_id,
					"name": name,
					"arguments": codex_tool_arguments(call_input)
				});

				Some(CodexConvertedItem::Pair {
					primary: Box::new(CodexConvertedItem::Message {
						message: codex_assistant_message(vec![call], timestamp, model, "toolUse"),
						timestamp,
					}),
					following: Box::new(CodexConvertedItem::Message { message: result_msg, timestamp }),
				})
			}
		},
		Some("mcp_tool_call_end") => {
			let call_id = string_field_obj(payload, "call_id")?;
			if canonical.tool_calls.contains(call_id) {
				return None;
			}
			let invocation = payload.get("invocation").and_then(Value::as_object)?;
			let server = string_field_obj(invocation, "server")?;
			let tool = string_field_obj(invocation, "tool")?;
			let name = format!("{server}/{tool}");
			tool_names.insert(call_id.to_string(), name.clone());

			let call = json!({
				"type": "toolCall",
				"id": call_id,
				"name": name,
				"arguments": codex_tool_arguments(invocation.get("arguments"))
			});

			let res_val = payload.get("result");
			let unwrapped_ok = res_val
				.and_then(|r| r.get("Ok"))
				.unwrap_or_else(|| res_val.unwrap_or(&Value::Null));

			let error = res_val.and_then(|r| r.get("Err")).and_then(Value::as_str);

			let is_error = error.is_some() || unwrapped_ok.get("isError").is_some_and(|v| v == true);

			let content = if let Some(err) = error {
				vec![json!({ "type": "text", "text": err })]
			} else {
				let body = unwrapped_ok.get("content").unwrap_or(unwrapped_ok);
				codex_tool_output_content(Some(body))
			};

			let result_msg = json!({
				"role": "toolResult",
				"toolCallId": call_id,
				"toolName": name,
				"content": content,
				"isError": is_error,
				"timestamp": timestamp,
			});

			Some(CodexConvertedItem::Pair {
				primary: Box::new(CodexConvertedItem::Message {
					message: codex_assistant_message(vec![call], timestamp, model, "toolUse"),
					timestamp,
				}),
				following: Box::new(CodexConvertedItem::Message { message: result_msg, timestamp }),
			})
		},
		Some("thread_name_updated") => string_field_obj(payload, "thread_name")
			.map(|title| CodexConvertedItem::Title { title: title.to_string() }),
		Some("thread_rolled_back") => {
			let turns = payload
				.get("num_turns")
				.and_then(Value::as_u64)
				.unwrap_or(0) as usize;
			Some(CodexConvertedItem::Rollback { turns })
		},
		_ => None,
	}
}

fn rollback_codex_records(records: &mut Vec<CodexConvertedItem>, mut turns: usize) {
	while turns > 0 {
		let mut user_index = None;
		for (index, item) in records.iter().enumerate().rev() {
			if let CodexConvertedItem::Message { message, .. } = item {
				if message.get("role").and_then(Value::as_str) == Some("user") {
					user_index = Some(index);
					break;
				}
			}
		}
		let Some(idx) = user_index else {
			return;
		};
		records.truncate(idx);
		turns -= 1;
	}
}

// ─────────────────────────────────────────────────────────────────────────────
// Shared Parsing & Formatting Utilities
// ─────────────────────────────────────────────────────────────────────────────

fn string_field<'a>(val: &'a Value, key: &str) -> Option<&'a str> {
	val.get(key)
		.and_then(Value::as_str)
		.filter(|s| !s.is_empty())
}

fn string_field_obj<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
	map.get(key)
		.and_then(Value::as_str)
		.filter(|s| !s.is_empty())
}

fn number_field(val: &Value, key: &str) -> u64 {
	val.get(key)
		.and_then(Value::as_f64)
		.filter(|n| n.is_finite() && *n >= 0.0)
		.map_or(0, |n| n as u64)
}

fn clean_preview(value: &str) -> Option<String> {
	let mut out = String::with_capacity(value.len());
	let mut in_whitespace = false;
	for ch in value.chars() {
		if ch.is_whitespace() {
			if !in_whitespace && !out.is_empty() {
				out.push(' ');
				in_whitespace = true;
			}
		} else {
			out.push(ch);
			in_whitespace = false;
		}
	}
	let trimmed = out.trim();
	if trimmed.is_empty() {
		None
	} else {
		Some(trimmed.to_string())
	}
}

fn unique_entry_id(base: &str, used: &mut HashSet<String>) -> String {
	let mut id = base.to_string();
	let mut suffix = 2;
	while used.contains(&id) {
		id = format!("{base}-{suffix}");
		suffix += 1;
	}
	used.insert(id.clone());
	id
}

fn to_base36(mut n: usize) -> String {
	if n == 0 {
		return "0".to_string();
	}
	const CHARSET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
	let mut buf = Vec::new();
	while n > 0 {
		buf.push(CHARSET[n % 36]);
		n /= 36;
	}
	buf.reverse();
	String::from_utf8(buf).unwrap_or_else(|_| "0".to_string())
}

fn file_mtime_ms(meta: &fs::Metadata) -> u64 {
	meta
		.modified()
		.ok()
		.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
		.map_or(0, |d| d.as_millis() as u64)
}

fn file_birth_or_ctime_ms(meta: &fs::Metadata, fallback_ms: u64) -> u64 {
	meta
		.created()
		.ok()
		.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
		.map_or(fallback_ms, |d| d.as_millis() as u64)
}

fn iso_timestamp_from_val(val: Option<&Value>, fallback_ms: u64) -> String {
	let ts = val.map_or(fallback_ms, |v| parse_timestamp_ms(v, fallback_ms));
	format_iso_timestamp(ts)
}

pub fn parse_timestamp_ms(value: &Value, fallback: u64) -> u64 {
	if let Some(num) = value.as_f64() {
		if num.is_finite() && num >= 0.0 {
			let u = num as u64;
			return if u < 10_000_000_000 { u * 1000 } else { u };
		}
	}
	if let Some(s) = value.as_str() {
		if let Some(parsed) = parse_iso_timestamp_ms(s) {
			return parsed;
		}
		if let Ok(num) = s.parse::<f64>() {
			if num.is_finite() && num >= 0.0 {
				let u = num as u64;
				return if u < 10_000_000_000 { u * 1000 } else { u };
			}
		}
	}
	fallback
}

pub fn parse_iso_timestamp_ms(s: &str) -> Option<u64> {
	let s = s.trim();
	if s.len() < 19 {
		return None;
	}
	let year: i32 = s.get(0..4)?.parse().ok()?;
	if s.as_bytes().get(4)? != &b'-' {
		return None;
	}
	let month: u32 = s.get(5..7)?.parse().ok()?;
	if s.as_bytes().get(7)? != &b'-' {
		return None;
	}
	let day: u32 = s.get(8..10)?.parse().ok()?;
	let t = s.as_bytes().get(10)?;
	if *t != b'T' && *t != b' ' {
		return None;
	}
	let hour: u32 = s.get(11..13)?.parse().ok()?;
	if s.as_bytes().get(13)? != &b':' {
		return None;
	}
	let min: u32 = s.get(14..16)?.parse().ok()?;
	if s.as_bytes().get(16)? != &b':' {
		return None;
	}
	let sec: u32 = s.get(17..19)?.parse().ok()?;

	let mut millis: u32 = 0;
	let rest = &s[19..];
	let (frac_part, tz_part) = if let Some(stripped) = rest.strip_prefix('.') {
		let end = stripped
			.find(|c: char| !c.is_ascii_digit())
			.unwrap_or(stripped.len());
		(&stripped[..end], &stripped[end..])
	} else {
		("", rest)
	};

	if !frac_part.is_empty() {
		let f_len = frac_part.len();
		let val: u32 = frac_part.parse().ok()?;
		millis = match f_len {
			1 => val * 100,
			2 => val * 10,
			3 => val,
			_ => val / 10u32.pow((f_len - 3) as u32),
		};
	}

	let days = civil_date_to_days(year, month, day);
	let total_secs = days * 86400 + i64::from(hour) * 3600 + i64::from(min) * 60 + i64::from(sec);

	let tz_offset_secs = if tz_part.starts_with('+') || tz_part.starts_with('-') {
		let sign: i64 = if tz_part.starts_with('+') { 1 } else { -1 };
		let tz_body = &tz_part[1..];
		if let Some((h_str, m_str)) = tz_body.split_once(':') {
			let h: i64 = h_str.parse().ok()?;
			let m: i64 = m_str.parse().ok()?;
			sign * (h * 3600 + m * 60)
		} else if tz_body.len() >= 2 {
			let h: i64 = tz_body[..2].parse().ok()?;
			sign * h * 3600
		} else {
			0
		}
	} else {
		0
	};

	let utc_secs = total_secs - tz_offset_secs;
	if utc_secs < 0 {
		return None;
	}
	Some((utc_secs as u64) * 1000 + u64::from(millis))
}

pub fn format_iso_timestamp(ms: u64) -> String {
	let secs = ms / 1000;
	let millis = ms % 1000;
	let days = (secs / 86400) as i64;
	let rem_secs = (secs % 86400) as u32;
	let hours = rem_secs / 3600;
	let mins = (rem_secs % 3600) / 60;
	let s = rem_secs % 60;

	let (year, month, day) = civil_days_to_date(days);
	format!("{year:04}-{month:02}-{day:02}T{hours:02}:{mins:02}:{s:02}.{millis:03}Z")
}

fn civil_date_to_days(y: i32, m: u32, d: u32) -> i64 {
	let y = if m <= 2 {
		i64::from(y) - 1
	} else {
		i64::from(y)
	};
	let era = if y >= 0 { y / 400 } else { (y - 399) / 400 };
	let yoe = (y - era * 400) as u32;
	let mp = if m > 2 { m - 3 } else { m + 9 };
	let doy = (153 * mp + 2) / 5 + d - 1;
	let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
	era * 146097 + i64::from(doe) - 719468
}

fn civil_days_to_date(days: i64) -> (i32, u32, u32) {
	let z = days + 719468;
	let era = if z >= 0 {
		z / 146097
	} else {
		(z - 146096) / 146097
	};
	let doe = (z - era * 146097) as u32;
	let yoe = (doe - doe / 1024 + doe / 1461 - doe / 146096) / 365;
	let y = i64::from(yoe) + era * 400;
	let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
	let mp = (5 * doy + 2) / 153;
	let d = doy - (153 * mp + 2) / 5 + 1;
	let m = if mp < 10 { mp + 3 } else { mp - 9 };
	let y = if m <= 2 { y + 1 } else { y };
	(y as i32, m, d)
}
