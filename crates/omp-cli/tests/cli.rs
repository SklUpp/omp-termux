//! Integration tests: the argument surface matches the TypeScript CLI, the
//! runner seam accepts an implementation, and the binary fails honestly when
//! its configuration is missing.

use std::path::PathBuf;
use std::process::Command as ProcCommand;
use std::sync::Arc;

use clap::Parser;
use omp_cli::{
	Result,
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
	assert!(Cli::try_parse_from(["omp", "--mode", "rpc"]).is_err());
	assert!(Cli::try_parse_from(["omp", "--approval-mode", "auto"]).is_err());
	assert!(Cli::try_parse_from(["omp", "--unknown-flag"]).is_err());
	assert!(Cli::try_parse_from(["omp", "--max-time", "soon"]).is_err());
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
	assert_eq!(output.status.code(), Some(1));
	let stderr = String::from_utf8(output.stderr).expect("utf8");
	assert!(stderr.contains("no model"), "stderr was {stderr:?}");
	assert!(output.stdout.is_empty());
}

#[test]
fn bad_max_time_is_a_usage_error() {
	let output = omp()
		.args(["--max-time", "soon", "hi"])
		.output()
		.expect("run omp");
	assert!(!output.status.success());
	let stderr = String::from_utf8(output.stderr).expect("utf8");
	assert!(stderr.contains("Invalid --max-time value"), "stderr was {stderr:?}");
}

/// Scripted runner: proves `agent::install` is a real seam and that its event
/// stream round-trips through the session log. No network, no model.
struct ScriptedRunner;

impl AgentRunner for ScriptedRunner {
	fn run(&self, _request: &RunRequest) -> Result<EventStream> {
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
			},
			Event::MessageEnd { text: "ok".to_string() },
			Event::TurnEnd { stop_reason: "end_turn".to_string() },
			Event::AgentEnd { text: "ok".to_string() },
		];
		Ok(Box::new(events.into_iter().map(Ok::<Event, omp_cli::Error>)))
	}
}

fn fake_request(prompt: &str) -> RunRequest {
	RunRequest {
		prompt: prompt.to_string(),
		cwd: PathBuf::from("/work"),
		output_mode: OutputMode::Text,
		model: None,
		provider: None,
		api_key: None,
		system_prompt: None,
		append_system_prompt: None,
		context_files: Vec::new(),
		skills: Vec::new(),
		history: Vec::new(),
		config: Config::default(),
		catalog: Arc::new(Catalog::default()),
		approvals: Approvals::default(),
		max_time: None,
		max_turns: 1,
		tools_enabled: false,
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
	assert_eq!(record.stop_reason, "end_turn");
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
