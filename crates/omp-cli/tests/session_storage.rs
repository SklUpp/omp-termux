use std::fs;
use std::path::PathBuf;

use omp_cli::dirs;
use omp_cli::session::{
	SessionOptions, SessionStore, ToolCallRecord, TurnRecord, compute_default_session_dir,
	continue_recent_with_breadcrumb, fork_session, import_legacy_v1, overlay_title_slot,
	overwrite_title_slot_rolled_back, parse_title_slot, read_terminal_breadcrumb,
	resolve_resumable_session, serialize_title_slot, session_matches_resume_arg,
	write_terminal_breadcrumb,
};

static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn temp_dir(prefix: &str) -> PathBuf {
	let unique = format!(
		"omp-test-{prefix}-{}-{}",
		std::process::id(),
		COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
	);
	let dir = std::env::temp_dir().join(unique);
	fs::create_dir_all(&dir).expect("create temp dir");
	dir
}

#[test]
fn read_ts_v3_session_fixture() {
	let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
		.join("tests/fixtures/v3_session.jsonl");
	assert!(fixture_path.is_file(), "fixture must exist");

	let temp = temp_dir("v3-read");
	let test_file = temp.join("session.jsonl");
	fs::copy(&fixture_path, &test_file).expect("copy fixture");

	let mut store = SessionStore::open(&SessionOptions {
		enabled: true,
		cwd: PathBuf::from("/work/test-project"),
		model: None,
		provider: None,
		dir: Some(temp.clone()),
		id: Some("session".to_string()),
		continue_latest: false,
	});

	assert!(store.enabled(), "store must be enabled: {:?}", store.take_warnings());
	let header = store.header().expect("header must be present");
	assert_eq!(header.id, "01a178ce-3463-7504-8047-ca4fab78a16b");
	assert_eq!(header.cwd, "/path/to/test-project");
	assert_eq!(header.title.as_deref(), Some("Test Session Title"));

	let history = store.history();
	assert_eq!(history.len(), 1, "history must reconstruct 1 turn");
	assert_eq!(history[0].prompt, "hello from test user");
	assert_eq!(history[0].assistant, "calling tool");
	assert_eq!(history[0].tool_calls.len(), 1);
	assert_eq!(history[0].tool_calls[0].id, "call_abc123");
	assert_eq!(history[0].tool_calls[0].name, "read");
	assert_eq!(history[0].tool_calls[0].result, "file content");
	assert!(!history[0].tool_calls[0].is_error);

	assert!(store.has_image_content(), "must preserve image block");
	assert!(!store.entries().is_empty(), "must retain entries");

	// Typed image parts must survive resume for provider replay.
	let messages = store.messages();
	assert_eq!(messages.len(), 3, "fixture has 3 message entries (user, assistant, toolResult)");
	let user_parts = messages[0].message.content.parts();
	assert!(
		user_parts
			.iter()
			.any(|part| part.is_image()
				&& part.data.as_deref() == Some("aGVsbG8=")
				&& part.mime_type.as_deref() == Some("image/png")),
		"user image part must survive replay: {user_parts:?}"
	);
	let tool_parts = messages[2].message.content.parts();
	assert!(
		tool_parts
			.iter()
			.any(|part| part.is_image() && part.data.as_deref() == Some("aW1hZ2VkYXRh")),
		"toolResult image part must survive replay: {tool_parts:?}"
	);

	// Provider metadata and TS catalog usage keys (including required cost) round-trip truthfully.
	let asst = &messages[1].message;
	assert_eq!(asst.api.as_deref(), Some("openai-completions"));
	assert_eq!(asst.provider.as_deref(), Some("openai"));
	assert_eq!(asst.model.as_deref(), Some("gpt-4o"));
	let usage = asst.usage.as_ref().expect("usage must round-trip");
	assert_eq!((usage.input, usage.output, usage.total_tokens), (10, 20, 30));
	assert!((usage.cost.total - 0.0003).abs() < 1e-6, "cost.total must round-trip truthfully: {:?}", usage.cost);
	assert!((usage.cost.input - 0.0001).abs() < 1e-6);
	assert!((usage.cost.output - 0.0002).abs() < 1e-6);
	assert_eq!(asst.stop_reason.as_deref(), Some("toolUse"));
	fs::remove_dir_all(temp).ok();
}

#[test]
fn v1_legacy_log_is_read_only_and_never_overwritten() {
	let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
		.join("tests/fixtures/v1_session.jsonl");
	let temp = temp_dir("v1-readonly");
	let v1_file = temp.join("v1-legacy-id.jsonl");
	fs::copy(&fixture_path, &v1_file).expect("copy fixture");
	let original_content = fs::read_to_string(&v1_file).expect("read original");

	let mut store = SessionStore::open(&SessionOptions {
		enabled: true,
		cwd: PathBuf::from("/work/legacy"),
		model: None,
		provider: None,
		dir: Some(temp.clone()),
		id: Some("v1-legacy-id".to_string()),
		continue_latest: false,
	});

	assert!(store.enabled(), "legacy store can be opened for reading");
	assert_eq!(store.history().len(), 1);
	assert_eq!(store.history()[0].prompt, "fix the bug");

	// Appending to v1 must fail / be rejected and NEVER mutate the v1 file on disk
	let dummy_turn = TurnRecord {
		prompt: "next".to_string(),
		assistant: "done".to_string(),
		stop_reason: "end_turn".to_string(),
		tool_calls: Vec::new(),
	};
	let append_result = store.append_turn(&dummy_turn);
	assert!(
		append_result.is_err(),
		"appending to legacy v1 log must be rejected"
	);

	let current_content = fs::read_to_string(&v1_file).expect("read v1");
	assert_eq!(
		original_content, current_content,
		"legacy v1 file on disk must not be modified"
	);

	fs::remove_dir_all(temp).ok();
}

#[test]
fn explicit_import_legacy_v1_creates_valid_v3_copy_without_mutating_v1() {
	let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
		.join("tests/fixtures/v1_session.jsonl");
	let temp = temp_dir("v1-import");
	let v1_file = temp.join("v1-source.jsonl");
	fs::copy(&fixture_path, &v1_file).expect("copy fixture");
	let original_content = fs::read_to_string(&v1_file).expect("read v1");

	let target_dir = temp.join("v3-target");
	fs::create_dir_all(&target_dir).expect("create target dir");

	let imported_path = import_legacy_v1(&v1_file, &target_dir, None)
		.expect("import legacy v1");
	assert!(imported_path.is_file());

	// v1 original must be untouched
	assert_eq!(
		fs::read_to_string(&v1_file).expect("read v1"),
		original_content,
		"original v1 must remain untouched"
	);

	// Imported file must have exact 256-byte title slot
	let imported_bytes = fs::read(&imported_path).expect("read imported");
	assert!(imported_bytes.len() > 256);
	assert_eq!(imported_bytes[255], b'\n');

	// Imported file must be reopenable as v3 and have the same turns
	let store = SessionStore::open(&SessionOptions {
		enabled: true,
		cwd: PathBuf::from("/work/legacy"),
		model: None,
		provider: None,
		dir: Some(target_dir.clone()),
		id: None,
		continue_latest: true,
	});
	assert!(store.enabled(), "reopened imported store must be enabled");
	assert_eq!(store.history().len(), 1);
	assert_eq!(store.history()[0].prompt, "fix the bug");

	fs::remove_dir_all(temp).ok();
}

#[test]
fn malformed_header_fails_without_append() {
	let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
		.join("tests/fixtures/malformed_header.jsonl");
	let temp = temp_dir("bad-header");
	let bad_file = temp.join("bad.jsonl");
	fs::copy(&fixture_path, &bad_file).expect("copy bad");
	let original = fs::read_to_string(&bad_file).expect("read");

	let mut store = SessionStore::open(&SessionOptions {
		enabled: true,
		cwd: PathBuf::from("/work"),
		model: None,
		provider: None,
		dir: Some(temp.clone()),
		id: Some("bad".to_string()),
		continue_latest: false,
	});

	assert!(!store.enabled(), "store must be disabled on malformed header");
	let warnings = store.take_warnings();
	assert!(
		warnings.iter().any(|w| w.contains("session header") || w.contains("invalid")),
		"must report warning about bad header: {warnings:?}"
	);

	let turn = TurnRecord {
		prompt: "hi".into(),
		assistant: "hello".into(),
		stop_reason: "end_turn".into(),
		tool_calls: vec![],
	};
	let _ = store.append_turn(&turn);
	assert_eq!(
		fs::read_to_string(&bad_file).expect("read"),
		original,
		"file must never be modified when header is invalid"
	);

	fs::remove_dir_all(temp).ok();
}

#[test]
fn malformed_entry_disables_persistence_and_refuses_append() {
	let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
		.join("tests/fixtures/malformed_entry.jsonl");
	let temp = temp_dir("bad-entry");
	let file = temp.join("malformed.jsonl");
	fs::copy(&fixture_path, &file).expect("copy");
	let original = fs::read_to_string(&file).expect("read");

	let mut store = SessionStore::open(&SessionOptions {
		enabled: true,
		cwd: PathBuf::from("/work/test"),
		model: None,
		provider: None,
		dir: Some(temp.clone()),
		id: Some("malformed".to_string()),
		continue_latest: false,
	});

	assert!(!store.enabled(), "store must disable persistence when log contains corrupted records");
	let warnings = store.take_warnings();
	assert!(
		warnings.iter().any(|w| w.contains("malformed") || w.contains("corrupting")),
		"warnings must report malformed record: {warnings:?}"
	);

	let turn = TurnRecord {
		prompt: "hi".into(),
		assistant: "hello".into(),
		stop_reason: "end_turn".into(),
		tool_calls: vec![],
	};
	let _ = store.append_turn(&turn);
	assert_eq!(
		fs::read_to_string(&file).expect("read after append attempt"),
		original,
		"corrupted file must never be appended to"
	);

	fs::remove_dir_all(temp).ok();
}

#[test]
fn title_slot_exact_256_bytes_and_overlay() {
	let slot = serialize_title_slot(
		"Test Title with Unicode 🦀 emoji and long description",
		Some("auto"),
		"2026-10-26T10:00:00.000Z",
	);
	assert_eq!(slot.as_bytes().len(), 256, "slot must be exactly 256 bytes");
	assert!(slot.ends_with('\n'), "slot must end with newline");

	let parsed = parse_title_slot(&slot).expect("parse slot");
	assert_eq!(parsed.title, "Test Title with Unicode 🦀 emoji and long description");
	assert_eq!(parsed.source.as_deref(), Some("auto"));
	assert_eq!(parsed.updated_at, "2026-10-26T10:00:00.000Z");

	// Massive title exceeding 256 bytes truncated cleanly by codepoints
	let massive_title = "🦀".repeat(200);
	let truncated_slot = serialize_title_slot(&massive_title, Some("user"), "2026-10-26T10:00:00.000Z");
	assert_eq!(truncated_slot.as_bytes().len(), 256, "truncated slot must be exactly 256 bytes");
	let parsed_trunc = parse_title_slot(&truncated_slot).expect("parse truncated slot");
	assert!(!parsed_trunc.title.is_empty());
	assert!(massive_title.starts_with(&parsed_trunc.title));

	// Overlay test
	let content = format!("{slot}line 2\nline 3\n");
	let new_slot = serialize_title_slot("Updated Title", Some("user"), "2026-10-26T11:00:00.000Z");
	let overlaid = overlay_title_slot(&content, &new_slot);
	assert!(overlaid.ends_with("line 2\nline 3\n"));
	assert_eq!(overlaid.as_bytes().len(), content.as_bytes().len());
	let parsed_overlaid = parse_title_slot(&overlaid).expect("parse overlaid");
	assert_eq!(parsed_overlaid.title, "Updated Title");
}

#[test]
fn ts_cwd_encoded_directory_names() {
	let home = dirs::home_dir().expect("home dir");
	let sessions_root = home.join(".omp/agent/sessions");

	// Home subpath
	let home_project = home.join("projects/my-app");
	let dir = compute_default_session_dir(&home_project, Some(&sessions_root));
	assert_eq!(dir, sessions_root.join("-projects-my-app"));

	// Home root
	let dir_home = compute_default_session_dir(&home, Some(&sessions_root));
	assert_eq!(dir_home, sessions_root.join("-"));

	// Tmp subpath
	let tmp = std::env::temp_dir();
	let tmp_project = tmp.join("sandbox/app");
	let dir_tmp = compute_default_session_dir(&tmp_project, Some(&sessions_root));
	assert!(
		dir_tmp.file_name().unwrap().to_str().unwrap().starts_with("-tmp"),
		"tmp dir must start with -tmp: {:?}", dir_tmp
	);
}

#[test]
fn resume_matching_and_breadcrumb_continue() {
	let temp = temp_dir("resume-test");

	// Filename matching
	let session_file = temp.join("2026-10-26T10-00-00-000Z_0192a5b0-7e44-7123-8abc-0123456789ab.jsonl");
	fs::write(&session_file, "{\"type\":\"session\",\"version\":3,\"id\":\"0192a5b0-7e44-7123-8abc-0123456789ab\",\"timestamp\":\"2026-10-26T10:00:00.000Z\",\"cwd\":\"/work\"}\n").expect("write");

	assert!(session_matches_resume_arg("0192a5b0-7e44-7123-8abc-0123456789ab", &session_file, "0192a5b0"));
	assert!(session_matches_resume_arg("0192a5b0-7e44-7123-8abc-0123456789ab", &session_file, "2026-10-26"));
	assert!(session_matches_resume_arg("0192a5b0-7e44-7123-8abc-0123456789ab", &session_file, "0192a"));
	assert!(!session_matches_resume_arg("0192a5b0-7e44-7123-8abc-0123456789ab", &session_file, ""));
	assert!(!session_matches_resume_arg("0192a5b0-7e44-7123-8abc-0123456789ab", &session_file, "nonexistent"));

	// Breadcrumb write / read
	let crumb_file = temp.join("test-term-id");
	write_terminal_breadcrumb(&PathBuf::from("/work"), &session_file, false, Some(&crumb_file)).expect("write crumb");
	let crumb = read_terminal_breadcrumb(Some(&crumb_file)).expect("read crumb");
	assert_eq!(crumb.cwd, PathBuf::from("/work"));
	assert_eq!(crumb.session_file, session_file);
	assert!(crumb.exists);
	assert!(!crumb.fresh);

	// Fresh breadcrumb with missing target file
	let missing_target = temp.join("never_created.jsonl");
	write_terminal_breadcrumb(&PathBuf::from("/work"), &missing_target, true, Some(&crumb_file)).expect("write fresh crumb");
	let fresh_crumb = read_terminal_breadcrumb(Some(&crumb_file)).expect("read fresh crumb");
	assert_eq!(fresh_crumb.session_file, missing_target);
	assert!(!fresh_crumb.exists);
	assert!(fresh_crumb.fresh);

	fs::remove_dir_all(temp).ok();
}

#[test]
fn fork_session_creates_new_session_with_parent_reference() {
	let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
		.join("tests/fixtures/v3_session.jsonl");
	let temp = temp_dir("fork-test");
	let original = temp.join("original.jsonl");
	fs::copy(&fixture_path, &original).expect("copy");

	let target_dir = temp.join("forks");
	fs::create_dir_all(&target_dir).expect("create forks dir");

	let forked_path = fork_session(&original, None, Some(&target_dir))
		.expect("fork session");
	assert!(forked_path.is_file());

	let forked_store = SessionStore::open(&SessionOptions {
		enabled: true,
		cwd: PathBuf::from("/work/test-project"),
		model: None,
		provider: None,
		dir: Some(target_dir.clone()),
		id: None,
		continue_latest: true,
	});
	assert!(forked_store.enabled());
	let forked_header = forked_store.header().expect("forked header");
	assert_ne!(forked_header.id, "01a178ce-3463-7504-8047-ca4fab78a16b");
	assert_eq!(
		forked_header.parent_session.as_deref(),
		Some("01a178ce-3463-7504-8047-ca4fab78a16b")
	);
	assert_eq!(forked_store.history().len(), 1);

	fs::remove_dir_all(temp).ok();
}

#[test]
fn moved_worktree_continue_reroots_and_resume_prefix_matches() {
	let temp = temp_dir("moved-wt");
	let sessions_dir = temp.join("sessions");
	let old_cwd = temp.join("old-project");
	let new_cwd = temp.join("new-project");
	fs::create_dir_all(&sessions_dir).expect("create sessions dir");
	fs::create_dir_all(&old_cwd).expect("create old cwd");
	fs::create_dir_all(&new_cwd).expect("create new cwd");

	let session_file = sessions_dir
		.join("2026-10-26T10-00-00-000Z_0192a5b0-7e44-7123-8abc-0123456789ab.jsonl");
	let mut body = serialize_title_slot("Moved", Some("auto"), "2026-10-26T10:00:00.000Z");
	body.push_str(&format!(
		"{}\n",
		serde_json::json!({
			"type": "session",
			"version": 3,
			"id": "0192a5b0-7e44-7123-8abc-0123456789ab",
			"timestamp": "2026-10-26T10:00:00.000Z",
			"cwd": old_cwd.display().to_string(),
		})
	));
	body.push_str(
		"{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-10-26T10:00:01.000Z\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}],\"timestamp\":1793095201000}}\n",
	);
	fs::write(&session_file, &body).expect("write session");

	let crumb = temp.join("term-crumb");
	write_terminal_breadcrumb(&old_cwd, &session_file, false, Some(&crumb)).expect("write crumb");

	// The worktree moved away: the recorded cwd is gone.
	fs::remove_dir_all(&old_cwd).expect("remove old cwd");

	let resumed = continue_recent_with_breadcrumb(&new_cwd, Some(&sessions_dir), Some(&crumb))
		.expect("continue")
		.expect("session re-rooted");
	assert_eq!(resumed, session_file, "same-dir move keeps the log path");

	// Header cwd is re-rooted to the new worktree (TS `moveTo` semantics).
	let content = fs::read_to_string(&session_file).expect("read re-rooted");
	let header_line = content.lines().nth(1).expect("header line");
	let header: serde_json::Value = serde_json::from_str(header_line).expect("header json");
	let rerooted = PathBuf::from(header["cwd"].as_str().expect("cwd"));
	assert_eq!(
		fs::canonicalize(&rerooted).unwrap_or_else(|_| rerooted.clone()),
		fs::canonicalize(&new_cwd).expect("canonical new cwd"),
		"header cwd must be re-rooted to the moved worktree"
	);

	// Resume selector matches the moved session by id prefix.
	let resolved = resolve_resumable_session("0192a5b0", &new_cwd, Some(&sessions_dir), false)
		.expect("resolve")
		.expect("prefix match");
	assert_eq!(resolved, session_file);

	fs::remove_dir_all(temp).ok();
}
#[test]
fn append_turn_titles_and_appends_entries_and_reopens_cleanly() {
	let temp = temp_dir("append-test");
	let mut store = SessionStore::open(&SessionOptions {
		enabled: true,
		cwd: PathBuf::from("/work/append-test"),
		model: None,
		provider: None,
		dir: Some(temp.clone()),
		id: Some("01a178ce-append-test".to_string()),
		continue_latest: false,
	});
	assert!(store.enabled());
	let file = store.file().expect("file exists").to_path_buf();
	let size_before = fs::metadata(&file).expect("stat").len();

	// First turn auto-titles the session slot and appends entries.
	store
		.append_turn(&TurnRecord {
			prompt: "initial prompt".to_string(),
			assistant: "initial reply".to_string(),
			stop_reason: "end_turn".to_string(),
			tool_calls: Vec::new(),
		})
		.expect("initial append succeeds");

	assert_eq!(store.title_slot().expect("title slot").title, "initial prompt");
	assert_eq!(store.header().expect("header").title.as_deref(), Some("initial prompt"));
	let size_after = fs::metadata(&file).expect("stat").len();
	assert!(size_after > size_before);

	// Reopening loads the title slot and turn history cleanly.
	let reopened = SessionStore::open(&SessionOptions {
		enabled: true,
		cwd: PathBuf::from("/work/append-test"),
		model: None,
		provider: None,
		dir: Some(temp.clone()),
		id: Some("01a178ce-append-test".to_string()),
		continue_latest: false,
	});
	assert!(reopened.enabled());
	assert_eq!(reopened.title_slot().expect("title slot").title, "initial prompt");
	assert_eq!(reopened.history().len(), 1);
	assert_eq!(reopened.history()[0].prompt, "initial prompt");
	assert_eq!(reopened.history()[0].assistant, "initial reply");

	fs::remove_dir_all(temp).ok();
}

#[test]
fn overwrite_title_slot_rolled_back_updates_slot_and_preserves_length() {
	let temp = temp_dir("title-update-test");
	let file = temp.join("01a178ce-test.jsonl");

	let initial_slot = serialize_title_slot("Initial Title", Some("auto"), "2026-10-26T12:00:00.000Z");
	let header_line = r#"{"type":"session","version":3,"id":"01a178ce-test","timestamp":"2026-10-26T12:00:00.000Z","cwd":"/work/test"}"#;
	let mut init = Vec::new();
	init.extend_from_slice(initial_slot.as_bytes());
	init.extend_from_slice(format!("{header_line}\n").as_bytes());
	fs::write(&file, &init).expect("write initial");

	assert_eq!(initial_slot.as_bytes().len(), 256);
	let total_len = init.len();

	let store = SessionStore::open(&SessionOptions {
		enabled: true,
		cwd: PathBuf::from("/work/test"),
		model: None,
		provider: None,
		dir: Some(temp.clone()),
		id: Some("01a178ce-test".to_string()),
		continue_latest: false,
	});
	assert!(store.enabled());
	assert_eq!(store.title_slot().expect("slot").title, "Initial Title");

	// Update title using overwrite_title_slot_rolled_back
	let updated_slot = serialize_title_slot("Updated Title", Some("user"), "2026-10-26T13:00:00.000Z");
	overwrite_title_slot_rolled_back(&file, &updated_slot).expect("update slot");

	// Verify total file length is strictly preserved and content parses
	assert_eq!(fs::metadata(&file).expect("stat").len() as usize, total_len);
	let content = fs::read_to_string(&file).expect("read");
	let parsed = parse_title_slot(content.lines().next().unwrap()).expect("parse");
	assert_eq!(parsed.title, "Updated Title");

	// Reopen with SessionStore verifies new title is loaded
	let reopened = SessionStore::open(&SessionOptions {
		enabled: true,
		cwd: PathBuf::from("/work/test"),
		model: None,
		provider: None,
		dir: Some(temp.clone()),
		id: Some("01a178ce-test".to_string()),
		continue_latest: false,
	});
	assert!(reopened.enabled());
	assert_eq!(reopened.title_slot().expect("slot").title, "Updated Title");

	fs::remove_dir_all(temp).ok();
}

#[test]
fn unsupported_session_version_rejected_without_append() {
	let temp = temp_dir("bad-version");
	let file = temp.join("unsupported-v4.jsonl");

	let initial_slot = serialize_title_slot("", Some("auto"), "2026-10-26T12:00:00.000Z");
	let header_line = r#"{"type":"session","version":4,"id":"unsupported-v4","timestamp":"2026-10-26T12:00:00.000Z","cwd":"/work/test"}"#;
	let mut init = Vec::new();
	init.extend_from_slice(initial_slot.as_bytes());
	init.extend_from_slice(format!("{header_line}\n").as_bytes());
	fs::write(&file, &init).expect("write bad version file");

	let original = fs::read_to_string(&file).expect("read");

	let mut store = SessionStore::open(&SessionOptions {
		enabled: true,
		cwd: PathBuf::from("/work/test"),
		model: None,
		provider: None,
		dir: Some(temp.clone()),
		id: Some("unsupported-v4".to_string()),
		continue_latest: false,
	});

	assert!(!store.enabled(), "store must be disabled for unsupported session version");
	let warnings = store.take_warnings();
	assert!(
		warnings.iter().any(|w| w.contains("unsupported session version `4`")),
		"must report warning about unsupported session version 4: {warnings:?}"
	);

	let turn = TurnRecord {
		prompt: "hi".into(),
		assistant: "hello".into(),
		stop_reason: "stop".into(),
		tool_calls: vec![],
	};
	let _ = store.append_turn(&turn);
	assert_eq!(
		fs::read_to_string(&file).expect("read"),
		original,
		"file must never be modified when session version is unsupported"
	);

	fs::remove_dir_all(temp).ok();
}

#[test]
fn missing_session_version_rejected_without_append() {
	let temp = temp_dir("missing-version");
	let file = temp.join("missing-ver.jsonl");

	let initial_slot = serialize_title_slot("", Some("auto"), "2026-10-26T12:00:00.000Z");
	let header_line = r#"{"type":"session","id":"missing-ver","timestamp":"2026-10-26T12:00:00.000Z","cwd":"/work/test"}"#;
	let mut init = Vec::new();
	init.extend_from_slice(initial_slot.as_bytes());
	init.extend_from_slice(format!("{header_line}\n").as_bytes());
	fs::write(&file, &init).expect("write missing version file");

	let original = fs::read_to_string(&file).expect("read");

	let mut store = SessionStore::open(&SessionOptions {
		enabled: true,
		cwd: PathBuf::from("/work/test"),
		model: None,
		provider: None,
		dir: Some(temp.clone()),
		id: Some("missing-ver".to_string()),
		continue_latest: false,
	});

	assert!(!store.enabled(), "store must be disabled when session version is missing");
	let warnings = store.take_warnings();
	assert!(
		warnings.iter().any(|w| w.contains("unsupported session version `missing`")),
		"must report warning about missing session version: {warnings:?}"
	);

	let turn = TurnRecord {
		prompt: "hi".into(),
		assistant: "hello".into(),
		stop_reason: "stop".into(),
		tool_calls: vec![],
	};
	let _ = store.append_turn(&turn);
	assert_eq!(
		fs::read_to_string(&file).expect("read"),
		original,
		"file must never be modified when session version is missing"
	);

	fs::remove_dir_all(temp).ok();
}

#[test]
fn stop_reason_canonicalized_in_persisted_v3_log() {
	let temp = temp_dir("canon-stop-reason");
	let mut store = SessionStore::open(&SessionOptions {
		enabled: true,
		cwd: PathBuf::from("/work/canon"),
		model: None,
		provider: None,
		dir: Some(temp.clone()),
		id: Some("canon-stop-test".to_string()),
		continue_latest: false,
	});
	assert!(store.enabled());
	let file = store.file().expect("file exists").to_path_buf();

	// Turn 1 with legacy/variant stop reason "end_turn"
	store
		.append_turn(&TurnRecord {
			prompt: "first prompt".to_string(),
			assistant: "first reply".to_string(),
			stop_reason: "end_turn".to_string(),
			tool_calls: Vec::new(),
		})
		.expect("append turn 1");

	// Turn 2 with legacy/variant stop reason "tool_use"
	store
		.append_turn(&TurnRecord {
			prompt: "second prompt".to_string(),
			assistant: "second reply".to_string(),
			stop_reason: "tool_use".to_string(),
			tool_calls: vec![ToolCallRecord {
				id: "call-1".to_string(),
				name: "read".to_string(),
				arguments: serde_json::json!({"path": "foo"}),
				result: "contents".to_string(),
				is_error: false,
			}],
		})
		.expect("append turn 2");

	let content = fs::read_to_string(&file).expect("read log");
	assert!(
		content.contains(r#""stopReason":"stop""#),
		"persisted log must contain canonical 'stop' stopReason: {content}"
	);
	assert!(
		!content.contains(r#""stopReason":"end_turn""#),
		"persisted log must never contain non-canonical 'end_turn': {content}"
	);
	assert!(
		content.contains(r#""stopReason":"toolUse""#),
		"persisted log must contain canonical 'toolUse' stopReason: {content}"
	);
	assert!(
		!content.contains(r#""stopReason":"tool_use""#),
		"persisted log must never contain non-canonical 'tool_use': {content}"
	);

	// Reopen and verify resumed history records canonical stop reasons
	let reopened = SessionStore::open(&SessionOptions {
		enabled: true,
		cwd: PathBuf::from("/work/canon"),
		model: None,
		provider: None,
		dir: Some(temp.clone()),
		id: Some("canon-stop-test".to_string()),
		continue_latest: false,
	});
	assert!(reopened.enabled());
	let hist = reopened.history();
	assert_eq!(hist.len(), 2);
	assert_eq!(hist[0].stop_reason, "stop");
	assert_eq!(hist[1].stop_reason, "toolUse");

	fs::remove_dir_all(temp).ok();
}
