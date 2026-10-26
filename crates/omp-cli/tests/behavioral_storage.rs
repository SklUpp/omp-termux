use std::fs;
use std::path::PathBuf;

use omp_cli::session::{SessionOptions, SessionStore, TurnRecord, import_legacy_v1};

static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn temp_dir(prefix: &str) -> PathBuf {
	let unique = format!(
		"omp-behavioral-{prefix}-{}-{}",
		std::process::id(),
		COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
	);
	let dir = std::env::temp_dir().join(unique);
	fs::create_dir_all(&dir).expect("create temp dir");
	dir
}

#[test]
fn read_real_ts_v3_session_and_preserve_typed_content() {
	let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
		.join("tests/fixtures/v3_session.jsonl");
	assert!(fixture_path.is_file(), "fixture must exist");

	let temp = temp_dir("v3");
	let test_file = temp.join("session.jsonl");
	fs::copy(&fixture_path, &test_file).expect("copy fixture");

	let mut store = SessionStore::open(&SessionOptions {
		enabled: true,
		cwd: PathBuf::from("/work/test-project"),
		model: Some("claude-opus-4-5".to_string()),
		provider: Some("anthropic".to_string()),
		dir: Some(temp.clone()),
		id: Some("session".to_string()),
		continue_latest: false,
	});

	assert!(store.enabled(), "store must be enabled when reading TS v3 session");
	let history = store.history();
	assert_eq!(history.len(), 1, "history must contain 1 turn from v3 session");
	assert_eq!(history[0].prompt, "hello from test user");
	assert_eq!(history[0].assistant, "calling tool");

	// P1: Assert image content is preserved in typed session entries
	assert!(store.has_image_content(), "must preserve image content block");

	// P1: Append turn and assert required TS message fields and numeric timestamps are written
	let turn = TurnRecord {
		prompt: "check image".to_string(),
		assistant: "looks good".to_string(),
		stop_reason: "aborted".to_string(),
		tool_calls: Vec::new(),
	};
	store.append_turn(&turn).expect("append turn");

	let content = fs::read_to_string(&test_file).expect("read appended file");
	let lines: Vec<&str> = content.lines().collect();
	assert!(lines.len() >= 4);

	// Check user message line
	let user_line: serde_json::Value = serde_json::from_str(lines[lines.len() - 2]).expect("user json");
	assert_eq!(user_line["type"], "message");
	assert_eq!(user_line["message"]["role"], "user");
	assert!(user_line["message"]["timestamp"].is_number(), "must have numeric timestamp");

	// Check assistant message line
	let asst_line: serde_json::Value = serde_json::from_str(lines[lines.len() - 1]).expect("asst json");
	assert_eq!(asst_line["type"], "message");
	let asst_msg = &asst_line["message"];
	assert_eq!(asst_msg["role"], "assistant");
	assert_eq!(asst_msg["provider"], "anthropic");
	assert_eq!(asst_msg["model"], "claude-opus-4-5");
	// TurnRecord carries no route/usage: storage omits them, never invents.
	assert!(asst_msg.get("api").is_none(), "api unknown from TurnRecord — must be absent");
	assert!(asst_msg.get("usage").is_none(), "usage unavailable — must be absent, never zero-filled");
	assert_eq!(asst_msg["stopReason"], "aborted");
	assert_eq!(asst_msg["errorMessage"], "Interrupted by user");
	assert!(asst_msg["timestamp"].is_number(), "must have numeric timestamp");

	fs::remove_dir_all(temp).ok();
}

#[test]
fn v1_legacy_log_rejects_append_and_stays_immutable() {
	let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
		.join("tests/fixtures/v1_session.jsonl");
	assert!(fixture_path.is_file(), "fixture must exist");

	let temp = temp_dir("v1-immutable");
	let test_file = temp.join("legacy.jsonl");
	fs::copy(&fixture_path, &test_file).expect("copy fixture");
	let original = fs::read_to_string(&test_file).expect("read original");

	let mut store = SessionStore::open(&SessionOptions {
		enabled: true,
		cwd: PathBuf::from("/work/legacy"),
		model: None,
		provider: None,
		dir: Some(temp.clone()),
		id: Some("legacy".to_string()),
		continue_latest: false,
	});

	assert!(store.enabled(), "store can open legacy log for reading");
	assert_eq!(store.history().len(), 1);

	let new_turn = TurnRecord {
		prompt: "do more work".to_string(),
		assistant: "done".to_string(),
		stop_reason: "end_turn".to_string(),
		tool_calls: Vec::new(),
	};

	let result = store.append_turn(&new_turn);
	assert!(result.is_err(), "append_turn on legacy v1 session must be rejected");

	let current = fs::read_to_string(&test_file).expect("read after append");
	assert_eq!(original, current, "legacy v1 file must never be mutated on disk");

	fs::remove_dir_all(temp).ok();
}

#[test]
fn malformed_tail_refuses_append() {
	let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
		.join("tests/fixtures/malformed_tail.jsonl");
	assert!(fixture_path.is_file(), "fixture must exist");

	let temp = temp_dir("bad-tail");
	let test_file = temp.join("bad_tail.jsonl");
	fs::copy(&fixture_path, &test_file).expect("copy fixture");
	let original = fs::read_to_string(&test_file).expect("read original");

	let mut store = SessionStore::open(&SessionOptions {
		enabled: true,
		cwd: PathBuf::from("/work"),
		model: None,
		provider: None,
		dir: Some(temp.clone()),
		id: Some("bad_tail".to_string()),
		continue_latest: false,
	});

	assert!(!store.enabled(), "store must disable persistence on malformed/unclosed tail");
	let warnings = store.take_warnings();
	assert!(
		warnings.iter().any(|w| w.contains("incomplete") || w.contains("unclosed") || w.contains("malformed")),
		"must report warning about incomplete tail: {warnings:?}"
	);

	let turn = TurnRecord {
		prompt: "hi".into(),
		assistant: "hello".into(),
		stop_reason: "end_turn".into(),
		tool_calls: vec![],
	};
	let _ = store.append_turn(&turn);

	assert_eq!(
		fs::read_to_string(&test_file).expect("read after append attempt"),
		original,
		"corrupted file must never be appended to"
	);

	fs::remove_dir_all(temp).ok();
}

#[test]
fn import_legacy_v1_refuses_overwrite() {
	let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
		.join("tests/fixtures/v1_session.jsonl");
	assert!(fixture_path.is_file(), "fixture must exist");

	let temp = temp_dir("import-overwrite");
	let v1_file = temp.join("source.jsonl");
	fs::copy(&fixture_path, &v1_file).expect("copy fixture");

	let target_dir = temp.join("v3-target");
	fs::create_dir_all(&target_dir).expect("create target dir");

	// First import succeeds
	let first_path = import_legacy_v1(&v1_file, &target_dir, None)
		.expect("first import must succeed");
	assert!(first_path.is_file());

	// Second import must fail with overwrite error
	let second_result = import_legacy_v1(&v1_file, &target_dir, None);
	assert!(
		second_result.is_err(),
		"second import to same target directory must refuse to overwrite"
	);
	assert!(
		second_result.unwrap_err().contains("refusing to overwrite"),
		"error message must state refusing to overwrite"
	);

	fs::remove_dir_all(temp).ok();
}
