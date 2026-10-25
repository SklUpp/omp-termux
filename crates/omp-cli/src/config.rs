//! `config.yml` loading and merge.
//!
//! Only the runtime subset the headless binary consumes is modelled; unknown
//! root keys are ignored because the file is shared with the full TS app.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

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
	pub api_key: Option<String>,
	pub api: Option<String>,
	pub auth: Option<String>,
	pub headers: BTreeMap<String, String>,
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
	pub default_model: Option<String>,
	pub providers: BTreeMap<String, ProviderConfig>,
	pub system_prompt: Option<String>,
	pub append_system_prompt: Option<String>,
	pub approval_mode: Option<ApprovalMode>,
	pub allow: Vec<String>,
	pub cwd: Option<PathBuf>,
	/// Paths that contributed, base first.
	pub sources: Vec<PathBuf>,
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
		if !overlay.allow.is_empty() {
			self.allow = overlay.allow;
		}
		for (id, provider) in overlay.providers {
			self.providers.entry(id).or_default().merge(provider);
		}
		self.sources.extend(overlay.sources);
	}

	/// Base config (agent dir `config.yml`, then `config.yaml`) followed by each
	/// `--config` overlay in order; later files win.
	pub fn load(overlays: &[PathBuf]) -> Result<Self> {
		let mut config = Self::default();
		if let Some(base) = base_path() {
			let overlay = Self::load_file(&base)?;
			config.merge(overlay);
		}
		for path in overlays {
			let overlay = Self::load_file(path)?;
			config.merge(overlay);
		}
		Ok(config)
	}

	fn load_file(path: &Path) -> Result<Self> {
		if !path.is_file() {
			return Err(Error::new(format!("config file not found: {}", path.display())));
		}
		let text = fs::read_to_string(path)
			.map_err(|err| Error::with_source(format!("cannot read {}", path.display()), err))?;
		let value =
			yaml::parse(&text).map_err(|err| Error::new(format!("{}: {err}", path.display())))?;
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
				"approvalMode" => {
					let raw = expect_text(item, key, path)?
						.ok_or_else(|| invalid(path, key, "expected one of always-ask, write, yolo"))?;
					let mode = ApprovalMode::parse(&raw)
						.ok_or_else(|| invalid(path, key, "expected one of always-ask, write, yolo"))?;
					config.approval_mode = Some(mode);
				},
				"allow" => config.allow = expect_list_of_text(item, key, path)?,
				"providers" => config.providers = parse_providers(item, path)?,
				// Unknown keys are ignored: the config file is shared with the
				// full TS app, which carries extra keys (setupVersion etc.).
				_ => {},
			}
		}
		config.sources.push(path.to_path_buf());
		Ok(config)
	}

	/// Resolve a provider API key through its `config.yml` connection block.
	#[must_use]
	pub fn resolve_api_key(&self, provider: &str) -> Option<String> {
		self.providers.get(provider)?.resolve_api_key()
	}
}

/// `~/.omp/agent/config.yml`, falling back to `.yaml`; `None` when neither exists.
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

#[cfg(test)]
mod tests {
	use super::*;

	fn parse_config(text: &str) -> Result<Config> {
		Config::from_value(&yaml::parse(text).expect("yaml"), Path::new("test.yml"))
	}

	#[test]
	fn later_overlay_wins_per_scalar_and_per_provider_field() {
		let mut base = parse_config(
			"defaultModel: a\nproviders:\n  openai:\n    baseUrl: https://one\n    apiKey: ONE_KEY\nallow: [read]\n",
		)
		.expect("base");
		let overlay = parse_config(
			"defaultModel: b\nproviders:\n  openai:\n    baseUrl: https://two\n    headers:\n      X-Test: '1'\nallow: [bash, edit]\n",
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
		assert!(Config::load(&[PathBuf::from("/nonexistent/omp-config.yml")]).is_err());
	}

	#[test]
	fn invalid_approval_mode_and_auth_are_rejected() {
		assert!(parse_config("approvalMode: sometimes\n").is_err());
		assert!(parse_config("providers:\n  p:\n    auth: magic\n").is_err());
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
