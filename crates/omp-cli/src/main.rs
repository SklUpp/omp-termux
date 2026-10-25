//! `omp` binary entry point: resolve files, then hand turns to the runner.

use std::io::{self, BufRead, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use omp_cli::{
	Error, Result,
	agent::{self, AgentRunner, RunRequest},
	approve::Approvals,
	args::{ApprovalMode, Cli, Command},
	catalog::Catalog,
	config::Config,
	dirs,
	output::{OutputMode, Reporter, session_id},
	prompts::{self, Prompts},
	repl,
	runtime::{self, PiAgentRunner},
	skills::{self, Skill},
};

fn main() {
	if let Err(error) = run() {
		eprintln!("{error}");
		std::process::exit(1);
	}
}

fn run() -> Result<()> {
	let cli = Cli::parse();
	if agent::install(Box::new(PiAgentRunner::new()?)).is_err() {
		return Err(Error::new("agent runtime already registered"));
	}
	// Unreachable in the binary (registration happens above); kept so a library
	// caller that skips `install` still fails honestly instead of pretending.
	let Some(runner) = agent::runner() else {
		return Err(Error::new(agent::MISSING_RUNTIME));
	};

	let config = Config::load(&cli.config)?;
	let cwd = resolve_cwd(cli.cwd.clone(), config.cwd.clone())?;
	let prompt_inputs = (cli.system_prompt.clone(), cli.append_system_prompt.clone());
	let loaded_prompts =
		prompts::resolve(prompt_inputs.0.as_deref(), prompt_inputs.1.as_deref(), &cwd)?;
	let skills_filter = (cli.no_skills, cli.skills.clone());
	let loaded_skills = skills::load(&cwd, skills_filter.0, &skills_filter.1)?;
	let catalog = Arc::new(Catalog::load(&cwd)?);
	let approvals = Approvals::new(
		cli.approval_mode
			.or_else(|| cli.yolo.then_some(ApprovalMode::Yolo))
			.or(config.approval_mode)
			.unwrap_or_default(),
		cli.allow
			.iter()
			.cloned()
			.chain(config.allow.iter().cloned()),
	);
	let default_model = config.default_model.clone();
	let requested_model = cli.model.clone();
	let requested_provider = cli.provider.clone();
	let requested_api_key = cli.api_key.clone();
	let mut session = Session {
		runner,
		config,
		cwd,
		prompts: loaded_prompts,
		skills: loaded_skills,
		catalog,
		approvals,
		model: None,
		provider: None,
		api_key: None,
		max_time: cli.max_time,
		max_turns: runtime::DEFAULT_MAX_TURNS,
		tools_enabled: !cli.no_tools,
		prompt_inputs,
		skills_filter,
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

	let mode = cli.mode;
	let mut reporter = Reporter::new(mode, io::stdout(), io::stderr());
	reporter.session_header(&session_id(), &session.cwd)?;

	let repl_mode = matches!(cli.command.as_ref(), Some(Command::Repl))
		|| (cli.command.is_none() && cli.prompt.is_empty() && !cli.print);
	if repl_mode {
		return run_repl(&mut session, mode, &mut reporter);
	}

	let mut prompts_to_run = cli.prompt.clone();
	if prompts_to_run.is_empty() {
		let mut text = String::new();
		io::stdin().read_to_string(&mut text)?;
		if text.trim().is_empty() {
			return Err(Error::new("no prompt provided: pass a prompt argument or pipe one on stdin"));
		}
		prompts_to_run.push(text);
	}
	for prompt in prompts_to_run {
		session.turn(prompt, mode, &mut reporter)?;
	}
	reporter.finish()
}

fn run_repl<W: Write, E: Write>(
	session: &mut Session<'_>,
	mode: OutputMode,
	reporter: &mut Reporter<W, E>,
) -> Result<()> {
	let stdin = io::stdin();
	let mut input = stdin.lock();
	let mut line = String::new();
	loop {
		eprint!("> ");
		io::stderr().flush()?;
		line.clear();
		if input.read_line(&mut line)? == 0 {
			break;
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
			repl::ReplCommand::Prompt(text) => session.turn(text, mode, reporter)?,
		}
	}
	reporter.finish()
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
	max_time: Option<Duration>,
	max_turns: usize,
	tools_enabled: bool,
	/// Raw `--system-prompt` / `--append-system-prompt` inputs, kept so a REPL
	/// `/cwd` re-resolves the project files for the new directory.
	prompt_inputs: (Option<String>, Option<String>),
	skills_filter: (bool, Vec<String>),
}

impl Session<'_> {
	fn select_model(&mut self, id: String) {
		self.provider = self.catalog.provider_of(&id).map(str::to_string);
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

	fn request(&self, prompt: String, output_mode: OutputMode) -> RunRequest {
		RunRequest {
			prompt,
			cwd: self.cwd.clone(),
			output_mode,
			model: self.model.clone(),
			provider: self.provider.clone(),
			api_key: self.api_key.clone(),
			system_prompt: self.prompts.system.clone(),
			append_system_prompt: self.prompts.append.clone(),
			context_files: self.prompts.files.clone(),
			skills: self.skills.clone(),
			config: self.config.clone(),
			catalog: Arc::clone(&self.catalog),
			approvals: self.approvals.clone(),
			max_time: self.max_time,
			max_turns: self.max_turns,
			tools_enabled: self.tools_enabled,
		}
	}

	fn turn<W: Write, E: Write>(
		&self,
		prompt: String,
		mode: OutputMode,
		reporter: &mut Reporter<W, E>,
	) -> Result<()> {
		reporter.begin_turn()?;
		for event in self.runner.run(&self.request(prompt, mode))? {
			reporter.event(&event?)?;
		}
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
