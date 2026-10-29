use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

use omp_cli::output;
use omp_cli::session::{
	ContentPart, MessageContent, MessageEntry, SessionMessage, SessionOptions, SessionStore,
	TurnRecord, create_session_log_exclusive, import_legacy_v1, sanitize_terminal_id,
};

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
		parent_session: None,
		no_title: false,
	});

	assert!(store.enabled(), "store must be enabled when reading TS v3 session");
	let history = store.history();
	assert_eq!(history.len(), 1, "history must contain 1 turn from v3 session");
	assert_eq!(history[0].prompt, "hello from test user");
	assert_eq!(history[0].assistant, "calling tool");
	assert_eq!(history[0].images().len(), 1, "history turn must preserve user image parts from TS fixture");
	assert_eq!(history[0].images()[0].mime_type, "image/png");
	assert_eq!(history[0].images()[0].data_base64, "aGVsbG8=");
	assert_eq!(history[0].parts.len(), 2, "history turn must preserve all ordered parts [text, image]");
	// P1: Assert image content is preserved in typed session entries
	assert!(store.has_image_content(), "must preserve image content block");

	// P1: Append turn and assert required TS message fields and numeric timestamps are written
	let turn = TurnRecord {
		prompt: "check image".to_string(),
		assistant: "looks good".to_string(),
		stop_reason: "aborted".to_string(),
		tool_calls: Vec::new(),
		parts: Vec::new(),
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
		parent_session: None,
		no_title: false,
	});

	assert!(store.enabled(), "store can open legacy log for reading");
	assert_eq!(store.history().len(), 1);

	let new_turn = TurnRecord {
		prompt: "do more work".to_string(),
		assistant: "done".to_string(),
		stop_reason: "end_turn".to_string(),
		tool_calls: Vec::new(),
		parts: Vec::new(),
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
	let original_bytes = fs::read(&test_file).expect("read original");

	let mut store = SessionStore::open(&SessionOptions {
		enabled: true,
		cwd: PathBuf::from("/work"),
		model: None,
		provider: None,
		dir: Some(temp.clone()),
		id: Some("bad_tail".to_string()),
		continue_latest: false,
		parent_session: None,
		no_title: false,
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
		tool_calls: Vec::new(),
		parts: Vec::new(),
	};
	let result = store.append_turn(&turn);
	assert!(result.is_err(), "append_turn on malformed tail session must return Err");
	let err = result.unwrap_err();
	assert!(
		err.contains("incomplete/unclosed tail record"),
		"append error must state exact cause: {err}"
	);

	assert_eq!(
		fs::read(&test_file).expect("read after append attempt"),
		original_bytes,
		"corrupted file must remain byte-exact unchanged"
	);

	fs::remove_dir_all(temp).ok();
}

#[test]
fn malformed_entry_refuses_append() {
	let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
		.join("tests/fixtures/malformed_entry.jsonl");
	assert!(fixture_path.is_file(), "fixture must exist");

	let temp = temp_dir("bad-entry");
	let test_file = temp.join("bad_entry.jsonl");
	fs::copy(&fixture_path, &test_file).expect("copy fixture");
	let original_bytes = fs::read(&test_file).expect("read original");

	let mut store = SessionStore::open(&SessionOptions {
		enabled: true,
		cwd: PathBuf::from("/work/test"),
		model: None,
		provider: None,
		dir: Some(temp.clone()),
		id: Some("bad_entry".to_string()),
		continue_latest: false,
		parent_session: None,
		no_title: false,
	});

	assert!(!store.enabled(), "store must disable persistence on malformed entry");
	let warnings = store.take_warnings();
	assert!(
		warnings.iter().any(|w| w.contains("malformed") || w.contains("corrupt")),
		"must report warning about malformed entry: {warnings:?}"
	);

	let turn = TurnRecord {
		prompt: "hi".into(),
		assistant: "hello".into(),
		stop_reason: "end_turn".into(),
		tool_calls: Vec::new(),
		parts: Vec::new(),
	};
	let result = store.append_turn(&turn);
	assert!(result.is_err(), "append_turn on malformed entry session must return Err");
	let err = result.unwrap_err();
	assert!(
		err.contains("malformed record"),
		"append error must state exact cause: {err}"
	);

	assert_eq!(
		fs::read(&test_file).expect("read after append attempt"),
		original_bytes,
		"corrupted file must remain byte-exact unchanged"
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

#[test]
fn session_create_new_collision_refuses_overwrite_and_leaves_victim_intact() {
	let temp = temp_dir("collision-victim");
	let victim_content = b"CRITICAL VICTIM DATA MUST REMAIN UNTOUCHED";

	// Direct exclusive-create unit test (exercises O_EXCL immutability without id-stem scan)
	let direct_victim = temp.join("direct_victim.jsonl");
	fs::write(&direct_victim, victim_content).expect("write direct victim");
	let direct_err = create_session_log_exclusive(&direct_victim, b"OVERWRITE ATTEMPT")
		.expect_err("create_session_log_exclusive must fail on existing file");
	assert_eq!(direct_err.kind(), std::io::ErrorKind::AlreadyExists);
	assert_eq!(
		fs::read(&direct_victim).expect("read direct victim"),
		victim_content,
		"direct victim file must be byte-exact unmodified"
	);

	#[cfg(unix)]
	{
		let direct_sym_victim = temp.join("direct_sym_victim.txt");
		fs::write(&direct_sym_victim, victim_content).expect("write sym victim");
		let direct_symlink = temp.join("direct_symlink.jsonl");
		std::os::unix::fs::symlink(&direct_sym_victim, &direct_symlink).expect("create symlink");

		let sym_err = create_session_log_exclusive(&direct_symlink, b"OVERWRITE ATTEMPT")
			.expect_err("create_session_log_exclusive must fail on existing symlink");
		assert_eq!(sym_err.kind(), std::io::ErrorKind::AlreadyExists);
		assert_eq!(
			fs::read(&direct_sym_victim).expect("read direct sym victim"),
			victim_content,
			"symlink target file must be byte-exact unmodified"
		);
	}

	// 1. Existing regular file collision
	let now_iso = omp_cli::output::iso8601(std::time::SystemTime::now());
	let ts = omp_cli::session::file_safe_timestamp(&now_iso);
	let target_filename = format!("{ts}_collision_target.jsonl");
	let victim_file = temp.join(&target_filename);
	fs::write(&victim_file, victim_content).expect("write regular victim");

	let store = SessionStore::open(&SessionOptions {
		enabled: true,
		cwd: PathBuf::from("/work"),
		model: None,
		provider: None,
		dir: Some(temp.clone()),
		id: Some("collision_target".to_string()),
		continue_latest: false,
		parent_session: None,
		no_title: false,
	});

	assert!(!store.enabled(), "store must be disabled on collision");
	assert_eq!(
		fs::read(&victim_file).expect("read victim file"),
		victim_content,
		"victim file must never be overwritten or truncated on collision"
	);

	// 2. Symlink collision
	#[cfg(unix)]
	{
		let symlink_victim = temp.join("symlink_victim_file.txt");
		let symlink_target_bytes = b"SENSITIVE SYMLINK TARGET NEVER TRUNCATE";
		fs::write(&symlink_victim, symlink_target_bytes).expect("write symlink victim");

		let symlink_path = temp.join(format!("{ts}_symlink_target.jsonl"));
		std::os::unix::fs::symlink(&symlink_victim, &symlink_path).expect("create symlink");

		let symlink_store = SessionStore::open(&SessionOptions {
			enabled: true,
			cwd: PathBuf::from("/work"),
			model: None,
			provider: None,
			dir: Some(temp.clone()),
			id: Some("symlink_target".to_string()),
			continue_latest: false,
			parent_session: None,
			no_title: false,
		});

		assert!(!symlink_store.enabled(), "store must be disabled on symlink collision");
		assert_eq!(
			fs::read(&symlink_victim).expect("read symlink target"),
			symlink_target_bytes,
			"symlink victim must remain untouched"
		);
	}

	fs::remove_dir_all(temp).ok();
}

#[test]
fn terminal_id_env_traversal_is_rejected_or_sanitized() {
	// Traversal sequences must be rejected
	assert!(sanitize_terminal_id("../../../etc/passwd").is_none());
	assert!(sanitize_terminal_id("..").is_none());
	assert!(sanitize_terminal_id(".").is_none());
	assert!(sanitize_terminal_id("").is_none());
	assert!(sanitize_terminal_id("   ").is_none());
	assert!(sanitize_terminal_id("foo/../bar").is_none());
	assert!(sanitize_terminal_id("foo\\..\\bar").is_none());
	assert!(sanitize_terminal_id("pane/\0/bad").is_none());

	// Safe single-component names must be accepted
	assert_eq!(sanitize_terminal_id("tmux-%0").as_deref(), Some("tmux-%0"));
	assert_eq!(sanitize_terminal_id("zellij-sess-1").as_deref(), Some("zellij-sess-1"));
	assert_eq!(sanitize_terminal_id("kitty-123").as_deref(), Some("kitty-123"));

	// Slashes without traversal are sanitized to a single component
	assert_eq!(sanitize_terminal_id("session/1").as_deref(), Some("session-1"));
	assert_eq!(sanitize_terminal_id("session\\2").as_deref(), Some("session-2"));
}

#[test]
fn parse_error_does_not_leak_raw_line_secrets() {
	let temp = temp_dir("secret-leak-syntax");
	let bad_file = temp.join("s-secret-1.jsonl");
	let secret_marker = "SUPER_SECRET_TOKEN_DO_NOT_LEAK_IN_DIAGNOSTICS_999";
	// Truly invalid JSON syntax with secret
	let content = format!(
		"{{\"type\":\"session\",\"version\":3,\"id\":\"s-secret-1\",\"timestamp\":\"2026-10-26T10:00:00.000Z\",\"cwd\":\"/work\"}}\n\
		 {{NOT_VALID_JSON_SYNTAX_WITH_SECRET: \"{secret_marker}\"}}\n"
	);
	fs::write(&bad_file, &content).expect("write bad file");

	let mut store = SessionStore::open(&SessionOptions {
		enabled: true,
		cwd: PathBuf::from("/work"),
		model: None,
		provider: None,
		dir: Some(temp.clone()),
		id: Some("s-secret-1".to_string()),
		continue_latest: false,
		parent_session: None,
		no_title: false,
	});

	assert!(!store.enabled());
	let warnings = store.take_warnings();
	assert!(!warnings.is_empty(), "must have warnings on malformed log");
	for warning in &warnings {
		assert!(
			!warning.contains(secret_marker),
			"warning message must never leak raw secret content: {warning}"
		);
	}

	let turn = TurnRecord {
		prompt: "hi".into(),
		assistant: "hello".into(),
		stop_reason: "end_turn".into(),
		tool_calls: Vec::new(),
		parts: Vec::new(),
	};
	let append_res = store.append_turn(&turn);
	assert!(append_res.is_err());
	let err = append_res.unwrap_err();
	assert!(
		!err.contains(secret_marker),
		"append error must never leak raw secret content: {err}"
	);

	fs::remove_dir_all(temp).ok();
}

#[test]
fn parse_error_field_type_mismatch_does_not_leak_field_value_secret() {
	let temp = temp_dir("secret-leak-type-mismatch");
	let bad_file = temp.join("s-secret-2.jsonl");
	let secret_field_val = "SUPER_SECRET_FIELD_VALUE_NEVER_LEAK_12345";
	// Valid JSON, but message timestamp is string instead of numeric ms
	let content = format!(
		"{{\"type\":\"session\",\"version\":3,\"id\":\"s-secret-2\",\"timestamp\":\"2026-10-26T10:00:00.000Z\",\"cwd\":\"/work\"}}\n\
		 {{\"type\":\"message\",\"id\":\"msg-1\",\"parentId\":null,\"timestamp\":\"2026-10-26T10:00:01.000Z\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"hi\"}}],\"timestamp\":\"{secret_field_val}\"}}}}\n"
	);
	fs::write(&bad_file, &content).expect("write bad file");

	let mut store = SessionStore::open(&SessionOptions {
		enabled: true,
		cwd: PathBuf::from("/work"),
		model: None,
		provider: None,
		dir: Some(temp.clone()),
		id: Some("s-secret-2".to_string()),
		continue_latest: false,
		parent_session: None,
		no_title: false,
	});

	assert!(!store.enabled(), "store must disable persistence on schema error");
	let warnings = store.take_warnings();
	assert!(!warnings.is_empty(), "must have warnings on malformed entry");
	for warning in &warnings {
		assert!(
			!warning.contains(secret_field_val),
			"warning message must never leak field value secret: {warning}"
		);
	}

	let turn = TurnRecord {
		prompt: "hi".into(),
		assistant: "hello".into(),
		stop_reason: "end_turn".into(),
		tool_calls: Vec::new(),
		parts: Vec::new(),
	};
	let append_res = store.append_turn(&turn);
	assert!(append_res.is_err());
	let err = append_res.unwrap_err();
	assert!(
		!err.contains(secret_field_val),
		"append error must never leak field value secret: {err}"
	);

	fs::remove_dir_all(temp).ok();
}

#[test]
fn parse_error_untyped_record_is_rejected_without_secret_leak() {
	let temp = temp_dir("secret-leak-untyped");
	let bad_file = temp.join("s-secret-3.jsonl");
	let secret_marker = "SUPER_SECRET_IN_UNTYPED_RECORD_777";
	// Valid JSON, but missing record type discriminator
	let content = format!(
		"{{\"type\":\"session\",\"version\":3,\"id\":\"s-secret-3\",\"timestamp\":\"2026-10-26T10:00:00.000Z\",\"cwd\":\"/work\"}}\n\
		 {{\"untyped_data\": true, \"secret\": \"{secret_marker}\"}}\n"
	);
	fs::write(&bad_file, &content).expect("write bad file");

	let mut store = SessionStore::open(&SessionOptions {
		enabled: true,
		cwd: PathBuf::from("/work"),
		model: None,
		provider: None,
		dir: Some(temp.clone()),
		id: Some("s-secret-3".to_string()),
		continue_latest: false,
		parent_session: None,
		no_title: false,
	});

	assert!(!store.enabled(), "store must disable persistence on untyped record");
	let warnings = store.take_warnings();
	assert!(!warnings.is_empty(), "must have warnings on untyped record");
	for warning in &warnings {
		assert!(
			!warning.contains(secret_marker),
			"warning message must never leak secret: {warning}"
		);
	}

	let turn = TurnRecord {
		prompt: "hi".into(),
		assistant: "hello".into(),
		stop_reason: "end_turn".into(),
		tool_calls: Vec::new(),
		parts: Vec::new(),
	};
	let append_res = store.append_turn(&turn);
	assert!(append_res.is_err());
	let err = append_res.unwrap_err();
	assert!(
		!err.contains(secret_marker),
		"append error must never leak secret: {err}"
	);

	fs::remove_dir_all(temp).ok();
}

#[test]
fn parse_v1_log_field_type_mismatch_does_not_leak_field_value_secret() {
	let temp = temp_dir("secret-leak-v1");
	let bad_file = temp.join("s-secret-v1.jsonl");
	let secret_marker = "SUPER_SECRET_V1_PAYLOAD_DO_NOT_LEAK";
	// V1 format session with a turn record whose tool_calls is an invalid string type containing secret
	let content = format!(
		"{{\"type\":\"header\",\"version\":1,\"id\":\"s-secret-v1\",\"timestamp\":\"2026-10-26T10:00:00.000Z\",\"cwd\":\"/work\"}}\n\
		 {{\"type\":\"turn\",\"prompt\":\"hi\",\"assistant\":\"there\",\"stop_reason\":\"end_turn\",\"tool_calls\":\"{secret_marker}\"}}\n"
	);
	fs::write(&bad_file, &content).expect("write bad v1 file");

	// 1. SessionStore::open on malformed v1
	let mut store = SessionStore::open(&SessionOptions {
		enabled: true,
		cwd: PathBuf::from("/work"),
		model: None,
		provider: None,
		dir: Some(temp.clone()),
		id: Some("s-secret-v1".to_string()),
		continue_latest: false,
		parent_session: None,
		no_title: false,
	});

	assert!(!store.enabled(), "store must disable persistence on malformed v1 log");
	let warnings = store.take_warnings();
	assert!(!warnings.is_empty(), "must have warnings on malformed v1 log");
	for warning in &warnings {
		assert!(
			!warning.contains(secret_marker),
			"warning message must never leak v1 secret: {warning}"
		);
	}

	let turn = TurnRecord {
		prompt: "hi".into(),
		assistant: "hello".into(),
		stop_reason: "end_turn".into(),
		tool_calls: Vec::new(),
		parts: Vec::new(),
	};
	let append_res = store.append_turn(&turn);
	assert!(append_res.is_err());
	let err = append_res.unwrap_err();
	assert!(
		!err.contains(secret_marker),
		"append error must never leak v1 secret: {err}"
	);

	// 2. import_legacy_v1 on malformed v1
	let import_dest = temp.join("imported_target");
	let import_res = import_legacy_v1(&bad_file, &import_dest, None);
	assert!(import_res.is_err(), "import_legacy_v1 must fail on malformed v1 log");
	let import_err = import_res.unwrap_err();
	assert!(
		!import_err.contains(secret_marker),
		"import error must never leak v1 secret: {import_err}"
	);

	fs::remove_dir_all(temp).ok();
}

#[test]
fn raw_content_with_secret_does_not_leak_into_diagnostics_or_warnings() {
	let temp = temp_dir("secret-leak-raw-content");
	let bad_file = temp.join("s-secret-raw.jsonl");
	let secret_marker = "SUPER_SECRET_RAW_TOKEN_DO_NOT_LEAK_IN_DIAGNOSTICS_12345";
	let content = format!(
		"{{\"type\":\"session\",\"version\":3,\"id\":\"s-secret-raw\",\"timestamp\":\"2026-10-26T10:00:00.000Z\",\"cwd\":\"/work\"}}\n\
		 {{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-10-26T10:00:01.000Z\",\"message\":{{\"role\":\"user\",\"content\":{{\"custom_field\":\"{secret_marker}\"}}}}}}\n"
	);
	fs::write(&bad_file, &content).expect("write bad file");

	let mut store = SessionStore::open(&SessionOptions {
		enabled: true,
		cwd: PathBuf::from("/work"),
		model: None,
		provider: None,
		dir: Some(temp.clone()),
		id: Some("s-secret-raw".to_string()),
		continue_latest: false,
		parent_session: None,
		no_title: false,
	});

	assert!(!store.enabled());
	let reason = store.corrupt_reason().expect("corrupt_reason");
	assert!(
		!reason.contains(secret_marker),
		"corrupt_reason must never leak secret: {reason}"
	);
	for warning in store.take_warnings() {
		assert!(
			!warning.contains(secret_marker),
			"warning message must never leak secret: {warning}"
		);
	}

	let turn = TurnRecord {
		prompt: "hi".to_string(),
		assistant: "hello".to_string(),
		stop_reason: "stop".to_string(),
		tool_calls: vec![],
		parts: vec![],
	};
	let err = store.append_turn(&turn).unwrap_err();
	assert!(
		!err.contains(secret_marker),
		"append error must never leak secret: {err}"
	);

	fs::remove_dir_all(temp).ok();
}

#[test]
fn deserialize_turn_parts_does_not_leak_secret() {
	let secret_marker = "SUPER_SECRET_TURN_RECORD_PART_PAYLOAD_999";
	let raw_json = format!(
		"{{\"prompt\":\"hello\",\"parts\":[{{\"unsupported_plugin_block\":\"{secret_marker}\"}}]}}"
	);
	let err = serde_json::from_str::<TurnRecord>(&raw_json).unwrap_err().to_string();
	assert!(
		!err.contains(secret_marker),
		"TurnRecord deserialization error must never leak secret payload: {err}"
	);
	assert!(err.contains("index 0"), "error should identify part position: {err}");
}

#[test]
fn deserialize_turn_parts_with_unknown_type_does_not_leak_secret() {
	let secret_marker = "SUPER_SECRET_TURN_RECORD_PART_TYPE_888";
	let raw_json = format!(
		"{{\"prompt\":\"hello\",\"parts\":[{{\"type\":\"{secret_marker}\"}}]}}"
	);
	let err = serde_json::from_str::<TurnRecord>(&raw_json).unwrap_err().to_string();
	assert!(
		!err.contains(secret_marker),
		"TurnRecord deserialization error must never leak secret part type: {err}"
	);
	assert!(
		err.contains("unsupported content part kind in turn record at index 0"),
		"must match branch at session.rs:608: {err}"
	);
}

#[test]
fn part_type_secret_does_not_leak_into_diagnostics_or_warnings() {
	let temp = temp_dir("secret-leak-part-type");
	let bad_file = temp.join("s-secret-part.jsonl");
	let secret_marker = "SUPER_SECRET_PART_KIND_TOKEN_999";
	let content = format!(
		"{{\"type\":\"session\",\"version\":3,\"id\":\"s-secret-part\",\"timestamp\":\"2026-10-26T10:00:00.000Z\",\"cwd\":\"/work\"}}\n\
		 {{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-10-26T10:00:01.000Z\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"{secret_marker}\"}}]}}}}\n"
	);
	fs::write(&bad_file, &content).expect("write bad file");

	let mut store = SessionStore::open(&SessionOptions {
		enabled: true,
		cwd: PathBuf::from("/work"),
		model: None,
		provider: None,
		dir: Some(temp.clone()),
		id: Some("s-secret-part".to_string()),
		continue_latest: false,
		parent_session: None,
		no_title: false,
	});

	assert!(!store.enabled());
	let reason = store.corrupt_reason().expect("corrupt_reason");
	assert!(
		!reason.contains(secret_marker),
		"corrupt_reason must never leak secret part kind: {reason}"
	);
	for warning in store.take_warnings() {
		assert!(
			!warning.contains(secret_marker),
			"warning message must never leak secret part kind: {warning}"
		);
	}

	fs::remove_dir_all(temp).ok();
}

#[test]
fn typed_part_with_unknown_type_and_image_fields_is_rejected_without_fallback() {
	let raw_json = r#"{
		"prompt": "hello",
		"parts": [
			{"type": "future_video", "data": "aW1n", "mimeType": "image/png"}
		]
	}"#;
	let err = serde_json::from_str::<TurnRecord>(raw_json).unwrap_err().to_string();
	assert!(
		err.contains("unsupported content part kind in turn record at index 0"),
		"typed unknown kind must be rejected without fallback to ImagePart: {err}"
	);
	assert!(!err.contains("future_video"), "error must not leak kind value: {err}");

	// Also verify pi_agent::Message rejection
	let msg_json = r#"{
		"role": "user",
		"parts": [
			{"type": "future_video", "data": "aW1n", "mimeType": "image/png"}
		]
	}"#;
	let msg_err = serde_json::from_str::<pi_agent::Message>(msg_json).unwrap_err().to_string();
	assert!(
		msg_err.contains("unsupported message content part at index 0"),
		"pi_agent::Message must reject unknown typed part without fallback: {msg_err}"
	);
}

#[test]
fn typed_part_with_malformed_text_and_image_fields_is_rejected_without_fallback() {
	let raw_json = r#"{
		"prompt": "hello",
		"parts": [
			{"type": "text", "data": "aW1n", "mimeType": "image/png"}
		]
	}"#;
	let err = serde_json::from_str::<TurnRecord>(raw_json).unwrap_err().to_string();
	assert!(
		err.contains("malformed text part in turn record at index 0: missing text"),
		"malformed text part must be rejected without fallback to ImagePart: {err}"
	);

	// Also verify pi_agent::Message rejection
	let msg_json = r#"{
		"role": "user",
		"parts": [
			{"type": "text", "data": "aW1n", "mimeType": "image/png"}
		]
	}"#;
	let msg_err = serde_json::from_str::<pi_agent::Message>(msg_json).unwrap_err().to_string();
	assert!(
		msg_err.contains("unsupported message content part at index 0"),
		"pi_agent::Message must reject malformed text part without fallback: {msg_err}"
	);
}

#[test]
fn session_store_open_with_malformed_text_and_image_disables_store_and_preserves_file() {
	let temp = temp_dir("open-malformed-text-and-image");
	let bad_file = temp.join("s-malformed-text.jsonl");
	let content = format!(
		"{{\"type\":\"session\",\"version\":3,\"id\":\"s-malformed-text\",\"timestamp\":\"2026-10-26T10:00:00.000Z\",\"cwd\":\"/work\"}}\n\
		 {{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-10-26T10:00:01.000Z\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"image\",\"data\":\"aW1n\",\"mimeType\":\"image/png\"}},{{\"type\":\"text\"}}]}}}}\n"
	);
	fs::write(&bad_file, &content).expect("write bad file");

	let file_content_before = fs::read_to_string(&bad_file).expect("read before");
	let file_size_before = fs::metadata(&bad_file).expect("metadata before").len();

	let mut store = SessionStore::open(&SessionOptions {
		enabled: true,
		cwd: PathBuf::from("/work"),
		model: None,
		provider: None,
		dir: Some(temp.clone()),
		id: Some("s-malformed-text".to_string()),
		continue_latest: false,
		parent_session: None,
		no_title: false,
	});

	assert!(!store.enabled(), "store must be disabled on malformed text part in log");
	let reason = store.corrupt_reason().expect("corrupt_reason");
	assert!(
		reason.contains("missing text"),
		"corrupt_reason must mention missing text: {reason}"
	);

	let file_content_after = fs::read_to_string(&bad_file).expect("read after");
	assert_eq!(file_content_before, file_content_after, "session file must remain 100% intact");
	let file_size_after = fs::metadata(&bad_file).expect("metadata after").len();
	assert_eq!(file_size_before, file_size_after, "session file size must not change");

	fs::remove_dir_all(temp).ok();
}

#[test]
fn direct_append_message_with_malformed_text_and_image_fails_preflight_and_preserves_file_and_memory() {
	let temp = temp_dir("append-malformed-text-and-image");
	let mut store = SessionStore::open(&SessionOptions {
		enabled: true,
		cwd: PathBuf::from("/work"),
		model: None,
		provider: None,
		dir: Some(temp.clone()),
		id: Some("s-direct-malformed-text".to_string()),
		continue_latest: false,
		parent_session: None,
		no_title: false,
	});
	assert!(store.enabled());

	// Initial baseline turn
	let initial_turn = TurnRecord {
		prompt: "initial turn".to_string(),
		assistant: "initial reply".to_string(),
		stop_reason: "stop".to_string(),
		tool_calls: vec![],
		parts: vec![],
	};
	store.append_turn(&initial_turn).expect("append initial turn");
	let file = store.file().expect("file").to_path_buf();

	let file_content_before = fs::read_to_string(&file).expect("read before");
	let file_size_before = fs::metadata(&file).expect("metadata before").len();
	let messages_len_before = store.messages().len();
	let entries_len_before = store.entries().len();
	let history_len_before = store.history().len();

	// Direct MessageEntry with parts: valid image + malformed text (text: None)
	let bad_entry = MessageEntry::new(
		"msg-bad-text".to_string(),
		store.messages().last().map(|m| m.id.clone()),
		"2026-10-26T10:00:02.000Z".to_string(),
		SessionMessage::user(
			MessageContent::Parts(vec![
				ContentPart::image("aW1n", "image/png"),
				ContentPart {
					kind: "text".to_string(),
					text: None,
					data: None,
					mime_type: None,
					extra: serde_json::Map::new(),
				},
			]),
			99999,
		),
	);

	let res = store.append_message(&bad_entry);
	assert!(res.is_err(), "append_message must fail on text part missing text");
	let err = res.unwrap_err();
	assert!(err.contains("missing text"), "error must mention missing text: {err}");

	// Verify exact equality of file and in-memory state
	let file_content_after = fs::read_to_string(&file).expect("read after");
	assert_eq!(file_content_before, file_content_after, "session file must remain 100% identical");
	let file_size_after = fs::metadata(&file).expect("metadata after").len();
	assert_eq!(file_size_before, file_size_after, "session file size must not change");
	assert_eq!(store.messages().len(), messages_len_before, "messages must not mutate");
	assert_eq!(store.entries().len(), entries_len_before, "entries must not mutate");
	assert_eq!(store.history().len(), history_len_before, "history must not mutate");

	fs::remove_dir_all(temp).ok();
}
