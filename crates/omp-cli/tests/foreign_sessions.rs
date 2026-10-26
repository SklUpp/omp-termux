//! Integration tests for foreign session import readers (Claude Code & OpenAI Codex).
//! Matches behavior and fixtures from packages/coding-agent/test/foreign-session-stores.test.ts (port02).

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use omp_cli::foreign_session::{
	ClaudeSessionStore, CodexSessionStore, CodexSqliteDiscovery, ForeignSessionInfo,
	ForeignSessionSource, ForeignSessionStore, LoadedForeignSession, collect_foreign_json_records,
	create_foreign_session_store, encode_project_dir, format_iso_timestamp, parse_iso_timestamp_ms,
};
use rusqlite::Connection;
use serde_json::{Value, json};

struct TestDir {
	path: PathBuf,
}

impl TestDir {
	fn new(prefix: &str) -> Self {
		let dir =
			std::env::temp_dir().join(format!("omp-foreign-test-{prefix}-{}", std::process::id()));
		let _ = fs::remove_dir_all(&dir);
		fs::create_dir_all(&dir).expect("create test temp dir");
		Self { path: dir }
	}

	fn path(&self) -> &Path {
		&self.path
	}
}

impl Drop for TestDir {
	fn drop(&mut self) {
		let _ = fs::remove_dir_all(&self.path);
	}
}

fn write_jsonl(path: &Path, records: &[Value]) {
	if let Some(parent) = path.parent() {
		fs::create_dir_all(parent).expect("create parent dirs");
	}
	let mut file = File::create(path).expect("create jsonl file");
	for record in records {
		let line = serde_json::to_string(record).expect("serialize json");
		writeln!(file, "{line}").expect("write line");
	}
}

// ─────────────────────────────────────────────────────────────────────────────
// Claude Tests (port02 Oracle Parity)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_claude_unindexed_recorded_cwd() {
	let temp = TestDir::new("claude-recorded-cwd");
	let root = temp.path().join(".claude");
	let cwd = temp.path().join("my-project.dir");
	let id = "33333333-3333-4333-8333-333333333333";
	let encoded = encode_project_dir(&cwd.to_string_lossy());

	write_jsonl(
		&root
			.join("projects")
			.join(&encoded)
			.join(format!("{id}.jsonl")),
		&[
			json!({ "type": "file-history-snapshot", "timestamp": "2026-01-01T00:00:00.000Z" }),
			json!({
				"type": "user",
				"uuid": "u",
				"parentUuid": Value::Null,
				"timestamp": "2026-01-01T00:00:01.000Z",
				"cwd": cwd.to_string_lossy(),
				"message": { "content": "." }
			}),
		],
	);

	let store = ClaudeSessionStore::new(&root);
	let list = store.list().expect("list sessions");
	let session = list.iter().find(|s| s.id == id).expect("session found");
	assert_eq!(session.cwd, cwd);
}

#[test]
fn test_claude_bounded_prefix_fallback() {
	let temp = TestDir::new("claude-prefix-fallback");
	let root = temp.path().join(".claude");
	let cwd = temp.path().join("late-project.dir");
	let encoded = encode_project_dir(&cwd.to_string_lossy());
	let id = "55555555-5555-4555-8555-555555555555";

	// Snapshot exceeds 64 KiB bounded prefix
	let large_padding = "x".repeat(128 * 1024);
	write_jsonl(
		&root
			.join("projects")
			.join(&encoded)
			.join(format!("{id}.jsonl")),
		&[
			json!({ "type": "file-history-snapshot", "snapshot": large_padding }),
			json!({ "type": "user", "cwd": cwd.to_string_lossy(), "message": { "content": "." } }),
		],
	);

	let store = ClaudeSessionStore::new(&root);
	let list = store.list().expect("list sessions");
	let session = list.iter().find(|s| s.id == id).expect("session found");
	// Falls back to encoded project directory
	assert_eq!(session.cwd, PathBuf::from(encoded.replace('-', "/")));
}

#[test]
fn test_claude_history_cwd_precedence() {
	let temp = TestDir::new("claude-cwd-precedence");
	let root = temp.path().join(".claude");
	let indexed_cwd = temp.path().join("indexed-project");
	let id = "44444444-4444-4444-8444-444444444444";
	let encoded = encode_project_dir(&indexed_cwd.to_string_lossy());

	write_jsonl(
		&root.join("history.jsonl"),
		&[json!({
			"sessionId": id,
			"timestamp": 1_767_225_600_000u64,
			"display": ".",
			"project": indexed_cwd.to_string_lossy()
		})],
	);

	write_jsonl(
		&root
			.join("projects")
			.join(&encoded)
			.join(format!("{id}.jsonl")),
		&[json!({
			"type": "user",
			"uuid": "u",
			"parentUuid": Value::Null,
			"timestamp": "2026-01-01T00:00:00.000Z",
			"cwd": temp.path().join("somewhere-else").to_string_lossy(),
			"message": { "content": "." }
		})],
	);

	let store = ClaudeSessionStore::new(&root);
	let list = store.list().expect("list sessions");
	let session = list.iter().find(|s| s.id == id).expect("session found");
	assert_eq!(session.cwd, indexed_cwd);
}

#[test]
fn test_claude_api_error_import() {
	let temp = TestDir::new("claude-api-error");
	let root = temp.path().join(".claude");
	let cwd = temp.path().join("overloaded");
	let id = "22222222-2222-4222-8222-222222222222";
	let encoded = encode_project_dir(&cwd.to_string_lossy());

	write_jsonl(
		&root.join("history.jsonl"),
		&[json!({
			"sessionId": id,
			"timestamp": 1_767_225_600_000u64,
			"display": ".",
			"project": cwd.to_string_lossy()
		})],
	);

	write_jsonl(
		&root
			.join("projects")
			.join(&encoded)
			.join(format!("{id}.jsonl")),
		&[
			json!({
				"type": "user",
				"uuid": "u",
				"parentUuid": Value::Null,
				"timestamp": "2026-01-01T00:00:00.000Z",
				"cwd": cwd.to_string_lossy(),
				"message": { "content": "." }
			}),
			json!({
				"type": "assistant",
				"uuid": "a",
				"parentUuid": "u",
				"timestamp": "2026-01-01T00:00:01.000Z",
				"isApiErrorMessage": true,
				"apiErrorStatus": 529,
				"error": "server_error",
				"message": {
					"id": "msg_err",
					"model": "claude-sonnet-4-5",
					"stop_reason": "stop_sequence",
					"content": [{ "type": "text", "text": "API Error: 529 Overloaded." }]
				}
			}),
		],
	);

	let store = ClaudeSessionStore::new(&root);
	let list = store.list().expect("list sessions");
	let loaded = store.load(&list[0]).expect("load session");

	let assistant = loaded
		.messages()
		.into_iter()
		.find(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
		.unwrap();
	assert_eq!(assistant.get("stopReason").and_then(Value::as_str), Some("error"));
	assert_eq!(assistant.get("errorStatus").and_then(Value::as_i64), Some(529));
	assert_eq!(assistant.get("errorMessage").and_then(Value::as_str), Some("server_error"));
}

#[test]
fn test_claude_user_text_split_around_tool_result() {
	let temp = TestDir::new("claude-split-text");
	let root = temp.path().join(".claude");
	let cwd = temp.path().join("project-with-hyphen");
	let id = "11111111-1111-4111-8111-111111111111";
	let encoded = encode_project_dir(&cwd.to_string_lossy());
	let session_path = root
		.join("projects")
		.join(&encoded)
		.join(format!("{id}.jsonl"));

	write_jsonl(
		&root.join("history.jsonl"),
		&[
			json!({ "sessionId": id, "timestamp": 1_767_225_600_000u64, "display": "First prompt", "project": cwd.to_string_lossy() }),
			json!({ "sessionId": id, "timestamp": 1_767_225_603_000u64, "display": "Second prompt", "project": cwd.to_string_lossy() }),
		],
	);

	write_jsonl(
		&session_path,
		&[
			json!({
				"type": "user",
				"uuid": "claude-user",
				"parentUuid": Value::Null,
				"timestamp": "2026-01-01T00:00:00.000Z",
				"cwd": cwd.to_string_lossy(),
				"message": { "content": "First prompt" }
			}),
			json!({
				"type": "assistant",
				"uuid": "claude-assistant",
				"parentUuid": "claude-user",
				"timestamp": "2026-01-01T00:00:01.000Z",
				"message": {
					"id": "msg_claude",
					"model": "claude-sonnet-4-5",
					"stop_reason": "tool_use",
					"usage": { "input_tokens": 10, "output_tokens": 5 },
					"content": [
						{ "type": "thinking", "thinking": "Inspect the file", "signature": "sig" },
						{ "type": "text", "text": "I will inspect it." },
						{ "type": "tool_use", "id": "tool-claude", "name": "read", "input": { "path": "file.ts" } }
					]
				}
			}),
			json!({
				"type": "user",
				"uuid": "claude-result",
				"parentUuid": "claude-assistant",
				"timestamp": "2026-01-01T00:00:02.000Z",
				"message": {
					"content": [
						{ "type": "text", "text": "Preserve this before the result." },
						{ "type": "tool_result", "tool_use_id": "tool-claude", "content": "file contents" },
						{ "type": "text", "text": "Preserve this after the result." }
					]
				}
			}),
			json!({
				"type": "custom-title",
				"customTitle": "Imported Claude",
				"timestamp": "2026-01-01T00:00:03.000Z"
			}),
		],
	);

	let store = ClaudeSessionStore::new(&root);
	let list = store.list().expect("list sessions");
	let loaded = store.load(&list[0]).expect("load session");

	assert_eq!(loaded.title.as_deref(), Some("Imported Claude"));

	// Exact role sequence matching port02 oracle
	let roles = loaded.message_roles();
	assert_eq!(roles, vec!["user", "assistant", "user", "toolResult", "user"]);

	let messages = loaded.messages();
	assert_eq!(
		messages[2].get("content").unwrap(),
		&json!([{ "type": "text", "text": "Preserve this before the result." }])
	);
	assert_eq!(messages[3].get("toolCallId").and_then(Value::as_str), Some("tool-claude"));
	assert_eq!(
		messages[4].get("content").unwrap(),
		&json!([{ "type": "text", "text": "Preserve this after the result." }])
	);
}

#[test]
fn test_claude_image_preservation() {
	let temp = TestDir::new("claude-image");
	let root = temp.path().join(".claude");
	let id = "33333333-3333-3333-3333-333333333333";
	let session_path = root
		.join("projects")
		.join("sample")
		.join(format!("{id}.jsonl"));

	write_jsonl(
		&session_path,
		&[json!({
			"type": "user",
			"uuid": "img-user",
			"parentUuid": Value::Null,
			"timestamp": "2026-01-01T00:00:00.000Z",
			"message": {
				"content": [
					{ "type": "text", "text": "Look at this image" },
					{
						"type": "image",
						"source": {
							"type": "base64",
							"media_type": "image/png",
							"data": "iVBORw0KGgoAAAANSUhEUg=="
						}
					}
				]
			}
		})],
	);

	let store = ClaudeSessionStore::new(&root);
	let info = ForeignSessionInfo {
		source: ForeignSessionSource::Claude,
		id: id.to_string(),
		path: session_path,
		cwd: PathBuf::from("/tmp"),
		title: None,
		created_ms: 1_767_225_600_000,
		modified_ms: 1_767_225_600_000,
		message_count: None,
		first_message: None,
	};

	let loaded = store.load(&info).expect("load claude image session");
	let messages = loaded.messages();
	assert_eq!(messages.len(), 1);
	let content = messages[0]
		.get("content")
		.and_then(Value::as_array)
		.unwrap();
	let image_part = content
		.iter()
		.find(|b| b.get("type").and_then(Value::as_str) == Some("image"))
		.unwrap();
	assert_eq!(image_part.get("mimeType").and_then(Value::as_str), Some("image/png"));
	assert_eq!(image_part.get("data").and_then(Value::as_str), Some("iVBORw0KGgoAAAANSUhEUg=="));
}

// ─────────────────────────────────────────────────────────────────────────────
// Codex Tests (SQLite & Rollout Parity)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_codex_sqlite_discovery_and_query() {
	let temp = TestDir::new("codex-sqlite");
	let root = temp.path().join(".codex");
	fs::create_dir_all(&root).unwrap();

	let db_path = root.join("state_5.sqlite");
	let conn = Connection::open(&db_path).unwrap();
	conn.execute(
		"CREATE TABLE threads (id TEXT, rollout_path TEXT, created_at INTEGER, updated_at INTEGER, cwd TEXT, title TEXT, first_user_message TEXT)",
		[],
	).unwrap();

	conn
		.execute(
			"INSERT INTO threads VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
			(
				"33333333-3333-4333-8333-333333333333",
				"sessions/missing.jsonl",
				1_767_225_600i64,
				1_767_225_660i64,
				temp
					.path()
					.join("codex-project")
					.to_string_lossy()
					.to_string(),
				"Indexed Codex",
				"Indexed prompt",
			),
		)
		.unwrap();
	drop(conn);

	let store = CodexSessionStore::new(&root);
	let discovery = store.sqlite_discovery();
	assert!(matches!(discovery, CodexSqliteDiscovery::Available { version: 5, .. }));

	let list = store.list().expect("list codex sessions from sqlite");
	assert_eq!(list.len(), 1);
	let info = &list[0];
	assert_eq!(info.id, "33333333-3333-4333-8333-333333333333");
	assert_eq!(info.title.as_deref(), Some("Indexed Codex"));
	assert_eq!(info.first_message.as_deref(), Some("Indexed prompt"));
	assert_eq!(info.created_iso(), "2026-01-01T00:00:00.000Z");
	assert_eq!(info.modified_iso(), "2026-01-01T00:01:00.000Z");
}

#[test]
fn test_codex_sqlite_corrupted_or_schema_mismatch_no_overwrite() {
	let temp = TestDir::new("codex-schema-err");
	let root = temp.path().join(".codex");
	fs::create_dir_all(&root).unwrap();

	// Case 1: Newer/mismatched schema table
	let db_path = root.join("state_5.sqlite");
	let conn = Connection::open(&db_path).unwrap();
	conn
		.execute("CREATE TABLE wrong_table (id TEXT)", [])
		.unwrap();
	drop(conn);

	let before_bytes = fs::read(&db_path).unwrap();
	let before_mtime = fs::metadata(&db_path).unwrap().modified().unwrap();

	let store = CodexSessionStore::new(&root);

	// list_from_sqlite returns explicit error with path
	let err = store.list_from_sqlite().unwrap_err();
	assert!(
		err.to_string().contains("threads") || err.to_string().contains("prepare query"),
		"err: {err}"
	);

	// list() gracefully falls back to rollout scan without crashing
	let list = store.list().expect("list fallback");
	assert!(list.is_empty());

	// Assert database file is untouched (no migration, no overwrite)
	assert_eq!(fs::read(&db_path).unwrap(), before_bytes);
	assert_eq!(fs::metadata(&db_path).unwrap().modified().unwrap(), before_mtime);

	// Case 2: Corrupted garbage bytes
	let garbage = b"GARBAGE BYTES NOT A SQLITE FILE";
	fs::write(&db_path, garbage).unwrap();
	let garbage_mtime = fs::metadata(&db_path).unwrap().modified().unwrap();

	let err2 = store.list_from_sqlite().unwrap_err();
	assert!(err2.to_string().contains(&db_path.display().to_string()), "err: {err2}");
	assert_eq!(fs::read(&db_path).unwrap(), garbage);
	assert_eq!(fs::metadata(&db_path).unwrap().modified().unwrap(), garbage_mtime);
}

#[test]
fn test_codex_malformed_compaction_ignored() {
	let temp = TestDir::new("codex-comp-malformed");
	let root = temp.path().join(".codex");
	let id = "77777777-7777-7777-7777-777777777777";
	let session_path = root.join("sessions").join(format!("rollout-{id}.jsonl"));

	write_jsonl(
		&session_path,
		&[
			json!({ "type": "session_meta", "timestamp": "2025-01-01T00:00:00.000Z", "payload": { "id": id, "cwd": "/tmp" } }),
			json!({
				"type": "compacted",
				"timestamp": "2025-01-01T00:00:01.000Z",
				"payload": {
					"message": "",
					// malformed: elements are not records/objects
					"replacement_history": ["string-instead-of-object", 123]
				}
			}),
		],
	);

	let store = CodexSessionStore::new(&root);
	let info = ForeignSessionInfo {
		source: ForeignSessionSource::Codex,
		id: id.to_string(),
		path: session_path,
		cwd: PathBuf::from("/tmp"),
		title: None,
		created_ms: 0,
		modified_ms: 0,
		message_count: None,
		first_message: None,
	};

	let loaded = store.load(&info).expect("load session");
	// Compaction with non-object replacement_history and empty message is ignored
	let has_compaction = loaded
		.entries
		.iter()
		.any(|e| e.get("type").and_then(Value::as_str) == Some("compaction"));
	assert!(!has_compaction, "malformed replacement_history must not produce compaction entry");
}

#[test]
fn test_codex_legacy_rollout_list_and_load() {
	let temp = TestDir::new("codex-rollout");
	let root = temp.path().join(".codex");
	let cwd = temp.path().join("codex-project");
	let id = "44444444-4444-4444-8444-444444444444";
	let session_path = root
		.join(".sessions")
		.join("2025")
		.join("01")
		.join("01")
		.join(format!("rollout-{id}.jsonl"));

	write_jsonl(
		&root.join("session_index.jsonl"),
		&[json!({
			"id": id,
			"thread_name": "Legacy Codex",
			"updated_at": "2025-01-01T00:00:12.000Z"
		})],
	);

	let replacement_history = vec![
		json!({ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "Retained context" }] }),
		json!({ "type": "compaction", "encrypted_content": "encrypted-compaction" }),
	];

	write_jsonl(
		&session_path,
		&[
			json!({ "type": "session_meta", "timestamp": "2025-01-01T00:00:00.000Z", "payload": { "id": id, "cwd": cwd.to_string_lossy() } }),
			json!({ "type": "turn_context", "timestamp": "2025-01-01T00:00:01.000Z", "payload": { "model": "gpt-5.3-codex" } }),
			json!({ "type": "response_item", "timestamp": "2025-01-01T00:00:02.000Z", "payload": { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "Inspect" }] } }),
			json!({ "type": "response_item", "timestamp": "2025-01-01T00:00:03.000Z", "payload": { "type": "reasoning", "summary": [{ "type": "summary_text", "text": "Plan" }] } }),
			json!({ "type": "response_item", "timestamp": "2025-01-01T00:00:04.000Z", "payload": { "type": "function_call", "call_id": "call-codex", "name": "read", "arguments": "{\"path\":\"file.ts\"}" } }),
			json!({ "type": "response_item", "timestamp": "2025-01-01T00:00:05.000Z", "payload": { "type": "function_call_output", "call_id": "call-codex", "output": "file contents" } }),
			json!({ "type": "compacted", "timestamp": "2025-01-01T00:00:06.000Z", "payload": { "message": "", "replacement_history": replacement_history } }),
			json!({ "type": "turn_context", "timestamp": "2025-01-01T00:00:07.000Z", "payload": { "model": "gpt-5.3-codex" } }),
			json!({ "type": "response_item", "timestamp": "2025-01-01T00:00:08.000Z", "payload": { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "Continue" }] } }),
			json!({ "type": "event_msg", "timestamp": "2025-01-01T00:00:09.000Z", "payload": { "type": "web_search_end", "call_id": "call-web", "query": "docs", "action": { "query": "docs" } } }),
			json!({ "type": "response_item", "timestamp": "2025-01-01T00:00:10.000Z", "payload": { "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "Done" }] } }),
			json!({ "type": "event_msg", "timestamp": "2025-01-01T00:00:11.000Z", "payload": { "type": "thread_name_updated", "thread_name": "Imported Codex" } }),
		],
	);

	let store = CodexSessionStore::new(&root);
	let list = store.list().expect("list codex sessions");
	assert_eq!(list.len(), 1);
	let info = &list[0];
	assert_eq!(info.id, id);
	assert_eq!(info.title.as_deref(), Some("Legacy Codex"));

	let loaded = store.load(info).expect("load codex session");
	assert_eq!(loaded.source, ForeignSessionSource::Codex);
	assert_eq!(loaded.title.as_deref(), Some("Imported Codex"));

	let has_model = loaded.entries.iter().any(|e| {
		e.get("type").and_then(Value::as_str) == Some("model_change")
			&& e.get("model").and_then(Value::as_str) == Some("openai-codex/gpt-5.3-codex")
	});
	assert!(has_model, "expected openai-codex/gpt-5.3-codex model_change");

	let compaction = loaded
		.entries
		.iter()
		.find(|e| e.get("type").and_then(Value::as_str) == Some("compaction"));
	assert!(compaction.is_some(), "expected compaction entry");
	let comp = compaction.unwrap();
	assert_eq!(
		comp
			.get("preserveData")
			.and_then(|p| p.get("openaiRemoteCompaction"))
			.is_some(),
		true
	);

	let mut tool_calls = std::collections::HashSet::new();
	let mut tool_results = std::collections::HashSet::new();
	for msg in loaded.messages() {
		if msg.get("role").and_then(Value::as_str) == Some("assistant") {
			if let Some(arr) = msg.get("content").and_then(Value::as_array) {
				for block in arr {
					if block.get("type").and_then(Value::as_str) == Some("toolCall") {
						if let Some(call_id) = block.get("id").and_then(Value::as_str) {
							tool_calls.insert(call_id.to_string());
						}
					}
				}
			}
		} else if msg.get("role").and_then(Value::as_str) == Some("toolResult") {
			if let Some(call_id) = msg.get("toolCallId").and_then(Value::as_str) {
				tool_results.insert(call_id.to_string());
			}
		}
	}

	assert!(tool_calls.contains("call-codex"), "missing call-codex");
	assert!(tool_calls.contains("call-web"), "missing call-web");
	assert!(tool_results.contains("call-codex"), "missing result for call-codex");
	assert!(tool_results.contains("call-web"), "missing result for call-web");
}

#[test]
fn test_codex_image_preservation() {
	let temp = TestDir::new("codex-img");
	let root = temp.path().join(".codex");
	let id = "55555555-5555-5555-5555-555555555555";
	let session_path = root.join("sessions").join(format!("rollout-{id}.jsonl"));

	write_jsonl(
		&session_path,
		&[
			json!({ "type": "session_meta", "timestamp": "2025-01-01T00:00:00.000Z", "payload": { "id": id, "cwd": "/tmp" } }),
			json!({
				"type": "response_item",
				"timestamp": "2025-01-01T00:00:01.000Z",
				"payload": {
					"type": "message",
					"role": "user",
					"content": [
						{ "type": "input_text", "text": "Photo:" },
						{ "type": "input_image", "image_url": "data:image/jpeg;base64,12345base64==", "detail": "high" }
					]
				}
			}),
		],
	);

	let store = CodexSessionStore::new(&root);
	let info = ForeignSessionInfo {
		source: ForeignSessionSource::Codex,
		id: id.to_string(),
		path: session_path,
		cwd: PathBuf::from("/tmp"),
		title: None,
		created_ms: 0,
		modified_ms: 0,
		message_count: None,
		first_message: None,
	};

	let loaded = store.load(&info).expect("load codex image session");
	let messages = loaded.messages();
	assert_eq!(messages.len(), 1);
	let content = messages[0]
		.get("content")
		.and_then(Value::as_array)
		.unwrap();
	let img = content
		.iter()
		.find(|b| b.get("type").and_then(Value::as_str) == Some("image"))
		.unwrap();
	assert_eq!(img.get("mimeType").and_then(Value::as_str), Some("image/jpeg"));
	assert_eq!(img.get("data").and_then(Value::as_str), Some("12345base64=="));
	assert_eq!(img.get("detail").and_then(Value::as_str), Some("high"));
}

#[test]
fn test_provenance_entry_format() {
	let info = ForeignSessionInfo {
		source: ForeignSessionSource::Claude,
		id: "src-123".to_string(),
		path: PathBuf::from("/home/user/.claude/projects/foo/src-123.jsonl"),
		cwd: PathBuf::from("/home/user/project"),
		title: Some("Title".to_string()),
		created_ms: 1_767_225_600_000,
		modified_ms: 1_767_225_600_000,
		message_count: Some(5),
		first_message: Some("Hello".to_string()),
	};

	let loaded = LoadedForeignSession {
		source: ForeignSessionSource::Claude,
		id: info.id.clone(),
		cwd: info.cwd.clone(),
		title: info.title.clone(),
		entries: vec![],
	};

	let provenance = loaded.provenance_entry(&info);
	assert_eq!(provenance.get("type").and_then(Value::as_str), Some("custom"));
	assert_eq!(provenance.get("customType").and_then(Value::as_str), Some("foreign_session_import"));
	assert_eq!(provenance.get("id").and_then(Value::as_str), Some("import-src-123"));
	assert_eq!(
		provenance.get("timestamp").and_then(Value::as_str),
		Some("2026-01-01T00:00:00.000Z")
	);

	let prov_entry: omp_cli::session::SessionEntry = serde_json::from_value(provenance.clone())
		.expect("provenance entry must deserialize to SessionEntry");
	assert!(prov_entry.validate().is_ok(), "provenance entry must pass validate()");

	let data = provenance.get("data").and_then(Value::as_object).unwrap();
	assert_eq!(data.get("source").and_then(Value::as_str), Some("claude"));
	assert_eq!(data.get("sourceId").and_then(Value::as_str), Some("src-123"));
	assert_eq!(
		data.get("sourcePath").and_then(Value::as_str),
		Some("/home/user/.claude/projects/foo/src-123.jsonl")
	);
	assert_eq!(data.get("sourceCwd").and_then(Value::as_str), Some("/home/user/project"));
}

#[test]
fn test_session_entry_validation_requires_id_and_timestamp() {
	let valid = omp_cli::session::SessionEntry {
		kind: "custom".into(),
		id: "entry-1".into(),
		parent_id: None,
		timestamp: "2026-10-26T12:00:00.000Z".into(),
		extra: serde_json::json!({}),
	};
	assert!(valid.validate().is_ok());

	let missing_id = omp_cli::session::SessionEntry {
		kind: "custom".into(),
		id: String::new(),
		parent_id: None,
		timestamp: "2026-10-26T12:00:00.000Z".into(),
		extra: serde_json::json!({}),
	};
	assert!(missing_id.validate().is_err());

	let missing_ts = omp_cli::session::SessionEntry {
		kind: "custom".into(),
		id: "entry-1".into(),
		parent_id: None,
		timestamp: String::new(),
		extra: serde_json::json!({}),
	};
	assert!(missing_ts.validate().is_err());
}
