use std::{
	collections::VecDeque,
	sync::{
		Mutex,
		atomic::{AtomicUsize, Ordering},
	},
};

use async_trait::async_trait;
use futures::StreamExt;
use pi_agent::{
	AgentConfig, AgentError, AgentError::Interrupted, AgentLoop, AgentState, ChunkStream, EditTool,
	Message, ModelProvider, OpenAiCompatibleProvider, ProviderError, ReadTool, Role, SteeringEvent,
	StreamAssembler, Tool, ToolCall, ToolRegistry, WriteTool, parse_sse_line, sse_stream,
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
	let message = assembler.finish();
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
