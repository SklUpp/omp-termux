//! Behavioral tests for runtime approval gating:
//! - Tools are not hidden from registry under always-ask/write
//! - Closed stdin denies execution before side-effects (file unchanged)
//! - Explicit --allow permits execution
//! - --approval-mode write auto-permits read only, denies write/bash
//! - MCP tools default to exec tier and are denied before call unless allowed

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use omp_cli::{
	agent::{AgentRunner, Event, RunRequest},
	approve::{ApprovalMode, Approvals},
	catalog::Catalog,
	config::{Config, ProviderConfig},
	output::OutputMode,
	runtime::PiAgentRunner,
};
use serde_json::json;

struct TempDir {
	path: PathBuf,
}

impl TempDir {
	fn new(prefix: &str) -> Self {
		static COUNTER: AtomicUsize = AtomicUsize::new(0);
		let path = std::env::temp_dir().join(format!(
			"omp-test-approval-{}-{}-{}",
			prefix,
			std::process::id(),
			COUNTER.fetch_add(1, Ordering::SeqCst)
		));
		fs::create_dir_all(&path).expect("create temp dir");
		Self { path }
	}
}

impl Drop for TempDir {
	fn drop(&mut self) {
		let _ = fs::remove_dir_all(&self.path);
	}
}

/// Spawns a single-turn mock model HTTP server returning tool_calls.
fn spawn_mock_model_server(
	tool_name: &'static str,
	tool_args: serde_json::Value,
) -> (SocketAddr, std::thread::JoinHandle<()>) {
	let (addr_tx, addr_rx) = std::sync::mpsc::channel::<SocketAddr>();
	let handle = std::thread::spawn(move || {
		let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
		let _ = listener.set_nonblocking(true);
		addr_tx.send(listener.local_addr().unwrap()).unwrap();
		let deadline = std::time::Instant::now() + Duration::from_secs(5);
		let mut socket = loop {
			if std::time::Instant::now() >= deadline {
				return;
			}
			match listener.accept() {
				Ok((sock, _)) => break sock,
				Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
					std::thread::sleep(Duration::from_millis(10));
				}
				Err(_) => return,
			}
		};
		let _ = socket.set_read_timeout(Some(Duration::from_secs(1)));
		let _ = socket.set_write_timeout(Some(Duration::from_secs(2)));
		let mut buf = [0u8; 4096];
		while std::time::Instant::now() < deadline {
			match socket.read(&mut buf) {
				Ok(0) => break,
				Ok(_) => break,
				Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
					continue;
				}
				Err(_) => break,
			}
		}

		let escaped_args = serde_json::to_string(&tool_args.to_string()).unwrap();
		let response_body = format!(
			"data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{{\"name\":\"{tool_name}\",\"arguments\":{escaped_args}}}}}]}},\"finish_reason\":null}}]}}\n\ndata: [DONE]\n\n"
		);

		let _ = socket.write_all(
			format!(
				"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{}",
				response_body
			)
			.as_bytes(),
		);
		let _ = socket.flush();
	});

	let addr = addr_rx.recv_timeout(Duration::from_secs(5)).unwrap();
	(addr, handle)
}

fn make_run_request(
	addr: SocketAddr,
	work_dir: &Path,
	approvals: Approvals,
) -> RunRequest {
	let mut config = Config::default();
	config.providers.insert(
		"local".to_string(),
		ProviderConfig {
			base_url: Some(format!("http://{addr}/v1")),
			api_key: Some("sk-test".to_string()),
			..ProviderConfig::default()
		},
	);

	RunRequest {
		prompt: "call tool".to_string(),
		parts: Vec::new(),
		cwd: work_dir.to_path_buf(),
		output_mode: OutputMode::Json,
		model: Some("local/mock-model".to_string()),
		provider: Some("local".to_string()),
		api_key: Some("sk-test".to_string()),
		system_prompt: None,
		append_system_prompt: None,
		context_files: Vec::new(),
		skills: Vec::new(),
		history: Vec::new(),
		config,
		catalog: Arc::new(Catalog::default()),
		approvals,
		max_time: Some(Duration::from_secs(10)),
		max_turns: 1,
		tools_enabled: true,
		cancellation_token: None,
	}
}

#[test]
fn test_closed_stdin_denies_write_and_file_unchanged() {
	let temp = TempDir::new("write-denied");
	let target_file = temp.path.join("denied.txt");
	assert!(!target_file.exists());

	let (addr, handle) = spawn_mock_model_server(
		"write",
		json!({
			"path": target_file.to_str().unwrap(),
			"content": "must not be written"
		}),
	);

	// Default always-ask with empty allow list; stdin is not interactive
	let req = make_run_request(
		addr,
		&temp.path,
		Approvals::new(ApprovalMode::AlwaysAsk, Vec::new()),
	);

	let runner = PiAgentRunner::new().expect("runner");
	let stream = runner.run(&req).expect("run");

	let mut tool_results = Vec::new();
	for ev in stream {
		if let Ok(Event::ToolEnd { id, tool, result, error }) = ev {
			tool_results.push((id, tool, result, error));
		}
	}
	handle.join().unwrap();

	// Verify tool call was denied
	assert_eq!(tool_results.len(), 1, "expected 1 tool result");
	let (_, tool, result, error) = &tool_results[0];
	assert_eq!(tool, "write");
	assert!(error.is_some(), "error must be present for denied tool");
	assert!(
		result.to_lowercase().contains("denied")
			|| error.as_deref().unwrap_or("").to_lowercase().contains("denied")
			|| result.to_lowercase().contains("closed"),
		"result: {result}, error: {error:?}"
	);

	// File MUST NOT exist
	assert!(
		!target_file.exists(),
		"file must not be created when write is denied"
	);
}

#[test]
fn test_allow_permits_write() {
	let temp = TempDir::new("write-allowed");
	let target_file = temp.path.join("allowed.txt");
	assert!(!target_file.exists());

	let (addr, handle) = spawn_mock_model_server(
		"write",
		json!({
			"path": target_file.to_str().unwrap(),
			"content": "hello allowed world"
		}),
	);

	// always-ask with --allow write
	let req = make_run_request(
		addr,
		&temp.path,
		Approvals::new(ApprovalMode::AlwaysAsk, vec!["write".to_string()]),
	);

	let runner = PiAgentRunner::new().expect("runner");
	let stream = runner.run(&req).expect("run");

	let mut tool_results = Vec::new();
	for ev in stream {
		if let Ok(Event::ToolEnd { id, tool, result, error }) = ev {
			tool_results.push((id, tool, result, error));
		}
	}
	handle.join().unwrap();

	assert_eq!(tool_results.len(), 1);
	let (_, tool, _, error) = &tool_results[0];
	assert_eq!(tool, "write");
	assert!(error.is_none(), "expected success with --allow write");

	// File MUST exist with exact content
	assert!(target_file.exists());
	let content = fs::read_to_string(&target_file).expect("read file");
	assert_eq!(content, "hello allowed world");
}

#[test]
fn test_approval_mode_write_read_only_permits() {
	let temp = TempDir::new("mode-write");
	let read_target = temp.path.join("readonly.txt");
	fs::write(&read_target, "secret data").unwrap();

	// 1. read tool under write mode is auto-approved
	let (addr1, handle1) = spawn_mock_model_server(
		"read",
		json!({
			"path": read_target.to_str().unwrap()
		}),
	);

	let req1 = make_run_request(
		addr1,
		&temp.path,
		Approvals::new(ApprovalMode::Write, Vec::new()),
	);

	let runner = PiAgentRunner::new().expect("runner");
	let stream1 = runner.run(&req1).expect("run read");
	let mut tool_results1 = Vec::new();
	for ev in stream1 {
		if let Ok(Event::ToolEnd { id, tool, result, error }) = ev {
			tool_results1.push((id, tool, result, error));
		}
	}
	handle1.join().unwrap();

	assert_eq!(tool_results1.len(), 1);
	assert_eq!(tool_results1[0].1, "read");
	assert!(tool_results1[0].3.is_none(), "read must succeed under write mode");
	assert!(tool_results1[0].2.contains("secret data"));

	// 2. bash tool under write mode is NOT auto-approved (denied under closed stdin)
	let side_effect_file = temp.path.join("bash_side_effect.txt");
	let (addr2, handle2) = spawn_mock_model_server(
		"bash",
		json!({
			"command": format!("touch {}", side_effect_file.display())
		}),
	);

	let req2 = make_run_request(
		addr2,
		&temp.path,
		Approvals::new(ApprovalMode::Write, Vec::new()),
	);

	let stream2 = runner.run(&req2).expect("run bash");
	let mut tool_results2 = Vec::new();
	for ev in stream2 {
		if let Ok(Event::ToolEnd { id, tool, result, error }) = ev {
			tool_results2.push((id, tool, result, error));
		}
	}
	handle2.join().unwrap();

	assert_eq!(tool_results2.len(), 1);
	assert_eq!(tool_results2[0].1, "bash");
	assert!(tool_results2[0].3.is_some(), "bash must be denied under write mode");
	assert!(
		!tool_results2[0].2.contains("Tool not found"),
		"bash must exist in registry, not be hidden: result was '{}'",
		tool_results2[0].2
	);
	assert!(
		tool_results2[0].2.to_lowercase().contains("denied")
			|| tool_results2[0].2.to_lowercase().contains("approval")
			|| tool_results2[0].2.to_lowercase().contains("closed"),
		"rejection must come from approval gate: result was '{}'",
		tool_results2[0].2
	);
	assert!(
		!side_effect_file.exists(),
		"bash side effect must not run when denied"
	);
}

#[test]
fn test_mcp_tool_denied_before_call_without_allow() {
	let temp = TempDir::new("mcp-denied");
	let agent_dir = temp.path.join("agent");
	let project_dir = temp.path.join("proj");
	let omp_dir = project_dir.join(".omp");
	fs::create_dir_all(&agent_dir).unwrap();
	fs::create_dir_all(&omp_dir).unwrap();

	// Create mock python stdio MCP server that writes a marker file if called
	let marker_file = temp.path.join("mcp_called.marker");
	let mock_py = temp.path.join("mcp_srv.py");
	fs::write(
		&mock_py,
		format!(
			r#"import sys, json
while True:
    line = sys.stdin.readline()
    if not line: break
    req = json.loads(line)
    mid = req.get("id")
    method = req.get("method")
    if method == "initialize":
        res = {{"jsonrpc":"2.0","id":mid,"result":{{"protocolVersion":"2024-11-05","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"mock","version":"1.0"}}}}}}
    elif method == "tools/list":
        res = {{"jsonrpc":"2.0","id":mid,"result":{{"tools":[{{"name":"danger_tool","description":"dangerous tool","inputSchema":{{"type":"object"}}}}]}}}}
    elif method == "tools/call":
        open("{}", "w").write("called")
        res = {{"jsonrpc":"2.0","id":mid,"result":{{"content":[{{"type":"text","text":"ok"}}]}}}}
    else:
        res = {{"jsonrpc":"2.0","id":mid,"result":{{}}}}
    sys.stdout.write(json.dumps(res) + "\n")
    sys.stdout.flush()
"#,
			marker_file.display()
		),
	)
	.unwrap();

	fs::write(
		omp_dir.join("mcp.json"),
		json!({
			"mcpServers": {
				"mock": {
					"command": "python3",
					"args": [mock_py.to_str().unwrap()]
				}
			}
		})
		.to_string(),
	)
	.unwrap();

	// 1. Model requests mcp__mock_danger_tool under write mode without allow
	let (addr, handle) = spawn_mock_model_server("mcp__mock_danger_tool", json!({}));
	let req = make_run_request(
		addr,
		&project_dir,
		Approvals::new(ApprovalMode::Write, Vec::new()),
	);

	let runner = PiAgentRunner::new().expect("runner");
	let stream = runner.run(&req).expect("run mcp");
	let mut tool_results = Vec::new();
	for ev in stream {
		if let Ok(Event::ToolEnd { id, tool, result, error }) = ev {
			tool_results.push((id, tool, result, error));
		}
	}
	handle.join().unwrap();

	assert_eq!(tool_results.len(), 1);
	assert_eq!(tool_results[0].1, "mcp__mock_danger_tool");
	assert!(tool_results[0].3.is_some(), "MCP tool must be denied");
	assert!(
		!tool_results[0].2.contains("Tool not found"),
		"MCP tool rejection must be explicit, not hidden as not-found: result was '{}'",
		tool_results[0].2
	);
	assert!(
		tool_results[0].2.contains("MCP server \"mock\" was not launched: stdio launch was not approved")
			&& tool_results[0].2.contains("tool existence not confirmed"),
		"rejection must report exact server not launched message and tool existence not confirmed: result was '{}'",
		tool_results[0].2
	);
	assert!(
		!marker_file.exists(),
		"MCP server must NOT be called when approval denied"
	);

	// 1b. Truly unknown tool under write mode reports Tool not found, NOT launch denial
	let (addr_unknown, handle_unknown) =
		spawn_mock_model_server("mcp__unknown_danger_tool", json!({}));
	let req_unknown = make_run_request(
		addr_unknown,
		&project_dir,
		Approvals::new(ApprovalMode::Write, Vec::new()),
	);
	let stream_unknown = runner.run(&req_unknown).expect("run unknown mcp");
	let mut tool_results_unknown = Vec::new();
	for ev in stream_unknown {
		if let Ok(Event::ToolEnd { id, tool, result, error }) = ev {
			tool_results_unknown.push((id, tool, result, error));
		}
	}
	handle_unknown.join().unwrap();

	assert_eq!(tool_results_unknown.len(), 1);
	assert_eq!(tool_results_unknown[0].1, "mcp__unknown_danger_tool");
	assert!(tool_results_unknown[0].3.is_some(), "unknown tool must error");
	assert_eq!(
		tool_results_unknown[0].2,
		"Tool not found: mcp__unknown_danger_tool",
		"truly unknown MCP tool must return 'Tool not found', not launch denial: result was '{}'",
		tool_results_unknown[0].2
	);
	assert!(
		!tool_results_unknown[0].2.contains("not launched"),
		"truly unknown tool must not report launch denial: result was '{}'",
		tool_results_unknown[0].2
	);

	// 1c. Unknown tool with prefix of denied server still reports server not launched and existence not confirmed
	let (addr_prefix_unknown, handle_prefix_unknown) =
		spawn_mock_model_server("mcp__mock_nonexistent_subtool", json!({}));
	let req_prefix_unknown = make_run_request(
		addr_prefix_unknown,
		&project_dir,
		Approvals::new(ApprovalMode::Write, Vec::new()),
	);
	let stream_prefix_unknown = runner.run(&req_prefix_unknown).expect("run prefix unknown mcp");
	let mut tool_results_prefix_unknown = Vec::new();
	for ev in stream_prefix_unknown {
		if let Ok(Event::ToolEnd { id, tool, result, error }) = ev {
			tool_results_prefix_unknown.push((id, tool, result, error));
		}
	}
	handle_prefix_unknown.join().unwrap();

	assert_eq!(tool_results_prefix_unknown.len(), 1);
	assert_eq!(tool_results_prefix_unknown[0].1, "mcp__mock_nonexistent_subtool");
	assert!(tool_results_prefix_unknown[0].3.is_some());
	assert!(
		tool_results_prefix_unknown[0].2.contains("mock")
			&& tool_results_prefix_unknown[0].2.contains("was not launched")
			&& tool_results_prefix_unknown[0].2.contains("tool existence not confirmed"),
		"must report matching server not launched without asserting whether tool exists: result was '{}'",
		tool_results_prefix_unknown[0].2
	);

	// 2. With explicit --allow mcp__mock_danger_tool, execution is permitted
	let (addr2, handle2) = spawn_mock_model_server("mcp__mock_danger_tool", json!({}));
	let req2 = make_run_request(
		addr2,
		&project_dir,
		Approvals::new(
			ApprovalMode::Write,
			vec!["mcp__mock_danger_tool".to_string()],
		),
	);

	let stream2 = runner.run(&req2).expect("run mcp allowed");
	let mut tool_results2 = Vec::new();
	for ev in stream2 {
		if let Ok(Event::ToolEnd { id, tool, result, error }) = ev {
			tool_results2.push((id, tool, result, error));
		}
	}
	handle2.join().unwrap();

	assert_eq!(tool_results2.len(), 1);
	assert_eq!(tool_results2[0].1, "mcp__mock_danger_tool");
	assert!(
		tool_results2[0].3.is_none(),
		"MCP tool must be permitted with --allow"
	);
	assert!(
		marker_file.exists(),
		"MCP server must be called when approved"
	);
	let marker_content = fs::read_to_string(&marker_file).unwrap();
	assert_eq!(marker_content, "called");

	// 2b. When server is running, nonexistent tool on it reports Tool not found
	let (addr2_unknown, handle2_unknown) =
		spawn_mock_model_server("mcp__mock_nonexistent_subtool", json!({}));
	let req2_unknown = make_run_request(
		addr2_unknown,
		&project_dir,
		Approvals::new(
			ApprovalMode::Write,
			vec!["mcp__mock_danger_tool".to_string()],
		),
	);
	let stream2_unknown = runner.run(&req2_unknown).expect("run running mcp nonexistent tool");
	let mut tool_results2_unknown = Vec::new();
	for ev in stream2_unknown {
		if let Ok(Event::ToolEnd { id, tool, result, error }) = ev {
			tool_results2_unknown.push((id, tool, result, error));
		}
	}
	handle2_unknown.join().unwrap();

	assert_eq!(tool_results2_unknown.len(), 1);
	assert_eq!(tool_results2_unknown[0].1, "mcp__mock_nonexistent_subtool");
	assert!(tool_results2_unknown[0].3.is_some());
	assert_eq!(
		tool_results2_unknown[0].2,
		"Tool not found: mcp__mock_nonexistent_subtool",
		"when server is running, unlisted tool must report Tool not found: result was '{}'",
		tool_results2_unknown[0].2
	);

	// 2c. With tool-level colon allow --allow mcp:mock:danger_tool, execution is permitted
	let (addr3, handle3) = spawn_mock_model_server("mcp__mock_danger_tool", json!({}));
	let req3 = make_run_request(
		addr3,
		&project_dir,
		Approvals::new(
			ApprovalMode::AlwaysAsk,
			vec!["mcp:mock:danger_tool".to_string()],
		),
	);
	let stream3 = runner.run(&req3).expect("run mcp allowed via colon syntax");
	let mut tool_results3 = Vec::new();
	for ev in stream3 {
		if let Ok(Event::ToolEnd { id, tool, result, error }) = ev {
			tool_results3.push((id, tool, result, error));
		}
	}
	handle3.join().unwrap();

	assert_eq!(tool_results3.len(), 1);
	assert_eq!(tool_results3[0].1, "mcp__mock_danger_tool");
	assert!(
		tool_results3[0].3.is_none(),
		"MCP tool must be permitted with tool-level colon allow without prompt"
	);

	// 2d. Server-level allow --allow mcp:mock launches server, but execution requires approval
	// which fails closed with closed stdin
	let (addr4, handle4) = spawn_mock_model_server("mcp__mock_danger_tool", json!({}));
	let req4 = make_run_request(
		addr4,
		&project_dir,
		Approvals::new(
			ApprovalMode::AlwaysAsk,
			vec!["mcp:mock".to_string()],
		),
	);
	let stream4 = runner.run(&req4).expect("run server level allow");
	let mut tool_results4 = Vec::new();
	for ev in stream4 {
		if let Ok(Event::ToolEnd { id, tool, result, error }) = ev {
			tool_results4.push((id, tool, result, error));
		}
	}
	handle4.join().unwrap();

	assert_eq!(tool_results4.len(), 1);
	assert_eq!(tool_results4[0].1, "mcp__mock_danger_tool");
	assert!(
		tool_results4[0].3.is_some(),
		"server-level allow must NOT auto-approve tool execution under AlwaysAsk"
	);
}

#[test]
fn test_canceled_execution_aborts_cleanly() {
	let temp = TempDir::new("cancel");
	let (addr, handle) = spawn_mock_model_server(
		"bash",
		json!({
			"command": "sleep 5"
		}),
	);

	let mut req = make_run_request(
		addr,
		&temp.path,
		Approvals::new(ApprovalMode::Yolo, Vec::new()),
	);
	req.max_time = Some(Duration::from_millis(150));

	let runner = PiAgentRunner::new().expect("runner");
	let stream = runner.run(&req).expect("run");

	let mut turn_ends = Vec::new();
	let mut tool_results = Vec::new();
	for ev in stream {
		match ev {
			Ok(Event::TurnEnd { stop_reason, .. }) => turn_ends.push(stop_reason),
			Ok(Event::ToolEnd { result, error, .. }) => tool_results.push((result, error)),
			_ => {},
		}
	}
	handle.join().unwrap();

	assert_eq!(turn_ends, vec!["aborted".to_string()]);
	assert_eq!(tool_results.len(), 1);
	assert!(
		tool_results[0].1.is_some(),
		"canceled execution must report error on tool"
	);
}

#[test]
fn test_mcp_tool_ambiguous_prefix_names_no_server() {
	let temp = TempDir::new("mcp-ambiguous");
	let project_dir = temp.path.join("proj");
	let omp_dir = project_dir.join(".omp");
	fs::create_dir_all(&omp_dir).unwrap();

	fs::write(
		omp_dir.join("mcp.json"),
		json!({
			"mcpServers": {
				"foo": {
					"command": "python3",
					"args": ["-c", "pass"]
				},
				"foo_bar": {
					"command": "python3",
					"args": ["-c", "pass"]
				}
			}
		})
		.to_string(),
	)
	.unwrap();

	let (addr, handle) = spawn_mock_model_server("mcp__foo_bar_echo", json!({}));
	let req = make_run_request(
		addr,
		&project_dir,
		Approvals::new(ApprovalMode::Write, Vec::new()),
	);

	let runner = PiAgentRunner::new().expect("runner");
	let stream = runner.run(&req).expect("run ambiguous mcp");
	let mut tool_results = Vec::new();
	for ev in stream {
		if let Ok(Event::ToolEnd { id, tool, result, error }) = ev {
			tool_results.push((id, tool, result, error));
		}
	}
	handle.join().unwrap();

	assert_eq!(tool_results.len(), 1);
	assert_eq!(tool_results[0].1, "mcp__foo_bar_echo");
	assert!(tool_results[0].3.is_some(), "ambiguous tool call must error");
	assert_eq!(
		tool_results[0].2,
		"Tool not found: mcp__foo_bar_echo",
		"ambiguous tool name must remain unknown: result was '{}'",
		tool_results[0].2
	);
	assert!(
		!tool_results[0].2.contains("matching server")
			&& !tool_results[0].2.contains("was not launched"),
		"must not attribute to any server when ambiguous"
	);
}

#[test]
fn test_mcp_tool_error_redacts_http_body_secrets() {
	let temp = TempDir::new("mcp-secret");
	let project_dir = temp.path.join("proj");
	let omp_dir = project_dir.join(".omp");
	fs::create_dir_all(&omp_dir).unwrap();

	let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
	let srv_addr = listener.local_addr().unwrap();
	let srv_handle = std::thread::spawn(move || {
		let _ = listener.set_nonblocking(true);
		let deadline = std::time::Instant::now() + Duration::from_secs(5);
		while std::time::Instant::now() < deadline {
			let (mut socket, _) = match listener.accept() {
				Ok(conn) => conn,
				Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
					std::thread::sleep(Duration::from_millis(10));
					continue;
				}
				Err(_) => break,
			};
			let _ = socket.set_read_timeout(Some(Duration::from_secs(1)));
			let _ = socket.set_write_timeout(Some(Duration::from_secs(2)));
			let mut buf = [0u8; 1024];
			while std::time::Instant::now() < deadline {
				match socket.read(&mut buf) {
					Ok(0) => break,
					Ok(_) => break,
					Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
						continue;
					}
					Err(_) => break,
				}
			}
			let body = r#"{"error":"db_failed","secret_token":"SUPER_SECRET_TOKEN_DO_NOT_LEAK"}"#;
			let response = format!(
				"HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
				body.len(),
				body
			);
			let _ = socket.write_all(response.as_bytes());
			let _ = socket.flush();
			break;
		}
	});

	fs::write(
		omp_dir.join("mcp.json"),
		json!({
			"mcpServers": {
				"secure_srv": {
					"url": format!("http://{srv_addr}/sse")
				}
			}
		})
		.to_string(),
	)
	.unwrap();

	let (addr, handle) = spawn_mock_model_server("mcp__secure_srv_any_tool", json!({}));
	let req = make_run_request(
		addr,
		&project_dir,
		Approvals::new(ApprovalMode::Write, Vec::new()),
	);

	let runner = PiAgentRunner::new().expect("runner");
	let stream = runner.run(&req).expect("run");
	let mut tool_results = Vec::new();
	for ev in stream {
		if let Ok(Event::ToolEnd { id, tool, result, error }) = ev {
			tool_results.push((id, tool, result, error));
		}
	}
	handle.join().unwrap();
	srv_handle.join().unwrap();

	assert_eq!(tool_results.len(), 1);
	assert_eq!(tool_results[0].1, "mcp__secure_srv_any_tool");
	assert!(tool_results[0].3.is_some(), "tool call must error");
	assert!(
		!tool_results[0].2.contains("SUPER_SECRET_TOKEN_DO_NOT_LEAK"),
		"diagnostic must NOT leak raw HTTP response body or secrets: result was '{}'",
		tool_results[0].2
	);
	assert!(
		tool_results[0].2.contains("tool existence not confirmed"),
		"diagnostic must state tool existence not confirmed: result was '{}'",
		tool_results[0].2
	);
	assert!(
		tool_results[0].2.contains("secure_srv"),
		"diagnostic must identify matching server: result was '{}'",
		tool_results[0].2
	);
}

#[test]
fn test_mcp_tool_call_error_redacts_http_body_secrets() {
	let temp = TempDir::new("mcp-call-secret");
	let project_dir = temp.path.join("proj");
	let omp_dir = project_dir.join(".omp");
	fs::create_dir_all(&omp_dir).unwrap();

	let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
	let srv_addr = listener.local_addr().unwrap();
	let srv_handle = std::thread::spawn(move || {
		let _ = listener.set_nonblocking(true);
		let deadline = std::time::Instant::now() + Duration::from_secs(5);
		let mut done = false;
		while !done && std::time::Instant::now() < deadline {
			let (mut socket, _) = match listener.accept() {
				Ok(conn) => conn,
				Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
					std::thread::sleep(Duration::from_millis(10));
					continue;
				}
				Err(_) => break,
			};
			let _ = socket.set_read_timeout(Some(Duration::from_secs(1)));
			let _ = socket.set_write_timeout(Some(Duration::from_secs(2)));
			let mut buf = Vec::new();
			let mut tmp = [0u8; 1024];
			while !done && std::time::Instant::now() < deadline {
				let n = match socket.read(&mut tmp) {
					Ok(0) => break,
					Ok(n) => n,
					Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
						continue;
					}
					Err(_) => break,
				};
				buf.extend_from_slice(&tmp[..n]);

				while let Some(header_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
					let header_str = String::from_utf8_lossy(&buf[..header_end]);
					let content_len = header_str
						.lines()
						.find_map(|line| {
							let l = line.to_ascii_lowercase();
							if l.starts_with("content-length:") {
								l.split(':').nth(1)?.trim().parse::<usize>().ok()
							} else {
								None
							}
						})
						.unwrap_or(0);

					let body_start = header_end + 4;
					if buf.len() < body_start + content_len {
						break;
					}

					let body_bytes = &buf[body_start..body_start + content_len];
					let json_body: serde_json::Value =
						serde_json::from_slice(body_bytes).unwrap_or(serde_json::Value::Null);
					let method = json_body.get("method").and_then(|v| v.as_str()).unwrap_or("");
					let req_id = json_body.get("id").cloned().unwrap_or(serde_json::Value::from(1));

					buf.drain(..body_start + content_len);

					if method == "initialize" {
						let resp = json!({
							"jsonrpc": "2.0",
							"id": req_id,
							"result": {
								"protocolVersion": "2024-11-05",
								"capabilities": { "tools": {} },
								"serverInfo": { "name": "secure_call_srv", "version": "1.0.0" }
							}
						});
						let resp_bytes = serde_json::to_vec(&resp).unwrap();
						let http_resp = format!(
							"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
							resp_bytes.len()
						);
						let _ = socket.write_all(http_resp.as_bytes());
						let _ = socket.write_all(&resp_bytes);
						let _ = socket.flush();
					} else if method == "notifications/initialized" {
						let http_resp = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n";
						let _ = socket.write_all(http_resp.as_bytes());
						let _ = socket.flush();
					} else if method == "tools/list" {
						let resp = json!({
							"jsonrpc": "2.0",
							"id": req_id,
							"result": {
								"tools": [{
									"name": "call_secret_tool",
									"description": "tool that fails with secret in body",
									"inputSchema": { "type": "object" }
								}]
							}
						});
						let resp_bytes = serde_json::to_vec(&resp).unwrap();
						let http_resp = format!(
							"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
							resp_bytes.len()
						);
						let _ = socket.write_all(http_resp.as_bytes());
						let _ = socket.write_all(&resp_bytes);
						let _ = socket.flush();
					} else if method == "tools/call" {
						let body = r#"{"error":"internal_failure","secret_token":"CALL_TIME_SUPER_SECRET_TOKEN"}"#;
						let http_resp = format!(
							"HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
							body.len(),
							body
						);
						let _ = socket.write_all(http_resp.as_bytes());
						let _ = socket.flush();
						done = true;
						break;
					}
				}
				if done {
					break;
				}
			}
		}
	});

	fs::write(
		omp_dir.join("mcp.json"),
		json!({
			"mcpServers": {
				"secure_call_srv": {
					"url": format!("http://{srv_addr}/")
				}
			}
		})
		.to_string(),
	)
	.unwrap();

	let (addr, handle) = spawn_mock_model_server("mcp__secure_call_srv_call_secret_tool", json!({}));
	let req = make_run_request(
		addr,
		&project_dir,
		Approvals::new(
			ApprovalMode::Write,
			vec!["mcp__secure_call_srv_call_secret_tool".to_string()],
		),
	);

	let runner = PiAgentRunner::new().expect("runner");
	let stream = runner.run(&req).expect("run");
	let mut tool_results = Vec::new();
	for ev in stream {
		if let Ok(Event::ToolEnd { id, tool, result, error }) = ev {
			tool_results.push((id, tool, result, error));
		}
	}
	handle.join().unwrap();
	srv_handle.join().unwrap();

	assert_eq!(tool_results.len(), 1);
	assert_eq!(tool_results[0].1, "mcp__secure_call_srv_call_secret_tool");
	assert!(tool_results[0].3.is_some(), "tool call must report error on HTTP 500");
	assert!(
		!tool_results[0].2.contains("CALL_TIME_SUPER_SECRET_TOKEN"),
		"tool call result must NOT leak raw HTTP response body or secrets: result was '{}'",
		tool_results[0].2
	);
	assert!(
		tool_results[0].2.contains("HTTP error 500"),
		"tool call result must safely state HTTP error status: result was '{}'",
		tool_results[0].2
	);
}

#[test]
fn test_mcp_tool_call_json_rpc_error_redacts_message_secrets() {
	let temp = TempDir::new("mcp-jsonrpc-secret");
	let project_dir = temp.path.join("proj");
	let omp_dir = project_dir.join(".omp");
	fs::create_dir_all(&omp_dir).unwrap();

	let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
	let srv_addr = listener.local_addr().unwrap();
	let srv_handle = std::thread::spawn(move || {
		let _ = listener.set_nonblocking(true);
		let deadline = std::time::Instant::now() + Duration::from_secs(5);
		let mut done = false;
		while !done && std::time::Instant::now() < deadline {
			let (mut socket, _) = match listener.accept() {
				Ok(conn) => conn,
				Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
					std::thread::sleep(Duration::from_millis(10));
					continue;
				}
				Err(_) => break,
			};
			let _ = socket.set_read_timeout(Some(Duration::from_secs(1)));
			let _ = socket.set_write_timeout(Some(Duration::from_secs(2)));
			let mut buf = Vec::new();
			let mut tmp = [0u8; 1024];
			while !done && std::time::Instant::now() < deadline {
				let n = match socket.read(&mut tmp) {
					Ok(0) => break,
					Ok(n) => n,
					Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
						continue;
					}
					Err(_) => break,
				};
				buf.extend_from_slice(&tmp[..n]);

				while let Some(header_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
					let header_str = String::from_utf8_lossy(&buf[..header_end]);
					let content_len = header_str
						.lines()
						.find_map(|line| {
							let l = line.to_ascii_lowercase();
							if l.starts_with("content-length:") {
								l.split(':').nth(1)?.trim().parse::<usize>().ok()
							} else {
								None
							}
						})
						.unwrap_or(0);
					let body_start = header_end + 4;
					if buf.len() < body_start + content_len {
						break;
					}

					let body_bytes = &buf[body_start..body_start + content_len];
					let json_body: serde_json::Value =
						serde_json::from_slice(body_bytes).unwrap_or(serde_json::Value::Null);
					let method = json_body.get("method").and_then(|v| v.as_str()).unwrap_or("");
					let req_id = json_body.get("id").cloned().unwrap_or(serde_json::Value::from(1));

					buf.drain(..body_start + content_len);

					if method == "initialize" {
						let resp = json!({
							"jsonrpc": "2.0",
							"id": req_id,
							"result": {
								"protocolVersion": "2024-11-05",
								"capabilities": { "tools": {} },
								"serverInfo": { "name": "secure_rpc_srv", "version": "1.0.0" }
							}
						});
						let resp_bytes = serde_json::to_vec(&resp).unwrap();
						let http_resp = format!(
							"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
							resp_bytes.len()
						);
						let _ = socket.write_all(http_resp.as_bytes());
						let _ = socket.write_all(&resp_bytes);
						let _ = socket.flush();
					} else if method == "notifications/initialized" {
						let http_resp = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n";
						let _ = socket.write_all(http_resp.as_bytes());
						let _ = socket.flush();
					} else if method == "tools/list" {
						let resp = json!({
							"jsonrpc": "2.0",
							"id": req_id,
							"result": {
								"tools": [{
									"name": "call_rpc_tool",
									"description": "tool that fails with JSON-RPC secret error",
									"inputSchema": { "type": "object" }
								}]
							}
						});
						let resp_bytes = serde_json::to_vec(&resp).unwrap();
						let http_resp = format!(
							"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
							resp_bytes.len()
						);
						let _ = socket.write_all(http_resp.as_bytes());
						let _ = socket.write_all(&resp_bytes);
						let _ = socket.flush();
					} else if method == "tools/call" {
						let resp = json!({
							"jsonrpc": "2.0",
							"id": req_id,
							"error": {
								"code": -32603,
								"message": "database error: secret_key=SUPER_SECRET_JSONRPC_TOKEN"
							}
						});
						let resp_bytes = serde_json::to_vec(&resp).unwrap();
						let http_resp = format!(
							"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
							resp_bytes.len()
						);
						let _ = socket.write_all(http_resp.as_bytes());
						let _ = socket.write_all(&resp_bytes);
						let _ = socket.flush();
						done = true;
						break;
					}
				}
				if done {
					break;
				}
			}
		}
	});

	fs::write(
		omp_dir.join("mcp.json"),
		json!({
			"mcpServers": {
				"secure_rpc_srv": {
					"url": format!("http://{srv_addr}/")
				}
			}
		})
		.to_string(),
	)
	.unwrap();

	let (addr, handle) = spawn_mock_model_server("mcp__secure_rpc_srv_call_rpc_tool", json!({}));
	let req = make_run_request(
		addr,
		&project_dir,
		Approvals::new(
			ApprovalMode::Write,
			vec!["mcp__secure_rpc_srv_call_rpc_tool".to_string()],
		),
	);

	let runner = PiAgentRunner::new().expect("runner");
	let stream = runner.run(&req).expect("run");
	let mut tool_results = Vec::new();
	for ev in stream {
		if let Ok(Event::ToolEnd { id, tool, result, error }) = ev {
			tool_results.push((id, tool, result, error));
		}
	}
	handle.join().unwrap();
	srv_handle.join().unwrap();

	assert_eq!(tool_results.len(), 1);
	assert_eq!(tool_results[0].1, "mcp__secure_rpc_srv_call_rpc_tool");
	assert!(tool_results[0].3.is_some(), "tool call must report error on JSON-RPC error");
	assert!(
		!tool_results[0].2.contains("SUPER_SECRET_JSONRPC_TOKEN"),
		"tool call result must NOT leak raw JSON-RPC error message or secrets: result was '{}'",
		tool_results[0].2
	);
	assert!(
		tool_results[0].2.contains("JSON-RPC error -32603"),
		"tool call result must preserve numeric JSON-RPC error code: result was '{}'",
		tool_results[0].2
	);
}
