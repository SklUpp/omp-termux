//! Behavioral contract for `omp config`: the real binary, the real schema
//! snapshot, an isolated agent directory per test.
//!
//! These drive the command end to end on purpose — the schema, the layer
//! reader, the writer and the redaction rules are only correct together.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};


struct Sandbox {
	root: PathBuf,
}

impl Sandbox {
	fn new(prefix: &str) -> Self {
		let root = std::env::temp_dir().join(format!("omp-config-{prefix}-{}", std::process::id()));
		fs::create_dir_all(root.join("config/agent")).expect("agent dir");
		fs::create_dir_all(root.join("project")).expect("project dir");
		Self { root }
	}

	fn agent_config(&self) -> PathBuf {
		self.root.join("config/agent/config.yml")
	}

	fn project(&self) -> PathBuf {
		self.root.join("project")
	}

	fn command(&self) -> Command {
		let mut command = Command::new(env!("CARGO_BIN_EXE_omp"));
		command
			.env("PI_CONFIG_DIR", self.root.join("config"))
			.env("PI_CODING_AGENT_DIR", self.root.join("config/agent"))
			.env("HOME", self.root.join("home"))
			.env("XDG_DATA_HOME", self.root.join("xdg/data"))
			.env("XDG_STATE_HOME", self.root.join("xdg/state"))
			.env("XDG_CACHE_HOME", self.root.join("xdg/cache"))
			.env("OMP_SHARE_DIR", self.root.join("share"))
			.env_remove("PI_CONFIG_FILES")
			.env_remove("PI_PROFILE")
			.env_remove("OMP_PROFILE")
			.current_dir(self.project())
			.stdin(Stdio::null());
		command
	}

	fn run(&self, args: &[&str]) -> Output {
		self.command().args(args).output().expect("run omp")
	}

	fn stdout(&self, args: &[&str]) -> String {
		let output = self.run(args);
		assert!(
			output.status.success(),
			"{args:?} failed: {}",
			String::from_utf8_lossy(&output.stderr)
		);
		String::from_utf8(output.stdout).expect("utf8 stdout")
	}

	fn stdout_json(&self, args: &[&str]) -> serde_json::Value {
		serde_json::from_str(self.stdout(args).trim()).expect("json stdout")
	}

	fn write(&self, path: &Path, text: &str) {
		if let Some(parent) = path.parent() {
			fs::create_dir_all(parent).expect("parent dir");
		}
		fs::write(path, text).expect("write fixture");
	}
}

impl Drop for Sandbox {
	fn drop(&mut self) {
		fs::remove_dir_all(&self.root).ok();
	}
}

const SECRET: &str = "credential-value-not-for-output";

#[test]
fn list_reports_the_full_schema_and_omits_values_for_unset_keys() {
	let sandbox = Sandbox::new("list");
	let listing = sandbox.stdout_json(&["config", "list", "--json"]);
	let entries = listing.as_object().expect("object");

	// Keys this binary never consumes are still part of the surface.
	for path in ["defaultThinkingLevel", "compaction.idleTimeoutSeconds", "modelRoles"] {
		let entry = entries.get(path).unwrap_or_else(|| panic!("missing {path}"));
		assert!(entry.get("type").is_some(), "{path} has no type");
		assert!(entry.get("description").is_some(), "{path} has no description");
	}

	// An unset key reports its default; a key with no default reports neither a
	// value nor a redaction marker.
	assert_eq!(entries["autoResume"]["value"], serde_json::json!(false));
	assert!(entries["searxng.token"].get("value").is_none());
	assert!(entries["searxng.token"].get("redacted").is_none());

	// `omp config` without an action lists.
	assert!(sandbox.stdout(&["config"]).starts_with("Settings:"));
}

#[test]
fn set_persists_every_value_shape_and_get_reads_it_back() {
	let sandbox = Sandbox::new("typed");

	sandbox.stdout(&["config", "set", "autoResume", "true", "--json"]);
	sandbox.stdout(&["config", "set", "compaction.idleTimeoutSeconds", "600", "--json"]);
	sandbox.stdout(&["config", "set", "defaultThinkingLevel", "max", "--json"]);
	sandbox.stdout(&["config", "set", "enabledModels", r#"["a","b"]"#, "--json"]);
	sandbox.stdout(&["config", "set", "modelRoles", r#"{"default":"x"}"#, "--json"]);
	sandbox.stdout(&["config", "set", "providers.maxInFlightRequests", r#"{"openai":2.8,"anthropic":1}"#, "--json"]);

	let read = |key: &str| sandbox.stdout_json(&["config", "get", key, "--json"]);
	assert_eq!(read("autoResume")["value"], serde_json::json!(true));
	assert_eq!(read("compaction.idleTimeoutSeconds")["value"], serde_json::json!(600));
	assert_eq!(read("defaultThinkingLevel")["value"], serde_json::json!("max"));
	assert_eq!(read("enabledModels")["value"], serde_json::json!(["a", "b"]));
	assert_eq!(read("modelRoles")["value"], serde_json::json!({"default": "x"}));
	// Floored limits are what lands on disk, not just what prints.
	assert_eq!(
		read("providers.maxInFlightRequests")["value"],
		serde_json::json!({"openai": 2, "anthropic": 1})
	);

	let written = fs::read_to_string(sandbox.agent_config()).expect("config.yml");
	for expected in [
		"autoResume: true",
		"idleTimeoutSeconds: 600",
		"defaultThinkingLevel: max",
		"enabledModels:",
		"modelRoles:",
	] {
		assert!(written.contains(expected), "missing {expected:?} in:\n{written}");
	}
}

#[test]
fn invalid_values_fail_without_writing_the_file() {
	let sandbox = Sandbox::new("invalid");
	let before = sandbox.run(&["config", "set", "autoResume", "maybe"]);
	assert_eq!(before.status.code(), Some(1));
	assert!(
		String::from_utf8_lossy(&before.stderr).contains("Invalid boolean value"),
		"{}",
		String::from_utf8_lossy(&before.stderr)
	);
	assert!(!sandbox.agent_config().exists(), "a rejected value must not create the file");

	let limits = sandbox.run(&[
		"config",
		"set",
		"providers.maxInFlightRequests",
		r#"{"openai":"2","anthropic":0}"#,
	]);
	assert_eq!(limits.status.code(), Some(1));
	assert!(
		String::from_utf8_lossy(&limits.stderr)
			.contains("Provider request limits must be positive numbers: openai, anthropic"),
		"{}",
		String::from_utf8_lossy(&limits.stderr)
	);

	let unknown = sandbox.run(&["config", "get", "not.a.setting"]);
	assert_eq!(unknown.status.code(), Some(1));
	assert!(String::from_utf8_lossy(&unknown.stderr).contains("Unknown setting: not.a.setting"));

	let action = sandbox.run(&["config", "frobnicate"]);
	assert_eq!(action.status.code(), Some(1));
	assert!(String::from_utf8_lossy(&action.stderr).contains("Unknown config command: frobnicate"));
}

#[test]
fn credentials_are_redacted_in_list_but_plain_in_get() {
	let sandbox = Sandbox::new("credentials");
	sandbox.stdout(&["config", "set", "searxng.token", SECRET, "--json"]);

	let human = sandbox.stdout(&["config", "list"]);
	assert!(human.contains("searxng.token = ********"), "{human}");
	assert!(!human.contains(SECRET), "human list leaked the credential");

	let raw = sandbox.stdout(&["config", "list", "--json"]);
	assert!(!raw.contains(SECRET), "json list leaked the credential");
	assert!(!raw.contains("********"), "json list emitted a placeholder value");
	let listing = serde_json::from_str::<serde_json::Value>(&raw).expect("json");
	assert_eq!(listing["searxng.token"]["redacted"], serde_json::json!(true));
	assert!(listing["searxng.token"].get("value").is_none());

	// `get` is an explicit single-value request, so it stays plain.
	assert_eq!(sandbox.stdout(&["config", "get", "searxng.token"]).trim(), SECRET);

	// A cleared credential is not "configured": masking it would claim otherwise.
	sandbox.stdout(&["config", "set", "searxng.token", "", "--json"]);
	let cleared = sandbox.stdout_json(&["config", "list", "--json"]);
	assert!(cleared["searxng.token"].get("redacted").is_none());
	assert!(!sandbox.stdout(&["config", "list"]).contains("searxng.token = ********"));
}

#[test]
fn unknown_keys_and_their_order_survive_a_write() {
	let sandbox = Sandbox::new("preserve");
	sandbox.write(
		&sandbox.agent_config(),
		"customKey: keep\nzeta: 1\nnested:\n  inner: value\nautoResume: false\n",
	);
	sandbox.stdout(&["config", "set", "autoResume", "true", "--json"]);

	let written = fs::read_to_string(sandbox.agent_config()).expect("config.yml");
	let position = |needle: &str| written.find(needle).unwrap_or_else(|| panic!("missing {needle}"));
	assert!(position("customKey: keep") < position("zeta: 1"));
	assert!(position("zeta: 1") < position("nested:"));
	assert!(position("nested:") < position("autoResume: true"));
	assert!(written.contains("inner: value"));

	// Unknown keys stay inert for the reader too.
	assert_eq!(sandbox.stdout(&["config", "get", "autoResume"]).trim(), "true");
}

#[test]
fn malformed_config_is_fatal_and_left_untouched() {
	let sandbox = Sandbox::new("malformed");
	let bytes = "broken: [unterminated\n";
	sandbox.write(&sandbox.agent_config(), bytes);

	let output = sandbox.run(&["config", "set", "autoResume", "true"]);
	assert_eq!(output.status.code(), Some(1));
	let stderr = String::from_utf8_lossy(&output.stderr);
	assert!(stderr.contains("Settings config is invalid"), "{stderr}");
	assert!(!stderr.contains("broken"), "payload echoed back: {stderr}");
	assert_eq!(
		fs::read_to_string(sandbox.agent_config()).expect("config.yml"),
		bytes,
		"a malformed file must not be rewritten or moved"
	);
}

#[test]
fn concurrent_disjoint_writes_both_survive() {
	let sandbox = Sandbox::new("concurrent");
	let mut first = sandbox.command();
	first.args(["config", "set", "autoResume", "true", "--json"]);
	let mut second = sandbox.command();
	second.args(["config", "set", "compaction.enabled", "false", "--json"]);
	let first = first.spawn().expect("spawn first");
	let second = second.spawn().expect("spawn second");
	for mut child in [first, second] {
		let status = child.wait().expect("wait");
		assert!(status.success(), "concurrent write failed: {status}");
	}

	let written = fs::read_to_string(sandbox.agent_config()).expect("config.yml");
	assert!(written.contains("autoResume: true"), "{written}");
	let compaction = written
		.split("compaction:")
		.nth(1)
		.unwrap_or_else(|| panic!("missing compaction group in:\n{written}"));
	assert!(
		compaction.lines().any(|line| line.trim() == "enabled: false"),
		"second write was lost:\n{written}"
	);
}

/// Layer precedence: global < project config.yml < PI_CONFIG_FILES in path order.
#[test]
fn later_layers_win_and_overlays_apply_in_path_order() {
	let sandbox = Sandbox::new("layers");
	sandbox.write(&sandbox.agent_config(), "defaultThinkingLevel: low\n");
	sandbox.write(
		&sandbox.project().join(".omp/config.yml"),
		"defaultThinkingLevel: medium\n",
	);
	assert_eq!(
		sandbox.stdout(&["config", "get", "defaultThinkingLevel"]).trim(),
		"medium"
	);

	let base = sandbox.root.join("base-overlay.yml");
	let final_overlay = sandbox.root.join("final-overlay.yml");
	sandbox.write(&base, "defaultThinkingLevel: high\n");
	sandbox.write(&final_overlay, "defaultThinkingLevel: max\n");
	let files = format!("{}{}{}", base.display(), if cfg!(windows) { ";" } else { ":" }, final_overlay.display());
	let output = sandbox
		.command()
		.args(["config", "get", "defaultThinkingLevel", "--json"])
		.env("PI_CONFIG_FILES", files)
		.output()
		.expect("run omp");
	assert!(output.status.success());
	let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
	assert_eq!(value["value"], serde_json::json!("max"));

	// `--config` overlays win over everything before them.
	let override_file = sandbox.root.join("override.yml");
	sandbox.write(&override_file, "defaultThinkingLevel: minimal\n");
	let output = sandbox
		.command()
		.arg("--config")
		.arg(&override_file)
		.args(["config", "get", "defaultThinkingLevel"])
		.output()
		.expect("run omp");
	assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
	assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "minimal");

	// A shared project settings.json is read too, and a foreign scalar that
	// would shadow a settings group cannot hide the real settings.
	sandbox.write(
		&sandbox.project().join(".omp/settings.json"),
		r#"{"model": "opus", "autoResume": true}"#,
	);
	assert_eq!(sandbox.stdout(&["config", "get", "autoResume"]).trim(), "true");
	assert!(sandbox.stdout(&["config", "list"]).starts_with("Settings:"));
}

#[test]
fn reset_returns_to_the_schema_default() {
	let sandbox = Sandbox::new("reset");
	sandbox.stdout(&["config", "set", "autoResume", "true", "--json"]);
	sandbox.stdout(&["config", "reset", "autoResume", "--json"]);
	assert_eq!(
		sandbox.stdout_json(&["config", "get", "autoResume", "--json"])["value"],
		serde_json::json!(false)
	);
}

#[test]
fn path_prints_the_agent_directory_and_init_xdg_creates_it() {
	let sandbox = Sandbox::new("paths");
	let printed = sandbox.stdout(&["config", "path"]);
	assert_eq!(
		PathBuf::from(printed.trim()),
		sandbox.root.join("config/agent"),
		"path must print the active agent directory"
	);

	sandbox.stdout(&["config", "init-xdg"]);
	for dir in ["xdg/data/omp", "xdg/state/omp", "xdg/cache/omp"] {
		assert!(sandbox.root.join(dir).is_dir(), "missing {dir}");
	}
}

#[test]
fn path_scoped_arrays_flatten_for_the_current_directory_only() {
	let sandbox = Sandbox::new("scoped");
	sandbox.write(
		&sandbox.agent_config(),
		"enabledModels:\n  - always\n  - path: /elsewhere\n    values: [lost]\n  - pathPrefixes: [\"~\", /]\n    models: [matched]\n",
	);
	// `~` is the sandbox HOME, which does not contain the project; `/` does.
	let value = sandbox.stdout_json(&["config", "get", "enabledModels", "--json"]);
	assert_eq!(value["value"], serde_json::json!(["always", "matched"]));

	// A stored plain string array keeps passing through unchanged.
	sandbox.stdout(&["config", "set", "enabledModels", r#"["a","b"]"#, "--json"]);
	let value = sandbox.stdout_json(&["config", "get", "enabledModels", "--json"]);
	assert_eq!(value["value"], serde_json::json!(["a", "b"]));
}

#[test]
fn writes_follow_a_symlinked_config_file() {
	let sandbox = Sandbox::new("symlink");
	let target = sandbox.root.join("dotfiles/config.yml");
	sandbox.write(&target, "customKey: kept\n");
	std::os::unix::fs::symlink(&target, sandbox.agent_config()).expect("symlink");

	sandbox.stdout(&["config", "set", "autoResume", "true", "--json"]);

	let link = fs::symlink_metadata(sandbox.agent_config()).expect("link metadata");
	assert!(link.file_type().is_symlink(), "config symlink must survive a write");
	let written = fs::read_to_string(&target).expect("target");
	assert!(written.contains("autoResume: true"), "{written}");
	assert!(written.contains("customKey: kept"), "{written}");
}

#[test]
fn writes_recreate_a_dangling_symlink_target() {
	let sandbox = Sandbox::new("dangling");
	let target = sandbox.root.join("config/dotfiles/new.yml");
	std::os::unix::fs::symlink(Path::new("../dotfiles/new.yml"), sandbox.agent_config()).expect("symlink");

	sandbox.stdout(&["config", "set", "autoResume", "true", "--json"]);

	let link = fs::symlink_metadata(sandbox.agent_config()).expect("link metadata");
	assert!(link.file_type().is_symlink());
	assert!(target.is_file(), "dangling target must be created");
	assert!(fs::read_to_string(&target).expect("target").contains("autoResume: true"));
}

#[test]
fn symlink_cycles_fail_without_writing() {
	let sandbox = Sandbox::new("cycle");
	let first = sandbox.root.join("a.yml");
	let second = sandbox.root.join("b.yml");
	std::os::unix::fs::symlink(&second, &first).expect("a");
	std::os::unix::fs::symlink(&first, &second).expect("b");
	std::os::unix::fs::symlink(&first, sandbox.agent_config()).expect("agent link");

	let output = sandbox.run(&["config", "set", "autoResume", "true"]);
	assert_eq!(output.status.code(), Some(1));
	assert!(
		String::from_utf8_lossy(&output.stderr).contains("too many symbolic links"),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	assert!(!first.exists() && !second.exists(), "a cycle must not create files");
}

#[test]
fn invalid_layers_abort_before_any_write() {
	let sandbox = Sandbox::new("prevalidate");
	sandbox.write(&sandbox.agent_config(), "autoResume: true\n");
	sandbox.write(&sandbox.project().join(".omp/config.yml"), "broken: [unterminated\n");

	let cases: [&[&str]; 2] = [
		&["config", "set", "compaction.enabled", "false"],
		&["config", "reset", "autoResume"],
	];
	for args in cases {
		let output = sandbox.run(args);
		assert_eq!(output.status.code(), Some(1), "{args:?} must fail");
		assert_eq!(
			fs::read_to_string(sandbox.agent_config()).expect("global"),
			"autoResume: true\n",
			"{args:?} must not touch the global file"
		);
	}
}

#[test]
fn project_config_group_shadows_are_dropped() {
	let sandbox = Sandbox::new("shadow");
	sandbox.write(
		&sandbox.agent_config(),
		"compaction:\n  enabled: false\n  idleTimeoutSeconds: 600\n",
	);
	sandbox.write(&sandbox.project().join(".omp/config.yml"), "compaction: disabled\n");

	let enabled = sandbox.stdout_json(&["config", "get", "compaction.enabled", "--json"]);
	assert_eq!(enabled["value"], serde_json::json!(false));
	let idle = sandbox.stdout_json(&["config", "get", "compaction.idleTimeoutSeconds", "--json"]);
	assert_eq!(idle["value"], serde_json::json!(600));
}

#[test]
fn empty_xdg_variables_fall_back_to_home() {
	let sandbox = Sandbox::new("xdg-empty");
	let output = sandbox
		.command()
		.env("XDG_DATA_HOME", "")
		.env("XDG_STATE_HOME", "")
		.env("XDG_CACHE_HOME", "")
		.args(["config", "init-xdg"])
		.output()
		.expect("run omp");
	assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
	assert!(sandbox.root.join("home/.local/share/omp").is_dir());
	assert!(!sandbox.project().join("omp").exists(), "an empty XDG var must not create ./omp");
}

fn get_json(sandbox: &Sandbox, key: &str) -> serde_json::Value {
	sandbox.stdout_json(&["config", "get", key, "--json"])
}

#[test]
fn migrates_legacy_settings_the_way_upstream_does() {
	let sandbox = Sandbox::new("migrate-global");
	fs::write(
		sandbox.agent_config(),
		r#"codexResets:
  autoRedeem: false
memories:
  enabled: false
serviceTier: priority
power:
  preventIdleSleep: true
search:
  enabled: true
  contextBefore: 4
providers:
  webSearch: gemini
exa:
  enabled: true
  enableSearch: false
dev:
  autoqa:
    consent: granted
computer:
  backend: legacy
advisor:
  subagents: true
"#,
	)
	.expect("write config");

	assert_eq!(get_json(&sandbox, "codexResets.autoRedeem")["value"], serde_json::json!("no"));
	assert_eq!(get_json(&sandbox, "memory.backend")["value"], serde_json::json!("off"));
	assert_eq!(get_json(&sandbox, "tier.openai")["value"], serde_json::json!("priority"));
	assert_eq!(get_json(&sandbox, "tier.anthropic")["value"], serde_json::json!("priority"));
	assert_eq!(get_json(&sandbox, "tier.google")["value"], serde_json::json!("priority"));
	assert_eq!(get_json(&sandbox, "power.sleepPrevention")["value"], serde_json::json!("idle"));
	assert_eq!(get_json(&sandbox, "grep.enabled")["value"], serde_json::json!(true));
	assert_eq!(get_json(&sandbox, "grep.contextBefore")["value"], serde_json::json!(4));
	let order = get_json(&sandbox, "providers.webSearchOrder");
	assert_eq!(order["value"][0], serde_json::json!("gemini"));
	assert_eq!(order["value"].as_array().map(Vec::len), Some(23));
	assert_eq!(get_json(&sandbox, "exa.enabled")["value"], serde_json::json!(false));
	assert_eq!(get_json(&sandbox, "dev.autoqaConsent")["value"], serde_json::json!("granted"));
	let advisor = get_json(&sandbox, "task.agentAdvisor");
	assert_eq!(advisor["value"]["task"], serde_json::json!("on"));
}

#[test]
fn migration_runs_per_layer_so_project_still_overrides_global() {
	let sandbox = Sandbox::new("migrate-layers");
	fs::write(
		sandbox.agent_config(),
		"advisor:\n  subagents: true\nmemories:\n  enabled: true\n",
	)
	.expect("write global");
	let project_config = sandbox.project().join(".omp/config.yml");
	fs::create_dir_all(project_config.parent().expect("dir")).expect("mkdir");
	fs::write(&project_config, "advisor:\n  subagents: false\nmemories:\n  enabled: false\n")
		.expect("write project");

	let advisor = get_json(&sandbox, "task.agentAdvisor");
	assert_eq!(advisor["value"]["task"], serde_json::json!("off"));
	assert_eq!(get_json(&sandbox, "memory.backend")["value"], serde_json::json!("off"));
}

#[test]
fn migration_is_idempotent_and_a_write_does_not_disturb_it() {
	let sandbox = Sandbox::new("migrate-idempotent");
	fs::write(
		sandbox.agent_config(),
		"serviceTier: claude-only\npower:\n  preventDisplaySleep: true\n",
	)
	.expect("write config");

	let first = get_json(&sandbox, "tier.anthropic");
	let second = get_json(&sandbox, "tier.anthropic");
	assert_eq!(first, second);
	assert_eq!(first["value"], serde_json::json!("priority"));
	assert_eq!(get_json(&sandbox, "power.sleepPrevention")["value"], serde_json::json!("display"));

	let output = sandbox.run(&["config", "set", "autoResume", "true"]);
	assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
	let again = get_json(&sandbox, "tier.anthropic");
	assert_eq!(again, first);
	assert_eq!(get_json(&sandbox, "power.sleepPrevention")["value"], serde_json::json!("display"));
}

#[test]
fn symlinked_legacy_settings_archives_the_link_not_the_target() {
	let sandbox = Sandbox::new("symlink-legacy");
	let target = sandbox.root.join("dotfiles-settings.json");
	let contents = "{\n  \"theme\": { \"dark\": \"linked-dark\" },\n}\n";
	sandbox.write(&target, contents);
	let settings = sandbox.root.join("config/agent/settings.json");
	std::os::unix::fs::symlink(&target, &settings).expect("symlink settings.json");

	assert_eq!(get_json(&sandbox, "theme.dark")["value"], serde_json::json!("linked-dark"));
	assert_eq!(fs::read_to_string(&target).expect("target read"), contents, "target untouched");
	assert!(!settings.exists(), "the link itself is archived, not the target");
	assert_eq!(
		fs::read_link(sandbox.root.join("config/agent/settings.json.bak")).expect("backup link"),
		target
	);
	assert_eq!(
		fs::read_to_string(sandbox.agent_config()).expect("config.yml"),
		"theme:\n  dark: linked-dark\n"
	);
}

#[test]
fn merges_opencode_project_jsonc_with_comments_and_trailing_commas() {
	let sandbox = Sandbox::new("opencode-jsonc");
	let root = sandbox.project();
	fs::write(
		root.join("opencode.jsonc"),
		r#"// project defaults from OpenCode
{
  "autoResume": true,
  "theme": { "dark": "a//b", },
  /* block comment */
  "compaction": { "idleTimeoutSeconds": 900, },
}
"#,
	)
	.expect("write opencode.jsonc");

	assert_eq!(get_json(&sandbox, "autoResume")["value"], serde_json::json!(true));
	assert_eq!(get_json(&sandbox, "theme.dark")["value"], serde_json::json!("a//b"));
	assert_eq!(
		get_json(&sandbox, "compaction.idleTimeoutSeconds")["value"],
		serde_json::json!(900)
	);

	// Malformed JSONC must be skipped, not crash the layer read.
	fs::write(root.join("opencode.jsonc"), "{\n  \"autoResume\": true,\n")
		.expect("write broken jsonc");
	let output = sandbox.run(&["config", "get", "autoResume", "--json"]);
	assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
	assert!(
		String::from_utf8_lossy(&output.stderr).contains("skipping invalid settings"),
		"expected a warning, got: {}",
		String::from_utf8_lossy(&output.stderr)
	);
}

#[test]
fn first_write_seeds_the_legacy_settings_instead_of_losing_them() {
	let sandbox = Sandbox::new("legacy-seed");
	let agent_dir = sandbox.root.join("config/agent");
	fs::write(
		agent_dir.join("settings.json"),
		"{\n  // legacy JSONC\n  \"autoResume\": true,\n  \"theme\": { \"dark\": \"legacy-dark\" },\n}\n",
	)
	.expect("write legacy");
	assert!(!sandbox.agent_config().exists());

	let output = sandbox.run(&["config", "set", "defaultThinkingLevel", "max"]);
	assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

	assert_eq!(get_json(&sandbox, "autoResume")["value"], serde_json::json!(true));
	assert_eq!(get_json(&sandbox, "theme.dark")["value"], serde_json::json!("legacy-dark"));
	assert_eq!(get_json(&sandbox, "defaultThinkingLevel")["value"], serde_json::json!("max"));
	let written = fs::read_to_string(sandbox.agent_config()).expect("seeded config");
	assert!(written.contains("autoResume"), "legacy setting not persisted: {written}");
}

#[test]
fn provider_project_layers_override_builtin_but_native_model_roles_win() {
	let sandbox = Sandbox::new("project-order");
	let project = sandbox.project();
	fs::create_dir_all(project.join(".omp")).expect("mkdir .omp");
	fs::create_dir_all(project.join(".claude")).expect("mkdir .claude");
	fs::write(
		project.join(".omp/config.yml"),
		"autoResume: false\nmodelRoles:\n  task: builtin-role\n",
	)
	.expect("write native");
	fs::write(
		project.join(".claude/settings.json"),
		r#"{ "autoResume": true, "modelRoles": { "task": "claude-role" } }"#,
	)
	.expect("write claude");

	// Claude (80) is merged after builtin (100), so its value wins.
	assert_eq!(get_json(&sandbox, "autoResume")["value"], serde_json::json!(true));
	// Only the native file's modelRoles are re-merged last.
	let roles = get_json(&sandbox, "modelRoles");
	assert_eq!(roles["value"]["task"], serde_json::json!("builtin-role"));
}

#[test]
fn opencode_project_config_expands_env_and_file_variables() {
	let sandbox = Sandbox::new("opencode-vars");
	let project = sandbox.project();
	fs::write(project.join("extra-dark.txt"), "  from-file  \n").expect("write fragment");
	fs::write(
		project.join("opencode.jsonc"),
		r#"{ "theme": { "dark": "{file:extra-dark.txt}", "light": "{env:OMP_CONFIG_TEST_UNSET}" } }"#,
	)
	.expect("write opencode.jsonc");

	assert_eq!(get_json(&sandbox, "theme.dark")["value"], serde_json::json!("from-file"));
	assert_eq!(get_json(&sandbox, "theme.light")["value"], serde_json::json!(""));
}

#[test]
fn legacy_reset_keeps_every_other_key_and_archives_settings_json() {
	let sandbox = Sandbox::new("legacy-reset");
	let agent_dir = sandbox.root.join("config/agent");
	let settings_json = agent_dir.join("settings.json");
	fs::write(
		&settings_json,
		r#"{ "autoResume": true, "theme": { "dark": "kept" }, "codexResets": { "autoRedeem": false } }"#,
	)
	.expect("write legacy");

	let output = sandbox.run(&["config", "reset", "autoResume"]);
	assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

	assert_eq!(get_json(&sandbox, "theme.dark")["value"], serde_json::json!("kept"));
	assert_eq!(get_json(&sandbox, "codexResets.autoRedeem")["value"], serde_json::json!("no"));
	assert_eq!(get_json(&sandbox, "autoResume")["value"], serde_json::json!(false));
	assert!(!settings_json.exists(), "legacy settings.json should be archived");
	assert!(agent_dir.join("settings.json.bak").is_file(), "expected settings.json.bak");
}

#[test]
fn first_write_seeds_settings_kept_in_the_agent_database() {
	let sandbox = Sandbox::new("legacy-db");
	let db_path = sandbox.root.join("config/agent/agent.db");
	let connection = rusqlite::Connection::open(&db_path).expect("open agent.db");
	connection
		.execute_batch(
			r#"CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
			INSERT INTO settings (key, value) VALUES ('autoResume', 'true');
			INSERT INTO settings (key, value) VALUES ('theme', '{"dark":"db-dark"}');"#,
		)
		.expect("seed settings table");
	drop(connection);

	let output = sandbox.run(&["config", "set", "defaultThinkingLevel", "max"]);
	assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

	assert_eq!(get_json(&sandbox, "autoResume")["value"], serde_json::json!(true));
	assert_eq!(get_json(&sandbox, "theme.dark")["value"], serde_json::json!("db-dark"));
}

#[test]
fn legacy_only_install_is_migrated_before_get_and_list() {
	let sandbox = Sandbox::new("legacy-init-get");
	let agent_dir = sandbox.root.join("config/agent");
	fs::write(
		agent_dir.join("settings.json"),
		r#"{ "autoResume": true, "theme": { "dark": "legacy-dark" } }"#,
	)
	.expect("write legacy");
	assert!(!sandbox.agent_config().exists());

	// `runConfigCommand` awaits Settings.init() before dispatching, so a plain
	// get already sees the migrated legacy values.
	assert_eq!(get_json(&sandbox, "autoResume")["value"], serde_json::json!(true));
	assert_eq!(get_json(&sandbox, "theme.dark")["value"], serde_json::json!("legacy-dark"));

	// `list --json` is an object keyed by setting path (upstream shape).
	let listed = sandbox.stdout_json(&["config", "list", "--json"]);
	let entry = listed
		.as_object()
		.expect("list --json object")
		.get("autoResume")
		.expect("autoResume listed");
	assert_eq!(entry["value"], serde_json::json!(true));

	assert!(sandbox.agent_config().exists(), "init should write config.yml");
	assert!(agent_dir.join("settings.json.bak").is_file(), "legacy file archived");
}

#[test]
fn archiving_never_overwrites_an_existing_backup() {
	let sandbox = Sandbox::new("legacy-bak");
	let agent_dir = sandbox.root.join("config/agent");
	fs::write(agent_dir.join("settings.json.bak"), "pre-existing backup").expect("write old backup");
	fs::write(agent_dir.join("settings.json"), r#"{ "autoResume": true }"#).expect("write legacy");

	assert_eq!(get_json(&sandbox, "autoResume")["value"], serde_json::json!(true));

	assert_eq!(
		fs::read_to_string(agent_dir.join("settings.json.bak")).expect("old backup"),
		"pre-existing backup"
	);
	assert!(agent_dir.join("settings.json.bak.1").is_file(), "second backup slot used");
	assert!(!agent_dir.join("settings.json").exists(), "source archived");
}

#[test]
fn failed_seed_write_keeps_the_legacy_file_and_reports_the_error() {
	let sandbox = Sandbox::new("legacy-write-fail");
	let agent_dir = sandbox.root.join("config/agent");
	let settings_json = agent_dir.join("settings.json");
	fs::write(&settings_json, r#"{ "autoResume": true }"#).expect("write legacy");

	let mut permissions = fs::metadata(&agent_dir).expect("stat agent dir").permissions();
	permissions.set_readonly(true);
	fs::set_permissions(&agent_dir, permissions).expect("make read-only");
	let output = sandbox.run(&["config", "get", "autoResume", "--json"]);
	let mut permissions = fs::metadata(&agent_dir).expect("stat agent dir").permissions();
	#[allow(clippy::permissions_set_readonly_false, reason = "restore the writable sandbox dir")]
	permissions.set_readonly(false);
	fs::set_permissions(&agent_dir, permissions).expect("restore writable");

	assert!(!output.status.success(), "seeding into a read-only dir must fail");
	assert!(settings_json.is_file(), "legacy file must survive a failed write");
	assert!(!sandbox.agent_config().exists(), "no partial config.yml");

	// The next attempt still finds and migrates the legacy file.
	assert_eq!(get_json(&sandbox, "autoResume")["value"], serde_json::json!(true));
	assert!(agent_dir.join("settings.json.bak").is_file());
}
