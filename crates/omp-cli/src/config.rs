//! `config.yml` loading and merge.
//!
//! Only the runtime subset the headless binary consumes is modelled; unknown
//! root keys are ignored because the file is shared with the full TS app.

use std::{
	collections::{BTreeMap, HashMap, HashSet},
	env, fs,
	path::{Path, PathBuf},
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

/// `edit` settings group consumed by the headless runtime.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EditConfig {
	/// `edit.enforceSeenLines`; absent means [`Self::DEFAULT_ENFORCE_SEEN_LINES`]
	/// (the upstream settings-schema default).
	pub enforce_seen_lines: Option<bool>,
}

impl EditConfig {
	/// Seen-line guard default, matching the upstream settings schema. The
	/// runtime maps this onto `EditToolOptions { enforce_seen_lines, .. }`.
	pub const DEFAULT_ENFORCE_SEEN_LINES: bool = false;

	#[must_use]
	pub fn enforce_seen_lines(&self) -> bool {
		self.enforce_seen_lines.unwrap_or(Self::DEFAULT_ENFORCE_SEEN_LINES)
	}

	fn merge(&mut self, overlay: Self) {
		if overlay.enforce_seen_lines.is_some() {
			self.enforce_seen_lines = overlay.enforce_seen_lines;
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
	/// `modelRoles.default` and `tools.approvalMode`. Root-level legacy keys and
	/// other schema groups are intentionally ignored (they are either owned by
	/// the full TS app or unsupported by the headless runtime).
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

	/// Resolve a provider API key through its `config.yml` connection block.
	#[must_use]
	pub fn resolve_api_key(&self, provider: &str) -> Option<String> {
		self.providers.get(provider)?.resolve_api_key()
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
	if let Some(default) = value.get("modelRoles").and_then(|roles| roles.get("default")) {
		config.default_model = expect_text(default, "modelRoles.default", path)?;
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
	Ok(())
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
fn resolve_layer_path(cwd: &Path, raw: &Path) -> PathBuf {
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

/// Keep YAML diagnostics useful without echoing file contents (which may hold
/// credentials). The in-tree parser only quotes payloads in `unterminated flow
/// sequence`, but strip every backticked span defensively.
fn sanitize_yaml_error(message: &str) -> String {
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

fn json_to_value(json: &serde_json::Value) -> Value {
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
