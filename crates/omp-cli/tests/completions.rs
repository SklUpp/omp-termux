//! Behavioral tests for `omp completions` and the hidden `omp __complete`
//! helper: real script generation, real bash evaluation of the generated
//! function, and the dynamic model/session endpoints the scripts call.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcCommand, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use omp_cli::{EXIT_FAILURE, EXIT_SUCCESS};

/// Isolated HOME/config/share/session roots plus a project directory.
struct Sandbox {
	path: PathBuf,
}

impl Sandbox {
	fn new(prefix: &str) -> Self {
		static COUNTER: AtomicUsize = AtomicUsize::new(0);
		let path = std::env::temp_dir().join(format!(
			"omp-test-completions-{}-{}-{}",
			prefix,
			std::process::id(),
			COUNTER.fetch_add(1, Ordering::SeqCst)
		));
		fs::create_dir_all(path.join("home/project")).expect("create home");
		fs::create_dir_all(path.join("share")).expect("create share");
		Self { path }
	}

	fn path(&self) -> &Path {
		&self.path
	}

	fn home(&self) -> PathBuf {
		self.path.join("home")
	}

	fn work(&self) -> PathBuf {
		self.path.join("home/project")
	}

	fn agent(&self) -> PathBuf {
		self.path.join("agent")
	}
}

impl Drop for Sandbox {
	fn drop(&mut self) {
		let _ = fs::remove_dir_all(&self.path);
	}
}

fn omp() -> ProcCommand {
	ProcCommand::new(env!("CARGO_BIN_EXE_omp"))
}

/// Point every config/session location at the sandbox and run inside its
/// project directory, so no host state leaks into the child process.
fn isolate(command: &mut ProcCommand, sandbox: &Sandbox) {
	command
		.env("HOME", sandbox.home())
		.env("PI_CONFIG_DIR", sandbox.path().join("config"))
		.env("PI_CODING_AGENT_DIR", sandbox.agent())
		.env("OMP_SHARE_DIR", sandbox.path().join("share"))
		.env_remove("PREFIX")
		.env_remove("OMP_PROFILE")
		.env_remove("PI_PROFILE")
		.env_remove("PI_CONFIG_FILES")
		.env_remove("XDG_DATA_HOME")
		.env_remove("XDG_STATE_HOME")
		.env_remove("XDG_CACHE_HOME")
		.current_dir(sandbox.work());
}

fn run(mut command: ProcCommand) -> Output {
	command.output().expect("run omp")
}

fn stderr(output: &Output) -> String {
	String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
	String::from_utf8(output.stdout.clone()).expect("utf8 stdout")
}

/// Pre-migration absolute-path session directory for the sandbox project.
fn legacy_session_dir(sandbox: &Sandbox) -> PathBuf {
	let cwd = fs::canonicalize(sandbox.work()).expect("canonical project");
	let stripped = cwd.to_string_lossy().trim_start_matches(['/', '\\']).to_string();
	let encoded = stripped.replace(['/', '\\', ':'], "-");
	sandbox.agent().join("sessions").join(format!("--{encoded}--"))
}

fn completion_script(sandbox: &Sandbox, shell: &str) -> String {
	let mut command = omp();
	command.args(["completions", shell]);
	isolate(&mut command, sandbox);
	let output = run(command);
	assert_eq!(
		output.status.code(),
		Some(i32::from(EXIT_SUCCESS)),
		"{shell} generation failed: {}",
		stderr(&output)
	);
	stdout(&output)
}

#[test]
fn prints_a_script_for_every_supported_shell() {
	let sandbox = Sandbox::new("scripts");
	for (shell, header) in [
		("bash", "# bash completion for omp"),
		("zsh", "#compdef omp"),
		("fish", "# fish completion for omp"),
	] {
		let script = completion_script(&sandbox, shell);
		assert!(script.starts_with(header), "{shell} header: {script:.120}");
		assert!(script.ends_with('\n'), "{shell} script must end with a newline");
		assert!(
			script.contains("__complete models"),
			"{shell} must resolve model values dynamically"
		);
		assert!(
			script.contains("__complete sessions") || script.contains("_omp_call sessions"),
			"{shell} must resolve session values dynamically"
		);
		assert!(!script.contains("__complete provider"), "{shell} invented an endpoint");
		assert!(
			!script.contains("_omp_cmd___complete"),
			"{shell} offered the hidden helper as a subcommand"
		);
	}
	// Script generation runs before config, sessions, and the agent runtime.
	assert!(!sandbox.agent().exists(), "completions bootstrapped the agent dir");
	assert!(
		!sandbox.path().join("config").exists(),
		"completions touched the config dir"
	);
}

#[test]
fn missing_or_unknown_shell_is_a_usage_failure() {
	let sandbox = Sandbox::new("usage");
	for args in [vec!["completions"], vec!["completions", "powershell"]] {
		let mut command = omp();
		command.args(&args);
		isolate(&mut command, &sandbox);
		let output = run(command);
		assert_eq!(
			output.status.code(),
			Some(i32::from(EXIT_FAILURE)),
			"{args:?} must exit 1"
		);
		assert_eq!(stderr(&output), "Usage: omp completions <bash|zsh|fish>\n");
		assert!(output.stdout.is_empty());
	}
}

const BASH_PROBE: &str = r#"source "$1" || exit 9
emit() {
	printf '@@\n'
	printf '%s\n' "${COMPREPLY[@]}"
}
COMP_WORDS=(omp --mo); COMP_CWORD=1; _omp; emit
COMP_WORDS=(omp --approval-mode ''); COMP_CWORD=2; _omp; emit
COMP_WORDS=(omp completions ba); COMP_CWORD=2; _omp; emit
COMP_WORDS=(omp --tools re); COMP_CWORD=2; _omp; emit
COMP_WORDS=(omp --model local/); COMP_CWORD=2; _omp; emit
COMP_WORDS=(omp --mode ''); COMP_CWORD=2; _omp; emit
COMP_WORDS=(omp completions ''); COMP_CWORD=2; _omp; emit
COMP_WORDS=(omp --tools ''); COMP_CWORD=2; _omp; emit
"#;

/// Split the probe output into the COMPREPLY set of each `emit` call.
fn probe_groups(text: &str) -> Vec<Vec<String>> {
	text.split("@@\n")
		.skip(1)
		.map(|block| {
			block
				.lines()
				.filter(|line| !line.is_empty())
				.map(str::to_string)
				.collect()
		})
		.collect()
}

#[test]
fn bash_script_parses_and_completes_real_arguments() {
	let sandbox = Sandbox::new("bash");
	fs::write(
		sandbox.work().join("models.json"),
		r#"{"local":{"gpt":{"id":"gpt","provider":"local"}}}"#,
	)
	.expect("models fixture");
	let script = completion_script(&sandbox, "bash");
	let script_path = sandbox.path().join("omp.bash");
	fs::write(&script_path, &script).expect("write script");

	let syntax = ProcCommand::new("bash")
		.arg("-n")
		.arg(&script_path)
		.output()
		.expect("bash is required for the completion tests");
	assert!(
		syntax.status.success(),
		"generated bash script is not valid: {}",
		String::from_utf8_lossy(&syntax.stderr)
	);

	let binary_dir = Path::new(env!("CARGO_BIN_EXE_omp"))
		.parent()
		.expect("binary directory")
		.to_path_buf();
	let mut probe = ProcCommand::new("bash");
	probe.arg("-c").arg(BASH_PROBE).arg("bash").arg(&script_path);
	isolate(&mut probe, &sandbox);
	probe.env(
		"PATH",
		format!(
			"{}:{}",
			binary_dir.display(),
			std::env::var("PATH").unwrap_or_default()
		),
	);
	let output = run(probe);
	assert!(
		output.status.success(),
		"bash probe failed: {}",
		String::from_utf8_lossy(&output.stderr)
	);
	assert!(
		output.stderr.is_empty(),
		"bash probe wrote to stderr: {}",
		String::from_utf8_lossy(&output.stderr)
	);

	let groups = probe_groups(&stdout(&output));
	assert_eq!(groups.len(), 8, "missing probe groups: {groups:?}");

	let mut root_flags = groups[0].clone();
	root_flags.sort();
	assert!(root_flags.contains(&"--mode".to_string()), "{root_flags:?}");
	assert!(root_flags.contains(&"--model".to_string()), "{root_flags:?}");

	let mut approval = groups[1].clone();
	approval.sort();
	assert_eq!(approval, ["always-ask", "write", "yolo"]);

	assert_eq!(groups[2], ["bash"], "shell operand completion");
	assert_eq!(groups[3], ["read"], "comma-separated --tools completion");
	assert_eq!(groups[4], ["local/gpt"], "dynamic --model completion");

	let mut modes = groups[5].clone();
	modes.sort();
	assert_eq!(modes, ["json", "text"], "--mode completion");

	let mut shells = groups[6].clone();
	shells.sort();
	assert_eq!(shells, ["bash", "fish", "zsh"], "completions shell operand");

	let mut tools = groups[7].clone();
	tools.sort();
	let mut expected_tools: Vec<String> = omp_cli::runtime::BUILTIN_TOOLS
		.iter()
		.map(|(tool, _)| (*tool).to_string())
		.collect();
	expected_tools.sort();
	assert_eq!(tools, expected_tools, "--tools completes every builtin tool");
}

#[test]
fn complete_models_reads_the_catalog_and_filters_by_prefix() {
	let sandbox = Sandbox::new("models");
	fs::write(
		sandbox.work().join("models.json"),
		r#"{
			"mock": {"alpha": {"id": "alpha", "provider": "mock"}},
			"other": {"beta": {"id": "beta", "provider": "other"}}
		}"#,
	)
	.expect("models fixture");

	let mut command = omp();
	command.args(["__complete", "models"]);
	isolate(&mut command, &sandbox);
	let output = run(command);
	assert_eq!(output.status.code(), Some(i32::from(EXIT_SUCCESS)), "{}", stderr(&output));
	assert_eq!(
		stdout(&output).lines().collect::<Vec<_>>(),
		["alpha\tmock", "beta\tother", "mock/alpha\tmock", "other/beta\tother"]
	);

	let mut filtered = omp();
	filtered.args(["__complete", "models", "--", "mock/"]);
	isolate(&mut filtered, &sandbox);
	let output = run(filtered);
	assert_eq!(stdout(&output), "mock/alpha\tmock\n");
}

#[test]
fn complete_sessions_reports_ids_titles_and_honors_the_prefix() {
	let sandbox = Sandbox::new("sessions");
	// `compute_default_session_dir` encodes the project as `-project` when the
	// cwd sits directly under $HOME.
	let sessions = sandbox.agent().join("sessions/-project");
	fs::create_dir_all(&sessions).expect("sessions dir");
	let titled = sessions.join("aaa111.jsonl");
	let untitled = sessions.join("bbb222.jsonl");
	let described = sessions.join("ccc333.jsonl");
	let slot_titled = sessions.join("ddd444.jsonl");
	fs::write(
		&titled,
		"{\"type\":\"session\",\"version\":3,\"id\":\"aaa111\",\"timestamp\":\"2026-10-28T00:00:00Z\",\
		 \"cwd\":\"/work\",\"title\":\"Fix the build\"}\n",
	)
	.expect("titled session");
	fs::write(&untitled, "not a header\n").expect("untitled session");
	fs::write(
		&slot_titled,
		concat!(
			"{\"type\":\"title\",\"v\":1,\"title\":\"Slot title\",\"source\":\"auto\",\
			 \"updatedAt\":\"2026-10-28T00:00:03Z\",\"pad\":\"\"}\n",
			"{\"type\":\"session\",\"version\":3,\"id\":\"ddd444\",\
			 \"timestamp\":\"2026-10-28T00:00:03Z\",\"cwd\":\"/work\",\"title\":null}\n",
		),
	)
	.expect("title-slot session");
	fs::write(
		&described,
		concat!(
			"{\"type\":\"session\",\"version\":3,\"id\":\"ccc333\",\
			 \"timestamp\":\"2026-10-28T00:00:01Z\",\"cwd\":\"/work\"}\n",
			"{\"type\":\"message\",\"id\":\"u1\",\"parentId\":null,\
			 \"timestamp\":\"2026-10-28T00:00:02Z\",\
			 \"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"Describe the bug\"}]}}\n",
		),
	)
	.expect("first-message session");
	let age = |path: &Path, seconds: u64| {
		fs::File::options()
			.write(true)
			.open(path)
			.expect("open session")
			.set_modified(SystemTime::now() - Duration::from_secs(seconds))
			.expect("age session");
	};
	age(&titled, 120);
	age(&described, 60);
	age(&slot_titled, 30);

	let mut command = omp();
	command.args(["__complete", "sessions"]);
	isolate(&mut command, &sandbox);
	let output = run(command);
	assert_eq!(output.status.code(), Some(i32::from(EXIT_SUCCESS)), "{}", stderr(&output));
	assert_eq!(
		stdout(&output).lines().collect::<Vec<_>>(),
		[
			"bbb222\t",
			"ddd444\tSlot title",
			"ccc333\tDescribe the bug",
			"aaa111\tFix the build"
		],
		"newest first; id from the header else the stem; title slot/header title else first user message"
	);

	let mut filtered = omp();
	filtered.args(["__complete", "sessions", "--", "aaa"]);
	isolate(&mut filtered, &sandbox);
	let output = run(filtered);
	assert_eq!(stdout(&output), "aaa111\tFix the build\n");
}

#[test]
fn session_completion_is_read_only_and_lists_legacy_dirs() {
	let sandbox = Sandbox::new("legacy-readonly");
	let legacy_dir = legacy_session_dir(&sandbox);
	fs::create_dir_all(&legacy_dir).expect("legacy sessions dir");
	let legacy_file = legacy_dir.join("legacy111.jsonl");
	fs::write(
		&legacy_file,
		"{\"type\":\"session\",\"version\":3,\"id\":\"legacy111\",\
		 \"timestamp\":\"2026-10-28T00:00:00Z\",\"cwd\":\"/work\",\"title\":\"Legacy\"}\n",
	)
	.expect("legacy session");
	let current_dir = sandbox.agent().join("sessions/-project");
	assert!(!current_dir.exists(), "fixture starts without the encoded directory");

	let mut helper = omp();
	helper.args(["__complete", "sessions"]);
	isolate(&mut helper, &sandbox);
	let output = run(helper);
	assert_eq!(output.status.code(), Some(i32::from(EXIT_SUCCESS)), "{}", stderr(&output));
	assert!(
		stdout(&output).contains("legacy111\tLegacy"),
		"helper must list sessions from the legacy directory: {}",
		stdout(&output)
	);
	assert!(legacy_dir.is_dir(), "helper must not remove the legacy directory");
	assert!(legacy_file.is_file(), "helper must not move the legacy session file");
	assert!(!current_dir.exists(), "helper must not migrate legacy files");

	let script = completion_script(&sandbox, "bash");
	let script_path = sandbox.path().join("readonly.bash");
	fs::write(&script_path, &script).expect("write script");
	let binary_dir = Path::new(env!("CARGO_BIN_EXE_omp"))
		.parent()
		.expect("binary directory")
		.to_path_buf();
	let mut tab = ProcCommand::new("bash");
	tab.arg("-c")
		.arg(
			r#"source "$1"
COMP_WORDS=(omp --resume ''); COMP_CWORD=2; _omp
printf '%s\n' "${COMPREPLY[@]}"
"#,
		)
		.arg("bash")
		.arg(&script_path);
	isolate(&mut tab, &sandbox);
	tab.env(
		"PATH",
		format!(
			"{}:{}",
			binary_dir.display(),
			std::env::var("PATH").unwrap_or_default()
		),
	);
	let output = run(tab);
	assert!(
		output.status.success(),
		"TAB probe failed: {}",
		stderr(&output)
	);
	assert!(
		stdout(&output).contains("legacy111"),
		"TAB must complete legacy sessions: {}",
		stdout(&output)
	);
	assert!(legacy_dir.is_dir(), "TAB must not remove the legacy directory");
	assert!(legacy_file.is_file(), "TAB must not move the legacy session file");
	assert!(!current_dir.exists(), "TAB must not migrate legacy files");
}

#[test]
fn unknown_completion_kinds_print_nothing() {
	let sandbox = Sandbox::new("unknown");
	let mut command = omp();
	command.args(["__complete", "providers", "--", "x"]);
	isolate(&mut command, &sandbox);
	let output = run(command);
	assert_eq!(output.status.code(), Some(i32::from(EXIT_SUCCESS)));
	assert!(output.stdout.is_empty());
	assert!(output.stderr.is_empty());
}

#[test]
fn zsh_and_fish_scripts_parse_when_their_shells_are_installed() {
	let sandbox = Sandbox::new("syntax");
	for (shell, args) in [("zsh", ["-n"]), ("fish", ["--no-execute"])] {
		let available = ProcCommand::new(shell)
			.arg("--version")
			.output()
			.is_ok_and(|output| output.status.success());
		if !available {
			eprintln!("skipping {shell} syntax check: shell not installed");
			continue;
		}
		let script_path = sandbox.path().join(format!("omp.{shell}"));
		fs::write(&script_path, completion_script(&sandbox, shell)).expect("write script");
		let output = ProcCommand::new(shell)
			.args(args)
			.arg(&script_path)
			.output()
			.expect("run shell");
		assert!(
			output.status.success(),
			"generated {shell} script is not valid: {}",
			String::from_utf8_lossy(&output.stderr)
		);
	}
}
