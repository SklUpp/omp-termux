//! `omp` binary entry point: resolve files, then hand turns to the runner.

use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use omp_cli::{
	Error, Result,
	agent::{self, AgentRunner, RunRequest},
	agents,
	approve::Approvals,
	args::{Cli, Command},
	catalog::Catalog,
	completions,
	config::Config,
	daemon_broker,
	deadline::{self, Deadline},
	dirs,
	file_mentions,
	foreign_session::{self, ForeignSessionSource, ForeignSessionStore},
	local_models,
	output::{self, OutputMode, Reporter, session_id},
	process_cli,
	profile,
	prompts::{self, Prompts},
	repl,
	runtime::{self, PiAgentRunner},
	session::{
		self, ContentPart, MessageContent, MessageEntry, SessionHeader, SessionMessage,
		SessionOptions, SessionStore, TurnBuilder, TurnRecord,
	},
	skills::{self, Skill},
	tool_commands,
};

struct CliError {
	message: String,
	code: u8,
}

impl CliError {
	fn fail(msg: impl Into<String>) -> Self {
		Self {
			message: msg.into(),
			code: omp_cli::EXIT_FAILURE,
		}
	}

	fn usage(msg: impl Into<String>) -> Self {
		Self {
			message: msg.into(),
			code: omp_cli::EXIT_USAGE,
		}
	}
}

impl From<Error> for CliError {
	fn from(err: Error) -> Self {
		let msg = err.to_string();
		if msg.contains("legacy v1 session log: read-only")
			|| msg.contains("cannot append to legacy v1 session log")
		{
			Self::usage(msg)
		} else {
			Self::fail(msg)
		}
	}
}

impl From<std::io::Error> for CliError {
	fn from(err: std::io::Error) -> Self {
		Self::fail(err.to_string())
	}
}

fn main() -> std::process::ExitCode {
	match run() {
		Ok(()) => std::process::ExitCode::from(omp_cli::EXIT_SUCCESS),
		Err(error) => {
			eprintln!("{}", error.message);
			std::process::ExitCode::from(error.code)
		},
	}
}

fn run() -> std::result::Result<(), CliError> {
	// Bootstrap before clap (as `runCli` does before `parseArgs`): profile flags
	// are cracked out of raw argv with the shared value-consumption contract so
	// `--system-prompt --profile` stays literal and invalid OMP_PROFILE errors
	// even for `--help`.
	let bootstrap = profile::extract_cli_flags(std::env::args_os().collect())?;
	let profile = profile::resolve_cli(bootstrap.profile.as_deref())?;
	profile::activate(profile.as_deref());
	// Hidden worker dispatch before clap/config/session boot: the broker worker
	// is a separate process carrying its own scope in the environment, not a
	// user CLI invocation.
	if daemon_broker::is_worker_invocation() {
		daemon_broker::run_worker().map_err(CliError::fail)?;
		return Ok(());
	}
	// Share this profile's exact runtime scope with `pi-agent` broker clients.
	daemon_broker::share_runtime_root(&dirs::agent_dir());
	let cli = Cli::parse_from(bootstrap.argv.iter().cloned());
	if let Some(alias_name) = &bootstrap.alias {
		let result = profile::install_alias(alias_name, profile.as_deref())?;
		print!(
			"Created {} for profile {} in {}\nRestart your shell or run: {}\nThen use: {} update, {} --version, or {}\n",
			result.alias_name,
			result.profile,
			result.config_path.display(),
			result.reloaded_with,
			result.alias_name,
			result.alias_name,
			result.alias_name,
		);
		return Ok(());
	}
	// `agents`, `models`, read/grep, and completion commands run before the agent
	// runtime, config, and sessions are loaded: unpacking definitions, listing
	// the catalog, or generating a script must never boot a session, read config,
	// or touch the network.
	match &cli.command {
		Some(Command::Agents { action, dir, user, project, force, json }) => {
			let base = match &cli.cwd {
				Some(cwd) => dirs::absolute(&std::env::current_dir()?, cwd),
				None => std::env::current_dir()?,
			};
			let Some(action) = action.as_deref() else {
				agents::print_help()?;
				return Ok(());
			};
			if action != "unpack" {
				return Err(CliError::usage(format!(
					"Unknown agents action '{action}'. Expected: unpack"
				)));
			}
			let summary = agents::unpack(
				&base,
				&agents::UnpackOptions {
					force: *force,
					dir: dir.clone(),
					user: *user,
					project: *project,
				},
			)?;
			agents::print_unpack(&summary, *json)?;
			return Ok(());
		},
		Some(Command::Models { action, pattern, json, extension, config, .. }) => {
			let base = match &cli.cwd {
				Some(cwd) => dirs::absolute(&std::env::current_dir()?, cwd),
				None => std::env::current_dir()?,
			};
			// Root `--config` (before the subcommand) plus command-level overlays,
			// later files winning, like the run path.
			let request = local_models::Request {
				action: action.clone(),
				pattern: pattern.clone(),
				json: *json,
				extensions: extension.clone(),
				config: cli.config.iter().chain(config).cloned().collect(),
			};
			let stdout = io::stdout();
			let mut out = io::BufWriter::new(stdout.lock());
			local_models::run(&request, &base, &mut out)
				.map_err(|err| CliError::fail(err.to_string()))?;
			return Ok(());
		},
		Some(Command::Read { path }) => {
			let base = match &cli.cwd {
				Some(cwd) => dirs::absolute(&std::env::current_dir()?, cwd),
				None => std::env::current_dir()?,
			};
			let stdout = io::stdout();
			let mut out = io::BufWriter::new(stdout.lock());
			tool_commands::run_read(path.as_deref().unwrap_or(""), &base, &mut out)
				.map_err(CliError::fail)?;
			return Ok(());
		},
		Some(Command::Grep {
			pattern,
			path,
			glob,
			limit,
			context,
			files,
			count,
			no_gitignore,
		}) => {
			let base = match &cli.cwd {
				Some(cwd) => dirs::absolute(&std::env::current_dir()?, cwd),
				None => std::env::current_dir()?,
			};
			let mode = tool_commands::cli_mode(*count, *files);
			let request = tool_commands::GrepRequest {
				pattern: pattern.clone().unwrap_or_default(),
				path: path.clone().unwrap_or_else(|| ".".to_string()),
				glob: glob.clone(),
				limit: *limit,
				context: *context,
				mode,
				gitignore: !*no_gitignore,
			};
			let stdout = io::stdout();
			let mut out = io::BufWriter::new(stdout.lock());
			tool_commands::run_grep(&request, &base, &mut out).map_err(CliError::fail)?;
			return Ok(());
		},
		Some(Command::Ps {
			action, name, all, json, plain, dir, global, follow, head, lines, grep, timeout,
		}) => {
			let base = match &cli.cwd {
				Some(cwd) => dirs::absolute(&std::env::current_dir()?, cwd),
				None => std::env::current_dir()?,
			};
			let request = process_cli::PsRequest {
				action: action.clone().unwrap_or_else(|| "list".to_string()),
				name: name.clone(),
				all: *all,
				json: *json,
				plain: *plain,
				dir: dir.as_ref().map(|dir| dirs::absolute(&base, dir)),
				global: global.clone(),
				follow: *follow,
				head: *head,
				lines: *lines,
				grep: grep.clone(),
				timeout: *timeout,
			};
			let stdout = io::stdout();
			let mut out = io::BufWriter::new(stdout.lock());
			return process_cli::run(&request, &mut out).map_err(CliError::fail);
		},
		Some(Command::Completions { shell }) => {
			return completions::run(shell.as_deref()).map_err(|message| CliError::fail(message));
		},
		Some(Command::Complete { args }) => {
			completions::complete(args);
			return Ok(());
		},
		_ => {},
	}
	if agent::install(Box::new(PiAgentRunner::new()?)).is_err() {
		return Err(CliError::fail("agent runtime already registered"));
	}
	let Some(runner) = agent::runner() else {
		return Err(CliError::fail(agent::MISSING_RUNTIME));
	};

	// Project discovery (and relative overlays) use the effective run directory:
	// an explicit `--cwd` first, else the base config `cwd`, else the process cwd.
	let mut config = Config::load_run(&cli.config, cli.cwd.as_deref())?;
	// Dedicated flags are the runtime override layer: they win over every file.
	config.apply_runtime(&cli);
	let cwd = resolve_cwd(cli.cwd.clone(), config.cwd.clone())?;
	let processed_cli_files = if !cli.file_args.is_empty() {
		process_cli_file_args(&cli.file_args, &cwd)?
	} else {
		ProcessedCliFiles::default()
	};
	if cli.resume.as_deref() == Some("")
		&& (!io::stdin().is_terminal() || !io::stdout().is_terminal())
	{
		return Err(CliError::usage(
			"Non-interactive environment cannot display session picker. Specify a session ID or file path with --resume <id>.",
		));
	}

	// Foreign and session flags validation
	if cli.from_claude && cli.from_codex {
		return Err(CliError::usage(
			"--from-claude and --from-codex cannot be used together",
		));
	}
	let foreign_source = if cli.from_claude {
		Some(foreign_session::ForeignSessionSource::Claude)
	} else if cli.from_codex {
		Some(foreign_session::ForeignSessionSource::Codex)
	} else {
		None
	};
	if let Some(src) = foreign_source {
		let src_name = src.to_string().to_ascii_lowercase();
		if cli.no_session {
			return Err(CliError::usage(format!(
				"--from-{src_name} requires session persistence"
			)));
		}
		if cli.continue_session || cli.resume.is_some() || cli.fork.is_some() {
			return Err(CliError::usage(format!(
				"--from-{src_name} cannot be combined with --continue, --resume, or --fork"
			)));
		}
		if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
			return Err(CliError::usage(format!(
				"Non-interactive environment cannot display session picker. --from-{src_name} requires an interactive session picker."
			)));
		}
	}
	if cli.fork.is_some() && cli.no_session {
		return Err(CliError::usage("--fork requires session persistence"));
	}

	let prompt_inputs = (config.system_prompt.clone(), config.append_system_prompt.clone());
	let loaded_prompts =
		prompts::resolve(prompt_inputs.0.as_deref(), prompt_inputs.1.as_deref(), &cwd)?;
	let skills_filter = (cli.no_skills, cli.skills.clone());
	let loaded_skills = skills::load(&cwd, skills_filter.0, &skills_filter.1)?;
	let catalog = Arc::new(Catalog::load(&cwd)?);
	let approvals = Approvals::new(
		config.approval_mode.unwrap_or_default(),
		config.allow.iter().cloned(),
	);
	let default_model = config.default_model.clone();
	let requested_model = cli.model.clone();
	let requested_provider = cli.provider.clone();
	let requested_api_key = cli.api_key.clone();
	let thinking = cli.thinking.as_deref().and_then(|raw| {
		let parsed = runtime::parse_cli_thinking(raw);
		if parsed.is_none() {
			eprintln!("{}", runtime::cli_thinking_warning(raw));
		}
		parsed
	});
	let session_dir = cli
		.session_dir
		.as_ref()
		.map(|dir| dirs::absolute(&cwd, dir));

	// Pre-resolve session target
	let mut chosen_file: Option<PathBuf> = None;

	if let Some(src) = foreign_source {
		let store = foreign_session::create_foreign_session_store(src, None);
		let sessions = store
			.list()
			.map_err(|e| CliError::fail(format!("Failed to list {} sessions: {e}", src.name())))?;
		if sessions.is_empty() {
			eprintln!("No {} sessions found", src.name());
			return Ok(());
		}
		if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
			eprintln!("No {} session selected", src.name());
			return Ok(());
		}
		eprintln!("Import {} Session", src.name());
		for (i, info) in sessions.iter().enumerate() {
			let label = info
				.title
				.as_deref()
				.or(info.first_message.as_deref())
				.unwrap_or(&info.id);
			eprintln!("{}. {} ({})", i + 1, label, info.created_iso());
		}
		eprint!("Select session (1-{}): ", sessions.len());
		io::stderr().flush()?;
		let mut line = String::new();
		io::stdin().read_line(&mut line)?;
		let idx: usize = line
			.trim()
			.parse()
			.map_err(|_| CliError::fail(format!("No {} session selected", src.name())))?;
		if idx < 1 || idx > sessions.len() {
			eprintln!("No {} session selected", src.name());
			return Ok(());
		}
		let selected_info = &sessions[idx - 1];
		let loaded = store
			.load(selected_info)
			.map_err(|e| CliError::fail(format!("Failed to import {} session: {e}", src.name())))?;

		let target_cwd = if loaded.cwd.as_os_str().is_empty() || !loaded.cwd.exists() {
			cwd.clone()
		} else {
			loaded.cwd.clone()
		};
		let mut imported_store = SessionStore::open(&SessionOptions {
			enabled: true,
			cwd: target_cwd,
			model: requested_model.clone().or_else(|| default_model.clone()),
			provider: requested_provider.clone(),
			dir: session_dir.clone(),
			id: None,
			continue_latest: false,
			no_title: false,
		});
		let prov = loaded.provenance_entry(selected_info);
		let prov_entry: session::SessionEntry = serde_json::from_value(prov)
			.map_err(|e| CliError::fail(format!("cannot parse provenance entry: {e}")))?;
		imported_store
			.append_entry(prov_entry)
			.map_err(|e| CliError::fail(format!("cannot append provenance entry: {e}")))?;
		for entry_val in &loaded.entries {
			let entry: session::SessionEntry = serde_json::from_value(entry_val.clone())
				.map_err(|e| CliError::fail(format!("cannot parse imported entry: {e}")))?;
			imported_store
				.append_entry(entry)
				.map_err(|e| CliError::fail(format!("cannot append imported entry: {e}")))?;
		}
		chosen_file = imported_store.file().map(Path::to_path_buf);
	} else if let Some(fork_arg) = &cli.fork {
		let source_path = if fork_arg.contains('/') || fork_arg.contains('\\') || fork_arg.ends_with(".jsonl") {
			dirs::absolute(&cwd, Path::new(fork_arg))
		} else {
			match session::resolve_resumable_session(fork_arg, &cwd, session_dir.as_deref(), true) {
				Ok(Some(path)) => path,
				_ => {
					return Err(CliError::usage(format!(
						"Session \"{fork_arg}\" not found.\nRun `omp --resume` without an argument to pick from recent sessions, or `omp` to start a new one."
					)));
				}
			}
		};
		let new_file = session::fork_session(&source_path, Some(&cwd), session_dir.as_deref())
			.map_err(CliError::usage)?;
		chosen_file = Some(new_file);
	} else if let Some(resume_arg) = &cli.resume {
		if resume_arg.trim().is_empty() {
			if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
				return Err(CliError::usage(
					"Non-interactive environment cannot display session picker. Specify a session ID or file path with --resume <id>.",
				));
			}
			let target_dir = session_dir
				.clone()
				.unwrap_or_else(|| session::compute_default_session_dir(&cwd, None));
			let mut sessions = list_sessions_for_picker(&target_dir);
			if sessions.is_empty() {
				let global_root = SessionOptions::default_sessions_root();
				if global_root != target_dir && global_root.is_dir() {
					sessions = list_sessions_for_picker(&global_root);
				}
			}
			if sessions.is_empty() {
				eprintln!("No sessions found");
				return Ok(());
			}
			for (i, (path, title, id, _)) in sessions.iter().enumerate() {
				let name = title.as_deref().unwrap_or(id);
				eprintln!("{}. {} ({})", i + 1, name, path.display());
			}
			eprint!("Select session (1-{}): ", sessions.len());
			io::stderr().flush()?;
			let mut line = String::new();
			io::stdin().read_line(&mut line)?;
			let idx: usize = line
				.trim()
				.parse()
				.map_err(|_| CliError::fail("No session selected"))?;
			if idx < 1 || idx > sessions.len() {
				eprintln!("No session selected");
				return Ok(());
			}
			chosen_file = Some(sessions[idx - 1].0.clone());
		} else if resume_arg.contains('/') || resume_arg.contains('\\') || resume_arg.ends_with(".jsonl") {
			let path = dirs::absolute(&cwd, Path::new(resume_arg));
			if !path.exists() {
				return Err(CliError::usage(format!(
					"Session \"{resume_arg}\" not found.\nRun `omp --resume` without an argument to pick from recent sessions, or `omp` to start a new one."
				)));
			}
			chosen_file = Some(path);
		} else {
			match session::resolve_resumable_session(resume_arg, &cwd, session_dir.as_deref(), true) {
				Ok(Some(path)) => {
					chosen_file = Some(path);
				},
				_ => {
					return Err(CliError::usage(format!(
						"Session \"{resume_arg}\" not found.\nRun `omp --resume` without an argument to pick from recent sessions, or `omp` to start a new one."
					)));
				}
			}
		}
	}

	let mut session = Session {
		runner,
		config,
		cwd: cwd.clone(),
		prompts: loaded_prompts,
		skills: loaded_skills,
		catalog,
		approvals,
		model: None,
		provider: None,
		api_key: None,
		thinking,
		provider_session_id: None,
		prompt_cache_key: None,
		prompt_cache_key_source: None,
		max_time: cli.max_time,
		deadline: Deadline::new(cli.max_time),
		max_turns: runtime::DEFAULT_MAX_TURNS,
		tools_enabled: !cli.no_tools,
		prompt_inputs,
		skills_filter,
		store: SessionStore::default(),
		history: Vec::new(),
		session_dir: session_dir.clone(),
	};
	if let Some(model) = requested_model.or(default_model) {
		session.select_model(model);
	}
	if let Some(provider) = requested_provider {
		session.api_key = session.config.resolve_api_key(&provider);
		session.provider = Some(provider);
	}
	if requested_api_key.is_some() {
		session.api_key = requested_api_key;
	}

	let mut explicitly_existing = chosen_file.is_some();
	// TS `main.ts` truthiness: any non-empty `PI_NO_TITLE` disables titles,
	// including `0`. Resolved here (CLI boundary) so the library never reads
	// process-wide env and tests stay race-free.
	let no_title = cli.no_title
		|| std::env::var_os("PI_NO_TITLE").is_some_and(|value| !value.is_empty());
	let open_opts = if let Some(target) = &chosen_file {
		let dir = target.parent().map(PathBuf::from);
		let id = target.file_stem().and_then(|s| s.to_str()).map(str::to_string);
		SessionOptions {
			enabled: !cli.no_session,
			cwd: cwd.clone(),
			model: session.model.clone(),
			provider: session.provider.clone(),
			dir,
			id,
			continue_latest: false,
			no_title,
		}
	} else {
		if let Some(req_id) = &cli.session_id {
			let check_dir = session_dir
				.clone()
				.unwrap_or_else(|| session::compute_default_session_dir(&cwd, None));
			let direct_file = check_dir.join(format!("{req_id}.jsonl"));
			if direct_file.is_file() {
				explicitly_existing = true;
			}
		}
		SessionOptions {
			enabled: !cli.no_session,
			cwd: cwd.clone(),
			model: session.model.clone(),
			provider: session.provider.clone(),
			dir: session_dir,
			id: cli.session_id.clone(),
			continue_latest: cli.continue_session,
			no_title,
		}
	};

	let mut store = SessionStore::open(&open_opts);
	for warning in store.take_warnings() {
		eprintln!("warning: {warning}");
	}
	// TS `main.ts:buildSessionOptions`: an explicit `--prompt-cache-key` always
	// wins; otherwise a resumed/forked header key is inherited only when the
	// provider route and prompt/tool shape are unchanged.
	let explicit_cache_key = cli.prompt_cache_key.clone().filter(|key| !key.is_empty());
	let cache_shape_changed = cli.model.is_some()
		|| session.thinking.is_some()
		|| session.config.system_prompt.is_some()
		|| session.config.append_system_prompt.is_some()
		|| cli.tools.is_some()
		|| cli.no_tools;
	if let Some(key) = explicit_cache_key {
		session.prompt_cache_key = Some(key);
		session.prompt_cache_key_source = Some(PromptCacheKeySource::Explicit);
	} else if !cache_shape_changed {
		if let Some(key) = store
			.header()
			.and_then(|header| header.provider_prompt_cache_key.clone())
		{
			session.prompt_cache_key = Some(key);
			session.prompt_cache_key_source = Some(PromptCacheKeySource::Inherited);
		}
	}
	session.provider_session_id = cli.provider_session_id.clone().filter(|id| !id.is_empty());
	if !cli.no_session && explicitly_existing && !store.enabled() {
		return Err(CliError::usage(
			"cannot open session log: persistence disabled due to format error",
		));
	}
	session.history = store.history().to_vec();
	session.store = store;

	let mode = cli.mode;
	let reporter_id = if session.store.enabled() {
		session.store.id().to_string()
	} else {
		session_id()
	};
	let mut reporter = Reporter::new(mode, io::stdout(), io::stderr());
	reporter.session_header(&reporter_id, &session.cwd)?;

	if matches!(cli.command.as_ref(), Some(Command::Repl)) {
		return run_repl(&mut session, mode, &mut reporter).map_err(CliError::from);
	}

	let piped_stdin = read_piped_stdin()?;

	let repl_mode = cli.command.is_none()
		&& cli.prompt.is_empty()
		&& cli.file_args.is_empty()
		&& piped_stdin.is_none()
		&& !cli.print;
	if repl_mode {
		return run_repl(&mut session, mode, &mut reporter).map_err(CliError::from);
	}

	let mut remaining_prompts = cli.prompt.clone();
	let initial_prompt = if !remaining_prompts.is_empty() {
		remaining_prompts.remove(0)
	} else {
		String::new()
	};

	let mut context_prefix = String::new();
	if let Some(stdin) = piped_stdin {
		context_prefix.push_str(&stdin);
		context_prefix.push('\n');
	}
	if !processed_cli_files.text.is_empty() {
		context_prefix.push_str(&processed_cli_files.text);
	}

	let has_initial_content = !context_prefix.is_empty()
		|| !initial_prompt.trim().is_empty()
		|| !processed_cli_files.images.is_empty();

	if !has_initial_content && remaining_prompts.is_empty() {
		// TS print mode with zero messages writes nothing and exits 0.
		return reporter.finish().map_err(CliError::from);
	}

	let mut prompts_to_run = Vec::new();
	if has_initial_content {
		prompts_to_run.push((
			if context_prefix.is_empty() { None } else { Some(context_prefix) },
			initial_prompt,
			processed_cli_files.images,
		));
	}
	for prompt in remaining_prompts {
		prompts_to_run.push((None, prompt, Vec::new()));
	}

	for (ctx, prompt, parts) in prompts_to_run {
		if let Err(error) = session.turn(ctx, prompt, parts, mode, &mut reporter) {
			reporter.finish().ok();
			return Err(CliError::from(error));
		}
	}
	reporter.finish().map_err(CliError::from)
}

fn list_sessions_for_picker(dir: &Path) -> Vec<(PathBuf, Option<String>, String, String)> {
	let mut result = Vec::new();
	let Ok(entries) = std::fs::read_dir(dir) else {
		return result;
	};
	for entry in entries.flatten() {
		let path = entry.path();
		if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
			continue;
		}
		let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
			continue;
		};
		let id = stem.rfind('_').map_or(stem, |pos| &stem[pos + 1..]).to_string();
		let mtime = entry
			.metadata()
			.and_then(|m| m.modified())
			.unwrap_or(std::time::SystemTime::UNIX_EPOCH);
		let iso = output::iso8601(mtime);

		let title = std::fs::read_to_string(&path).ok().and_then(|c| {
			c.lines()
				.next()
				.and_then(session::parse_title_slot)
				.map(|s| s.title)
		});
		result.push((path, title, id, iso));
	}
	result.sort_by(|a, b| b.3.cmp(&a.3));
	result
}

fn run_repl<W: Write, E: Write>(
	session: &mut Session<'_>,
	mode: OutputMode,
	reporter: &mut Reporter<W, E>,
) -> Result<()> {
	// The turn needs stdin for approvals and `ask`; the REPL locks only while
	// reading its own prompt line.
	if io::stdin().is_terminal() && io::stderr().is_terminal() {
		session
			.approvals
			.set_ask(Some(omp_cli::ask::terminal_channel(session.config.ask.timeout())));
	} else {
		session.approvals.set_ask(None);
	}
	let mut line = String::new();
	loop {
		eprint!("> ");
		io::stderr().flush()?;
		line.clear();
		{
			let stdin = io::stdin();
			let mut input = stdin.lock();
			if input.read_line(&mut line)? == 0 {
				break;
			}
		}
		let Some(command) = repl::parse(&line) else {
			continue;
		};
		match command {
			repl::ReplCommand::Exit => break,
			repl::ReplCommand::Model(Some(id)) => {
				session.select_model(id.clone());
				eprintln!("model: {id}");
			},
			repl::ReplCommand::Model(None) => {
				eprintln!("model: {}", session.model.as_deref().unwrap_or("(default)"));
			},
			repl::ReplCommand::Cwd(Some(path)) => {
				session.change_cwd(PathBuf::from(path))?;
				eprintln!("cwd: {}", session.cwd.display());
			},
			repl::ReplCommand::Cwd(None) => eprintln!("cwd: {}", session.cwd.display()),
			repl::ReplCommand::Prompt(text) => {
				if let Err(error) = session.turn(None, text, Vec::new(), mode, reporter) {
					reporter.finish().ok();
					eprintln!("{error}");
					continue;
				}
				// Text mode buffers the final assistant message until `finish`:
				// flush it per turn so REPL output is visible immediately instead
				// of only when `/exit` ends the session.
				reporter.finish()?;
			},
		}
	}
	reporter.finish()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptCacheKeySource {
	Explicit,
	Inherited,
}

/// TS `#clearInheritedProviderPromptCacheKey`: a model switch clears only the
/// inherited header key, never an explicit `--prompt-cache-key`.
fn invalidate_inherited_cache_key(
	key: &mut Option<String>,
	source: &mut Option<PromptCacheKeySource>,
) {
	if *source == Some(PromptCacheKeySource::Inherited) {
		*key = None;
		*source = None;
	}
}

struct Session<'a> {
	runner: &'a dyn AgentRunner,
	config: Config,
	cwd: PathBuf,
	prompts: Prompts,
	skills: Vec<Skill>,
	catalog: Arc<Catalog>,
	approvals: Approvals,
	model: Option<String>,
	provider: Option<String>,
	api_key: Option<String>,
	/// Validated `--thinking` selector; wins over a model `:level` suffix.
	thinking: Option<runtime::CliThinking>,
	/// Provider-side sticky session id from `--provider-session-id`.
	provider_session_id: Option<String>,
	/// Explicit prompt-cache identity: flag value, else resumed/forked header.
	prompt_cache_key: Option<String>,
	/// Where `prompt_cache_key` came from: inherited keys die on a model switch,
	/// explicit `--prompt-cache-key` values survive (TS `providerPromptCacheKeySource`).
	prompt_cache_key_source: Option<PromptCacheKeySource>,
	max_time: Option<Duration>,
	deadline: Option<Deadline>,
	max_turns: usize,
	tools_enabled: bool,
	/// Raw `--system-prompt` / `--append-system-prompt` inputs, kept so a REPL
	/// `/cwd` re-resolves the project files for the new directory.
	prompt_inputs: (Option<String>, Option<String>),
	skills_filter: (bool, Vec<String>),
	store: SessionStore,
	history: Vec<TurnRecord>,
	session_dir: Option<PathBuf>,
}

impl Session<'_> {
	fn select_model(&mut self, id: String) {
		let provider = self.catalog.provider_of(&id).map(str::to_string);
		let changed = self.model.as_deref() != Some(id.as_str()) || self.provider != provider;
		if changed {
			invalidate_inherited_cache_key(
				&mut self.prompt_cache_key,
				&mut self.prompt_cache_key_source,
			);
		}
		self.provider = provider;
		self.api_key = self
			.provider
			.as_deref()
			.and_then(|provider| self.config.resolve_api_key(provider));
		self.model = Some(id);
	}

	fn change_cwd(&mut self, path: PathBuf) -> Result<()> {
		let candidate = dirs::absolute(&self.cwd, &path);
		self.cwd = std::fs::canonicalize(&candidate).map_err(|err| {
			Error::with_source(format!("cwd {} is not accessible", candidate.display()), err)
		})?;
		self.prompts = prompts::resolve(
			self.prompt_inputs.0.as_deref(),
			self.prompt_inputs.1.as_deref(),
			&self.cwd,
		)?;
		self.skills = skills::load(&self.cwd, self.skills_filter.0, &self.skills_filter.1)?;
		Ok(())
	}

	fn request(&self, prompt: String, parts: Vec<ContentPart>, output_mode: OutputMode) -> RunRequest {
		let model = match (self.model.as_deref(), self.thinking) {
			(Some(model), Some(level)) => Some(runtime::apply_thinking(model, level)),
			(model, _) => model.map(str::to_string),
		};
		RunRequest {
			prompt,
			parts,
			cwd: self.cwd.clone(),
			output_mode,
			model,
			provider: self.provider.clone(),
			api_key: self.api_key.clone(),
			provider_session_id: self.provider_session_id.clone(),
			prompt_cache_key: self.prompt_cache_key.clone(),
			system_prompt: self.prompts.system.clone(),
			append_system_prompt: self.prompts.append.clone(),
			context_files: self.prompts.files.clone(),
			skills: self.skills.clone(),
			history: self.history.clone(),
			todo_phases: self.store.latest_todo_phases(),
			session_persisted: self.store.enabled() && !self.store.is_v1(),
			config: self.config.clone(),
			catalog: Arc::clone(&self.catalog),
			approvals: self.approvals.clone(),
			max_time: self.max_time,
			max_turns: self.max_turns,
			tools_enabled: self.tools_enabled,
			cancellation_token: None,
		}
	}

	fn turn<W: Write, E: Write>(
		&mut self,
		context_prefix: Option<String>,
		raw_prompt: String,
		initial_parts: Vec<ContentPart>,
		mode: OutputMode,
		reporter: &mut Reporter<W, E>,
	) -> Result<()> {
		if self.store.is_v1() {
			return Err(Error::new(
				"cannot append to legacy v1 session log: read-only; use import_legacy_v1",
			));
		}
		reporter.begin_turn()?;

		let now_start = std::time::SystemTime::now();
		let now_iso = output::iso8601(now_start);
		let now_ms = now_start
			.duration_since(std::time::UNIX_EPOCH)
			.unwrap_or_default()
			.as_millis() as u64;

		let mentions = file_mentions::extract_file_mentions(&raw_prompt);
		let mention_msg = if !mentions.is_empty() {
			file_mentions::process_file_mentions(&mentions, &self.cwd)
		} else {
			None
		};

		let mut full_prompt = String::new();
		if let Some(ctx) = context_prefix {
			full_prompt.push_str(&ctx);
		}

		let mut parts = initial_parts;

		if let Some(msg) = &mention_msg {
			let text_block = msg.format_text_prompt_block();
			let image_block = msg.format_image_prompt_block();
			if !text_block.is_empty() {
				full_prompt.push_str(&text_block);
				full_prompt.push('\n');
			}
			if !image_block.is_empty() {
				full_prompt.push_str(&image_block);
				full_prompt.push('\n');
			}
			for file in msg.image_files() {
				if let Some(img) = &file.image {
					parts.push(ContentPart::image(&img.data_base64, &img.mime_type));
				}
			}
		}

		if !raw_prompt.is_empty() {
			full_prompt.push_str(&raw_prompt);
		}

		let has_images = parts.iter().any(ContentPart::is_image);
		if has_images {
			if let Some(requested) = self.model.as_deref() {
				let (provider, model_id) = match self.provider.as_deref() {
					Some(p) => {
						let prefix = format!("{p}/");
						let raw = requested.strip_prefix(&prefix).unwrap_or(requested);
						let (id, _) = runtime::split_effort(raw);
						(p.to_string(), id.to_string())
					},
					None => match requested.split_once('/') {
						Some((p, id)) if !p.is_empty() && !id.is_empty() => {
							let (id, _) = runtime::split_effort(id);
							(p.to_string(), id.to_string())
						},
						_ => {
							let (id, _) = runtime::split_effort(requested);
							let p = self
								.catalog
								.provider_of(id)
								.map(str::to_string)
								.unwrap_or_default();
							(p, id.to_string())
						},
					},
				};
				let supports_images = self.catalog.supports_images(&provider, &model_id).unwrap_or(true);
				if !supports_images {
					return Err(Error::new(format!(
						"model '{provider}/{model_id}' does not support image input"
					)));
				}
			}
		}

		let (turn_parts, user_content) = if has_images {
			let mut assembled = Vec::new();
			if !full_prompt.is_empty() {
				assembled.push(ContentPart::text(&full_prompt));
			}
			for p in parts {
				if p.is_image() {
					assembled.push(p);
				}
			}
			(assembled.clone(), MessageContent::Parts(assembled))
		} else {
			(Vec::new(), MessageContent::Text(full_prompt.clone()))
		};

		let user_id = format!("msg-{}", output::session_id());
		let parent_id = self
			.store
			.messages()
			.last()
			.map(|m| m.id.clone())
			.filter(|id| !id.is_empty());

		let user_entry = MessageEntry::new(
			user_id.clone(),
			parent_id,
			&now_iso,
			SessionMessage::user(user_content, now_ms),
		);
		if self.store.enabled() {
			if let Err(err) = user_entry.validate() {
				eprintln!("warning: {err}");
			} else if let Err(err) = self.store.append_message(&user_entry) {
				eprintln!("warning: {err}");
			}
		}

		let mut current_parent_id = Some(user_id);
		let mut tool_name_by_id: std::collections::HashMap<String, String> =
			std::collections::HashMap::new();

		let cancel_token = pi_agent::CancellationToken::new();
		let mut request = self.request(full_prompt.clone(), turn_parts.clone(), mode);
		request.cancellation_token = Some(cancel_token.clone());
		let started = std::time::Instant::now();
		let mut builder = if !turn_parts.is_empty() {
			TurnBuilder::new_with_parts(full_prompt, turn_parts)
		} else {
			TurnBuilder::new(full_prompt)
		};
		let mut stream = self.runner.run(&request)?;
		let mut reporter_failed = false;

		let session_config = &self.config;
		let mut handle_event = |event: &agent::Event,
		                        store: &mut SessionStore,
		                        parent: &mut Option<String>,
		                        tools: &mut std::collections::HashMap<String, String>,
		                        model_fallback: Option<&str>,
		                        provider_fallback: Option<&str>| {
			match event {
				agent::Event::ToolStart { id, tool, .. } => {
					tools.insert(id.clone(), tool.clone());
				},
				agent::Event::TurnEnd {
					message,
					tool_results,
					stop_reason,
					api,
					provider,
					model,
				} => {
					let now_evt = std::time::SystemTime::now();
					let evt_iso = output::iso8601(now_evt);
					let evt_ms = now_evt
						.duration_since(std::time::UNIX_EPOCH)
						.unwrap_or_default()
						.as_millis() as u64;

					let mut parts = Vec::new();
					if !message.content.is_empty() {
						parts.push(ContentPart::text(message.content.clone()));
					}
					for tc in &message.tool_calls {
						let mut part = ContentPart {
							kind: "toolCall".to_string(),
							text: None,
							data: None,
							mime_type: None,
							extra: serde_json::Map::new(),
						};
						part.extra
							.insert("id".to_string(), serde_json::json!(tc.id));
						part.extra
							.insert("name".to_string(), serde_json::json!(tc.name));
						part.extra
							.insert("arguments".to_string(), tc.arguments.clone());
						parts.push(part);
					}
					let content = if parts.is_empty() {
						MessageContent::Text(String::new())
					} else {
						MessageContent::Parts(parts)
					};

					let asst_api = api
						.clone()
						.or_else(|| {
							provider
								.as_deref()
								.or(provider_fallback)
								.and_then(|p| session_config.providers.get(p).and_then(|c| c.api.clone()))
						})
						.unwrap_or_default();
					let asst_provider = provider
						.clone()
						.or_else(|| provider_fallback.map(str::to_string))
						.unwrap_or_default();
					let asst_model = model
						.clone()
						.or_else(|| model_fallback.map(str::to_string))
						.unwrap_or_default();

					let asst_id = format!("msg-{}", output::session_id());
					let canon_stop = session::canonical_stop_reason(&stop_reason);
					let mut asst_msg = SessionMessage::assistant(
						content,
						asst_api,
						asst_provider,
						asst_model,
						None,
						canon_stop,
						evt_ms,
					);
					if canon_stop == "aborted" {
						asst_msg.error_message = Some("Interrupted by user".to_string());
					}
					let asst_entry = MessageEntry::new(
						asst_id.clone(),
						parent.take(),
						&evt_iso,
						asst_msg,
					);

					let mut turn_entries = vec![asst_entry];
					let mut prev_id = asst_id;

					for tr in tool_results {
						let res_id = format!("msg-{}", output::session_id());
						let tool_name = tools
							.get(&tr.tool_call_id)
							.cloned()
							.unwrap_or_default();
						let mut tool_msg = SessionMessage::tool_result(
							tr.tool_call_id.clone(),
							tool_name,
							MessageContent::Text(tr.content.clone()),
							tr.is_error,
							evt_ms,
						);
						if let Some(details) = &tr.details {
							tool_msg.extra.insert("details".to_string(), details.clone());
						}
						let tool_entry = MessageEntry::new(
							res_id.clone(),
							Some(prev_id.clone()),
							&evt_iso,
							tool_msg,
						);
						turn_entries.push(tool_entry);
						prev_id = res_id;
					}

					let mut valid_turn_entries = Vec::new();
					for entry in turn_entries {
						if let Err(err) = entry.validate() {
							eprintln!("warning: {err}");
						} else {
							valid_turn_entries.push(entry);
						}
					}
					if !valid_turn_entries.is_empty() {
						if let Err(err) = store.append_messages(&valid_turn_entries) {
							eprintln!("warning: {err}");
						}
					}
					*parent = Some(prev_id);
				},
				_ => {},
			}
		};

		let model_opt = self.model.clone();
		let provider_opt = self.provider.clone();

		let drive_result = deadline::drive(
			&mut stream,
			self.deadline,
			|| started.elapsed(),
			|event| {
				builder.observe(event);
				handle_event(
					event,
					&mut self.store,
					&mut current_parent_id,
					&mut tool_name_by_id,
					model_opt.as_deref(),
					provider_opt.as_deref(),
				);
				if let Err(err) = reporter.event(event) {
					reporter_failed = true;
					return Err(err);
				}
				Ok(())
			},
		);

		if drive_result.is_err() {
			cancel_token.cancel();
			for event in stream {
				if let Ok(event) = event {
					builder.observe(&event);
					handle_event(
						&event,
						&mut self.store,
						&mut current_parent_id,
						&mut tool_name_by_id,
						model_opt.as_deref(),
						provider_opt.as_deref(),
					);
					if !reporter_failed {
						let _ = reporter.event(&event);
					}
				}
			}
		}

		let mut record = builder.finish();
		if let Err(err) = drive_result {
			let canon = session::canonical_stop_reason(&record.stop_reason);
			if record.stop_reason.is_empty() || canon == "toolUse" {
				record.stop_reason = "aborted".to_string();
			}
			self.history.push(record);
			return Err(err);
		}
		self.history.push(record);
		Ok(())
	}
}

fn resolve_cwd(cli_cwd: Option<PathBuf>, config_cwd: Option<PathBuf>) -> Result<PathBuf> {
	let base = std::env::current_dir()?;
	let Some(path) = cli_cwd.or(config_cwd) else {
		return Ok(base);
	};
	let path = dirs::absolute(&base, &path);
	std::fs::canonicalize(&path)
		.map_err(|err| Error::with_source(format!("cwd {} is not accessible", path.display()), err))
}


#[cfg(unix)]
fn open_file_safe(path: &Path) -> std::io::Result<std::fs::File> {
	use std::os::unix::fs::OpenOptionsExt;
	#[cfg(all(unix, not(any(target_os = "macos", target_os = "ios", target_os = "freebsd", target_os = "netbsd", target_os = "openbsd", target_os = "dragonfly"))))]
	const O_NONBLOCK: i32 = 0o4000;
	#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd", target_os = "netbsd", target_os = "openbsd", target_os = "dragonfly"))]
	const O_NONBLOCK: i32 = 0x0004;

	let mut options = std::fs::OpenOptions::new();
	options.read(true);
	options.custom_flags(O_NONBLOCK);
	options.open(path)
}

#[cfg(not(unix))]
fn open_file_safe(path: &Path) -> std::io::Result<std::fs::File> {
	std::fs::File::open(path)
}

// Mirrors TS `readPipedInput` (packages/coding-agent/src/main.ts:199-217): the read is
// unbounded — a timer that stops early would silently drop input — but the wait must be
// audible, so say what we are blocked on once per silent interval.
const STDIN_WAIT_NOTICE_MS: std::os::raw::c_int = 1_000;
const STDIN_WAIT_NOTICE_REPEAT_MS: i32 = 10_000;

#[cfg(unix)]
fn poll_stdin(timeout_ms: std::os::raw::c_int) -> std::io::Result<bool> {
	#[repr(C)]
	struct PollFd {
		fd: std::os::raw::c_int,
		events: std::os::raw::c_short,
		revents: std::os::raw::c_short,
	}
	// `read` can proceed on any of these: a hangup means the writer closed (read yields 0/EOF),
	// an error surfaces through `read` itself. Treating only POLLIN as ready would spin forever
	// on an EOF'd pipe, where Linux reports POLLHUP and no POLLIN.
	const READY: std::os::raw::c_short = 0x0001 | 0x0008 | 0x0010 | 0x0020; // POLLIN|POLLERR|POLLHUP|POLLNVAL
	const EINTR: i32 = 4;
	unsafe extern "C" {
		fn poll(fds: *mut PollFd, nfds: usize, timeout_ms: std::os::raw::c_int) -> std::os::raw::c_int;
	}
	let mut pfd = PollFd {
		fd: 0,
		events: 0x0001,
		revents: 0,
	};
	loop {
		let ret = unsafe { poll(&mut pfd, 1, timeout_ms) };
		if ret == 0 {
			return Ok(false);
		}
		if ret > 0 {
			return Ok((pfd.revents & READY) != 0);
		}
		// Interrupted by a signal: retry. Anything else must surface, not masquerade as a timeout.
		let err = std::io::Error::last_os_error();
		if err.raw_os_error() != Some(EINTR) {
			return Err(err);
		}
		pfd.revents = 0;
	}
}

/// Reads stdin to EOF, printing the wait notice while a pipe stays silent.
#[cfg(unix)]
fn read_stdin_to_eof() -> std::io::Result<Vec<u8>> {
	let mut bytes = Vec::new();
	let mut buf = [0u8; 8192];
	let mut silent_ms: i32 = 0;
	loop {
		if !poll_stdin(STDIN_WAIT_NOTICE_MS)? {
			silent_ms = silent_ms.saturating_add(STDIN_WAIT_NOTICE_MS);
			if silent_ms == STDIN_WAIT_NOTICE_MS || silent_ms % STDIN_WAIT_NOTICE_REPEAT_MS == 0 {
				eprintln!("Reading prompt from piped stdin (waiting for EOF; ctrl+c to abort)…");
			}
			continue;
		}
		silent_ms = 0;
		match io::stdin().read(&mut buf) {
			Ok(0) => break,
			Ok(n) => bytes.extend_from_slice(&buf[..n]),
			Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
			Err(err) => return Err(err),
		}
	}
	Ok(bytes)
}

#[cfg(not(unix))]
fn read_stdin_to_eof() -> std::io::Result<Vec<u8>> {
	let mut bytes = Vec::new();
	io::stdin().read_to_end(&mut bytes)?;
	Ok(bytes)
}

fn read_piped_stdin() -> std::io::Result<Option<String>> {
	if io::stdin().is_terminal() {
		return Ok(None);
	}
	let bytes = read_stdin_to_eof()?;
	let text = String::from_utf8(bytes).map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
	if text.trim().is_empty() {
		Ok(None)
	} else {
		Ok(Some(text))
	}
}

#[derive(Debug, Default, Clone)]
struct ProcessedCliFiles {
	text: String,
	images: Vec<ContentPart>,
}

fn process_cli_file_args(file_args: &[String], cwd: &Path) -> std::result::Result<ProcessedCliFiles, CliError> {
	let mut text = String::new();
	let mut images = Vec::new();
	for file_arg in file_args {
		let resolved = file_mentions::resolve_path(file_arg, cwd);
		let abs_path = dirs::absolute(cwd, &resolved);

		if abs_path.is_dir() {
			let listing = file_mentions::build_directory_listing(&abs_path).map_err(|err| {
				CliError::fail(format!(
					"Error: Could not read directory {}: {err}",
					abs_path.display()
				))
			})?;
			text.push_str(&format!(
				"<file name=\"{}\">\n{}\n</file>\n",
				abs_path.display(),
				listing.output
			));
			continue;
		}

		let mut file = match open_file_safe(&abs_path) {
			Ok(f) => f,
			Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
				return Err(CliError::fail(format!(
					"Error: File not found: {}",
					abs_path.display()
				)));
			},
			Err(err) => {
				return Err(CliError::fail(format!(
					"Error: Could not read file {}: {err}",
					abs_path.display()
				)));
			},
		};

		let opened_meta = match file.metadata() {
			Ok(m) => m,
			Err(err) => {
				return Err(CliError::fail(format!(
					"Error: Could not read file {}: {err}",
					abs_path.display()
				)));
			},
		};

		if !opened_meta.is_file() {
			return Err(CliError::fail(format!(
				"Error: Could not read file {}: not a regular file",
				abs_path.display()
			)));
		}

		if opened_meta.len() == 0 {
			continue;
		}

		let mut header = [0u8; 512];
		let mut n = 0;
		while n < header.len() {
			match file.read(&mut header[n..]) {
				Ok(0) => break,
				Ok(bytes_read) => n += bytes_read,
				Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
				Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
				Err(err) => {
					return Err(CliError::fail(format!(
						"Error: Could not read file {}: {err}",
						abs_path.display()
					)));
				},
			}
		}

		let detected_image_mime = file_mentions::detect_image_mime(&header[..n]);
		let initial_buf = header[..n].to_vec();

		if let Some(mime) = detected_image_mime {
			let max_bytes = file_mentions::MAX_AUTO_READ_IMAGE_BYTES;
			let bytes = match file_mentions::read_bounded(
				&mut file,
				initial_buf,
				max_bytes,
			) {
				Ok(b) => b,
				Err(file_mentions::BoundedReadError::TooLarge { actual_read }) => {
					let disk_size = file.metadata().map(|m| m.len()).unwrap_or(0);
					let reported_size = if disk_size > max_bytes {
						disk_size
					} else {
						actual_read
					};
					eprintln!(
						"Warning: Skipping file contents (too large: {}): {}",
						file_mentions::format_bytes(reported_size),
						abs_path.display()
					);
					text.push_str(&format!(
						"<file name=\"{}\">(skipped: too large, {})</file>\n",
						abs_path.display(),
						file_mentions::format_bytes(reported_size)
					));
					continue;
				},
				Err(file_mentions::BoundedReadError::Io(err)) => {
					return Err(CliError::fail(format!(
						"Error: Could not read file {}: {err:?}",
						abs_path.display()
					)));
				},
			};

			let b64 = file_mentions::base64_encode(&bytes);
			text.push_str(&format!("<file name=\"{}\"></file>\n", abs_path.display()));
			images.push(ContentPart::image(b64, mime));
		} else {
			let max_bytes = file_mentions::MAX_AUTO_READ_TEXT_BYTES;
			let bytes = match file_mentions::read_bounded(
				&mut file,
				initial_buf,
				max_bytes,
			) {
				Ok(b) => b,
				Err(file_mentions::BoundedReadError::TooLarge { actual_read }) => {
					let disk_size = file.metadata().map(|m| m.len()).unwrap_or(0);
					let reported_size = if disk_size > max_bytes {
						disk_size
					} else {
						actual_read
					};
					eprintln!(
						"Warning: Skipping file contents (too large: {}): {}",
						file_mentions::format_bytes(reported_size),
						abs_path.display()
					);
					text.push_str(&format!(
						"<file name=\"{}\">(skipped: too large, {})</file>\n",
						abs_path.display(),
						file_mentions::format_bytes(reported_size)
					));
					continue;
				},
				Err(file_mentions::BoundedReadError::Io(err)) => {
					return Err(CliError::fail(format!(
						"Error: Could not read file {}: {err:?}",
						abs_path.display()
					)));
				},
			};

			let content = String::from_utf8_lossy(&bytes);
			text.push_str(&format!(
				"<file name=\"{}\">\n{content}\n</file>\n",
				abs_path.display()
			));
		}
	}
	Ok(ProcessedCliFiles { text, images })
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn model_switch_clears_inherited_cache_key_but_keeps_explicit() {
		let mut key = Some("header-cache".to_string());
		let mut source = Some(PromptCacheKeySource::Inherited);
		invalidate_inherited_cache_key(&mut key, &mut source);
		assert_eq!(key, None);
		assert_eq!(source, None);

		let mut key = Some("explicit-cache".to_string());
		let mut source = Some(PromptCacheKeySource::Explicit);
		invalidate_inherited_cache_key(&mut key, &mut source);
		assert_eq!(key.as_deref(), Some("explicit-cache"));
		assert_eq!(source, Some(PromptCacheKeySource::Explicit));
	}
}
