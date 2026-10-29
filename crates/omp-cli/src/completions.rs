//! `omp completions <bash|zsh|fish>` and the hidden `omp __complete <kind>`
//! helper the generated scripts call for dynamic candidates.
//!
//! The completion spec is derived from clap's command tree — the same registry
//! that renders `--help` — and rendered per shell the way
//! `packages/coding-agent/src/cli/completion-gen.ts` does. Dynamic values are
//! resolved by `__complete`, which reads only local state (the model catalog and
//! the session logs), so a TAB never starts a session or a network request.
//! Script generation itself never loads config, sessions, or the agent runtime.

use std::io::{BufRead as _, Write as _};
use std::path::Path;
use std::time::SystemTime;

use clap::{Arg, ArgAction, CommandFactory};

use crate::args::Cli;
use crate::catalog::Catalog;
use crate::runtime::BUILTIN_TOOLS;
use crate::session;

/// Shells accepted by `omp completions` (upstream `SHELLS`).
pub const SHELLS: [&str; 3] = ["bash", "zsh", "fish"];

#[derive(Debug, Clone, PartialEq, Eq)]
enum ValueSource {
	/// Boolean — takes no value.
	Flag,
	/// Static single value.
	Enum(Vec<String>),
	/// Static comma-separated list.
	List(Vec<String>),
	/// Dynamic: the model catalog.
	Models { multiple: bool },
	/// Dynamic: on-disk sessions.
	Sessions,
	File,
	Dir,
}

impl ValueSource {
	fn takes_value(&self) -> bool {
		!matches!(self, Self::Flag)
	}
}

#[derive(Debug)]
struct FlagSpec {
	name: String,
	short: Option<char>,
	description: String,
	value: ValueSource,
	repeatable: bool,
}

#[derive(Debug)]
struct ArgSpec {
	name: String,
	description: String,
	value: ValueSource,
}

#[derive(Debug)]
struct CommandSpec {
	name: String,
	aliases: Vec<String>,
	description: String,
	flags: Vec<FlagSpec>,
	args: Vec<ArgSpec>,
}

#[derive(Debug)]
struct Spec {
	bin: String,
	root_flags: Vec<FlagSpec>,
	commands: Vec<CommandSpec>,
}

// --- Public entry points ------------------------------------------------------

/// Print `shell`'s completion script. Invalid or missing shell: the usage line
/// is returned as the error, which `main` writes to stderr with exit code 1.
pub fn run(shell: Option<&str>) -> Result<(), String> {
	let shell = shell.filter(|value| SHELLS.contains(value));
	let Some(shell) = shell else {
		return Err(format!("Usage: omp completions <{}>", SHELLS.join("|")));
	};
	let script = generate(shell);
	let mut stdout = std::io::stdout().lock();
	stdout
		.write_all(script.as_bytes())
		.map_err(|error| error.to_string())?;
	stdout.flush().map_err(|error| error.to_string())
}

/// `omp __complete <kind> [-- <prefix>]` — one `value<TAB>description` line per
/// candidate. Unknown kinds and missing local state print nothing.
pub fn complete(args: &[String]) {
	let argv: Vec<&str> = args
		.iter()
		.map(String::as_str)
		.filter(|token| *token != "--")
		.collect();
	let Some(kind) = argv.first().copied() else {
		return;
	};
	let prefix = if argv.len() > 1 {
		argv[argv.len() - 1]
	} else {
		""
	};
	match kind {
		"models" => complete_models(prefix),
		"sessions" => complete_sessions(prefix),
		_ => {},
	}
}

fn generate(shell: &str) -> String {
	let spec = build_spec();
	match shell {
		"bash" => generate_bash(&spec),
		"zsh" => generate_zsh(&spec),
		_ => generate_fish(&spec),
	}
}

// --- Spec from the clap registry ---------------------------------------------

fn build_spec() -> Spec {
	let command = Cli::command();
	let bin = command.get_name().to_string();
	let (root_flags, _) = collect(&command, None);
	let mut commands = Vec::new();
	for sub in command.get_subcommands() {
		if sub.is_hide_set() {
			continue;
		}
		let name = sub.get_name().to_string();
		let (flags, args) = collect(sub, Some(&name));
		commands.push(CommandSpec {
			aliases: sub.get_all_aliases().map(ToString::to_string).collect(),
			name,
			description: sub.get_about().map(ToString::to_string).unwrap_or_default(),
			flags,
			args,
		});
	}
	commands.sort_by(|a, b| a.name.cmp(&b.name));
	Spec { bin, root_flags, commands }
}

fn collect(command: &clap::Command, subcommand: Option<&str>) -> (Vec<FlagSpec>, Vec<ArgSpec>) {
	let mut flags = Vec::new();
	let mut args = Vec::new();
	for arg in command.get_arguments() {
		if arg.is_hide_set() {
			continue;
		}
		let id = arg.get_id().as_str();
		// clap's auto-generated `-h/--help` and `-V/--version` are not part of
		// the command metadata the upstream generator walks.
		if id == "help" || id == "version" {
			continue;
		}
		let description = arg.get_help().map(ToString::to_string).unwrap_or_default();
		if arg.is_positional() {
			args.push(ArgSpec { name: id.to_string(), description, value: arg_value(subcommand, id, arg) });
			continue;
		}
		let Some(long) = arg.get_long() else {
			continue;
		};
		flags.push(FlagSpec {
			name: long.to_string(),
			short: arg.get_short(),
			description,
			value: flag_value(long, arg),
			repeatable: matches!(arg.get_action(), ArgAction::Append),
		});
	}
	(flags, args)
}

/// Flag→value mapping: the one manual knob, keyed by flag name like upstream.
fn flag_value(name: &str, arg: &Arg) -> ValueSource {
	if !arg.get_action().takes_values() {
		return ValueSource::Flag;
	}
	let possible: Vec<String> = arg.get_possible_values().iter().map(|value| value.get_name().to_string()).collect();
	if !possible.is_empty() {
		return ValueSource::Enum(possible);
	}
	match name {
		"model" | "smol" | "slow" | "plan" => ValueSource::Models { multiple: false },
		"models" => ValueSource::Models { multiple: true },
		"resume" | "fork" | "session" => ValueSource::Sessions,
		"tools" => ValueSource::List(BUILTIN_TOOLS.iter().map(|(tool, _)| (*tool).to_string()).collect()),
		"session-dir" | "plugin-dir" => ValueSource::Dir,
		_ => ValueSource::File,
	}
}

fn arg_value(subcommand: Option<&str>, name: &str, arg: &Arg) -> ValueSource {
	// The shell operand deliberately accepts any string so a bad value exits 1
	// with the usage line (the TS command validates it in `run`), but the
	// completion surface still offers the real domain.
	if subcommand == Some("completions") && name == "shell" {
		return ValueSource::Enum(SHELLS.iter().map(|shell| (*shell).to_string()).collect());
	}
	let possible: Vec<String> = arg.get_possible_values().iter().map(|value| value.get_name().to_string()).collect();
	if !possible.is_empty() {
		return ValueSource::Enum(possible);
	}
	ValueSource::File
}

fn command_tokens(command: &CommandSpec) -> Vec<String> {
	let mut tokens = vec![command.name.clone()];
	tokens.extend(command.aliases.iter().cloned());
	tokens
}

// --- Dynamic candidates (`__complete`) ----------------------------------------

fn complete_models(prefix: &str) {
	let cwd = std::env::current_dir().unwrap_or_default();
	let Ok(catalog) = Catalog::load(&cwd) else {
		return;
	};
	let needle = prefix.to_lowercase();
	let mut seen = std::collections::BTreeSet::new();
	let mut lines = Vec::new();
	for (provider_id, models) in &catalog.providers {
		for (model_id, info) in models {
			// Offer both the fully-qualified `provider/id` and the bare `id`,
			// matching the fuzzy resolution `--model` accepts.
			for candidate in [format!("{provider_id}/{model_id}"), model_id.clone()] {
				if !seen.insert(candidate.clone()) {
					continue;
				}
				if !needle.is_empty() && !candidate.to_lowercase().contains(&needle) {
					continue;
				}
				lines.push(format!("{}\t{}", candidate, info.provider));
			}
		}
	}
	lines.sort();
	if !lines.is_empty() {
		println!("{}", lines.join("\n"));
	}
}

fn complete_sessions(prefix: &str) {
	let cwd = std::env::current_dir().unwrap_or_default();
	let mut sessions: Vec<(SystemTime, String, String)> = Vec::new();
	for dir in session::session_completion_dirs(&cwd, None) {
		let Ok(entries) = std::fs::read_dir(&dir) else {
			continue;
		};
		for entry in entries.flatten() {
			let path = entry.path();
			if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
				continue;
			}
			let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
				continue;
			};
			let header = session_header(&path);
			let id = header
				.as_ref()
				.and_then(|value| value.get("id"))
				.and_then(serde_json::Value::as_str)
				.map_or_else(|| stem.to_string(), str::to_string);
			if !prefix.is_empty() && !id.starts_with(prefix) {
				continue;
			}
			let title = title_slot_title(&path)
				.or_else(|| {
					header
						.as_ref()
						.and_then(|value| value.get("title"))
						.and_then(serde_json::Value::as_str)
						.map(clean)
				})
				.or_else(|| first_user_message(&path))
				.unwrap_or_default();
			let modified = entry
				.metadata()
				.and_then(|metadata| metadata.modified())
				.unwrap_or(SystemTime::UNIX_EPOCH);
			sessions.push((modified, id, title));
		}
	}
	sessions.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
	// A partially migrated legacy directory can still hold a log that also
	// exists in the current directory; keep the newest copy per session id.
	// Sorted newest first, so the first row seen for an id is the copy to keep.
	let mut seen = std::collections::BTreeSet::new();
	sessions.retain(|(_, id, _)| seen.insert(id.clone()));
	for (_, id, title) in sessions {
		println!("{id}\t{title}");
	}
}

fn session_header(path: &Path) -> Option<serde_json::Value> {
	let file = std::fs::File::open(path).ok()?;
	// v3 logs start with the fixed 256-byte title slot, then the session header.
	for line in std::io::BufReader::new(file).lines().take(4) {
		let Ok(line) = line else { break };
		let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
			continue;
		};
		if value.get("type").and_then(serde_json::Value::as_str) == Some("session") {
			return Some(value);
		}
	}
	None
}

/// Title written into the leading v3 title slot (`type:"title"`), matching
/// TS `SessionManager.list` which surfaces the slot rather than the header copy.
fn title_slot_title(path: &Path) -> Option<String> {
	let file = std::fs::File::open(path).ok()?;
	let mut line = String::new();
	std::io::BufReader::new(file).read_line(&mut line).ok()?;
	let value: serde_json::Value = serde_json::from_str(&line).ok()?;
	if value.get("type").and_then(serde_json::Value::as_str) != Some("title") {
		return None;
	}
	let title = value.get("title").and_then(serde_json::Value::as_str)?;
	let title = clean(title);
	(!title.is_empty()).then_some(title)
}

/// First user message text, the description fallback for untitled sessions
/// (upstream `title ?? firstMessage`). Only the leading 512 log lines are
/// scanned: the first user turn sits next to the header, and a TAB must stay
/// cheap on large logs.
fn first_user_message(path: &Path) -> Option<String> {
	let file = std::fs::File::open(path).ok()?;
	for line in std::io::BufReader::new(file).lines().take(512) {
		let Ok(line) = line else { break };
		if let Some(text) = user_message_text(&line) {
			let text = clean(&text);
			if !text.is_empty() {
				return Some(text);
			}
		}
	}
	None
}

/// Text of one log line when it is a user message, else `None`.
fn user_message_text(line: &str) -> Option<String> {
	let value: serde_json::Value = serde_json::from_str(line).ok()?;
	if value.get("type").and_then(serde_json::Value::as_str) != Some("message") {
		return None;
	}
	let message = value.get("message")?;
	if message.get("role").and_then(serde_json::Value::as_str) != Some("user") {
		return None;
	}
	let content = message.get("content")?;
	if let Some(text) = content.as_str() {
		return Some(text.to_string());
	}
	let parts = content.as_array()?;
	let text: String = parts
		.iter()
		.filter(|part| {
			part.get("type").and_then(serde_json::Value::as_str).unwrap_or("text") == "text"
		})
		.filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
		.collect::<Vec<_>>()
		.join(" ");
	(!text.is_empty()).then_some(text)
}

/// Strip control chars that would corrupt the tab-separated line protocol.
fn clean(text: &str) -> String {
	let flattened: String = text
		.chars()
		.map(|ch| if matches!(ch, '\t' | '\r' | '\n') { ' ' } else { ch })
		.collect();
	flattened.trim().chars().take(72).collect()
}

// --- bash ---------------------------------------------------------------------

/// Escape for use inside a bash double-quoted `compgen -W "…"` word list.
fn bash_words(values: &[String]) -> String {
	values.join(" ").replace('"', "\\\"")
}

fn bash_value_branch(bin: &str, value: &ValueSource) -> String {
	match value {
		ValueSource::Flag => "return 0".to_string(),
		ValueSource::Enum(values) => {
			format!("COMPREPLY=( $(compgen -W \"{}\" -- \"$cur\") ); return 0", bash_words(values))
		},
		ValueSource::List(values) => format!("_omp_comma \"{}\"; return 0", bash_words(values)),
		ValueSource::Models { multiple: true } => {
			format!("_omp_comma \"$(command {bin} __complete models 2>/dev/null | cut -f1)\"; return 0")
		},
		ValueSource::Models { multiple: false } => format!(
			"COMPREPLY=( $(compgen -W \"$(command {bin} __complete models -- \"$cur\" 2>/dev/null | \
			 cut -f1)\" -- \"$cur\") ); return 0"
		),
		ValueSource::Sessions => format!(
			"COMPREPLY=( $(compgen -W \"$(command {bin} __complete sessions -- \"$cur\" 2>/dev/null | \
			 cut -f1)\" -- \"$cur\") ); return 0"
		),
		ValueSource::File => "COMPREPLY=( $(compgen -f -- \"$cur\") ); compopt -o filenames; return 0".to_string(),
		ValueSource::Dir => "COMPREPLY=( $(compgen -d -- \"$cur\") ); compopt -o filenames; return 0".to_string(),
	}
}

/// Build the `case "$prev" in …` arms for every value-taking flag in scope.
fn bash_flag_case(bin: &str, flags: &[FlagSpec]) -> String {
	let mut arms = String::new();
	for flag in flags {
		if !flag.value.takes_value() {
			continue;
		}
		let mut labels = vec![format!("--{}", flag.name)];
		if let Some(short) = flag.short {
			labels.push(format!("-{short}"));
		}
		arms.push_str(&format!(
			"\t\t{})\n\t\t\t{}\n\t\t\t;;\n",
			labels.join("|"),
			bash_value_branch(bin, &flag.value)
		));
	}
	arms
}

fn bash_flag_words(flags: &[FlagSpec]) -> String {
	let mut words = Vec::new();
	for flag in flags {
		words.push(format!("--{}", flag.name));
		if let Some(short) = flag.short {
			words.push(format!("-{short}"));
		}
	}
	words.join(" ")
}

fn generate_bash(spec: &Spec) -> String {
	let bin = &spec.bin;
	let mut parts: Vec<String> = Vec::new();
	parts.push(format!(
		"# bash completion for {bin} — generated by `{bin} completions bash`"
	));
	parts.push(String::new());

	parts.push(
		r#"_omp_comma() {
	local words="$1" realcur prefix
	realcur="${cur##*,}"
	prefix="${cur%"$realcur"}"
	local -a matches
	matches=( $(compgen -W "$words" -- "$realcur") )
	local i
	for (( i=0; i < ${#matches[@]}; i++ )); do matches[i]="$prefix${matches[i]}"; done
	COMPREPLY=( "${matches[@]}" )
	compopt -o nospace 2>/dev/null
}"#
			.to_string(),
	);
	parts.push(String::new());

	let mut sub_tokens: Vec<String> = spec.commands.iter().flat_map(|command| command_tokens(command)).collect();
	sub_tokens.sort();
	let root_words = format!("{} {}", bash_words(&sub_tokens), bash_flag_words(&spec.root_flags));
	let mut root = String::from("_omp_root() {\n\tcase \"$prev\" in\n");
	root.push_str(&bash_flag_case(bin, &spec.root_flags));
	root.push_str("\tesac\n\tif [[ \"$cur\" == -* ]]; then\n");
	root.push_str(&format!(
		"\t\tCOMPREPLY=( $(compgen -W \"{}\" -- \"$cur\") )\n",
		bash_flag_words(&spec.root_flags)
	));
	root.push_str("\telse\n");
	root.push_str(&format!(
		"\t\tCOMPREPLY=( $(compgen -W \"{}\" -- \"$cur\") )\n",
		root_words.trim_end()
	));
	root.push_str("\tfi\n}");
	parts.push(root);
	parts.push(String::new());

	for command in &spec.commands {
		let enum_words = command.args.iter().find_map(|arg| {
			if let ValueSource::Enum(values) = &arg.value {
				Some(bash_words(values))
			} else {
				None
			}
		});
		let else_branch = enum_words.map_or_else(
			|| {
				if command.args.iter().any(|arg| matches!(&arg.value, ValueSource::File)) {
					"COMPREPLY=( $(compgen -f -- \"$cur\") ); compopt -o filenames".to_string()
				} else {
					":".to_string()
				}
			},
			|words| format!("COMPREPLY=( $(compgen -W \"{words}\" -- \"$cur\") )"),
		);
		parts.push(format!(
			"_omp_cmd_{}() {{\n\tcase \"$prev\" in\n{}\tesac\n\tif [[ \"$cur\" == -* ]]; then\n\t\tCOMPREPLY=( \
			 $(compgen -W \"{}\" -- \"$cur\") )\n\telse\n\t\t{}\n\tfi\n}}",
			bash_fn(&command.name),
			bash_flag_case(bin, &command.flags),
			bash_flag_words(&command.flags),
			else_branch
		));
		parts.push(String::new());
	}

	let mut dispatch = String::new();
	for command in &spec.commands {
		dispatch.push_str(&format!(
			"\t\t{})\n\t\t\t_omp_cmd_{}\n\t\t\t;;\n",
			command_tokens(command).join("|"),
			bash_fn(&command.name)
		));
	}
	parts.push(format!(
		r#"_omp() {{
	local cur prev cmd i
	cur="${{COMP_WORDS[COMP_CWORD]}}"
	prev="${{COMP_WORDS[COMP_CWORD-1]}}"
	cmd=""
	for (( i=1; i < COMP_CWORD; i++ )); do
		case "${{COMP_WORDS[i]}}" in
			-*) ;;
			*) cmd="${{COMP_WORDS[i]}}"; break ;;
		esac
	done
	case "$cmd" in
{dispatch}		*) _omp_root ;;
	esac
}}
complete -F _omp {bin}"#
	));
	parts.push(String::new());
	format!("{}\n", parts.join("\n"))
}

fn bash_fn(name: &str) -> String {
	name.chars()
		.map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
		.collect()
}

// --- zsh ----------------------------------------------------------------------

/// Sanitize a description for embedding in a single-quoted zsh `_arguments` spec.
fn zsh_desc(description: &str) -> String {
	description
		.replace('\'', "’")
		.replace('[', "(")
		.replace(']', ")")
		.replace('\r', " ")
		.replace('\n', " ")
		.replace(':', " ")
		.trim()
		.to_string()
}

fn zsh_action(value: &ValueSource) -> String {
	match value {
		ValueSource::Flag => String::new(),
		ValueSource::Enum(values) => format!(":value:({})", values.join(" ")),
		ValueSource::List(_) => ":value:_omp_tools".to_string(),
		ValueSource::Models { multiple: true } => ":models:_omp_models_list".to_string(),
		ValueSource::Models { multiple: false } => ":model:_omp_call models".to_string(),
		ValueSource::Sessions => ":session:_omp_call sessions".to_string(),
		ValueSource::File => ":file:_files".to_string(),
		ValueSource::Dir => ":dir:_files -/".to_string(),
	}
}

fn zsh_flag_spec(flag: &FlagSpec) -> String {
	let body = format!("[{}]{}", zsh_desc(&flag.description), zsh_action(&flag.value));
	match (flag.short, flag.repeatable) {
		(Some(short), true) => format!("'*'{{-{short},--{}}}'{body}'", flag.name),
		(Some(short), false) => format!("'(-{short} --{})'{{-{short},--{}}}'{body}'", flag.name, flag.name),
		(None, true) => format!("'*--{}{body}'", flag.name),
		(None, false) => format!("'--{}{body}'", flag.name),
	}
}

fn zsh_arg_spec(arg: &ArgSpec) -> String {
	match &arg.value {
		ValueSource::Enum(values) => format!("':{}:({})'", arg.name, values.join(" ")),
		_ => format!("':{}:_files'", arg.name),
	}
}

fn generate_zsh(spec: &Spec) -> String {
	let bin = &spec.bin;
	let mut parts: Vec<String> = Vec::new();
	parts.push(format!("#compdef {bin}"));
	parts.push(format!(
		"# zsh completion for {bin} — generated by `{bin} completions zsh`"
	));
	parts.push(String::new());

	let tools = spec
		.root_flags
		.iter()
		.chain(spec.commands.iter().flat_map(|command| command.flags.iter()))
		.find_map(|flag| {
			if let ValueSource::List(values) = &flag.value {
				Some(values.join(" "))
			} else {
				None
			}
		})
		.unwrap_or_default();
	let helpers = r#"_omp_call() {
	local kind=$1
	local -a items
	local line
	for line in "${(@f)$(command @BIN@ __complete $kind -- "$PREFIX" 2>/dev/null)}"; do
		[[ -z $line ]] && continue
		items+=( "${line//$'\t'/:}" )
	done
	_describe -t "$kind" "$kind" items
}
_omp_models_list() {
	local -a items
	local line
	for line in "${(@f)$(command @BIN@ __complete models 2>/dev/null)}"; do
		[[ -z $line ]] && continue
		items+=( "${line%%$'\t'*}" )
	done
	_values -s , 'models' $items
}
_omp_tools() { _values -s , 'tools' @TOOLS@ }"#;
	parts.push(helpers.replace("@BIN@", bin).replace("@TOOLS@", &tools));
	parts.push(String::new());

	let rows: Vec<String> = spec
		.commands
		.iter()
		.map(|command| format!("\t\t'{}:{}'", command.name, zsh_desc(&command.description)))
		.collect();
	parts.push(format!(
		"_omp_commands() {{\n\tlocal -a commands\n\tcommands=(\n{}\n\t)\n\t_describe -t commands 'command' \
		 commands\n}}",
		rows.join("\n")
	));
	parts.push(String::new());

	for command in &spec.commands {
		let mut specs = vec!["'(-h --help)'{-h,--help}'[Show help]'".to_string()];
		specs.extend(command.flags.iter().map(zsh_flag_spec));
		specs.extend(command.args.iter().map(zsh_arg_spec));
		parts.push(format!(
			"_omp_cmd_{}() {{\n\t_arguments -s \\\n\t\t{}\n}}",
			bash_fn(&command.name),
			specs.join(" \\\n\t\t")
		));
		parts.push(String::new());
	}

	let arms = spec
		.commands
		.iter()
		.map(|command| {
			format!(
				"\t\t\t{}) _omp_cmd_{} ;;",
				command_tokens(command).join("|"),
				bash_fn(&command.name)
			)
		})
		.collect::<Vec<_>>()
		.join("\n");
	let mut root_specs = vec![
		"'(-h --help)'{-h,--help}'[Show help]'".to_string(),
		"'(-V --version)'{-V,--version}'[Show version]'".to_string(),
	];
	root_specs.extend(spec.root_flags.iter().map(zsh_flag_spec));
	root_specs.push("'1: :_omp_commands'".to_string());
	root_specs.push("'*::arg:->args'".to_string());
	parts.push(format!(
		"_omp() {{\n\tlocal curcontext=\"$curcontext\" state line\n\ttypeset -A opt_args\n\t_arguments -C -s \
		 \\\n\t\t{}\n\tcase $state in\n\t\targs)\n\t\t\tcase $line[1] in\n{}\n\t\t\tesac\n\t\t\t;;\n\tesac\n}}",
		root_specs.join(" \\\n\t\t"),
		arms
	));
	parts.push(String::new());
	parts.push(format!(
		"# Works both ways: autoloaded from $fpath (file named _omp) or eval'd from a\n# startup file. When \
		 autoloaded, funcstack[1] is _omp and we invoke it; when sourced/eval'd we register it with compdef \
		 instead.\nif [ \"$funcstack[1]\" = \"_omp\" ]; then\n\t_omp \"$@\"\nelse\n\tcompdef _omp {bin}\nfi"
	));
	parts.push(String::new());
	format!("{}\n", parts.join("\n"))
}

// --- fish ---------------------------------------------------------------------

fn fish_desc(description: &str) -> String {
	description
		.replace('\'', "’")
		.replace('\r', " ")
		.replace('\n', " ")
		.trim()
		.to_string()
}

fn fish_value(bin: &str, value: &ValueSource) -> String {
	match value {
		ValueSource::Flag => String::new(),
		ValueSource::Enum(values) | ValueSource::List(values) => format!("-x -a '{}'", values.join(" ")),
		ValueSource::Models { .. } => {
			format!("-x -a '(command {bin} __complete models -- (commandline -ct))'")
		},
		ValueSource::Sessions => format!("-x -a '(command {bin} __complete sessions -- (commandline -ct))'"),
		ValueSource::File => "-r -F".to_string(),
		ValueSource::Dir => "-x -a '(__fish_complete_directories (commandline -ct))'".to_string(),
	}
}

fn fish_flag_line(bin: &str, condition: &str, flag: &FlagSpec) -> String {
	let mut segments = vec![format!("complete -c {bin}"), format!("-n '{condition}'")];
	if let Some(short) = flag.short {
		segments.push(format!("-s {short}"));
	}
	segments.push(format!("-l {}", flag.name));
	if !flag.description.is_empty() {
		segments.push(format!("-d '{}'", fish_desc(&flag.description)));
	}
	let value = fish_value(bin, &flag.value);
	if !value.is_empty() {
		segments.push(value);
	}
	segments.join(" ")
}

fn generate_fish(spec: &Spec) -> String {
	let bin = &spec.bin;
	let mut lines = vec![
		format!("# fish completion for {bin} — generated by `{bin} completions fish`"),
		String::new(),
	];

	let all_tokens: Vec<String> = spec.commands.iter().flat_map(command_tokens).collect();
	lines.push("function __fish_omp_no_subcommand".to_string());
	lines.push("\tfor i in (commandline -opc)".to_string());
	lines.push(format!("\t\tif contains -- $i {}", all_tokens.join(" ")));
	lines.push("\t\t\treturn 1".to_string());
	lines.push("\t\tend".to_string());
	lines.push("\tend".to_string());
	lines.push("\treturn 0".to_string());
	lines.push("end".to_string());
	lines.push(String::new());

	let root_condition = "__fish_omp_no_subcommand";
	for command in &spec.commands {
		for token in command_tokens(command) {
			lines.push(format!(
				"complete -c {bin} -f -n '{root_condition}' -a '{token}' -d '{}'",
				fish_desc(&command.description)
			));
		}
	}
	lines.push(String::new());

	for flag in &spec.root_flags {
		lines.push(fish_flag_line(bin, root_condition, flag));
	}
	lines.push(String::new());

	for command in &spec.commands {
		let condition = format!("__fish_seen_subcommand_from {}", command_tokens(command).join(" "));
		for flag in &command.flags {
			lines.push(fish_flag_line(bin, &condition, flag));
		}
		let enum_args: Vec<&ArgSpec> = command
			.args
			.iter()
			.filter(|arg| matches!(&arg.value, ValueSource::Enum(_)))
			.collect();
		if enum_args.is_empty() {
			if command.args.iter().any(|arg| matches!(&arg.value, ValueSource::File)) {
				lines.push(format!("complete -c {bin} -F -n '{condition}'"));
			}
		} else {
			for arg in enum_args {
				if let ValueSource::Enum(values) = &arg.value {
					lines.push(format!(
						"complete -c {bin} -f -n '{condition}' -a '{}' -d '{}'",
						values.join(" "),
						fish_desc(&arg.description)
					));
				}
			}
		}
	}
	lines.push(String::new());
	format!("{}\n", lines.join("\n"))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn scripts_reference_only_implemented_dynamic_kinds() {
		for shell in ["bash", "zsh", "fish"] {
			let script = generate(shell);
			// `models` is a literal endpoint in every script. zsh routes sessions
			// through `_omp_call sessions`; bash and fish call `__complete
			// sessions` directly.
			assert!(script.contains("__complete models"), "{shell} must resolve models dynamically");
			assert!(
				script.contains("__complete sessions") || script.contains("_omp_call sessions"),
				"{shell} must resolve sessions dynamically"
			);
			assert!(!script.contains("__complete provider"), "{shell} has no provider resolver");
			assert!(!script.contains("_omp_cmd___complete"));
			assert!(script.contains("repl"), "{shell} must offer the ported `repl` subcommand");
			if shell == "zsh" {
				// clap's version flag is `-V`; the upstream `-v` would fail.
				assert!(script.contains("{-V,--version}"), "zsh must complete the real version flag");
				assert!(!script.contains("{-v,--version}"), "zsh must not invent `-v`");
			}
		}
	}
}
