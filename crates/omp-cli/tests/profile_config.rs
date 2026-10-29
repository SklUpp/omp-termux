//! Profile bootstrap and layered settings/config resolution, end to end.
//!
//! Layer contract (highest first, `docs/settings.md`): CLI flags -> `--config`
//! overlays -> `PI_CONFIG_FILES` overlays -> project `.omp/settings.json` then
//! `.omp/config.yml` -> global `agent/config.yml` -> defaults. Profile selection
//! (`--profile`/`OMP_PROFILE`/`PI_PROFILE`) happens before every path helper,
//! so it is verified through the real binary (catalog/session discovery), not
//! by asserting the resolver against itself.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcCommand, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

use omp_cli::approve::ApprovalMode;
use omp_cli::config::Config;

static ENV_LOCK: Mutex<()> = Mutex::new(());
static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn temp_dir(label: &str) -> PathBuf {
	let unique = format!(
		"omp-cli-{label}-{}-{}",
		std::process::id(),
		COUNTER.fetch_add(1, Ordering::Relaxed)
	);
	let dir = std::env::temp_dir().join(unique);
	fs::create_dir_all(&dir).expect("temp dir");
	dir
}

fn omp() -> ProcCommand {
	ProcCommand::new(env!("CARGO_BIN_EXE_omp"))
}

/// Isolated environment for a real CLI run: no host profile, catalog, share
/// dir, or base config can leak into the assertion.
fn isolated(cmd: &mut ProcCommand, root: &Path, home: &Path) {
	cmd.env("PI_CONFIG_DIR", root)
		.env("HOME", home)
		.env("OMP_SHARE_DIR", root.join("share"))
		.env_remove("PI_CODING_AGENT_DIR")
		.env_remove("OMP_PROFILE")
		.env_remove("PI_PROFILE")
		.env_remove("PI_CONFIG_FILES")
		.env_remove("XDG_DATA_HOME")
		.env_remove("XDG_STATE_HOME")
		.env_remove("XDG_CACHE_HOME")
		.stdin(Stdio::null());
}

fn write(path: &Path, contents: &str) {
	if let Some(parent) = path.parent() {
		fs::create_dir_all(parent).expect("parent dir");
	}
	fs::write(path, contents).expect("write fixture");
}

fn stderr(output: &std::process::Output) -> String {
	String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &std::process::Output) -> String {
	String::from_utf8_lossy(&output.stdout).into_owned()
}

fn models_json() -> &'static str {
	r#"{"local":{"m":{"id":"m","provider":"local"}}}"#
}

fn snapshot(dir: &Path) -> BTreeSet<PathBuf> {
	let mut paths = BTreeSet::new();
	let mut stack = vec![dir.to_path_buf()];
	while let Some(current) = stack.pop() {
		let Ok(entries) = fs::read_dir(&current) else { continue };
		for entry in entries.flatten() {
			let path = entry.path();
			if path.is_dir() {
				stack.push(path.clone());
			}
			paths.insert(path);
		}
	}
	paths
}

struct EnvGuard {
	_guard: MutexGuard<'static, ()>,
	previous: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EnvGuard {
	fn set(values: &[(&'static str, Option<&Path>)]) -> Self {
		let guard = ENV_LOCK.lock().expect("env lock");
		let mut previous = Vec::new();
		for (key, value) in values {
			previous.push((*key, std::env::var_os(key)));
			// SAFETY: guarded by ENV_LOCK and restored on drop.
			unsafe {
				match value {
					Some(path) => std::env::set_var(key, path),
					None => std::env::remove_var(key),
				}
			}
		}
		Self { _guard: guard, previous }
	}
}

impl Drop for EnvGuard {
	fn drop(&mut self) {
		// SAFETY: still holding ENV_LOCK.
		unsafe {
			for (key, value) in self.previous.drain(..) {
				match value {
					Some(value) => std::env::set_var(key, value),
					None => std::env::remove_var(key),
				}
			}
		}
	}
}

#[test]
fn config_layers_follow_the_documented_precedence() {
	let root = temp_dir("layers");
	let home = root.join("home");
	let agent = root.join("agent");
	let project = root.join("project");
	let elsewhere = root.join("elsewhere");
	fs::create_dir_all(&home).expect("home");
	fs::create_dir_all(&elsewhere).expect("elsewhere");

	write(
		&agent.join("config.yml"),
		&format!(
			"defaultModel: global/model\nsystemPrompt: global-system\ncwd: {}\nproviders:\n  openai:\n    \
			 baseUrl: https://global\n    apiKey: GLOBAL_KEY\n    headers:\n      X-Global: one\n  \
			 only-global:\n    baseUrl: https://only-global\nallow: [read]\ntools: [read]\n",
			project.display()
		),
	);
	write(
		&project.join(".omp/settings.json"),
		r#"{"modelRoles":{"default":"settings/model"},"tools":{"approvalMode":"write"}}"#,
	);
	write(
		&project.join(".omp/config.yml"),
		"defaultModel: project/model\nproviders:\n  openai:\n    baseUrl: https://project\nallow: \
		 [bash]\ntools: [bash, edit]\n",
	);
	write(&root.join("env1.yml"), "defaultModel: env1/model\nproviders:\n  openai:\n    headers:\n      X-Env: yes\n");
	write(&root.join("env2.yml"), "defaultModel: env2/model\n");
	write(&root.join("cli.yml"), "defaultModel: cli/model\nproviders:\n  openai:\n    baseUrl: https://cli\n");

	let _env = EnvGuard::set(&[
		("PI_CONFIG_DIR", Some(root.as_path())),
		("PI_CODING_AGENT_DIR", Some(agent.as_path())),
		("OMP_PROFILE", None),
		("PI_PROFILE", None),
	]);
	let config = Config::load_with_cwd(
		&[root.join("cli.yml")],
		Some(&project),
		&[root.join("env1.yml"), root.join("env2.yml")],
	)
	.expect("layered config");

	assert_eq!(config.default_model.as_deref(), Some("cli/model"));
	assert_eq!(config.system_prompt.as_deref(), Some("global-system"));
	// settings.json supplied the approval mode; project config.yml did not reset it.
	assert_eq!(config.approval_mode, Some(ApprovalMode::Write));
	assert_eq!(config.allow, ["bash"]);
	assert_eq!(config.tools.as_deref(), Some(["bash".to_string(), "edit".to_string()].as_slice()));
	let openai = config.providers.get("openai").expect("openai");
	assert_eq!(openai.base_url.as_deref(), Some("https://cli"));
	assert_eq!(openai.api_key.as_deref(), Some("GLOBAL_KEY"));
	assert_eq!(openai.headers.get("X-Global").map(String::as_str), Some("one"));
	assert_eq!(openai.headers.get("X-Env").map(String::as_str), Some("yes"));
	assert!(config.providers.contains_key("only-global"));
	assert_eq!(
		config.sources,
		[
			agent.join("config.yml"),
			project.join(".omp/settings.json"),
			project.join(".omp/config.yml"),
			root.join("env1.yml"),
			root.join("env2.yml"),
			root.join("cli.yml"),
		]
	);

	// A different effective cwd must not pick up the project layer.
	let no_project = Config::load_with_cwd(&[], Some(&elsewhere), &[root.join("env2.yml")])
		.expect("without project");
	assert_eq!(no_project.default_model.as_deref(), Some("env2/model"));
	assert_eq!(no_project.allow, ["read"]);

	// Without `--cwd`, the base config `cwd` is the effective directory, so the
	// project layer for that directory applies.
	let by_config_cwd = Config::load_with_cwd(&[], None, &[]).expect("config cwd");
	assert_eq!(by_config_cwd.default_model.as_deref(), Some("project/model"));
	assert_eq!(by_config_cwd.allow, ["bash"]);
	assert_eq!(by_config_cwd.approval_mode, Some(ApprovalMode::Write));
	fs::remove_dir_all(&root).ok();
}

#[test]
fn legacy_global_settings_json_is_a_read_only_fallback() {
	let root = temp_dir("legacy-global");
	let agent = root.join("agent");
	let work = root.join("work");
	fs::create_dir_all(&work).expect("work");
	write(
		&agent.join("settings.json"),
		r#"{"modelRoles":{"default":"legacy/model"},"tools":{"approvalMode":"write"}}"#,
	);
	let _env = EnvGuard::set(&[
		("PI_CONFIG_DIR", Some(root.as_path())),
		("PI_CODING_AGENT_DIR", Some(agent.as_path())),
		("OMP_PROFILE", None),
		("PI_PROFILE", None),
	]);

	let legacy = Config::load_with_cwd(&[], Some(&work), &[]).expect("legacy settings");
	assert_eq!(legacy.default_model.as_deref(), Some("legacy/model"));
	assert_eq!(legacy.approval_mode, Some(ApprovalMode::Write));
	assert_eq!(legacy.sources, [agent.join("settings.json")]);

	// Any main YAML file supersedes the legacy settings file.
	write(&agent.join("config.yml"), "defaultModel: yaml/model\n");
	let yaml = Config::load_with_cwd(&[], Some(&work), &[]).expect("yaml settings");
	assert_eq!(yaml.default_model.as_deref(), Some("yaml/model"));
	assert_eq!(yaml.sources, [agent.join("config.yml")]);
	fs::remove_dir_all(&root).ok();
}

#[test]
fn bootstrap_rejects_bad_profile_values_before_clap() {
	let root = temp_dir("bootstrap");
	let home = root.join("home");
	let work = root.join("work");
	fs::create_dir_all(&home).expect("home");
	fs::create_dir_all(&work).expect("work");
	write(&root.join("agent/models.json"), models_json());

	let run = |args: &[&str], env: &[(&str, &str)]| {
		let mut cmd = omp();
		isolated(&mut cmd, &root, &home);
		cmd.current_dir(&work).env("SHELL", "/bin/bash");
		for (key, value) in env {
			cmd.env(key, value);
		}
		cmd.args(args).output().expect("run")
	};

	for args in [
		&["--profile"][..],
		&["--profile="],
		&["--profile", "--print"],
		&["--alias"],
		&["--alias="],
		&["--alias", "--print"],
	] {
		let output = run(args, &[]);
		assert_eq!(output.status.code(), Some(1), "{args:?}");
		let message = stderr(&output);
		assert!(
			message.contains("requires a profile name") || message.contains("requires a command name"),
			"{args:?} stderr: {message}"
		);
	}

	// `--system-prompt` consumes a flag-looking successor, so `--profile` is
	// literal prompt text and the default agent catalog boots.
	let literal = run(&["--print", "--system-prompt", "--profile"], &[]);
	assert_eq!(literal.status.code(), Some(0), "stderr: {}", stderr(&literal));

	// Environment validation runs before clap, so even `--help` reports it.
	assert_eq!(run(&["--help"], &[]).status.code(), Some(0));
	let invalid = run(&["--help"], &[("OMP_PROFILE", "..")]);
	assert_eq!(invalid.status.code(), Some(1));
	assert!(stderr(&invalid).contains("Invalid OMP profile"), "stderr: {}", stderr(&invalid));
	fs::remove_dir_all(&root).ok();
}

#[test]
fn registered_subcommands_pass_profile_flags_to_clap_validation() {
	let root = temp_dir("subcommand-flags");
	let home = root.join("home");
	let work = root.join("work");
	fs::create_dir_all(&home).expect("home");
	fs::create_dir_all(&work).expect("work");
	fs::write(work.join("file.txt"), "hello").expect("file");

	let run = |args: &[&str], profile: Option<&str>| {
		let mut cmd = omp();
		isolated(&mut cmd, &root, &home);
		cmd.current_dir(&work).env("SHELL", "/bin/bash");
		if let Some(profile) = profile {
			cmd.env("OMP_PROFILE", profile);
		}
		cmd.args(args).output().expect("run")
	};

	// `read` is a registered subcommand, so `--alias` reaches clap and is
	// rejected: it must never install a shell shortcut into the user's rc file.
	let before = snapshot(&home);
	let alias = run(&["read", "--alias", "wk", "file.txt"], Some("work"));
	assert_ne!(alias.status.code(), Some(0), "stdout: {}", stdout(&alias));
	assert!(stderr(&alias).contains("--alias"), "stderr: {}", stderr(&alias));
	assert_eq!(snapshot(&home), before, "`read --alias` must not write rc files");

	// Likewise `--profile` stays with the command instead of being cracked out
	// and activated globally.
	let profile = run(&["read", "--profile", "work", "file.txt"], None);
	assert_ne!(profile.status.code(), Some(0), "stdout: {}", stdout(&profile));
	assert!(stderr(&profile).contains("--profile"), "stderr: {}", stderr(&profile));
	fs::remove_dir_all(&root).ok();
}

#[test]
fn profile_flag_selects_the_profiled_agent_dir() {
	let root = temp_dir("profile-path");
	let home = root.join("home");
	let work = root.join("work");
	fs::create_dir_all(&home).expect("home");
	fs::create_dir_all(&work).expect("work");
	write(&root.join("profiles/work/agent/models.json"), models_json());

	let mut profiled = omp();
	isolated(&mut profiled, &root, &home);
	let profiled = profiled.args(["--profile", "work", "--print"]).current_dir(&work).output().expect("run");
	assert_eq!(profiled.status.code(), Some(0), "stderr: {}", stderr(&profiled));

	let mut plain = omp();
	isolated(&mut plain, &root, &home);
	let plain = plain.arg("--print").current_dir(&work).output().expect("run");
	assert_ne!(plain.status.code(), Some(0));
	assert!(stderr(&plain).contains("no models"), "stderr: {}", stderr(&plain));
	fs::remove_dir_all(&root).ok();
}

#[test]
fn profile_env_precedence_matches_the_typescript_contract() {
	let root = temp_dir("profile-env");
	let home = root.join("home");
	let work = root.join("work");
	fs::create_dir_all(&home).expect("home");
	fs::create_dir_all(&work).expect("work");
	write(&root.join("profiles/work/agent/models.json"), models_json());

	let run = |configure: &dyn Fn(&mut ProcCommand)| {
		let mut cmd = omp();
		isolated(&mut cmd, &root, &home);
		cmd.current_dir(&work).arg("--print");
		configure(&mut cmd);
		cmd.output().expect("run")
	};

	// OMP_PROFILE is canonical, PI_PROFILE the fallback.
	assert_eq!(run(&|cmd| { cmd.env("OMP_PROFILE", "work"); }).status.code(), Some(0));
	assert_eq!(run(&|cmd| { cmd.env("PI_PROFILE", "work"); }).status.code(), Some(0));
	// An explicitly empty OMP_PROFILE selects the default profile instead of
	// falling back to PI_PROFILE, so the profiled catalog is invisible.
	let empty = run(&|cmd| {
		cmd.env("OMP_PROFILE", "").env("PI_PROFILE", "work");
	});
	assert_ne!(empty.status.code(), Some(0));
	assert!(stderr(&empty).contains("no models"), "stderr: {}", stderr(&empty));
	// `--profile default` overrides the env selection entirely.
	let explicit_default = run(&|cmd| {
		cmd.env("OMP_PROFILE", "work").args(["--profile", "default"]);
	});
	assert_ne!(explicit_default.status.code(), Some(0));
	// `--profile` wins over a conflicting OMP_PROFILE.
	assert_eq!(
		run(&|cmd| {
			cmd.env("OMP_PROFILE", "other").args(["--profile", "work"]);
		})
		.status
		.code(),
		Some(0)
	);
	// Invalid and reserved names fail cleanly before any runtime work.
	let mut cases: Vec<String> = ["..", "Work", "CON", "com7", "work.", "a/b"]
		.iter()
		.map(|value| (*value).to_string())
		.collect();
	cases.push("a".repeat(65));
	for value in &cases {
		let output = run(&|cmd| {
			cmd.args(["--profile", value.as_str()]);
		});
		assert_eq!(output.status.code(), Some(1), "{value} must fail");
		assert!(
			stderr(&output).contains("Invalid OMP profile"),
			"{value} stderr: {}",
			stderr(&output)
		);
	}
	let invalid_env = run(&|cmd| {
		cmd.env("OMP_PROFILE", "..");
	});
	assert_eq!(invalid_env.status.code(), Some(1));
	assert!(stderr(&invalid_env).contains("Invalid OMP profile"));
	fs::remove_dir_all(&root).ok();
}

#[test]
fn explicit_agent_dir_is_preserved_only_for_the_default_profile() {
	let root = temp_dir("agent-override");
	let home = root.join("home");
	let custom = root.join("custom");
	let work = root.join("work");
	fs::create_dir_all(&home).expect("home");
	fs::create_dir_all(&work).expect("work");
	write(&custom.join("models.json"), models_json());

	let mut default_run = omp();
	isolated(&mut default_run, &root, &home);
	let default_run = default_run
		.env("PI_CODING_AGENT_DIR", &custom)
		.arg("--print")
		.current_dir(&work)
		.output()
		.expect("run");
	assert_eq!(default_run.status.code(), Some(0), "stderr: {}", stderr(&default_run));

	// A named profile derives its own agent dir and ignores the override.
	let mut profiled = omp();
	isolated(&mut profiled, &root, &home);
	let profiled = profiled
		.env("PI_CODING_AGENT_DIR", &custom)
		.args(["--profile", "work", "--print"])
		.current_dir(&work)
		.output()
		.expect("run");
	assert_ne!(profiled.status.code(), Some(0));
	assert!(stderr(&profiled).contains("no models"), "stderr: {}", stderr(&profiled));
	fs::remove_dir_all(&root).ok();
}

#[test]
fn alias_installs_a_managed_shell_function_and_requires_a_profile() {
	let root = temp_dir("alias");
	let home = root.join("home");
	fs::create_dir_all(&home).expect("home");

	let mut cmd = omp();
	isolated(&mut cmd, &root, &home);
	let first = cmd
		.env("SHELL", "/bin/bash")
		.args(["--alias", "wk", "--profile", "work"])
		.output()
		.expect("run");
	assert_eq!(first.status.code(), Some(0), "stderr: {}", stderr(&first));
	let output = stdout(&first);
	assert!(output.contains("Created wk for profile work in"), "stdout: {output}");
	assert!(output.contains("Then use: wk update, wk --version, or wk"), "stdout: {output}");
	let bashrc = fs::read_to_string(home.join(".bashrc")).expect("bashrc");
	assert!(bashrc.contains("# >>> omp profile alias: wk >>>"), "{bashrc}");
	assert!(bashrc.contains("command omp --profile=work \"$@\""), "{bashrc}");

	// Re-running replaces the managed block instead of appending a second one.
	let mut again = omp();
	isolated(&mut again, &root, &home);
	let again = again
		.env("SHELL", "/bin/bash")
		.args(["--alias", "wk", "--profile", "work"])
		.output()
		.expect("run");
	assert_eq!(again.status.code(), Some(0));
	let bashrc = fs::read_to_string(home.join(".bashrc")).expect("bashrc");
	assert_eq!(bashrc.matches("# >>> omp profile alias: wk >>>").count(), 1);

	// The profile may come from the environment.
	let mut env_profile = omp();
	isolated(&mut env_profile, &root, &home);
	let env_profile = env_profile
		.env("SHELL", "/bin/bash")
		.env("OMP_PROFILE", "work")
		.args(["--alias", "w2"])
		.output()
		.expect("run");
	assert_eq!(env_profile.status.code(), Some(0), "stderr: {}", stderr(&env_profile));
	assert!(fs::read_to_string(home.join(".bashrc")).expect("bashrc").contains("alias: w2"));

	let missing = omp()
		.env("PI_CONFIG_DIR", &root)
		.env("HOME", &home)
		.env("SHELL", "/bin/bash")
		.env_remove("OMP_PROFILE")
		.env_remove("PI_PROFILE")
		.args(["--alias", "wk"])
		.output()
		.expect("run");
	assert_eq!(missing.status.code(), Some(1));
	assert!(stderr(&missing).contains("--alias requires --profile"), "stderr: {}", stderr(&missing));

	for (alias, expected) in [("omp", "Refusing to shadow"), ("if", "reserved word")] {
		let mut cmd = omp();
		isolated(&mut cmd, &root, &home);
		let rejected = cmd
			.env("SHELL", "/bin/bash")
			.args(["--alias", alias, "--profile", "work"])
			.output()
			.expect("run");
		assert_eq!(rejected.status.code(), Some(1), "{alias}");
		assert!(stderr(&rejected).contains(expected), "{alias} stderr: {}", stderr(&rejected));
	}
	fs::remove_dir_all(&root).ok();
}

#[test]
fn alias_treats_empty_shell_path_env_vars_as_unset() {
	let root = temp_dir("alias-empty");
	let home = root.join("home");
	let work = root.join("work");
	fs::create_dir_all(&home).expect("home");
	fs::create_dir_all(&work).expect("work");

	let mut zsh = omp();
	isolated(&mut zsh, &root, &home);
	let zsh = zsh
		.env("SHELL", "/usr/bin/zsh")
		.env("ZDOTDIR", "")
		.args(["--alias", "z", "--profile", "work"])
		.current_dir(&work)
		.output()
		.expect("run");
	assert_eq!(zsh.status.code(), Some(0), "stderr: {}", stderr(&zsh));
	assert!(home.join(".zshrc").is_file());
	assert!(!work.join(".zshrc").exists(), "must not write into cwd");

	let mut fish = omp();
	isolated(&mut fish, &root, &home);
	let fish = fish
		.env("SHELL", "/usr/bin/fish")
		.env("XDG_CONFIG_HOME", "")
		.args(["--alias", "f", "--profile", "work"])
		.current_dir(&work)
		.output()
		.expect("run");
	assert_eq!(fish.status.code(), Some(0), "stderr: {}", stderr(&fish));
	assert!(home.join(".config/fish/conf.d/omp-profiles.fish").is_file());
	assert!(!work.join("fish").exists(), "must not write into cwd");
	fs::remove_dir_all(&root).ok();
}

#[test]
fn cwd_drives_project_discovery() {
	let root = temp_dir("cwd");
	let home = root.join("home");
	let project = root.join("project");
	let elsewhere = root.join("elsewhere");
	fs::create_dir_all(&home).expect("home");
	fs::create_dir_all(&elsewhere).expect("elsewhere");
	// Malformed project settings are reported with their path, which proves
	// discovery ran against `--cwd` before the catalog/runtime started.
	write(&project.join(".omp/settings.json"), r#"{"modelRoles":"#);

	let mut profiled = omp();
	isolated(&mut profiled, &root, &home);
	let profiled = profiled
		.args(["--cwd"])
		.arg(&project)
		.arg("--print")
		.current_dir(&elsewhere)
		.output()
		.expect("run");
	assert_ne!(profiled.status.code(), Some(0));
	let message = stderr(&profiled);
	assert!(message.contains("skipping malformed project settings"), "stderr: {message}");
	assert!(message.contains("project/.omp/settings.json"), "stderr: {message}");

	let mut plain = omp();
	isolated(&mut plain, &root, &home);
	let plain = plain.arg("--print").current_dir(&elsewhere).output().expect("run");
	assert!(!stderr(&plain).contains("skipping malformed project settings"), "stderr: {}", stderr(&plain));
	fs::remove_dir_all(&root).ok();
}

#[test]
fn malformed_project_and_cli_config_fail_without_touching_inputs() {
	let root = temp_dir("invalid-config");
	let home = root.join("home");
	let project = root.join("project");
	let work = root.join("work");
	fs::create_dir_all(&home).expect("home");
	fs::create_dir_all(&work).expect("work");
	let secret = "sk-project-secret";
	let malformed = format!("providers:\n  p:\n    apiKey: \"{secret}\n");
	let project_config = project.join(".omp/config.yml");
	write(&project_config, &malformed);

	let mut cmd = omp();
	isolated(&mut cmd, &root, &home);
	let output = cmd
		.args(["--cwd"])
		.arg(&project)
		.arg("--print")
		.current_dir(&work)
		.output()
		.expect("run");
	assert_eq!(output.status.code(), Some(1));
	let message = stderr(&output);
	assert!(message.contains("project/.omp/config.yml"), "stderr: {message}");
	assert!(!message.contains(secret), "stderr: {message}");
	// No quarantine: the invalid file keeps its bytes and location.
	assert_eq!(fs::read_to_string(&project_config).expect("unchanged"), malformed);

	// Explicit `--config` is fatal as well and is never moved aside.
	let cli_config = root.join("cli-bad.yml");
	write(&cli_config, &malformed);
	let mut cmd = omp();
	isolated(&mut cmd, &root, &home);
	let output = cmd
		.arg("--config")
		.arg(&cli_config)
		.arg("--print")
		.current_dir(&work)
		.output()
		.expect("run");
	assert_eq!(output.status.code(), Some(1));
	let message = stderr(&output);
	assert!(message.contains("cli-bad.yml"), "stderr: {message}");
	assert!(!message.contains(secret), "stderr: {message}");
	assert_eq!(fs::read_to_string(&cli_config).expect("unchanged"), malformed);

	// Global main YAML is fatal in place too (no quarantine).
	let global_config = root.join("agent/config.yml");
	write(&global_config, &malformed);
	let mut cmd = omp();
	isolated(&mut cmd, &root, &home);
	let output = cmd.arg("--print").current_dir(&work).output().expect("run");
	assert_eq!(output.status.code(), Some(1));
	let message = stderr(&output);
	assert!(message.contains("agent/config.yml"), "stderr: {message}");
	assert!(!message.contains(secret), "stderr: {message}");
	assert_eq!(fs::read_to_string(&global_config).expect("unchanged"), malformed);
	fs::remove_dir_all(&root).ok();
}

#[test]
fn overlays_fail_honestly_without_leaking_secrets() {
	let root = temp_dir("overlays");
	let home = root.join("home");
	let work = root.join("work");
	fs::create_dir_all(&home).expect("home");
	fs::create_dir_all(&work).expect("work");
	write(
		&root.join("secret.yml"),
		"providers:\n  p:\n    apiKey: \"sk-do-not-print\n",
	);

	let mut env_overlay = omp();
	isolated(&mut env_overlay, &root, &home);
	let env_overlay = env_overlay
		.env("PI_CONFIG_FILES", root.join("secret.yml"))
		.arg("--print")
		.current_dir(&work)
		.output()
		.expect("run");
	assert_eq!(env_overlay.status.code(), Some(1));
	let message = stderr(&env_overlay);
	assert!(message.contains("secret.yml"), "stderr: {message}");
	assert!(!message.contains("sk-do-not-print"), "secret leaked: {message}");

	// Environment overlays load before CLI overlays, so the environment file's
	// error is the one reported when both are missing.
	let mut ordering = omp();
	isolated(&mut ordering, &root, &home);
	let ordering = ordering
		.env("PI_CONFIG_FILES", root.join("env-missing.yml"))
		.args(["--config"])
		.arg(root.join("cli-missing.yml"))
		.arg("--print")
		.current_dir(&work)
		.output()
		.expect("run");
	assert_eq!(ordering.status.code(), Some(1));
	let message = stderr(&ordering);
	assert!(message.contains("env-missing.yml"), "stderr: {message}");
	assert!(!message.contains("cli-missing.yml"), "stderr: {message}");

	// Relative PI_CONFIG_FILES entries resolve against the effective --cwd.
	write(&root.join("project/overrides.yml"), "defaultModel: [not-a-scalar]\n");
	let mut relative = omp();
	isolated(&mut relative, &root, &home);
	let relative = relative
		.env("PI_CONFIG_FILES", "overrides.yml")
		.args(["--cwd"])
		.arg(root.join("project"))
		.arg("--print")
		.current_dir(&work)
		.output()
		.expect("run");
	assert_eq!(relative.status.code(), Some(1));
	assert!(stderr(&relative).contains("project/overrides.yml"), "stderr: {}", stderr(&relative));
	fs::remove_dir_all(&root).ok();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn xdg_pins_sessions_for_default_and_named_profiles() {
	let root = temp_dir("xdg");
	let home = root.join("home");
	let work = root.join("work");
	let xdg = root.join("xdg-data");
	fs::create_dir_all(&home).expect("home");
	fs::create_dir_all(&work).expect("work");
	let xdg_app = xdg.join("omp");
	fs::create_dir_all(&xdg_app).expect("xdg app root");

	let add_session = |sessions_root: &Path, id: &str| {
		let dirs = omp_cli::session::session_completion_dirs(&work, Some(sessions_root));
		write(
			&dirs[0].join(format!("{id}.jsonl")),
			"{\"type\":\"title\",\"title\":\"Fixture title\"}\n",
		);
	};
	add_session(&xdg_app.join("sessions"), "xdg-default");
	add_session(&root.join("profiles/work/agent/sessions"), "legacy-work");

	let run = |args: &[&str], env: &[(&str, &Path)]| {
		let mut cmd = omp();
		isolated(&mut cmd, &root, &home);
		cmd.current_dir(&work).env("XDG_DATA_HOME", &xdg);
		for (key, value) in env {
			cmd.env(key, value);
		}
		cmd.args(args).output().expect("run")
	};
	let complete = |args: &[&str], env: &[(&str, &Path)]| {
		let output = run(args, env);
		assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
		stdout(&output)
	};

	// Default profile: an existing $XDG_DATA_HOME/omp redirects sessions.
	let default = complete(&["__complete", "sessions"], &[]);
	assert!(default.contains("xdg-default"), "stdout: {default}");

	// Named profile pin: the base app root is ignored while the profile-specific
	// XDG path is absent, so the legacy profile sessions stay authoritative.
	let pinned_out = complete(&["--profile", "work", "__complete", "sessions"], &[]);
	assert!(pinned_out.contains("legacy-work"), "stdout: {pinned_out}");
	assert!(!pinned_out.contains("xdg-default"), "stdout: {pinned_out}");

	// Once the profile-specific XDG category exists, that path is pinned and the
	// legacy profile sessions are no longer consulted.
	add_session(&xdg_app.join("profiles/work/sessions"), "xdg-work");
	let pinned_in = complete(&["--profile", "work", "__complete", "sessions"], &[]);
	assert!(pinned_in.contains("xdg-work"), "stdout: {pinned_in}");
	assert!(!pinned_in.contains("legacy-work"), "stdout: {pinned_in}");

	// An explicit agent-dir override is not the default agent dir, so XDG is off.
	let override_agent = root.join("override-agent");
	add_session(&override_agent.join("sessions"), "override");
	let overridden = complete(
		&["__complete", "sessions"],
		&[("PI_CODING_AGENT_DIR", override_agent.as_path())],
	);
	assert!(overridden.contains("override"), "stdout: {overridden}");
	assert!(!overridden.contains("xdg-default"), "stdout: {overridden}");
	fs::remove_dir_all(&root).ok();
}

#[test]
fn help_and_completions_are_read_only_and_profile_aware() {
	let root = temp_dir("complete");
	let home = root.join("home");
	let work = root.join("work");
	fs::create_dir_all(&home).expect("home");
	fs::create_dir_all(&work).expect("work");
	let agent = root.join("profiles/work/agent");
	let sessions_root = agent.join("sessions");
	let dirs = omp_cli::session::session_completion_dirs(&work, Some(&sessions_root));
	write(
		&dirs[0].join("sess-1.jsonl"),
		"{\"type\":\"title\",\"title\":\"Fixture title\"}\n",
	);
	write(&agent.join("models.json"), models_json());

	let before = {
		let mut all = snapshot(&root);
		all.extend(snapshot(&home));
		all
	};

	let mut help = omp();
	isolated(&mut help, &root, &home);
	let help = help.arg("--help").current_dir(&work).output().expect("run");
	assert_eq!(help.status.code(), Some(0));
	assert!(stdout(&help).contains("--profile"));
	assert!(stdout(&help).contains("--alias"));

	let mut script = omp();
	isolated(&mut script, &root, &home);
	let script = script
		.args(["completions", "bash"])
		.current_dir(&work)
		.output()
		.expect("run");
	assert_eq!(script.status.code(), Some(0));
	assert!(stdout(&script).contains("__complete sessions"));

	let mut profiled = omp();
	isolated(&mut profiled, &root, &home);
	let profiled = profiled
		.args(["--profile", "work", "__complete", "sessions"])
		.current_dir(&work)
		.output()
		.expect("run");
	assert_eq!(profiled.status.code(), Some(0), "stderr: {}", stderr(&profiled));
	assert!(stdout(&profiled).contains("sess-1"), "stdout: {}", stdout(&profiled));

	let mut plain = omp();
	isolated(&mut plain, &root, &home);
	let plain = plain
		.args(["__complete", "sessions"])
		.current_dir(&work)
		.output()
		.expect("run");
	assert_eq!(plain.status.code(), Some(0));
	assert!(stdout(&plain).is_empty(), "stdout: {}", stdout(&plain));

	let after = {
		let mut all = snapshot(&root);
		all.extend(snapshot(&home));
		all
	};
	assert_eq!(before, after, "help/completions must not create files");
	fs::remove_dir_all(&root).ok();
}
