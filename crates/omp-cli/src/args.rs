//! `omp` command-line surface.
//!
//! Flag names, aliases and value domains mirror
//! `packages/coding-agent/src/cli/{flag-tables,args}.ts`.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};

use crate::approve::ApprovalMode;
use crate::output::OutputMode;

/// Milliseconds in the supported duration units.
const DURATION_UNITS: [(char, f64); 3] = [('s', 1.0), ('m', 60.0), ('h', 3_600.0)];

#[derive(Debug, Parser)]
#[command(
	name = "omp",
	version,
	about = "oh-my-pi headless coding agent",
	long_about = None,
	disable_help_subcommand = true
)]
pub struct Cli {
	/// Prompt text; repeated positionals run as sequential turns.
	#[arg(value_name = "PROMPT")]
	pub prompt: Vec<String>,

	/// Non-interactive single-shot output (the default when a prompt is given).
	#[arg(short = 'p', long)]
	pub print: bool,

	/// Output format.
	#[arg(long, value_enum, default_value = "text")]
	pub mode: OutputMode,

	/// Working directory for the session.
	#[arg(long, value_name = "DIR")]
	pub cwd: Option<PathBuf>,

	/// Model id (`provider/model` or bare id).
	#[arg(long)]
	pub model: Option<String>,

	/// Provider id.
	#[arg(long)]
	pub provider: Option<String>,

	/// Provider API key (literal, not an env var name).
	#[arg(long = "api-key", value_name = "KEY")]
	pub api_key: Option<String>,

	/// System prompt text, or a path to a file.
	#[arg(long = "system-prompt", value_name = "TEXT|FILE")]
	pub system_prompt: Option<String>,

	/// Text or file appended to the system prompt.
	#[arg(long = "append-system-prompt", value_name = "TEXT|FILE")]
	pub append_system_prompt: Option<String>,

	/// Turn deadline: seconds or a duration such as `5s`, `10m`, `1h`.
	#[arg(long = "max-time", value_name = "DURATION", value_parser = parse_max_time)]
	pub max_time: Option<Duration>,

	/// Disable every tool.
	#[arg(long = "no-tools")]
	pub no_tools: bool,

	/// When to ask before running a tool.
	#[arg(long = "approval-mode", value_enum, value_name = "MODE")]
	pub approval_mode: Option<ApprovalMode>,

	/// Auto-approve every tool (same as `--approval-mode yolo`).
	#[arg(long = "yolo", alias = "auto-approve", conflicts_with = "approval_mode")]
	pub yolo: bool,

	/// Auto-approve a tool by name (repeatable).
	#[arg(long = "allow", value_name = "TOOL")]
	pub allow: Vec<String>,

	/// Additional config overlay (repeatable, later files win).
	#[arg(long, value_name = "PATH")]
	pub config: Vec<PathBuf>,

	/// Resume the newest session in the session directory.
	#[arg(short = 'c', long = "continue")]
	pub continue_session: bool,

	/// Directory holding session logs (default: ~/.omp/agent/sessions/omp-cli).
	#[arg(long = "session-dir", value_name = "DIR")]
	pub session_dir: Option<PathBuf>,

	/// Session id to create or resume.
	#[arg(long = "session-id", value_name = "ID")]
	pub session_id: Option<String>,

	/// Do not read or write session logs.
	#[arg(long = "no-session")]
	pub no_session: bool,

	/// Discover no skills.
	#[arg(long = "no-skills")]
	pub no_skills: bool,

	/// Only load skills matching this glob (comma-separated or repeatable).
	#[arg(long, value_delimiter = ',', value_name = "GLOB")]
	pub skills: Vec<String>,

	#[command(subcommand)]
	pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
	/// Read prompts from stdin until `/exit`.
	Repl,
}

/// Parse `--max-time`: positive seconds or `<number>{s,m,h}`.
pub fn parse_max_time(raw: &str) -> std::result::Result<Duration, String> {
	let text = raw.trim();
	let invalid = || {
		format!(
			"Invalid --max-time value: {raw:?}. Expected a positive number of seconds or \
			 duration like \"5s\", \"10m\", \"1h\"."
		)
	};
	let (number, multiplier) = match text.chars().last() {
		Some(unit) => match DURATION_UNITS
			.iter()
			.find(|(candidate, _)| *candidate == unit)
		{
			Some((_, multiplier)) => (&text[..text.len() - unit.len_utf8()], *multiplier),
			None => (text, 1.0),
		},
		None => return Err(invalid()),
	};
	let value: f64 = number.trim().parse().map_err(|_| invalid())?;
	if !value.is_finite() || value <= 0.0 {
		return Err(invalid());
	}
	let seconds = value * multiplier;
	if !seconds.is_finite() || seconds <= 0.0 {
		return Err(invalid());
	}
	Ok(Duration::from_secs_f64(seconds))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn parse(args: &[&str]) -> Cli {
		Cli::try_parse_from(args).expect("parse")
	}

	#[test]
	fn positional_prompt_and_defaults() {
		let cli = parse(&["omp", "fix", "the", "build"]);
		assert_eq!(cli.prompt, ["fix", "the", "build"]);
		assert!(!cli.print);
		assert_eq!(cli.mode, OutputMode::Text);
		assert_eq!(cli.approval_mode, None);
		assert!(cli.command.is_none());
	}

	#[test]
	fn flags_follow_typescript_names() {
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
			"prompt.md",
			"--append-system-prompt",
			"more",
			"--max-time",
			"10m",
			"--no-tools",
			"--approval-mode",
			"yolo",
			"--allow",
			"read",
			"--allow",
			"bash",
			"--config",
			"a.yml",
			"--config",
			"b.yml",
			"--no-skills",
			"hello",
		]);
		assert!(cli.print);
		assert_eq!(cli.mode, OutputMode::Json);
		assert_eq!(cli.cwd, Some(PathBuf::from("/work")));
		assert_eq!(cli.model.as_deref(), Some("openai/gpt-5"));
		assert_eq!(cli.provider.as_deref(), Some("openai"));
		assert_eq!(cli.api_key.as_deref(), Some("sk-test"));
		assert_eq!(cli.system_prompt.as_deref(), Some("prompt.md"));
		assert_eq!(cli.append_system_prompt.as_deref(), Some("more"));
		assert_eq!(cli.max_time, Some(Duration::from_mins(10)));
		assert!(cli.no_tools);
		assert_eq!(cli.approval_mode, Some(ApprovalMode::Yolo));
		assert_eq!(cli.allow, ["read", "bash"]);
		assert_eq!(cli.config, [PathBuf::from("a.yml"), PathBuf::from("b.yml")]);
		assert!(cli.no_skills);
		assert_eq!(cli.prompt, ["hello"]);
	}

	#[test]
	fn skills_accept_comma_separated_globs() {
		let cli = parse(&["omp", "--skills", "review,test*"]);
		assert_eq!(cli.skills, ["review", "test*"]);
	}

	#[test]
	fn repl_subcommand_and_approval_modes() {
		assert!(matches!(parse(&["omp", "repl"]).command, Some(Command::Repl)));
		for (raw, expected) in [
			("always-ask", ApprovalMode::AlwaysAsk),
			("write", ApprovalMode::Write),
			("yolo", ApprovalMode::Yolo),
		] {
			let cli = parse(&["omp", "--approval-mode", raw]);
			assert_eq!(cli.approval_mode, Some(expected));
		}
		assert!(Cli::try_parse_from(["omp", "--approval-mode", "sometimes"]).is_err());
		assert!(parse(&["omp", "--yolo"]).yolo);
		assert!(parse(&["omp", "--auto-approve"]).yolo);
		assert!(Cli::try_parse_from(["omp", "--yolo", "--approval-mode", "yolo"]).is_err());
	}

	#[test]
	fn max_time_accepts_seconds_and_units() {
		assert_eq!(parse_max_time("5"), Ok(Duration::from_secs(5)));
		assert_eq!(parse_max_time("5s"), Ok(Duration::from_secs(5)));
		assert_eq!(parse_max_time("10m"), Ok(Duration::from_mins(10)));
		assert_eq!(parse_max_time("1h"), Ok(Duration::from_hours(1)));
		for invalid in ["0", "-3", "abc", "1d", ""] {
			assert!(parse_max_time(invalid).is_err(), "{invalid} must fail");
		}
	}
}
