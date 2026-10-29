//! Integration tests: the argument surface matches the TypeScript CLI, the
//! runner seam accepts an implementation, and the binary fails honestly when
//! its configuration is missing.

use std::path::PathBuf;
use std::process::{Command as ProcCommand, Stdio};
use std::sync::Arc;

use clap::Parser;
use omp_cli::{
	EXIT_FAILURE, EXIT_SUCCESS, EXIT_USAGE, Result,
	agent::{AgentRunner, Event, EventStream, RunRequest},
	approve::{ApprovalMode, Approvals},
	args::{Cli, Command},
	catalog::Catalog,
	config::Config,
	output::OutputMode,
	session::{SessionOptions, SessionStore, TurnBuilder},
};

fn parse(args: &[&str]) -> Cli {
	Cli::try_parse_from(args).expect("parse")
}

fn omp() -> ProcCommand {
	ProcCommand::new(env!("CARGO_BIN_EXE_omp"))
}

#[test]
fn parses_the_documented_flag_surface() {
	let cli = parse(&[
		"omp",
		"-p",
		"--mode",
		"json",
		"--cwd",
		"/work",
		"--model",
		"openai/gpt-5",
		"--provider",
		"openai",
		"--api-key",
		"sk-test",
		"--system-prompt",
		"SYSTEM.md",
		"--append-system-prompt",
		"APPEND_SYSTEM.md",
		"--max-time",
		"1h",
		"--no-tools",
		"--approval-mode",
		"write",
		"--allow",
		"read",
		"--allow",
		"bash",
		"--config",
		"one.yml",
		"--config",
		"two.yml",
		"-c",
		"--session-dir",
		"/tmp/sessions",
		"--session-id",
		"fixed",
		"--no-session",
		"--no-skills",
		"--skills",
		"review,test*",
		"do the thing",
	]);
	assert!(cli.print);
	assert_eq!(cli.mode, OutputMode::Json);
	assert_eq!(cli.approval_mode, Some(ApprovalMode::Write));
	assert_eq!(cli.allow, ["read", "bash"]);
	assert_eq!(cli.skills, ["review", "test*"]);
	assert!(cli.continue_session);
	assert_eq!(cli.session_dir, Some(PathBuf::from("/tmp/sessions")));
	assert_eq!(cli.session_id.as_deref(), Some("fixed"));
	assert!(cli.no_session);
	assert_eq!(cli.prompt, ["do the thing"]);
	assert_eq!(cli.max_time.map(|time| time.as_secs()), Some(3_600));
}

#[test]
fn rejects_values_outside_the_typescript_domains() {
	assert!(Cli::try_parse_from(["omp", "--mode", "yaml"]).is_err());
	assert!(Cli::try_parse_from(["omp", "--approval-mode", "auto"]).is_err());
	assert!(Cli::try_parse_from(["omp", "--unknown-flag"]).is_err());
	assert!(Cli::try_parse_from(["omp", "--max-time", "soon"]).is_err());
}

#[test]
fn rpc_mode_parses_without_a_prompt() {
	let cli = parse(&["omp", "--mode", "rpc"]);
	assert_eq!(cli.mode, OutputMode::Rpc);
	assert!(cli.prompt.is_empty());
	assert!(cli.file_args.is_empty());
	assert!(!cli.print);
}

#[test]
fn repl_is_a_subcommand() {
	assert!(matches!(parse(&["omp", "repl"]).command, Some(Command::Repl)));
}

#[test]
fn help_lists_the_runtime_flags() {
	let output = omp().arg("--help").output().expect("run omp --help");
	assert!(output.status.success());
	let stdout = String::from_utf8(output.stdout).expect("utf8");
	for flag in ["--print", "--mode", "--cwd", "--approval-mode", "--skills", "repl"] {
		assert!(stdout.contains(flag), "help is missing {flag}");
	}
}

#[test]
fn missing_configuration_is_an_honest_failure() {
	// No models.json/models.yml in an isolated config dir: the binary must exit
	// non-zero with a diagnostic instead of fabricating a session.
	let isolated = std::env::temp_dir().join(format!("omp-cli-empty-config-{}", std::process::id()));
	let share = isolated.join("share");
	std::fs::create_dir_all(&share).expect("temp share dir");
	let output = omp()
		.args(["--print", "hello"])
		.env("PI_CONFIG_DIR", &isolated)
		.env("PI_CODING_AGENT_DIR", isolated.join("agent"))
		.env("OMP_SHARE_DIR", &share)
		.output()
		.expect("run omp");
	std::fs::remove_dir_all(&isolated).ok();
	assert_eq!(output.status.code(), Some(i32::from(EXIT_FAILURE)));
	let stderr = String::from_utf8(output.stderr).expect("utf8");
	assert!(stderr.contains("no model"), "stderr was {stderr:?}");
	assert!(output.stdout.is_empty());
}

#[test]
fn version_and_help_exit_zero() {
	for flag in ["--version", "--help"] {
		let output = omp().arg(flag).output().expect("run omp");
		assert_eq!(output.status.code(), Some(i32::from(EXIT_SUCCESS)), "{flag}");
	}
}

#[test]
fn print_mode_without_input_exits_zero() {
	// TS print mode with zero messages writes nothing and exits 0; the binary
	// must reach that path without inventing a turn.
	let root = std::env::temp_dir().join(format!("omp-cli-empty-prompt-{}", std::process::id()));
	let share = root.join("share");
	let work = root.join("work");
	std::fs::create_dir_all(&share).expect("share dir");
	std::fs::create_dir_all(&work).expect("work dir");
	std::fs::write(work.join("models.json"), r#"{"local":{"m":{"id":"m","provider":"local"}}}"#)
		.expect("models fixture");
	let output = omp()
		.args(["--print", "--cwd"])
		.arg(&work)
		.env("PI_CONFIG_DIR", root.join("config"))
		.env("PI_CODING_AGENT_DIR", root.join("config/agent"))
		.env("OMP_SHARE_DIR", &share)
		.stdin(Stdio::null())
		.output()
		.expect("run omp");
	std::fs::remove_dir_all(&root).ok();
	assert_eq!(
		output.status.code(),
		Some(i32::from(EXIT_SUCCESS)),
		"stderr: {}",
		String::from_utf8_lossy(&output.stderr)
	);
	assert!(output.stdout.is_empty());
}

#[test]
fn bad_max_time_is_a_usage_error() {
	let output = omp()
		.args(["--max-time", "soon", "hi"])
		.output()
		.expect("run omp");
	assert_eq!(output.status.code(), Some(i32::from(EXIT_USAGE)));
	let stderr = String::from_utf8(output.stderr).expect("utf8");
	assert!(stderr.contains("Invalid --max-time value"), "stderr was {stderr:?}");
}

#[test]
fn known_verbs_are_refused_without_starting_a_runtime() {
	let root = std::env::temp_dir().join(format!("omp-cli-verb-refusal-{}", std::process::id()));
	let share = root.join("share");
	std::fs::create_dir_all(&share).expect("share dir");
	let agent = root.join("config/agent");
	let run = |args: &[&str]| {
		omp()
			.args(args)
			.env("PI_CONFIG_DIR", root.join("config"))
			.env("PI_CODING_AGENT_DIR", &agent)
			.env("OMP_SHARE_DIR", &share)
			.stdin(Stdio::null())
			.output()
			.expect("run omp")
	};

	let missing = run(&["acp"]);
	assert_eq!(missing.status.code(), Some(i32::from(EXIT_USAGE)));
	let stderr = String::from_utf8(missing.stderr).expect("utf8");
	assert!(stderr.contains("`omp acp` is not implemented"), "stderr was {stderr:?}");
	assert!(missing.stdout.is_empty());

	let excluded = run(&["plugin", "list"]);
	assert_eq!(excluded.status.code(), Some(i32::from(EXIT_USAGE)));
	let stderr = String::from_utf8(excluded.stderr).expect("utf8");
	assert!(
		stderr.contains("outside the approved boundary"),
		"stderr was {stderr:?}"
	);
	assert!(excluded.stdout.is_empty());

	// The refusal path starts nothing: no agent directory, no session log.
	assert!(!agent.exists(), "refusal must not create the agent directory");
	assert!(
		std::fs::read_dir(&share).expect("share dir").next().is_none(),
		"refusal must not write to the share dir"
	);

	// Only a leading help/version request stays with clap's own actions.
	let help = run(&["--help"]);
	assert_eq!(help.status.code(), Some(i32::from(EXIT_SUCCESS)));
	assert!(
		String::from_utf8_lossy(&help.stdout).contains("--print"),
		"stdout: {}",
		String::from_utf8_lossy(&help.stdout)
	);
	let version = run(&["--version"]);
	assert_eq!(version.status.code(), Some(i32::from(EXIT_SUCCESS)));
	assert!(
		String::from_utf8_lossy(&version.stdout).contains(env!("CARGO_PKG_VERSION")),
		"stdout: {}",
		String::from_utf8_lossy(&version.stdout)
	);
	// `launch` keeps the root help and version: the escape hatch does not fork the
	// user-facing surface, wherever the request sits in the leading flags.
	for args in [vec!["launch", "--help"], vec!["--help", "launch"]] {
		let help = run(&args);
		assert_eq!(help.status.code(), Some(i32::from(EXIT_SUCCESS)), "{args:?}");
		assert!(
			String::from_utf8_lossy(&help.stdout).contains("--print"),
			"{args:?} must print the root help: {}",
			String::from_utf8_lossy(&help.stdout)
		);
	}
	for args in [vec!["launch", "--version"], vec!["--version", "launch"]] {
		let version = run(&args);
		assert_eq!(version.status.code(), Some(i32::from(EXIT_SUCCESS)), "{args:?}");
		assert!(
			String::from_utf8_lossy(&version.stdout).contains(env!("CARGO_PKG_VERSION")),
			"{args:?}: {}",
			String::from_utf8_lossy(&version.stdout)
		);
	}
	// A help token after a verb is not a help request: there is no help for a
	// command this build does not have.
	let trailing_help = run(&["acp", "--help"]);
	assert_eq!(trailing_help.status.code(), Some(i32::from(EXIT_USAGE)));
	assert!(
		String::from_utf8_lossy(&trailing_help.stderr).contains("`omp acp` is not implemented"),
		"stderr: {}",
		String::from_utf8_lossy(&trailing_help.stderr)
	);

	// `--` keeps the same word as literal prompt text: it reaches the runtime and
	// fails on the missing model config, not on the verb guard.
	let literal = run(&["--print", "--", "acp"]);
	assert_eq!(literal.status.code(), Some(i32::from(EXIT_FAILURE)));
	let stderr = String::from_utf8(literal.stderr).expect("utf8");
	assert!(stderr.contains("no model"), "stderr was {stderr:?}");

	// A `launch` remainder is prompt text for any word — including subcommands and
	// refused verbs — and flags keep parsing on both sides of the operand.
	for args in [
		vec!["launch", "--print", "repl"],
		vec!["launch", "--print", "models"],
		vec!["launch", "--print", "acp"],
		vec!["launch", "repl", "--print"],
	] {
		let output = run(&args);
		assert_eq!(output.status.code(), Some(i32::from(EXIT_FAILURE)), "{args:?}");
		let stderr = String::from_utf8(output.stderr).expect("utf8");
		assert!(stderr.contains("no model"), "{args:?}: {stderr:?}");
	}
	std::fs::remove_dir_all(&root).ok();
}

#[test]
fn models_no_extensions_is_honored_instead_of_silently_dropped() {
	let root = std::env::temp_dir().join(format!("omp-cli-models-noext-{}", std::process::id()));
	let share = root.join("share");
	let work = root.join("work");
	let agent = root.join("config/agent");
	std::fs::create_dir_all(&share).expect("share dir");
	std::fs::create_dir_all(&work).expect("work dir");
	std::fs::create_dir_all(&agent).expect("agent dir");
	// Keyless provider in the agent catalog: listing needs neither network nor key.
	std::fs::write(
		agent.join("models.yml"),
		"providers:\n  local:\n    baseUrl: http://127.0.0.1:20128/v1\n    api: \
		 openai-completions\n    auth: none\n    models:\n      - id: llama3\n",
	)
	.expect("models.yml");
	let run = |args: &[&str]| {
		omp()
			.arg("--cwd")
			.arg(&work)
			.args(args)
			.env("PI_CONFIG_DIR", root.join("config"))
			.env("PI_CODING_AGENT_DIR", &agent)
			.env("OMP_SHARE_DIR", &share)
			.stdin(Stdio::null())
			.output()
			.expect("run omp")
	};

	let plain = run(&["models", "--json"]);
	assert_eq!(
		plain.status.code(),
		Some(i32::from(EXIT_SUCCESS)),
		"stderr: {}",
		String::from_utf8_lossy(&plain.stderr)
	);
	assert!(String::from_utf8_lossy(&plain.stdout).contains("llama3"));

	// Headless builds never discover extensions, so the flag's request already
	// holds: it must be accepted and produce the same file-backed listing.
	let flagged = run(&["models", "--no-extensions", "--json"]);
	assert_eq!(
		flagged.status.code(),
		Some(i32::from(EXIT_SUCCESS)),
		"stderr: {}",
		String::from_utf8_lossy(&flagged.stderr)
	);
	assert_eq!(flagged.stdout, plain.stdout);

	// An explicit extension path is still a hard prerequisite failure: the flag
	// may not downgrade an explicit request into a silent skip.
	let explicit = run(&["models", "--no-extensions", "-e", "ext.ts"]);
	assert_eq!(explicit.status.code(), Some(i32::from(EXIT_FAILURE)));
	let stderr = String::from_utf8(explicit.stderr).expect("utf8");
	assert!(stderr.contains("extension runtime"), "stderr was {stderr:?}");
	assert!(explicit.stdout.is_empty());
	std::fs::remove_dir_all(&root).ok();
}

#[test]
fn excluded_tool_in_tools_selection_is_refused_with_the_boundary_reason() {
	let root = std::env::temp_dir().join(format!("omp-cli-tools-boundary-{}", std::process::id()));
	let share = root.join("share");
	let work = root.join("work");
	let agent = root.join("config/agent");
	std::fs::create_dir_all(&share).expect("share dir");
	std::fs::create_dir_all(&work).expect("work dir");
	std::fs::create_dir_all(&agent).expect("agent dir");
	std::fs::write(
		agent.join("models.yml"),
		"providers:\n  local:\n    baseUrl: http://127.0.0.1:20128/v1\n    api: \
		 openai-completions\n    auth: none\n    models:\n      - id: llama3\n",
	)
	.expect("models.yml");
	let output = omp()
		.args(["--print", "--model", "local/llama3", "--tools", "read,computer"])
		.arg("--cwd")
		.arg(&work)
		.arg("hi")
		.env("PI_CONFIG_DIR", root.join("config"))
		.env("PI_CODING_AGENT_DIR", &agent)
		.env("OMP_SHARE_DIR", &share)
		.stdin(Stdio::null())
		.output()
		.expect("run omp");
	std::fs::remove_dir_all(&root).ok();
	assert_eq!(output.status.code(), Some(i32::from(EXIT_FAILURE)));
	let stderr = String::from_utf8(output.stderr).expect("utf8");
	assert!(stderr.contains("`computer`"), "stderr was {stderr:?}");
	assert!(
		stderr.contains("excluded by the platform boundary"),
		"stderr was {stderr:?}"
	);
	assert!(output.stdout.is_empty());
}

/// Scripted runner: proves `agent::install` is a real seam and that its event
/// stream round-trips through the session log. No network, no model.
struct ScriptedRunner;

impl AgentRunner for ScriptedRunner {
	fn run(&self, _request: &RunRequest) -> Result<EventStream> {
		let mut asst = pi_agent::Message::assistant("ok");
		asst.tool_calls.push(pi_agent::ToolCall {
			id: "call-1".to_string(),
			name: "read".to_string(),
			arguments: serde_json::json!({"path": "a.txt"}),
		});
		let events = vec![
			Event::MessageStart { role: "assistant".to_string() },
			Event::ToolStart {
				id: "call-1".to_string(),
				tool: "read".to_string(),
				args: serde_json::json!({"path": "a.txt"}),
			},
			Event::ToolEnd {
				id: "call-1".to_string(),
				tool: "read".to_string(),
				result: "contents".to_string(),
				error: None,
				images: Vec::new(),
				details: None,
			},
			Event::message_end(asst.clone()),
			Event::turn_end(
				asst.clone(),
				vec![pi_agent::ToolResult {
					tool_call_id: "call-1".to_string(),
					content: "contents".to_string(),
					is_error: false,
					terminate: false,
					details: None,
				}],
				"stop",
			),
			Event::agent_end(vec![asst]),
		];
		Ok(Box::new(events.into_iter().map(Ok::<Event, omp_cli::Error>)))
	}
}

fn fake_request(prompt: &str) -> RunRequest {
	RunRequest {
		prompt: prompt.to_string(),
		parts: Vec::new(),
		cwd: PathBuf::from("/work"),
		output_mode: OutputMode::Text,
		model: None,
		provider: None,
		api_key: None,
		provider_session_id: None,
		prompt_cache_key: None,
		system_prompt: None,
		append_system_prompt: None,
		context_files: Vec::new(),
		skills: Vec::new(),
		history: Vec::new(),
		history_messages: None,
		checkpoint_controller: None,
		turn_ack: None,
		todo_phases: Vec::new(),
		session_persisted: false,
		session_file: None,
		session_id: None,
		config: Config::default(),
		catalog: Arc::new(Catalog::default()),
		approvals: Approvals::default(),
		max_time: None,
		max_turns: 1,
		tools_enabled: false,
		cancellation_token: None,
		extra_tools: Vec::new(),
		internal_urls: None,
		url_scope: None,
		subagent_observer: None,
		child_session_root: None,
	}
}

#[test]
fn fake_runner_stream_is_recorded_and_resumed() {
	assert!(omp_cli::agent::install(Box::new(ScriptedRunner)).is_ok());
	let runner = omp_cli::agent::runner().expect("registered runner");
	let request = fake_request("fix the build");
	let mut builder = TurnBuilder::new(request.prompt.clone());
	for event in runner.run(&request).expect("run") {
		builder.observe(&event.expect("event"));
	}
	let record = builder.finish();
	assert_eq!(record.assistant, "ok");
	assert_eq!(record.stop_reason, "stop");
	assert_eq!(record.tool_calls.len(), 1);

	let dir = std::env::temp_dir().join(format!("omp-cli-session-smoke-{}", std::process::id()));
	std::fs::create_dir_all(&dir).expect("temp session dir");
	let options = SessionOptions {
		enabled: true,
		cwd: PathBuf::from("/work"),
		model: None,
		provider: None,
		dir: Some(dir.clone()),
		id: Some("smoke".to_string()),
		continue_latest: false,
		parent_session: None,
		no_title: false,
	};
	let mut store = SessionStore::open(&options);
	store.append_turn(&record).expect("append");
	assert!(store.take_warnings().is_empty());
	drop(store);

	let resumed = SessionStore::open(&SessionOptions { continue_latest: true, ..options });
	assert_eq!(resumed.id(), "smoke");
	assert_eq!(resumed.history().to_vec(), vec![record]);
	std::fs::remove_dir_all(dir).ok();
}

#[test]
fn interrupted_stream_persists_aborted_turn_and_history_excludes_assistant() {
	let mut builder = TurnBuilder::new("break me");
	let asst = pi_agent::Message::assistant("partial");
	builder.observe(&Event::MessageStart { role: "assistant".to_string() });
	builder.observe(&Event::MessageUpdate { delta: "partial".to_string() });
	builder.observe(&Event::message_end(asst.clone()));
	builder.observe(&Event::turn_end(asst, Vec::new(), "aborted"));
	let record = builder.finish();
	assert_eq!(record.assistant, "partial");
	assert_eq!(record.stop_reason, "aborted");
	assert!(record.tool_calls.is_empty());

	let dir = std::env::temp_dir().join(format!("omp-cli-aborted-smoke-{}", std::process::id()));
	std::fs::create_dir_all(&dir).expect("temp session dir");
	let options = SessionOptions {
		enabled: true,
		cwd: PathBuf::from("/work"),
		model: None,
		provider: None,
		dir: Some(dir.clone()),
		id: Some("aborted_smoke".to_string()),
		continue_latest: false,
		parent_session: None,
		no_title: false,
	};
	let mut store = SessionStore::open(&options);
	store.append_turn(&record).expect("append");
	let history = store.history().to_vec();
	assert_eq!(history.len(), 1);
	assert_eq!(history[0].stop_reason, "aborted");
	drop(store);
	std::fs::remove_dir_all(dir).ok();
}
