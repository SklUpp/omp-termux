//! `config.yml` loading and merge.
//!
//! Only the runtime subset the headless binary consumes is modelled; unknown
//! root keys are ignored because the file is shared with the full TS app.

use std::{
	collections::{BTreeMap, HashMap, HashSet},
	env, fs,
	path::{Component, Path, PathBuf},
};

use pi_agent::mcp::{
	MCPHttpServerConfig, MCPServerConfig, MCPSseServerConfig, MCPStdioServerConfig, RequestIdFormat,
	validate_server_config,
};

use crate::{
	Error, Result,
	approve::ApprovalMode,
	dirs,
	yaml::{self, Value},
};

/// Allowed `auth` values for a provider block.
pub const AUTH_MODES: [&str; 3] = ["apiKey", "none", "oauth"];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderConfig {
	pub base_url: Option<String>,
	/// Env var name when it names one, otherwise a literal key. Resolution
	/// happens in [`Config::resolve_api_key`].
	pub api_key:  Option<String>,
	pub api:      Option<String>,
	pub auth:     Option<String>,
	pub headers:  BTreeMap<String, String>,
}

impl ProviderConfig {
	/// Resolve `apiKey`: the stored value is an environment variable name when
	/// that variable exists, otherwise the literal key.
	#[must_use]
	pub fn resolve_api_key(&self) -> Option<String> {
		let value = self.api_key.as_deref()?;
		Some(env::var(value).unwrap_or_else(|_| value.to_string()))
	}

	fn merge(&mut self, overlay: Self) {
		if overlay.base_url.is_some() {
			self.base_url = overlay.base_url;
		}
		if overlay.api_key.is_some() {
			self.api_key = overlay.api_key;
		}
		if overlay.api.is_some() {
			self.api = overlay.api;
		}
		if overlay.auth.is_some() {
			self.auth = overlay.auth;
		}
		self.headers.extend(overlay.headers);
	}
}

/// One `disabledProviders` entry: a bare provider id, or a path-scoped group
/// (`{path|paths|pathPrefix|pathPrefixes, values|items|providers}`) like the
/// upstream settings schema's path-scoped arrays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisabledProviderEntry {
	All(String),
	Scoped { prefixes: Vec<String>, providers: Vec<String> },
}

/// Edit tool variants: the settings schema's `edit.mode` values.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum EditMode {
	Replace,
	Patch,
	#[default]
	Hashline,
	ApplyPatch,
	Sloppy,
}

impl EditMode {
	#[must_use]
	pub fn parse(raw: &str) -> Option<Self> {
		match raw {
			"replace" => Some(Self::Replace),
			"patch" => Some(Self::Patch),
			"hashline" => Some(Self::Hashline),
			"apply_patch" => Some(Self::ApplyPatch),
			"sloppy" => Some(Self::Sloppy),
			_ => None,
		}
	}

	#[must_use]
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Replace => "replace",
			Self::Patch => "patch",
			Self::Hashline => "hashline",
			Self::ApplyPatch => "apply_patch",
			Self::Sloppy => "sloppy",
		}
	}
}

/// Raw `edit.modelVariants` layer value. Type filtering and the `atom`/`vim`
/// migration happen after the layers merge (upstream `#getEditVariantEntries`),
/// so an entry that is invalid in one layer but fixed by a later one keeps its
/// insertion position, and a non-object group replaces the lower layer instead
/// of being ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ModelVariants {
	/// Key absent from this layer: merging keeps the lower layer.
	#[default]
	Absent,
	/// Present but not a mapping: the merged group is no longer a mapping and
	/// yields no variants (the upstream `isRecord` guard).
	NotAMap,
	/// Present mapping in file order (the order is the precedence). `None`
	/// holds a non-string value, which can never match but still occupies its
	/// merge position.
	Entries(Vec<(String, Option<String>)>),
}

/// `edit` settings group consumed by the headless runtime.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EditConfig {
	/// `edit.enforceSeenLines`; absent means [`Self::DEFAULT_ENFORCE_SEEN_LINES`]
	/// (the upstream settings-schema default).
	pub enforce_seen_lines: Option<bool>,
	/// `edit.mode` after the `atom`/`vim` → `hashline` migration; absent means
	/// [`Self::mode`]'s `hashline` default.
	pub mode:               Option<EditMode>,
	/// `edit.modelVariants`, raw and in file insertion order; typed variants
	/// are derived in [`Self::variant_for_model`]. Merging is case-sensitive
	/// like the upstream deep merge; only matching folds case.
	pub model_variants:     ModelVariants,
}

impl pi_agent::tools::edit_variants::EditModeSettings for Config {
	fn get_edit_mode(&self) -> Option<&str> {
		self.edit.mode.map(EditMode::as_str)
	}

	fn get_edit_variant_for_model(
		&self,
		model: Option<&str>,
	) -> Option<pi_agent::tools::edit_variants::EditMode> {
		model
			.and_then(|model| self.edit.variant_for_model(model))
			.map(core_edit_mode)
	}

	fn get_read_line_numbers(&self) -> bool {
		self.read_line_numbers()
	}
}

/// CLI settings keep their own [`EditMode`]; map it onto the core wire enum at
/// this boundary (same five variants, exhaustive so drift fails to compile).
fn core_edit_mode(mode: EditMode) -> pi_agent::tools::edit_variants::EditMode {
	match mode {
		EditMode::Replace => pi_agent::tools::edit_variants::EditMode::Replace,
		EditMode::Patch => pi_agent::tools::edit_variants::EditMode::Patch,
		EditMode::Hashline => pi_agent::tools::edit_variants::EditMode::Hashline,
		EditMode::ApplyPatch => pi_agent::tools::edit_variants::EditMode::ApplyPatch,
		EditMode::Sloppy => pi_agent::tools::edit_variants::EditMode::Sloppy,
	}
}

impl EditConfig {
	/// Seen-line guard default, matching the upstream settings schema. The
	/// runtime maps this onto `EditToolOptions { enforce_seen_lines, .. }`.
	pub const DEFAULT_ENFORCE_SEEN_LINES: bool = false;
	/// `edit.mode` default (`DEFAULT_EDIT_MODE` upstream).
	pub const DEFAULT_MODE: EditMode = EditMode::Hashline;

	#[must_use]
	pub fn enforce_seen_lines(&self) -> bool {
		self.enforce_seen_lines.unwrap_or(Self::DEFAULT_ENFORCE_SEEN_LINES)
	}

	/// Configured `edit.mode`, or the `hashline` default.
	#[must_use]
	pub fn mode(&self) -> EditMode {
		self.mode.unwrap_or(Self::DEFAULT_MODE)
	}

	/// First valid `edit.modelVariants` pattern contained in `model`
	/// (case-insensitive), like upstream `getEditVariantForModel`. Raw keys stay
	/// distinct for merging and invalid values are skipped here rather than at
	/// parse time, so a value fixed by a later layer keeps its position; only
	/// matching folds case.
	#[must_use]
	pub fn variant_for_model(&self, model: &str) -> Option<EditMode> {
		let ModelVariants::Entries(entries) = &self.model_variants else {
			return None;
		};
		let model = model.to_lowercase();
		entries.iter().find_map(|(pattern, raw)| {
			let raw = raw.as_deref()?;
			if !model.contains(&pattern.to_lowercase()) {
				return None;
			}
			let normalized = match raw {
				"atom" | "vim" => "hashline",
				other => other,
			};
			EditMode::parse(normalized)
		})
	}

	fn merge(&mut self, overlay: Self) {
		if overlay.enforce_seen_lines.is_some() {
			self.enforce_seen_lines = overlay.enforce_seen_lines;
		}
		if overlay.mode.is_some() {
			self.mode = overlay.mode;
		}
		// Upstream deep-merges mappings and replaces the group whenever either
		// side is not a mapping: an overlay pattern overrides the raw value in
		// place (keeping the lower layer's position), new patterns append in
		// overlay order, and a non-object overlay drops the inherited group.
		match overlay.model_variants {
			ModelVariants::Absent => {},
			ModelVariants::NotAMap => self.model_variants = ModelVariants::NotAMap,
			ModelVariants::Entries(entries) => {
				if let ModelVariants::Entries(existing) = &mut self.model_variants {
					for (pattern, raw) in entries {
						match existing.iter().position(|(key, _)| *key == pattern) {
							Some(index) => existing[index].1 = raw,
							None => existing.push((pattern, raw)),
						}
					}
				} else {
					self.model_variants = ModelVariants::Entries(entries);
				}
			},
		}
	}
}

/// `async` settings group consumed by the headless runtime.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AsyncConfig {
	/// `async.enabled`; absent means [`Self::DEFAULT_ENABLED`] (upstream
	/// settings-schema default `true`).
	pub enabled: Option<bool>,
}

impl AsyncConfig {
	/// Upstream `async.enabled` default. A explicit `false` disables background
	/// task execution; the headless runtime still runs `task` synchronously.
	pub const DEFAULT_ENABLED: bool = true;

	#[must_use]
	pub fn enabled(&self) -> bool {
		self.enabled.unwrap_or(Self::DEFAULT_ENABLED)
	}

	fn merge(&mut self, overlay: Self) {
		if overlay.enabled.is_some() {
			self.enabled = overlay.enabled;
		}
	}
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AskConfig {
	/// `ask.enabled`; absent means the upstream default (`true`).
	pub enabled: Option<bool>,
	/// `ask.timeout` in milliseconds; absent or `0` disables the timeout.
	/// Legacy settings values above 1000 were milliseconds and are converted to
	/// seconds before mapping here, matching the TypeScript migration.
	pub timeout_ms: Option<u64>,
}

impl AskConfig {
	pub const DEFAULT_ENABLED: bool = true;

	#[must_use]
	pub fn enabled(&self) -> bool {
		self.enabled.unwrap_or(Self::DEFAULT_ENABLED)
	}

	#[must_use]
	pub fn timeout(&self) -> Option<std::time::Duration> {
		self
			.timeout_ms
			.filter(|millis| *millis > 0)
			.map(std::time::Duration::from_millis)
	}

	fn merge(&mut self, overlay: Self) {
		if overlay.enabled.is_some() {
			self.enabled = overlay.enabled;
		}
		if overlay.timeout_ms.is_some() {
			self.timeout_ms = overlay.timeout_ms;
		}
	}
}

/// `lsp` settings group consumed by the headless runtime.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LspConfig {
	/// `lsp.enabled`; absent means [`Self::DEFAULT_ENABLED`].
	pub enabled: Option<bool>,
	/// `lsp.shared`; absent means [`Self::DEFAULT_SHARED`] (one language
	/// server per project through the daemon broker).
	pub shared:  Option<bool>,
}

impl LspConfig {
	/// Upstream `lsp.enabled` default.
	pub const DEFAULT_ENABLED: bool = true;
	/// Upstream `lsp.shared` default.
	pub const DEFAULT_SHARED: bool = true;

	#[must_use]
	pub fn enabled(&self) -> bool {
		self.enabled.unwrap_or(Self::DEFAULT_ENABLED)
	}

	#[must_use]
	pub fn shared(&self) -> bool {
		self.shared.unwrap_or(Self::DEFAULT_SHARED)
	}

	fn merge(&mut self, overlay: Self) {
		if overlay.enabled.is_some() {
			self.enabled = overlay.enabled;
		}
		if overlay.shared.is_some() {
			self.shared = overlay.shared;
		}
	}
}

/// `debug` settings group: the DAP debug tool.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DebugConfig {
	/// `debug.enabled`; absent means [`Self::DEFAULT_ENABLED`].
	pub enabled: Option<bool>,
}

impl DebugConfig {
	/// Upstream `debug.enabled` default.
	pub const DEFAULT_ENABLED: bool = true;

	#[must_use]
	pub fn enabled(&self) -> bool {
		self.enabled.unwrap_or(Self::DEFAULT_ENABLED)
	}

	fn merge(&mut self, overlay: Self) {
		if overlay.enabled.is_some() {
			self.enabled = overlay.enabled;
		}
	}
}

/// `checkpoint` settings group: the checkpoint/rewind tools.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckpointConfig {
	/// `checkpoint.enabled`; absent means [`Self::DEFAULT_ENABLED`].
	pub enabled: Option<bool>,
}

impl CheckpointConfig {
	/// Upstream `checkpoint.enabled` default.
	pub const DEFAULT_ENABLED: bool = false;

	#[must_use]
	pub fn enabled(&self) -> bool {
		self.enabled.unwrap_or(Self::DEFAULT_ENABLED)
	}

	fn merge(&mut self, overlay: Self) {
		if overlay.enabled.is_some() {
			self.enabled = overlay.enabled;
		}
	}
}

/// `eval` / `python` / `ruby` / `julia` settings groups.
///
/// Raw layer values only: environment booleans (`PI_PY` …) are folded in by the
/// runtime, so `config.yml` parsing never reads process environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EvalConfig {
	/// `eval.py`; absent keeps the eval default (`true`).
	pub python:             Option<bool>,
	/// `eval.js`; absent keeps the eval default (`true`).
	pub js:                 Option<bool>,
	/// `eval.rb`; absent keeps the eval default (`false`).
	pub ruby:               Option<bool>,
	/// `eval.jl`; absent keeps the eval default (`false`).
	pub julia:              Option<bool>,
	/// `python.kernelMode` (`session` or `per-call`).
	pub python_kernel_mode: Option<pi_agent::eval::KernelMode>,
	pub python_interpreter: Option<String>,
	pub ruby_interpreter:   Option<String>,
	pub julia_interpreter:  Option<String>,
	/// `eval.autoBackground.enabled`; absent keeps the upstream default
	/// (`false`), so long cells stay foreground unless configured.
	pub auto_background_enabled: Option<bool>,
	/// `eval.autoBackground.thresholdMs`; absent keeps the upstream default
	/// (`60_000`). Only consulted when auto-background is enabled.
	pub auto_background_threshold_ms: Option<u64>,
}

impl EvalConfig {
	fn merge(&mut self, overlay: Self) {
		for (target, value) in [
			(&mut self.python, overlay.python),
			(&mut self.js, overlay.js),
			(&mut self.ruby, overlay.ruby),
			(&mut self.julia, overlay.julia),
		] {
			if value.is_some() {
				*target = value;
			}
		}
		if overlay.python_kernel_mode.is_some() {
			self.python_kernel_mode = overlay.python_kernel_mode;
		}
		if overlay.auto_background_enabled.is_some() {
			self.auto_background_enabled = overlay.auto_background_enabled;
		}
		if overlay.auto_background_threshold_ms.is_some() {
			self.auto_background_threshold_ms = overlay.auto_background_threshold_ms;
		}
		for (target, value) in [
			(&mut self.python_interpreter, overlay.python_interpreter),
			(&mut self.ruby_interpreter, overlay.ruby_interpreter),
			(&mut self.julia_interpreter, overlay.julia_interpreter),
		] {
			if value.is_some() {
				*target = value;
			}
		}
	}
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
	pub default_model:        Option<String>,
	pub providers:            BTreeMap<String, ProviderConfig>,
	pub system_prompt:        Option<String>,
	pub append_system_prompt: Option<String>,
	pub approval_mode:        Option<ApprovalMode>,
	pub allow:                Vec<String>,
	pub cwd:                  Option<PathBuf>,
	pub tools:                Option<Vec<String>>,
	pub edit:                 EditConfig,
	pub async_config:         AsyncConfig,
	pub ask:                  AskConfig,
	pub lsp:                  LspConfig,
	pub debug:                DebugConfig,
	pub checkpoint:           CheckpointConfig,
	/// `eval.*` backend enablement and interpreter groups.
	pub eval_config:          EvalConfig,
	/// `tools.maxTimeout` in seconds; `0` means no global cap.
	pub max_tool_timeout_secs: Option<u64>,
	/// `readLineNumbers` (root schema key, not a `read` group leaf); absent
	/// means `false`. Drives plain line numbers in read output.
	pub read_line_numbers:    Option<bool>,
	/// `modelRoles` aliases (`task`, `smol`, `slow`, ...). `default` also feeds
	/// `default_model`.
	pub model_roles:          BTreeMap<String, String>,
	/// `extensions` roots searched for `<root>/agents`.
	pub extensions:           Option<Vec<PathBuf>>,
	/// `task.disabledAgents`.
	pub disabled_agents:      Option<Vec<String>>,
	/// `disabledProviders`; path-scoped groups resolve against the run cwd in
	/// [`Config::disabled_providers`].
	pub disabled_providers:   Option<Vec<DisabledProviderEntry>>,
	/// `task.agentModelOverrides` (string or list per agent).
	pub agent_model_overrides: BTreeMap<String, Vec<String>>,
	/// `task.maxRecursionDepth`.
	pub max_recursion_depth:  Option<i32>,
	/// `task.maxConcurrency`.
	pub max_concurrency:      Option<usize>,
	/// `task.maxEffort`.
	pub max_effort:           Option<String>,
	/// Paths that contributed, base first.
	pub sources:              Vec<PathBuf>,
}

impl Config {
	pub fn merge(&mut self, overlay: Self) {
		if overlay.default_model.is_some() {
			self.default_model = overlay.default_model;
		}
		if overlay.system_prompt.is_some() {
			self.system_prompt = overlay.system_prompt;
		}
		if overlay.append_system_prompt.is_some() {
			self.append_system_prompt = overlay.append_system_prompt;
		}
		if overlay.approval_mode.is_some() {
			self.approval_mode = overlay.approval_mode;
		}
		if overlay.cwd.is_some() {
			self.cwd = overlay.cwd;
		}
		if overlay.tools.is_some() {
			self.tools = overlay.tools;
		}
		self.edit.merge(overlay.edit);
		self.async_config.merge(overlay.async_config);
		self.ask.merge(overlay.ask);
		self.lsp.merge(overlay.lsp);
		self.debug.merge(overlay.debug);
		self.checkpoint.merge(overlay.checkpoint);
		self.eval_config.merge(overlay.eval_config);
		if overlay.max_tool_timeout_secs.is_some() {
			self.max_tool_timeout_secs = overlay.max_tool_timeout_secs;
		}
		if overlay.read_line_numbers.is_some() {
			self.read_line_numbers = overlay.read_line_numbers;
		}
		self.model_roles.extend(overlay.model_roles);
		if overlay.extensions.is_some() {
			self.extensions = overlay.extensions;
		}
		if overlay.disabled_agents.is_some() {
			self.disabled_agents = overlay.disabled_agents;
		}
		if overlay.disabled_providers.is_some() {
			self.disabled_providers = overlay.disabled_providers;
		}
		for (agent, patterns) in overlay.agent_model_overrides {
			self.agent_model_overrides.insert(agent, patterns);
		}
		if overlay.max_recursion_depth.is_some() {
			self.max_recursion_depth = overlay.max_recursion_depth;
		}
		if overlay.max_concurrency.is_some() {
			self.max_concurrency = overlay.max_concurrency;
		}
		if overlay.max_effort.is_some() {
			self.max_effort = overlay.max_effort;
		}
		if !overlay.allow.is_empty() {
			self.allow = overlay.allow;
		}
		for (id, provider) in overlay.providers {
			self.providers.entry(id).or_default().merge(provider);
		}
		self.sources.extend(overlay.sources);
	}

	/// Base config (agent dir `config.yml`, then `config.yaml`), project config
	/// under `cwd`, `PI_CONFIG_FILES`, then each `--config` overlay in order;
	/// later files win. Mirrors `Settings` precedence: global <- project <-
	/// environment overlays <- CLI overlays <- runtime overrides (applied by the
	/// caller).
	pub fn load(overlays: &[PathBuf]) -> Result<Self> {
		Self::load_with_cwd(overlays, None, &env_config_files())
	}

	/// Load with the run's effective working directory (already resolved from
	/// `--cwd` by the caller) so project discovery and relative overlays use it.
	pub fn load_run(overlays: &[PathBuf], explicit_cwd: Option<&Path>) -> Result<Self> {
		Self::load_with_cwd(overlays, explicit_cwd, &env_config_files())
	}

	pub fn load_with_cwd(
		overlays: &[PathBuf],
		explicit_cwd: Option<&Path>,
		env_overlays: &[PathBuf],
	) -> Result<Self> {
		Self::load_layers(base_path().as_deref(), explicit_cwd, env_overlays, overlays)
	}

	fn load_layers(
		base: Option<&Path>,
		explicit_cwd: Option<&Path>,
		env_overlays: &[PathBuf],
		overlays: &[PathBuf],
	) -> Result<Self> {
		let mut config = Self::default();
		let mut has_base = false;
		if let Some(base) = base {
			config.merge(Self::load_file(base)?);
			has_base = true;
		}
		if !has_base {
			// `docs/settings.md`: `agent/settings.json` is the legacy global
			// settings file, superseded once any main YAML exists. Reading it
			// here mirrors the migration result without writing.
			let legacy = dirs::agent_dir().join("settings.json");
			if legacy.is_file() {
				match Self::load_settings_json(&legacy) {
					Ok(Some(overlay)) => config.merge(overlay),
					Ok(None) => {
						eprintln!("warning: skipping malformed global settings {}", legacy.display());
					},
					Err(_) => {
						eprintln!("warning: skipping invalid global settings {}", legacy.display());
					},
				}
			}
		}
		let process_cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
		let cwd = explicit_cwd
			.map(|path| dirs::absolute(&process_cwd, path))
			.or_else(|| {
				config
					.cwd
					.as_ref()
					.map(|path| dirs::absolute(&process_cwd, path))
			})
			.unwrap_or(process_cwd);
		Self::apply_project_layers(&mut config, &cwd)?;
		for path in env_overlays.iter().chain(overlays) {
			let resolved = resolve_layer_path(&cwd, path);
			config.merge(Self::load_file(&resolved)?);
		}
		Ok(config)
	}

	fn apply_project_layers(config: &mut Self, cwd: &Path) -> Result<()> {
		let project_dir = dirs::project_config_dir(cwd);
		let settings_path = project_dir.join("settings.json");
		if settings_path.is_file() {
			match Self::load_settings_json(&settings_path) {
				Ok(Some(overlay)) => config.merge(overlay),
				Ok(None) => {
					eprintln!(
						"warning: skipping malformed project settings {}",
						settings_path.display()
					);
				},
				Err(_) => {
					eprintln!(
						"warning: skipping invalid project settings {}",
						settings_path.display()
					);
				},
			}
		}
		// TS contract: an invalid project `config.yml` is fatal after the file is
		// quarantined (`Settings.#loadYamlIfPresentForStartup` throws). This binary
		// must not move or delete user files without authorization, so read-only
		// startup fails in place: bytes stay untouched, the path is reported, and
		// the YAML reason is sanitized by `load_file`. Explicit `--config` and
		// `PI_CONFIG_FILES` overlays are fatal for the same contract reason.
		let project_config = project_dir.join("config.yml");
		if project_config.is_file() {
			config.merge(Self::load_file(&project_config)?);
		}
		Ok(())
	}

	/// Project `settings.json` is a settings-schema document, not the legacy
	/// root-key dialect; map only the fields this binary consumes.
	fn load_settings_json(path: &Path) -> Result<Option<Self>> {
		let text = fs::read_to_string(path)
			.map_err(|err| Error::with_source(format!("cannot read {}", path.display()), err))?;
		let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
			return Ok(None);
		};
		let value = json_to_value(&json);
		Self::from_settings_value(&value, path).map(Some)
	}

	fn load_file(path: &Path) -> Result<Self> {
		if !path.is_file() {
			return Err(Error::new(format!("config file not found: {}", path.display())));
		}
		let text = fs::read_to_string(path)
			.map_err(|err| Error::with_source(format!("cannot read {}", path.display()), err))?;
		let value = yaml::parse(&text)
			.map_err(|err| Error::new(format!("{}: {}", path.display(), sanitize_yaml_error(&err.to_string()))))?;
		Self::from_value(&value, path)
	}

	pub fn from_value(value: &Value, path: &Path) -> Result<Self> {
		let mut config = Self::default();
		let Value::Map(pairs) = value else {
			if matches!(value, Value::Null) {
				return Ok(config);
			}
			return Err(Error::new(format!(
				"{}: expected a mapping at the document root",
				path.display()
			)));
		};
		for (key, item) in pairs {
			match key.as_str() {
				"defaultModel" => config.default_model = expect_text(item, key, path)?,
				"systemPrompt" => config.system_prompt = expect_text(item, key, path)?,
				"appendSystemPrompt" => config.append_system_prompt = expect_text(item, key, path)?,
				"cwd" => {
					config.cwd = expect_text(item, key, path)?.map(PathBuf::from);
				},
				"approvalMode" => config.approval_mode = parse_approval(item, key, path)?,
				"allow" => config.allow = expect_list_of_text(item, key, path)?,
				"tools" => config.tools = expect_tools(item, key, path)?,
				"providers" => config.providers = parse_providers(item, path)?,
				// Unknown keys are ignored: the config file is shared with the
				// full TS app, which carries extra keys (setupVersion etc.).
				_ => {},
			}
		}
		apply_nested_settings(&mut config, value, path)?;
		config.sources.push(path.to_path_buf());
		Ok(config)
	}

	/// Project `settings.json` is a `SETTINGS_SCHEMA` document, not the legacy
	/// root-key dialect. Only fields this binary consumes are mapped:
	/// `modelRoles.default`, `tools.approvalMode`, and `disabledProviders`.
	/// Root-level legacy keys and other schema groups are intentionally ignored
	/// (they are either owned by the full TS app or unsupported by the headless
	/// runtime).
	fn from_settings_value(value: &Value, path: &Path) -> Result<Self> {
		let mut config = Self::default();
		let Value::Map(_) = value else {
			if matches!(value, Value::Null) {
				return Ok(config);
			}
			return Err(Error::new(format!(
				"{}: expected a JSON object at the document root",
				path.display()
			)));
		};
		apply_nested_settings(&mut config, value, path)?;
		config.sources.push(path.to_path_buf());
		Ok(config)
	}

	/// `readLineNumbers` with its `false` default. The runtime combines this
	/// with the hashline display resolver (hashline anchors imply line numbers).
	#[must_use]
	pub fn read_line_numbers(&self) -> bool {
		self.read_line_numbers.unwrap_or(false)
	}

	/// Resolve a provider API key through its `config.yml` connection block.
	#[must_use]
	pub fn resolve_api_key(&self, provider: &str) -> Option<String> {
		self.providers.get(provider)?.resolve_api_key()
	}

	/// `disabledProviders` ids that apply to `cwd`: plain entries plus
	/// path-scoped groups whose prefix contains it (upstream
	/// `resolvePathScopedStringArray`). Paths are lexical — `.`/`..` collapse
	/// without filesystem access, like `path.resolve`, so nonexistent and
	/// symlinked paths still compare; no prefix escaping is allowed.
	#[must_use]
	pub fn disabled_providers(&self, cwd: &Path) -> Vec<String> {
		let Some(entries) = &self.disabled_providers else {
			return Vec::new();
		};
		let base = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
		let cwd = normalize_lexically(&dirs::absolute(&base, cwd));
		let mut ids = Vec::new();
		for entry in entries {
			match entry {
				DisabledProviderEntry::All(id) => ids.push(id.clone()),
				DisabledProviderEntry::Scoped { prefixes, providers } => {
					let applies = prefixes.iter().any(|prefix| {
						let prefix =
							normalize_lexically(&resolve_layer_path(&base, Path::new(prefix)));
						cwd.starts_with(prefix)
					});
					if applies {
						ids.extend(providers.iter().cloned());
					}
				},
			}
		}
		ids
	}

	/// Apply dedicated CLI flags after the layered load. Runtime overrides are
	/// the highest settings layer (`docs/settings.md` order 1) and are never
	/// persisted: flags replace the file value even when the file set one.
	pub fn apply_runtime(&mut self, cli: &crate::args::Cli) {
		if let Some(tools) = &cli.tools {
			self.tools = Some(
				tools
					.iter()
					.flat_map(|value| value.split(','))
					.map(str::trim)
					.filter(|value| !value.is_empty())
					.map(str::to_string)
					.collect(),
			);
		}
		if let Some(mode) = cli.approval_mode {
			self.approval_mode = Some(mode);
		} else if cli.yolo {
			self.approval_mode = Some(ApprovalMode::Yolo);
		}
		if !cli.allow.is_empty() {
			let mut allow = cli.allow.clone();
			allow.append(&mut self.allow);
			self.allow = allow;
		}
		if let Some(input) = &cli.system_prompt {
			self.system_prompt = Some(input.clone());
		}
		if let Some(input) = &cli.append_system_prompt {
			self.append_system_prompt = Some(input.clone());
		}
	}
}

fn apply_nested_settings(config: &mut Config, value: &Value, path: &Path) -> Result<()> {
	if let Some(disabled) = value.get("disabledProviders") {
		config.disabled_providers = Some(parse_disabled_providers(disabled, path)?);
	}
	if let Some(roles) = value.get("modelRoles").and_then(Value::as_map) {
		for (role, pattern) in roles {
			let Some(pattern) = expect_text(pattern, &format!("modelRoles.{role}"), path)? else {
				continue;
			};
			if role == "default" {
				config.default_model = Some(pattern.clone());
			}
			config.model_roles.insert(role.clone(), pattern);
		}
	}
	if let Some(task) = value.get("task").and_then(Value::as_map) {
		for (key, item) in task {
			match key.as_str() {
				"disabledAgents" => {
					config.disabled_agents = Some(expect_list_of_text(
						item,
						"task.disabledAgents",
						path,
					)?);
				},
				"agentModelOverrides" => {
					let overrides = item.as_map().ok_or_else(|| {
						invalid(path, "task.agentModelOverrides", "expected a mapping")
					})?;
					for (agent, patterns) in overrides {
						config
							.agent_model_overrides
							.insert(agent.clone(), expect_model_patterns(patterns, agent, path)?);
					}
				},
				"maxRecursionDepth" => {
					config.max_recursion_depth = expect_i32(item, "task.maxRecursionDepth", path)?;
				},
				"maxConcurrency" => {
					config.max_concurrency = expect_usize(item, "task.maxConcurrency", path)?;
				},
				"maxEffort" => {
					config.max_effort = expect_text(item, "task.maxEffort", path)?;
				},
				_ => {},
			}
		}
	}
	if let Some(extensions) = value.get("extensions").and_then(Value::as_list) {
		config.extensions = Some(
			extensions
				.iter()
				.map(|item| {
					item.to_text()
						.map(PathBuf::from)
						.ok_or_else(|| invalid(path, "extensions", "expected a sequence of paths"))
				})
				.collect::<Result<Vec<_>>>()?,
		);
	}
	if let Some(mode) = value
		.get("tools")
		.and_then(|tools| tools.get("approvalMode"))
	{
		config.approval_mode = parse_approval(mode, "tools.approvalMode", path)?;
	}
	if let Some(enforce) = value
		.get("edit")
		.and_then(|edit| edit.get("enforceSeenLines"))
	{
		config.edit.enforce_seen_lines = expect_bool(enforce, "edit.enforceSeenLines", path)?;
	}
	if let Some(mode) = settings_leaf(value, "edit", "mode", "edit.mode") {
		config.edit.mode = parse_edit_mode(mode, "edit.mode", path)?;
	}
	// `edit.modelVariants` is an untyped object in the upstream schema: raw
	// entries are kept for the merge and filtered when matching, and a
	// non-object group replaces the merged group (upstream `isRecord` guard).
	if let Some(variants) = value.get("edit").and_then(|edit| edit.get("modelVariants")) {
		config.edit.model_variants = parse_model_variants(variants);
	}
	// Root schema leaf (not `read.lineNumbers`); legacy `readHashLines` stays an
	// ignored unknown root key, matching the upstream migration that deletes it.
	if let Some(enabled) = value.get("readLineNumbers") {
		config.read_line_numbers = expect_bool(enabled, "readLineNumbers", path)?;
	}
	if let Some(enabled) = value.get("async").and_then(|async_settings| async_settings.get("enabled"))
	{
		config.async_config.enabled = expect_bool(enabled, "async.enabled", path)?;
	}
	if let Some(ask) = value.get("ask") {
		if let Some(enabled) = ask.get("enabled") {
			config.ask.enabled = expect_bool(enabled, "ask.enabled", path)?;
		}
		if let Some(timeout) = ask.get("timeout") {
			config.ask.timeout_ms = parse_ask_timeout(timeout, "ask.timeout", path)?;
		}
	}
	if let Some(enabled) = settings_leaf(value, "lsp", "enabled", "lsp.enabled") {
		config.lsp.enabled = expect_bool(enabled, "lsp.enabled", path)?;
	}
	if let Some(shared) = settings_leaf(value, "lsp", "shared", "lsp.shared") {
		config.lsp.shared = expect_bool(shared, "lsp.shared", path)?;
	}
	if let Some(enabled) = settings_leaf(value, "debug", "enabled", "debug.enabled") {
		config.debug.enabled = expect_bool(enabled, "debug.enabled", path)?;
	}
	if let Some(enabled) = settings_leaf(value, "checkpoint", "enabled", "checkpoint.enabled") {
		config.checkpoint.enabled = expect_bool(enabled, "checkpoint.enabled", path)?;
	}
	for (language, group, dotted) in [
		("python", "py", "eval.py"),
		("javascript", "js", "eval.js"),
		("ruby", "rb", "eval.rb"),
		("julia", "jl", "eval.jl"),
	] {
		if let Some(enabled) = settings_leaf(value, "eval", group, dotted) {
			let enabled = expect_bool(enabled, dotted, path)?;
			match language {
				"python" => config.eval_config.python = enabled,
				"javascript" => config.eval_config.js = enabled,
				"ruby" => config.eval_config.ruby = enabled,
				"julia" => config.eval_config.julia = enabled,
				_ => {},
			}
		}
	}
	if let Some(mode) = settings_leaf(value, "python", "kernelMode", "python.kernelMode") {
		config.eval_config.python_kernel_mode =
			parse_kernel_mode(mode, "python.kernelMode", path)?;
	}
	if let Some(interpreter) =
		settings_leaf(value, "python", "interpreter", "python.interpreter")
	{
		config.eval_config.python_interpreter =
			expect_text(interpreter, "python.interpreter", path)?
				.filter(|value| !value.trim().is_empty());
	}
	if let Some(interpreter) = settings_leaf(value, "ruby", "interpreter", "ruby.interpreter") {
		config.eval_config.ruby_interpreter =
			expect_text(interpreter, "ruby.interpreter", path)?
				.filter(|value| !value.trim().is_empty());
	}
	if let Some(interpreter) = settings_leaf(value, "julia", "interpreter", "julia.interpreter") {
		config.eval_config.julia_interpreter =
			expect_text(interpreter, "julia.interpreter", path)?
				.filter(|value| !value.trim().is_empty());
	}
	if let Some(enabled) = settings_nested_leaf(
		value,
		"eval",
		"autoBackground",
		"enabled",
		"eval.autoBackground.enabled",
	) {
		config.eval_config.auto_background_enabled =
			expect_bool(enabled, "eval.autoBackground.enabled", path)?;
	}
	if let Some(threshold) = settings_nested_leaf(
		value,
		"eval",
		"autoBackground",
		"thresholdMs",
		"eval.autoBackground.thresholdMs",
	) {
		config.eval_config.auto_background_threshold_ms =
			expect_millis(threshold, "eval.autoBackground.thresholdMs", path)?;
	}
	if let Some(timeout) = settings_leaf(value, "tools", "maxTimeout", "tools.maxTimeout") {
		config.max_tool_timeout_secs = expect_usize(timeout, "tools.maxTimeout", path)?
			.map(|seconds| seconds as u64);
	}
	Ok(())
}

/// `python.kernelMode`: the schema enum, so an invalid value is a load error
/// rather than a silent session-mode fallback.
fn parse_kernel_mode(
	value: &Value,
	key: &str,
	path: &Path,
) -> Result<Option<pi_agent::eval::KernelMode>> {
	let Some(raw) = expect_text(value, key, path)? else {
		return Ok(None);
	};
	match raw.as_str() {
		"session" => Ok(Some(pi_agent::eval::KernelMode::Session)),
		"per-call" => Ok(Some(pi_agent::eval::KernelMode::PerCall)),
		_ => Err(invalid(path, key, "expected `session` or `per-call`")),
	}
}

/// Settings leaf written either nested (`lsp: { enabled: … }`) or as a quoted
/// dotted schema path (`"lsp.enabled": …`). The nested form wins, and a
/// non-object group is dropped wholesale like the upstream settings
/// group-shadow guard (`dropSettingsGroupShadows`): its leaves fall back to
/// the defaults instead of erroring.
fn settings_leaf<'a>(value: &'a Value, group: &str, leaf: &str, dotted: &str) -> Option<&'a Value> {
	let Some(group_value) = value.get(group) else {
		return value.get(dotted);
	};
	match group_value {
		Value::Map(_) => group_value.get(leaf),
		_ => None,
	}
}

/// Two-level settings leaf (`eval.autoBackground.enabled`). Same rules as
/// [`settings_leaf`]: the nested path wins, and a non-object group or subgroup
/// shadows every leaf below it, matching `dropSettingsGroupShadows`.
fn settings_nested_leaf<'a>(
	value: &'a Value,
	group: &str,
	subgroup: &str,
	leaf: &str,
	dotted: &str,
) -> Option<&'a Value> {
	let Some(group_value) = value.get(group) else {
		return value.get(dotted);
	};
	let Value::Map(_) = group_value else {
		return None;
	};
	let Some(subgroup_value) = group_value.get(subgroup) else {
		return None;
	};
	match subgroup_value {
		Value::Map(_) => subgroup_value.get(leaf),
		_ => None,
	}
}

/// `edit.mode`: legacy `atom`/`vim` values migrate to `hashline`, then the
/// remaining value must be a known variant. Unlike the untyped
/// `modelVariants` group this is a schema enum, so a bad value fails the load
/// at the trust boundary instead of silently defaulting.
fn parse_edit_mode(value: &Value, key: &str, path: &Path) -> Result<Option<EditMode>> {
	let Some(raw) = expect_text(value, key, path)? else {
		return Ok(None);
	};
	let normalized = match raw.as_str() {
		"atom" | "vim" => "hashline",
		other => other,
	};
	EditMode::parse(normalized).map(Some).ok_or_else(|| {
		invalid(path, key, "expected one of replace, patch, hashline, apply_patch, sloppy")
	})
}

/// Raw `edit.modelVariants` layer value in file insertion order. Mirrors
/// upstream `#getEditVariantEntries`: every key is kept so the deep merge stays
/// case-sensitive (`GPT` and `gpt` are distinct entries) and insertion position
/// survives until [`EditConfig::variant_for_model`] filters; the untyped group
/// may hold non-strings, kept as `None` (they never match), and a non-mapping
/// value becomes [`ModelVariants::NotAMap`] so it replaces inherited entries.
fn parse_model_variants(value: &Value) -> ModelVariants {
	let Some(entries) = value.as_map() else {
		return ModelVariants::NotAMap;
	};
	ModelVariants::Entries(
		entries
			.iter()
			.map(|(pattern, item)| (pattern.clone(), item.as_str().map(str::to_string)))
			.collect(),
	)
}

/// `disabledProviders` entries: bare strings, plus path-scoped maps whose
/// prefixes gate their `values`/`items`/`providers`. Other item shapes are
/// skipped, matching the upstream resolver's isRecord guard.
fn parse_disabled_providers(value: &Value, path: &Path) -> Result<Vec<DisabledProviderEntry>> {
	if matches!(value, Value::Null) {
		return Ok(Vec::new());
	}
	let Some(items) = value.as_list() else {
		return Err(invalid(path, "disabledProviders", "expected a sequence"));
	};
	let mut entries = Vec::new();
	for item in items {
		match item {
			Value::Str(id) => entries.push(DisabledProviderEntry::All(id.clone())),
			Value::Map(_) => {
				let prefixes: Vec<String> =
					["path", "paths", "pathPrefix", "pathPrefixes"]
						.iter()
						.flat_map(|key| string_or_list(item.get(key)))
						.collect();
				if prefixes.is_empty() {
					continue;
				}
				let providers = ["values", "items", "providers"]
					.iter()
					.flat_map(|key| string_or_list(item.get(key)))
					.collect();
				entries.push(DisabledProviderEntry::Scoped { prefixes, providers });
			},
			_ => {},
		}
	}
	Ok(entries)
}

fn string_or_list(value: Option<&Value>) -> Vec<String> {
	match value {
		Some(Value::Str(text)) => vec![text.clone()],
		Some(Value::List(items)) => items.iter().filter_map(Value::to_text).collect(),
		_ => Vec::new(),
	}
}

/// `ask.timeout` is seconds; legacy values above 1000 are migrated from
/// milliseconds. `0` disables the timeout.
fn parse_ask_timeout(value: &Value, key: &str, path: &Path) -> Result<Option<u64>> {
	let seconds = match value {
		Value::Null => return Ok(None),
		Value::Int(value) => {
			if *value < 0 {
				return Err(invalid(path, key, "expected a non-negative number of seconds"));
			}
			if *value > 1000 {
				(*value as f64 / 1000.0).round()
			} else {
				*value as f64
			}
		},
		Value::Float(value) => {
			if !value.is_finite() || *value < 0.0 {
				return Err(invalid(path, key, "expected a non-negative number of seconds"));
			}
			if *value > 1000.0 { *value / 1000.0 } else { *value }
		},
		_ => return Err(invalid(path, key, "expected a number of seconds")),
	};
	let millis = (seconds * 1000.0).round() as u64;
	Ok(Some(millis))
}

fn parse_approval(value: &Value, key: &str, path: &Path) -> Result<Option<ApprovalMode>> {
	let raw = expect_text(value, key, path)?
		.ok_or_else(|| invalid(path, key, "expected one of always-ask, write, yolo"))?;
	ApprovalMode::parse(&raw)
		.map(Some)
		.ok_or_else(|| invalid(path, key, "expected one of always-ask, write, yolo"))
}

fn expect_tools(value: &Value, key: &str, path: &Path) -> Result<Option<Vec<String>>> {
	match value {
		// `tools` is a list in the legacy dialect and a settings group in the
		// TS schema; accept the object form so a shared project file loads.
		Value::Map(_) => Ok(None),
		Value::Null => Ok(Some(Vec::new())),
		other => other
			.as_list()
			.map(|_| expect_list_of_text(other, key, path).map(Some))
			.unwrap_or_else(|| Err(invalid(path, key, "expected a sequence of strings"))),
	}
}

/// `PI_CONFIG_FILES` entries in declared order (`:` on Unix, `;` on Windows).
#[must_use]
pub fn env_config_files() -> Vec<PathBuf> {
	env::var_os("PI_CONFIG_FILES")
		.map(|value| {
			env::split_paths(&value)
				.filter(|path| !path.as_os_str().is_empty())
				.collect()
		})
		.unwrap_or_default()
}

/// Overlay paths resolve relative to the effective project cwd after `~`
/// expansion, matching the TS `Settings` constructor.
pub(crate) fn resolve_layer_path(cwd: &Path, raw: &Path) -> PathBuf {
	let text = raw.to_string_lossy();
	let expanded = if let Some(home) = dirs::home_dir() {
		if text == "~" {
			home
		} else if let Some(rest) = text.strip_prefix("~/") {
			home.join(rest)
		} else {
			raw.to_path_buf()
		}
	} else {
		raw.to_path_buf()
	};
	dirs::absolute(cwd, &expanded)
}

/// Lexically collapse `.`/`..` components without touching the filesystem
/// (`path.resolve` semantics): a `..` above the root is dropped, and a
/// relative path keeps its leading `..` components.
pub(crate) fn normalize_lexically(path: &Path) -> PathBuf {
	let mut normalized = PathBuf::new();
	for component in path.components() {
		match component {
			Component::CurDir => {},
			Component::ParentDir => {
				let parent_is_normal = normalized
					.components()
					.next_back()
					.is_some_and(|last| matches!(last, Component::Normal(_)));
				if parent_is_normal {
					normalized.pop();
				} else if !normalized.has_root() {
					normalized.push("..");
				}
			},
			other => normalized.push(other),
		}
	}
	normalized
}

/// Keep YAML diagnostics useful without echoing file contents (which may hold
/// credentials). The in-tree parser only quotes payloads in `unterminated flow
/// sequence`, but strip every backticked span defensively.
pub(crate) fn sanitize_yaml_error(message: &str) -> String {
	let mut out = String::with_capacity(message.len());
	let mut in_span = false;
	for ch in message.chars() {
		if ch == '`' {
			in_span = !in_span;
			continue;
		}
		if !in_span {
			out.push(ch);
		}
	}
	let trimmed = out.trim().trim_end_matches(':').trim().to_string();
	if trimmed.is_empty() { "invalid YAML".to_string() } else { trimmed }
}

pub(crate) fn json_to_value(json: &serde_json::Value) -> Value {
	match json {
		serde_json::Value::Null => Value::Null,
		serde_json::Value::Bool(value) => Value::Bool(*value),
		serde_json::Value::Number(number) => number
			.as_i64()
			.map_or_else(|| Value::Float(number.as_f64().unwrap_or_default()), Value::Int),
		serde_json::Value::String(value) => Value::Str(value.clone()),
		serde_json::Value::Array(items) => {
			Value::List(items.iter().map(json_to_value).collect())
		},
		serde_json::Value::Object(map) => Value::Map(
			map.iter()
				.map(|(key, value)| (key.clone(), json_to_value(value)))
				.collect(),
		),
	}
}

/// `~/.omp/agent/config.yml`, falling back to `.yaml`; `None` when neither
/// exists.
#[must_use]
pub fn base_path() -> Option<PathBuf> {
	let dir = dirs::agent_dir();
	dirs::MAIN_CONFIG_FILENAMES
		.iter()
		.map(|name| dir.join(name))
		.find(|path| path.is_file())
}

fn parse_providers(value: &Value, path: &Path) -> Result<BTreeMap<String, ProviderConfig>> {
	if matches!(value, Value::Null) {
		return Ok(BTreeMap::new());
	}
	let Some(pairs) = value.as_map() else {
		return Err(invalid(path, "providers", "expected a mapping"));
	};
	let mut providers = BTreeMap::new();
	for (id, item) in pairs {
		if id.is_empty() {
			return Err(invalid(path, "providers", "empty provider id"));
		}
		providers.insert(id.clone(), parse_provider(item, id, path)?);
	}
	Ok(providers)
}

pub(crate) fn parse_provider(value: &Value, id: &str, path: &Path) -> Result<ProviderConfig> {
	if matches!(value, Value::Null) {
		return Ok(ProviderConfig::default());
	}
	let Some(pairs) = value.as_map() else {
		return Err(invalid(path, id, "expected a mapping"));
	};
	let mut provider = ProviderConfig::default();
	for (key, item) in pairs {
		match key.as_str() {
			"baseUrl" => {
				provider.base_url = expect_text(item, key, path)?;
				if provider.base_url.as_deref() == Some("") {
					return Err(invalid(path, key, "expected a non-empty string"));
				}
			},
			"apiKey" => {
				provider.api_key = expect_text(item, key, path)?;
				if provider.api_key.as_deref() == Some("") {
					return Err(invalid(path, key, "expected a non-empty string"));
				}
			},
			"api" => provider.api = expect_text(item, key, path)?,
			"auth" => {
				let raw = expect_text(item, key, path)?
					.ok_or_else(|| invalid(path, key, "expected one of apiKey, none, oauth"))?;
				if !AUTH_MODES.contains(&raw.as_str()) {
					return Err(invalid(path, key, "expected one of apiKey, none, oauth"));
				}
				provider.auth = Some(raw);
			},
			"headers" => {
				let Some(headers) = item.as_map() else {
					return Err(invalid(path, key, "expected a mapping of string headers"));
				};
				for (name, header) in headers {
					let text = header
						.to_text()
						.ok_or_else(|| invalid(path, key, "expected string header values"))?;
					provider.headers.insert(name.clone(), text);
				}
			},
			// Provider blocks are shared with `models.yml`, which carries many more
			// keys; ignore the rest instead of rejecting a valid catalog.
			_ => {},
		}
	}
	Ok(provider)
}

fn expect_text(value: &Value, key: &str, path: &Path) -> Result<Option<String>> {
	match value {
		Value::Null => Ok(None),
		other => other
			.to_text()
			.map(Some)
			.ok_or_else(|| invalid(path, key, "expected a string")),
	}
}

fn expect_bool(value: &Value, key: &str, path: &Path) -> Result<Option<bool>> {
	match value {
		Value::Null => Ok(None),
		Value::Bool(value) => Ok(Some(*value)),
		_ => Err(invalid(path, key, "expected a boolean")),
	}
}

fn expect_list_of_text(value: &Value, key: &str, path: &Path) -> Result<Vec<String>> {
	if matches!(value, Value::Null) {
		return Ok(Vec::new());
	}
	let Some(items) = value.as_list() else {
		return Err(invalid(path, key, "expected a sequence of strings"));
	};
	items
		.iter()
		.map(|item| {
			item
				.to_text()
				.ok_or_else(|| invalid(path, key, "expected a sequence of strings"))
		})
		.collect()
}

fn expect_model_patterns(value: &Value, agent: &str, path: &Path) -> Result<Vec<String>> {
	match value {
		Value::Str(text) => Ok(vec![text.clone()]),
		Value::List(items) => items
			.iter()
			.map(|item| {
				item.to_text().ok_or_else(|| {
					invalid(
						path,
						&format!("task.agentModelOverrides.{agent}"),
						"expected a string or sequence of strings",
					)
				})
			})
			.collect(),
		_ => Err(invalid(
			path,
			&format!("task.agentModelOverrides.{agent}"),
			"expected a string or sequence of strings",
		)),
	}
}

fn expect_i32(value: &Value, key: &str, path: &Path) -> Result<Option<i32>> {
	match value {
		Value::Null => Ok(None),
		Value::Int(value) => i32::try_from(*value)
			.map(Some)
			.map_err(|_| invalid(path, key, "expected a 32-bit integer")),
		_ => Err(invalid(path, key, "expected an integer")),
	}
}

fn expect_usize(value: &Value, key: &str, path: &Path) -> Result<Option<usize>> {
	match value {
		Value::Null => Ok(None),
		Value::Int(value) => usize::try_from(*value)
			.map(Some)
			.map_err(|_| invalid(path, key, "expected a non-negative integer")),
		_ => Err(invalid(path, key, "expected an integer")),
	}
}

/// `*.thresholdMs`: the schema type is `number`, so integers and finite floats
/// both load. Upstream floors the value and treats `<= 0` as "background
/// immediately"; negative values clamp to `0` for the same behaviour.
fn expect_millis(value: &Value, key: &str, path: &Path) -> Result<Option<u64>> {
	let millis = match value {
		Value::Null => return Ok(None),
		Value::Int(value) => *value as f64,
		Value::Float(value) if value.is_finite() => *value,
		Value::Float(_) => return Err(invalid(path, key, "expected a finite number")),
		_ => return Err(invalid(path, key, "expected a number")),
	};
	Ok(Some(millis.max(0.0).floor() as u64))
}

fn invalid(path: &Path, key: &str, expected: &str) -> Error {
	Error::new(format!("{}: key `{key}`: {expected}", path.display()))
}

/// Discovered MCP server configuration with its declaration source.
#[derive(Debug, Clone)]
pub struct DiscoveredMcpServer {
	pub name:        String,
	pub config:      MCPServerConfig,
	pub source_path: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct McpDiscoveryReport {
	pub servers:          Vec<DiscoveredMcpServer>,
	pub all_server_names: Vec<String>,
	pub warnings:         Vec<String>,
}

/// Validate server name against
/// packages/coding-agent/src/config/mcp-schema.json: ^[a-zA-Z0-9_.-]{1,100}$
#[must_use]
pub fn is_valid_mcp_server_name(name: &str) -> bool {
	if name.is_empty() || name.len() > 100 {
		return false;
	}
	name
		.chars()
		.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
}

/// Expand environment variables in string matching upstream syntax:
/// `${VAR}` and `${VAR:-default}`
pub fn expand_env_vars(value: &str) -> String {
	let mut result = String::with_capacity(value.len());
	let mut chars = value.chars().peekable();

	while let Some(c) = chars.next() {
		if c == '$' && chars.peek() == Some(&'{') {
			chars.next(); // consume '{'
			let mut inner = String::new();
			let mut closed = false;
			while let Some(ic) = chars.next() {
				if ic == '}' {
					closed = true;
					break;
				}
				inner.push(ic);
			}

			if closed {
				let (var_name, default_val) = match inner.split_once(":-") {
					Some((name, def)) => (name, Some(def)),
					None => (inner.as_str(), None),
				};
				if let Ok(val) = env::var(var_name) {
					result.push_str(&val);
				} else if let Some(def) = default_val {
					result.push_str(def);
				} else {
					result.push_str("${");
					result.push_str(&inner);
					result.push('}');
				}
			} else {
				result.push_str("${");
				result.push_str(&inner);
			}
		} else {
			result.push(c);
		}
	}

	result
}

/// Recursively expand environment variables in a JSON value.
pub fn expand_env_vars_deep(val: &serde_json::Value) -> serde_json::Value {
	match val {
		serde_json::Value::String(s) => serde_json::Value::String(expand_env_vars(s)),
		serde_json::Value::Array(arr) => {
			serde_json::Value::Array(arr.iter().map(expand_env_vars_deep).collect())
		},
		serde_json::Value::Object(map) => {
			let mut new_map = serde_json::Map::new();
			for (k, v) in map {
				new_map.insert(k.clone(), expand_env_vars_deep(v));
			}
			serde_json::Value::Object(new_map)
		},
		other => other.clone(),
	}
}

/// Validate enabled value: coerce boolean, coerce string
/// "true"/"false"/"1"/"0", warn on other types. Returns (normalized_enabled,
/// optional_warning).
pub fn parse_enabled_value(val: Option<&serde_json::Value>) -> (Option<bool>, Option<String>) {
	match val {
		None | Some(serde_json::Value::Null) => (None, None),
		Some(serde_json::Value::Bool(b)) => (Some(*b), None),
		Some(serde_json::Value::String(s)) => {
			let lower = s.trim().to_ascii_lowercase();
			if lower == "false" || lower == "0" {
				(Some(false), None)
			} else if lower == "true" || lower == "1" {
				(Some(true), None)
			} else {
				(None, Some(format!("invalid enabled value \"{s}\", ignoring")))
			}
		},
		Some(other) => (None, Some(format!("invalid enabled type {other}, ignoring"))),
	}
}

/// Validate raw JSON value against MCP server schema and construct
/// `MCPServerConfig`.
pub fn parse_and_validate_mcp_server(
	name: &str,
	val: &serde_json::Value,
) -> std::result::Result<MCPServerConfig, String> {
	let expanded_val = expand_env_vars_deep(val);
	let obj = expanded_val
		.as_object()
		.ok_or_else(|| format!("Server \"{name}\": configuration must be a JSON object"))?;

	let has_command = obj.contains_key("command");
	let has_url = obj.contains_key("url");

	if has_command && has_url {
		return Err(format!("Server \"{name}\": cannot specify both \"command\" and \"url\""));
	}
	if !has_command && !has_url {
		return Err(format!("Server \"{name}\": requires \"command\" (stdio) or \"url\" (http/sse)"));
	}

	let transport_type = match obj.get("type").and_then(|v| v.as_str()) {
		Some("stdio") => {
			if !has_command {
				return Err(format!("Server \"{name}\": stdio server requires \"command\" field"));
			}
			"stdio"
		},
		Some("http") => {
			if !has_url {
				return Err(format!("Server \"{name}\": http server requires \"url\" field"));
			}
			"http"
		},
		Some("sse") => {
			if !has_url {
				return Err(format!("Server \"{name}\": sse server requires \"url\" field"));
			}
			"sse"
		},
		Some(other) => {
			return Err(format!(
				"Server \"{name}\": invalid transport type \"{other}\", expected stdio, http, or sse"
			));
		},
		None => {
			if has_command {
				"stdio"
			} else {
				"http"
			}
		},
	};

	let timeout = match obj.get("timeout") {
		Some(serde_json::Value::Number(num)) => {
			if let Some(n) = num.as_u64() {
				Some(n)
			} else if let Some(f) = num.as_f64() {
				if !f.is_finite() || f < 0.0 {
					return Err(format!(
						"Server \"{name}\": timeout must be a finite non-negative number"
					));
				}
				Some(f as u64)
			} else {
				return Err(format!("Server \"{name}\": invalid timeout value"));
			}
		},
		Some(serde_json::Value::String(s)) => {
			if let Ok(n) = s.trim().parse::<u64>() {
				Some(n)
			} else if let Ok(f) = s.trim().parse::<f64>() {
				if !f.is_finite() || f < 0.0 {
					return Err(format!(
						"Server \"{name}\": timeout must be a finite non-negative number"
					));
				}
				Some(f as u64)
			} else {
				return Err(format!("Server \"{name}\": invalid timeout value \"{s}\""));
			}
		},
		Some(serde_json::Value::Null) | None => None,
		Some(_) => return Err(format!("Server \"{name}\": timeout must be a number")),
	};

	let request_id_format = match obj.get("requestIdFormat").and_then(|v| v.as_str()) {
		Some("number") => Some(RequestIdFormat::Number),
		Some("string") => Some(RequestIdFormat::String),
		Some(other) => {
			return Err(format!(
				"Server \"{name}\": invalid requestIdFormat \"{other}\", expected number or string"
			));
		},
		None => None,
	};

	let config = match transport_type {
		"stdio" => {
			let command = obj
				.get("command")
				.and_then(|v| v.as_str())
				.ok_or_else(|| format!("Server \"{name}\": stdio server requires \"command\" field"))?;
			if command.trim().is_empty() {
				return Err(format!("Server \"{name}\": stdio server requires \"command\" field"));
			}

			let args = match obj.get("args") {
				Some(serde_json::Value::Array(arr)) => {
					let mut res = Vec::new();
					for (i, item) in arr.iter().enumerate() {
						let s = item
							.as_str()
							.ok_or_else(|| format!("Server \"{name}\": args[{i}] must be a string"))?;
						res.push(s.to_string());
					}
					res
				},
				Some(serde_json::Value::Null) | None => Vec::new(),
				Some(_) => return Err(format!("Server \"{name}\": args must be an array of strings")),
			};

			let env = match obj.get("env") {
				Some(serde_json::Value::Object(map)) => {
					let mut res = HashMap::new();
					for (k, v) in map {
						let s = v
							.as_str()
							.ok_or_else(|| format!("Server \"{name}\": env[\"{k}\"] must be a string"))?;
						res.insert(k.clone(), s.to_string());
					}
					res
				},
				Some(serde_json::Value::Null) | None => HashMap::new(),
				Some(_) => return Err(format!("Server \"{name}\": env must be a string map")),
			};

			let cwd = match obj.get("cwd") {
				Some(serde_json::Value::String(s)) => Some(s.clone()),
				Some(serde_json::Value::Null) | None => None,
				Some(_) => return Err(format!("Server \"{name}\": cwd must be a string")),
			};

			MCPServerConfig::Stdio(MCPStdioServerConfig {
				command: command.to_string(),
				args,
				env,
				cwd,
				timeout,
				request_id_format,
			})
		},
		"http" | "sse" => {
			let url = obj.get("url").and_then(|v| v.as_str()).ok_or_else(|| {
				format!("Server \"{name}\": {transport_type} server requires \"url\" field")
			})?;
			if url.trim().is_empty() {
				return Err(format!(
					"Server \"{name}\": {transport_type} server requires \"url\" field"
				));
			}

			let headers = match obj.get("headers") {
				Some(serde_json::Value::Object(map)) => {
					let mut res = HashMap::new();
					for (k, v) in map {
						let s = v.as_str().ok_or_else(|| {
							format!("Server \"{name}\": headers[\"{k}\"] must be a string")
						})?;
						res.insert(k.clone(), s.to_string());
					}
					res
				},
				Some(serde_json::Value::Null) | None => HashMap::new(),
				Some(_) => return Err(format!("Server \"{name}\": headers must be a string map")),
			};

			let auth = obj.get("auth").cloned();
			let oauth = obj.get("oauth").cloned();

			if transport_type == "http" {
				MCPServerConfig::Http(MCPHttpServerConfig {
					url: url.to_string(),
					headers,
					timeout,
					request_id_format,
					header_policy: None,
					auth,
					oauth,
				})
			} else {
				MCPServerConfig::Sse(MCPSseServerConfig {
					url: url.to_string(),
					headers,
					timeout,
					request_id_format,
					header_policy: None,
					auth,
					oauth,
				})
			}
		},
		_ => unreachable!(),
	};

	validate_server_config(name, &config).map_err(|errs| errs.join("; "))?;
	Ok(config)
}

/// Discover MCP servers following discovery priority:
/// 1. <cwd>/.omp/mcp.json
/// 2. <cwd>/.omp/.mcp.json
/// 3. <agent_dir>/mcp.json
/// 4. <agent_dir>/.mcp.json
/// 5. <cwd>/mcp.json
/// 6. <cwd>/.mcp.json
#[must_use]
pub fn discover_mcp_servers_with_paths(cwd: &Path, agent_dir: &Path) -> McpDiscoveryReport {
	let candidate_paths = [
		cwd.join(".omp").join("mcp.json"),
		cwd.join(".omp").join(".mcp.json"),
		agent_dir.join("mcp.json"),
		agent_dir.join(".mcp.json"),
		cwd.join("mcp.json"),
		cwd.join(".mcp.json"),
	];

	let mut report = McpDiscoveryReport::default();
	let mut seen_paths = HashSet::new();

	// Step 1: Collect user-level disabledServers and enabledServers ONLY from
	// agent_dir configs
	let mut disabled_servers = HashSet::new();
	let mut forced_enabled_servers = HashSet::new();
	let user_candidate_paths = [agent_dir.join("mcp.json"), agent_dir.join(".mcp.json")];

	for path in &user_candidate_paths {
		if !path.is_file() {
			continue;
		}
		if let Ok(content) = fs::read_to_string(path) {
			if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
				if let Some(arr) = val.get("disabledServers").and_then(|v| v.as_array()) {
					for item in arr {
						if let Some(s) = item.as_str() {
							disabled_servers.insert(s.to_string());
						}
					}
				}
				if let Some(arr) = val.get("enabledServers").and_then(|v| v.as_array()) {
					for item in arr {
						if let Some(s) = item.as_str() {
							forced_enabled_servers.insert(s.to_string());
						}
					}
				}
			}
		}
	}

	// Step 2: Iterate paths in priority order with first-wins key reservation
	let mut seen_servers = HashSet::new();

	for path in &candidate_paths {
		let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.clone());
		if !seen_paths.insert(canonical) {
			continue;
		}
		if !path.is_file() {
			continue;
		}

		let content = match fs::read_to_string(path) {
			Ok(c) => c,
			Err(err) => {
				report
					.warnings
					.push(format!("Failed to read {}: {err}", path.display()));
				continue;
			},
		};

		let val: serde_json::Value = match serde_json::from_str(&content) {
			Ok(v) => v,
			Err(err) => {
				report
					.warnings
					.push(format!("Failed to parse JSON in {}: {err}", path.display()));
				continue;
			},
		};

		let val = expand_env_vars_deep(&val);
		let Some(servers_obj) = val.get("mcpServers").and_then(|v| v.as_object()) else {
			continue;
		};

		for (server_name, server_val) in servers_obj {
			if seen_servers.contains(server_name) {
				// Shadowed by higher-priority configuration
				continue;
			}
			seen_servers.insert(server_name.clone());

			if !is_valid_mcp_server_name(server_name) {
				report.warnings.push(format!(
					"Server \"{server_name}\" in {}: invalid server name",
					path.display()
				));
				continue;
			}

			let (enabled_opt, enabled_warn) = parse_enabled_value(server_val.get("enabled"));
			if let Some(warn) = enabled_warn {
				report
					.warnings
					.push(format!("MCP server \"{server_name}\": {warn}"));
			}

			let is_explicitly_disabled = enabled_opt == Some(false);
			let is_denylisted = disabled_servers.contains(server_name);
			let is_forced_enabled = forced_enabled_servers.contains(server_name);

			if is_denylisted || (is_explicitly_disabled && !is_forced_enabled) {
				// Suppressed/disabled: shadows lower priority, but is not spawned
				continue;
			}

			match parse_and_validate_mcp_server(server_name, server_val) {
				Ok(config) => {
					report.servers.push(DiscoveredMcpServer {
						name: server_name.clone(),
						config,
						source_path: path.clone(),
					});
				},
				Err(err) => {
					report
						.warnings
						.push(format!("Server \"{server_name}\" in {}: {err}", path.display()));
				},
			}
		}
	}
	let mut all_names: Vec<String> = seen_servers.into_iter().collect();
	all_names.sort();
	report.all_server_names = all_names;


	report
}

pub fn discover_mcp_servers(cwd: &Path) -> Result<Vec<DiscoveredMcpServer>> {
	let agent_dir = dirs::agent_dir();
	let report = discover_mcp_servers_with_paths(cwd, &agent_dir);
	for warning in &report.warnings {
		eprintln!("{warning}");
	}
	Ok(report.servers)
}
#[cfg(test)]
mod tests {
	use super::*;

	fn parse_config(text: &str) -> Result<Config> {
		Config::from_value(&yaml::parse(text).expect("yaml"), Path::new("test.yml"))
	}

	#[test]
	fn ask_settings_parse_seconds_and_migrate_legacy_milliseconds() {
		let config = parse_config("ask:\n  enabled: false\n  timeout: 15\n").expect("ask");
		assert!(!config.ask.enabled());
		assert_eq!(config.ask.timeout(), Some(std::time::Duration::from_secs(15)));

		let legacy = parse_config("ask:\n  timeout: 1500\n").expect("legacy timeout");
		assert_eq!(legacy.ask.timeout(), Some(std::time::Duration::from_secs(2)));

		let disabled = parse_config("ask:\n  timeout: 0\n").expect("disabled");
		assert_eq!(disabled.ask.timeout_ms, Some(0));
		assert_eq!(disabled.ask.timeout(), None);
		assert!(disabled.ask.enabled());

		let boundary = parse_config("ask:\n  timeout: 1000\n").expect("boundary");
		assert_eq!(boundary.ask.timeout(), Some(std::time::Duration::from_secs(1000)));

		assert!(parse_config("ask:\n  timeout: -1\n").is_err());
		assert!(parse_config("ask:\n  timeout: soon\n").is_err());
		assert!(parse_config("ask:\n  enabled: maybe\n").is_err());

		let mut base = parse_config("ask:\n  timeout: 30\n").expect("base");
		let overlay = parse_config("ask:\n  timeout: 0\n").expect("overlay");
		base.merge(overlay);
		assert_eq!(base.ask.timeout(), None, "explicit 0 must disable a lower layer");
	}

	#[test]
	fn later_overlay_wins_per_scalar_and_per_provider_field() {
		let mut base = parse_config(
			"defaultModel: a\nproviders:\n  openai:\n    baseUrl: https://one\n    apiKey: \
			 ONE_KEY\nallow: [read]\n",
		)
		.expect("base");
		let overlay = parse_config(
			"defaultModel: b\nproviders:\n  openai:\n    baseUrl: https://two\n    headers:\n      \
			 X-Test: '1'\nallow: [bash, edit]\n",
		)
		.expect("overlay");
		base.merge(overlay);
		assert_eq!(base.default_model.as_deref(), Some("b"));
		let openai = base.providers.get("openai").expect("provider");
		assert_eq!(openai.base_url.as_deref(), Some("https://two"));
		assert_eq!(openai.api_key.as_deref(), Some("ONE_KEY"));
		assert_eq!(openai.headers.get("X-Test").map(String::as_str), Some("1"));
		assert_eq!(base.allow, ["bash", "edit"]);
	}

	#[test]
	fn unknown_root_keys_are_ignored() {
		// The config file is shared with the full TS app; extra keys
		// (setupVersion etc.) must not break the headless binary.
		let config = parse_config("setupVersion: 3\ndefaultModel: m\n").expect("parse");
		assert_eq!(config.default_model.as_deref(), Some("m"));
	}

	#[test]
	fn disabled_providers_parse_plain_and_path_scoped_entries() {
		let config = parse_config(
			"disabledProviders:\n  - openai\n  - path: /work/project\n    providers: [local, acme]\n  - paths:\n      - /work\n    values:\n      - ollama\n  - providers: [ignored]\n",
		)
		.expect("disabledProviders");
		assert_eq!(
			config.disabled_providers(Path::new("/work/project/sub")),
			["openai", "local", "acme", "ollama"]
		);
		assert_eq!(config.disabled_providers(Path::new("/elsewhere")), ["openai"]);

		// Later layers replace the array, like every other settings scalar.
		let mut base = parse_config("disabledProviders: [openai]\n").expect("base");
		base.merge(parse_config("disabledProviders: [local]\n").expect("overlay"));
		assert_eq!(base.disabled_providers(Path::new("/elsewhere")), ["local"]);

		assert!(parse_config("disabledProviders: openai\n").is_err());
	}

	#[test]
	fn disabled_provider_scope_normalizes_dot_and_parent_components() {
		let config = parse_config(
			"disabledProviders:\n  - path: /work/project\n    providers: [acme]\n",
		)
		.expect("scoped");
		// cwd-side `..`/`.` collapse before the prefix check.
		assert_eq!(
			config.disabled_providers(Path::new("/work/../work/project/sub")),
			["acme"]
		);
		assert_eq!(config.disabled_providers(Path::new("/work/./project")), ["acme"]);
		// Escaping the prefix must not disable anything.
		assert!(config.disabled_providers(Path::new("/work/project/../other")).is_empty());
		assert!(config.disabled_providers(Path::new("/work/project/..")).is_empty());

		// Prefix-side `..` collapses too.
		let config = parse_config(
			"disabledProviders:\n  - path: /work/../project\n    providers: [beta]\n",
		)
		.expect("scoped");
		assert_eq!(config.disabled_providers(Path::new("/project/sub")), ["beta"]);
		assert!(config.disabled_providers(Path::new("/project/../other")).is_empty());
	}

	#[test]
	fn missing_overlay_is_a_hard_error() {
		let _env = crate::TEST_ENV_LOCK.lock().expect("env lock");
		// Isolate the base config too: a developer `~/.omp/agent/config.yml` or a
		// shell-exported override must not change this test's inputs.
		let empty = std::env::temp_dir().join(format!("omp-cli-config-{}", std::process::id()));
		let previous_agent = env::var_os("PI_CODING_AGENT_DIR");
		let previous_config = env::var_os("PI_CONFIG_DIR");
		// SAFETY: guarded by TEST_ENV_LOCK, like the other env-mutating tests.
		unsafe {
			env::set_var("PI_CONFIG_DIR", &empty);
			env::set_var("PI_CODING_AGENT_DIR", empty.join("agent"));
		}
		let result = Config::load(&[PathBuf::from("/nonexistent/omp-config.yml")]);
		unsafe {
			match previous_agent {
				Some(value) => env::set_var("PI_CODING_AGENT_DIR", value),
				None => env::remove_var("PI_CODING_AGENT_DIR"),
			}
			match previous_config {
				Some(value) => env::set_var("PI_CONFIG_DIR", value),
				None => env::remove_var("PI_CONFIG_DIR"),
			}
		}
		let err = result.err().expect("missing overlay must fail");
		assert!(err.to_string().contains("config file not found"), "error was {err}");
	}

	#[test]
	fn invalid_approval_mode_and_auth_are_rejected() {
		assert!(parse_config("approvalMode: sometimes\n").is_err());
		assert!(parse_config("providers:\n  p:\n    auth: magic\n").is_err());
	}

	#[test]
	fn lsp_debug_checkpoint_settings_default_and_overlay() {
		let default = parse_config("").expect("default");
		assert_eq!(default.lsp.enabled, None);
		assert_eq!(default.lsp.shared, None);
		assert_eq!(default.debug.enabled, None);
		assert_eq!(default.checkpoint.enabled, None);
		assert!(default.lsp.enabled() && default.lsp.shared());
		assert!(default.debug.enabled());
		assert!(!default.checkpoint.enabled());

		// Explicit `false` reaches the accessor and wins over a lower layer's
		// `true`, one group at a time.
		let explicit = parse_config(
			"lsp:\n  enabled: false\n  shared: false\ndebug:\n  enabled: false\ncheckpoint:\n  enabled: \
			 true\n",
		)
		.expect("explicit");
		assert_eq!(explicit.lsp.enabled, Some(false));
		assert_eq!(explicit.lsp.shared, Some(false));
		assert_eq!(explicit.debug.enabled, Some(false));
		assert_eq!(explicit.checkpoint.enabled, Some(true));

		let mut layered = parse_config(
			"lsp:\n  enabled: true\n  shared: true\ndebug:\n  enabled: true\ncheckpoint:\n  enabled: \
			 false\n",
		)
		.expect("base");
		layered.merge(
			parse_config(
				"lsp:\n  enabled: false\n  shared: false\ndebug:\n  enabled: false\ncheckpoint:\n  \
				 enabled: true\n",
			)
			.expect("overlay"),
		);
		assert!(!layered.lsp.enabled() && !layered.lsp.shared());
		assert!(!layered.debug.enabled());
		assert!(layered.checkpoint.enabled());

		// Absent keys in a higher layer preserve the lower layer.
		layered.merge(parse_config("lsp:\n  enabled: true\n").expect("partial overlay"));
		assert!(layered.lsp.enabled());
		assert!(!layered.lsp.shared(), "absent key must keep the lower layer");

		// Quoted dotted schema paths parse too, and the nested form wins.
		let dotted = parse_config(
			"\"lsp.enabled\": false\n\"lsp.shared\": false\n\
			 \"debug.enabled\": false\n\"checkpoint.enabled\": true\n",
		)
		.expect("dotted");
		assert_eq!(dotted.lsp.enabled, Some(false));
		assert_eq!(dotted.lsp.shared, Some(false));
		assert_eq!(dotted.debug.enabled, Some(false));
		assert_eq!(dotted.checkpoint.enabled, Some(true));

		let both = parse_config("lsp:\n  enabled: true\n\"lsp.enabled\": false\n").expect("both");
		assert_eq!(both.lsp.enabled, Some(true), "nested wins over dotted");

		// A non-object group is dropped whole like the upstream group-shadow
		// guard: its leaves fall back to defaults, and the dotted form is not
		// consulted once the group key exists.
		let shadow = parse_config("lsp: false\n").expect("group shadow");
		assert_eq!(shadow.lsp.enabled, None);
		assert!(shadow.lsp.enabled() && shadow.lsp.shared());
		let shadow_dotted =
			parse_config("lsp: false\n\"lsp.enabled\": false\n").expect("group shadow dotted");
		assert_eq!(shadow_dotted.lsp.enabled, None);

		// Wrong scalar types fail at the trust boundary with the file path.
		for text in [
			"lsp:\n  enabled: yes\n",
			"lsp:\n  shared: always\n",
			"debug:\n  enabled: 1\n",
			"checkpoint:\n  enabled: off\n",
		] {
			let err = parse_config(text).expect_err(text);
			assert!(err.to_string().contains("test.yml"), "{err}");
		}
	}

	#[test]
	fn eval_and_max_timeout_settings_parse_with_layer_precedence() {
		let config = parse_config(
			"eval:\n  py: false\n  js: true\n  rb: true\n  jl: false\n\
			 python:\n  kernelMode: per-call\n  interpreter: /opt/py\n\
			 ruby:\n  interpreter: /opt/rb\n\
			 tools:\n  maxTimeout: 45\n",
		)
		.expect("eval settings");
		assert_eq!(config.eval_config.python, Some(false));
		assert_eq!(config.eval_config.js, Some(true));
		assert_eq!(config.eval_config.ruby, Some(true));
		assert_eq!(config.eval_config.julia, Some(false));
		assert_eq!(
			config.eval_config.python_kernel_mode,
			Some(pi_agent::eval::KernelMode::PerCall)
		);
		assert_eq!(config.eval_config.python_interpreter.as_deref(), Some("/opt/py"));
		assert_eq!(config.eval_config.ruby_interpreter.as_deref(), Some("/opt/rb"));
		assert_eq!(config.max_tool_timeout_secs, Some(45));

		// Empty interpreter means auto-detect, never a literal empty program.
		let empty = parse_config("python:\n  interpreter: \"\"\n").expect("empty interpreter");
		assert_eq!(empty.eval_config.python_interpreter, None);

		// Dotted schema paths work, nested wins, and a bad enum fails the load.
		let dotted = parse_config("\"eval.py\": false\n\"python.kernelMode\": session\n")
			.expect("dotted eval");
		assert_eq!(dotted.eval_config.python, Some(false));
		assert_eq!(
			dotted.eval_config.python_kernel_mode,
			Some(pi_agent::eval::KernelMode::Session)
		);
		let both = parse_config("eval:\n  py: true\n\"eval.py\": false\n").expect("both");
		assert_eq!(both.eval_config.python, Some(true), "nested wins over dotted");
		assert!(parse_config("python:\n  kernelMode: sometimes\n").is_err());
		assert!(parse_config("eval:\n  py: yes\n").is_err());

		// Higher layer false/true replaces the lower value; absent keys persist.
		let mut layered = parse_config("eval:\n  py: false\n  js: false\n").expect("base");
		layered.merge(parse_config("eval:\n  py: true\n").expect("overlay"));
		assert_eq!(layered.eval_config.python, Some(true));
		assert_eq!(layered.eval_config.js, Some(false));
	}

	#[test]
	fn eval_auto_background_settings_honor_defaults_precedence_and_boundaries() {
		// Upstream defaults: disabled, 60s threshold; neither is materialized
		// until configured so layers can still override independently.
		let default = parse_config("").expect("default");
		assert_eq!(default.eval_config.auto_background_enabled, None);
		assert_eq!(default.eval_config.auto_background_threshold_ms, None);

		let nested = parse_config(
			"eval:\n  autoBackground:\n    enabled: true\n    thresholdMs: 250\n",
		)
		.expect("nested auto-background");
		assert_eq!(nested.eval_config.auto_background_enabled, Some(true));
		assert_eq!(nested.eval_config.auto_background_threshold_ms, Some(250));

		// Dotted schema paths work, and the nested group wins, like every other
		// settings leaf.
		let dotted = parse_config(
			"\"eval.autoBackground.enabled\": true\n\"eval.autoBackground.thresholdMs\": 900\n",
		)
		.expect("dotted auto-background");
		assert_eq!(dotted.eval_config.auto_background_enabled, Some(true));
		assert_eq!(dotted.eval_config.auto_background_threshold_ms, Some(900));
		let both = parse_config(
			"eval:\n  autoBackground:\n    enabled: false\n\
			 \"eval.autoBackground.enabled\": true\n",
		)
		.expect("both");
		assert_eq!(
			both.eval_config.auto_background_enabled,
			Some(false),
			"nested wins over dotted"
		);

		// `number` accepts floats and clamps non-positive values to immediate
		// background, matching `resolveAutoBackgroundWaitMs`.
		let boundaries = parse_config(
			"eval:\n  autoBackground:\n    thresholdMs: 12.9\n",
		)
		.expect("float threshold");
		assert_eq!(boundaries.eval_config.auto_background_threshold_ms, Some(12));
		let immediate = parse_config("eval:\n  autoBackground:\n    thresholdMs: -5\n")
			.expect("negative threshold");
		assert_eq!(immediate.eval_config.auto_background_threshold_ms, Some(0));
		let zero = parse_config("eval:\n  autoBackground:\n    thresholdMs: 0\n")
			.expect("zero threshold");
		assert_eq!(zero.eval_config.auto_background_threshold_ms, Some(0));

		// Wrong types fail at the trust boundary.
		for text in [
			"eval:\n  autoBackground:\n    enabled: yes\n",
			"eval:\n  autoBackground:\n    thresholdMs: soon\n",
		] {
			let error = parse_config(text).expect_err(text);
			assert!(error.to_string().contains("test.yml"), "{error}");
		}
		// A non-object subgroup shadows its dotted leaves, like the upstream
		// group-shadow guard.
		let shadow = parse_config(
			"eval:\n  autoBackground: false\n\"eval.autoBackground.enabled\": true\n",
		)
		.expect("subgroup shadow");
		assert_eq!(shadow.eval_config.auto_background_enabled, None);

		// Per-layer merge: a higher layer owns the threshold it sets and leaves
		// the other key inherited.
		let mut layered = parse_config(
			"eval:\n  autoBackground:\n    enabled: true\n    thresholdMs: 250\n",
		)
		.expect("base");
		layered.merge(parse_config("eval:\n  autoBackground:\n    thresholdMs: 500\n").expect("overlay"));
		assert_eq!(layered.eval_config.auto_background_enabled, Some(true));
		assert_eq!(layered.eval_config.auto_background_threshold_ms, Some(500));

		// Dotted `tools.maxTimeout` is the schema path form of the nested key.
		let dotted_timeout = parse_config("\"tools.maxTimeout\": 30\n").expect("dotted maxTimeout");
		assert_eq!(dotted_timeout.max_tool_timeout_secs, Some(30));
	}

	#[test]
	fn settings_json_maps_only_schema_paths() {
		let json = serde_json::json!({
			"modelRoles": { "default": "openai/gpt-5", "smol": "x" },
			"tools": { "approvalMode": "write", "approval": { "bash": "allow" } },
			"edit": { "enforceSeenLines": false },
			// Legacy root keys are not settings-schema paths; the full TS app
			// ignores them here and the headless binary must not invent support.
			"defaultModel": "legacy/ignored",
			"cwd": "/ignored",
			"allow": ["bash"],
		});
		let config = Config::from_settings_value(&json_to_value(&json), Path::new("settings.json"))
			.expect("settings");
		assert_eq!(config.default_model.as_deref(), Some("openai/gpt-5"));
		assert_eq!(config.approval_mode, Some(ApprovalMode::Write));
		assert_eq!(config.edit.enforce_seen_lines, Some(false));
		assert_eq!(config.cwd, None);
		assert!(config.allow.is_empty());
		assert!(config.tools.is_none());
	}

	#[test]
	fn edit_enforce_seen_lines_defaults_false_and_overrides() {
		let default = parse_config("").expect("default");
		assert_eq!(default.edit.enforce_seen_lines, None);
		assert!(!default.edit.enforce_seen_lines());

		let explicit_false =
			parse_config("edit:\n  enforceSeenLines: false\n").expect("explicit false");
		assert_eq!(explicit_false.edit.enforce_seen_lines, Some(false));
		assert!(!explicit_false.edit.enforce_seen_lines());

		let explicit_true = parse_config("edit:\n  enforceSeenLines: true\n").expect("explicit true");
		assert_eq!(explicit_true.edit.enforce_seen_lines, Some(true));
		assert!(explicit_true.edit.enforce_seen_lines());

		let mut layered = parse_config("edit:\n  enforceSeenLines: true\n").expect("base");
		layered.merge(parse_config("edit:\n  enforceSeenLines: false\n").expect("overlay"));
		assert!(!layered.edit.enforce_seen_lines());

		let mut layered_on = parse_config("edit:\n  enforceSeenLines: false\n").expect("base");
		layered_on.merge(parse_config("edit:\n  enforceSeenLines: true\n").expect("overlay"));
		assert!(layered_on.edit.enforce_seen_lines());
		assert!(parse_config("edit:\n  enforceSeenLines: nope\n").is_err());
	}

	#[test]
	fn edit_mode_variants_and_read_line_numbers_parse_and_overlay() {
		let default = parse_config("").expect("default");
		assert_eq!(default.edit.mode, None);
		assert_eq!(default.edit.mode(), EditMode::Hashline);
		assert_eq!(default.edit.model_variants, ModelVariants::Absent);
		assert_eq!(default.edit.variant_for_model("openai/gpt-5"), None);
		assert_eq!(default.read_line_numbers, None);
		assert!(!default.read_line_numbers());

		let explicit = parse_config(
			"edit:\n  mode: apply_patch\n  modelVariants:\n    GPT: replace\n    claude: \
			 patch\nreadLineNumbers: true\n",
		)
		.expect("explicit");
		assert_eq!(explicit.edit.mode(), EditMode::ApplyPatch);
		assert!(explicit.read_line_numbers());
		// Patterns match case-insensitively by substring, first entry wins.
		assert_eq!(explicit.edit.variant_for_model("openai/GPT-5"), Some(EditMode::Replace));
		assert_eq!(explicit.edit.variant_for_model("anthropic/claude-x"), Some(EditMode::Patch));
		assert_eq!(explicit.edit.variant_for_model("xai/grok"), None);

		// `atom`/`vim` migrate to `hashline` in the mode and each variant value;
		// quoted dotted `"edit.mode"` is the same leaf.
		let legacy = parse_config(
			"edit:\n  mode: vim\n  modelVariants:\n    atom-model: atom\n",
		)
		.expect("legacy");
		assert_eq!(legacy.edit.mode(), EditMode::Hashline);
		assert_eq!(legacy.edit.variant_for_model("atom-model-1"), Some(EditMode::Hashline));
		let dotted_mode = parse_config("\"edit.mode\": atom\n").expect("dotted mode");
		assert_eq!(dotted_mode.edit.mode(), EditMode::Hashline);

		// Overlay merge: same pattern overrides in place, new ones append, and an
		// absent field keeps the lower layer (explicit false still wins).
		let mut layered = parse_config(
			"edit:\n  mode: patch\n  modelVariants:\n    claude: replace\n    gpt: \
			 hashline\n",
		)
		.expect("base");
		layered.merge(
			parse_config("edit:\n  modelVariants:\n    gpt: sloppy\n    gemini: apply_patch\n")
				.expect("overlay"),
		);
		assert_eq!(layered.edit.mode(), EditMode::Patch, "absent mode keeps the lower layer");
		let ModelVariants::Entries(layered_entries) = &layered.edit.model_variants else {
			panic!("layered variants: {:?}", layered.edit.model_variants);
		};
		let patterns: Vec<&str> =
			layered_entries.iter().map(|(pattern, _)| pattern.as_str()).collect();
		assert_eq!(patterns, ["claude", "gpt", "gemini"]);
		assert_eq!(layered.edit.variant_for_model("claude-3"), Some(EditMode::Replace));
		assert_eq!(layered.edit.variant_for_model("gpt-4"), Some(EditMode::Sloppy));
		assert_eq!(layered.edit.variant_for_model("gemini-pro"), Some(EditMode::ApplyPatch));

		let mut lines = parse_config("readLineNumbers: true\n").expect("base lines");
		lines.merge(parse_config("readLineNumbers: false\n").expect("false overlay"));
		assert!(!lines.read_line_numbers(), "explicit false must override a lower layer");

		// Raw pattern keys merge case-sensitively (`GPT` and `gpt` stay distinct
		// entries) and case folds only at match time, so the lower layer's entry
		// keeps the first-match position like upstream `#deepMerge`.
		let mut cased =
			parse_config("edit:\n  modelVariants:\n    GPT: patch\n").expect("cased base");
		cased.merge(
			parse_config("edit:\n  modelVariants:\n    gpt: replace\n").expect("cased overlay"),
		);
		assert_eq!(
			cased.edit.model_variants,
			ModelVariants::Entries(vec![
				("GPT".to_string(), Some("patch".to_string())),
				("gpt".to_string(), Some("replace".to_string())),
			])
		);
		assert_eq!(cased.edit.variant_for_model("gpt-4"), Some(EditMode::Patch));

		// Untyped `modelVariants` values stay raw until matching (upstream
		// `#getEditVariantEntries`), so a value repaired by a later layer keeps
		// its position and the first matching pattern still wins.
		let mut repaired =
			parse_config("edit:\n  modelVariants:\n    m: bogus\n    gpt: patch\n")
				.expect("repaired base");
		repaired.merge(
			parse_config("edit:\n  modelVariants:\n    m: replace\n").expect("repaired overlay"),
		);
		assert_eq!(repaired.edit.variant_for_model("m-gpt-4"), Some(EditMode::Replace));
		let mut non_string =
			parse_config("edit:\n  modelVariants:\n    m: 1\n    gpt: patch\n").expect("raw base");
		non_string.merge(
			parse_config("edit:\n  modelVariants:\n    m: replace\n").expect("raw overlay"),
		);
		assert_eq!(non_string.edit.variant_for_model("m-gpt-4"), Some(EditMode::Replace));

		// Untyped entries and non-object groups yield no variant; a non-object
		// overlay replaces the inherited group, while an empty mapping keeps it
		// (upstream deep merge).
		let skipped = parse_config(
			"edit:\n  modelVariants:\n    prose: text\n    gpt: 1\n    claude-model: \
			 replace\n",
		)
		.expect("skipped");
		assert_eq!(
			skipped.edit.model_variants,
			ModelVariants::Entries(vec![
				("prose".to_string(), Some("text".to_string())),
				("gpt".to_string(), None),
				("claude-model".to_string(), Some("replace".to_string())),
			])
		);
		assert_eq!(skipped.edit.variant_for_model("claude-model-1"), Some(EditMode::Replace));
		let non_map = parse_config("edit:\n  modelVariants: true\n").expect("non map");
		assert_eq!(non_map.edit.model_variants, ModelVariants::NotAMap);
		assert_eq!(non_map.edit.variant_for_model("gpt-4"), None);

		let mut cleared =
			parse_config("edit:\n  modelVariants:\n    gpt: patch\n").expect("cleared base");
		cleared.merge(parse_config("edit:\n  modelVariants: null\n").expect("null overlay"));
		assert_eq!(cleared.edit.model_variants, ModelVariants::NotAMap);
		assert_eq!(cleared.edit.variant_for_model("gpt-4"), None);

		let mut kept =
			parse_config("edit:\n  modelVariants:\n    gpt: patch\n").expect("kept base");
		kept.merge(
			Config::from_settings_value(
				&json_to_value(&serde_json::json!({ "edit": { "modelVariants": {} } })),
				Path::new("overlay.json"),
			)
			.expect("empty overlay"),
		);
		assert_eq!(kept.edit.variant_for_model("gpt-4"), Some(EditMode::Patch));
		for text in [
			"edit:\n  mode: plain\n",
			"edit:\n  mode: 1\n",
			"\"edit.mode\": fast\n",
			"readLineNumbers: yes\n",
		] {
			let err = parse_config(text).expect_err(text);
			assert!(err.to_string().contains("test.yml"), "{err}");
		}

		// settings.json keeps JSON insertion order (serde_json `preserve_order`),
		// so the first declared pattern still wins.
		let json = serde_json::json!({
			"edit": {
				"mode": "sloppy",
				"modelVariants": { "gpt": "patch", "claude": "replace" },
			},
			"readLineNumbers": true,
		});
		let settings = Config::from_settings_value(&json_to_value(&json), Path::new("settings.json"))
			.expect("settings json");
		assert_eq!(settings.edit.mode(), EditMode::Sloppy);
		assert_eq!(settings.edit.variant_for_model("gpt-claude-mini"), Some(EditMode::Patch));
		assert!(settings.read_line_numbers());
	}

	#[test]
	fn async_enabled_defaults_true_and_explicit_false_wins() {
		let default = parse_config("").expect("default");
		assert_eq!(default.async_config.enabled, None);
		assert!(default.async_config.enabled());

		let explicit_false =
			parse_config("async:\n  enabled: false\n").expect("explicit false");
		assert_eq!(explicit_false.async_config.enabled, Some(false));
		assert!(!explicit_false.async_config.enabled());

		let explicit_true = parse_config("async:\n  enabled: true\n").expect("explicit true");
		assert!(explicit_true.async_config.enabled());

		let mut layered = parse_config("async:\n  enabled: true\n").expect("base");
		layered.merge(parse_config("async:\n  enabled: false\n").expect("overlay"));
		assert!(!layered.async_config.enabled());
		assert!(parse_config("async:\n  enabled: nope\n").is_err());
	}

	#[test]
	fn cli_flags_override_every_file_layer() {
		use clap::Parser;
		let mut config = parse_config(
			"defaultModel: file/model\nsystemPrompt: file-system\nappendSystemPrompt: \
			 file-append\napprovalMode: always-ask\nallow: [config-allow]\ntools: [read]\n",
		)
		.expect("config");
		let cli = crate::args::Cli::try_parse_from([
			"omp",
			"--tools",
			"bash,edit",
			"--approval-mode",
			"yolo",
			"--allow",
			"cli-allow",
			"--system-prompt",
			"cli-system",
			"--append-system-prompt",
			"cli-append",
		])
		.expect("cli");
		config.apply_runtime(&cli);
		assert_eq!(config.approval_mode, Some(ApprovalMode::Yolo));
		assert_eq!(config.tools.as_deref(), Some(["bash".to_string(), "edit".to_string()].as_slice()));
		assert_eq!(config.allow, ["cli-allow", "config-allow"]);
		assert_eq!(config.system_prompt.as_deref(), Some("cli-system"));
		assert_eq!(config.append_system_prompt.as_deref(), Some("cli-append"));
		// A flag does not overwrite unrelated file fields.
		assert_eq!(config.default_model.as_deref(), Some("file/model"));
	}

	#[test]
	fn api_key_resolves_env_then_literal() {
		let _env = crate::TEST_ENV_LOCK.lock().expect("env lock");
		// SAFETY: single-threaded unit test mutating its own process env.
		unsafe { env::set_var("OMP_CLI_TEST_KEY", "resolved") };
		let config = parse_config(
			"providers:\n  env:\n    apiKey: OMP_CLI_TEST_KEY\n  literal:\n    apiKey: sk-literal\n",
		)
		.expect("config");
		assert_eq!(config.resolve_api_key("env").as_deref(), Some("resolved"));
		assert_eq!(config.resolve_api_key("literal").as_deref(), Some("sk-literal"));
		assert_eq!(config.resolve_api_key("missing"), None);
	}
}
