//! `omp` command-line surface.
//!
//! Flag names, aliases and value domains mirror
//! `packages/coding-agent/src/cli/{flag-tables,args}.ts`.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};

use crate::approve::ApprovalMode;
use crate::output::OutputMode;

/// Milliseconds in the supported duration units.
const DURATION_UNITS: [(char, f64); 3] = [('s', 1.0), ('m', 60.0), ('h', 3_600.0)];

/// Every option of the launch surface, shared by the two parsing surfaces:
/// `RawCli` (root, with subcommands) and `LaunchCli` (no subcommands).
#[derive(Debug, Args)]
struct LaunchArgs {
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

	/// Thinking level: off, minimal, low, medium, high, xhigh, max, or auto.
	#[arg(long, value_name = "LEVEL")]
	thinking: Option<String>,

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
}

/// Root parsing surface: the launch options plus the optional subcommand.
#[derive(Debug, Parser)]
#[command(
	name = "omp",
	version,
	about = "oh-my-pi headless coding agent",
	long_about = None,
	disable_help_subcommand = true
)]
struct RawCli {
	#[command(flatten)]
	launch: LaunchArgs,

	#[command(subcommand)]
	command: Option<Command>,
}

/// The `launch` surface: the same options with no subcommands at all, so every
/// operand is prompt text and nothing can dispatch.
#[derive(Debug, Parser)]
#[command(name = "omp", version)]
struct LaunchCli {
	#[command(flatten)]
	launch: LaunchArgs,
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

	/// Thinking level: off, minimal, low, medium, high, xhigh, max, or auto.
	pub thinking: Option<String>,

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

/// Whether `value` is a canonical 8-4-4-4-12 session UUID, matching TS
/// `SESSION_ID_ARG_RE` (ASCII hex, case-insensitive, exact length).
#[must_use]
pub fn is_session_uuid(value: &str) -> bool {
	let bytes = value.as_bytes();
	bytes.len() == 36
		&& bytes.iter().enumerate().all(|(index, byte)| match index {
			8 | 13 | 18 | 23 => *byte == b'-',
			_ => byte.is_ascii_hexdigit(),
		})
}

/// TS `normalizeContinueSessionArgs`: `--continue <session-uuid>` targets that
/// session instead of continuing the latest one with the UUID as prompt text.
/// An explicit `--resume`/`--fork` wins, and a non-UUID prompt is untouched.
pub fn normalize_continue_session_args(cli: &mut Cli) {
	if !cli.continue_session
		|| cli.resume.is_some()
		|| cli.fork.is_some()
		|| cli.prompt.len() != 1
	{
		return;
	}
	let message = cli.prompt[0].trim();
	if !is_session_uuid(message) {
		return;
	}
	cli.resume = Some(message.to_string());
	cli.continue_session = false;
	cli.prompt.clear();
}

impl Cli {
	fn from_launch(raw: LaunchArgs, command: Option<Command>) -> Self {
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
			thinking: raw.thinking,
			no_title: raw.no_title,
			session_dir: raw.session_dir,
			session_id: raw.session_id,
			no_session: raw.no_session,
			no_skills: raw.no_skills,
			skills: raw.skills,
			command,
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
		Ok(Self::from_launch(raw.launch, raw.command))
	}

	fn from_arg_matches_mut(matches: &mut clap::ArgMatches) -> std::result::Result<Self, clap::Error> {
		let raw = RawCli::from_arg_matches_mut(matches)?;
		Ok(Self::from_launch(raw.launch, raw.command))
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

/// Known upstream verbs (`packages/coding-agent/src/cli-commands.ts`) that this
/// binary refuses before prompt assembly because they are approved exclusions.
/// The reason is user-facing: an excluded verb must not read like a typo.
const EXCLUDED_VERBS: [(&str, &str); 6] = [
	("git", "the full-screen git TUI is outside the approved boundary; run `git` directly"),
	("install", "plugin and extension installation is outside the approved boundary"),
	("join", "the full-screen join TUI and live WebRTC are outside the approved boundary"),
	("plugin", "plugin and extension execution is outside the approved boundary"),
	(
		"shell",
		"the interactive full-screen shell is outside the approved boundary; run `bash` directly",
	),
	("update", "there is no Android release target to update"),
];

/// Known upstream verbs this binary does not implement yet, including the
/// upstream registry aliases `img` and `q`.
const MISSING_VERBS: [&str; 27] = [
	"acp",
	"auth-broker",
	"auth-gateway",
	"bench",
	"browser-relay",
	"cleanse",
	"commit",
	"compress",
	"dry-balance",
	"gc",
	"gallery",
	"grievances",
	"images",
	"img",
	"if-bench",
	"say",
	"share",
	"setup",
	"render",
	"ssh",
	"stats",
	"usage",
	"tiny-models",
	"token",
	"ttsr",
	"search",
	"q",
];

/// Refusal for a known upstream verb that would otherwise be absorbed as prompt
/// text, or `None` when argv is not such an invocation.
///
/// `omp acp` currently parses as the prompt "acp": there is no such clap
/// subcommand, so `main` falls through to the agent runtime and starts config,
/// session, provider, and tool setup for text the user meant as a command.
/// Scanning mirrors the TypeScript `leadingSubcommandIndex`: leading launch flags
/// and the values they consume are skipped (the value category comes from the
/// clap definition, so it cannot drift from the real surface, including combined
/// short flags and attached values), the scan stops at `--` and at `-` (POSIX
/// literal operands), and only exact verb matches refuse. Help and version
/// requests stay with clap's own actions, and arbitrary prompt text — including a
/// first word that merely resembles a verb — passes through untouched.
pub fn refused_verb_message(argv: &[std::ffi::OsString]) -> Option<String> {
	let command = RawCli::command();
	if requests_help_or_version(&command, argv, 1) {
		return None;
	}
	let verb = verb_slot(&command, argv)?;
	if let Some((_, reason)) = EXCLUDED_VERBS.iter().find(|(name, _)| *name == verb) {
		return Some(format!(
			"`omp {verb}` is excluded from this build: {reason}. Nothing was started. Pass `--` \
			 before it to send the word as prompt text: `omp -- {verb}`."
		));
	}
	if MISSING_VERBS.contains(&verb) {
		return Some(format!(
			"`omp {verb}` is not implemented in this binary yet; nothing was started. Run `omp \
			 --help` for the commands it does provide, or pass `--` before it to send the word as \
			 prompt text: `omp -- {verb}`."
		));
	}
	None
}

/// Leading `--help`/`-h`/`--version`/`-V` (also inside a leading short-flag
/// cluster such as `-ph`) belong to clap's help and version actions, so a known
/// verb after them is not refused. The scan stops at the first operand: once a
/// verb or prompt leads, a later `--help` is not a help request (`omp acp --help`
/// refuses `acp`, which this build cannot help with).
///
/// Flag values are skipped with the same clap-derived contract as the verb scan:
/// a `--help` used as a value (`--system-prompt --help`, `-rh`) is not a request,
/// so the verb after it still refuses, while a later token can still be one
/// (`-rh --help acp` shows help).
fn requests_help_or_version(
	command: &clap::Command,
	argv: &[std::ffi::OsString],
	mut index: usize,
) -> bool {
	while index < argv.len() {
		let Some(text) = argv[index].to_str() else {
			return false;
		};
		if text == "--" || text == "-" || !text.starts_with('-') {
			return false; // the leading flag region is over
		}
		if text == "--help" || text == "--version" {
			return true;
		}
		// An attached cluster value ends that flag, not the scan: a later token can
		// still be the help request (`-rh --help acp`).
		if !text.starts_with("--") && short_cluster_help(command, text) {
			return true;
		}
		let next = argv.get(index + 1).and_then(|next| next.to_str());
		index += if flag_consumes_value(command, text, next) { 2 } else { 1 };
	}
	false
}

/// Whether one short-flag token (`-ph`, `-rh`) carries a help/version switch.
/// A switch behind a value-taking flag is that flag's value, not a request
/// (`-rh` is `--resume h`).
fn short_cluster_help(command: &clap::Command, flag: &str) -> bool {
	for short in flag.trim_start_matches('-').chars() {
		let Some(arg) = short_flag(command, short) else {
			return false;
		};
		if !matches!(flag_value_kind(arg), FlagValueKind::Valueless) {
			return false;
		}
		if short == 'h' || short == 'V' {
			return true;
		}
	}
	false
}

/// Parse argv, giving a leading exact `launch` command its own surface.
///
/// `launch` is the TypeScript default command: every token after it is launch
/// surface, so its first operand is prompt text and can never dispatch as a
/// subcommand (`omp launch repl` prompts `repl`, `omp launch acp` prompts `acp`
/// instead of refusing it). That surface is [`LaunchCli`]: the same [`LaunchArgs`]
/// options flattened into a command that declares no subcommands at all, so
/// globals before `launch`, flags around the operand, flag values, and literal
/// `--` all keep their exact meaning. A help/version request in the leading flag
/// region, or at the head of the launch remainder, stays on the root surface so
/// clap prints the real help text (`omp launch --help`, `omp --help launch`).
///
/// Without a leading `launch` this is exactly [`Cli::parse_from`].
pub fn parse_cli(argv: Vec<std::ffi::OsString>) -> Cli {
	let command = RawCli::command();
	if requests_help_or_version(&command, &argv, 1) {
		return parse_matches(command, argv);
	}
	let launch = launch_slot(&command, &argv);
	if let Some(index) = launch {
		if requests_help_or_version(&command, &argv, index + 1) {
			let mut root_argv = Vec::with_capacity(argv.len() - 1);
			root_argv.extend_from_slice(&argv[..index]);
			root_argv.extend_from_slice(&argv[index + 1..]);
			return parse_matches(command, root_argv);
		}
		let mut launch_argv = Vec::with_capacity(argv.len() - 1);
		launch_argv.extend_from_slice(&argv[..index]);
		launch_argv.extend_from_slice(&argv[index + 1..]);
		return parse_launch(launch_argv);
	}
	parse_matches(command, argv)
}

/// Exact leading `launch` command slot, or `None`.
fn launch_slot(command: &clap::Command, argv: &[std::ffi::OsString]) -> Option<usize> {
	let index = verb_slot_index(command, argv)?;
	(argv[index].to_str()? == "launch").then_some(index)
}

/// Parse the launch surface: the shared options, no subcommand slot.
fn parse_launch(argv: Vec<std::ffi::OsString>) -> Cli {
	let matches = LaunchCli::command()
		.try_get_matches_from(argv)
		.unwrap_or_else(|error| error.exit());
	let launch = LaunchCli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());
	Cli::from_launch(launch.launch, None)
}

fn parse_matches(command: clap::Command, argv: Vec<std::ffi::OsString>) -> Cli {
	let matches = command
		.try_get_matches_from(argv)
		.unwrap_or_else(|error| error.exit());
	Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit())
}

/// Text of the first positional token on the launch surface, if any.
fn verb_slot<'a>(command: &clap::Command, argv: &'a [std::ffi::OsString]) -> Option<&'a str> {
	verb_slot_index(command, argv).and_then(|index| argv[index].to_str())
}

/// Index of the first positional token on the launch surface, skipping flags and
/// the values they consume.
fn verb_slot_index(command: &clap::Command, argv: &[std::ffi::OsString]) -> Option<usize> {
	positional_index(command, argv, 1)
}

/// Index of the first positional token at or after `start`.
fn positional_index(
	command: &clap::Command,
	argv: &[std::ffi::OsString],
	mut index: usize,
) -> Option<usize> {
	while index < argv.len() {
		let text = argv[index].to_str()?;
		if text == "--" || text == "-" {
			return None;
		}
		if !text.starts_with('-') {
			return Some(index);
		}
		let next = argv.get(index + 1).and_then(|next| next.to_str());
		index += if flag_consumes_value(command, text, next) { 2 } else { 1 };
	}
	None
}

/// How a flag on the clap surface treats the token after it.
enum FlagValueKind {
	/// Boolean switch: consumes nothing.
	Valueless,
	/// `num_args(0..=1)` such as `--resume [<selector>]`: consumes a value-like token.
	Optional,
	/// Required value: consumes the next token even when it looks like a flag.
	Always,
}

/// Whether a flag consumes the next token.
///
/// The category comes from the clap definition. Unknown flags are clap's error,
/// but a value-like successor is still protected so it cannot be mistaken for a
/// verb.
fn flag_consumes_value(command: &clap::Command, flag: &str, next: Option<&str>) -> bool {
	let value_like = next.is_some_and(|next| !next.starts_with('-'));
	if flag.contains('=') {
		return false; // inline `--flag=value`
	}
	if let Some(long) = flag.strip_prefix("--") {
		let Some(arg) = long_flag(command, long) else {
			return value_like;
		};
		return match flag_value_kind(arg) {
			FlagValueKind::Valueless => false,
			FlagValueKind::Optional => value_like,
			FlagValueKind::Always => true,
		};
	}
	let Some(body) = flag.strip_prefix('-') else {
		return value_like;
	};
	// Combined short flags: valueless flags continue the cluster; the first
	// value-taking one takes the rest of the cluster as its attached value
	// (`-rc` is `--resume c`), otherwise it takes the next token (`-pr acp`).
	let mut chars = body.chars();
	while let Some(short) = chars.next() {
		let Some(arg) = short_flag(command, short) else {
			return value_like;
		};
		let kind = flag_value_kind(arg);
		if matches!(kind, FlagValueKind::Valueless) {
			continue;
		}
		if !chars.as_str().is_empty() {
			return false;
		}
		return matches!(kind, FlagValueKind::Always) || value_like;
	}
	false
}

/// Flag on the clap surface named `name`, through its long name or any alias.
fn long_flag<'a>(command: &'a clap::Command, name: &str) -> Option<&'a clap::Arg> {
	command.get_arguments().find(|arg| {
		arg.get_long().is_some_and(|long| long == name)
			|| arg.get_all_aliases().into_iter().flatten().any(|alias| alias == name)
	})
}

/// Flag on the clap surface named `short`, through its short name or any alias.
fn short_flag(command: &clap::Command, short: char) -> Option<&clap::Arg> {
	command.get_arguments().find(|arg| {
		arg.get_short().is_some_and(|name| name == short)
			|| arg.get_all_short_aliases().into_iter().flatten().any(|alias| alias == short)
	})
}

fn flag_value_kind(arg: &clap::Arg) -> FlagValueKind {
	match arg.get_action() {
		clap::ArgAction::SetTrue
		| clap::ArgAction::SetFalse
		| clap::ArgAction::Count
		| clap::ArgAction::Help
		| clap::ArgAction::Version => FlagValueKind::Valueless,
		clap::ArgAction::Set => match arg.get_num_args() {
			Some(range) if range.min_values() == 0 => FlagValueKind::Optional,
			_ => FlagValueKind::Always,
		},
		_ => FlagValueKind::Always,
	}
}

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

	/// Inspect and edit settings.
	Config {
		/// `list` (default) | `get` | `set` | `reset` | `path` | `init-xdg`.
		#[arg(value_name = "ACTION")]
		action: Option<String>,
		/// Setting path, such as `compaction.enabled` or `defaultThinkingLevel`.
		#[arg(value_name = "KEY")]
		key:    Option<String>,
		/// Value for `set`; multiple words are joined with spaces.
		#[arg(value_name = "VALUE")]
		value:  Vec<String>,
		/// Print machine-readable JSON.
		#[arg(long)]
		json:   bool,
	},

	/// List or safely reclaim agent-managed worktrees under the worktree base.
	#[command(name = "worktree", alias = "wt")]
	Worktree {
		/// `list` (default) | `clear`.
		#[arg(value_name = "ACTION")]
		action:   Option<String>,
		/// Clear every entry, including live checkouts; live isolation owners,
		/// locked worktrees, and submodule checkouts are still refused.
		#[arg(long)]
		all:      bool,
		/// Print the plan without moving anything (`clear`).
		#[arg(short = 'n', long = "dry-run")]
		dry_run:  bool,
		/// Emit machine-readable JSON.
		#[arg(short = 'j', long)]
		json:     bool,
	},

	/// List and control daemon-supervised background processes.
	Ps {
		/// `list` (default), `info`, `logs`, `stop`, `kill`, or `restart`.
		#[arg(
			value_name = "ACTION",
			value_parser = ["list", "info", "logs", "stop", "kill", "restart"]
		)]
		action:  Option<String>,
		/// Process name (required for every action except `list`).
		#[arg(value_name = "NAME")]
		name:    Option<String>,
		/// List every project and global service scope (list).
		#[arg(short = 'a', long)]
		all:     bool,
		/// Emit machine-readable JSON.
		#[arg(short = 'j', long)]
		json:    bool,
		/// Static listing instead of the interactive monitor (list).
		#[arg(long)]
		plain:   bool,
		/// Target another project directory instead of the current one.
		#[arg(long, value_name = "DIR")]
		dir:     Option<PathBuf>,
		/// Target a machine-global service scope (e.g. `browser-relay`).
		#[arg(long, value_name = "SERVICE")]
		global:  Option<String>,
		/// Keep streaming new output (logs).
		#[arg(short = 'f', long)]
		follow:  bool,
		/// Read from the beginning instead of the tail (logs).
		#[arg(long)]
		head:    bool,
		/// Number of log lines, max 1000 (logs).
		#[arg(short = 'n', long, value_name = "N")]
		lines:   Option<usize>,
		/// Regex filter applied to log lines (logs).
		#[arg(long, value_name = "PATTERN")]
		grep:    Option<String>,
		/// Grace period in seconds before hard kill (stop).
		#[arg(long, value_name = "SECONDS")]
		timeout: Option<u64>,
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
	fn session_uuid_detection_matches_ts_pattern() {
		assert!(is_session_uuid("019ea530-ffff-7000-8000-000000000000"));
		assert!(is_session_uuid("019EA530-FFFF-7000-8000-000000000000"));
		assert!(!is_session_uuid("019ea530-ffff-7000-8000-00000000000"));
		assert!(!is_session_uuid("019ea530-ffff-7000-8000-0000000000000"));
		assert!(!is_session_uuid("019ea530-ffff-7000-8000-00000000000g"));
		assert!(!is_session_uuid("019ea530ffff70008000000000000000"));
		assert!(!is_session_uuid("019ea530-ffff-7000-8000-00000000000-"));
		assert!(!is_session_uuid("summarize the diff"));
	}

	#[test]
	fn continue_with_session_uuid_resumes_that_session() {
		let mut cli = parse(&[
			"omp",
			"--continue",
			"019EA530-FFFF-7000-8000-000000000000",
			"--no-title",
		]);
		assert!(cli.continue_session);
		normalize_continue_session_args(&mut cli);
		assert!(!cli.continue_session);
		assert_eq!(cli.resume.as_deref(), Some("019EA530-FFFF-7000-8000-000000000000"));
		assert!(cli.prompt.is_empty());
	}

	#[test]
	fn continue_non_uuid_prompt_stays_prompt() {
		let mut cli = parse(&["omp", "--continue", "summarize the diff"]);
		normalize_continue_session_args(&mut cli);
		assert!(cli.continue_session);
		assert_eq!(cli.resume, None);
		assert_eq!(cli.prompt, ["summarize the diff"]);
	}

	#[test]
	fn continue_bare_or_uuid_prompt_with_more_text_stays_unchanged() {
		let mut bare = parse(&["omp", "--continue"]);
		normalize_continue_session_args(&mut bare);
		assert!(bare.continue_session);
		assert!(bare.resume.is_none());
		assert!(bare.prompt.is_empty());

		let mut multi = parse(&[
			"omp",
			"--continue",
			"019EA530-FFFF-7000-8000-000000000000",
			"also this",
		]);
		normalize_continue_session_args(&mut multi);
		assert!(multi.continue_session);
		assert_eq!(multi.resume, None);
		assert_eq!(multi.prompt.len(), 2);
	}

	#[test]
	fn explicit_resume_or_fork_is_never_overridden() {
		let mut resumed = parse(&[
			"omp",
			"--continue",
			"--resume",
			"abc",
			"019EA530-FFFF-7000-8000-000000000000",
		]);
		normalize_continue_session_args(&mut resumed);
		assert!(resumed.continue_session);
		assert_eq!(resumed.resume.as_deref(), Some("abc"));
		assert_eq!(resumed.prompt, ["019EA530-FFFF-7000-8000-000000000000"]);

		let mut forked = parse(&[
			"omp",
			"--continue",
			"--fork",
			"abc",
			"019EA530-FFFF-7000-8000-000000000000",
		]);
		normalize_continue_session_args(&mut forked);
		assert!(forked.continue_session);
		assert_eq!(forked.fork.as_deref(), Some("abc"));
		assert_eq!(forked.prompt, ["019EA530-FFFF-7000-8000-000000000000"]);
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

	fn refusal(args: &[&str]) -> Option<String> {
		let mut argv = vec![std::ffi::OsString::from("omp")];
		argv.extend(args.iter().map(|arg| std::ffi::OsString::from(*arg)));
		refused_verb_message(&argv)
	}

	#[test]
	fn known_missing_verbs_are_refused_not_absorbed_as_prompts() {
		for verb in ["acp", "browser-relay", "auth-broker", "img", "q"] {
			let message = refusal(&[verb]).unwrap_or_else(|| panic!("{verb} must be refused"));
			assert!(message.contains(&format!("`omp {verb}`")), "{message}");
			assert!(!message.contains("excluded"), "{verb} is missing, not excluded: {message}");
		}
	}

	#[test]
	fn excluded_verbs_report_the_boundary_reason() {
		for (verb, reason) in EXCLUDED_VERBS {
			let message = refusal(&[verb]).unwrap_or_else(|| panic!("{verb} must be refused"));
			assert!(message.contains(&format!("`omp {verb}` is excluded")), "{message}");
			assert!(message.contains(reason), "{message}");
			assert!(!message.contains("not implemented"), "{message}");
		}
	}

	#[test]
	fn registered_subcommands_are_never_refused() {
		for verb in crate::profile::SUBCOMMANDS {
			assert_eq!(refusal(&[verb]), None, "`omp {verb}` must reach its own dispatch");
			assert_eq!(refusal(&["--print", verb]), None, "`omp --print {verb}` too");
		}
	}

	#[test]
	fn arbitrary_prompts_and_literal_operands_are_never_refused() {
		// Genuine prompts, including one that starts with a verb-like word.
		assert_eq!(refusal(&["fix", "the", "build"]), None);
		assert_eq!(refusal(&["please", "share", "the", "log"]), None);
		assert_eq!(refusal(&["-"]), None);
		assert_eq!(refusal(&["-", "acp"]), None);
		// POSIX end-of-options: everything after `--` is literal prompt text.
		assert_eq!(refusal(&["--", "acp"]), None);
		assert_eq!(refusal(&["--", "plugin", "list"]), None);
		// Released app prompts from real invocations still run.
		assert_eq!(refusal(&["--print", "summarize", "the", "diff"]), None);
	}

	#[test]
	fn leading_flags_do_not_hide_or_capture_a_verb() {
		// Flag values are values, not verbs.
		assert_eq!(refusal(&["--model", "acp"]), None);
		assert_eq!(refusal(&["--system-prompt", "acp", "hello"]), None);
		assert_eq!(refusal(&["--resume", "acp"]), None);
		assert_eq!(refusal(&["--model=acp", "hello"]), None);
		// A leading valueless flag does not turn the following verb into a prompt.
		assert!(refusal(&["--no-tools", "acp"]).is_some());
		assert!(refusal(&["-p", "compress"]).is_some());
		assert!(refusal(&["-c", "acp"]).is_some());
		// Combined short flags: valueless ones keep the verb in the command slot…
		assert!(refusal(&["-pc", "acp"]).is_some());
		assert!(refusal(&["-cp", "acp"]).is_some());
		// …while a value-taking flag at the tail consumes the next token, and the
		// attached form (`-rc` = resume "c") consumes only the cluster tail.
		assert_eq!(refusal(&["-pr", "acp"]), None);
		assert!(refusal(&["-rc", "acp"]).is_some());
		// A recognized subcommand keeps its own dispatch.
		assert_eq!(refusal(&["models", "acp"]), None);
		assert_eq!(refusal(&["read", "acp"]), None);
	}

	#[test]
	fn leading_help_and_version_keep_clap_actions() {
		for args in [
			vec!["--help", "acp"],
			vec!["-h", "acp"],
			vec!["--version", "acp"],
			vec!["-V", "acp"],
			vec!["-ph", "acp"],
			vec!["--model", "m", "--help", "acp"],
		] {
			assert_eq!(refusal(&args), None, "{args:?} stays a leading help/version request");
		}
		// Once an operand leads, a later `--help` does not rescue the verb: this
		// build cannot help with a command it does not have.
		assert!(refusal(&["acp", "--help"]).is_some());
		assert!(refusal(&["--print", "acp", "--help"]).is_some());
		// A help character consumed as a flag value is not a request.
		assert!(refusal(&["-rh", "acp"]).is_some());
		assert!(refusal(&["--system-prompt", "--help", "acp"]).is_some());
		assert!(refusal(&["--resume=--help", "acp"]).is_some());
		// An attached cluster value ends that flag, not the scan: a later `--help`
		// is still the leading help request.
		assert_eq!(refusal(&["-rh", "--help", "acp"]), None);
		assert_eq!(refusal(&["-rc", "--help", "acp"]), None);
		assert!(refusal(&["-rc", "acp"]).is_some());
	}

	#[test]
	fn a_leading_launch_command_parses_its_own_surface() {
		fn parse(args: &[&str]) -> Cli {
			let mut argv = vec![std::ffi::OsString::from("omp")];
			argv.extend(args.iter().map(|arg| std::ffi::OsString::from(*arg)));
			parse_cli(argv)
		}
		let prompt = parse(&["launch", "fix", "the", "build"]);
		assert!(prompt.command.is_none());
		assert_eq!(prompt.prompt, ["fix", "the", "build"]);
		// A subcommand or refused verb as the launch operand is prompt text.
		for operand in ["repl", "read", "models", "acp", "plugin", "git"] {
			let parsed = parse(&["launch", operand]);
			assert!(parsed.command.is_none(), "{operand} must not dispatch");
			assert_eq!(parsed.prompt, [operand]);
		}
		// Globals before the command word and flags around the operand keep parsing.
		let after = parse(&["launch", "hello", "--print"]);
		assert!(after.command.is_none());
		assert!(after.print);
		assert_eq!(after.prompt, ["hello"]);
		let before = parse(&["--print", "launch", "hello"]);
		assert!(before.print);
		assert_eq!(before.prompt, ["hello"]);
		// Flag values stay values, and literal `--` keeps its meaning.
		let value = parse(&["launch", "--model", "repl", "hello"]);
		assert_eq!(value.model.as_deref(), Some("repl"));
		assert_eq!(value.prompt, ["hello"]);
		let literal = parse(&["launch", "--", "repl", "--print"]);
		assert!(!literal.print);
		assert_eq!(literal.prompt, ["repl", "--print"]);
		// Only the leading slot is the command: a later `launch` is a prompt word,
		// and a real subcommand before it still dispatches.
		assert_eq!(parse(&["hello", "launch"]).prompt, ["hello", "launch"]);
		assert!(matches!(
			parse(&["models", "launch"]).command,
			Some(Command::Models { .. })
		));
	}
}
