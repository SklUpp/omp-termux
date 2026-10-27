//! Tests for stdio MCP launch approval gate, --tools filtering before spawn,
//! truthful connection errors, and early discovery cancellation.

use std::fs;
use std::path::Path;

use omp_cli::approve::{ApprovalMode, Approvals};
use omp_cli::config::{Config, DiscoveredMcpServer};
use omp_cli::runtime::filter_and_spawn_mcp_servers;
use pi_agent::mcp::{MCPServerConfig, MCPStdioServerConfig, create_mcp_tool_name};
use tempfile::TempDir;

fn make_mock_stdio_server(dir: &Path, server_name: &str) -> (DiscoveredMcpServer, std::path::PathBuf) {
	let marker_file = dir.join(format!("{server_name}_spawned.marker"));
	let script_path = dir.join(format!("{server_name}_mcp.py"));

	fs::write(
		&script_path,
		format!(
			r#"import sys, json
open("{}", "w").write("spawned")
for line in sys.stdin:
    if not line: break
    req = json.loads(line)
    mid = req.get("id")
    method = req.get("method")
    if method == "initialize":
        res = {{"jsonrpc":"2.0","id":mid,"result":{{"protocolVersion":"2024-11-05","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"{}","version":"1.0"}}}}}}
    elif method == "tools/list":
        res = {{"jsonrpc":"2.0","id":mid,"result":{{"tools":[{{"name":"echo","description":"Echo","inputSchema":{{"type":"object"}}}}]}}}}
    else:
        res = {{"jsonrpc":"2.0","id":mid,"result":{{}}}}
    sys.stdout.write(json.dumps(res) + "\n")
    sys.stdout.flush()
"#,
			marker_file.display(),
			server_name,
		),
	)
	.expect("write mock script");

	let config = MCPServerConfig::Stdio(MCPStdioServerConfig {
		command: "python3".to_string(),
		args: vec![script_path.to_str().unwrap().to_string()],
		env: std::collections::HashMap::new(),
		cwd: None,
		timeout: Some(5000),
		request_id_format: None,
	});

	let server = DiscoveredMcpServer {
		name: server_name.to_string(),
		config,
		source_path: dir.to_path_buf(),
	};

	(server, marker_file)
}

#[test]
fn test_stdio_mcp_launch_denied_without_allow_or_yolo() {
	let temp_dir = TempDir::new().unwrap();
	let (server, marker_file) = make_mock_stdio_server(temp_dir.path(), "mock");

	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();

	let config = Config::default();
	let approvals = Approvals::new(ApprovalMode::AlwaysAsk, Vec::new());
	let cancel_token = pi_agent::CancellationToken::new();

	let tools = rt
		.block_on(filter_and_spawn_mcp_servers(
			vec![server],
			temp_dir.path(),
			&config,
			&approvals,
			&cancel_token,
		))
		.expect("discovery must not abort runs when server is skipped");

	assert!(tools.is_empty(), "unapproved stdio server tools must not be registered");
	assert!(
		!marker_file.exists(),
		"stdio MCP server must NOT be spawned when launch approval is denied"
	);
}

#[test]
fn test_stdio_mcp_launch_denied_under_write_mode_without_allow() {
	let temp_dir = TempDir::new().unwrap();
	let (server, marker_file) = make_mock_stdio_server(temp_dir.path(), "mock");

	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();

	let config = Config::default();
	let approvals = Approvals::new(ApprovalMode::Write, Vec::new());
	let cancel_token = pi_agent::CancellationToken::new();

	let tools = rt
		.block_on(filter_and_spawn_mcp_servers(
			vec![server],
			temp_dir.path(),
			&config,
			&approvals,
			&cancel_token,
		))
		.expect("discovery must not abort runs under write mode when server is skipped");

	assert!(tools.is_empty());
	assert!(
		!marker_file.exists(),
		"stdio MCP server must NOT be spawned under write mode without explicit allow"
	);
}

#[test]
fn test_stdio_mcp_launch_allowed_with_yolo() {
	let temp_dir = TempDir::new().unwrap();
	let (server, marker_file) = make_mock_stdio_server(temp_dir.path(), "mock");

	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();

	let config = Config::default();
	let approvals = Approvals::new(ApprovalMode::Yolo, Vec::new());
	let cancel_token = pi_agent::CancellationToken::new();

	let tools = rt
		.block_on(filter_and_spawn_mcp_servers(
			vec![server],
			temp_dir.path(),
			&config,
			&approvals,
			&cancel_token,
		))
		.expect("yolo must permit stdio launch");

	assert_eq!(tools.len(), 1);
	assert_eq!(tools[0].name(), "mcp__mock_echo");
	assert!(
		marker_file.exists(),
		"stdio MCP server must be spawned under yolo"
	);
}

#[test]
fn test_stdio_mcp_launch_allowed_with_explicit_server_allow() {
	let temp_dir = TempDir::new().unwrap();
	let (server, marker_file) = make_mock_stdio_server(temp_dir.path(), "mock");

	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();

	let config = Config::default();
	let approvals = Approvals::new(ApprovalMode::AlwaysAsk, vec!["mock".to_string()]);
	let cancel_token = pi_agent::CancellationToken::new();

	let tools = rt
		.block_on(filter_and_spawn_mcp_servers(
			vec![server],
			temp_dir.path(),
			&config,
			&approvals,
			&cancel_token,
		))
		.expect("explicit server allow must permit stdio launch");

	assert_eq!(tools.len(), 1);
	assert_eq!(tools[0].name(), "mcp__mock_echo");
	assert!(
		marker_file.exists(),
		"stdio MCP server must be spawned when server is explicitly allowed"
	);
}

#[test]
fn test_stdio_mcp_launch_allowed_with_explicit_tool_allow() {
	let temp_dir = TempDir::new().unwrap();
	let (server, marker_file) = make_mock_stdio_server(temp_dir.path(), "mock");

	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();

	let config = Config::default();
	let approvals = Approvals::new(ApprovalMode::AlwaysAsk, vec!["mcp__mock_echo".to_string()]);
	let cancel_token = pi_agent::CancellationToken::new();

	let tools = rt
		.block_on(filter_and_spawn_mcp_servers(
			vec![server],
			temp_dir.path(),
			&config,
			&approvals,
			&cancel_token,
		))
		.expect("explicit tool allow must permit stdio launch");

	assert_eq!(tools.len(), 1);
	assert_eq!(tools[0].name(), "mcp__mock_echo");
	assert!(
		marker_file.exists(),
		"stdio MCP server must be spawned when tool is explicitly allowed"
	);
}

#[test]
fn test_stdio_mcp_skipped_when_tools_filter_excludes_it() {
	let temp_dir = TempDir::new().unwrap();
	let (server, marker_file) = make_mock_stdio_server(temp_dir.path(), "mock");

	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();

	// Even under Yolo, if --tools restricts to builtin "read", mock server must NOT be spawned
	let mut config = Config::default();
	config.tools = Some(vec!["read".to_string()]);
	let approvals = Approvals::new(ApprovalMode::Yolo, Vec::new());
	let cancel_token = pi_agent::CancellationToken::new();

	let tools = rt
		.block_on(filter_and_spawn_mcp_servers(
			vec![server],
			temp_dir.path(),
			&config,
			&approvals,
			&cancel_token,
		))
		.expect("excluding server via --tools must skip spawn cleanly");

	assert!(tools.is_empty());
	assert!(
		!marker_file.exists(),
		"stdio MCP server must NOT be spawned when excluded by --tools filter"
	);
}

#[test]
fn test_truthful_connection_error_when_server_fails() {
	let temp_dir = TempDir::new().unwrap();
	let broken_config = MCPServerConfig::Stdio(MCPStdioServerConfig {
		command: "/nonexistent/binary_path_that_fails".to_string(),
		args: Vec::new(),
		env: std::collections::HashMap::new(),
		cwd: None,
		timeout: Some(1000),
		request_id_format: None,
	});

	let server = DiscoveredMcpServer {
		name: "broken_srv".to_string(),
		config: broken_config,
		source_path: temp_dir.path().to_path_buf(),
	};

	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();

	let mut config = Config::default();
	config.tools = Some(vec!["mcp__broken_srv_echo".to_string()]);
	let approvals = Approvals::new(ApprovalMode::Yolo, Vec::new());
	let cancel_token = pi_agent::CancellationToken::new();

	let res = rt.block_on(filter_and_spawn_mcp_servers(
		vec![server],
		temp_dir.path(),
		&config,
		&approvals,
		&cancel_token,
	));

	let err = res.expect_err("connecting to broken server must fail");
	let err_msg = err.to_string();
	assert!(
		err_msg.contains("Failed to connect to MCP server \"broken_srv\""),
		"must report truthful connection error, got: {err_msg}"
	);
	assert!(
		!err_msg.contains("Unknown tool in --tools"),
		"must NOT report misleading Unknown tool error when server connection failed, got: {err_msg}"
	);
}

#[test]
fn test_truthful_error_when_denied_server_tool_requested_in_tools() {
	let temp_dir = TempDir::new().unwrap();
	let (server, marker_file) = make_mock_stdio_server(temp_dir.path(), "mock");

	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();

	let mut config = Config::default();
	config.tools = Some(vec!["mcp__mock_echo".to_string()]);
	let approvals = Approvals::new(ApprovalMode::AlwaysAsk, Vec::new());
	let cancel_token = pi_agent::CancellationToken::new();

	let res = rt.block_on(filter_and_spawn_mcp_servers(
		vec![server],
		temp_dir.path(),
		&config,
		&approvals,
		&cancel_token,
	));

	let err = res.expect_err("requesting denied server tool must fail with truthful launch denial");
	let err_msg = err.to_string();
	assert!(
		err_msg.contains("MCP server \"mock\" was not launched"),
		"must report truthful launch denial error, got: {err_msg}"
	);
	assert!(
		!err_msg.contains("Unknown tool in --tools"),
		"must NOT report misleading Unknown tool error when server launch was denied, got: {err_msg}"
	);
	assert!(
		!marker_file.exists(),
		"stdio MCP server must NOT be spawned when launch approval is denied"
	);
}

#[test]
fn test_exact_server_identity_prevents_sanitized_collision() {
	let temp_dir = TempDir::new().unwrap();
	let (server_dash, marker_dash) = make_mock_stdio_server(temp_dir.path(), "foo-bar");
	let (server_dot, marker_dot) = make_mock_stdio_server(temp_dir.path(), "foo.bar");

	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();

	let config = Config::default();
	// Explicitly allow ONLY "foo-bar", NOT "foo.bar"
	let approvals = Approvals::new(ApprovalMode::AlwaysAsk, vec!["foo-bar".to_string()]);
	let cancel_token = pi_agent::CancellationToken::new();

	let _tools = rt
		.block_on(filter_and_spawn_mcp_servers(
			vec![server_dash, server_dot],
			temp_dir.path(),
			&config,
			&approvals,
			&cancel_token,
		))
		.expect("discovery should proceed");

	assert!(marker_dash.exists(), "foo-bar was explicitly allowed and must spawn");
	assert!(!marker_dot.exists(), "foo.bar was NOT allowed and must NOT spawn despite sanitized name similarity");
}

#[test]
fn test_tool_level_allow_ambiguous_prefix_denies_both_servers() {
	let temp_dir = TempDir::new().unwrap();
	let (server_foo, marker_foo) = make_mock_stdio_server(temp_dir.path(), "foo");
	let (server_foo_bar, marker_foo_bar) = make_mock_stdio_server(temp_dir.path(), "foo_bar");

	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();

	let config = Config::default();
	// Ambiguous tool allow: could be tool "bar_echo" on "foo", or tool "echo" on "foo_bar"
	let approvals = Approvals::new(ApprovalMode::AlwaysAsk, vec!["mcp__foo_bar_echo".to_string()]);
	let cancel_token = pi_agent::CancellationToken::new();

	let tools = rt
		.block_on(filter_and_spawn_mcp_servers(
			vec![server_foo, server_foo_bar],
			temp_dir.path(),
			&config,
			&approvals,
			&cancel_token,
		))
		.expect("discovery should proceed");

	assert!(tools.is_empty(), "neither server should register tools without explicit server allow when ambiguous");
	assert!(!marker_foo.exists(), "unrelated prefix server foo must NOT spawn for ambiguous tool allow");
	assert!(!marker_foo_bar.exists(), "server foo_bar must NOT spawn on ambiguous tool allow without explicit server-level allow");
}

#[test]
fn test_explicit_server_allow_resolves_ambiguity_between_prefix_servers() {
	let temp_dir = TempDir::new().unwrap();
	let (server_foo, marker_foo) = make_mock_stdio_server(temp_dir.path(), "foo");
	let (server_foo_bar, marker_foo_bar) = make_mock_stdio_server(temp_dir.path(), "foo_bar");

	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();

	let config = Config::default();
	// Explicit server-level allow resolves the ambiguity conservatively
	let approvals = Approvals::new(ApprovalMode::AlwaysAsk, vec!["foo_bar".to_string()]);
	let cancel_token = pi_agent::CancellationToken::new();

	let tools = rt
		.block_on(filter_and_spawn_mcp_servers(
			vec![server_foo, server_foo_bar],
			temp_dir.path(),
			&config,
			&approvals,
			&cancel_token,
		))
		.expect("discovery should proceed");

	assert_eq!(tools.len(), 1);
	assert_eq!(tools[0].name(), "mcp__foo_bar_echo");
	assert!(marker_foo_bar.exists(), "foo_bar was explicitly allowed at server level and must spawn");
	assert!(!marker_foo.exists(), "foo was NOT allowed and must NOT spawn");
}

#[test]
fn test_tools_prefilter_does_not_launch_unrelated_prefix_server() {
	let temp_dir = TempDir::new().unwrap();
	let (server_foo, marker_foo) = make_mock_stdio_server(temp_dir.path(), "foo");
	let (server_foo_bar, marker_foo_bar) = make_mock_stdio_server(temp_dir.path(), "foo_bar");

	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();

	// Even under Yolo, --tools specifying foo_bar's tool must NOT launch unrelated server foo
	let mut config = Config::default();
	config.tools = Some(vec!["mcp__foo_bar_echo".to_string()]);
	let approvals = Approvals::new(ApprovalMode::Yolo, Vec::new());
	let cancel_token = pi_agent::CancellationToken::new();

	let tools = rt
		.block_on(filter_and_spawn_mcp_servers(
			vec![server_foo, server_foo_bar],
			temp_dir.path(),
			&config,
			&approvals,
			&cancel_token,
		))
		.expect("discovery should proceed");

	assert_eq!(tools.len(), 1);
	assert_eq!(tools[0].name(), "mcp__foo_bar_echo");
	assert!(!marker_foo.exists(), "unrelated server foo must NOT spawn when --tools requested foo_bar's tool");
	assert!(marker_foo_bar.exists(), "foo_bar must spawn when its tool is requested in --tools");
}

#[test]
fn test_tools_prefilter_matches_bounded_64_char_tool_name() {
	let temp_dir = TempDir::new().unwrap();
	let long_name = "a_very_long_mcp_server_name_that_exceeds_fifty_characters_in_total";
	let (server, marker) = make_mock_stdio_server(temp_dir.path(), long_name);

	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();

	let tool_name = create_mcp_tool_name(long_name, "echo");
	assert_eq!(tool_name.len(), 64, "tool name must be capped at 64 chars");

	let mut config = Config::default();
	config.tools = Some(vec![tool_name.clone()]);
	let approvals = Approvals::new(ApprovalMode::Yolo, Vec::new());
	let cancel_token = pi_agent::CancellationToken::new();

	let tools = rt
		.block_on(filter_and_spawn_mcp_servers(
			vec![server],
			temp_dir.path(),
			&config,
			&approvals,
			&cancel_token,
		))
		.expect("discovery should proceed");

	assert_eq!(tools.len(), 1);
	assert_eq!(tools[0].name(), tool_name);
	assert!(marker.exists(), "server with bounded 64-char tool name must be matched by --tools and spawned");
}

#[test]
fn test_cancellation_aborts_discovery_without_spawning() {
	let temp_dir = TempDir::new().unwrap();
	let (server, marker) = make_mock_stdio_server(temp_dir.path(), "mock");

	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();

	let config = Config::default();
	let approvals = Approvals::new(ApprovalMode::Yolo, Vec::new());
	let cancel_token = pi_agent::CancellationToken::new();
	cancel_token.cancel();

	let tools = rt
		.block_on(filter_and_spawn_mcp_servers(
			vec![server],
			temp_dir.path(),
			&config,
			&approvals,
			&cancel_token,
		))
		.expect("cancelled discovery returns cleanly");

	assert!(tools.is_empty(), "cancelled discovery must not return tools");
	assert!(!marker.exists(), "cancelled discovery must not spawn servers or leave readers");
}

#[test]
fn test_two_long_servers_differing_in_prefix_spawns_only_requested_server() {
	let temp_dir = TempDir::new().unwrap();
	let name_a = "long_server_alpha_name_exceeding_fifty_characters_in_total";
	let name_b = "long_server_beta_name_exceeding_fifty_characters_in_total";
	let (server_a, marker_a) = make_mock_stdio_server(temp_dir.path(), name_a);
	let (server_b, marker_b) = make_mock_stdio_server(temp_dir.path(), name_b);

	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();

	let tool_a = create_mcp_tool_name(name_a, "echo");
	assert_eq!(tool_a.len(), 64, "tool name must be capped at 64 chars");

	let mut config = Config::default();
	config.tools = Some(vec![tool_a.clone()]);
	let approvals = Approvals::new(ApprovalMode::Yolo, Vec::new());
	let cancel_token = pi_agent::CancellationToken::new();

	let tools = rt
		.block_on(filter_and_spawn_mcp_servers(
			vec![server_a, server_b],
			temp_dir.path(),
			&config,
			&approvals,
			&cancel_token,
		))
		.expect("discovery should proceed");

	assert_eq!(tools.len(), 1);
	assert_eq!(tools[0].name(), tool_a);
	assert!(marker_a.exists(), "server A must spawn when its 64-char capped tool is requested");
	assert!(!marker_b.exists(), "server B must NOT spawn when server A's capped tool is requested");
}

#[test]
fn test_two_long_servers_uncapped_exact_length_64_prefilter_spawns_only_requested() {
	let temp_dir = TempDir::new().unwrap();
	let name_a = "shared_prefix_exceeding_fifty_characters_in_total_aaaa";
	let name_b = "shared_prefix_exceeding_fifty_characters_in_total_bbbb";
	let (server_a, marker_a) = make_mock_stdio_server(temp_dir.path(), name_a);
	let (server_b, marker_b) = make_mock_stdio_server(temp_dir.path(), name_b);

	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();

	let tool_a = create_mcp_tool_name(name_a, "echo");
	assert_eq!(tool_a.len(), 64, "tool name must be exactly 64 chars");

	let mut config = Config::default();
	config.tools = Some(vec![tool_a.clone()]);
	let approvals = Approvals::new(ApprovalMode::Yolo, Vec::new());
	let cancel_token = pi_agent::CancellationToken::new();

	let tools = rt
		.block_on(filter_and_spawn_mcp_servers(
			vec![server_a, server_b],
			temp_dir.path(),
			&config,
			&approvals,
			&cancel_token,
		))
		.expect("discovery should proceed");

	assert_eq!(tools.len(), 1);
	assert_eq!(tools[0].name(), tool_a);
	assert!(marker_a.exists(), "server A must spawn when its tool is requested");
	assert!(!marker_b.exists(), "server B must NOT spawn when server A's tool is requested");
}

#[test]
fn test_two_long_servers_sharing_first_50_chars_tool_allow_spawns_only_requested() {
	let temp_dir = TempDir::new().unwrap();
	let name_a = "shared_prefix_exceeding_fifty_characters_in_total_aaaa";
	let name_b = "shared_prefix_exceeding_fifty_characters_in_total_bbbb";
	let (server_a, marker_a) = make_mock_stdio_server(temp_dir.path(), name_a);
	let (server_b, marker_b) = make_mock_stdio_server(temp_dir.path(), name_b);

	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();

	let tool_a = create_mcp_tool_name(name_a, "echo");
	assert_eq!(tool_a.len(), 64, "tool name must be exactly 64 chars");

	let config = Config::default();
	let approvals = Approvals::new(ApprovalMode::AlwaysAsk, vec![tool_a.clone()]);
	let cancel_token = pi_agent::CancellationToken::new();

	let tools = rt
		.block_on(filter_and_spawn_mcp_servers(
			vec![server_a, server_b],
			temp_dir.path(),
			&config,
			&approvals,
			&cancel_token,
		))
		.expect("discovery should proceed");

	assert_eq!(tools.len(), 1);
	assert_eq!(tools[0].name(), tool_a);
	assert!(marker_a.exists(), "server A must spawn when its unambiguous 64-char tool is allowed");
	assert!(!marker_b.exists(), "server B must NOT spawn when server A's tool is allowed");
}

#[test]
fn test_real_capped_tie_tool_allow_ambiguous_denies_both() {
	let temp_dir = TempDir::new().unwrap();
	let prefix = "common_prefix_long_enough_to_exceed_the_fifty_limit_";
	let name_a = format!("{prefix}xxxxxx");
	let name_b = format!("{prefix}yyyyyy");
	let (server_a, marker_a) = make_mock_stdio_server(temp_dir.path(), &name_a);
	let (server_b, marker_b) = make_mock_stdio_server(temp_dir.path(), &name_b);

	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();

	let tool_a = create_mcp_tool_name(&name_a, "echo");
	assert_eq!(tool_a.len(), 64, "capped tool name must be 64 chars");

	let config = Config::default();
	let approvals = Approvals::new(ApprovalMode::AlwaysAsk, vec![tool_a]);
	let cancel_token = pi_agent::CancellationToken::new();

	let tools = rt
		.block_on(filter_and_spawn_mcp_servers(
			vec![server_a, server_b],
			temp_dir.path(),
			&config,
			&approvals,
			&cancel_token,
		))
		.expect("discovery should proceed");

	assert!(tools.is_empty(), "tied capped tool allow must deny launch for both servers without explicit server allow");
	assert!(!marker_a.exists(), "server A must NOT spawn under ambiguous capped tool allow");
	assert!(!marker_b.exists(), "server B must NOT spawn under ambiguous capped tool allow");
}

#[test]
fn test_real_capped_tie_prefilter_returns_ambiguity_error_under_yolo() {
	let temp_dir = TempDir::new().unwrap();
	let prefix = "common_prefix_long_enough_to_exceed_the_fifty_limit_";
	let name_a = format!("{prefix}xxxxxx");
	let name_b = format!("{prefix}yyyyyy");
	let (server_a, marker_a) = make_mock_stdio_server(temp_dir.path(), &name_a);
	let (server_b, marker_b) = make_mock_stdio_server(temp_dir.path(), &name_b);

	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();

	let tool_a = create_mcp_tool_name(&name_a, "echo");
	assert_eq!(tool_a.len(), 64, "capped tool name must be 64 chars");

	let mut config = Config::default();
	config.tools = Some(vec![tool_a.clone()]);
	let approvals = Approvals::new(ApprovalMode::Yolo, Vec::new());
	let cancel_token = pi_agent::CancellationToken::new();

	let res = rt.block_on(filter_and_spawn_mcp_servers(
		vec![server_a, server_b],
		temp_dir.path(),
		&config,
		&approvals,
		&cancel_token,
	));

	let err = res.expect_err("tied capped prefix under Yolo must return ambiguity error");
	let err_msg = err.to_string();
	assert!(
		err_msg.contains("Ambiguous tool") && err_msg.contains("matches multiple servers"),
		"must return precise ambiguity error naming servers, got: {err_msg}"
	);
	assert!(!marker_a.exists(), "tied capped prefix must not spawn A");
	assert!(!marker_b.exists(), "tied capped prefix must not spawn B");
}

#[test]
fn test_real_capped_tie_disambiguated_by_server_allow_spawns_only_a() {
	let temp_dir = TempDir::new().unwrap();
	let prefix = "common_prefix_long_enough_to_exceed_the_fifty_limit_";
	let name_a = format!("{prefix}xxxxxx");
	let name_b = format!("{prefix}yyyyyy");
	let (server_a, marker_a) = make_mock_stdio_server(temp_dir.path(), &name_a);
	let (server_b, marker_b) = make_mock_stdio_server(temp_dir.path(), &name_b);

	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();

	let tool_a = create_mcp_tool_name(&name_a, "echo");
	assert_eq!(tool_a.len(), 64, "capped tool name must be 64 chars");

	let mut config = Config::default();
	config.tools = Some(vec![tool_a.clone()]);
	let approvals = Approvals::new(ApprovalMode::AlwaysAsk, vec![name_a.clone()]);
	let cancel_token = pi_agent::CancellationToken::new();

	let tools = rt
		.block_on(filter_and_spawn_mcp_servers(
			vec![server_a, server_b],
			temp_dir.path(),
			&config,
			&approvals,
			&cancel_token,
		))
		.expect("explicit server allow must disambiguate capped tie");

	assert_eq!(tools.len(), 1);
	assert_eq!(tools[0].name(), tool_a);
	assert!(marker_a.exists(), "server A must spawn when disambiguated by server allow");
	assert!(!marker_b.exists(), "unrelated server B must NOT spawn");
}

#[test]
fn test_tool_level_allow_with_exact_tool_name_does_not_launch_distinct_server() {
	let temp_dir = TempDir::new().unwrap();
	let (server_foo, marker_foo) = make_mock_stdio_server(temp_dir.path(), "foo");
	let (server_foo_bar, marker_foo_bar) = make_mock_stdio_server(temp_dir.path(), "foo_bar");

	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();

	let config = Config::default();
	// User allowed mcp__foo_bar (tool "bar" on server "foo").
	// When server "foo_bar" is also discovered, mcp__foo_bar matches bare server form
	// for foo_bar and tool on foo: ambiguous, fail-closed, neither server spawns.
	let approvals = Approvals::new(ApprovalMode::AlwaysAsk, vec!["mcp__foo_bar".to_string()]);
	let cancel_token = pi_agent::CancellationToken::new();

	let tools = rt
		.block_on(filter_and_spawn_mcp_servers(
			vec![server_foo, server_foo_bar],
			temp_dir.path(),
			&config,
			&approvals,
			&cancel_token,
		))
		.expect("discovery should proceed");

	assert!(tools.is_empty(), "ambiguous bare server mcp__ entry must register no tools");
	assert!(!marker_foo_bar.exists(), "server foo_bar must NOT spawn from mcp__foo_bar tool allow");
	assert!(!marker_foo.exists(), "server foo must NOT spawn because identity is ambiguous with foo_bar");
}

#[test]
fn test_tool_level_allow_with_matching_tool_launches_when_server_identity_unambiguous() {
	let temp_dir = TempDir::new().unwrap();
	// Discovery-shaping resistance: foo_bar is configured in .omp/mcp.json but disabled.
	// A naive check seeing only discovered server "foo" would mistake `mcp__foo_bar` for tool
	// "bar" on "foo" and launch foo. omp inspects all config layers, detects collision with
	// configured server foo_bar, and fails closed.
	let mcp_json = temp_dir.path().join(".omp").join("mcp.json");
	fs::create_dir_all(mcp_json.parent().unwrap()).unwrap();
	fs::write(
		&mcp_json,
		r#"{"mcpServers":{"foo_bar":{"command":"true","enabled":false}}}"#,
	)
	.unwrap();

	let (server_foo, marker_foo) = make_mock_stdio_server(temp_dir.path(), "foo");

	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();

	let config = Config::default();
	let approvals = Approvals::new(ApprovalMode::AlwaysAsk, vec!["mcp__foo_bar".to_string()]);
	let cancel_token = pi_agent::CancellationToken::new();

	let tools = rt
		.block_on(filter_and_spawn_mcp_servers(
			vec![server_foo],
			temp_dir.path(),
			&config,
			&approvals,
			&cancel_token,
		))
		.expect("discovery should proceed");

	assert!(tools.is_empty(), "structurally ambiguous mcp__ allow must register no tools");
	assert!(!marker_foo.exists(), "server foo must NOT spawn from structurally ambiguous mcp__ allow");

	// Unambiguous colon syntax mcp:foo:echo explicitly and safely authorizes the tool.
	let (server_foo_2, marker_foo_2) = make_mock_stdio_server(temp_dir.path(), "foo");
	let approvals_colon = Approvals::new(ApprovalMode::AlwaysAsk, vec!["mcp:foo:echo".to_string()]);
	let tools_colon = rt
		.block_on(filter_and_spawn_mcp_servers(
			vec![server_foo_2],
			temp_dir.path(),
			&config,
			&approvals_colon,
			&cancel_token,
		))
		.expect("discovery should proceed");

	assert_eq!(tools_colon.len(), 1);
	assert!(marker_foo_2.exists(), "server foo must spawn when authorized via unambiguous colon syntax");
}
