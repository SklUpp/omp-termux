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
	CancellationToken, ChunkStream, EditTool, GlobTool, GrepTool, ImagePart, Message, ModelProvider,
	OpenAiCompatibleProvider, ProviderError, ReadTool, Role, SteeringEvent,
	StreamAssembler, StreamChunk, Tool, ToolCall, ToolCallDelta, ToolRegistry, TurnStatus,
	WriteTool, parse_sse_line, sse_stream,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
		self.check_support(_messages)?;
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
		self.check_support(_messages)?;
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

#[tokio::test]
async fn grep_pattern_and_skip_validation() {
	let dir = TempDir::new("grep-validation");
	std::fs::write(dir.path.join("file.txt"), "hello world\n").expect("write");
	let ctx = pi_agent::AgentContext::new(dir.path.clone());

	// Empty pattern rejected
	let res_empty = GrepTool.execute(&json!({"pattern": ""}), &ctx).await.expect("grep empty");
	assert!(res_empty.is_error);
	assert_eq!(res_empty.content, "Pattern must not be empty");

	// Whitespace-only pattern rejected
	let res_ws = GrepTool.execute(&json!({"pattern": "   \t \n "}), &ctx).await.expect("grep ws");
	assert!(res_ws.is_error);
	assert_eq!(res_ws.content, "Pattern must not be empty");

	// Invalid regex pattern rejected
	let res_inv = GrepTool.execute(&json!({"pattern": "(unclosed"}), &ctx).await.expect("grep invalid");
	assert!(res_inv.is_error);
	assert!(res_inv.content.starts_with("Invalid regex pattern '(unclosed':"));

	// Negative skip rejected
	let res_neg_skip = GrepTool.execute(&json!({"pattern": "hello", "skip": -1}), &ctx).await.expect("grep neg skip");
	assert!(res_neg_skip.is_error);
	assert_eq!(res_neg_skip.content, "Skip must be a non-negative number");

	// Negative float skip rejected
	let res_neg_float = GrepTool.execute(&json!({"pattern": "hello", "skip": -0.5}), &ctx).await.expect("grep neg float");
	assert!(res_neg_float.is_error);
	assert_eq!(res_neg_float.content, "Skip must be a non-negative number");

	// String skip rejected
	let res_str_skip = GrepTool.execute(&json!({"pattern": "hello", "skip": "bad"}), &ctx).await.expect("grep str skip");
	assert!(res_str_skip.is_error);
	assert_eq!(res_str_skip.content, "Skip must be a non-negative number");

	// Boolean skip rejected
	let res_bool_skip = GrepTool.execute(&json!({"pattern": "hello", "skip": true}), &ctx).await.expect("grep bool skip");
	assert!(res_bool_skip.is_error);
	assert_eq!(res_bool_skip.content, "Skip must be a non-negative number");

	// Null skip accepted and defaults to 0
	let res_null_skip = GrepTool.execute(&json!({"pattern": "hello", "skip": null}), &ctx).await.expect("grep null skip");
	assert!(!res_null_skip.is_error);
	assert!(res_null_skip.content.contains("file.txt:1: hello world"));
}

#[tokio::test]
async fn grep_case_sensitivity() {
	let dir = TempDir::new("grep-case");
	std::fs::write(dir.path.join("upper.txt"), "HELLO WORLD\n").expect("write upper");
	std::fs::write(dir.path.join("lower.txt"), "hello world\n").expect("write lower");
	let ctx = pi_agent::AgentContext::new(dir.path.clone());

	// Default: case-sensitive
	let res_def = GrepTool.execute(&json!({"pattern": "hello"}), &ctx).await.expect("grep def");
	assert!(!res_def.is_error);
	assert!(res_def.content.contains("lower.txt:1: hello world"));
	assert!(!res_def.content.contains("upper.txt"));

	// Explicit case: true -> case-sensitive
	let res_case_true = GrepTool.execute(&json!({"pattern": "hello", "case": true}), &ctx).await.expect("grep case true");
	assert!(!res_case_true.is_error);
	assert!(res_case_true.content.contains("lower.txt:1: hello world"));
	assert!(!res_case_true.content.contains("upper.txt"));

	// Explicit case: false -> case-insensitive
	let res_case_false = GrepTool.execute(&json!({"pattern": "hello", "case": false}), &ctx).await.expect("grep case false");
	assert!(!res_case_false.is_error);
	assert!(res_case_false.content.contains("lower.txt:1: hello world"));
	assert!(res_case_false.content.contains("upper.txt:1: HELLO WORLD"));
}

#[tokio::test]
async fn grep_gitignore_behavior() {
	let dir = TempDir::new("grep-gitignore");
	std::fs::write(dir.path.join(".gitignore"), "secret.txt\n").expect("write gitignore");
	std::fs::write(dir.path.join("tracked.txt"), "common needle\n").expect("write tracked");
	std::fs::write(dir.path.join("secret.txt"), "common needle\n").expect("write secret");
	let ctx = pi_agent::AgentContext::new(dir.path.clone());

	// Default: respects gitignore
	let res_def = GrepTool.execute(&json!({"pattern": "common needle"}), &ctx).await.expect("grep gitignore def");
	assert!(!res_def.is_error);
	assert!(res_def.content.contains("tracked.txt:1: common needle"));
	assert!(!res_def.content.contains("secret.txt"));

	// Explicit gitignore: true
	let res_git_true = GrepTool.execute(&json!({"pattern": "common needle", "gitignore": true}), &ctx).await.expect("grep gitignore true");
	assert!(!res_git_true.is_error);
	assert!(res_git_true.content.contains("tracked.txt:1: common needle"));
	assert!(!res_git_true.content.contains("secret.txt"));

	// Explicit gitignore: false -> finds in ignored files too
	let res_git_false = GrepTool.execute(&json!({"pattern": "common needle", "gitignore": false}), &ctx).await.expect("grep gitignore false");
	assert!(!res_git_false.is_error);
	assert!(res_git_false.content.contains("tracked.txt:1: common needle"));
	assert!(res_git_false.content.contains("secret.txt:1: common needle"));
}

#[tokio::test]
async fn grep_skip_pagination() {
	let dir = TempDir::new("grep-pagination");
	std::fs::write(dir.path.join("a.txt"), "item match\n").expect("write a");
	std::fs::write(dir.path.join("b.txt"), "item match\n").expect("write b");
	std::fs::write(dir.path.join("c.txt"), "item match\n").expect("write c");
	let ctx = pi_agent::AgentContext::new(dir.path.clone());

	// skip: 0 returns all matching files
	let res_0 = GrepTool.execute(&json!({"pattern": "item match", "skip": 0}), &ctx).await.expect("grep skip 0");
	assert!(!res_0.is_error);
	assert!(res_0.content.contains("a.txt:1: item match"));
	assert!(res_0.content.contains("b.txt:1: item match"));
	assert!(res_0.content.contains("c.txt:1: item match"));

	// skip: 1 skips first matching file (a.txt)
	let res_1 = GrepTool.execute(&json!({"pattern": "item match", "skip": 1}), &ctx).await.expect("grep skip 1");
	assert!(!res_1.is_error);
	assert!(!res_1.content.contains("a.txt:"));
	assert!(res_1.content.contains("b.txt:1: item match"));
	assert!(res_1.content.contains("c.txt:1: item match"));

	// skip: 2 skips first two matching files (a.txt, b.txt)
	let res_2 = GrepTool.execute(&json!({"pattern": "item match", "skip": 2}), &ctx).await.expect("grep skip 2");
	assert!(!res_2.is_error);
	assert!(!res_2.content.contains("a.txt:"));
	assert!(!res_2.content.contains("b.txt:"));
	assert!(res_2.content.contains("c.txt:1: item match"));

	// skip: 3 is past the end -> informative notice, not error
	let res_3 = GrepTool.execute(&json!({"pattern": "item match", "skip": 3}), &ctx).await.expect("grep skip 3");
	assert!(!res_3.is_error);
	assert_eq!(res_3.content, "No more results (3 files total; skip=3 is past the end)");

	// Single-file scope does not paginate with skip
	let res_file_skip = GrepTool.execute(&json!({"pattern": "item match", "path": "a.txt", "skip": 5}), &ctx).await.expect("grep file skip");
	assert!(!res_file_skip.is_error);
	assert_eq!(res_file_skip.content.trim(), "a.txt:1: item match");
}

#[tokio::test]
async fn grep_root_guard() {
	let dir = TempDir::new("grep-root");
	let ctx = pi_agent::AgentContext::new(dir.path.clone());

	// Root "/" rejected
	let res_root = GrepTool.execute(&json!({"pattern": "test", "path": "/"}), &ctx).await.expect("grep root");
	assert!(res_root.is_error);
	assert_eq!(res_root.content, "Searching from root directory '/' is not allowed");

	// Multi-slash "///" rejected
	let res_slashes = GrepTool.execute(&json!({"pattern": "test", "path": "///"}), &ctx).await.expect("grep slashes");
	assert!(res_slashes.is_error);
	assert_eq!(res_slashes.content, "Searching from root directory '/' is not allowed");

	// Parent directory traversal resolving to root rejected
	let res_parent = GrepTool.execute(&json!({"pattern": "test", "path": "/tmp/.."}), &ctx).await.expect("grep parent");
	assert!(res_parent.is_error);
	assert_eq!(res_parent.content, "Searching from root directory '/' is not allowed");

	#[cfg(unix)]
	{
		let root_symlink = dir.path.join("root_link");
		std::os::unix::fs::symlink("/", &root_symlink).expect("create root symlink");
		let res_sym = GrepTool.execute(&json!({"pattern": "test", "path": "root_link"}), &ctx).await.expect("grep symlink");
		assert!(res_sym.is_error);
		assert_eq!(res_sym.content, "Searching from root directory '/' is not allowed");
	}
}

#[tokio::test]
async fn grep_path_validation_and_multipath() {
	let dir = TempDir::new("grep-multipath");
	std::fs::create_dir_all(dir.path.join("sub_a")).expect("mkdir sub_a");
	std::fs::create_dir_all(dir.path.join("sub_b")).expect("mkdir sub_b");
	std::fs::write(dir.path.join("sub_a/file1.txt"), "target word\n").expect("write file1");
	std::fs::write(dir.path.join("sub_b/file2.txt"), "target word\n").expect("write file2");
	let ctx = pi_agent::AgentContext::new(dir.path.clone());

	// Empty path rejected
	let res_empty = GrepTool.execute(&json!({"pattern": "target", "path": ""}), &ctx).await.expect("grep empty path");
	assert!(res_empty.is_error);
	assert_eq!(res_empty.content, "`path` must contain non-empty globs or paths");

	// Single missing path returns Path not found error
	let res_missing = GrepTool.execute(&json!({"pattern": "target", "path": "nonexistent"}), &ctx).await.expect("grep missing");
	assert!(res_missing.is_error);
	assert_eq!(res_missing.content, "Path not found: nonexistent");

	// Semicolon-delimited multi-path searches both targets
	let res_multi = GrepTool.execute(&json!({"pattern": "target", "path": "sub_a; sub_b"}), &ctx).await.expect("grep multi");
	assert!(!res_multi.is_error);
	assert!(res_multi.content.contains("sub_a/file1.txt:1: target word"));
	assert!(res_multi.content.contains("sub_b/file2.txt:1: target word"));

	// Partial missing path: surviving target searched, missing path noted
	let res_partial = GrepTool.execute(&json!({"pattern": "target", "path": "sub_a; missing_sub"}), &ctx).await.expect("grep partial");
	assert!(!res_partial.is_error);
	assert!(res_partial.content.contains("sub_a/file1.txt:1: target word"));
	assert!(res_partial.content.contains("Skipped missing paths: missing_sub"));

	// All missing paths returns Path not found error
	let res_all_missing = GrepTool.execute(&json!({"pattern": "target", "path": "missing_1; missing_2"}), &ctx).await.expect("grep all missing");
	assert!(res_all_missing.is_error);
	assert_eq!(res_all_missing.content, "Path not found: missing_1, missing_2");
}

#[tokio::test]
async fn grep_cancellation() {
	let dir = TempDir::new("grep-cancel");
	std::fs::write(dir.path.join("data.txt"), "searchable text\n").expect("write data");
	let mut ctx = pi_agent::AgentContext::new(dir.path.clone());

	ctx.cancellation_token.cancel();
	let res = GrepTool.execute(&json!({"pattern": "searchable"}), &ctx).await.expect("grep cancel");
	assert!(res.is_error);
	assert_eq!(res.content, "Grep search cancelled");
}

#[tokio::test]
async fn grep_glob_path_pattern() {
	let dir = TempDir::new("grep-glob");
	std::fs::create_dir_all(dir.path.join("src")).expect("mkdir src");
	std::fs::create_dir_all(dir.path.join("tests")).expect("mkdir tests");
	std::fs::write(dir.path.join("src/a.ts"), "target_code\n").expect("write a");
	std::fs::write(dir.path.join("src/b.rs"), "target_code\n").expect("write b");
	std::fs::write(dir.path.join("tests/c.ts"), "target_code\n").expect("write c");
	let ctx = pi_agent::AgentContext::new(dir.path.clone());

	// Single glob path filters to matching extension under base dir
	let res_glob = GrepTool
		.execute(&json!({"pattern": "target_code", "path": "src/**/*.ts"}), &ctx)
		.await
		.expect("grep glob");
	assert!(!res_glob.is_error);
	assert!(res_glob.content.contains("src/a.ts:1: target_code"));
	assert!(!res_glob.content.contains("src/b.rs"));
	assert!(!res_glob.content.contains("tests/c.ts"));

	// Multi-path with glob and plain directory
	let res_multi = GrepTool
		.execute(&json!({"pattern": "target_code", "path": "src/**/*.ts; tests"}), &ctx)
		.await
		.expect("grep multi glob");
	assert!(!res_multi.is_error);
	assert!(res_multi.content.contains("src/a.ts:1: target_code"));
	assert!(!res_multi.content.contains("src/b.rs"));
	assert!(res_multi.content.contains("tests/c.ts:1: target_code"));

	// Missing base dir returns Path not found
	let res_missing = GrepTool
		.execute(&json!({"pattern": "target_code", "path": "nonexistent_dir/**/*.ts"}), &ctx)
		.await
		.expect("grep missing glob base");
	assert!(res_missing.is_error);
	assert_eq!(res_missing.content, "Path not found: nonexistent_dir/**/*.ts");
}

#[tokio::test]
async fn grep_pagination_beyond_500_candidates() {
	let dir = TempDir::new("grep-pag-500");
	for i in 0..=500 {
		std::fs::write(dir.path.join(format!("file_{i:04}.txt")), "paged_needle\n").expect("write file");
	}
	let ctx = pi_agent::AgentContext::new(dir.path.clone());

	// skip: 500 must return the 501st file (file_0500.txt) rather than cutting off at 500 files
	let res_500 = GrepTool
		.execute(&json!({"pattern": "paged_needle", "skip": 500}), &ctx)
		.await
		.expect("grep skip 500");
	assert!(!res_500.is_error);
	assert!(res_500.content.contains("file_0500.txt:1: paged_needle"));

	// skip: 501 is past the end of all 501 files
	let res_501 = GrepTool
		.execute(&json!({"pattern": "paged_needle", "skip": 501}), &ctx)
		.await
		.expect("grep skip 501");
	assert!(!res_501.is_error);
	assert_eq!(res_501.content, "No more results (501 files total; skip=501 is past the end)");
}

#[tokio::test]
async fn grep_dedup_normalized_paths() {
	let dir = TempDir::new("grep-dedup");
	std::fs::create_dir_all(dir.path.join("nested")).expect("mkdir nested");
	std::fs::write(dir.path.join("a.txt"), "dedup_token\n").expect("write a");
	let ctx = pi_agent::AgentContext::new(dir.path.clone());

	// "a.txt" and "nested/../a.txt" reference the same file and must not duplicate results
	let res = GrepTool
		.execute(&json!({"pattern": "dedup_token", "path": "a.txt; nested/../a.txt"}), &ctx)
		.await
		.expect("grep dedup");
	assert!(!res.is_error);
	let match_lines: Vec<&str> = res.content.lines().filter(|l| l.contains("dedup_token")).collect();
	assert_eq!(match_lines.len(), 1);
	assert_eq!(match_lines[0], "a.txt:1: dedup_token");

	// skip: 1 past the single unique file reports 1 file total, not 2
	let res_skip = GrepTool
		.execute(&json!({"pattern": "dedup_token", "path": "a.txt; nested/../a.txt", "skip": 1}), &ctx)
		.await
		.expect("grep dedup skip");
	assert!(!res_skip.is_error);
	assert_eq!(res_skip.content, "No more results (1 files total; skip=1 is past the end)");
}

#[tokio::test]
async fn grep_bounded_file_matches_and_active_cancel() {
	let dir = TempDir::new("grep-bounded");
	// Write file with 300 matching lines
	let mut content = String::new();
	for i in 1..=300 {
		content.push_str(&format!("line {i} match_bulk\n"));
	}
	std::fs::write(dir.path.join("big.txt"), content).expect("write big");
	let mut ctx = pi_agent::AgentContext::new(dir.path.clone());

	// Result must be capped at MAX_GREP_MATCHES (100)
	let res = GrepTool
		.execute(&json!({"pattern": "match_bulk", "path": "big.txt"}), &ctx)
		.await
		.expect("grep bulk");
	assert!(!res.is_error);
	let match_count = res.content.lines().filter(|l| l.contains("match_bulk")).count();
	assert_eq!(match_count, 100);

	// Cancellation stops scanning immediately
	ctx.cancellation_token.cancel();
	let res_cancel = GrepTool
		.execute(&json!({"pattern": "match_bulk", "path": "big.txt"}), &ctx)
		.await
		.expect("grep cancel");
	assert!(res_cancel.is_error);
	assert_eq!(res_cancel.content, "Grep search cancelled");
}

#[tokio::test]
async fn grep_sparse_matches_pagination() {
	let dir = TempDir::new("grep-sparse");
	for i in 0..1000 {
		let line = if i == 0 || i == 999 {
			"sparse_target_needle\n"
		} else {
			"regular content\n"
		};
		std::fs::write(dir.path.join(format!("file_{i:04}.txt")), line).expect("write");
	}
	let ctx = pi_agent::AgentContext::new(dir.path.clone());

	// skip: 1 skips first matching file (file_0000.txt) and successfully finds the late match in file_0999.txt
	let res_1 = GrepTool
		.execute(&json!({"pattern": "sparse_target_needle", "skip": 1}), &ctx)
		.await
		.expect("grep sparse skip 1");
	assert!(!res_1.is_error);
	assert!(res_1.content.contains("file_0999.txt:1: sparse_target_needle"));
	assert!(!res_1.content.contains("file_0000.txt"));
	assert!(!res_1.content.contains("past the end"));

	// skip: 2 is past the end of the 2 matching files
	let res_2 = GrepTool
		.execute(&json!({"pattern": "sparse_target_needle", "skip": 2}), &ctx)
		.await
		.expect("grep sparse skip 2");
	assert!(!res_2.is_error);
	assert_eq!(res_2.content, "No more results (2 files total; skip=2 is past the end)");
}

#[tokio::test]
async fn grep_huge_skip_resource_ceiling() {
	let dir = TempDir::new("grep-huge-skip");
	std::fs::write(dir.path.join("a.txt"), "common_text\n").expect("write a");
	std::fs::write(dir.path.join("b.txt"), "common_text\n").expect("write b");
	std::fs::write(dir.path.join("c.txt"), "other\n").expect("write c");
	let ctx = pi_agent::AgentContext::new(dir.path.clone());

	// Enormous skip does not breach resource ceilings or hang
	let res = GrepTool
		.execute(&json!({"pattern": "common_text", "skip": 1_000_000}), &ctx)
		.await
		.expect("grep huge skip");
	assert!(!res.is_error);
	assert_eq!(res.content, "No more results (2 files total; skip=1000000 is past the end)");
}

#[tokio::test]
async fn grep_truncated_result_on_budget_limit() {
	let dir = TempDir::new("grep-truncation");
	for i in 0..2005 {
		let line = if i < 2 {
			"limit_target\n"
		} else {
			"filler\n"
		};
		std::fs::write(dir.path.join(format!("t_{i:04}.txt")), line).expect("write");
	}
	let ctx = pi_agent::AgentContext::new(dir.path.clone());

	// Exceeding MAX_GREP_FILES (2000) surfaces scan truncated notice with matches
	let res = GrepTool
		.execute(&json!({"pattern": "limit_target"}), &ctx)
		.await
		.expect("grep limit");
	assert!(!res.is_error);
	assert!(res.content.contains("t_0000.txt:1: limit_target"));
	assert!(res.content.contains("[Search limit reached: 2000 files scanned; scan truncated]"));

	// Empty matches on truncated scan honestly reports truncation, never claiming "past the end"
	let res_skip_trunc = GrepTool
		.execute(&json!({"pattern": "limit_target", "skip": 5000}), &ctx)
		.await
		.expect("grep skip trunc");
	assert!(!res_skip_trunc.is_error);
	assert!(res_skip_trunc.content.contains("Search limit reached (2000 files scanned); scan truncated"));
	assert!(!res_skip_trunc.content.contains("past the end"));
}

#[tokio::test]
async fn grep_shared_budget_across_multiple_scopes() {
	let dir = TempDir::new("grep-multi-scope-budget");
	std::fs::create_dir_all(dir.path.join("scope_a")).expect("mkdir a");
	std::fs::create_dir_all(dir.path.join("scope_b")).expect("mkdir b");
	for i in 0..1500 {
		std::fs::write(dir.path.join(format!("scope_a/a_{i:04}.txt")), "filler_content\n").expect("write a");
	}
	for i in 0..1000 {
		let line = if i == 999 {
			"beyond_budget_needle\n"
		} else {
			"filler_content\n"
		};
		std::fs::write(dir.path.join(format!("scope_b/b_{i:04}.txt")), line).expect("write b");
	}
	let ctx = pi_agent::AgentContext::new(dir.path.clone());

	// Total files across scope_a (1500) and scope_b (1000) is 2500, which exceeds the shared 2000 budget.
	// The late match in scope_b/b_0999.txt lies beyond the 2000 budget window.
	// It must report search limit reached and truncation, never falsely claiming 'past the end' or 'No matches found'.
	let res = GrepTool
		.execute(
			&json!({"pattern": "beyond_budget_needle", "path": "scope_a; scope_b"}),
			&ctx,
		)
		.await
		.expect("grep multi scope budget");
	assert!(!res.is_error);
	assert!(res.content.contains("Search limit reached (2000 files scanned); scan truncated"));
	assert!(!res.content.contains("past the end"));
}

#[tokio::test]
async fn grep_exact_2000_files_no_false_truncation() {
	let dir = TempDir::new("grep-exact-2000");
	for i in 0..2000 {
		std::fs::write(dir.path.join(format!("f_{i:04}.txt")), "exact_match\n").expect("write f");
	}
	let ctx = pi_agent::AgentContext::new(dir.path.clone());

	// Exactly 2000 files fully inspected with zero discarded entries must not trigger false truncation
	let res_past_end = GrepTool
		.execute(&json!({"pattern": "exact_match", "skip": 5000}), &ctx)
		.await
		.expect("grep exact 2000 skip");
	assert!(!res_past_end.is_error);
	assert_eq!(
		res_past_end.content,
		"No more results (2000 files total; skip=5000 is past the end)"
	);
	assert!(!res_past_end.content.contains("Search limit reached"));
	assert!(!res_past_end.content.contains("scan truncated"));
}

#[tokio::test]
async fn grep_exact_2000_files_overlapping_and_subsequent_scopes() {
	let dir = TempDir::new("grep-exact-2000-overlap");
	let dir_a = dir.path.join("a");
	let dir_b = dir.path.join("b");
	let dir_empty = dir.path.join("empty_dir");
	std::fs::create_dir_all(&dir_a).expect("mkdir a");
	std::fs::create_dir_all(&dir_b).expect("mkdir b");
	std::fs::create_dir_all(&dir_empty).expect("mkdir empty");

	for i in 0..2000 {
		std::fs::write(dir_a.join(format!("f_{i:04}.txt")), "exact_match\n").expect("write a");
	}
	std::fs::write(dir_b.join("new_file.txt"), "exact_match\n").expect("write b");

	let ctx = pi_agent::AgentContext::new(dir.path.clone());

	// Case 1: "a; a" (duplicate directory scope with 2000 files in a) => no truncation notice
	let res_dup_dir = GrepTool
		.execute(&json!({"pattern": "exact_match", "path": "a; a", "skip": 5000}), &ctx)
		.await
		.expect("grep a; a");
	assert!(!res_dup_dir.is_error);
	assert_eq!(
		res_dup_dir.content,
		"No more results (2000 files total; skip=5000 is past the end)"
	);
	assert!(!res_dup_dir.content.contains("Search limit reached"));
	assert!(!res_dup_dir.content.contains("scan truncated"));

	// Case 2: "a; a/f_0001.txt" (directory scope + duplicate direct file) => no truncation notice
	let res_dup_file = GrepTool
		.execute(&json!({"pattern": "exact_match", "path": "a; a/f_0001.txt", "skip": 5000}), &ctx)
		.await
		.expect("grep a; a/f_0001.txt");
	assert!(!res_dup_file.is_error);
	assert_eq!(
		res_dup_file.content,
		"No more results (2000 files total; skip=5000 is past the end)"
	);
	assert!(!res_dup_file.content.contains("Search limit reached"));
	assert!(!res_dup_file.content.contains("scan truncated"));

	// Case 3: "a; empty_dir" (directory scope + empty directory) => no truncation notice
	let res_empty = GrepTool
		.execute(&json!({"pattern": "exact_match", "path": "a; empty_dir", "skip": 5000}), &ctx)
		.await
		.expect("grep a; empty_dir");
	assert!(!res_empty.is_error);
	assert_eq!(
		res_empty.content,
		"No more results (2000 files total; skip=5000 is past the end)"
	);
	assert!(!res_empty.content.contains("Search limit reached"));
	assert!(!res_empty.content.contains("scan truncated"));

	// Case 4: "a; b" (directory a with 2000 files + directory b with new file) => honest truncation notice
	let res_new_file = GrepTool
		.execute(&json!({"pattern": "exact_match", "path": "a; b", "skip": 5000}), &ctx)
		.await
		.expect("grep a; b");
	assert!(!res_new_file.is_error);
	assert!(res_new_file.content.contains("Search limit reached (2000 files scanned); scan truncated"));
	assert!(!res_new_file.content.contains("past the end"));
}

#[tokio::test]
async fn glob_hidden_flag_behavior() {
	let dir = TempDir::new("glob-hidden");
	std::fs::write(dir.path.join("visible.txt"), "hello").expect("write visible");
	std::fs::write(dir.path.join(".hidden.txt"), "secret").expect("write hidden");
	let ctx = pi_agent::AgentContext::new(dir.path.clone());

	// Default includes hidden files
	let res_default = GlobTool.execute(&json!({"path": "*"}), &ctx).await.expect("glob default");
	assert!(!res_default.is_error);
	assert!(res_default.content.contains("visible.txt"));
	assert!(res_default.content.contains(".hidden.txt"));

	// Explicit hidden: true
	let res_hidden_true = GlobTool.execute(&json!({"path": "*", "hidden": true}), &ctx).await.expect("glob hidden true");
	assert!(!res_hidden_true.is_error);
	assert!(res_hidden_true.content.contains("visible.txt"));
	assert!(res_hidden_true.content.contains(".hidden.txt"));

	// Explicit hidden: false excludes hidden files
	let res_hidden_false = GlobTool.execute(&json!({"path": "*", "hidden": false}), &ctx).await.expect("glob hidden false");
	assert!(!res_hidden_false.is_error);
	assert!(res_hidden_false.content.contains("visible.txt"));
	assert!(!res_hidden_false.content.contains(".hidden.txt"));
}

#[tokio::test]
async fn glob_gitignore_behavior() {
	let dir = TempDir::new("glob-gitignore");
	std::fs::write(dir.path.join(".gitignore"), "ignored.log\n.env\n").expect("write gitignore");
	std::fs::write(dir.path.join("tracked.txt"), "tracked").expect("write tracked");
	std::fs::write(dir.path.join("ignored.log"), "ignore me").expect("write ignored");
	std::fs::write(dir.path.join(".env"), "secret=1").expect("write env");
	let ctx = pi_agent::AgentContext::new(dir.path.clone());

	// Default respects gitignore
	let res_default = GlobTool.execute(&json!({"path": "*"}), &ctx).await.expect("glob default");
	assert!(!res_default.is_error);
	assert!(res_default.content.contains("tracked.txt"));
	assert!(!res_default.content.contains("ignored.log"));
	assert!(!res_default.content.contains(".env"));

	// gitignore: false includes ignored files
	let res_no_ignore = GlobTool.execute(&json!({"path": "*", "gitignore": false}), &ctx).await.expect("glob no ignore");
	assert!(!res_no_ignore.is_error);
	assert!(res_no_ignore.content.contains("tracked.txt"));
	assert!(res_no_ignore.content.contains("ignored.log"));
	assert!(res_no_ignore.content.contains(".env"));
}

#[tokio::test]
async fn glob_limits_and_validation() {
	let dir = TempDir::new("glob-limits");
	for i in 1..=5 {
		std::fs::write(dir.path.join(format!("file_{i}.txt")), "data").expect("write file");
	}
	let ctx = pi_agent::AgentContext::new(dir.path.clone());

	// Negative limit rejected with error
	let res_neg = GlobTool.execute(&json!({"path": "*", "limit": -5}), &ctx).await.expect("glob neg");
	assert!(res_neg.is_error);
	assert_eq!(res_neg.content, "Limit must be a positive number");

	// Zero limit rejected with error
	let res_zero = GlobTool.execute(&json!({"path": "*", "limit": 0}), &ctx).await.expect("glob zero");
	assert!(res_zero.is_error);
	assert_eq!(res_zero.content, "Limit must be a positive number");

	// Valid limit caps results
	let res_capped = GlobTool.execute(&json!({"path": "*", "limit": 2}), &ctx).await.expect("glob capped");
	assert!(!res_capped.is_error);
	let lines: Vec<&str> = res_capped.content.lines().filter(|l| !l.is_empty()).collect();
	assert_eq!(lines.len(), 2);

	// Ceiling clamped to 200 without error
	let res_ceiling = GlobTool.execute(&json!({"path": "*", "limit": 5000}), &ctx).await.expect("glob ceiling");
	assert!(!res_ceiling.is_error);

	// Non-numeric limit rejected with error
	let res_bad = GlobTool.execute(&json!({"path": "*", "limit": "invalid"}), &ctx).await.expect("glob bad limit");
	assert!(res_bad.is_error);
	assert_eq!(res_bad.content, "Limit must be a positive number");

	// Boolean limit rejected with error
	let res_bool = GlobTool.execute(&json!({"path": "*", "limit": false}), &ctx).await.expect("glob bool limit");
	assert!(res_bool.is_error);
	assert_eq!(res_bool.content, "Limit must be a positive number");

	// Null limit defaults without error
	let res_null = GlobTool.execute(&json!({"path": "*", "limit": null}), &ctx).await.expect("glob null limit");
	assert!(!res_null.is_error);
}

#[tokio::test]
async fn glob_multiple_paths_and_missing_paths() {
	let dir = TempDir::new("glob-multipath");
	std::fs::create_dir_all(dir.path.join("src")).expect("mkdir src");
	std::fs::create_dir_all(dir.path.join("test")).expect("mkdir test");
	std::fs::write(dir.path.join("src/lib.rs"), "pub fn lib() {}").expect("write src");
	std::fs::write(dir.path.join("test/lib_test.rs"), "pub fn test() {}").expect("write test");
	let ctx = pi_agent::AgentContext::new(dir.path.clone());

	// Semicolon-delimited multi-path
	let res_semi = GlobTool.execute(&json!({"path": "src/**/*.rs; test/**/*.rs"}), &ctx).await.expect("glob semi");
	assert!(!res_semi.is_error);
	assert!(res_semi.content.contains("lib.rs"));
	assert!(res_semi.content.contains("lib_test.rs"));

	// Array multi-path
	let res_arr = GlobTool.execute(&json!({"path": ["src/**/*.rs", "test/**/*.rs"]}), &ctx).await.expect("glob arr");
	assert!(!res_arr.is_error);
	assert!(res_arr.content.contains("lib.rs"));
	assert!(res_arr.content.contains("lib_test.rs"));

	// Partial missing path: surviving path returns results, missing path noted as warning
	let res_partial = GlobTool.execute(&json!({"path": "src/**/*.rs; missing_dir/**/*.rs"}), &ctx).await.expect("glob partial");
	assert!(!res_partial.is_error);
	assert!(res_partial.content.contains("lib.rs"));
	assert!(res_partial.content.contains("Skipped missing paths: missing_dir/**/*.rs"));

	// All missing paths: fails with error
	let res_all_missing = GlobTool.execute(&json!({"path": "missing_a; missing_b"}), &ctx).await.expect("glob all missing");
	assert!(res_all_missing.is_error);
	assert_eq!(res_all_missing.content, "Path not found: missing_a, missing_b");
}

#[tokio::test]
async fn glob_root_guard() {
	let dir = TempDir::new("glob-root");
	let ctx = pi_agent::AgentContext::new(dir.path.clone());

	// Root "/" rejected
	let res_root = GlobTool.execute(&json!({"path": "/"}), &ctx).await.expect("glob root");
	assert!(res_root.is_error);
	assert_eq!(res_root.content, "Searching from root directory '/' is not allowed");

	// Multi-slash "///" rejected
	let res_slashes = GlobTool.execute(&json!({"path": "///"}), &ctx).await.expect("glob slashes");
	assert!(res_slashes.is_error);
	assert_eq!(res_slashes.content, "Searching from root directory '/' is not allowed");

	// Absolute glob pattern starting with root "/" rejected
	let res_abs_root_glob = GlobTool.execute(&json!({"path": "/*.txt"}), &ctx).await.expect("glob abs root");
	assert!(res_abs_root_glob.is_error);
	assert_eq!(res_abs_root_glob.content, "Searching from root directory '/' is not allowed");

	// Parent directory traversal resolving to root rejected
	let res_parent_root = GlobTool.execute(&json!({"path": "/tmp/.."}), &ctx).await.expect("glob parent root");
	assert!(res_parent_root.is_error);
	assert_eq!(res_parent_root.content, "Searching from root directory '/' is not allowed");

	#[cfg(unix)]
	{
		let root_symlink = dir.path.join("link_to_root");
		std::os::unix::fs::symlink("/", &root_symlink).expect("create root symlink");
		let res_symlink = GlobTool.execute(&json!({"path": "link_to_root"}), &ctx).await.expect("glob symlink");
		assert!(res_symlink.is_error);
		assert_eq!(res_symlink.content, "Searching from root directory '/' is not allowed");

		// Symlink pointing to a deeper directory where lexical traversal would hit root,
		// but canonical resolution follows symlink target and avoids false-positive root rejection.
		let deeper_dir = dir.path.join("d1/d2/d3");
		std::fs::create_dir_all(&deeper_dir).expect("mkdir deeper_dir");
		let deep_link = dir.path.join("deep_link");
		std::os::unix::fs::symlink(&deeper_dir, &deep_link).expect("symlink deeper");

		let ups = dir.path.components().count();
		let res_deep_canonical = GlobTool
			.execute(
				&json!({"path": format!("{}/deep_link{}/*.txt", dir.path.display(), "/..".repeat(ups))}),
				&ctx,
			)
			.await
			.expect("glob deep canonical");
		assert!(!res_deep_canonical.is_error);
	}
	// Absolute path inside valid non-root directory stays legal
	std::fs::write(dir.path.join("ok.txt"), "ok").expect("write ok");
	let abs_path = format!("{}/*.txt", dir.path.display());
	let res_abs = GlobTool.execute(&json!({"path": abs_path}), &ctx).await.expect("glob abs");
	assert!(!res_abs.is_error);
	assert!(res_abs.content.contains("ok.txt"));
}

#[tokio::test]
async fn glob_single_missing_path_and_file_handling() {
	let dir = TempDir::new("glob-file");
	std::fs::write(dir.path.join("regular.txt"), "content").expect("write regular");
	let ctx = pi_agent::AgentContext::new(dir.path.clone());

	// Single missing path returns Path not found error
	let res_missing = GlobTool.execute(&json!({"path": "does_not_exist"}), &ctx).await.expect("glob missing");
	assert!(res_missing.is_error);
	assert_eq!(res_missing.content, "Path not found: does_not_exist");

	// Single file path without glob returns the file
	let res_file = GlobTool.execute(&json!({"path": "regular.txt"}), &ctx).await.expect("glob file");
	assert!(!res_file.is_error);
	assert_eq!(res_file.content, "regular.txt");

	// File with glob appended fails because it is not a directory
	let res_not_dir = GlobTool.execute(&json!({"path": "regular.txt/*.rs"}), &ctx).await.expect("glob not dir");
	assert!(res_not_dir.is_error);
	assert!(res_not_dir.content.contains("Path is not a directory"));
}

#[tokio::test]
async fn glob_cancellation_and_legacy_args() {
	let dir = TempDir::new("glob-cancel");
	std::fs::create_dir_all(dir.path.join("sub")).expect("mkdir sub");
	std::fs::write(dir.path.join("sub/item.rs"), "item").expect("write item");
	let mut ctx = pi_agent::AgentContext::new(dir.path.clone());

	// Legacy pattern + path arguments
	let res_legacy = GlobTool.execute(&json!({"path": "sub", "pattern": "*.rs"}), &ctx).await.expect("glob legacy");
	assert!(!res_legacy.is_error);
	assert!(res_legacy.content.contains("item.rs"));

	// Legacy pattern with exact filename (no wildcard) must filter accurately
	std::fs::write(dir.path.join("sub/README.md"), "readme").expect("write readme");
	let res_legacy_exact = GlobTool.execute(&json!({"path": "sub", "pattern": "README.md"}), &ctx).await.expect("glob legacy exact");
	assert!(!res_legacy_exact.is_error);
	assert_eq!(res_legacy_exact.content, "sub/README.md");
	assert!(!res_legacy_exact.content.contains("item.rs"));


	// Legacy pattern with non-matching filename: returns empty result (not Path not found)
	let res_legacy_missing = GlobTool.execute(&json!({"path": "sub", "pattern": "MISSING.md"}), &ctx).await.expect("glob legacy missing");
	assert!(!res_legacy_missing.is_error);
	assert_eq!(res_legacy_missing.content, "No files found matching pattern");

	// Legacy pattern matching a directory: returns the directory itself, not its descendants
	let sub_docs = dir.path.join("sub/docs");
	std::fs::create_dir_all(&sub_docs).expect("mkdir sub/docs");
	std::fs::write(sub_docs.join("child.txt"), "child").expect("write child");
	let res_legacy_dir = GlobTool.execute(&json!({"path": "sub", "pattern": "docs"}), &ctx).await.expect("glob legacy dir");
	assert!(!res_legacy_dir.is_error);
	assert!(res_legacy_dir.content.contains("sub/docs"));
	assert!(!res_legacy_dir.content.contains("child.txt"));

	// Legacy pattern with non-existent path: returns Path not found error for the path
	let res_legacy_no_path = GlobTool.execute(&json!({"path": "nonexistent_sub", "pattern": "README.md"}), &ctx).await.expect("glob legacy no path");
	assert!(res_legacy_no_path.is_error);
	assert_eq!(res_legacy_no_path.content, "Path not found: nonexistent_sub");
	// Empty path error
	// Pattern-only call defaults path to "." and filters accurately
	std::fs::write(dir.path.join("root_file.txt"), "root").expect("write root_file");
	let res_pattern_only = GlobTool.execute(&json!({"pattern": "root_file.txt"}), &ctx).await.expect("glob pattern only");
	assert!(!res_pattern_only.is_error);
	assert_eq!(res_pattern_only.content, "root_file.txt");

	let res_pattern_only_missing = GlobTool.execute(&json!({"pattern": "missing_root.txt"}), &ctx).await.expect("glob pattern only missing");
	assert!(!res_pattern_only_missing.is_error);
	assert_eq!(res_pattern_only_missing.content, "No files found matching pattern");

	let res_empty = GlobTool.execute(&json!({"path": ""}), &ctx).await.expect("glob empty");
	assert!(res_empty.is_error);
	assert_eq!(res_empty.content, "`path` must contain non-empty globs or paths");

	// Cancellation aborts with error result
	ctx.cancellation_token.cancel();
	let res_cancel = GlobTool.execute(&json!({"path": "sub"}), &ctx).await.expect("glob cancel");
	assert!(res_cancel.is_error);
	assert_eq!(res_cancel.content, "Glob scan cancelled");
}

#[tokio::test]
async fn glob_ranking_by_mtime_before_limit() {
	let dir = TempDir::new("glob-mtime-rank");
	let ctx = pi_agent::AgentContext::new(dir.path.clone());

	let old_file_path = dir.path.join("a_older.txt");
	let new_file_path = dir.path.join("z_newer.txt");

	let old_file = std::fs::File::create(&old_file_path).expect("create old");
	let new_file = std::fs::File::create(&new_file_path).expect("create new");

	let past = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
	old_file.set_times(std::fs::FileTimes::new().set_modified(past)).expect("set old file mtime");
	new_file.set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::now())).expect("set new file mtime");
	drop(old_file);
	drop(new_file);

	// Limit 1 must select newer file (z_newer.txt) instead of first lexicographical entry (a_older.txt)
	let res = GlobTool.execute(&json!({"path": "*.txt", "limit": 1}), &ctx).await.expect("glob mtime rank");
	assert!(!res.is_error);
	assert_eq!(res.content.trim(), "z_newer.txt");
}

#[tokio::test]
async fn glob_literal_paths_with_metacharacters() {
	let dir = TempDir::new("glob-literal-meta");
	let ctx = pi_agent::AgentContext::new(dir.path.clone());

	let id_dir = dir.path.join("[id]");
	std::fs::create_dir_all(&id_dir).expect("mkdir [id]");
	std::fs::write(id_dir.join("page.tsx"), "export default function Page() {}").expect("write page");
	std::fs::write(dir.path.join("[id].txt"), "literal file").expect("write [id].txt");

	// Existing literal directory containing [id] must not be parsed as character class matching 'i' or 'd'
	let res_dir = GlobTool.execute(&json!({"path": "[id]"}), &ctx).await.expect("glob dir literal");
	assert!(!res_dir.is_error);
	assert!(res_dir.content.contains("[id]/page.tsx"));

	// Existing literal file containing [id].txt returned directly
	let res_file = GlobTool.execute(&json!({"path": "[id].txt"}), &ctx).await.expect("glob file literal");
	assert!(!res_file.is_error);
	assert_eq!(res_file.content, "[id].txt");
	// Existing literal directory containing [id] combined with pattern filters by pattern
	let res_dir_pattern = GlobTool.execute(&json!({"path": "[id]", "pattern": "*.tsx"}), &ctx).await.expect("glob dir literal pattern");
	assert!(!res_dir_pattern.is_error);
	assert_eq!(res_dir_pattern.content, "[id]/page.tsx");
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

	let payload = pi_agent::to_openai_messages(std::slice::from_ref(&message)).expect("openai messages");
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

async fn spawn_mock_openai_server() -> (String, tokio::sync::oneshot::Receiver<serde_json::Value>) {
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
	let port = listener.local_addr().expect("local addr").port();
	let url = format!("http://127.0.0.1:{port}");
	let (tx, rx) = tokio::sync::oneshot::channel();

	tokio::spawn(async move {
		if let Ok((mut stream, _)) = listener.accept().await {
			let mut buf = vec![0u8; 16384];
			let mut read_bytes = 0;
			while read_bytes < buf.len() {
				let n = stream.read(&mut buf[read_bytes..]).await.unwrap_or(0);
				if n == 0 {
					break;
				}
				read_bytes += n;
				if let Some(pos) = buf[..read_bytes].windows(4).position(|w| w == b"\r\n\r\n") {
					let header_str = String::from_utf8_lossy(&buf[..pos]);
					let content_length: usize = header_str
						.lines()
						.find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
						.and_then(|l| l.split(':').nth(1))
						.and_then(|v| v.trim().parse().ok())
						.unwrap_or(0);
					if read_bytes >= pos + 4 + content_length {
						let body_bytes = &buf[pos + 4..pos + 4 + content_length];
						if let Ok(json_body) = serde_json::from_slice::<serde_json::Value>(body_bytes) {
							let _ = tx.send(json_body);
						}
						break;
					}
				}
			}
			let resp_body = serde_json::json!({
				"id": "chatcmpl-test",
				"object": "chat.completion",
				"created": 12345,
				"model": "gpt-4o",
				"choices": [{
					"index": 0,
					"message": {
						"role": "assistant",
						"content": "Saw the image clearly"
					},
					"finish_reason": "stop"
				}]
			});
			let resp_str = resp_body.to_string();
			let http_response = format!(
				"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
				resp_str.len(),
				resp_str
			);
			let _ = stream.write_all(http_response.as_bytes()).await;
			let _ = stream.flush().await;
		}
	});

	(url, rx)
}

#[tokio::test]
async fn openai_request_with_image_serializes_multipart_data_uri() {
	let (server_url, rx) = spawn_mock_openai_server().await;
	let provider = OpenAiCompatibleProvider::new(server_url, "fake-key", "gpt-4o")
		.expect("provider build");

	let image_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
	let msg = Message::user_with_images(
		"describe this image",
		vec![ImagePart::new("image/png", image_b64)],
	);

	assert_eq!(msg.content, "describe this image");
	assert!(msg.has_image());

	let reply = provider.complete(&[msg], &[]).await.expect("complete ok");
	assert_eq!(reply.content, "Saw the image clearly");

	let wire = rx.await.expect("server received request payload");
	let messages = wire["messages"].as_array().expect("messages array");
	assert_eq!(messages.len(), 1);
	assert_eq!(messages[0]["role"], "user");

	let content = messages[0]["content"].as_array().expect("content array");
	assert_eq!(content.len(), 2);
	assert_eq!(content[0]["type"], "text");
	assert_eq!(content[0]["text"], "describe this image");
	assert_eq!(content[1]["type"], "image_url");
	assert_eq!(
		content[1]["image_url"]["url"],
		format!("data:image/png;base64,{image_b64}")
	);
}

#[tokio::test]
async fn openai_request_with_existing_data_uri_preserves_url() {
	let (server_url, rx) = spawn_mock_openai_server().await;
	let provider = OpenAiCompatibleProvider::new(server_url, "fake-key", "gpt-4o")
		.expect("provider build");

	let data_uri = "data:image/jpeg;base64,/9j/4AAQSkZJRg==";
	let msg = Message::user_with_images("", vec![ImagePart::new("image/jpeg", data_uri)]);

	assert_eq!(msg.content, "");
	assert!(msg.has_image());

	let _ = provider.complete(&[msg], &[]).await.expect("complete ok");

	let wire = rx.await.expect("server received request payload");
	let messages = wire["messages"].as_array().expect("messages array");
	let content = messages[0]["content"].as_array().expect("content array");
	assert_eq!(content.len(), 1);
	assert_eq!(content[0]["type"], "image_url");
	assert_eq!(content[0]["image_url"]["url"], data_uri);
}

#[tokio::test]
async fn unsupported_image_provider_rejects_clearly() {
	let msg = Message::user_with_images(
		"look",
		vec![ImagePart::new("image/png", "abc")],
	);

	// 1. OpenAiCompatibleProvider with images disabled
	let provider = OpenAiCompatibleProvider::new("http://127.0.0.1:9", "fake-key", "text-only")
		.expect("provider build")
		.with_images(false);

	let err = provider.complete(&[msg.clone()], &[]).await.unwrap_err();
	assert!(
		matches!(err, ProviderError::UnsupportedImage(_)),
		"expected UnsupportedImage error, got: {:?}",
		err
	);

	let stream_err = match provider.stream(&[msg.clone()], &[]).await {
		Err(err) => err,
		Ok(_) => panic!("expected UnsupportedImage error on stream"),
	};
	assert!(
		matches!(stream_err, ProviderError::UnsupportedImage(_)),
		"expected UnsupportedImage error on stream, got: {:?}",
		stream_err
	);

	// 2. ScriptedProvider (mock double without multimodal support)
	let scripted = ScriptedProvider::new(vec![Ok(Message::assistant("never called"))]);
	let script_err = scripted.complete(&[msg], &[]).await.unwrap_err();
	assert!(
		matches!(script_err, ProviderError::UnsupportedImage(_)),
		"unsupported provider must reject clearly, got: {:?}",
		script_err
	);
}

#[test]
fn pure_text_user_message_wire_format_is_unaltered() {
	let msg = Message::user("plain text prompt");
	assert!(!msg.has_image());
	assert_eq!(msg.content, "plain text prompt");

	let wire_msgs = pi_agent::to_openai_messages(&[msg]).expect("openai messages");
	let wire_val = serde_json::to_value(&wire_msgs[0]).expect("serialize");
	assert_eq!(wire_val["role"], "user");
	assert_eq!(wire_val["content"], "plain text prompt");
}

#[test]
fn deserialized_message_with_content_and_image_only_parts_preserves_both() {
	let json_str = r#"{
		"role": "user",
		"content": "look at this",
		"parts": [
			{"type": "image", "data": "aW1n", "mime_type": "image/png"}
		]
	}"#;
	let msg: Message = serde_json::from_str(json_str).expect("deserialize");
	assert_eq!(msg.content, "look at this");
	assert!(msg.has_image());

	let wire = pi_agent::to_openai_messages(&[msg]).expect("openai messages");
	let val = serde_json::to_value(&wire[0]).expect("serialize");
	let content = val["content"].as_array().expect("content array");
	assert_eq!(content.len(), 2);
	assert_eq!(content[0]["type"], "text");
	assert_eq!(content[0]["text"], "look at this");
	assert_eq!(content[1]["type"], "image_url");
	assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,aW1n");
}

#[test]
fn deserialized_message_with_legacy_images_alias_preserves_both() {
	let json_str = r#"{
		"role": "user",
		"content": "legacy images field",
		"images": [
			{"data_base64": "aW1n", "mime_type": "image/png"}
		]
	}"#;
	let msg: Message = serde_json::from_str(json_str).expect("deserialize");
	assert_eq!(msg.content, "legacy images field");
	assert!(msg.has_image());

	let wire = pi_agent::to_openai_messages(&[msg]).expect("openai messages");
	let val = serde_json::to_value(&wire[0]).expect("serialize");
	let content = val["content"].as_array().expect("content array");
	assert_eq!(content.len(), 2);
	assert_eq!(content[0]["type"], "text");
	assert_eq!(content[0]["text"], "legacy images field");
	assert_eq!(content[1]["type"], "image_url");
	assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,aW1n");
}

#[test]
fn deserialized_message_with_legacy_image_parts_alias_preserves_both() {
	let json_str = r#"{
		"role": "user",
		"content": "legacy image_parts field",
		"image_parts": [
			{"data_base64": "aW1n", "mime_type": "image/png"}
		]
	}"#;
	let msg: Message = serde_json::from_str(json_str).expect("deserialize");
	assert_eq!(msg.content, "legacy image_parts field");
	assert!(msg.has_image());

	let wire = pi_agent::to_openai_messages(&[msg]).expect("openai messages");
	let val = serde_json::to_value(&wire[0]).expect("serialize");
	let content = val["content"].as_array().expect("content array");
	assert_eq!(content.len(), 2);
	assert_eq!(content[0]["type"], "text");
	assert_eq!(content[0]["text"], "legacy image_parts field");
	assert_eq!(content[1]["type"], "image_url");
	assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,aW1n");
}

#[test]
fn already_ordered_parts_with_text_and_images_does_not_duplicate_text() {
	let json_str = r#"{
		"role": "user",
		"content": "do not duplicate me",
		"parts": [
			{"type": "text", "text": "do not duplicate me"},
			{"type": "image", "data": "aW1n", "mime_type": "image/png"}
		]
	}"#;
	let msg: Message = serde_json::from_str(json_str).expect("deserialize");
	assert_eq!(msg.content, "do not duplicate me");
	assert_eq!(msg.parts.len(), 2, "parts must not duplicate text");

	let wire = pi_agent::to_openai_messages(&[msg]).expect("openai messages");
	let val = serde_json::to_value(&wire[0]).expect("serialize");
	let content = val["content"].as_array().expect("content array");
	assert_eq!(content.len(), 2);
	assert_eq!(content[0]["type"], "text");
	assert_eq!(content[0]["text"], "do not duplicate me");
	assert_eq!(content[1]["type"], "image_url");
}

#[test]
fn directly_constructed_message_with_content_and_image_part_preserves_both_on_wire() {
	let msg = Message {
		role: Role::User,
		content: "manually constructed prompt".to_string(),
		parts: vec![pi_agent::ContentPart::image("aW1n", "image/png").expect("img")],
		name: None,
		tool_calls: vec![],
		tool_call_id: None,
		is_error: false,
	};
	let wire = pi_agent::to_openai_messages(&[msg]).expect("openai messages");
	let val = serde_json::to_value(&wire[0]).expect("serialize");
	let content = val["content"].as_array().expect("content array");
	assert_eq!(content.len(), 2);
	assert_eq!(content[0]["type"], "text");
	assert_eq!(content[0]["text"], "manually constructed prompt");
	assert_eq!(content[1]["type"], "image_url");
	assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,aW1n");
}

#[test]
fn plain_text_user_message_omits_parts_in_json() {
	let msg = Message::user("hello");
	let val = serde_json::to_value(&msg).expect("serialize");
	assert!(val.get("image_parts").is_none(), "plain text message must not serialize empty parts");
	assert_eq!(val["content"], "hello");
}

#[test]
fn image_part_rejects_external_http_urls_and_invalid_data() {
	assert!(ImagePart::new("image/png", "http://example.com/pic.png").validate().is_err());
	assert!(ImagePart::new("image/png", "https://example.com/pic.png").validate().is_err());
	assert!(ImagePart::new("image/png", "//example.com/pic.png").validate().is_err());
	assert!(ImagePart::new("image/png", "ftp://example.com/pic.png").validate().is_err());
	assert!(ImagePart::new("text/plain", "aGVsbG8=").validate().is_err());
	assert!(ImagePart::new("image/png", "not-base64-???").validate().is_err());
	assert!(ImagePart::new("image/png", "data:text/plain;base64,aGVsbG8=").validate().is_err());
	assert!(ImagePart::new("image/png", "data:image/png;notb64,aGVsbG8=").validate().is_err());

	let valid_raw = ImagePart::new("image/png", "aGVsbG8=");
	assert!(valid_raw.validate().is_ok());
	assert_eq!(valid_raw.to_data_url().unwrap(), "data:image/png;base64,aGVsbG8=");

	let valid_data = ImagePart::new("image/jpeg", "data:image/jpeg;base64,/9j/4AAQSkZJRg==");
	assert!(valid_data.validate().is_ok());
	assert_eq!(valid_data.to_data_url().unwrap(), "data:image/jpeg;base64,/9j/4AAQSkZJRg==");
}

#[tokio::test]
async fn provider_rejects_external_image_url_payload() {
	let provider = OpenAiCompatibleProvider::new("http://127.0.0.1:9", "fake-key", "gpt-4o")
		.expect("provider build");
	let msg = Message::user_with_images(
		"steal data",
		vec![ImagePart::new("image/png", "https://attacker.com/leak.png")],
	);
	let err = provider.complete(&[msg], &[]).await.unwrap_err();
	assert!(
		matches!(err, ProviderError::UnsupportedImage(_)),
		"provider must reject external image URLs via UnsupportedImage, got: {:?}",
		err
	);
}

#[test]
fn ordered_image_text_image_parts_preserve_sequence_on_openai_wire() {
	let parts = vec![
		pi_agent::ContentPart::image("aW1nMQ==", "image/png").expect("img1"),
		pi_agent::ContentPart::text("middle description"),
		pi_agent::ContentPart::image("aW1nMg==", "image/jpeg").expect("img2"),
	];
	let msg = Message::user_with_parts(parts);
	assert!(msg.has_image());

	let wire = pi_agent::to_openai_messages(&[msg]).expect("openai messages");
	let val = serde_json::to_value(&wire[0]).expect("serialize");
	let content = val["content"].as_array().expect("content array");
	assert_eq!(content.len(), 3);
	assert_eq!(content[0]["type"], "image_url");
	assert_eq!(content[0]["image_url"]["url"], "data:image/png;base64,aW1nMQ==");
	assert_eq!(content[1]["type"], "text");
	assert_eq!(content[1]["text"], "middle description");
	assert_eq!(content[2]["type"], "image_url");
	assert_eq!(content[2]["image_url"]["url"], "data:image/jpeg;base64,aW1nMg==");
}

#[test]
fn multiple_text_parts_preserve_sequence_on_openai_wire() {
	let parts = vec![
		pi_agent::ContentPart::text("first point"),
		pi_agent::ContentPart::text("second point"),
		pi_agent::ContentPart::text("third point"),
	];
	let msg = Message::user_with_parts(parts);
	assert!(!msg.has_image());

	let wire = pi_agent::to_openai_messages(&[msg]).expect("openai messages");
	let val = serde_json::to_value(&wire[0]).expect("serialize");
	let content = val["content"].as_array().expect("content array");
	assert_eq!(content.len(), 3);
	assert_eq!(content[0]["text"], "first point");
	assert_eq!(content[1]["text"], "second point");
	assert_eq!(content[2]["text"], "third point");
}

#[test]
fn data_uri_mime_mismatch_is_rejected() {
	let err = ImagePart::new("image/jpeg", "data:image/png;base64,aW1n").validate().unwrap_err();
	assert!(err.contains("MIME type mismatch"), "expected mismatch error, got: {err}");
}

#[test]
fn secret_in_mime_or_type_or_part_does_not_leak_in_errors() {
	let secret_marker = "SUPER_SECRET_PI_AGENT_TOKEN_999";

	// 1. Secret in declared MIME
	let err = pi_agent::state::normalize_image_payload("aW1n", &format!("image/{secret_marker}")).unwrap_err();
	assert!(!err.contains(secret_marker), "error must not leak secret declared MIME: {err}");

	// 2. Secret in data URI MIME
	let err = pi_agent::state::normalize_image_payload(&format!("data:image/{secret_marker};base64,aW1n"), "image/png").unwrap_err();
	assert!(!err.contains(secret_marker), "error must not leak secret URI MIME: {err}");

	// 3. Secret in data URI encoding
	let err = pi_agent::state::normalize_image_payload(&format!("data:image/png;{secret_marker},aW1n"), "image/png").unwrap_err();
	assert!(!err.contains(secret_marker), "error must not leak secret encoding: {err}");

	// 4. Secret in unsupported Message content part
	let raw_msg_json = format!(
		"{{\"role\":\"user\",\"content\":\"hi\",\"parts\":[{{\"type\":\"{secret_marker}\"}}]}}"
	);
	let err = serde_json::from_str::<pi_agent::Message>(&raw_msg_json).unwrap_err().to_string();
	assert!(!err.contains(secret_marker), "deserialization error must not leak secret part kind: {err}");
	assert!(err.contains("at index 0"), "error must report index: {err}");
}

#[test]
fn image_mime_and_base64_validation_strict_logic() {
	use pi_agent::state::{is_valid_base64_payload, is_valid_image_mime};

	assert!(is_valid_image_mime("image/png"));
	assert!(is_valid_image_mime("image/jpeg"));
	assert!(is_valid_image_mime("IMAGE/JPEG"));
	assert!(is_valid_image_mime("image/webp"));
	assert!(is_valid_image_mime("image/gif"));
	assert!(!is_valid_image_mime("text/plain"));
	assert!(!is_valid_image_mime("application/json"));
	assert!(!is_valid_image_mime("image/"));
	assert!(!is_valid_image_mime(""));

	assert!(is_valid_base64_payload("aW1n"));
	assert!(is_valid_base64_payload("aGVsbG8="));
	assert!(is_valid_base64_payload("/9j/4AAQSkZJRg=="));
	assert!(!is_valid_base64_payload(""));
	assert!(!is_valid_base64_payload("=="));
	assert!(!is_valid_base64_payload("="));
	assert!(is_valid_base64_payload("abc="));
	assert!(!is_valid_base64_payload("abd="));
	assert!(!is_valid_base64_payload("AAh=="));
	assert!(!is_valid_base64_payload("ab="));
	assert!(!is_valid_base64_payload("a"));
	assert!(!is_valid_base64_payload("not-b64-???"));
}
