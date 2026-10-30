use pi_agent::mcp::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Clone)]
struct TestTool {
    name: String,
    server_name: String,
    tool_name: String,
}

impl McpToolOriginSource for TestTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn origin_key(&self) -> Option<String> {
        Some(get_mcp_tool_origin_key(&self.server_name, &self.tool_name))
    }
}

#[tokio::test]
async fn test_naming_and_deduplication() {
    // 1. Sanitize
    assert_eq!(sanitize_mcp_tool_name_part("chrome-devtools-mcp", "server"), "chrome_devtools_mcp");
    assert_eq!(sanitize_mcp_tool_name_part("atlassian:atlassian", "server"), "atlassian_atlassian");
    assert_eq!(sanitize_mcp_tool_name_part("___", "fallback"), "fallback");

    // 2. Mint name within limit
    assert_eq!(create_mcp_tool_name("puppeteer", "puppeteer_screenshot"), "mcp__puppeteer_screenshot");
    assert_eq!(create_mcp_tool_name("my_server", "do_thing"), "mcp__my_server_do_thing");

    // 3. Cap overlong names at 64 chars with deterministic hash suffix
    let long_tool = "chrome_devtools_performance_analyze_insight_extra_long_subsystem_name";
    let minted = create_mcp_tool_name("chrome-devtools-mcp", long_tool);
    assert!(minted.len() <= 64, "Minted name too long: {} (len {})", minted, minted.len());
    assert!(minted.starts_with("mcp__chrome_devtools_mcp_"));

    // Deterministic
    let minted2 = create_mcp_tool_name("chrome-devtools-mcp", long_tool);
    assert_eq!(minted, minted2);

    let different_long = "chrome_devtools_performance_analyze_different_thing_entirely_long";
    let minted_diff = create_mcp_tool_name("chrome-devtools-mcp", different_long);
    assert_ne!(minted, minted_diff);
    assert!(minted_diff.len() <= 64);

    // 4. Deduplicate MCP tools by stable origin key winner (not insertion order)
    let t1 = TestTool {
        name: "mcp__shared_tool".to_string(),
        server_name: "server_b".to_string(),
        tool_name: "tool".to_string(),
    };
    let t2 = TestTool {
        name: "mcp__shared_tool".to_string(),
        server_name: "server_a".to_string(),
        tool_name: "tool".to_string(),
    };

    // server_a\0tool < server_b\0tool, so server_a must win regardless of input order
    let list1 = vec![t1.clone(), t2.clone()];
    let dedup1 = deduplicate_mcp_tools_by_name(&list1);
    assert_eq!(dedup1.len(), 1);
    assert_eq!(dedup1[0].server_name, "server_a");

    let list2 = vec![t2.clone(), t1.clone()];
    let dedup2 = deduplicate_mcp_tools_by_name(&list2);
    assert_eq!(dedup2.len(), 1);
    assert_eq!(dedup2[0].server_name, "server_a");
}

#[test]
fn test_redaction_and_timeout() {
    let raw_url = "https://api.example.com/mcp?apiKey=secret123&token=abc&normal=ok";
    let redacted = redact_url_for_log(raw_url);
    assert!(!redacted.contains("secret123"));
    assert!(!redacted.contains("abc"));
    assert!(redacted.contains("apiKey=[redacted]"));
    assert!(redacted.contains("token=[redacted]"));
    assert!(redacted.contains("normal=ok"));

    let mut headers = HashMap::new();
    headers.insert("Authorization".to_string(), "Bearer secret_token".to_string());
    headers.insert("X-Custom".to_string(), "value".to_string());
    let red_headers = redact_headers_for_log(&headers);
    assert_eq!(red_headers.get("Authorization").unwrap(), "[redacted]");
    assert_eq!(red_headers.get("X-Custom").unwrap(), "value");

    assert_eq!(resolve_mcp_timeout_ms(Some(5000)), 5000);
    assert_eq!(describe_mcp_timeout(0), "disabled");
    assert_eq!(describe_mcp_timeout(30000), "30000ms");
}
#[test]
fn mcp_config_debug_redacts_credentials() {
	use pi_agent::mcp::types::{
		MCPHttpServerConfig, MCPServerConfig, MCPSseServerConfig, MCPStdioServerConfig,
	};
	use std::collections::HashMap;

	let mut env = HashMap::new();
	env.insert("SECRET_KEY".to_string(), "super_secret_env_12345".to_string());
	let stdio = MCPServerConfig::Stdio(MCPStdioServerConfig {
		command: "my-tool".to_string(),
		args: vec!["--arg".to_string()],
		env,
		cwd: None,
		timeout: Some(30),
		request_id_format: None,
	});
	let debug_stdio = format!("{stdio:?}");
	assert!(!debug_stdio.contains("super_secret_env_12345"));
	assert!(debug_stdio.contains("SECRET_KEY"));
	assert!(debug_stdio.contains("[REDACTED]"));

	let mut headers = HashMap::new();
	headers.insert("Authorization".to_string(), "Bearer secret_token_xyz".to_string());
	let http = MCPServerConfig::Http(MCPHttpServerConfig {
		url: "https://user:secret_pass@example.com/mcp?api_key=query_secret_abc#frag".to_string(),
		headers: headers.clone(),
		timeout: Some(30),
		request_id_format: None,
		header_policy: None,
		auth: Some(serde_json::json!({ "token": "auth_token_999" })),
		oauth: Some(serde_json::json!({ "client_secret": "oauth_secret_888" })),
	});
	let debug_http = format!("{http:?}");
	assert!(!debug_http.contains("secret_pass"));
	assert!(!debug_http.contains("query_secret_abc"));
	assert!(!debug_http.contains("secret_token_xyz"));
	assert!(!debug_http.contains("auth_token_999"));
	assert!(!debug_http.contains("oauth_secret_888"));
	assert!(debug_http.contains("[REDACTED]"));

	let sse = MCPServerConfig::Sse(MCPSseServerConfig {
		url: "https://example.com/sse?secret_param=123".to_string(),
		headers,
		timeout: Some(30),
		request_id_format: None,
		header_policy: None,
		auth: Some(serde_json::json!({ "key": "sse_auth_secret" })),
		oauth: None,
	});
	let debug_sse = format!("{sse:?}");
	assert!(!debug_sse.contains("secret_param=123"));
	assert!(!debug_sse.contains("secret_token_xyz"));
	assert!(!debug_sse.contains("sse_auth_secret"));
	assert!(debug_sse.contains("[REDACTED]"));
}

#[tokio::test]
async fn test_oauth_explicit_rejection() {
    let http_cfg = MCPServerConfig::Http(MCPHttpServerConfig {
        url: "https://example.com/mcp".to_string(),
        headers: HashMap::new(),
        timeout: None,
        request_id_format: None,
        header_policy: None,
        auth: Some(json!({ "type": "oauth" })),
        oauth: None,
    });
    let err = validate_server_config("oauth_server", &http_cfg).unwrap_err();
    assert!(err.iter().any(|e| e.contains("OAuth authentication requires credential storage")));

    let res = connect_to_server("oauth_server", http_cfg).await;
    assert!(res.is_err());
}

#[tokio::test]
async fn test_stdio_initialize_list_call_and_ping() {
    // Write a mock stdio server in python
    let temp_dir = std::env::temp_dir();
    let script_path = temp_dir.join("mock_mcp_stdio.py");
    let mut script = std::fs::File::create(&script_path).unwrap();
    write!(
        script,
        r#"
import sys, json

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")

    if method == "initialize":
        # First send a ping request to the client to verify client handles server-to-client ping
        ping_req = json.dumps({{"jsonrpc": "2.0", "id": "server-ping-1", "method": "ping"}})
        sys.stdout.write(ping_req + "\n")
        sys.stdout.flush()

        # Then answer initialize
        resp = {{
            "jsonrpc": "2.0",
            "id": mid,
            "result": {{
                "protocolVersion": "2025-11-25",
                "capabilities": {{ "tools": {{}} }},
                "serverInfo": {{ "name": "mock-stdio", "version": "1.0.0" }}
            }}
        }}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif method == "notifications/initialized":
        pass
    elif mid == "server-ping-1":
        # Got pong response from client!
        pass
    elif method == "tools/list":
        cursor = msg.get("params", {{}}).get("cursor")
        if not cursor:
            # Page 1
            resp = {{
                "jsonrpc": "2.0",
                "id": mid,
                "result": {{
                    "tools": [{{ "name": "echo", "description": "Echo back", "inputSchema": {{}} }}],
                    "nextCursor": "page-2"
                }}
            }}
        else:
            # Page 2
            resp = {{
                "jsonrpc": "2.0",
                "id": mid,
                "result": {{
                    "tools": [{{ "name": "fail_tool", "description": "Fails", "inputSchema": {{}} }}],
                    "nextCursor": None
                }}
            }}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif method == "tools/call":
        tname = msg.get("params", {{}}).get("name")
        if tname == "echo":
            resp = {{
                "jsonrpc": "2.0",
                "id": mid,
                "result": {{
                    "content": [{{ "type": "text", "text": "echo result" }}],
                    "isError": False
                }}
            }}
        else:
            resp = {{
                "jsonrpc": "2.0",
                "id": mid,
                "result": {{
                    "content": [{{ "type": "text", "text": "bad tool call" }}],
                    "isError": True
                }}
            }}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
"#
    ).unwrap();

    let config = MCPServerConfig::Stdio(MCPStdioServerConfig {
        command: "python3".to_string(),
        args: vec![script_path.to_str().unwrap().to_string()],
        env: HashMap::new(),
        cwd: None,
        timeout: Some(10000),
        request_id_format: Some(RequestIdFormat::Number),
    });

    let conn = connect_to_server("test_mock", config).await.expect("Failed to connect stdio");
    assert_eq!(conn.server_info.name, "mock-stdio");

    // Test list_tools paginated (must fetch both page 1 and page 2)
    let tools = conn.list_tools().await.expect("Failed to list tools");
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].name, "echo");
    assert_eq!(tools[1].name, "fail_tool");

    // Test call_tool success
    let res = conn.call_tool("echo", json!({})).await.expect("Failed to call echo");
    assert_eq!(res.is_error, Some(false));
    assert_eq!(res.content.len(), 1);
    match &res.content[0] {
        MCPContent::Text { text } => assert_eq!(text, "echo result"),
        _ => panic!("Expected text content"),
    }

    // Test call_tool error (isError = true)
    let err_res = conn.call_tool("fail_tool", json!({})).await.expect("Failed to call fail_tool");
    assert_eq!(err_res.is_error, Some(true));

    conn.close().await.expect("Failed to close connection");
    assert!(matches!(conn.transport.read().await.request("tools/call", None, None).await, Err(McpError::Closed)));
    assert!(matches!(conn.transport.read().await.notify("ping", None).await, Err(McpError::Closed)));
    assert!(matches!(conn.call_tool("echo", json!({})).await, Err(McpError::Closed | McpError::Transport(_))));
    let _ = std::fs::remove_file(script_path);
}

#[tokio::test]
async fn test_stdio_timeout_and_zero_disable() {
    let temp_dir = std::env::temp_dir();
    let script_path = temp_dir.join("mock_mcp_sleep.py");
    let mut script = std::fs::File::create(&script_path).unwrap();
    write!(
        script,
        r#"
import sys, json, time

for line in sys.stdin:
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    if method == "initialize":
        resp = {{
            "jsonrpc": "2.0",
            "id": mid,
            "result": {{
                "protocolVersion": "2025-11-25",
                "capabilities": {{ "tools": {{}} }},
                "serverInfo": {{ "name": "mock-sleep", "version": "1.0.0" }}
            }}
        }}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif method == "tools/list":
        time.sleep(1.5)
        resp = {{
            "jsonrpc": "2.0",
            "id": mid,
            "result": {{ "tools": [] }}
        }}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
"#
    ).unwrap();

    // 1. Timeout 100ms should time out on tools/list (which takes 500ms)
    let config_timeout = MCPServerConfig::Stdio(MCPStdioServerConfig {
        command: "python3".to_string(),
        args: vec![script_path.to_str().unwrap().to_string()],
        env: HashMap::new(),
        cwd: None,
        timeout: Some(400),
        request_id_format: None,
    });
    let conn = connect_to_server("sleepy", config_timeout).await.expect("connect");
    let err = conn.list_tools().await.unwrap_err();
    assert!(matches!(err, McpError::Timeout(_)));
    conn.close().await.unwrap();

    // 2. Timeout 0 disables timeout
    let config_zero = MCPServerConfig::Stdio(MCPStdioServerConfig {
        command: "python3".to_string(),
        args: vec![script_path.to_str().unwrap().to_string()],
        env: HashMap::new(),
        cwd: None,
        timeout: Some(0),
        request_id_format: None,
    });
    let conn2 = connect_to_server("sleepy2", config_zero).await.expect("connect");
    let tools = conn2.list_tools().await.expect("tools list without timeout");
    assert_eq!(tools.len(), 0);
    conn2.close().await.unwrap();

    let _ = std::fs::remove_file(script_path);
}

#[tokio::test]
async fn test_http_transport_json_and_sse() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap();
                let req_str = String::from_utf8_lossy(&buf[..n]);

                // Check method and body
                let body_start = req_str.find("\r\n\r\n").map(|i| i + 4).unwrap_or(n);
                let body_str = &req_str[body_start..];
                let json_body: Value = serde_json::from_str(body_str).unwrap_or(Value::Null);

                let method = json_body.get("method").and_then(|v| v.as_str()).unwrap_or("");
                let mid = json_body.get("id").cloned().unwrap_or(json!(1));

                if method == "initialize" {
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": mid,
                        "result": {
                            "protocolVersion": "2025-11-25",
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": "mock-http", "version": "1.0.0" }
                        }
                    });
                    let resp_bytes = serde_json::to_vec(&resp).unwrap();
                    let http_resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: test-session-99\r\nContent-Length: {}\r\n\r\n",
                        resp_bytes.len()
                    );
                    socket.write_all(http_resp.as_bytes()).await.unwrap();
                    socket.write_all(&resp_bytes).await.unwrap();
                } else if method == "notifications/initialized" {
                    let http_resp = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
                    socket.write_all(http_resp.as_bytes()).await.unwrap();
                } else if method == "tools/list" {
                    // Send response as SSE stream!
                    // Verify headers had MCP-Protocol-Version and Mcp-Session-Id
                    assert!(req_str.contains("mcp-session-id: test-session-99") || req_str.contains("Mcp-Session-Id: test-session-99"));
                    assert!(req_str.contains("mcp-protocol-version: 2025-11-25") || req_str.contains("MCP-Protocol-Version: 2025-11-25"));

                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": mid,
                        "result": {
                            "tools": [{ "name": "http_tool", "description": "From HTTP", "inputSchema": {} }]
                        }
                    });
                    let sse_data = format!("data: {}\n\n", serde_json::to_string(&resp).unwrap());
                    let http_resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
                        sse_data.len(),
                        sse_data
                    );
                    socket.write_all(http_resp.as_bytes()).await.unwrap();
                }
            });
        }
    });

    let config = MCPServerConfig::Http(MCPHttpServerConfig {
        url: format!("http://{}", addr),
        headers: HashMap::new(),
        timeout: Some(5000),
        request_id_format: None,
        header_policy: Some("origin-locked".to_string()),
        auth: None,
        oauth: None,
    });

    let conn = connect_to_server("mock_http", config).await.expect("connect http");
    assert_eq!(conn.server_info.name, "mock-http");

    let tools = conn.list_tools().await.expect("tools list via SSE");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "http_tool");
}

#[tokio::test]
async fn test_legacy_sse_transport() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let endpoint_path = "/message_endpoint";
    let post_received = Arc::new(AtomicUsize::new(0));
    let post_received_clone = post_received.clone();

    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let p_clone = post_received_clone.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap();
                let req_str = String::from_utf8_lossy(&buf[..n]);

                if req_str.starts_with("GET /sse") {
                    // Send endpoint event, and then send JSON-RPC responses over the SSE connection
                    let sse_header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
                    socket.write_all(sse_header.as_bytes()).await.unwrap();

                    // 1. Send endpoint event
                    let endpoint_event = format!("event: endpoint\r\ndata: {}\r\n\r\n", endpoint_path);
                    socket.write_all(endpoint_event.as_bytes()).await.unwrap();

                    // Wait for POST requests to arrive, then send JSON-RPC responses down this SSE stream
                    loop {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        let count = p_clone.load(Ordering::SeqCst);
                        if count == 1 {
                            // Send initialize response
                            let resp = json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "result": {
                                    "protocolVersion": "2025-11-25",
                                    "capabilities": { "tools": {} },
                                    "serverInfo": { "name": "mock-legacy-sse", "version": "1.0.0" }
                                }
                            });
                            let msg = format!("event: message\r\ndata: {}\r\n\r\n", serde_json::to_string(&resp).unwrap());
                            if socket.write_all(msg.as_bytes()).await.is_err() { break; }
                            p_clone.store(2, Ordering::SeqCst);
                        } else if count == 3 {
                            // Send tools/call response
                            let resp = json!({
                                "jsonrpc": "2.0",
                                "id": 2,
                                "result": {
                                    "content": [{ "type": "text", "text": "legacy call success" }],
                                    "isError": false
                                }
                            });
                            let msg = format!("event: message\r\ndata: {}\r\n\r\n", serde_json::to_string(&resp).unwrap());
                            if socket.write_all(msg.as_bytes()).await.is_err() { break; }
                            p_clone.store(4, Ordering::SeqCst);
                        }
                    }
                } else if req_str.starts_with("POST /message_endpoint") {
                    if req_str.contains("\"initialize\"") {
                        p_clone.store(1, Ordering::SeqCst);
                    } else if req_str.contains("\"tools/call\"") {
                        p_clone.store(3, Ordering::SeqCst);
                    }
                    let ack = "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n";
                    socket.write_all(ack.as_bytes()).await.unwrap();
                }
            });
        }
    });

    let config = MCPServerConfig::Sse(MCPSseServerConfig {
        url: format!("http://{}/sse", addr),
        headers: HashMap::new(),
        timeout: Some(5000),
        request_id_format: Some(RequestIdFormat::Number),
        header_policy: None,
        auth: None,
        oauth: None,
    });

    let conn = connect_to_server("legacy_sse", config).await.expect("connect legacy sse");
    assert_eq!(conn.server_info.name, "mock-legacy-sse");

    let res = conn.call_tool("test_tool", json!({})).await.expect("call tool via legacy SSE");
    assert_eq!(res.is_error, Some(false));
    match &res.content[0] {
        MCPContent::Text { text } => assert_eq!(text, "legacy call success"),
        _ => panic!("Expected text"),
    }
}

#[tokio::test]
async fn test_stdio_drop_kills_child() {
    let temp_dir = std::env::temp_dir();
    let pid_file = temp_dir.join(format!("mcp_pid_{}.txt", std::process::id()));
    let script_path = temp_dir.join("mock_mcp_pid.py");
    let mut script = std::fs::File::create(&script_path).unwrap();
    write!(
        script,
        r#"
import sys, json, os

pid_path = sys.argv[1]
with open(pid_path, "w") as f:
    f.write(str(os.getpid()))

for line in sys.stdin:
    msg = json.loads(line)
    mid = msg.get("id")
    resp = {{
        "jsonrpc": "2.0",
        "id": mid,
        "result": {{
            "protocolVersion": "2025-11-25",
            "capabilities": {{ "tools": {{}} }},
            "serverInfo": {{ "name": "mock-pid", "version": "1.0.0" }}
        }}
    }}
    sys.stdout.write(json.dumps(resp) + "\n")
    sys.stdout.flush()
"#
    ).unwrap();

    let config = MCPServerConfig::Stdio(MCPStdioServerConfig {
        command: "python3".to_string(),
        args: vec![script_path.to_str().unwrap().to_string(), pid_file.to_str().unwrap().to_string()],
        env: HashMap::new(),
        cwd: None,
        timeout: Some(5000),
        request_id_format: None,
    });

    let conn = connect_to_server("pid_test", config).await.expect("connect");
    let pid_str = std::fs::read_to_string(&pid_file).expect("read pid file");
    let pid: i32 = pid_str.trim().parse().expect("parse pid");

    // Verify child is alive
    let is_alive = libc_kill_check(pid);
    assert!(is_alive, "Child should be alive before drop");

    // Drop connection
    drop(conn);

    tokio::time::sleep(Duration::from_millis(250)).await;

    // Verify child is dead
    let is_alive_after = libc_kill_check(pid);
    assert!(!is_alive_after, "Child should be killed after drop");

    let _ = std::fs::remove_file(script_path);
    let _ = std::fs::remove_file(pid_file);
}

fn libc_kill_check(pid: i32) -> bool {
    let res = std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status();
    match res {
        Ok(status) => status.success(),
        Err(_) => std::path::Path::new(&format!("/proc/{}", pid)).exists(),
    }
}

#[tokio::test]
async fn test_stdio_reconnect_on_error() {
    let temp_dir = std::env::temp_dir();
    let state_file = temp_dir.join(format!("mcp_state_{}.txt", std::process::id()));
    let _ = std::fs::write(&state_file, "0");

    let script_path = temp_dir.join("mock_mcp_reconnect.py");
    let mut script = std::fs::File::create(&script_path).unwrap();
    write!(
        script,
        r#"
import sys, json

state_path = sys.argv[1]

for line in sys.stdin:
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")

    if method == "initialize":
        resp = {{
            "jsonrpc": "2.0",
            "id": mid,
            "result": {{
                "protocolVersion": "2025-11-25",
                "capabilities": {{ "tools": {{}} }},
                "serverInfo": {{ "name": "mock-reconnect", "version": "1.0.0" }}
            }}
        }}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif method == "notifications/initialized":
        pass
    elif method == "tools/call":
        with open(state_path, "r") as f:
            val = int(f.read().strip())
        if val == 0:
            # First attempt: crash/exit abruptly
            with open(state_path, "w") as f:
                f.write("1")
            sys.exit(1)
        else:
            # Second attempt (after reconnect): succeed!
            resp = {{
                "jsonrpc": "2.0",
                "id": mid,
                "result": {{
                    "content": [{{ "type": "text", "text": "reconnected success" }}],
                    "isError": False
                }}
            }}
            sys.stdout.write(json.dumps(resp) + "\n")
            sys.stdout.flush()
"#
    ).unwrap();

    let config = MCPServerConfig::Stdio(MCPStdioServerConfig {
        command: "python3".to_string(),
        args: vec![script_path.to_str().unwrap().to_string(), state_file.to_str().unwrap().to_string()],
        env: HashMap::new(),
        cwd: None,
        timeout: Some(5000),
        request_id_format: None,
    });

    let conn = connect_to_server("reconn_test", config).await.expect("connect");

    // 1. First call fails because child crashed mid-execution (uncertain delivery: NEVER replayed!)
    let first_err = conn.call_tool("any_tool", json!({})).await.unwrap_err();
    assert!(
        matches!(first_err, McpError::Transport(_)),
        "Expected Transport error on crashed child, got: {:?}",
        first_err
    );
    // Assert mutation was NOT replayed (file contains exactly '1', not '2')
    let val_after_crash = std::fs::read_to_string(&state_file).unwrap();
    assert_eq!(val_after_crash.trim(), "1", "Tool call must NOT be replayed after crash");

    // 2. Future call succeeds because transport automatically reconnected!
    let res = conn.call_tool("any_tool", json!({})).await.expect("subsequent call_tool should succeed via reconnected child");
    assert_eq!(res.is_error, Some(false));
    match &res.content[0] {
        MCPContent::Text { text } => assert_eq!(text, "reconnected success"),
        _ => panic!("Expected text"),
    }

    let _ = std::fs::remove_file(script_path);
    let _ = std::fs::remove_file(state_file);
}

#[test]
fn test_exact_bun_hash_parity() {
    // 12 test vectors generated directly by Bun.hash in TypeScript across length boundaries (0, 1, 3, 4, 8, 16, 17, 48, 59, 69) and UTF-8
    let vectors = [
        ("", 0x409638ee2bde459u64, "27k1wwwh"),
        ("a", 0x28d2053309d28531u64, "mcie3cqm"),
        ("abc", 0x2a4f1d7cb516c72u64, "1g45uqqk"),
        ("tool", 0x38197f1f8ba0f753u64, "upn3vude"),
        ("12345678", 0x493e74fe7f93f7e9u64, "143j6g0h"),
        ("1234567890123456", 0x12832107d4cf0b7bu64, "a4ukh35r"),
        ("12345678901234567", 0xffbbc5e59ba62f4eu64, "3w04xpq3"),
        ("mcp__server_tool_name_boundary_test_32_bytes_len", 0x478d12aed807ccf7u64, "13661qwy"),
        ("mcp__server_tool_name_boundary_test_48_bytes_len_plus_extra", 0x1907531d5e1438a6u64, "dp9zy3hk"),
        ("chrome_devtools_performance_analyze_insight_extra_long_subsystem_name", 0x8a050e6c2523577fu64, "23k63v07"),
        ("mcp__test_tool_with_utf8_кириллица_тест_12345678901234567890", 0x161b665311cedbb2u64, "c3p42hrd"),
        ("mcp__emoji_🚀_test_tool_name_overlong_boundary_check_1234567890", 0xedc88d7e811c4a2cu64, "3m6d7nze"),
    ];

    for (input, expected_u64, _expected_slice8) in vectors {
        let h = bun_wyhash(0, input.as_bytes());
        assert_eq!(
            h, expected_u64,
            "Hash mismatch for input '{}': expected {:x}, got {:x}",
            input, expected_u64, h
        );
    }

    // Overlong name capped at 64 chars ends in exact Bun.hash slice
    let raw_name = "mcp__chrome_devtools_mcp_chrome_devtools_performance_analyze_insight";
    let capped = cap_mcp_tool_name_length(raw_name);
    assert_eq!(capped.len(), 64);
    assert!(
        capped.ends_with("_2cas0kft"),
        "Expected _2cas0kft suffix from Bun.hash wyhash parity, got: {}",
        capped
    );

    let raw_utf8 = "mcp__emoji_🚀_test_tool_name_overlong_boundary_check_1234567890";
    let capped_utf8 = cap_mcp_tool_name_length(raw_utf8);
    assert!(capped_utf8.ends_with("_3m6d7nze"));
}

#[tokio::test]
async fn test_http_mismatched_response_id_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap();
                let req_str = String::from_utf8_lossy(&buf[..n]);

                let body_start = req_str.find("\r\n\r\n").map(|i| i + 4).unwrap_or(n);
                let body_str = &req_str[body_start..];
                let json_body: Value = serde_json::from_str(body_str).unwrap_or(Value::Null);
                let method = json_body.get("method").and_then(|v| v.as_str()).unwrap_or("");

                if method == "initialize" {
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {
                            "protocolVersion": "2025-11-25",
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": "mock-mismatch", "version": "1.0.0" }
                        }
                    });
                    let resp_bytes = serde_json::to_vec(&resp).unwrap();
                    let http_resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                        resp_bytes.len()
                    );
                    socket.write_all(http_resp.as_bytes()).await.unwrap();
                    socket.write_all(&resp_bytes).await.unwrap();
                } else if method == "notifications/initialized" {
                    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await.unwrap();
                } else if method == "tools/list" {
                    // Deliberately return MISMATCHED ID (999 instead of 2)
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": 999,
                        "result": { "tools": [] }
                    });
                    let resp_bytes = serde_json::to_vec(&resp).unwrap();
                    let http_resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                        resp_bytes.len()
                    );
                    socket.write_all(http_resp.as_bytes()).await.unwrap();
                    socket.write_all(&resp_bytes).await.unwrap();
                }
            });
        }
    });

    let config = MCPServerConfig::Http(MCPHttpServerConfig {
        url: format!("http://{}", addr),
        headers: HashMap::new(),
        timeout: Some(5000),
        request_id_format: Some(RequestIdFormat::Number),
        header_policy: None,
        auth: None,
        oauth: None,
    });

    let conn = connect_to_server("mismatch_srv", config).await.expect("connect");
    let err = conn.list_tools().await.unwrap_err();
    assert!(
        matches!(err, McpError::Protocol(_)),
        "Expected Protocol error on mismatched ID, got: {:?}",
        err
    );
}

#[tokio::test]
async fn test_http_hanging_sse_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap();
                let req_str = String::from_utf8_lossy(&buf[..n]);

                let body_start = req_str.find("\r\n\r\n").map(|i| i + 4).unwrap_or(n);
                let body_str = &req_str[body_start..];
                let json_body: Value = serde_json::from_str(body_str).unwrap_or(Value::Null);
                let method = json_body.get("method").and_then(|v| v.as_str()).unwrap_or("");

                if method == "initialize" {
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {
                            "protocolVersion": "2025-11-25",
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": "mock-hanging", "version": "1.0.0" }
                        }
                    });
                    let resp_bytes = serde_json::to_vec(&resp).unwrap();
                    let http_resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                        resp_bytes.len()
                    );
                    socket.write_all(http_resp.as_bytes()).await.unwrap();
                    socket.write_all(&resp_bytes).await.unwrap();
                } else if method == "notifications/initialized" {
                    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await.unwrap();
                } else if method == "tools/list" {
                    // Send 200 OK with SSE content type, but NEVER send matching data
                    let http_resp = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n";
                    socket.write_all(http_resp.as_bytes()).await.unwrap();
                    // Sleep indefinitely until client aborts/times out
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            });
        }
    });

    let config = MCPServerConfig::Http(MCPHttpServerConfig {
        url: format!("http://{}", addr),
        headers: HashMap::new(),
        timeout: Some(300), // 300ms timeout
        request_id_format: Some(RequestIdFormat::Number),
        header_policy: None,
        auth: None,
        oauth: None,
    });

    let conn = connect_to_server("hanging_srv", config).await.expect("connect");
    let err = conn.list_tools().await.unwrap_err();
    assert!(
        matches!(err, McpError::Timeout(_)),
        "Expected Timeout error on hanging SSE stream, got: {:?}",
        err
    );
}

#[tokio::test]
async fn test_notification_failure_propagates() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap();
                let req_str = String::from_utf8_lossy(&buf[..n]);

                let body_start = req_str.find("\r\n\r\n").map(|i| i + 4).unwrap_or(n);
                let body_str = &req_str[body_start..];
                let json_body: Value = serde_json::from_str(body_str).unwrap_or(Value::Null);
                let method = json_body.get("method").and_then(|v| v.as_str()).unwrap_or("");

                if method == "initialize" {
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {
                            "protocolVersion": "2025-11-25",
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": "mock-notif-fail", "version": "1.0.0" }
                        }
                    });
                    let resp_bytes = serde_json::to_vec(&resp).unwrap();
                    let http_resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                        resp_bytes.len()
                    );
                    socket.write_all(http_resp.as_bytes()).await.unwrap();
                    socket.write_all(&resp_bytes).await.unwrap();
                } else if method == "notifications/initialized" {
                    // Reject notification with 500 Internal Server Error
                    let http_resp = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n";
                    socket.write_all(http_resp.as_bytes()).await.unwrap();
                }
            });
        }
    });

    let config = MCPServerConfig::Http(MCPHttpServerConfig {
        url: format!("http://{}", addr),
        headers: HashMap::new(),
        timeout: Some(5000),
        request_id_format: Some(RequestIdFormat::Number),
        header_policy: None,
        auth: None,
        oauth: None,
    });

    let res = connect_to_server("notif_fail_srv", config).await;
    assert!(res.is_err(), "connect_to_server must fail if notification fails");
    let err = res.err().unwrap();
    assert!(matches!(err, McpError::Http { status: 500, .. }));
}

#[tokio::test]
async fn test_legacy_sse_stream_close_drains_pending() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap();
                let req_str = String::from_utf8_lossy(&buf[..n]);

                if req_str.starts_with("GET /sse") {
                    let sse_header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n";
                    socket.write_all(sse_header.as_bytes()).await.unwrap();
                    let endpoint_event = "event: endpoint\r\ndata: /msg\r\n\r\n";
                    socket.write_all(endpoint_event.as_bytes()).await.unwrap();

                    // Send initialize response
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {
                            "protocolVersion": "2025-11-25",
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": "mock-abrupt", "version": "1.0.0" }
                        }
                    });
                    let msg = format!("event: message\r\ndata: {}\r\n\r\n", serde_json::to_string(&resp).unwrap());
                    socket.write_all(msg.as_bytes()).await.unwrap();

                    // Now abruptly close socket during the tool call
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let _ = socket.shutdown().await;
                } else if req_str.starts_with("POST /msg") {
                    socket.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n").await.unwrap();
                }
            });
        }
    });

    let config = MCPServerConfig::Sse(MCPSseServerConfig {
        url: format!("http://{}/sse", addr),
        headers: HashMap::new(),
        timeout: Some(5000),
        request_id_format: Some(RequestIdFormat::Number),
        header_policy: None,
        auth: None,
        oauth: None,
    });

    let conn = connect_to_server("abrupt_sse", config).await.expect("connect");
    // Call tool - the SSE stream closes abruptly before response
    let err = conn.call_tool("broken_tool", json!({})).await.unwrap_err();
    assert!(
        matches!(err, McpError::Closed | McpError::Transport(_)),
        "Expected Closed/Transport error on abruptly severed stream, got: {:?}",
        err
    );
}

#[tokio::test]
async fn test_origin_locked_redirect_strips_auth() {
    // Server 1 (origin 1) redirects to Server 2 (origin 2)
    let listener_target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr_target = listener_target.local_addr().unwrap();

    let listener_redir = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr_redir = listener_redir.local_addr().unwrap();

    let auth_leaked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let auth_leaked_clone = auth_leaked.clone();

    // Target server
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener_target.accept().await {
            let leaked = auth_leaked_clone.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap();
                let req_str = String::from_utf8_lossy(&buf[..n]);
                if req_str.contains("authorization:")
                    || req_str.contains("Authorization:")
                    || req_str.contains("mcp-session-id:")
                    || req_str.contains("Mcp-Session-Id:")
                {
                    leaked.store(true, Ordering::SeqCst);
                }
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": {
                        "protocolVersion": "2025-11-25",
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "target-server", "version": "1.0.0" }
                    }
                });
                let resp_bytes = serde_json::to_vec(&resp).unwrap();
                let http_resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                    resp_bytes.len()
                );
                socket.write_all(http_resp.as_bytes()).await.unwrap();
                socket.write_all(&resp_bytes).await.unwrap();
            });
        }
    });

    // Redirecting server (sends 307 to target)
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener_redir.accept().await {
            let target_url = format!("http://{}/mcp", addr_target);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let _ = socket.read(&mut buf).await.unwrap();
                let redir_resp = format!(
                    "HTTP/1.1 307 Temporary Redirect\r\nLocation: {}\r\nContent-Length: 0\r\n\r\n",
                    target_url
                );
                socket.write_all(redir_resp.as_bytes()).await.unwrap();
            });
        }
    });

    let mut headers = HashMap::new();
    headers.insert("Authorization".to_string(), "Bearer secret-token".to_string());

    let config = MCPServerConfig::Http(MCPHttpServerConfig {
        url: format!("http://{}/mcp", addr_redir),
        headers,
        timeout: Some(5000),
        request_id_format: Some(RequestIdFormat::Number),
        header_policy: Some("origin-locked".to_string()),
        auth: None,
        oauth: None,
    });

    let res = connect_to_server("redir_srv", config).await;
    assert!(
        res.is_err(),
        "Origin-locked policy must refuse cross-origin redirects"
    );
    assert!(
        !auth_leaked.load(Ordering::SeqCst),
        "Authorization and session ID must NOT leak across origins"
    );
}

#[tokio::test]
async fn test_no_replay_after_accepted_mutating_call() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let call_count = Arc::new(AtomicUsize::new(0));
    let call_count_clone = call_count.clone();

    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let cc = call_count_clone.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap();
                let req_str = String::from_utf8_lossy(&buf[..n]);

                let body_start = req_str.find("\r\n\r\n").map(|i| i + 4).unwrap_or(n);
                let body_str = &req_str[body_start..];
                let json_body: Value = serde_json::from_str(body_str).unwrap_or(Value::Null);
                let method = json_body.get("method").and_then(|v| v.as_str()).unwrap_or("");

                if method == "initialize" {
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {
                            "protocolVersion": "2025-11-25",
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": "mock-accepted", "version": "1.0.0" }
                        }
                    });
                    let resp_bytes = serde_json::to_vec(&resp).unwrap();
                    let http_resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                        resp_bytes.len()
                    );
                    socket.write_all(http_resp.as_bytes()).await.unwrap();
                    socket.write_all(&resp_bytes).await.unwrap();
                } else if method == "notifications/initialized" {
                    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await.unwrap();
                } else if method == "tools/call" {
                    cc.fetch_add(1, Ordering::SeqCst);
                    // Accept request with 200 OK SSE stream, then abruptly drop socket
                    let http_resp = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n";
                    socket.write_all(http_resp.as_bytes()).await.unwrap();
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    let _ = socket.shutdown().await;
                }
            });
        }
    });

    let config = MCPServerConfig::Http(MCPHttpServerConfig {
        url: format!("http://{}", addr),
        headers: HashMap::new(),
        timeout: Some(5000),
        request_id_format: Some(RequestIdFormat::Number),
        header_policy: None,
        auth: None,
        oauth: None,
    });

    let conn = connect_to_server("accepted_test", config).await.expect("connect");
    let err = conn.call_tool("mutating_action", json!({})).await.unwrap_err();
    assert!(
        matches!(err, McpError::AcceptedStreamError(_)),
        "Expected AcceptedStreamError, got: {:?}",
        err
    );
    // Assert call was only attempted ONCE and NOT replayed!
    assert_eq!(call_count.load(Ordering::SeqCst), 1, "Mutating call must NOT be replayed after acceptance");
}

#[tokio::test]
async fn test_typed_value_id_mismatch_numeric_vs_string() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap();
                let req_str = String::from_utf8_lossy(&buf[..n]);

                let body_start = req_str.find("\r\n\r\n").map(|i| i + 4).unwrap_or(n);
                let body_str = &req_str[body_start..];
                let json_body: Value = serde_json::from_str(body_str).unwrap_or(Value::Null);
                let method = json_body.get("method").and_then(|v| v.as_str()).unwrap_or("");

                if method == "initialize" {
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": "req_1", // Matching string ID
                        "result": {
                            "protocolVersion": "2025-11-25",
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": "mock-typed", "version": "1.0.0" }
                        }
                    });
                    let resp_bytes = serde_json::to_vec(&resp).unwrap();
                    let http_resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                        resp_bytes.len()
                    );
                    socket.write_all(http_resp.as_bytes()).await.unwrap();
                    socket.write_all(&resp_bytes).await.unwrap();
                } else if method == "notifications/initialized" {
                    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await.unwrap();
                } else if method == "tools/list" {
                    // Client sends string id: "req_2", but server returns numeric id: 2!
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": 2, // NUMERIC, not string "req_2"!
                        "result": { "tools": [] }
                    });
                    let resp_bytes = serde_json::to_vec(&resp).unwrap();
                    let http_resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                        resp_bytes.len()
                    );
                    socket.write_all(http_resp.as_bytes()).await.unwrap();
                    socket.write_all(&resp_bytes).await.unwrap();
                }
            });
        }
    });

    let config = MCPServerConfig::Http(MCPHttpServerConfig {
        url: format!("http://{}", addr),
        headers: HashMap::new(),
        timeout: Some(5000),
        request_id_format: Some(RequestIdFormat::String), // Explicit String IDs
        header_policy: None,
        auth: None,
        oauth: None,
    });

    let conn = connect_to_server("typed_srv", config).await.expect("connect");
    let err = conn.list_tools().await.unwrap_err();
    assert!(
        matches!(err, McpError::Protocol(_)),
        "Expected Protocol error on typed ID mismatch (string vs number), got: {:?}",
        err
    );
}

#[tokio::test]
async fn test_legacy_sse_post_401_fails_immediately() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap();
                let req_str = String::from_utf8_lossy(&buf[..n]);

                if req_str.starts_with("GET /sse") {
                    let sse_header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n";
                    socket.write_all(sse_header.as_bytes()).await.unwrap();
                    let endpoint_event = "event: endpoint\r\ndata: /unauthed\r\n\r\n";
                    socket.write_all(endpoint_event.as_bytes()).await.unwrap();
                    // Sleep SSE stream
                    tokio::time::sleep(Duration::from_secs(5)).await;
                } else if req_str.starts_with("POST /unauthed") {
                    // Immediately reject with 401 Unauthorized
                    let http_resp = "HTTP/1.1 401 Unauthorized\r\nContent-Length: 12\r\n\r\nUnauthorized";
                    socket.write_all(http_resp.as_bytes()).await.unwrap();
                }
            });
        }
    });

    let config = MCPServerConfig::Sse(MCPSseServerConfig {
        url: format!("http://{}/sse", addr),
        headers: HashMap::new(),
        timeout: Some(5000),
        request_id_format: Some(RequestIdFormat::Number),
        header_policy: None,
        auth: None,
        oauth: None,
    });

    let res = connect_to_server("unauthed_sse", config).await;
    assert!(res.is_err(), "Must fail immediately on POST 401");
    let err = res.err().unwrap();
    assert!(matches!(err, McpError::Http { status: 401, .. }));
}

#[tokio::test]
async fn test_accepted_json_reset_does_not_replay() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let call_count = Arc::new(AtomicUsize::new(0));
    let call_count_clone = call_count.clone();

    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let cc = call_count_clone.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap();
                let req_str = String::from_utf8_lossy(&buf[..n]);

                let body_start = req_str.find("\r\n\r\n").map(|i| i + 4).unwrap_or(n);
                let body_str = &req_str[body_start..];
                let json_body: Value = serde_json::from_str(body_str).unwrap_or(Value::Null);
                let method = json_body.get("method").and_then(|v| v.as_str()).unwrap_or("");

                if method == "initialize" {
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {
                            "protocolVersion": "2025-11-25",
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": "mock-json-reset", "version": "1.0.0" }
                        }
                    });
                    let resp_bytes = serde_json::to_vec(&resp).unwrap();
                    let http_resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                        resp_bytes.len()
                    );
                    socket.write_all(http_resp.as_bytes()).await.unwrap();
                    socket.write_all(&resp_bytes).await.unwrap();
                } else if method == "notifications/initialized" {
                    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await.unwrap();
                } else if method == "tools/call" {
                    cc.fetch_add(1, Ordering::SeqCst);
                    // Send 200 OK headers with Content-Length 1000, then abruptly close/reset connection without body!
                    let http_resp = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1000\r\n\r\n{\"part\":";
                    socket.write_all(http_resp.as_bytes()).await.unwrap();
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    let _ = socket.shutdown().await;
                }
            });
        }
    });

    let config = MCPServerConfig::Http(MCPHttpServerConfig {
        url: format!("http://{}", addr),
        headers: HashMap::new(),
        timeout: Some(5000),
        request_id_format: Some(RequestIdFormat::Number),
        header_policy: None,
        auth: None,
        oauth: None,
    });

    let conn = connect_to_server("json_reset_srv", config).await.expect("connect");
    let err = conn.call_tool("mutating_action", json!({})).await.unwrap_err();
    assert!(
        matches!(err, McpError::AcceptedStreamError(_)),
        "Expected AcceptedStreamError on body truncated after 200 headers, got: {:?}",
        err
    );
    // Assert call was only attempted ONCE and NOT replayed!
    assert_eq!(call_count.load(Ordering::SeqCst), 1, "Mutating call must NOT be replayed after 200 JSON headers");
}

#[tokio::test]
async fn test_server_reply_with_auth_session_protocol_and_numeric_id() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let ping_verified = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ping_verified_clone = ping_verified.clone();

    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let p_ver = ping_verified_clone.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap();
                let req_str = String::from_utf8_lossy(&buf[..n]);

                let body_start = req_str.find("\r\n\r\n").map(|i| i + 4).unwrap_or(n);
                let body_str = &req_str[body_start..];
                let json_body: Value = serde_json::from_str(body_str).unwrap_or(Value::Null);
                let method = json_body.get("method").and_then(|v| v.as_str()).unwrap_or("");

                if method == "initialize" {
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {
                            "protocolVersion": "2025-11-25",
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": "mock-reply-auth", "version": "1.0.0" }
                        }
                    });
                    let resp_bytes = serde_json::to_vec(&resp).unwrap();
                    let http_resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: sess-reply-99\r\nContent-Length: {}\r\n\r\n",
                        resp_bytes.len()
                    );
                    socket.write_all(http_resp.as_bytes()).await.unwrap();
                    socket.write_all(&resp_bytes).await.unwrap();
                } else if method == "notifications/initialized" {
                    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await.unwrap();
                } else if method == "tools/list" {
                    // 1. Send server ping request down SSE stream with NUMERIC ID 12345
                    let sse_header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n";
                    socket.write_all(sse_header.as_bytes()).await.unwrap();

                    let ping_req = json!({
                        "jsonrpc": "2.0",
                        "id": 12345,
                        "method": "ping"
                    });
                    let sse_msg = format!("data: {}\n\n", serde_json::to_string(&ping_req).unwrap());
                    socket.write_all(sse_msg.as_bytes()).await.unwrap();

                    // Wait until client replies to ping before completing tools/list
                    for _ in 0..50 {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        if p_ver.load(Ordering::SeqCst) {
                            break;
                        }
                    }

                    if p_ver.load(Ordering::SeqCst) {
                        let tools_resp = json!({
                            "jsonrpc": "2.0",
                            "id": 2,
                            "result": { "tools": [{ "name": "verified_tool", "inputSchema": {} }] }
                        });
                        let sse_tools = format!("data: {}\n\n", serde_json::to_string(&tools_resp).unwrap());
                        socket.write_all(sse_tools.as_bytes()).await.unwrap();
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                } else if json_body.get("result").is_some() {
                    // Server reply received! Check all 4 requirements:
                    let has_auth = req_str.contains("authorization: Bearer secret-token")
                        || req_str.contains("Authorization: Bearer secret-token");
                    let has_session = req_str.contains("mcp-session-id: sess-reply-99")
                        || req_str.contains("Mcp-Session-Id: sess-reply-99");
                    let has_protocol = req_str.contains("mcp-protocol-version: 2025-11-25")
                        || req_str.contains("MCP-Protocol-Version: 2025-11-25");
                    let exact_numeric_id = json_body.get("id") == Some(&json!(12345));

                    if has_auth && has_session && has_protocol && exact_numeric_id {
                        p_ver.store(true, Ordering::SeqCst);
                        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await.unwrap();
                    } else {
                        socket.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n").await.unwrap();
                    }
                }
            });
        }
    });

    let mut headers = HashMap::new();
    headers.insert("Authorization".to_string(), "Bearer secret-token".to_string());

    let config = MCPServerConfig::Http(MCPHttpServerConfig {
        url: format!("http://{}", addr),
        headers,
        timeout: Some(5000),
        request_id_format: Some(RequestIdFormat::Number),
        header_policy: Some("origin-locked".to_string()),
        auth: None,
        oauth: None,
    });

    let conn = connect_to_server("reply_auth_srv", config).await.expect("connect");
    let tools = conn.list_tools().await.expect("tools list must succeed after ping reply");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "verified_tool");
    assert!(ping_verified.load(Ordering::SeqCst), "Ping reply must have satisfied all auth/session/protocol/id checks");
}

#[tokio::test]
async fn test_legacy_sse_server_reply_with_auth_and_numeric_id() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let ping_verified = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ping_verified_clone = ping_verified.clone();
    let tool_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let tool_called_clone = tool_called.clone();

    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let p_ver = ping_verified_clone.clone();
            let t_called = tool_called_clone.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap();
                let req_str = String::from_utf8_lossy(&buf[..n]);

                if req_str.starts_with("GET /sse") {
                    let sse_header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n";
                    socket.write_all(sse_header.as_bytes()).await.unwrap();
                    let endpoint_event = "event: endpoint\r\ndata: /legacy_msg\r\n\r\n";
                    socket.write_all(endpoint_event.as_bytes()).await.unwrap();

                    // Wait for initialize POST
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    let init_resp = json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {
                            "protocolVersion": "2025-11-25",
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": "mock-legacy-auth", "version": "1.0.0" }
                        }
                    });
                    let msg = format!("event: message\r\ndata: {}\r\n\r\n", serde_json::to_string(&init_resp).unwrap());
                    socket.write_all(msg.as_bytes()).await.unwrap();

                    // Wait until tool is called
                    for _ in 0..100 {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        if t_called.load(Ordering::SeqCst) {
                            break;
                        }
                    }

                    // Send ping request with numeric ID 777
                    let ping_req = json!({
                        "jsonrpc": "2.0",
                        "id": 777,
                        "method": "ping"
                    });
                    let msg_ping = format!("event: message\r\ndata: {}\r\n\r\n", serde_json::to_string(&ping_req).unwrap());
                    socket.write_all(msg_ping.as_bytes()).await.unwrap();

                    // Wait for ping reply
                    for _ in 0..100 {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        if p_ver.load(Ordering::SeqCst) {
                            break;
                        }
                    }

                    if p_ver.load(Ordering::SeqCst) {
                        let tool_resp = json!({
                            "jsonrpc": "2.0",
                            "id": 2,
                            "result": {
                                "content": [{ "type": "text", "text": "legacy authenticated call success" }],
                                "isError": false
                            }
                        });
                        let msg_tool = format!("event: message\r\ndata: {}\r\n\r\n", serde_json::to_string(&tool_resp).unwrap());
                        socket.write_all(msg_tool.as_bytes()).await.unwrap();
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                } else if req_str.starts_with("POST /legacy_msg") {
                    let body_start = req_str.find("\r\n\r\n").map(|i| i + 4).unwrap_or(n);
                    let body_str = &req_str[body_start..];
                    let json_body: Value = serde_json::from_str(body_str).unwrap_or(Value::Null);

                    if let Some(method) = json_body.get("method").and_then(|v| v.as_str()) {
                        if method == "tools/call" {
                            t_called.store(true, Ordering::SeqCst);
                        }
                    }

                    if json_body.get("result").is_some() {
                        let has_auth = req_str.contains("authorization: Bearer legacy-secret")
                            || req_str.contains("Authorization: Bearer legacy-secret");
                        let exact_numeric_id = json_body.get("id") == Some(&json!(777));

                        if has_auth && exact_numeric_id {
                            p_ver.store(true, Ordering::SeqCst);
                        }
                    }
                    socket.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n").await.unwrap();
                }
            });
        }
    });

    let mut headers = HashMap::new();
    headers.insert("Authorization".to_string(), "Bearer legacy-secret".to_string());

    let config = MCPServerConfig::Sse(MCPSseServerConfig {
        url: format!("http://{}/sse", addr),
        headers,
        timeout: Some(5000),
        request_id_format: Some(RequestIdFormat::Number),
        header_policy: Some("origin-locked".to_string()),
        auth: None,
        oauth: None,
    });

    let conn = connect_to_server("legacy_auth_srv", config).await.expect("connect");
    let res = conn.call_tool("tool_x", json!({})).await.expect("call_tool success");
    assert_eq!(res.is_error, Some(false));
    assert!(ping_verified.load(Ordering::SeqCst), "Legacy ping reply must have included auth and exact numeric ID");
}

#[tokio::test]
async fn test_utf8_multibyte_split_across_chunks() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    let n = match socket.read(&mut tmp).await {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(_) => break,
                    };
                    buf.extend_from_slice(&tmp[..n]);

                    let mut handled_any = false;
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
                        let json_body: Value = serde_json::from_slice(body_bytes).unwrap_or(Value::Null);
                        let method = json_body.get("method").and_then(|v| v.as_str()).unwrap_or("");

                        buf.drain(..body_start + content_len);
                        handled_any = true;

                        if method == "initialize" {
                            let resp = json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "result": {
                                    "protocolVersion": "2025-11-25",
                                    "capabilities": { "tools": {} },
                                    "serverInfo": { "name": "mock-utf8", "version": "1.0.0" }
                                }
                            });
                            let resp_bytes = serde_json::to_vec(&resp).unwrap();
                            let http_resp = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                                 resp_bytes.len()
                            );
                            if socket.write_all(http_resp.as_bytes()).await.is_err() { return; }
                            if socket.write_all(&resp_bytes).await.is_err() { return; }
                        } else if method == "notifications/initialized" {
                            if socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await.is_err() { return; }
                        } else if method == "tools/call" {
                            let http_resp = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n";
                            if socket.write_all(http_resp.as_bytes()).await.is_err() { return; }

                            let full_resp = json!({
                                "jsonrpc": "2.0",
                                "id": 2,
                                "result": {
                                    "content": [{ "type": "text", "text": "café 🚀 тест" }],
                                    "isError": false
                                }
                            });
                            let sse_line = format!("data: {}\n\n", serde_json::to_string(&full_resp).unwrap());
                            let bytes = sse_line.as_bytes();

                            // Deliberately split chunk in the middle of multi-byte character 'é' (\xc3\xa9)
                            let split_pos = sse_line.find("caf").unwrap() + 4; // 'f' + 1 byte of 'é' (\xc3)
                            if socket.write_all(&bytes[..split_pos]).await.is_err() { return; }
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            if socket.write_all(&bytes[split_pos..]).await.is_err() { return; }
                            return;
                        }
                    }
                    if !handled_any && buf.len() > 65536 {
                        break;
                    }
                }
            });
        }
    });

    let config = MCPServerConfig::Http(MCPHttpServerConfig {
        url: format!("http://{}", addr),
        headers: HashMap::new(),
        timeout: Some(5000),
        request_id_format: Some(RequestIdFormat::Number),
        header_policy: None,
        auth: None,
        oauth: None,
    });

    let conn = connect_to_server("utf8_srv", config).await.expect("connect");
    let res = conn.call_tool("utf8_tool", json!({})).await.expect("call tool");
    assert_eq!(res.is_error, Some(false));
    match &res.content[0] {
        MCPContent::Text { text } => {
            assert_eq!(text, "café 🚀 тест", "UTF-8 must not contain replacement characters");
            assert!(!text.contains('\u{FFFD}'), "No replacement characters allowed in decoded UTF-8");
        }
        _ => panic!("Expected text"),
    }
}

#[tokio::test]
async fn test_notify_timeout_zero_disables_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap();
                let req_str = String::from_utf8_lossy(&buf[..n]);

                let body_start = req_str.find("\r\n\r\n").map(|i| i + 4).unwrap_or(n);
                let body_str = &req_str[body_start..];
                let json_body: Value = serde_json::from_str(body_str).unwrap_or(Value::Null);
                let method = json_body.get("method").and_then(|v| v.as_str()).unwrap_or("");

                if method == "initialize" {
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {
                            "protocolVersion": "2025-11-25",
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": "mock-slow-notif", "version": "1.0.0" }
                        }
                    });
                    let resp_bytes = serde_json::to_vec(&resp).unwrap();
                    let http_resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                        resp_bytes.len()
                    );
                    socket.write_all(http_resp.as_bytes()).await.unwrap();
                    socket.write_all(&resp_bytes).await.unwrap();
                } else if method == "notifications/initialized" {
                    // Delay response by 300ms
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await.unwrap();
                }
            });
        }
    });

    // Timeout: 0 explicitly disables timeout
    let config = MCPServerConfig::Http(MCPHttpServerConfig {
        url: format!("http://{}", addr),
        headers: HashMap::new(),
        timeout: Some(0),
        request_id_format: Some(RequestIdFormat::Number),
        header_policy: None,
        auth: None,
        oauth: None,
    });

    let res = connect_to_server("slow_notif_srv", config).await;
    assert!(res.is_ok(), "Must succeed when timeout=0 disables client deadline");
}

#[tokio::test]
async fn test_legacy_sse_connect_hanging_endpoint_times_out() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap();
                let req_str = String::from_utf8_lossy(&buf[..n]);

                if req_str.starts_with("GET /sse") {
                    // Server sends 200 SSE headers, but NEVER sends the endpoint event!
                    let sse_header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n";
                    socket.write_all(sse_header.as_bytes()).await.unwrap();
                    // Keep connection open without writing endpoint event
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            });
        }
    });

    let config = MCPServerConfig::Sse(MCPSseServerConfig {
        url: format!("http://{}/sse", addr),
        headers: HashMap::new(),
        timeout: Some(200), // 200ms timeout
        request_id_format: Some(RequestIdFormat::Number),
        header_policy: None,
        auth: None,
        oauth: None,
    });

    let err = connect_to_server("hanging_sse_srv", config).await.unwrap_err();
    assert!(
        matches!(err, McpError::Timeout(_)),
        "Expected Timeout when legacy SSE server hangs without endpoint event, got: {:?}",
        err
    );
}

#[tokio::test]
async fn test_http_multiline_sse_data_joined_correctly() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap();
                let req_str = String::from_utf8_lossy(&buf[..n]);

                let body_start = req_str.find("\r\n\r\n").map(|i| i + 4).unwrap_or(n);
                let body_str = &req_str[body_start..];
                let json_body: Value = serde_json::from_str(body_str).unwrap_or(Value::Null);
                let method = json_body.get("method").and_then(|v| v.as_str()).unwrap_or("");

                if method == "initialize" {
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {
                            "protocolVersion": "2025-11-25",
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": "mock-multiline", "version": "1.0.0" }
                        }
                    });
                    let resp_bytes = serde_json::to_vec(&resp).unwrap();
                    let http_resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                        resp_bytes.len()
                    );
                    socket.write_all(http_resp.as_bytes()).await.unwrap();
                    socket.write_all(&resp_bytes).await.unwrap();
                } else if method == "notifications/initialized" {
                    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await.unwrap();
                } else if method == "tools/call" {
                    let http_resp = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n";
                    socket.write_all(http_resp.as_bytes()).await.unwrap();

                    // Stream response split across multiple `data:` lines before \n\n
                    let sse_multiline = concat!(
                        "data: {\"jsonrpc\": \"2.0\",\n",
                        "data: \"id\": 2,\n",
                        "data: \"result\": {\"content\": [{\"type\": \"text\", \"text\": \"multiline success\"}], \"isError\": false}}\n\n"
                    );
                    socket.write_all(sse_multiline.as_bytes()).await.unwrap();
                }
            });
        }
    });

    let config = MCPServerConfig::Http(MCPHttpServerConfig {
        url: format!("http://{}", addr),
        headers: HashMap::new(),
        timeout: Some(5000),
        request_id_format: Some(RequestIdFormat::Number),
        header_policy: None,
        auth: None,
        oauth: None,
    });

    let conn = connect_to_server("multiline_srv", config).await.expect("connect");
    let res = conn.call_tool("multiline_tool", json!({})).await.expect("call_tool");
    assert_eq!(res.is_error, Some(false));
    match &res.content[0] {
        MCPContent::Text { text } => assert_eq!(text, "multiline success"),
        _ => panic!("Expected text content"),
    }
}

#[tokio::test]
async fn test_untrusted_frame_exceeding_bound_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let _ = socket.read(&mut buf).await.unwrap();

                // Send 500 status with an overlong 17 MiB body
                let header = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 17825792\r\n\r\n";
                socket.write_all(header.as_bytes()).await.unwrap();
                let chunk = vec![b'A'; 65536];
                for _ in 0..(17 * 1024 * 1024 / 65536 + 1) {
                    if socket.write_all(&chunk).await.is_err() {
                        break;
                    }
                }
            });
        }
    });

    let config = MCPServerConfig::Http(MCPHttpServerConfig {
        url: format!("http://{}", addr),
        headers: HashMap::new(),
        timeout: Some(5000),
        request_id_format: Some(RequestIdFormat::Number),
        header_policy: None,
        auth: None,
        oauth: None,
    });

    let err = connect_to_server("oversize_srv", config).await.unwrap_err();
    assert!(
        matches!(err, McpError::Transport(_) | McpError::Http { .. }),
        "Expected bounded rejection of oversize HTTP body, got: {:?}",
        err
    );
}

#[tokio::test]
async fn test_legacy_sse_drop_connection_aborts_reader_and_closes_socket() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let eof_observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let eof_observed_clone = eof_observed.clone();

    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let eof_clone = eof_observed_clone.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap();
                let req_str = String::from_utf8_lossy(&buf[..n]);

                if req_str.starts_with("GET /sse") {
                    let sse_header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n";
                    socket.write_all(sse_header.as_bytes()).await.unwrap();
                    let endpoint_event = "event: endpoint\r\ndata: /drop_msg\r\n\r\n";
                    socket.write_all(endpoint_event.as_bytes()).await.unwrap();

                    // Wait for initialize POST
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    let init_resp = json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {
                            "protocolVersion": "2025-11-25",
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": "mock-drop-sse", "version": "1.0.0" }
                        }
                    });
                    let msg = format!("event: message\r\ndata: {}\r\n\r\n", serde_json::to_string(&init_resp).unwrap());
                    socket.write_all(msg.as_bytes()).await.unwrap();

                    // Hold stream open and await EOF on client drop
                    let mut check_buf = [0u8; 64];
                    match socket.read(&mut check_buf).await {
                        Ok(0) => {
                            eof_clone.store(true, Ordering::SeqCst);
                        }
                        _ => {}
                    }
                } else if req_str.starts_with("POST /drop_msg") {
                    socket.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n").await.unwrap();
                }
            });
        }
    });

    let config = MCPServerConfig::Sse(MCPSseServerConfig {
        url: format!("http://{}/sse", addr),
        headers: HashMap::new(),
        timeout: Some(5000),
        request_id_format: Some(RequestIdFormat::Number),
        header_policy: None,
        auth: None,
        oauth: None,
    });

    let conn = connect_to_server("drop_sse_srv", config).await.expect("connect");
    drop(conn);

    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if eof_observed.load(Ordering::SeqCst) {
            break;
        }
    }

    assert!(
        eof_observed.load(Ordering::SeqCst),
        "Server must observe EOF on open SSE stream promptly after McpConnection is dropped"
    );
}

#[tokio::test]
async fn test_stdio_no_newline_oversized_frame_rejected() {
    let script_path = std::env::temp_dir().join(format!("omp-mcp-stdio-huge-{}.py", std::process::id()));
    let py_code = r#"import sys, time
line = sys.stdin.readline()
# Reply to initialize
sys.stdout.write('{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{},"serverInfo":{"name":"huge","version":"1"}}}\n')
sys.stdout.flush()
# Read notifications/initialized
sys.stdin.readline()
# Now emit 17 MiB without any newline!
chunk = "A" * 65536
for _ in range(17 * 16 + 1):
    try:
        sys.stdout.write(chunk)
        sys.stdout.flush()
    except BrokenPipeError:
        break
"#;
    std::fs::write(&script_path, py_code).unwrap();

    let config = MCPServerConfig::Stdio(MCPStdioServerConfig {
        command: "python3".to_string(),
        args: vec![script_path.to_string_lossy().into_owned()],
        env: HashMap::new(),
        cwd: None,
        timeout: Some(5000),
        request_id_format: Some(RequestIdFormat::Number),
    });

    let conn = connect_to_server("stdio_huge", config).await.expect("connect");
    // Next call fails because child exceeded 16 MiB line bound without newline and reader closed
    let err = conn.call_tool("any", json!({})).await.unwrap_err();
    assert!(
        matches!(err, McpError::Closed | McpError::Transport(_)),
        "Expected connection close on oversize no-newline stdio stream, got: {:?}",
        err
    );

    let _ = std::fs::remove_file(script_path);
}

#[tokio::test]
async fn test_sse_connect_many_short_data_lines_exceeding_bound() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let _ = socket.read(&mut buf).await.unwrap();

                let sse_header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n";
                socket.write_all(sse_header.as_bytes()).await.unwrap();

                // Send many short data lines until exceeding 16 MiB
                let short_line = "data: short chunk of data\n";
                for _ in 0..(17 * 1024 * 1024 / short_line.len() + 1) {
                    if socket.write_all(short_line.as_bytes()).await.is_err() {
                        break;
                    }
                }
            });
        }
    });

    let config = MCPServerConfig::Sse(MCPSseServerConfig {
        url: format!("http://{}/sse", addr),
        headers: HashMap::new(),
        timeout: Some(5000),
        request_id_format: Some(RequestIdFormat::Number),
        header_policy: None,
        auth: None,
        oauth: None,
    });

    let err = connect_to_server("many_short_lines", config).await.unwrap_err();
    assert!(
        matches!(err, McpError::Transport(_)),
        "Expected Transport error on multi-line data exceeding 16 MiB bound, got: {:?}",
        err
    );
}

/// Deterministic stdio MCP server exposing resources. `mode` selects:
/// `full` (resources + templates), `no-templates` (resources capability but
/// `resources/templates/list` answers -32601), `no-resources-cap`.
fn write_resource_fixture(script_path: &std::path::Path) {
    std::fs::write(
        script_path,
        r#"
import sys, json

mode = sys.argv[1] if len(sys.argv) > 1 else "full"
capabilities = {"tools": {}} if mode == "no-resources-cap" else {"resources": {"subscribe": False}}

PAGE1_URI = "test://opaque/a:b?x=1&y=%2F"
PAGE2_URI = "mem://notes/second"

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    params = msg.get("params") or {}

    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "protocolVersion": "2025-11-25",
            "capabilities": capabilities,
            "serverInfo": {"name": "mock-resources", "version": "1.0.0"}
        }})
    elif method == "notifications/initialized":
        pass
    elif method == "resources/list":
        if mode == "no-resources-cap":
            send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "Method not found"}})
        elif params.get("cursor") == "res-page-2":
            send({"jsonrpc": "2.0", "id": mid, "result": {
                "resources": [{"uri": PAGE2_URI, "name": "second"}],
                "nextCursor": None
            }})
        else:
            send({"jsonrpc": "2.0", "id": mid, "result": {
                "resources": [{
                    "uri": PAGE1_URI,
                    "name": "opaque",
                    "title": "Opaque resource",
                    "description": "colon and query preserved",
                    "mimeType": "text/plain",
                    "size": 42,
                    "annotations": {
                        "audience": ["user"],
                        "priority": 0.5,
                        "lastModified": "2026-01-02T03:04:05Z"
                    }
                }],
                "nextCursor": "res-page-2"
            }})
    elif method == "resources/templates/list":
        if mode == "no-templates":
            send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "Method not found"}})
        else:
            send({"jsonrpc": "2.0", "id": mid, "result": {
                "resourceTemplates": [{
                    "uriTemplate": "test://opaque/{id}{?x}",
                    "name": "tmpl",
                    "title": "Template",
                    "description": "parameterized",
                    "mimeType": "text/plain",
                    "annotations": {"audience": ["assistant"], "priority": 0.25}
                }],
                "nextCursor": None
            }})
    elif method == "resources/read":
        uri = params.get("uri")
        if uri is None:
            send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32602, "message": "missing uri"}})
        elif uri.startswith("missing:"):
            send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32002, "message": "Resource not found: " + uri}})
        else:
            send({"jsonrpc": "2.0", "id": mid, "result": {
                "contents": [
                    {"uri": uri, "mimeType": "text/plain", "text": "echo:" + uri},
                    {"uri": uri, "mimeType": "image/png", "blob": "AAECAwQ="}
                ]
            }})
    else:
        send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "Method not found: " + str(method)}})
"#,
    )
    .unwrap();
}

async fn resource_fixture_conn(mode: &str) -> (McpConnection, std::path::PathBuf) {
    let script_path = std::env::temp_dir().join(format!(
        "mock_mcp_resources_{}_{}.py",
        mode,
        std::process::id()
    ));
    write_resource_fixture(&script_path);

    let config = MCPServerConfig::Stdio(MCPStdioServerConfig {
        command: "python3".to_string(),
        args: vec![script_path.to_str().unwrap().to_string(), mode.to_string()],
        env: HashMap::new(),
        cwd: None,
        timeout: Some(10000),
        request_id_format: Some(RequestIdFormat::Number),
    });

    let conn = connect_to_server("test_resources", config)
        .await
        .expect("Failed to connect resources fixture");
    (conn, script_path)
}

#[tokio::test]
async fn test_stdio_resources_list_templates_read() {
    let (conn, script_path) = resource_fixture_conn("full").await;

    let resources = conn.list_resources().await.expect("list_resources");
    assert_eq!(resources.len(), 2, "both pages must be fetched");

    assert_eq!(resources[0].uri, "test://opaque/a:b?x=1&y=%2F");
    assert_eq!(resources[0].name, "opaque");
    assert_eq!(resources[0].title.as_deref(), Some("Opaque resource"));
    assert_eq!(resources[0].description.as_deref(), Some("colon and query preserved"));
    assert_eq!(resources[0].mime_type.as_deref(), Some("text/plain"));
    assert_eq!(resources[0].size, Some(42));
    let annotations = resources[0].annotations.as_ref().expect("annotations");
    assert_eq!(
        annotations.audience.as_deref(),
        Some(&[MCPAnnotationAudience::User][..])
    );
    assert_eq!(annotations.priority, Some(0.5));
    assert_eq!(annotations.last_modified.as_deref(), Some("2026-01-02T03:04:05Z"));

    assert_eq!(resources[1].uri, "mem://notes/second");
    assert_eq!(resources[1].name, "second");
    assert!(resources[1].size.is_none());
    assert!(resources[1].annotations.is_none());

    let templates = conn.list_resource_templates().await.expect("list templates");
    assert_eq!(templates.len(), 1);
    assert_eq!(templates[0].uri_template, "test://opaque/{id}{?x}");
    assert_eq!(templates[0].name, "tmpl");
    assert_eq!(templates[0].title.as_deref(), Some("Template"));
    assert_eq!(templates[0].description.as_deref(), Some("parameterized"));
    assert_eq!(templates[0].mime_type.as_deref(), Some("text/plain"));
    let t_annotations = templates[0].annotations.as_ref().expect("template annotations");
    assert_eq!(
        t_annotations.audience.as_deref(),
        Some(&[MCPAnnotationAudience::Assistant][..])
    );
    assert_eq!(t_annotations.priority, Some(0.25));

    // URI is opaque: the fixture echoes the request URI byte-for-byte.
    let opaque = "test://opaque/a:b?x=1&y=%2F";
    let read = conn.read_resource(opaque).await.expect("read_resource");
    assert_eq!(read.contents.len(), 2);
    assert_eq!(read.contents[0].uri, opaque);
    assert_eq!(read.contents[0].mime_type.as_deref(), Some("text/plain"));
    assert_eq!(read.contents[0].text.as_deref(), Some(&*format!("echo:{}", opaque)));
    assert!(read.contents[0].blob.is_none());
    assert_eq!(read.contents[1].uri, opaque);
    assert_eq!(read.contents[1].mime_type.as_deref(), Some("image/png"));
    assert_eq!(read.contents[1].blob.as_deref(), Some("AAECAwQ="));
    assert!(read.contents[1].text.is_none());

    // Read failures surface as JSON-RPC errors; no synthetic fallback.
    let err = conn
        .read_resource("missing:server-side")
        .await
        .expect_err("missing resource must fail");
    match err {
        McpError::JsonRpc { code, message, .. } => {
            assert_eq!(code, -32002);
            assert_eq!(message, "Resource not found: missing:server-side");
        }
        other => panic!("expected JsonRpc error, got {other:?}"),
    }

    conn.close().await.expect("close");
    let _ = std::fs::remove_file(script_path);
}

#[tokio::test]
async fn test_stdio_resources_missing_templates_method_returns_empty() {
    let (conn, script_path) = resource_fixture_conn("no-templates").await;

    // -32601 on resources/templates/list must not discard concrete resources.
    let resources = conn.list_resources().await.expect("list_resources");
    assert_eq!(resources.len(), 2);
    assert_eq!(resources[0].uri, "test://opaque/a:b?x=1&y=%2F");

    let templates = conn
        .list_resource_templates()
        .await
        .expect("missing method is treated as no templates");
    assert!(templates.is_empty());

    conn.close().await.expect("close");
    let _ = std::fs::remove_file(script_path);
}

#[tokio::test]
async fn test_stdio_resources_requires_capability() {
    let (conn, script_path) = resource_fixture_conn("no-resources-cap").await;

    // The fixture answers resources/list with -32601; Ok(vec![]) proves no
    // request was sent because the capability is absent.
    assert!(conn.capabilities.resources.is_none());
    assert!(conn.list_resources().await.expect("gated list").is_empty());
    assert!(conn
        .list_resource_templates()
        .await
        .expect("gated templates")
        .is_empty());

    conn.close().await.expect("close");
    let _ = std::fs::remove_file(script_path);
}
