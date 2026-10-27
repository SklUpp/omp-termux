//! Behavioral tests for MCP discovery priority, schema validation,
//! collision warnings, disabled servers/tools, and tool execution.

use std::{
	fs,
	path::{Path, PathBuf},
	sync::Arc,
	time::{SystemTime, UNIX_EPOCH},
};

use omp_cli::{
	approve::{ApprovalMode, Approvals},
	config::{
		Config, discover_mcp_servers_with_paths, expand_env_vars, is_valid_mcp_server_name,
		parse_and_validate_mcp_server, parse_enabled_value,
	},
	runtime::{McpTool, deduplicate_mcp_tools_with_warnings, load_and_filter_mcp_tools_with_paths},
};
use pi_agent::{
	Tool,
	mcp::{MCPServerConfig, MCPStdioServerConfig, McpToolOriginSource, connect_to_server},
};
use serde_json::json;

struct TempDir(PathBuf);

impl TempDir {
	fn new(name: &str) -> Self {
		let nanos = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.unwrap()
			.as_nanos();
		let path = std::env::temp_dir().join(format!("omp-test-{name}-{nanos}"));
		fs::create_dir_all(&path).expect("create temp dir");
		Self(path)
	}

	fn path(&self) -> &Path {
		&self.0
	}
}

impl Drop for TempDir {
	fn drop(&mut self) {
		let _ = fs::remove_dir_all(&self.0);
	}
}
#[test]
fn test_mcp_server_name_validation() {
	assert!(is_valid_mcp_server_name("my-server"));
	assert!(is_valid_mcp_server_name("server_123.test"));
	assert!(!is_valid_mcp_server_name(""));
	assert!(!is_valid_mcp_server_name("server with space"));
	assert!(!is_valid_mcp_server_name("server@invalid"));
	assert!(!is_valid_mcp_server_name(&"a".repeat(101)));
	assert!(is_valid_mcp_server_name(&"a".repeat(100)));
}

#[test]
fn test_mcp_schema_validation_infer_and_reject() {
	// 1. Infer stdio when command is present without type
	let typeless_stdio = json!({
		"command": "node",
		"args": ["server.js"]
	});
	let config = parse_and_validate_mcp_server("demo", &typeless_stdio).expect("infer stdio");
	assert!(matches!(config, MCPServerConfig::Stdio(_)));

	// 2. Infer http when url is present without type
	let typeless_http = json!({
		"url": "http://127.0.0.1:8080/mcp"
	});
	let config = parse_and_validate_mcp_server("demo", &typeless_http).expect("infer http");
	assert!(matches!(config, MCPServerConfig::Http(_)));

	// 3. Reject when both command and url are set
	let both = json!({
		"command": "node",
		"url": "http://127.0.0.1:8080/mcp"
	});
	assert!(parse_and_validate_mcp_server("demo", &both).is_err());

	// 4. Reject when neither command nor url is set
	let neither = json!({
		"timeout": 5000
	});
	assert!(parse_and_validate_mcp_server("demo", &neither).is_err());

	// 5. Reject empty command
	let empty_cmd = json!({
		"type": "stdio",
		"command": "   "
	});
	assert!(parse_and_validate_mcp_server("demo", &empty_cmd).is_err());

	// 6. Reject negative timeout
	let neg_timeout = json!({
		"command": "node",
		"timeout": -10
	});
	assert!(parse_and_validate_mcp_server("demo", &neg_timeout).is_err());

	// 7. Reject invalid requestIdFormat
	let bad_req_id = json!({
		"command": "node",
		"requestIdFormat": "uuid"
	});
	assert!(parse_and_validate_mcp_server("demo", &bad_req_id).is_err());

	// 8. Coerce string numeric timeout
	let str_timeout = json!({
		"command": "node",
		"timeout": "3000"
	});
	let cfg = parse_and_validate_mcp_server("demo", &str_timeout).expect("str timeout");
	match cfg {
		MCPServerConfig::Stdio(s) => assert_eq!(s.timeout, Some(3000)),
		_ => panic!("expected stdio"),
	}

	// 9. Reject non-finite timeouts like "NaN", "inf", "-inf"
	for invalid_timeout in ["NaN", "inf", "-inf", "infinity"] {
		let non_finite = json!({
			"command": "node",
			"timeout": invalid_timeout
		});
		assert!(
			parse_and_validate_mcp_server("demo", &non_finite).is_err(),
			"must reject timeout: {invalid_timeout}"
		);
	}
}

#[test]
fn test_env_placeholder_expansion() {
	let path_val = std::env::var("PATH").unwrap_or_default();
	assert_eq!(expand_env_vars("${PATH}"), path_val);
	assert_eq!(expand_env_vars("prefix:${PATH}:suffix"), format!("prefix:{path_val}:suffix"));
	assert_eq!(expand_env_vars("${OMP_UNSET:-fallback_val}"), "fallback_val");
	assert_eq!(expand_env_vars("${OMP_UNSET_NO_DEFAULT}"), "${OMP_UNSET_NO_DEFAULT}");

	// Test expansion in parse_and_validate_mcp_server
	let http_json = json!({
		"type": "http",
		"url": "http://${OMP_HOST:-127.0.0.1}:8080/mcp",
		"headers": {
			"Authorization": "Bearer ${OMP_TOKEN:-token-from-default}",
			"X-Custom": "${OMP_CUSTOM_HEADER:-default-header}"
		}
	});
	let cfg = parse_and_validate_mcp_server("test_http", &http_json).expect("parse http");
	match cfg {
		MCPServerConfig::Http(h) => {
			assert_eq!(h.url, "http://127.0.0.1:8080/mcp");
			assert_eq!(
				h.headers.get("Authorization").map(|s| s.as_str()),
				Some("Bearer token-from-default")
			);
			assert_eq!(h.headers.get("X-Custom").map(|s| s.as_str()), Some("default-header"));
		},
		_ => panic!("expected http config"),
	}
}

#[test]
fn test_discovery_priority_and_shadowing() {
	let temp_dir = TempDir::new("discovery");
	let base = temp_dir.path();
	let cwd = base.join("project");
	let agent_dir = base.join("agent");

	let omp_dir = cwd.join(".omp");
	fs::create_dir_all(&omp_dir).expect("create omp_dir");
	fs::create_dir_all(&agent_dir).expect("create agent_dir");

	// 1. .omp/mcp.json has s1 (echo-omp) and shared (shared-omp)
	fs::write(
		omp_dir.join("mcp.json"),
		json!({
			"mcpServers": {
				"s1": { "command": "echo-omp" },
				"shared": { "command": "shared-omp" }
			}
		})
		.to_string(),
	)
	.expect("write .omp/mcp.json");

	// 2. .omp/.mcp.json has dot_omp (echo-dot-omp)
	fs::write(
		omp_dir.join(".mcp.json"),
		json!({
			"mcpServers": {
				"dot_omp": { "command": "echo-dot-omp" }
			}
		})
		.to_string(),
	)
	.expect("write .omp/.mcp.json");

	// 3. agent_dir/mcp.json has s2 (echo-agent), shared (shared-agent), and
	//    agent_vs_root
	fs::write(
		agent_dir.join("mcp.json"),
		json!({
			"mcpServers": {
				"s2": { "command": "echo-agent" },
				"shared": { "command": "shared-agent" },
				"agent_vs_root": { "command": "from-agent" }
			}
		})
		.to_string(),
	)
	.expect("write agent/mcp.json");

	// 4. agent_dir/.mcp.json has dot_agent (echo-dot-agent)
	fs::write(
		agent_dir.join(".mcp.json"),
		json!({
			"mcpServers": {
				"dot_agent": { "command": "echo-dot-agent" }
			}
		})
		.to_string(),
	)
	.expect("write agent/.mcp.json");

	// 5. cwd/mcp.json has s3 (echo-root), shared (shared-root), and agent_vs_root
	fs::write(
		cwd.join("mcp.json"),
		json!({
			"mcpServers": {
				"s3": { "command": "echo-root" },
				"shared": { "command": "shared-root" },
				"agent_vs_root": { "command": "from-root" }
			}
		})
		.to_string(),
	)
	.expect("write cwd/mcp.json");

	// 6. cwd/.mcp.json has dot_root (echo-dot-root)
	fs::write(
		cwd.join(".mcp.json"),
		json!({
			"mcpServers": {
				"dot_root": { "command": "echo-dot-root" }
			}
		})
		.to_string(),
	)
	.expect("write cwd/.mcp.json");

	let report = discover_mcp_servers_with_paths(&cwd, &agent_dir);
	assert!(report.warnings.is_empty(), "warnings: {:?}", report.warnings);

	let names: Vec<&str> = report.servers.iter().map(|s| s.name.as_str()).collect();
	assert!(names.contains(&"s1"));
	assert!(names.contains(&"dot_omp"));
	assert!(names.contains(&"s2"));
	assert!(names.contains(&"dot_agent"));
	assert!(names.contains(&"s3"));
	assert!(names.contains(&"dot_root"));
	assert!(names.contains(&"shared"));
	assert!(names.contains(&"agent_vs_root"));

	// Check that .omp won first-wins for "shared"
	let shared = report
		.servers
		.iter()
		.find(|s| s.name == "shared")
		.expect("shared");
	match &shared.config {
		MCPServerConfig::Stdio(s) => assert_eq!(s.command, "shared-omp"),
		_ => panic!("expected stdio"),
	}

	// Check that agent_dir won over cwd project root for "agent_vs_root"
	let avr = report
		.servers
		.iter()
		.find(|s| s.name == "agent_vs_root")
		.expect("agent_vs_root");
	match &avr.config {
		MCPServerConfig::Stdio(s) => assert_eq!(s.command, "from-agent"),
		_ => panic!("expected stdio"),
	}
}
#[test]
fn test_disabled_servers_shadowing_and_denylist() {
	let temp_dir = TempDir::new("disabled");
	let base = temp_dir.path();
	let cwd = base.join("project");
	let agent_dir = base.join("agent");

	let omp_dir = cwd.join(".omp");
	fs::create_dir_all(&omp_dir).expect("create omp_dir");
	fs::create_dir_all(&agent_dir).expect("create agent_dir");

	// Higher priority (.omp) defines "shadow_disabled" with enabled: false
	fs::write(
		omp_dir.join("mcp.json"),
		json!({
			"mcpServers": {
				"shadow_disabled": { "command": "cmd1", "enabled": false }
			}
		})
		.to_string(),
	)
	.expect("write .omp/mcp.json");

	// Lower priority (cwd) defines "shadow_disabled" with enabled: true
	fs::write(
		cwd.join("mcp.json"),
		json!({
			"mcpServers": {
				"shadow_disabled": { "command": "cmd2", "enabled": true },
				"active": { "command": "cmd3" }
			}
		})
		.to_string(),
	)
	.expect("write cwd/mcp.json");

	// Agent config has disabledServers denylist for "active"
	fs::write(
		agent_dir.join("mcp.json"),
		json!({
			"disabledServers": ["active"]
		})
		.to_string(),
	)
	.expect("write agent/mcp.json");

	// Trust boundary: user explicitly disabled user_disabled server
	fs::write(
		agent_dir.join(".mcp.json"),
		json!({
			"mcpServers": {
				"user_disabled": { "command": "agent_cmd", "enabled": false }
			}
		})
		.to_string(),
	)
	.expect("write agent/.mcp.json");

	// Project tries to force-enable user_disabled server via enabledServers
	fs::write(
		cwd.join(".mcp.json"),
		json!({
			"enabledServers": ["user_disabled"],
			"mcpServers": {
				"user_disabled": { "command": "root_cmd" }
			}
		})
		.to_string(),
	)
	.expect("write cwd/.mcp.json");

	let report = discover_mcp_servers_with_paths(&cwd, &agent_dir);
	let names: Vec<&str> = report.servers.iter().map(|s| s.name.as_str()).collect();

	// shadow_disabled was shadowed by .omp's disabled entry -> must NOT be in
	// active servers
	assert!(!names.contains(&"shadow_disabled"));
	// active was denylisted by disabledServers -> must NOT be in active servers
	assert!(!names.contains(&"active"));
	// user_disabled cannot be forced on by project enabledServers -> must NOT be in
	// active servers
	assert!(!names.contains(&"user_disabled"));
}

#[test]
fn test_enabled_normalization_string_values_and_env_expansion() {
	// 1. Direct unit test of parse_enabled_value
	assert_eq!(parse_enabled_value(None), (None, None));
	assert_eq!(parse_enabled_value(Some(&json!(null))), (None, None));
	assert_eq!(parse_enabled_value(Some(&json!(false))), (Some(false), None));
	assert_eq!(parse_enabled_value(Some(&json!(true))), (Some(true), None));
	assert_eq!(parse_enabled_value(Some(&json!("false"))), (Some(false), None));
	assert_eq!(parse_enabled_value(Some(&json!("0"))), (Some(false), None));
	assert_eq!(parse_enabled_value(Some(&json!("FALSE"))), (Some(false), None));
	assert_eq!(parse_enabled_value(Some(&json!("true"))), (Some(true), None));
	assert_eq!(parse_enabled_value(Some(&json!("1"))), (Some(true), None));
	assert_eq!(parse_enabled_value(Some(&json!("TRUE"))), (Some(true), None));
	let (inv_val, inv_warn) = parse_enabled_value(Some(&json!("not_a_bool")));
	assert_eq!(inv_val, None);
	assert!(inv_warn.unwrap().contains("invalid enabled value"));

	// 2. Integration test in discovery with string values and env placeholders
	let temp_dir = TempDir::new("enabled-norm");
	let cwd = temp_dir.path().join("proj");
	let agent_dir = temp_dir.path().join("agent");
	let omp_dir = cwd.join(".omp");
	fs::create_dir_all(&omp_dir).expect("create omp dir");
	fs::create_dir_all(&agent_dir).expect("create agent dir");

	fs::write(
		omp_dir.join("mcp.json"),
		json!({
			"mcpServers": {
				"srv_str_false": { "command": "echo", "enabled": "false" },
				"srv_str_zero": { "command": "echo", "enabled": "0" },
				"srv_env_false": { "command": "echo", "enabled": "${MCP_UNSET_DIS:-false}" },
				"srv_str_true": { "command": "echo", "enabled": "true" },
				"srv_str_one": { "command": "echo", "enabled": "1" },
				"srv_env_true": { "command": "echo", "enabled": "${MCP_UNSET_EN:-true}" },
				"srv_invalid": { "command": "echo", "enabled": "not_bool" }
			}
		})
		.to_string(),
	)
	.expect("write mcp.json");

	let report = discover_mcp_servers_with_paths(&cwd, &agent_dir);
	let names: Vec<&str> = report.servers.iter().map(|s| s.name.as_str()).collect();

	// Disabled servers must NOT be spawned
	assert!(!names.contains(&"srv_str_false"));
	assert!(!names.contains(&"srv_str_zero"));
	assert!(!names.contains(&"srv_env_false"));

	// Enabled and fallback-enabled servers MUST be spawned
	assert!(names.contains(&"srv_str_true"));
	assert!(names.contains(&"srv_str_one"));
	assert!(names.contains(&"srv_env_true"));
	assert!(names.contains(&"srv_invalid"));

	// Warning for invalid value must be recorded
	assert!(
		report
			.warnings
			.iter()
			.any(|w| w.contains("invalid enabled value \"not_bool\""))
	);
}

#[test]
fn test_schema_error_shadow_then_drop_and_malformed_json() {
	let temp_dir = TempDir::new("schema-err");
	let base = temp_dir.path();
	let cwd = base.join("project");
	let agent_dir = base.join("agent");

	let omp_dir = cwd.join(".omp");
	fs::create_dir_all(&omp_dir).expect("create omp_dir");
	fs::create_dir_all(&agent_dir).expect("create agent_dir");

	// Higher priority (.omp) defines invalid server (command and url both set)
	fs::write(
		omp_dir.join("mcp.json"),
		json!({
			"mcpServers": {
				"invalid_high": { "command": "cmd1", "url": "http://example.com" }
			}
		})
		.to_string(),
	)
	.expect("write .omp/mcp.json");

	// Lower priority (cwd) defines valid "invalid_high"
	fs::write(
		cwd.join("mcp.json"),
		json!({
			"mcpServers": {
				"invalid_high": { "command": "cmd2" }
			}
		})
		.to_string(),
	)
	.expect("write cwd/mcp.json");

	// agent_dir has malformed json
	fs::write(agent_dir.join("mcp.json"), "{ broken json").expect("write broken");

	let report = discover_mcp_servers_with_paths(&cwd, &agent_dir);
	let names: Vec<&str> = report.servers.iter().map(|s| s.name.as_str()).collect();

	// invalid_high was claimed by .omp, failed validation, so dropped with warning
	assert!(!names.contains(&"invalid_high"));
	assert!(
		report
			.warnings
			.iter()
			.any(|w| w.contains("cannot specify both"))
	);
	assert!(
		report
			.warnings
			.iter()
			.any(|w| w.contains("Failed to parse JSON"))
	);
}

fn spawn_mock_stdio_server(temp_dir: &Path) -> MCPServerConfig {
	let script_path = temp_dir.join("mock_mcp.py");
	fs::write(
		&script_path,
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
        resp = {
            "jsonrpc": "2.0",
            "id": mid,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "mock-mcp", "version": "1.0.0" }
            }
        }
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        resp = {
            "jsonrpc": "2.0",
            "id": mid,
            "result": {
                "tools": [
                    { "name": "echo", "description": "Echo test", "inputSchema": {} },
                    { "name": "image_tool", "description": "Image test", "inputSchema": {} },
                    { "name": "hang_tool", "description": "Hang test", "inputSchema": {} },
                    { "name": "fail_tool", "description": "Failing tool", "inputSchema": {} }
                ]
            }
        }
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif method == "tools/call":
        name = msg.get("params", {}).get("name")
        if name == "echo":
            resp = {
                "jsonrpc": "2.0",
                "id": mid,
                "result": {
                    "content": [{ "type": "text", "text": "hello from echo" }],
                    "isError": False
                }
            }
        elif name == "image_tool":
            resp = {
                "jsonrpc": "2.0",
                "id": mid,
                "result": {
                    "content": [{ "type": "image", "data": "aW1hZ2VkYXRh", "mimeType": "image/png" }],
                    "isError": False
                }
            }
        elif name == "hang_tool":
            import time
            time.sleep(10)
            resp = { "jsonrpc": "2.0", "id": mid, "result": { "content": [], "isError": False } }
        else:
            resp = {
                "jsonrpc": "2.0",
                "id": mid,
                "result": {
                    "content": [{ "type": "text", "text": "tool failed badly" }],
                    "isError": True
                }
            }
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
"#,
	)
	.expect("write mock script");

	MCPServerConfig::Stdio(MCPStdioServerConfig {
		command:           "python3".to_string(),
		args:              vec![script_path.to_str().unwrap().to_string()],
		env:               std::collections::HashMap::new(),
		cwd:               None,
		timeout:           Some(5000),
		request_id_format: None,
	})
}

#[test]
fn test_tool_collision_warning_and_stable_winner() {
	let temp_dir = TempDir::new("collision");
	let config = spawn_mock_stdio_server(temp_dir.path());

	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();
	let conn = rt.block_on(async {
		connect_to_server("dummy", config)
			.await
			.expect("connect to mock stdio")
	});
	let conn = Arc::new(conn);

	// Server "a-b" tool "c" -> mcp__a_b_c (origin key "a-b\0c")
	// Server "a" tool "b_c" -> mcp__a_b_c (origin key "a\0b_c")
	let tool1 = McpTool::new("a-b", "c", "tool 1", json!({}), Arc::clone(&conn));
	let tool2 = McpTool::new("a", "b_c", "tool 2", json!({}), Arc::clone(&conn));

	assert_eq!(tool1.name(), tool2.name());
	assert_ne!(tool1.origin_key(), tool2.origin_key());

	// Tool 2 origin key < tool 1 origin key in ASCII, so tool 2 must win regardless
	// of order!
	let (dedup1, warnings1) =
		deduplicate_mcp_tools_with_warnings(vec![tool1.clone(), tool2.clone()]);
	assert_eq!(dedup1.len(), 1);
	assert_eq!(dedup1[0].server_name(), "a");
	assert_eq!(dedup1[0].tool_name(), "b_c");
	assert_eq!(warnings1.len(), 1);
	assert!(warnings1[0].contains("kept=\"a/b_c\""));
	assert!(warnings1[0].contains("ignored=\"a-b/c\""));
	assert!(warnings1[0].contains("mcp__a_b_c"));

	let (dedup2, warnings2) =
		deduplicate_mcp_tools_with_warnings(vec![tool2.clone(), tool1.clone()]);
	assert_eq!(dedup2.len(), 1);
	assert_eq!(dedup2[0].server_name(), "a");
	assert_eq!(dedup2[0].tool_name(), "b_c");
	assert_eq!(warnings2.len(), 1);
	assert!(warnings2[0].contains("kept=\"a/b_c\""));
	assert!(warnings2[0].contains("ignored=\"a-b/c\""));
}

#[test]
fn test_approval_and_tools_filtering_and_effective_cwd() {
	let temp_dir = TempDir::new("approval-cwd");
	let project_dir = temp_dir.path().join("proj");
	let agent_dir = temp_dir.path().join("agent");
	let omp_dir = project_dir.join(".omp");
	fs::create_dir_all(&omp_dir).expect("create omp dir");
	fs::create_dir_all(&agent_dir).expect("create agent dir");

	let _cfg = spawn_mock_stdio_server(&project_dir);

	// Write .omp/mcp.json pointing to "mock_mcp.py" as relative command argument
	// without cwd Spawn succeeds only if effective cwd is defaulted to project_dir
	fs::write(
		omp_dir.join("mcp.json"),
		json!({
			"mcpServers": {
				"mock": {
					"command": "python3",
					"args": ["mock_mcp.py"]
				}
			}
		})
		.to_string(),
	)
	.expect("write mcp.json");

	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();

	let cancel_token = pi_agent::CancellationToken::new();

	// 1. Without --tools, discovery returns all mock tools
	let mut config = Config::default();
	let approvals = Approvals::new(ApprovalMode::Yolo, Vec::new());
	let tools = rt
		.block_on(load_and_filter_mcp_tools_with_paths(
			&project_dir,
			&agent_dir,
			&config,
			&approvals,
			&cancel_token,
		))
		.expect("load tools with effective cwd");
	let tool_names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
	assert!(tool_names.contains(&"mcp__mock_echo"));

	// 2. With --tools specifying existing tool, only that tool is returned
	config.tools = Some(vec!["mcp__mock_echo".to_string()]);
	let filtered = rt
		.block_on(load_and_filter_mcp_tools_with_paths(
			&project_dir,
			&agent_dir,
			&config,
			&approvals,
			&cancel_token,
		))
		.expect("filter tools");
	assert_eq!(filtered.len(), 1);
	assert_eq!(filtered[0].name(), "mcp__mock_echo");

	// 3. With --tools specifying unknown tool, fails with informative error
	config.tools = Some(vec!["mcp__mock_echo".to_string(), "bogus_tool".to_string()]);
	let err = rt
		.block_on(load_and_filter_mcp_tools_with_paths(
			&project_dir,
			&agent_dir,
			&config,
			&approvals,
			&cancel_token,
		))
		.expect_err("unknown tool must fail");
	let err_str = err.to_string();
	assert!(err_str.contains("Unknown tool in --tools: bogus_tool"));
	assert!(err_str.contains("Valid tools:"));
}

#[test]
fn test_mcp_tool_execution_preserves_content_and_cancellation() {
	let temp_dir = TempDir::new("exec");
	let config = spawn_mock_stdio_server(temp_dir.path());

	let rt = tokio::runtime::Builder::new_multi_thread()
		.worker_threads(2)
		.enable_all()
		.build()
		.unwrap();
	let conn = rt.block_on(async {
		connect_to_server("demo", config)
			.await
			.expect("connect to mock stdio")
	});
	let conn = Arc::new(conn);

	let echo_tool = McpTool::new("demo", "echo", "Echo", json!({}), Arc::clone(&conn));
	let image_tool = McpTool::new("demo", "image_tool", "Image", json!({}), Arc::clone(&conn));
	let hang_tool = McpTool::new("demo", "hang_tool", "Hang", json!({}), Arc::clone(&conn));
	let fail_tool = McpTool::new("demo", "fail_tool", "Fail", json!({}), Arc::clone(&conn));

	// 1. Text execution
	let ctx = pi_agent::AgentContext::new(temp_dir.path().to_path_buf());
	let res_ok = rt
		.block_on(echo_tool.execute(&json!({}), &ctx))
		.expect("execute echo");
	assert_eq!(res_ok.content, "hello from echo");
	assert!(!res_ok.is_error);

	// 2. Image payload is preserved (base64 data present)
	let res_img = rt
		.block_on(image_tool.execute(&json!({}), &ctx))
		.expect("execute image");
	assert!(res_img.content.contains("image/png"));
	assert!(res_img.content.contains("base64=aW1hZ2VkYXRh"));
	assert!(!res_img.is_error);

	// 3. Failing tool call preserves is_error == true
	let res_err = rt
		.block_on(fail_tool.execute(&json!({}), &ctx))
		.expect("execute fail");
	assert!(res_err.content.contains("tool failed badly"));
	assert!(res_err.is_error);

	// 4. In-flight cancellation interrupts hang_tool (which sleeps 10s) via
	//    tokio::select!
	let cancel_token = pi_agent::CancellationToken::new();
	let ctx_cancel = pi_agent::AgentContext::with_cancellation(
		temp_dir.path().to_path_buf(),
		cancel_token.clone(),
	);
	let hang_clone = hang_tool.clone();
	let cancel_timer = cancel_token.clone();

	rt.spawn(async move {
		tokio::time::sleep(std::time::Duration::from_millis(200)).await;
		cancel_timer.cancel();
	});

	let start = std::time::Instant::now();
	let res_cancel = rt
		.block_on(hang_clone.execute(&json!({}), &ctx_cancel))
		.expect("execute");
	let elapsed = start.elapsed();

	assert!(
		elapsed >= std::time::Duration::from_millis(150),
		"must not exit before cancel: {:?}",
		elapsed
	);
	assert!(elapsed < std::time::Duration::from_secs(2), "must interrupt 10s sleep: {:?}", elapsed);
	assert!(res_cancel.is_error);
	assert!(res_cancel.content.contains("cancelled"));
}

#[test]
fn test_discovery_cancellation() {
	let temp_dir = TempDir::new("disc-cancel");
	let project_dir = temp_dir.path().join("proj");
	let agent_dir = temp_dir.path().join("agent");
	fs::create_dir_all(&project_dir).expect("create proj");
	fs::create_dir_all(&agent_dir).expect("create agent");

	let config = Config::default();
	let approvals = Approvals::new(ApprovalMode::Yolo, Vec::new());

	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();

	let cancel_token = pi_agent::CancellationToken::new();
	cancel_token.cancel(); // Pre-cancel

	let tools = rt
		.block_on(load_and_filter_mcp_tools_with_paths(
			&project_dir,
			&agent_dir,
			&config,
			&approvals,
			&cancel_token,
		))
		.expect("cancelled discovery halts cleanly");
	assert!(tools.is_empty());
}
