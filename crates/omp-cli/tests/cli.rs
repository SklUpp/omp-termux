//! Integration tests: the argument surface matches the TypeScript CLI, the
//! runner seam accepts an implementation, and the binary fails honestly when
//! its configuration is missing.

use std::process::Command as ProcCommand;

use clap::Parser;
use omp_cli::{
	Result,
	agent::{AgentRunner, EventStream, RunRequest},
	approve::ApprovalMode,
	args::{Cli, Command},
	output::OutputMode,
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

/// Fake runner proving `agent::install` is a real seam (no network, no model).
struct NoopRunner;

impl AgentRunner for NoopRunner {
	fn run(&self, _request: &RunRequest) -> Result<EventStream> {
		Ok(Box::new(std::iter::empty()))
	}
}

#[test]
fn injected_runner_is_visible_through_the_seam() {
	assert!(omp_cli::agent::install(Box::new(NoopRunner)).is_ok());
	assert!(omp_cli::agent::runner().is_some());
}
