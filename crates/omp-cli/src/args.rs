//! `omp` command-line surface.
//!
//! Flag names, aliases and value domains mirror
//! `packages/coding-agent/src/cli/{flag-tables,args}.ts`.

use std::path::PathBuf;
use std::time::Duration;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};

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
struct RawCli {
	/// Prompt text; repeated positionals run as sequential turns.
	#[arg(value_name = "PROMPT")]
	prompt: Vec<String>,

	/// Operands given after the `--` end-of-options marker.
	#[arg(last = true, allow_hyphen_values = true, hide = true)]
	trailing: Vec<String>,

	/// Non-interactive single-shot output (the default when a prompt is given).
	#[arg(short = 'p', long)]
	print: bool,

	/// Output format.
	#[arg(long, value_enum, default_value = "text")]
	mode: OutputMode,

	/// Working directory for the session.
	#[arg(long, value_name = "DIR")]
	cwd: Option<PathBuf>,

	/// Isolated profile for auth, sessions, settings, and caches.
	#[arg(long, value_name = "NAME")]
	profile: Option<String>,

	/// Create a shell shortcut for the selected profile and exit.
	#[arg(long, value_name = "NAME")]
	alias: Option<String>,

	/// Model id (`provider/model` or bare id).
	#[arg(long)]
	model: Option<String>,

	/// Provider id.
	#[arg(long)]
	provider: Option<String>,

	/// Provider API key (literal, not an env var name).
	#[arg(long = "api-key", value_name = "KEY")]
	api_key: Option<String>,

	/// System prompt text, or a path to a file.
	#[arg(long = "system-prompt", value_name = "TEXT|FILE")]
	system_prompt: Option<String>,

	/// Text or file appended to the system prompt.
	#[arg(long = "append-system-prompt", value_name = "TEXT|FILE")]
	append_system_prompt: Option<String>,

	/// Turn deadline: seconds or a duration such as `5s`, `10m`, `1h`.
	#[arg(long = "max-time", value_name = "DURATION", value_parser = parse_max_time)]
	max_time: Option<Duration>,

	/// Disable every tool.
	#[arg(long = "no-tools")]
	no_tools: bool,

	/// Only enable these tools (comma-separated or repeatable).
	#[arg(long, value_delimiter = ',', value_name = "TOOLS")]
	tools: Option<Vec<String>>,

	/// When to ask before running a tool.
	#[arg(long = "approval-mode", value_enum, value_name = "MODE")]
	approval_mode: Option<ApprovalMode>,

	/// Auto-approve every tool (same as `--approval-mode yolo`).
	#[arg(long = "yolo", alias = "auto-approve", conflicts_with = "approval_mode")]
	yolo: bool,

	/// Auto-approve a tool by name (repeatable).
	#[arg(long = "allow", value_name = "TOOL")]
	allow: Vec<String>,

	/// Additional config overlay (repeatable, later files win).
	#[arg(long, value_name = "PATH")]
	config: Vec<PathBuf>,

	/// Resume the newest session in the session directory.
	#[arg(short = 'c', long = "continue")]
	continue_session: bool,

	/// Resume a session (by ID prefix, path, or interactive picker if omitted).
	#[arg(
		short = 'r',
		long = "resume",
		alias = "session",
		num_args = 0..=1,
		default_missing_value = "",
		value_name = "SELECTOR"
	)]
	resume: Option<String>,

	/// Fork an existing session into a new session log.
	#[arg(long = "fork", value_name = "SELECTOR")]
	fork: Option<String>,

	/// Import a Claude Code session into OMP.
	#[arg(long = "from-claude")]
	from_claude: bool,

	/// Import an OpenAI Codex session into OMP.
	#[arg(long = "from-codex")]
	from_codex: bool,

	/// Provider-specific session ID.
	#[arg(long = "provider-session-id", value_name = "ID")]
	provider_session_id: Option<String>,

	/// Provider prompt cache key.
	#[arg(long = "prompt-cache-key", value_name = "KEY")]
	prompt_cache_key: Option<String>,

	/// Disable session title generation.
	#[arg(long = "no-title")]
	no_title: bool,

	/// Directory holding session logs (default: ~/.omp/agent/sessions/omp-cli).
	#[arg(long = "session-dir", value_name = "DIR")]
	session_dir: Option<PathBuf>,

	/// Session id to create or resume.
	#[arg(long = "session-id", value_name = "ID")]
	session_id: Option<String>,

	/// Do not read or write session logs.
	#[arg(long = "no-session")]
	no_session: bool,

	/// Discover no skills.
	#[arg(long = "no-skills")]
	no_skills: bool,

	/// Only load skills matching this glob (comma-separated or repeatable).
	#[arg(long, value_delimiter = ',', value_name = "GLOB")]
	skills: Vec<String>,

	/// Internal marker inserted by the profile bootstrap; never user-facing.
	#[arg(long = "omp-profile-boundary", hide = true)]
	profile_boundary: bool,

	#[command(subcommand)]
	command: Option<Command>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cli {
	/// Prompt text; repeated positionals run as sequential turns.
	pub prompt: Vec<String>,

	/// Attached file arguments (`@file`).
	pub file_args: Vec<String>,

	/// Non-interactive single-shot output (the default when a prompt is given).
	pub print: bool,

	/// Output format.
	pub mode: OutputMode,

	/// Working directory for the session.
	pub cwd: Option<PathBuf>,

	/// Isolated profile for auth, sessions, settings, and caches.
	pub profile: Option<String>,

	/// Create a shell shortcut for the selected profile and exit.
	pub alias: Option<String>,

	/// Model id (`provider/model` or bare id).
	pub model: Option<String>,

	/// Provider id.
	pub provider: Option<String>,

	/// Provider API key (literal, not an env var name).
	pub api_key: Option<String>,

	/// System prompt text, or a path to a file.
	pub system_prompt: Option<String>,

	/// Text or file appended to the system prompt.
	pub append_system_prompt: Option<String>,

	/// Turn deadline: seconds or a duration such as `5s`, `10m`, `1h`.
	pub max_time: Option<Duration>,

	/// Disable every tool.
	pub no_tools: bool,

	/// Only enable these tools (comma-separated or repeatable).
	pub tools: Option<Vec<String>>,

	/// When to ask before running a tool.
	pub approval_mode: Option<ApprovalMode>,

	/// Auto-approve every tool (same as `--approval-mode yolo`).
	pub yolo: bool,

	/// Auto-approve a tool by name (repeatable).
	pub allow: Vec<String>,

	/// Additional config overlay (repeatable, later files win).
	pub config: Vec<PathBuf>,

	/// Resume the newest session in the session directory.
	pub continue_session: bool,

	/// Resume a session (by ID prefix, path, or interactive picker if omitted).
	pub resume: Option<String>,

	/// Fork an existing session into a new session log.
	pub fork: Option<String>,

	/// Import a Claude Code session into OMP.
	pub from_claude: bool,

	/// Import an OpenAI Codex session into OMP.
	pub from_codex: bool,

	/// Provider-specific session ID.
	pub provider_session_id: Option<String>,

	/// Provider prompt cache key.
	pub prompt_cache_key: Option<String>,

	/// Disable session title generation.
	pub no_title: bool,

	/// Directory holding session logs (default: ~/.omp/agent/sessions/omp-cli).
	pub session_dir: Option<PathBuf>,

	/// Session id to create or resume.
	pub session_id: Option<String>,

	/// Do not read or write session logs.
	pub no_session: bool,

	/// Discover no skills.
	pub no_skills: bool,

	/// Only load skills matching this glob (comma-separated or repeatable).
	pub skills: Vec<String>,

	pub command: Option<Command>,
}

/// Parse an `@file` positional token into a file path according to TS CLI parity.
///
/// Strips leading `@` and, if enclosed in matching quotes (`"` or `'`), strips outer quotes.
#[must_use]
pub fn parse_file_arg(token: &str) -> String {
	let raw_path = token.strip_prefix('@').unwrap_or(token);
	if raw_path.len() > 1 && raw_path.starts_with('"') && raw_path.ends_with('"') {
		raw_path[1..raw_path.len() - 1].to_string()
	} else if raw_path.len() > 1 && raw_path.starts_with('\'') && raw_path.ends_with('\'') {
		raw_path[1..raw_path.len() - 1].to_string()
	} else {
		raw_path.to_string()
	}
}

impl Cli {
	fn from_raw(raw: RawCli) -> Self {
		let _ = raw.profile_boundary;
		let mut prompt = Vec::new();
		let mut file_args = Vec::new();

		for token in raw.prompt {
			if token.starts_with('@') {
				file_args.push(parse_file_arg(&token));
			} else {
				prompt.push(token);
			}
		}

		// Arguments after `--` are literal positional text (POSIX end-of-options).
		// Everything after `--` remains a prompt message, even if starting with `@`.
		prompt.extend(raw.trailing);

		Self {
			prompt,
			file_args,
			print: raw.print,
			mode: raw.mode,
			cwd: raw.cwd,
			profile: raw.profile,
			alias: raw.alias,
			model: raw.model,
			provider: raw.provider,
			api_key: raw.api_key,
			system_prompt: raw.system_prompt,
			append_system_prompt: raw.append_system_prompt,
			max_time: raw.max_time,
			no_tools: raw.no_tools,
			tools: raw.tools,
			approval_mode: raw.approval_mode,
			yolo: raw.yolo,
			allow: raw.allow,
			config: raw.config,
			continue_session: raw.continue_session,
			resume: raw.resume,
			fork: raw.fork,
			from_claude: raw.from_claude,
			from_codex: raw.from_codex,
			provider_session_id: raw.provider_session_id,
			prompt_cache_key: raw.prompt_cache_key,
			no_title: raw.no_title,
			session_dir: raw.session_dir,
			session_id: raw.session_id,
			no_session: raw.no_session,
			no_skills: raw.no_skills,
			skills: raw.skills,
			command: raw.command,
		}
	}
}

impl CommandFactory for Cli {
	fn command() -> clap::Command {
		RawCli::command()
	}

	fn command_for_update() -> clap::Command {
		RawCli::command_for_update()
	}
}

impl FromArgMatches for Cli {
	fn from_arg_matches(matches: &clap::ArgMatches) -> std::result::Result<Self, clap::Error> {
		let raw = RawCli::from_arg_matches(matches)?;
		Ok(Self::from_raw(raw))
	}

	fn from_arg_matches_mut(matches: &mut clap::ArgMatches) -> std::result::Result<Self, clap::Error> {
		let raw = RawCli::from_arg_matches_mut(matches)?;
		Ok(Self::from_raw(raw))
	}

	fn update_from_arg_matches(&mut self, matches: &clap::ArgMatches) -> std::result::Result<(), clap::Error> {
		let new = Self::from_arg_matches(matches)?;
		*self = new;
		Ok(())
	}

	fn update_from_arg_matches_mut(
		&mut self,
		matches: &mut clap::ArgMatches,
	) -> std::result::Result<(), clap::Error> {
		let new = Self::from_arg_matches_mut(matches)?;
		*self = new;
		Ok(())
	}
}

impl Parser for Cli {}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum Command {
	/// Read prompts from stdin until `/exit`.
	Repl,

	/// Print a shell completion script (bash, zsh, or fish).
	Completions {
		/// Target shell.
		shell: Option<String>,
	},

	/// Emit dynamic completion candidates (hidden helper used by the scripts).
	#[command(hide = true, name = "__complete", disable_help_flag = true)]
	Complete {
		/// Candidate kind (`models` or `sessions`) and an optional prefix.
		#[arg(value_name = "ARGS", allow_hyphen_values = true, hide = true)]
		args: Vec<String>,
	},

	/// Inspect or unpack the bundled agent definitions.
	Agents {
		/// Action to run (`unpack`); omitted prints help without writing.
		#[arg(value_name = "ACTION")]
		action:      Option<String>,
		/// Unpack into `<project>/<DIR>`.
		#[arg(long, value_name = "DIR")]
		dir:         Option<PathBuf>,
		/// Unpack into the user agent directory.
		#[arg(long)]
		user:        bool,
		/// Unpack into `<project>/.omp/agents`.
		#[arg(long)]
		project:     bool,
		/// Overwrite existing files.
		#[arg(short = 'f', long)]
		force:       bool,
		/// Print machine-readable JSON.
		#[arg(long)]
		json:        bool,
	},

	/// List and search available models.
	Models {
		/// `ls` (default) | `list` | `find` | `refresh` | `<provider>`.
		#[arg(value_name = "ACTION")]
		action:        Option<String>,
		/// Search substring, provider name, or `ls` filter.
		#[arg(value_name = "PATTERN")]
		pattern:       Option<String>,
		/// Print machine-readable JSON.
		#[arg(long)]
		json:          bool,
		/// Load an extension file before listing (repeatable).
		#[arg(short = 'e', long = "extension", value_name = "PATH")]
		extension:     Vec<PathBuf>,
		/// Disable extension discovery (headless builds never discover).
		#[arg(long = "no-extensions")]
		no_extensions: bool,
		/// Load an extra config.yml-style overlay for this run (repeatable).
		#[arg(long, value_name = "PATH")]
		config:        Vec<PathBuf>,
	},

	/// Show what the read tool returns for a path or selector.
	Read {
		/// Path, optional selector, URL, or internal URI.
		#[arg(value_name = "PATH")]
		path: Option<String>,
	},

	/// Search files with the same engine and output modes as the grep tool.
	Grep {
		/// Regex pattern to search for.
		#[arg(value_name = "PATTERN")]
		pattern:      Option<String>,
		/// Directory or file to search (default: `.`).
		#[arg(value_name = "PATH")]
		path:         Option<String>,
		/// Filter files by glob pattern.
		#[arg(short = 'g', long, value_name = "GLOB")]
		glob:         Option<String>,
		/// Maximum matches to emit (default: 20).
		#[arg(short = 'l', long, default_value_t = 20, value_name = "N")]
		limit:        u32,
		/// Context lines before and after each match (default: 2).
		#[arg(short = 'C', long, default_value_t = 2, value_name = "N")]
		context:      u32,
		/// Output matched file names only.
		#[arg(short = 'f', long)]
		files:        bool,
		/// Output per-file match counts.
		#[arg(short = 'c', long)]
		count:        bool,
		/// Include files excluded by `.gitignore`.
		#[arg(long = "no-gitignore")]
		no_gitignore: bool,
	},
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
	fn positional_file_args_separated_from_prompt() {
		let cli = parse(&["omp", "@first.rs", "explain this", "@second.rs", "and that"]);
		assert_eq!(cli.file_args, ["first.rs", "second.rs"]);
		assert_eq!(cli.prompt, ["explain this", "and that"]);
	}

	#[test]
	fn positional_file_args_strip_outer_quotes_according_to_ts() {
		let cli = parse(&[
			"omp",
			r#"@"path with spaces/file.txt""#,
			r#"@'another space/path.txt'"#,
			r#"@"quotes'inside.rs""#,
			r#"@'double"inside.rs'"#,
			r#"@"unclosed.rs"#,
			r#"@unclosed""#,
			"@",
			r#"@"""#,
			r#"@''"#,
			r#"@"a""#,
			r#"@""#,
			r#"@'"#,
			"@@double.rs",
		]);
		assert_eq!(
			cli.file_args,
			[
				"path with spaces/file.txt",
				"another space/path.txt",
				"quotes'inside.rs",
				r#"double"inside.rs"#,
				r#""unclosed.rs"#,
				r#"unclosed""#,
				"",
				"",
				"",
				"a",
				r#"""#,
				"'",
				"@double.rs",
			]
		);
		assert!(cli.prompt.is_empty());
	}

	#[test]
	fn posix_separator_keeps_at_prefixed_tokens_as_prompts() {
		let cli = parse(&["omp", "@before.rs", "first", "--", "@after.rs", "second", "--model", "opus"]);
		assert_eq!(cli.file_args, ["before.rs"]);
		assert_eq!(cli.prompt, ["first", "@after.rs", "second", "--model", "opus"]);
		assert!(cli.model.is_none());

		let bare_separator = parse(&["omp", "--", "hello"]);
		assert_eq!(bare_separator.prompt, ["hello"]);
		assert!(bare_separator.file_args.is_empty());
	}

	#[test]
	fn option_values_starting_with_at_are_not_intercepted_as_file_args() {
		let cli = parse(&[
			"omp",
			"--model",
			"@custom-model",
			"--append-system-prompt",
			"@system.md",
			"--allow",
			"@bash",
			"--allow",
			"@read",
			"--resume",
			"@session-id",
			"@actual_file.rs",
			"prompt text",
		]);
		assert_eq!(cli.model.as_deref(), Some("@custom-model"));
		assert_eq!(cli.append_system_prompt.as_deref(), Some("@system.md"));
		assert_eq!(cli.allow, ["@bash", "@read"]);
		assert_eq!(cli.resume.as_deref(), Some("@session-id"));
		assert_eq!(cli.file_args, ["actual_file.rs"]);
		assert_eq!(cli.prompt, ["prompt text"]);
	}

	#[test]
	fn lone_hyphen_remains_prompt_message() {
		let cli = parse(&["omp", "@file.rs", "-"]);
		assert_eq!(cli.file_args, ["file.rs"]);
		assert_eq!(cli.prompt, ["-"]);
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
	fn profile_and_alias_flags_parse() {
		let cli = parse(&["omp", "--profile", "work", "--alias", "wk", "hello"]);
		assert_eq!(cli.profile.as_deref(), Some("work"));
		assert_eq!(cli.alias.as_deref(), Some("wk"));
		assert_eq!(cli.prompt, ["hello"]);
		assert!(Cli::try_parse_from(["omp", "--profile"]).is_err());
		assert!(Cli::try_parse_from(["omp", "--alias"]).is_err());
	}

	#[test]
	fn bootstrap_boundary_keeps_the_next_token_positional() {
		let cli = parse(&["omp", "--resume", "--omp-profile-boundary", "work"]);
		assert_eq!(cli.resume.as_deref(), Some(""));
		assert_eq!(cli.prompt, ["work"]);
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
	fn completion_subcommands_parse() {
		assert_eq!(
			parse(&["omp", "completions", "bash"]).command,
			Some(Command::Completions { shell: Some("bash".to_string()) })
		);
		assert_eq!(
			parse(&["omp", "completions"]).command,
			Some(Command::Completions { shell: None })
		);
		let Some(Command::Complete { args }) = parse(&["omp", "__complete", "models", "--", "gp"]).command
		else {
			panic!("__complete must parse as the hidden completion helper");
		};
		let args: Vec<&str> =
			args.iter().map(String::as_str).filter(|arg| *arg != "--").collect();
		assert_eq!(args, ["models", "gp"]);
	}

	#[test]
	fn agents_subcommand_parses_unpack_flags() {
		assert_eq!(
			parse(&["omp", "agents"]).command,
			Some(Command::Agents {
				action:  None,
				dir:     None,
				user:    false,
				project: false,
				force:   false,
				json:    false,
			})
		);
		assert_eq!(
			parse(&["omp", "agents", "unpack", "--project", "--force", "--json"]).command,
			Some(Command::Agents {
				action:  Some("unpack".to_string()),
				dir:     None,
				user:    false,
				project: true,
				force:   true,
				json:    true,
			})
		);
		assert_eq!(
			parse(&["omp", "agents", "unpack", "--dir", "tmp/agents"]).command,
			Some(Command::Agents {
				action:  Some("unpack".to_string()),
				dir:     Some(PathBuf::from("tmp/agents")),
				user:    false,
				project: false,
				force:   false,
				json:    false,
			})
		);
	}

	#[test]
	fn read_and_grep_subcommands_parse() {
		assert_eq!(
			parse(&["omp", "read", "src/foo.ts:50-100"]).command,
			Some(Command::Read { path: Some("src/foo.ts:50-100".to_string()) })
		);
		// Missing operands reach the command bodies so they exit 1 with the
		// TS CLI's message instead of clap's exit 2.
		assert_eq!(parse(&["omp", "read"]).command, Some(Command::Read { path: None }));
		assert_eq!(
			parse(&["omp", "grep"]).command,
			Some(Command::Grep {
				pattern:      None,
				path:         None,
				glob:         None,
				limit:        20,
				context:      2,
				files:        false,
				count:        false,
				no_gitignore: false,
			})
		);
		assert_eq!(
			parse(&[
				"omp", "grep", "TODO", "src", "-g", "*.rs", "-l", "5", "-C", "3", "-f", "-c",
				"--no-gitignore",
			])
			.command,
			Some(Command::Grep {
				pattern:      Some("TODO".to_string()),
				path:         Some("src".to_string()),
				glob:         Some("*.rs".to_string()),
				limit:        5,
				context:      3,
				files:        true,
				count:        true,
				no_gitignore: true,
			})
		);
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
