use std::{
	sync::Arc,
	time::Duration,
};

use pi_agent::{
	AgentConfig, AgentContext, AgentLoop, AgentState,
	ChildStatus, DelegationLimits, OpenAiCompatibleProvider,
	Role, Supervisor, TaskTool, Tool, ToolRegistry,
};
use pi_agent::tools::task::TaskToolDetails;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

fn tool_call_response(tool_name: &str, tool_args: &Value) -> Value {
	json!({
		"choices": [
			{
				"message": {
					"role": "assistant",
					"content": null,
					"tool_calls": [
						{
							"id": "call_1",
							"type": "function",
							"function": {
								"name": tool_name,
								"arguments": serde_json::to_string(tool_args).unwrap()
							}
						}
					]
				}
			}
		]
	})
}

fn text_response(text: &str) -> Value {
	json!({
		"choices": [
			{
				"message": {
					"role": "assistant",
					"content": text,
					"tool_calls": null
				}
			}
		]
	})
}

struct MockServer {
	pub base_url: String,
	_shutdown: oneshot::Sender<()>,
	pub hang_closed_rx: Option<mpsc::Receiver<Result<usize, String>>>,
}

async fn spawn_mock_server(
	hang_marker: Option<&'static str>,
) -> (MockServer, Option<mpsc::Receiver<()>>) {
	let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind listener");
	let port = listener.local_addr().unwrap().port();
	let base_url = format!("http://127.0.0.1:{port}");

	let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
	let (hang_signal_tx, hang_signal_rx) = if hang_marker.is_some() {
		let (tx, rx) = mpsc::channel(1);
		(Some(tx), Some(rx))
	} else {
		(None, None)
	};

	let (hang_closed_tx, hang_closed_rx) = if hang_marker.is_some() {
		let (tx, rx) = mpsc::channel(1);
		(Some(tx), Some(rx))
	} else {
		(None, None)
	};

	tokio::spawn(async move {
		loop {
			tokio::select! {
				_ = &mut shutdown_rx => break,
				accept_res = listener.accept() => {
					let (mut socket, _) = match accept_res {
						Ok(s) => s,
						Err(_) => break,
					};

					let hang_signal_tx = hang_signal_tx.clone();
					let hang_closed_tx = hang_closed_tx.clone();

					tokio::spawn(async move {
						let mut buf = Vec::new();
						let mut temp = [0u8; 1024];
						let mut header_len = None;
						let mut content_len = 0;

						while let Ok(n) = socket.read(&mut temp).await {
							if n == 0 {
								break;
							}
							buf.extend_from_slice(&temp[..n]);

							if header_len.is_none() {
								if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
									header_len = Some(pos + 4);
									let header_str = String::from_utf8_lossy(&buf[..pos]);
									for line in header_str.lines() {
										if line.to_ascii_lowercase().starts_with("content-length:") {
											if let Some(val) = line.split(':').nth(1) {
												content_len = val.trim().parse::<usize>().unwrap_or(0);
											}
										}
									}
								}
							}

							if let Some(h_len) = header_len {
								if buf.len() >= h_len + content_len {
									break;
								}
							}
						}

						let h_len = header_len.unwrap_or(0);
						let body = if buf.len() >= h_len + content_len {
							String::from_utf8_lossy(&buf[h_len..h_len + content_len]).to_string()
						} else {
							String::new()
						};

						// Check for hang marker in T3
						if let Some(marker) = hang_marker {
							if body.contains(marker) {
								if let Some(tx) = hang_signal_tx {
									let _ = tx.send(()).await;
								}
								// Do NOT respond: wait for connection close
								let mut wait_buf = [0u8; 100];
								let res = socket.read(&mut wait_buf).await;
								if let Some(ctx) = hang_closed_tx {
									let _ = ctx.send(res.map_err(|e| e.to_string())).await;
								}
								return;
							}
						}

						// Route on the latest message only: the request body carries the
						// whole history, so matching on it replays the first route forever.
						let resp_json = route_response(&last_message_text(&body));
						let resp_body = serde_json::to_string(&resp_json).unwrap();
						let response = format!(
							"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
							resp_body.len(),
							resp_body
						);

						let _ = socket.write_all(response.as_bytes()).await;
						let _ = socket.flush().await;
					});
				}
			}
		}
	});

	(
		MockServer {
			base_url,
			_shutdown: shutdown_tx,
			hang_closed_rx,
		},
		hang_signal_rx,
	)
}

/// Text of the last message in an OpenAI-style chat request.
///
/// The request body carries the whole history, so scripted routes must key off
/// the message the model is actually answering instead of any history keyword.
fn last_message_text(body: &str) -> String {
	let Ok(request) = serde_json::from_str::<Value>(body) else {
		return body.to_string();
	};
	let Some(messages) = request.get("messages").and_then(Value::as_array) else {
		return body.to_string();
	};
	let Some(last) = messages.last() else {
		return String::new();
	};
	match last.get("content") {
		Some(Value::String(text)) => text.clone(),
		Some(Value::Array(parts)) => parts
			.iter()
			.filter_map(|part| part.get("text").and_then(Value::as_str))
			.collect::<Vec<_>>()
			.join(""),
		_ => String::new(),
	}
}

/// Scripted model reply for the latest message: most advanced conversation
/// stage first, because `child_finished` is a substring of `grandchild_finished`
/// and tool results echo the child task text they ran.
fn route_response(latest: &str) -> Value {
	// T2 recovery stages
	if latest.contains("Tool not found: task") {
		return tool_call_response(
			"yield",
			&json!({ "result": { "data": "grandchild_finished" } }),
		);
	}
	if latest.contains("grandchild_finished") {
		return tool_call_response(
			"yield",
			&json!({ "result": { "data": "child_finished" } }),
		);
	}
	if latest.contains("child_finished") {
		return text_response("Root completed after T2 depth.");
	}

	// T1 / T4 completion stages
	if latest.contains("result_val") {
		return text_response("Root completed after T1 child.");
	}
	if latest.contains("output_A") || latest.contains("output_B") {
		return text_response("Root completed after T4 batch.");
	}

	// T1 routes
	if latest.contains("T1_START") {
		return tool_call_response(
			"task",
			&json!({ "name": "sub-1", "task": "T1_CHILD_WORK" }),
		);
	}
	if latest.contains("T1_CHILD_WORK") {
		return tool_call_response(
			"yield",
			&json!({ "result": { "data": { "result_key": "result_val" } } }),
		);
	}

	// T2 routes
	if latest.contains("T2_START") {
		return tool_call_response(
			"task",
			&json!({ "name": "t2-child", "task": "T2_CHILD_WORK" }),
		);
	}
	if latest.contains("T2_CHILD_WORK") {
		return tool_call_response(
			"task",
			&json!({ "name": "t2-grandchild", "task": "T2_GRANDCHILD_WORK" }),
		);
	}
	if latest.contains("T2_GRANDCHILD_WORK") {
		return tool_call_response(
			"task",
			&json!({ "name": "t2-great", "task": "T2_GREAT_WORK" }),
		);
	}

	// T3 routes
	if latest.contains("T3_START") {
		return tool_call_response(
			"task",
			&json!({ "name": "hang-child", "task": "T3_HANG_CHILD" }),
		);
	}

	// T4 routes
	if latest.contains("T4_START") {
		return tool_call_response(
			"task",
			&json!({
				"context": "T4 shared context",
				"tasks": [
					{ "name": "task-A", "task": "T4_WORK_A" },
					{ "name": "task-B", "task": "T4_WORK_B" }
				]
			}),
		);
	}
	if latest.contains("T4_WORK_A") {
		return tool_call_response(
			"yield",
			&json!({ "result": { "data": "output_A" } }),
		);
	}
	if latest.contains("T4_WORK_B") {
		return tool_call_response(
			"yield",
			&json!({ "result": { "data": "output_B" } }),
		);
	}

	text_response("fallback")
}

#[tokio::test]
async fn test_t1_happy_delegation() {
	let (server, _) = spawn_mock_server(None).await;
	let provider = Arc::new(
		OpenAiCompatibleProvider::new(&server.base_url, "fake-key", "test-model").unwrap(),
	);

	let supervisor = Supervisor::new(
		ToolRegistry::new(),
		provider.clone(),
		None,
		DelegationLimits::default(),
	);

	let root_registry = supervisor.root_registry();
	let config = AgentConfig {
		max_turns: 5,
		context_limit: 100_000,
		system_prompt: None,
		cwd: std::env::temp_dir(),
	};

	let agent_loop = AgentLoop::new(config, provider, root_registry);
	let mut state = AgentState::default();

	let turn_results = agent_loop
		.run_dialog(
			"T1_START",
			&mut state,
			None,
		)
		.await
		.expect("root dialog run");

	// 1. Root completed
	assert_eq!(turn_results.len(), 2);
	assert!(turn_results.last().unwrap().is_completed);

	// 2. First turn executed task tool
	let first_turn = &turn_results[0];
	assert_eq!(first_turn.tool_results.len(), 1);
	let tool_res = &first_turn.tool_results[0];
	assert_eq!(tool_res.tool_call_id, first_turn.message.tool_calls[0].id);

	let details: TaskToolDetails =
		serde_json::from_str(&tool_res.content).expect("valid TaskToolDetails JSON");
	assert_eq!(details.results.len(), 1);

	let sub_result = &details.results[0];
	assert_eq!(sub_result.index, 0);
	assert_eq!(sub_result.id, "sub-1");
	assert_eq!(sub_result.agent, "task");
	assert_eq!(sub_result.exit_code, 0);

	// 3. extractedToolData has terminal yield payload
	let extracted = sub_result
		.extracted_tool_data
		.as_ref()
		.expect("extracted_tool_data");
	let yields = extracted.get("yield").expect("yield data in extractedToolData");
	assert_eq!(yields.len(), 1);
	assert_eq!(yields[0]["status"], "success");
	assert_eq!(yields[0]["data"]["result_key"], "result_val");

	// 4. Invariant: tool_call -> exactly one tool response message
	let tool_call_count = state
		.messages
		.iter()
		.filter(|m| m.role == Role::Assistant)
		.map(|m| m.tool_calls.len())
		.sum::<usize>();
	let tool_msg_count = state
		.messages
		.iter()
		.filter(|m| m.role == Role::Tool)
		.count();
	assert_eq!(tool_call_count, 1);
	assert_eq!(tool_msg_count, 1);

	// 5. Supervisor child record marked Completed
	let children = supervisor.children();
	assert_eq!(children.len(), 1);
	assert_eq!(children[0].id, "sub-1");
	assert_eq!(children[0].status, ChildStatus::Completed);
	assert!(children[0].duration_ms.is_some());
}

#[tokio::test]
async fn test_t2_depth_limits() {
	let (server, _) = spawn_mock_server(None).await;
	let provider = Arc::new(
		OpenAiCompatibleProvider::new(&server.base_url, "fake-key", "test-model").unwrap(),
	);

	let supervisor = Supervisor::new(
		ToolRegistry::new(),
		provider.clone(),
		None,
		DelegationLimits {
			max_depth: 2,
			max_concurrency: 32,
		},
	);

	// Check registry contents at depths:
	// depth 0 (root): has task
	assert!(supervisor.root_registry().get("task").is_some());
	// depth 1 (child): has task and yield
	let reg1 = supervisor.child_registry(1);
	assert!(reg1.get("task").is_some());
	assert!(reg1.get("yield").is_some());
	// depth 2 (grandchild, depth == max_depth): has yield, but NO task
	let reg2 = supervisor.child_registry(2);
	assert!(reg2.get("task").is_none());
	assert!(reg2.get("yield").is_some());

	// Separate unit check: invoking TaskTool directly at depth == max_depth returns exact TS message
	let unit_task = TaskTool::new(supervisor.clone(), 2);
	let ctx = AgentContext::new(std::env::temp_dir());
	let err = unit_task
		.execute(&json!({ "task": "test preflight" }), &ctx)
		.await
		.unwrap_err();
	let err_str = err.to_string();
	assert!(
		err_str.contains("Cannot spawn another agent at task depth 2; maximum depth is 2."),
		"Unexpected error message: {err_str}"
	);

	// End-to-end dialog test: root -> child (depth 1) -> grandchild (depth 2)
	// Grandchild tries to call task -> returns missing tool diagnostic -> recovers via yield -> finishes
	let root_registry = supervisor.root_registry();
	let config = AgentConfig {
		max_turns: 10,
		context_limit: 100_000,
		system_prompt: None,
		cwd: std::env::temp_dir(),
	};

	let agent_loop = AgentLoop::new(config, provider, root_registry);
	let mut state = AgentState::default();

	let turn_results = agent_loop
		.run_dialog("T2_START", &mut state, None)
		.await
		.expect("root dialog run");

	assert!(turn_results.last().unwrap().is_completed);

	let children = supervisor.children();
	assert!(children.iter().any(|c| c.id == "t2-child" && c.status == ChildStatus::Completed));
	assert!(children.iter().any(|c| c.id == "t2-grandchild" && c.status == ChildStatus::Completed));
}

#[tokio::test]
async fn test_t3_cancellation_cleans_up_and_records_aborted() {
	let (mut server, hang_rx) = spawn_mock_server(Some("T3_HANG_CHILD")).await;
	let mut hang_rx = hang_rx.expect("hang signal rx");

	let provider = Arc::new(
		OpenAiCompatibleProvider::new(&server.base_url, "fake-key", "test-model").unwrap(),
	);

	let supervisor = Supervisor::new(
		ToolRegistry::new(),
		provider.clone(),
		None,
		DelegationLimits::default(),
	);

	let root_registry = supervisor.root_registry();
	let config = AgentConfig {
		max_turns: 5,
		context_limit: 100_000,
		system_prompt: None,
		cwd: std::env::temp_dir(),
	};

	let agent_loop = AgentLoop::new(config, provider, root_registry);
	let mut state = AgentState::default();

	// Spawn root dialog as a background tokio task
	let root_task = tokio::spawn(async move {
		agent_loop.run_dialog("T3_START", &mut state, None).await
	});

	// Wait until child connects and reaches mock with T3_HANG_CHILD
	tokio::time::timeout(Duration::from_secs(5), hang_rx.recv())
		.await
		.expect("child reached hang mock")
		.expect("channel valid");

	// Now abort the root task
	root_task.abort();
	let _ = root_task.await;

	// Mock must observe socket disconnect / EOF because ChildGuard drops and aborts the child task
	let hang_closed_rx = server.hang_closed_rx.as_mut().expect("hang_closed_rx");
	let socket_res = tokio::time::timeout(Duration::from_secs(5), hang_closed_rx.recv())
		.await
		.expect("mock detected disconnect within 5s")
		.expect("disconnect payload");

	// Read result on closed connection is Ok(0) (EOF) or an IO error
	match socket_res {
		Ok(0) => {}, // clean EOF
		Ok(_) => {}, // read EOF
		Err(_) => {}, // connection reset
	}

	// Verify child record in supervisor is Aborted with abort_reason
	let children = supervisor.children();
	let child = children
		.iter()
		.find(|c| c.id == "hang-child")
		.expect("hang-child record in supervisor");
	assert_eq!(child.status, ChildStatus::Aborted);
	assert_eq!(child.aborted, Some(true));
	assert_eq!(child.abort_reason.as_deref(), Some("aborted by parent"));
}

#[tokio::test]
async fn test_t4_batch_execution_order() {
	let (server, _) = spawn_mock_server(None).await;
	let provider = Arc::new(
		OpenAiCompatibleProvider::new(&server.base_url, "fake-key", "test-model").unwrap(),
	);

	let supervisor = Supervisor::new(
		ToolRegistry::new(),
		provider.clone(),
		None,
		DelegationLimits::default(),
	);

	let root_registry = supervisor.root_registry();
	let config = AgentConfig {
		max_turns: 5,
		context_limit: 100_000,
		system_prompt: None,
		cwd: std::env::temp_dir(),
	};

	let agent_loop = AgentLoop::new(config, provider, root_registry);
	let mut state = AgentState::default();

	let turn_results = agent_loop
		.run_dialog("T4_START", &mut state, None)
		.await
		.expect("root dialog run");

	assert!(turn_results.last().unwrap().is_completed);

	let first_turn = &turn_results[0];
	let tool_res = &first_turn.tool_results[0];
	let details: TaskToolDetails = serde_json::from_str(&tool_res.content).unwrap();

	// Results must preserve exact input order (index 0, 1)
	assert_eq!(details.results.len(), 2);

	let r0 = &details.results[0];
	assert_eq!(r0.index, 0);
	assert_eq!(r0.id, "task-A");
	assert_eq!(r0.exit_code, 0);
	assert_eq!(
		r0.extracted_tool_data.as_ref().unwrap()["yield"][0]["data"],
		"output_A"
	);

	let r1 = &details.results[1];
	assert_eq!(r1.index, 1);
	assert_eq!(r1.id, "task-B");
	assert_eq!(r1.exit_code, 0);
	assert_eq!(
		r1.extracted_tool_data.as_ref().unwrap()["yield"][0]["data"],
		"output_B"
	);

	let children = supervisor.children();
	assert_eq!(children.len(), 2);
	assert!(children.iter().all(|c| c.status == ChildStatus::Completed));
}
