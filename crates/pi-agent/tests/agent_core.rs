use std::{
	collections::VecDeque,
	sync::{
		Arc, Mutex,
		atomic::{AtomicUsize, Ordering},
	},
};

use async_trait::async_trait;
use futures::StreamExt;
use pi_agent::provider::OpenAiChatRequest;
use pi_agent::{
	AgentConfig, AgentError, AgentError::Interrupted, AgentEvent, AgentLoop, AgentState,
	CancellationToken, ChunkStream, EditTool, Message, ModelProvider,
	OpenAiCompatibleProvider, ProviderError, ReadTool, Role, SteeringEvent,
	StreamAssembler, StreamChunk, Tool, ToolCall, ToolCallDelta, ToolRegistry, TurnStatus,
	WriteTool, parse_sse_line, sse_stream,
};
use serde_json::json;

/// Scripted transport: a queue of canned provider turns. This is a test double
/// for the network, not a production code path.
struct ScriptedProvider {
	script: Mutex<VecDeque<Result<Message, ProviderError>>>,
}

impl ScriptedProvider {
	fn new(script: Vec<Result<Message, ProviderError>>) -> Self {
		Self { script: Mutex::new(script.into()) }
	}

	fn with_turns(turns: Vec<Message>) -> Self {
		Self::new(turns.into_iter().map(Ok).collect())
	}
}

#[async_trait]
impl ModelProvider for ScriptedProvider {
	async fn complete(
		&self,
		_messages: &[Message],
		_tools: &[serde_json::Value],
	) -> Result<Message, ProviderError> {
		self
			.script
			.lock()
			.expect("script lock")
			.pop_front()
			.unwrap_or_else(|| Err(ProviderError::Protocol("script exhausted".to_string())))
	}

	async fn stream(
		&self,
		_messages: &[Message],
		_tools: &[serde_json::Value],
	) -> Result<ChunkStream, ProviderError> {
		Err(ProviderError::Protocol("scripted provider has no stream".to_string()))
	}
}

/// Always requests the same tool, to exercise the turn budget.
struct LoopingProvider {
	calls: AtomicUsize,
}

#[async_trait]
impl ModelProvider for LoopingProvider {
	async fn complete(
		&self,
		_messages: &[Message],
		_tools: &[serde_json::Value],
	) -> Result<Message, ProviderError> {
		let call = self.calls.fetch_add(1, Ordering::SeqCst);
		Ok(Message::assistant_with_tools(
			"",
			vec![ToolCall {
				id: format!("call_{call}"),
				name: "todo".to_string(),
				arguments: json!({"action": "list"}),
			}],
		))
	}

	async fn stream(
		&self,
		_messages: &[Message],
		_tools: &[serde_json::Value],
	) -> Result<ChunkStream, ProviderError> {
		Err(ProviderError::Protocol("looping provider has no stream".to_string()))
	}
}

/// Throwaway working directory; removes exactly the tree it created.
struct TempDir {
	path: std::path::PathBuf,
}

impl TempDir {
	fn new(label: &str) -> Self {
		static NEXT: AtomicUsize = AtomicUsize::new(0);
		let unique =
			format!("pi-agent-{label}-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::SeqCst));
		let path = std::env::temp_dir().join(unique);
		std::fs::create_dir_all(&path).expect("create temp dir");
		Self { path }
	}
}

impl Drop for TempDir {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.path);
	}
}

fn config(cwd: &std::path::Path, max_turns: usize) -> AgentConfig {
	AgentConfig {
		max_turns,
		context_limit: 100_000,
		system_prompt: Some("test system prompt".to_string()),
		cwd: cwd.to_path_buf(),
	}
}

fn tool_messages(state: &AgentState) -> Vec<&Message> {
	state
		.messages
		.iter()
		.filter(|message| message.role == Role::Tool)
		.collect()
}

/// Every assistant tool_call must be answered by a tool message with the same id.
fn assert_no_orphan_tool_calls(state: &AgentState) {
	let answered: Vec<&str> = tool_messages(state)
		.iter()
		.filter_map(|m| m.tool_call_id.as_deref())
		.collect();
	for message in &state.messages {
		for call in &message.tool_calls {
			assert!(answered.contains(&call.id.as_str()), "orphan tool_call {}", call.id);
		}
	}
}

#[tokio::test]
async fn tool_call_round_trip_feeds_result_back_to_the_model() {
	let dir = TempDir::new("roundtrip");
	let provider = ScriptedProvider::with_turns(vec![
		Message::assistant_with_tools(
			"writing",
			vec![ToolCall {
				id: "call_write".to_string(),
				name: "write".to_string(),
				arguments: json!({"path": "note.txt", "content": "hello"}),
			}],
		),
		Message::assistant_with_tools(
			"editing",
			vec![ToolCall {
				id: "call_edit".to_string(),
				name: "edit".to_string(),
				arguments: json!({"path": "note.txt", "old_string": "hello", "new_string": "world"}),
			}],
		),
		Message::assistant("done"),
	]);
	let agent = AgentLoop::new(
		config(&dir.path, 5),
		std::sync::Arc::new(provider),
		ToolRegistry::with_builtins(),
	);
	let mut state = AgentState::default();

	let turns = agent
		.run_dialog("write a note", &mut state, None)
		.await
		.expect("dialog");

	assert_eq!(turns.len(), 3);
	assert!(!turns[0].is_completed);
	assert!(!turns[1].is_completed);
	assert!(turns[2].is_completed);
	assert_eq!(turns[0].tool_results.len(), 1);
	assert_eq!(turns[1].tool_results.len(), 1);

	let roles: Vec<Role> = state.messages.iter().map(|message| message.role).collect();
	assert_eq!(
		roles,
		vec![
			Role::System,
			Role::User,
			Role::Assistant,
			Role::Tool,
			Role::Assistant,
			Role::Tool,
			Role::Assistant,
		]
	);
	let tools = tool_messages(&state);
	assert!(tools[0].content.contains("Successfully wrote"));
	assert!(tools[1].content.contains("Replaced 1 occurrence(s)"));
	assert_eq!(std::fs::read_to_string(dir.path.join("note.txt")).expect("file"), "world");
	assert_no_orphan_tool_calls(&state);

	// Each later provider call saw the previous tool answers in history.
	assert_eq!(state.total_turns, 3);
	assert!(
		state
			.messages
			.iter()
			.any(|message| message.role == Role::Tool
				&& message.tool_call_id.as_deref() == Some("call_write"))
	);
	assert!(
		state
			.messages
			.iter()
			.any(|message| message.role == Role::Tool
				&& message.tool_call_id.as_deref() == Some("call_edit"))
	);
}

#[tokio::test]
async fn provider_error_surfaces_without_polluting_history() {
	let dir = TempDir::new("provider-error");
	let provider = ScriptedProvider::new(vec![Err(ProviderError::HttpStatus(401, "no key".into()))]);
	let agent = AgentLoop::new(
		config(&dir.path, 3),
		std::sync::Arc::new(provider),
		ToolRegistry::with_builtins(),
	);
	let mut state = AgentState::default();

	let error = agent
		.run_dialog("hi", &mut state, None)
		.await
		.expect_err("must fail");

	match error {
		AgentError::Provider(ProviderError::HttpStatus(code, body)) => {
			assert_eq!(code, 401);
			assert_eq!(body, "no key");
		},
		other => panic!("unexpected error: {other:?}"),
	}
	assert!(
		!state
			.messages
			.iter()
			.any(|message| message.role == Role::Assistant)
	);
	assert_eq!(state.total_turns, 0);
}

#[tokio::test]
async fn max_turns_is_enforced_and_history_stays_answerable() {
	let dir = TempDir::new("max-turns");
	let agent = AgentLoop::new(
		config(&dir.path, 2),
		std::sync::Arc::new(LoopingProvider { calls: AtomicUsize::new(0) }),
		ToolRegistry::with_builtins(),
	);
	let mut state = AgentState::default();

	let error = agent
		.run_dialog("loop", &mut state, None)
		.await
		.expect_err("must stop");

	assert!(matches!(error, AgentError::MaxTurnsExceeded(2)));
	assert_eq!(state.total_turns, 2);
	assert_eq!(tool_messages(&state).len(), 2);
	assert_no_orphan_tool_calls(&state);
}

#[tokio::test]
async fn unknown_tool_still_answers_every_call() {
	let dir = TempDir::new("unknown-tool");
	let provider = ScriptedProvider::with_turns(vec![
		Message::assistant_with_tools(
			"trying",
			vec![
				ToolCall {
					id: "call_missing".to_string(),
					name: "not_a_tool".to_string(),
					arguments: json!({}),
				},
				ToolCall {
					id: "call_write".to_string(),
					name: "write".to_string(),
					arguments: json!({"path": "a.txt", "content": "x"}),
				},
			],
		),
		Message::assistant("ok"),
	]);
	let agent = AgentLoop::new(
		config(&dir.path, 3),
		std::sync::Arc::new(provider),
		ToolRegistry::with_builtins(),
	);
	let mut state = AgentState::default();

	let turns = agent
		.run_dialog("go", &mut state, None)
		.await
		.expect("dialog");

	assert_eq!(turns[0].tool_results.len(), 2);
	let results = &turns[0].tool_results;
	assert_eq!(results[0].tool_call_id, "call_missing");
	assert!(results[0].is_error);
	assert!(results[0].content.contains("Tool not found"));
	assert_eq!(results[1].tool_call_id, "call_write");
	assert!(!results[1].is_error);
	assert_no_orphan_tool_calls(&state);
}

#[tokio::test]
async fn interrupt_cancels_tools_without_orphans() {
	let dir = TempDir::new("interrupt");
	let provider = ScriptedProvider::with_turns(vec![Message::assistant_with_tools(
		"running",
		vec![ToolCall {
			id: "call_sleep".to_string(),
			name: "bash".to_string(),
			arguments: json!({"command": "sleep 5"}),
		}],
	)]);
	let agent = AgentLoop::new(
		config(&dir.path, 3),
		std::sync::Arc::new(provider),
		ToolRegistry::with_builtins(),
	);
	let mut state = AgentState::default();
	let (steering_tx, steering_rx) = tokio::sync::mpsc::channel(4);

	tokio::spawn(async move {
		tokio::time::sleep(std::time::Duration::from_millis(100)).await;
		let _ = steering_tx.send(SteeringEvent::Interrupt).await;
	});

	let error = agent
		.run_dialog("sleep", &mut state, Some(steering_rx))
		.await
		.expect_err("must interrupt");

	assert!(matches!(error, Interrupted));
	assert_no_orphan_tool_calls(&state);
	assert_eq!(tool_messages(&state).len(), 1);
}

#[tokio::test]
async fn bash_output_is_bounded() {
	let dir = TempDir::new("bash-bounded");
	let tool = pi_agent::BashTool;
	let ctx = pi_agent::AgentContext::new(dir.path.clone());

	let result = tool
		.execute(&json!({"command": "head -c 200000 /dev/zero | tr '\\0' 'a'"}), &ctx)
		.await
		.expect("bash");

	assert!(result.content.contains("output truncated:"), "got: {}", result.content.len());
	assert!(result.content.len() < 100 * 1024, "output escaped the cap: {}", result.content.len());
}

#[tokio::test]
async fn read_only_returns_regular_files_within_the_byte_cap() {
	let dir = TempDir::new("read-limits");
	let big = dir.path.join("big.txt");
	std::fs::write(&big, "a".repeat(300_000)).expect("write big");
	std::fs::create_dir_all(dir.path.join("sub")).expect("mkdir");
	let ctx = pi_agent::AgentContext::new(dir.path.clone());

	let truncated = ReadTool
		.execute(&json!({"path": "big.txt"}), &ctx)
		.await
		.expect("read");
	assert!(!truncated.is_error);
	assert!(
		truncated
			.content
			.contains("[truncated at 262144 bytes of 300000]")
	);
	assert!(truncated.content.len() < 300_000);

	let directory = ReadTool
		.execute(&json!({"path": "sub"}), &ctx)
		.await
		.expect("read dir");
	assert!(directory.is_error);
	assert!(directory.content.contains("not a regular file"));

	let missing = ReadTool
		.execute(&json!({"path": "nope.txt"}), &ctx)
		.await
		.expect("read miss");
	assert!(missing.is_error);
}

#[tokio::test]
async fn write_rejects_oversized_content() {
	let dir = TempDir::new("write-limits");
	let ctx = pi_agent::AgentContext::new(dir.path.clone());

	let error = WriteTool
		.execute(&json!({"path": "big.txt", "content": "x".repeat(1024 * 1024 + 1)}), &ctx)
		.await
		.expect_err("must reject");

	assert!(error.to_string().contains("the limit is 1048576"));
}

#[tokio::test]
async fn edit_replaces_a_unique_match_and_reports_the_count() {
	let dir = TempDir::new("edit-unique");
	std::fs::write(dir.path.join("note.txt"), "one needle two\n").expect("write");
	let ctx = pi_agent::AgentContext::new(dir.path.clone());

	let result = EditTool
		.execute(&json!({"path": "note.txt", "old_string": "needle", "new_string": "thread"}), &ctx)
		.await
		.expect("edit");

	assert!(!result.is_error);
	assert!(result.content.contains("Replaced 1 occurrence(s)"));
	assert_eq!(
		std::fs::read_to_string(dir.path.join("note.txt")).expect("read"),
		"one thread two\n"
	);

	let empty = EditTool
		.execute(&json!({"path": "note.txt", "old_string": "", "new_string": "x"}), &ctx)
		.await
		.expect_err("empty search must be rejected");
	assert!(empty.to_string().contains("must not be empty"));
}

#[tokio::test]
async fn edit_rejects_an_ambiguous_match_without_touching_the_file() {
	let dir = TempDir::new("edit-ambiguous");
	std::fs::write(dir.path.join("note.txt"), "dup dup\n").expect("write");
	let ctx = pi_agent::AgentContext::new(dir.path.clone());

	let result = EditTool
		.execute(&json!({"path": "note.txt", "old_string": "dup", "new_string": "one"}), &ctx)
		.await
		.expect("edit");

	assert!(result.is_error);
	assert!(result.content.contains("2 occurrences"));
	assert!(result.content.contains("set replace_all"));
	assert_eq!(std::fs::read_to_string(dir.path.join("note.txt")).expect("read"), "dup dup\n");
}

#[tokio::test]
async fn edit_replace_all_rewrites_every_occurrence() {
	let dir = TempDir::new("edit-all");
	std::fs::write(dir.path.join("note.txt"), "dup dup dup\n").expect("write");
	let ctx = pi_agent::AgentContext::new(dir.path.clone());

	let result = EditTool
		.execute(
			&json!({
				"path": "note.txt",
				"old_string": "dup",
				"new_string": "one",
				"replace_all": true
			}),
			&ctx,
		)
		.await
		.expect("edit");

	assert!(!result.is_error);
	assert!(result.content.contains("Replaced 3 occurrence(s)"));
	assert_eq!(std::fs::read_to_string(dir.path.join("note.txt")).expect("read"), "one one one\n");
}

#[tokio::test]
async fn edit_reports_missing_files_and_absent_matches() {
	let dir = TempDir::new("edit-missing");
	let ctx = pi_agent::AgentContext::new(dir.path.clone());

	let missing = EditTool
		.execute(&json!({"path": "nope.txt", "old_string": "a", "new_string": "b"}), &ctx)
		.await
		.expect("edit");
	assert!(missing.is_error);
	assert!(missing.content.contains("Failed to edit"));

	std::fs::write(dir.path.join("note.txt"), "present\n").expect("write");
	let absent = EditTool
		.execute(&json!({"path": "note.txt", "old_string": "absent", "new_string": "b"}), &ctx)
		.await
		.expect("edit");
	assert!(absent.is_error);
	assert!(absent.content.contains("Not found in"));
	assert_eq!(std::fs::read_to_string(dir.path.join("note.txt")).expect("read"), "present\n");
}

#[tokio::test]
async fn edit_preserves_original_bytes_and_cleans_temp_on_cancellation() {
	let dir = TempDir::new("edit-abort");
	let target = dir.path.join("file.txt");
	let original = "sensitive original content\n";
	std::fs::write(&target, original).expect("write original");

	let token = CancellationToken::new();
	token.cancel();
	let ctx = pi_agent::AgentContext::with_cancellation(dir.path.clone(), token);

	let result = EditTool
		.execute(
			&json!({
				"path": "file.txt",
				"old_string": "sensitive",
				"new_string": "corrupted"
			}),
			&ctx,
		)
		.await
		.expect("edit");

	assert!(result.is_error);
	assert!(result.content.contains("Edit of"));
	assert!(result.content.contains("cancelled"));

	let content = std::fs::read_to_string(&target).expect("read");
	assert_eq!(content, original);

	let entries: Vec<_> = std::fs::read_dir(&dir.path)
		.expect("read dir")
		.map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
		.collect();
	assert_eq!(entries, vec!["file.txt"]);
}

#[tokio::test]
async fn write_preserves_original_bytes_and_cleans_temp_on_cancellation() {
	let dir = TempDir::new("write-abort");
	let target = dir.path.join("file.txt");
	let original = "existing user file content\n";
	std::fs::write(&target, original).expect("write original");

	let token = CancellationToken::new();
	token.cancel();
	let ctx = pi_agent::AgentContext::with_cancellation(dir.path.clone(), token);

	let result = WriteTool
		.execute(
			&json!({
				"path": "file.txt",
				"content": "replacement that got cancelled\n"
			}),
			&ctx,
		)
		.await
		.expect("write");

	assert!(result.is_error);
	assert!(result.content.contains("Write to"));
	assert!(result.content.contains("cancelled"));

	let content = std::fs::read_to_string(&target).expect("read");
	assert_eq!(content, original);

	let entries: Vec<_> = std::fs::read_dir(&dir.path)
		.expect("read dir")
		.map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
		.collect();
	assert_eq!(entries, vec!["file.txt"]);
}

#[tokio::test]
async fn write_aborted_during_actual_write_cleans_temp_and_preserves_target() {
	let dir = TempDir::new("write-abort-inflight");
	let target = dir.path.join("file.txt");
	let original = "sensitive original content\n";
	std::fs::write(&target, original).expect("write original");

	let token = CancellationToken::new();
	let ctx = pi_agent::AgentContext::with_cancellation(dir.path.clone(), token.clone());

	let payload = "A".repeat(1024 * 1024);
	let args = json!({
		"path": "file.txt",
		"content": payload,
	});

	let task = tokio::spawn(async move {
		WriteTool.execute(&args, &ctx).await
	});

	for _ in 0..1000 {
		let has_temp = std::fs::read_dir(&dir.path)
			.expect("read dir")
			.any(|e| {
				e.ok()
					.map(|e| e.file_name().to_string_lossy().contains(".tmp-"))
					.unwrap_or(false)
			});
		if has_temp {
			break;
		}
		tokio::task::yield_now().await;
	}

	token.cancel();

	let result = task.await.expect("join").expect("execute");
	if result.is_error && result.content.contains("cancelled") {
		assert_eq!(std::fs::read_to_string(&target).expect("read"), original);
	}

	let entries: Vec<_> = std::fs::read_dir(&dir.path)
		.expect("read dir")
		.map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
		.filter(|name| name.contains(".tmp-"))
		.collect();
	assert!(entries.is_empty(), "no temp file may remain after abort: {entries:?}");
}

#[tokio::test]
async fn write_aborted_by_task_abort_cleans_temp_and_prevents_rename() {
	let dir = TempDir::new("write-abort-task");
	let target = dir.path.join("file.txt");
	let original = "sensitive original content\n";
	std::fs::write(&target, original).expect("write original");

	let token = CancellationToken::new();
	let ctx = pi_agent::AgentContext::with_cancellation(dir.path.clone(), token);

	let payload = "A".repeat(1024 * 1024);
	let args = json!({
		"path": "file.txt",
		"content": payload,
	});

	let task = tokio::spawn(async move {
		WriteTool.execute(&args, &ctx).await
	});

	for _ in 0..1000 {
		let has_temp = std::fs::read_dir(&dir.path)
			.expect("read dir")
			.any(|e| {
				e.ok()
					.map(|e| e.file_name().to_string_lossy().contains(".tmp-"))
					.unwrap_or(false)
			});
		if has_temp {
			break;
		}
		tokio::task::yield_now().await;
	}

	task.abort();
	match task.await {
		Ok(_) => {
			let content = std::fs::read_to_string(&target).expect("read");
			assert!(
				content == original || content == payload,
				"target must be either original or full payload, never partial"
			);
		},
		Err(e) if e.is_cancelled() => {
			for _ in 0..100 {
				let entries: Vec<_> = std::fs::read_dir(&dir.path)
					.expect("read dir")
					.map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
					.filter(|name| name.contains(".tmp-"))
					.collect();
				if entries.is_empty() {
					break;
				}
				tokio::time::sleep(std::time::Duration::from_millis(5)).await;
			}
			let content = std::fs::read_to_string(&target).expect("read");
			assert!(
				content == original || content == payload,
				"target must be either original or full payload, never partial"
			);
		},
		Err(e) => panic!("unexpected join error: {e}"),
	}

	let entries: Vec<_> = std::fs::read_dir(&dir.path)
		.expect("read dir")
		.map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
		.filter(|name| name.contains(".tmp-"))
		.collect();
	assert!(entries.is_empty(), "no temp file may remain after task.abort: {entries:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn write_follows_dangling_symlink_preserving_symlink() {
	let dir = TempDir::new("dangling-symlink");
	let target_file = dir.path.join("created_by_link.txt");
	let link_file = dir.path.join("dangling_link.txt");
	std::os::unix::fs::symlink(&target_file, &link_file).expect("symlink");

	let symlink_meta = std::fs::symlink_metadata(&link_file).expect("symlink metadata");
	assert!(symlink_meta.file_type().is_symlink());
	assert!(!target_file.exists());

	let ctx = pi_agent::AgentContext::new(dir.path.clone());
	let result = WriteTool
		.execute(
			&json!({
				"path": "dangling_link.txt",
				"content": "written through dangling link\n"
			}),
			&ctx,
		)
		.await
		.expect("write");

	assert!(!result.is_error);
	assert!(target_file.exists());
	assert_eq!(
		std::fs::read_to_string(&target_file).expect("read target"),
		"written through dangling link\n"
	);

	let symlink_meta_after = std::fs::symlink_metadata(&link_file).expect("symlink metadata after");
	assert!(symlink_meta_after.file_type().is_symlink(), "symlink must not be replaced by regular file");
	assert_eq!(
		std::fs::read_to_string(&link_file).expect("read through link"),
		"written through dangling link\n"
	);
}

#[tokio::test]
async fn write_fails_when_target_is_directory() {
	let dir = TempDir::new("target-is-dir");
	let subdir = dir.path.join("subfolder");
	std::fs::create_dir_all(&subdir).expect("mkdir");

	let ctx = pi_agent::AgentContext::new(dir.path.clone());
	let result = WriteTool
		.execute(
			&json!({
				"path": "subfolder",
				"content": "cannot overwrite directory\n"
			}),
			&ctx,
		)
		.await
		.expect("write");

	assert!(result.is_error);
	assert!(result.content.contains("is a directory") || result.content.contains("Failed to write"));
	assert!(subdir.is_dir());

	let entries: Vec<_> = std::fs::read_dir(&dir.path)
		.expect("read dir")
		.map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
		.collect();
	assert_eq!(entries, vec!["subfolder"]);
}

#[cfg(unix)]
#[tokio::test]
async fn edit_preserves_symlink_structure_and_permissions() {
	use std::os::unix::fs::PermissionsExt;

	let dir = TempDir::new("edit-symlink-perms");
	let real_file = dir.path.join("real_script.sh");
	let original = "#!/bin/sh\necho original\n";
	std::fs::write(&real_file, original).expect("write real");
	std::fs::set_permissions(&real_file, std::fs::Permissions::from_mode(0o755)).expect("chmod");

	let link_file = dir.path.join("link_script.sh");
	std::os::unix::fs::symlink(&real_file, &link_file).expect("symlink");

	let ctx = pi_agent::AgentContext::new(dir.path.clone());
	let result = EditTool
		.execute(
			&json!({
				"path": "link_script.sh",
				"old_string": "original",
				"new_string": "updated"
			}),
			&ctx,
		)
		.await
		.expect("edit");

	assert!(!result.is_error);
	assert_eq!(
		std::fs::read_to_string(&real_file).expect("read real"),
		"#!/bin/sh\necho updated\n"
	);

	let symlink_meta = std::fs::symlink_metadata(&link_file).expect("symlink metadata");
	assert!(symlink_meta.file_type().is_symlink());

	let real_meta = std::fs::metadata(&real_file).expect("real metadata");
	assert_eq!(real_meta.permissions().mode() & 0o777, 0o755);
}

#[cfg(unix)]
#[tokio::test]
async fn write_preserves_restrictive_mode_on_existing_file() {
	use std::os::unix::fs::PermissionsExt;

	let dir = TempDir::new("write-mode-0600");
	let target = dir.path.join("secret.key");
	std::fs::write(&target, "old secret\n").expect("write secret");
	std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).expect("chmod");

	let ctx = pi_agent::AgentContext::new(dir.path.clone());
	let result = WriteTool
		.execute(
			&json!({
				"path": "secret.key",
				"content": "new secret content\n"
			}),
			&ctx,
		)
		.await
		.expect("write");

	assert!(!result.is_error);
	assert_eq!(
		std::fs::read_to_string(&target).expect("read secret"),
		"new secret content\n"
	);

	let meta = std::fs::metadata(&target).expect("metadata");
	assert_eq!(meta.permissions().mode() & 0o777, 0o600);
}

#[test]
fn registry_schemas_are_sorted_and_include_edit() {
	let registry = ToolRegistry::with_builtins();
	let names: Vec<String> = registry
		.schemas()
		.iter()
		.map(|schema| {
			schema["function"]["name"]
				.as_str()
				.unwrap_or_default()
				.to_string()
		})
		.collect();

	assert_eq!(names, vec!["bash", "edit", "glob", "grep", "read", "todo", "write"]);
}

#[tokio::test]
async fn grep_uses_regex_and_skips_non_files() {
	let dir = TempDir::new("grep");
	std::fs::write(dir.path.join("a.rs"), "fn main() {}\n").expect("write a");
	std::fs::write(dir.path.join("b.txt"), "no match here\n").expect("write b");
	std::fs::create_dir_all(dir.path.join("nested")).expect("mkdir");
	std::fs::write(dir.path.join("nested/c.rs"), "fn  main() {}\n").expect("write c");
	let ctx = pi_agent::AgentContext::new(dir.path.clone());

	let result = pi_agent::GrepTool
		.execute(&json!({"pattern": "fn\\s+main", "path": "."}), &ctx)
		.await
		.expect("grep");

	assert!(!result.is_error);
	assert!(result.content.contains("a.rs:1: fn main() {}"));
	assert!(result.content.contains("nested/c.rs:1: fn  main() {}"));

	let none = pi_agent::GrepTool
		.execute(&json!({"pattern": "zzz", "path": "."}), &ctx)
		.await
		.expect("grep");
	assert_eq!(none.content, "No matches found");
}

#[test]
fn sse_lines_parse_into_chunks_and_done() {
	assert!(parse_sse_line(": keep-alive").is_none());
	assert!(parse_sse_line("event: message").is_none());
	assert!(parse_sse_line("").is_none());

	let done = parse_sse_line("data: [DONE]").expect("done").expect("ok");
	assert!(done.is_done);

	let chunk = parse_sse_line(
		r#"data: {"id":"1","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}"#,
	)
	.expect("chunk")
	.expect("ok");
	assert_eq!(chunk.content.as_deref(), Some("hi"));
	assert!(!chunk.is_done);

	assert!(matches!(
		parse_sse_line("data: {not json}").expect("some"),
		Err(ProviderError::Serialization(_))
	));
}

#[tokio::test]
async fn sse_stream_handles_records_split_across_transport_chunks() {
	let body = concat!(
		r#"data: {"id":"1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":"{\"pa"}}]},"finish_reason":null}]}"#,
		"\n\ndata: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":\\\"a.txt\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
		r#"data: {"id":"1","choices":[{"index":0,"delta":{"content":"done"},"finish_reason":null}]}"#,
		"\n\ndata: [DONE]\n\n",
	);
	// Deliberately one byte per transport chunk: records are split mid-JSON.
	let byte_chunks: Vec<Result<Vec<u8>, String>> =
		body.as_bytes().iter().map(|byte| Ok(vec![*byte])).collect();
	let mut stream = sse_stream(futures::stream::iter(byte_chunks));

	let mut assembler = StreamAssembler::new();
	let mut saw_done = false;
	while let Some(chunk) = stream.next().await {
		let chunk = chunk.expect("chunk");
		saw_done |= chunk.is_done;
		assembler.push(&chunk);
	}

	assert!(saw_done);
	let message = assembler.finish().expect("assembler finish");
	assert_eq!(message.content, "done");
	assert_eq!(message.tool_calls.len(), 1);
	assert_eq!(message.tool_calls[0].id, "call_1");
	assert_eq!(message.tool_calls[0].name, "read");
	assert_eq!(message.tool_calls[0].arguments, json!({"path": "a.txt"}));
}

#[test]
fn provider_endpoint_tolerates_trailing_slash() {
	let provider =
		OpenAiCompatibleProvider::new("https://example.test/v1/", "key", "model-x").expect("build");
	assert_eq!(provider.endpoint(), "https://example.test/v1/chat/completions");
}

/// Streams pre-scripted chunks, one at a time, with no timers: the loop must
/// emit text deltas in arrival order.
struct StreamingProvider {
	turns: Mutex<VecDeque<Vec<StreamChunk>>>,
	stream_calls: AtomicUsize,
	complete_calls: AtomicUsize,
}

impl StreamingProvider {
	fn new(turns: Vec<Vec<StreamChunk>>) -> Self {
		Self {
			turns: Mutex::new(turns.into()),
			stream_calls: AtomicUsize::new(0),
			complete_calls: AtomicUsize::new(0),
		}
	}
}

#[async_trait]
impl ModelProvider for StreamingProvider {
	async fn complete(
		&self,
		_messages: &[Message],
		_tools: &[serde_json::Value],
	) -> Result<Message, ProviderError> {
		self.complete_calls.fetch_add(1, Ordering::SeqCst);
		Err(ProviderError::Protocol(
			"complete() must not run when an event sink is attached".to_string(),
		))
	}

	async fn stream(
		&self,
		_messages: &[Message],
		_tools: &[serde_json::Value],
	) -> Result<ChunkStream, ProviderError> {
		self.stream_calls.fetch_add(1, Ordering::SeqCst);
		let chunks = self
			.turns
			.lock()
			.expect("turn lock")
			.pop_front()
			.unwrap_or_default();
		let items: Vec<Result<StreamChunk, ProviderError>> = chunks.into_iter().map(Ok).collect();
		let stream: ChunkStream = Box::pin(futures::stream::iter(items));
		Ok(stream)
	}
}

fn text_chunk(delta: &str) -> StreamChunk {
	StreamChunk { content: Some(delta.to_string()), tool_call_deltas: Vec::new(), is_done: false }
}

fn tool_delta(
	index: usize,
	id: Option<&str>,
	name: Option<&str>,
	arguments_delta: Option<&str>,
) -> StreamChunk {
	StreamChunk {
		content: None,
		tool_call_deltas: vec![ToolCallDelta {
			index,
			id: id.map(ToString::to_string),
			name: name.map(ToString::to_string),
			arguments_delta: arguments_delta.map(ToString::to_string),
		}],
		is_done: false,
	}
}

fn done_chunk() -> StreamChunk {
	StreamChunk { content: None, tool_call_deltas: Vec::new(), is_done: true }
}

/// Drains the event channel concurrently with the dialog: `run_dialog_with`
/// sends with backpressure, so a consumer must keep up or size the channel.
fn spawn_drainer(
	rx: tokio::sync::mpsc::Receiver<AgentEvent>,
) -> tokio::task::JoinHandle<Vec<String>> {
	tokio::spawn(async move {
		let mut deltas = Vec::new();
		let mut rx = rx;
		while let Some(event) = rx.recv().await {
			match event {
				AgentEvent::MessageUpdate { delta } => deltas.push(delta),
				_ => {},
			}
		}
		deltas
	})
}

#[tokio::test]
async fn streaming_emits_text_deltas_in_arrival_order() {
	let dir = TempDir::new("streaming-text");
	let provider = Arc::new(StreamingProvider::new(vec![vec![
		text_chunk("Hel"),
		text_chunk("lo"),
		text_chunk(", world"),
		done_chunk(),
	]]));
	let agent =
		AgentLoop::new(config(&dir.path, 3), provider.clone(), ToolRegistry::with_builtins());
	let mut state = AgentState::default();
	let (events_tx, events_rx) = tokio::sync::mpsc::channel(8);
	let drainer = spawn_drainer(events_rx);

	let turns = agent
		.run_dialog_with("hi", &mut state, None, Some(events_tx))
		.await
		.expect("dialog");

	assert_eq!(drainer.await.expect("drainer"), vec!["Hel", "lo", ", world"]);
	assert_eq!(turns.len(), 1);
	assert!(turns[0].is_completed);
	assert_eq!(turns[0].message.content, "Hello, world");
	assert_eq!(
		state
			.messages
			.last()
			.map(|message| message.content.as_str()),
		Some("Hello, world")
	);
	assert_eq!(provider.stream_calls.load(Ordering::SeqCst), 1);
	assert_eq!(provider.complete_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn streaming_keeps_tool_calls_finalized_across_turns() {
	let dir = TempDir::new("streaming-tools");
	let provider = Arc::new(StreamingProvider::new(vec![
		vec![
			text_chunk("Checking"),
			tool_delta(0, Some("call_todo"), Some("todo"), Some("{\"action\":")),
			tool_delta(0, None, None, Some("\"list\"}")),
			done_chunk(),
		],
		vec![text_chunk("Done"), done_chunk()],
	]));
	let agent =
		AgentLoop::new(config(&dir.path, 3), provider.clone(), ToolRegistry::with_builtins());
	let mut state = AgentState::default();
	let (events_tx, events_rx) = tokio::sync::mpsc::channel(8);
	let drainer = spawn_drainer(events_rx);

	let turns = agent
		.run_dialog_with("go", &mut state, None, Some(events_tx))
		.await
		.expect("dialog");

	// Only text travels as events; tool calls arrive finalized in the turn result.
	assert_eq!(drainer.await.expect("drainer"), vec!["Checking", "Done"]);
	assert_eq!(turns.len(), 2);
	assert_eq!(turns[0].message.tool_calls.len(), 1);
	assert_eq!(turns[0].message.tool_calls[0].id, "call_todo");
	assert_eq!(turns[0].message.tool_calls[0].name, "todo");
	assert_eq!(turns[0].message.tool_calls[0].arguments, json!({"action": "list"}));
	assert_eq!(turns[0].tool_results.len(), 1);
	assert_eq!(turns[0].tool_results[0].tool_call_id, "call_todo");
	assert!(!turns[0].tool_results[0].is_error);
	assert!(turns[1].is_completed);
	assert_eq!(turns[1].message.content, "Done");
	assert_no_orphan_tool_calls(&state);
	assert_eq!(provider.stream_calls.load(Ordering::SeqCst), 2);
	assert_eq!(provider.complete_calls.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn streaming_rejects_missing_tool_name() {
	let mut assembler = StreamAssembler::new();
	assembler.push(&StreamChunk {
		content: Some("text".to_string()),
		tool_call_deltas: vec![ToolCallDelta {
			index: 0,
			id: Some("call_abc".to_string()),
			name: None,
			arguments_delta: Some("{\"path\":\"x\"}".to_string()),
		}],
		is_done: true,
	});

	let result = assembler.finish();
	assert!(result.is_err(), "StreamAssembler::finish must reject tool call without name");
	let err = result.unwrap_err();
	assert!(
		matches!(err, ProviderError::Protocol(_)),
		"Expected ProviderError::Protocol, got: {:?}",
		err
	);
}

#[tokio::test]
async fn streaming_dialog_fails_on_missing_tool_name() {
	let dir = TempDir::new("missing-name");
	let tools = ToolRegistry::new();

	let provider = StreamingProvider::new(vec![vec![
		text_chunk("running tool"),
		tool_delta(0, Some("call_missing"), None, Some("{\"path\":\"x\"}")),
		done_chunk(),
	]]);

	let (events_tx, events_rx) = tokio::sync::mpsc::channel(8);
	let _drainer = spawn_drainer(events_rx);

	let agent = AgentLoop::new(config(&dir.path, 2), std::sync::Arc::new(provider), tools);
	let mut state = AgentState::default();
	let result = agent
		.run_dialog_with("go", &mut state, None, Some(events_tx))
		.await;

	assert!(result.is_err(), "Dialog must fail when streamed tool call has no name");
	let err = result.unwrap_err();
	assert!(
		matches!(err, pi_agent::AgentError::Provider(ProviderError::Protocol(_))),
		"Expected AgentError::Provider(Protocol), got: {:?}",
		err
	);
}


#[tokio::test]
async fn edit_refuses_files_above_the_cap_without_touching_them() {
	// Mirrors `MAX_EDIT_BYTES`; the limit is enforced on the bytes actually read,
	// so a file that grew after `metadata` cannot lose its tail to a rewrite.
	const EDIT_CAP: usize = 4 * 1024 * 1024;
	let dir = TempDir::new("edit-cap");
	let path = dir.path.join("big.txt");
	std::fs::write(&path, "a".repeat(EDIT_CAP + 1)).expect("write");
	let ctx = pi_agent::AgentContext::new(dir.path.clone());

	let result = EditTool
		.execute(&json!({"path": "big.txt", "old_string": "a", "new_string": "b"}), &ctx)
		.await
		.expect("edit");

	assert!(result.is_error);
	assert!(result.content.contains("edit limit"), "got: {}", result.content);
	let bytes = std::fs::read(&path).expect("read");
	assert_eq!(bytes.len(), EDIT_CAP + 1);
	assert!(bytes.iter().all(|byte| *byte == b'a'), "file content must be untouched");
}

#[test]
fn reasoning_effort_is_serialized_only_when_set() {
	let without = OpenAiChatRequest {
		model: "model-x".to_string(),
		messages: Vec::new(),
		tools: Vec::new(),
		stream: false,
		reasoning_effort: None,
	};
	let payload = serde_json::to_value(&without).expect("serialize");
	assert!(payload.get("reasoning_effort").is_none(), "got: {payload}");

	let with = OpenAiChatRequest { reasoning_effort: Some("high".to_string()), ..without };
	let payload = serde_json::to_value(&with).expect("serialize");
	assert_eq!(payload["reasoning_effort"], json!("high"));
}

#[test]
fn effort_builder_normalizes_blank_values() {
	let provider = OpenAiCompatibleProvider::new("https://example.test/v1", "key", "model-x")
		.expect("build")
		.with_effort(Some("   ".to_string()));
	assert_eq!(provider.effort(), None);

	let provider = provider.with_effort(Some("high".to_string()));
	assert_eq!(provider.effort(), Some("high"));

	let provider = provider.with_effort(None);
	assert_eq!(provider.effort(), None);
}

#[test]
fn message_name_reaches_the_openai_payload() {
	let mut message = Message::user("hello");
	message.name = Some("alice".to_string());

	let payload = pi_agent::to_openai_messages(std::slice::from_ref(&message));
	let json = serde_json::to_value(&payload[0]).expect("serialize");

	assert_eq!(json["role"], json!("user"));
	assert_eq!(json["name"], json!("alice"));
}

/// Stream provider that yields chunks from a channel.
struct ChannelProvider {
	stream: Mutex<Option<ChunkStream>>,
}

impl ChannelProvider {
	fn new(stream: ChunkStream) -> Self {
		Self { stream: Mutex::new(Some(stream)) }
	}
}

#[async_trait]
impl ModelProvider for ChannelProvider {
	async fn complete(
		&self,
		_messages: &[Message],
		_tools: &[serde_json::Value],
	) -> Result<Message, ProviderError> {
		Err(ProviderError::Protocol("streaming only".to_string()))
	}

	async fn stream(
		&self,
		_messages: &[Message],
		_tools: &[serde_json::Value],
	) -> Result<ChunkStream, ProviderError> {
		self
			.stream
			.lock()
			.expect("stream lock")
			.take()
			.ok_or_else(|| ProviderError::Protocol("exhausted".to_string()))
	}
}

struct StalledStreamProvider;

#[async_trait]
impl ModelProvider for StalledStreamProvider {
	async fn complete(
		&self,
		_messages: &[Message],
		_tools: &[serde_json::Value],
	) -> Result<Message, ProviderError> {
		futures::future::pending().await
	}

	async fn stream(
		&self,
		_messages: &[Message],
		_tools: &[serde_json::Value],
	) -> Result<ChunkStream, ProviderError> {
		futures::future::pending().await
	}
}

#[tokio::test]
async fn interrupt_stalled_provider_stream_setup() {
	let dir = TempDir::new("interrupt-stalled-stream");
	let provider = Arc::new(StalledStreamProvider);
	let agent = AgentLoop::new(config(&dir.path, 3), provider, ToolRegistry::with_builtins());
	let mut state = AgentState::default();
	let (steering_tx, steering_rx) = tokio::sync::mpsc::channel(4);
	let (events_tx, _events_rx) = tokio::sync::mpsc::channel(8);

	let mut dialog_task = tokio::spawn(async move {
		agent
			.run_dialog_with("hi", &mut state, Some(steering_rx), Some(events_tx))
			.await
	});

	tokio::time::sleep(std::time::Duration::from_millis(50)).await;
	steering_tx.send(SteeringEvent::Interrupt).await.unwrap();

	let res = tokio::time::timeout(std::time::Duration::from_secs(2), &mut dialog_task)
		.await
		.expect("dialog hung instead of interrupting stalled stream setup")
		.unwrap();

	assert!(matches!(res, Err(AgentError::Interrupted)));
}

#[tokio::test]
async fn first_message_update_readable_before_done() {
	let dir = TempDir::new("live-first-chunk");
	let (chunk_tx, chunk_rx) = futures::channel::mpsc::unbounded();
	let provider = Arc::new(ChannelProvider::new(Box::pin(chunk_rx)));
	let agent = AgentLoop::new(config(&dir.path, 3), provider, ToolRegistry::with_builtins());
	let mut state = AgentState::default();
	let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(8);

	// Provider emits first text chunk, but has NOT emitted done_chunk().
	chunk_tx
		.unbounded_send(Ok(text_chunk("first chunk")))
		.unwrap();

	let dialog_task = tokio::spawn(async move {
		agent
			.run_dialog_with("hi", &mut state, None, Some(events_tx))
			.await
	});

	// Expect the first message_update event while [DONE] has NOT been sent.
	let first_event = tokio::time::timeout(std::time::Duration::from_secs(2), events_rx.recv())
		.await
		.expect("timeout waiting for live event")
		.expect("events channel closed");
	assert_eq!(first_event, AgentEvent::MessageUpdate { delta: "first chunk".to_string() });

	// Now finish the stream with done_chunk()
	chunk_tx.unbounded_send(Ok(done_chunk())).unwrap();
	drop(chunk_tx);

	let res = dialog_task.await.unwrap();
	assert!(res.is_ok());
}

#[tokio::test]
async fn interrupt_pending_provider_stream() {
	let dir = TempDir::new("interrupt-stream");
	let (chunk_tx, chunk_rx) = futures::channel::mpsc::unbounded();
	let provider = Arc::new(ChannelProvider::new(Box::pin(chunk_rx)));
	let agent = AgentLoop::new(config(&dir.path, 3), provider, ToolRegistry::with_builtins());
	let mut state = AgentState::default();
	let (steering_tx, steering_rx) = tokio::sync::mpsc::channel(4);
	let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(8);

	// Provider emits partial chunk and then stays open/pending without finishing.
	chunk_tx
		.unbounded_send(Ok(text_chunk("partial text")))
		.unwrap();

	let mut dialog_task = tokio::spawn(async move {
		agent
			.run_dialog_with("hi", &mut state, Some(steering_rx), Some(events_tx))
			.await
	});

	// Wait until the partial event is observed
	let first_event = tokio::time::timeout(std::time::Duration::from_secs(2), events_rx.recv())
		.await
		.expect("timeout waiting for live event")
		.expect("events channel closed");
	assert_eq!(first_event, AgentEvent::MessageUpdate { delta: "partial text".to_string() });

	// Now interrupt the pending stream via steering channel
	steering_tx.send(SteeringEvent::Interrupt).await.unwrap();

	// The dialog task must return Err(AgentError::Interrupted) promptly without hanging!
	let res = tokio::time::timeout(std::time::Duration::from_secs(2), &mut dialog_task)
		.await
		.expect("dialog hung instead of interrupting pending stream")
		.unwrap();

	assert!(matches!(res, Err(AgentError::Interrupted)));
}

#[tokio::test]
async fn interrupt_two_tool_batch_without_orphan() {
	let dir = TempDir::new("interrupt-tools");
	let path1 = dir.path.join("file1.txt");
	let path2 = dir.path.join("file2.txt");
	std::fs::write(&path1, "content 1").unwrap();
	std::fs::write(&path2, "content 2").unwrap();

	// Provider requests 2 tools
	let provider = Arc::new(StreamingProvider::new(vec![vec![
		text_chunk("Calling tools"),
		tool_delta(
			0,
			Some("call_1"),
			Some("read"),
			Some(&format!("{{\"path\": \"{}\"}}", path1.display())),
		),
		tool_delta(
			1,
			Some("call_2"),
			Some("read"),
			Some(&format!("{{\"path\": \"{}\"}}", path2.display())),
		),
		done_chunk(),
	]]));

	let agent = AgentLoop::new(config(&dir.path, 3), provider, ToolRegistry::with_builtins());
	let mut state = AgentState::default();
	let (steering_tx, steering_rx) = tokio::sync::mpsc::channel(4);
	let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(16);

	let dialog_task = tokio::spawn(async move {
		let res = agent
			.run_dialog_with("go", &mut state, Some(steering_rx), Some(events_tx))
			.await;
		(res, state)
	});

	let mut events = Vec::new();
	while let Some(event) = events_rx.recv().await {
		let is_tool_start = matches!(event, AgentEvent::ToolStart { .. });
		events.push(event);
		if is_tool_start {
			let _ = steering_tx.send(SteeringEvent::Interrupt).await;
			break;
		}
	}

	while let Some(event) = events_rx.recv().await {
		events.push(event);
		if events
			.iter()
			.filter(|e| matches!(e, AgentEvent::ToolEnd { .. }))
			.count()
			>= 2
		{
			break;
		}
	}

	let (res, state) = dialog_task.await.unwrap();
	assert!(matches!(res, Err(AgentError::Interrupted)));
	while let Ok(event) = events_rx.try_recv() {
		events.push(event);
	}

	let tool_starts: Vec<_> = events
		.iter()
		.filter_map(|e| match e {
			AgentEvent::ToolStart { id, name, .. } => Some((id.clone(), name.clone())),
			_ => None,
		})
		.collect();
	assert_eq!(
		tool_starts,
		vec![("call_1".to_string(), "read".to_string()), ("call_2".to_string(), "read".to_string())]
	);

	let tool_ends: Vec<_> = events
		.iter()
		.filter_map(|e| match e {
			AgentEvent::ToolEnd { id, name, is_error, .. } => {
				Some((id.clone(), name.clone(), *is_error))
			},
			_ => None,
		})
		.collect();
	assert_eq!(tool_ends.len(), 2);
	assert_eq!(tool_ends[0].0, "call_1");
	assert_eq!(tool_ends[1].0, "call_2");

	// Verify no orphan tool calls in history
	assert_no_orphan_tool_calls(&state);
}

#[tokio::test]
async fn sse_toolbatch_ordering_message_end_before_tool_start_and_turn_end_status() {
	let dir = TempDir::new("ordering-envelope");
	let file = dir.path.join("data.txt");
	std::fs::write(&file, "hello world").unwrap();

	let provider = Arc::new(StreamingProvider::new(vec![
		vec![
			text_chunk("Calling read tool..."),
			tool_delta(
				0,
				Some("call_1"),
				Some("read"),
				Some(&format!("{{\"path\": \"{}\"}}", file.display())),
			),
			done_chunk(),
		],
		vec![text_chunk("File content read successfully."), done_chunk()],
	]));

	let agent = AgentLoop::new(config(&dir.path, 5), provider, ToolRegistry::with_builtins());
	let mut state = AgentState::default();
	let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(32);

	let dialog_task = tokio::spawn(async move {
		agent
			.run_dialog_with("read file", &mut state, None, Some(events_tx))
			.await
	});

	let mut events = Vec::new();
	while let Some(event) = events_rx.recv().await {
		events.push(event);
	}

	let res = dialog_task.await.unwrap();
	assert!(res.is_ok());

	// Find indexes of events
	let msg_end_1 = events
		.iter()
		.position(|e| matches!(e, AgentEvent::MessageEnd { message, status: TurnStatus::ToolUse } if !message.tool_calls.is_empty()))
		.expect("first MessageEnd");
	let tool_start_1 = events
		.iter()
		.position(|e| matches!(e, AgentEvent::ToolStart { id, .. } if id == "call_1"))
		.expect("ToolStart");
	let tool_end_1 = events
		.iter()
		.position(|e| matches!(e, AgentEvent::ToolEnd { id, .. } if id == "call_1"))
		.expect("ToolEnd");
	let turn_end_1 = events
		.iter()
		.position(|e| matches!(e, AgentEvent::TurnEnd { status: TurnStatus::ToolUse, .. }))
		.expect("TurnEnd ToolUse");

	// MessageEnd MUST precede ToolStart
	assert!(msg_end_1 < tool_start_1, "MessageEnd must be emitted before ToolStart");
	assert!(tool_start_1 < tool_end_1, "ToolStart must be emitted before ToolEnd");
	assert!(tool_end_1 < turn_end_1, "ToolEnd must be emitted before TurnEnd");

	// Second turn: EndTurn
	let msg_end_2 = events
		.iter()
		.position(|e| matches!(e, AgentEvent::MessageEnd { message, status: TurnStatus::EndTurn } if message.tool_calls.is_empty()))
		.expect("second MessageEnd");
	let turn_end_2 = events
		.iter()
		.position(|e| matches!(e, AgentEvent::TurnEnd { status: TurnStatus::EndTurn, .. }))
		.expect("TurnEnd EndTurn");

	assert!(turn_end_1 < msg_end_2);
	assert!(msg_end_2 < turn_end_2);

	// Exactly two MessageEnd and two TurnEnd
	let message_ends = events
		.iter()
		.filter(|e| matches!(e, AgentEvent::MessageEnd { .. }))
		.count();
	let turn_ends = events
		.iter()
		.filter(|e| matches!(e, AgentEvent::TurnEnd { .. }))
		.count();
	assert_eq!(message_ends, 2);
	assert_eq!(turn_ends, 2);
}

#[tokio::test]
async fn mid_stream_abort_discards_incomplete_tool_calls() {
	let dir = TempDir::new("mid-stream-abort");
	let (chunk_tx, chunk_rx) = futures::channel::mpsc::unbounded();
	let provider = Arc::new(ChannelProvider::new(Box::pin(chunk_rx)));
	let agent = AgentLoop::new(config(&dir.path, 3), provider, ToolRegistry::with_builtins());
	let mut state = AgentState::default();
	let (steering_tx, steering_rx) = tokio::sync::mpsc::channel(4);
	let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(16);

	// Send partial text and partial tool call, but DO NOT finish the stream
	chunk_tx
		.unbounded_send(Ok(text_chunk("thinking about tool")))
		.unwrap();
	chunk_tx
		.unbounded_send(Ok(tool_delta(0, Some("incomplete_call"), Some("read"), None)))
		.unwrap();

	let dialog_task = tokio::spawn(async move {
		agent
			.run_dialog_with("test abort", &mut state, Some(steering_rx), Some(events_tx))
			.await
	});

	// Wait for the text delta
	let first = events_rx.recv().await.expect("first event");
	assert!(matches!(first, AgentEvent::MessageUpdate { .. }));

	// Abort stream before done
	steering_tx.send(SteeringEvent::Interrupt).await.unwrap();

	let mut events = Vec::new();
	while let Some(event) = events_rx.recv().await {
		events.push(event);
	}

	let res = dialog_task.await.unwrap();
	assert!(matches!(res, Err(AgentError::Interrupted)));

	// Verify MessageEnd has tool_calls discarded
	let msg_end = events
		.iter()
		.find_map(|e| match e {
			AgentEvent::MessageEnd { message, status } => Some((message, status)),
			_ => None,
		})
		.expect("MessageEnd event must be emitted on abort");
	assert_eq!(msg_end.1, &TurnStatus::Aborted);
	assert!(
		msg_end.0.tool_calls.is_empty(),
		"Incomplete tool calls must be discarded on mid-stream abort"
	);
	assert_eq!(msg_end.0.content, "thinking about tool");
	// TurnEnd has Aborted status
	let turn_end = events
		.iter()
		.find_map(|e| match e {
			AgentEvent::TurnEnd { status, tool_results, .. } => Some((status, tool_results)),
			_ => None,
		})
		.expect("TurnEnd event must be emitted on abort");
	assert_eq!(turn_end.0, &TurnStatus::Aborted);
	assert!(turn_end.1.is_empty());

	// No ToolStart was emitted
	assert!(
		!events
			.iter()
			.any(|e| matches!(e, AgentEvent::ToolStart { .. }))
	);
}

#[tokio::test]
async fn batch_abort_paired_results_in_provider_context() {
	let dir = TempDir::new("batch-abort-paired");
	let f1 = dir.path.join("f1.txt");
	let f2 = dir.path.join("f2.txt");
	std::fs::write(&f1, "one").unwrap();
	std::fs::write(&f2, "two").unwrap();

	let provider = Arc::new(StreamingProvider::new(vec![vec![
		text_chunk("running batch"),
		tool_delta(0, Some("c1"), Some("read"), Some(&format!("{{\"path\":\"{}\"}}", f1.display()))),
		tool_delta(1, Some("c2"), Some("read"), Some(&format!("{{\"path\":\"{}\"}}", f2.display()))),
		done_chunk(),
	]]));

	let agent = AgentLoop::new(config(&dir.path, 3), provider, ToolRegistry::with_builtins());
	let mut state = AgentState::default();
	let (steering_tx, steering_rx) = tokio::sync::mpsc::channel(4);
	let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(16);

	let dialog_task = tokio::spawn(async move {
		let res = agent
			.run_dialog_with("go", &mut state, Some(steering_rx), Some(events_tx))
			.await;
		(res, state)
	});

	// Interrupt as soon as first tool start is seen
	while let Some(event) = events_rx.recv().await {
		if matches!(event, AgentEvent::ToolStart { .. }) {
			let _ = steering_tx.send(SteeringEvent::Interrupt).await;
			break;
		}
	}

	let mut events = Vec::new();
	while let Some(event) = events_rx.recv().await {
		events.push(event);
	}

	let (res, state) = dialog_task.await.unwrap();
	assert!(matches!(res, Err(AgentError::Interrupted)));

	// TurnEnd has Aborted status and 2 paired results
	let turn_end = events
		.iter()
		.find_map(|e| match e {
			AgentEvent::TurnEnd { status, tool_results, .. } => Some((status, tool_results)),
			_ => None,
		})
		.expect("TurnEnd on batch abort");
	assert_eq!(turn_end.0, &TurnStatus::Aborted);
	assert_eq!(turn_end.1.len(), 2);

	// Replayed provider context in state has assistant message and paired tool responses
	let assistant_msg = state
		.messages
		.iter()
		.find(|m| m.role == Role::Assistant)
		.expect("assistant message in state");
	assert_eq!(assistant_msg.tool_calls.len(), 2);

	let tool_msgs: Vec<_> = state
		.messages
		.iter()
		.filter(|m| m.role == Role::Tool)
		.collect();
	assert_eq!(tool_msgs.len(), 2);
	assert_eq!(tool_msgs[0].tool_call_id.as_deref(), Some("c1"));
	assert_eq!(tool_msgs[1].tool_call_id.as_deref(), Some("c2"));
}
