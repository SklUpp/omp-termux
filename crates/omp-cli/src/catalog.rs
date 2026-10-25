//! Model catalog: `models.json` (bundled snapshot) or `models.yml` overrides.
//!
//! Search order (`models.json`): `<cwd>/models.json`, `~/.omp/agent/models.json`,
//! `<share>/omp/models.json`. The bundled file is ~10 MB, so it is decoded
//! straight from a buffered reader into borrowed-free provider/model maps and
//! never materialised as one `String`.

use std::collections::BTreeMap;
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{Error, Result, dirs, yaml};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Catalog {
	/// provider id -> model id -> definition subset the runtime consumes.
	pub providers: BTreeMap<String, BTreeMap<String, ModelInfo>>,
	pub source: PathBuf,
}

/// Fields of one model definition the headless runtime needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
	/// Provider reference: `def.provider`, else the enclosing provider key.
	pub provider: String,
	/// `contextWindow` when the definition declares one.
	pub context_window: Option<usize>,
}

#[derive(Deserialize)]
struct ModelEntry {
	#[serde(default)]
	id: Option<String>,
	#[serde(default)]
	provider: Option<String>,
	#[serde(default, rename = "contextWindow")]
	context_window: Option<u64>,
}

impl Catalog {
	pub fn load(cwd: &Path) -> Result<Self> {
		let json_paths = json_candidates(cwd);
		let yml_path = dirs::agent_dir().join("models.yml");
		Self::load_from(&json_paths, &yml_path)
	}

	/// Testable loader: first existing `models.json` wins; otherwise `models.yml`.
	pub fn load_from(json_paths: &[PathBuf], yml_path: &Path) -> Result<Self> {
		if let Some(path) = json_paths.iter().find(|path| path.is_file()) {
			return Self::from_json(path);
		}
		if yml_path.is_file() {
			return Self::from_yml(yml_path);
		}
		Err(Error::new(
			"no models: expected models.json in the workspace, ~/.omp/agent, or share/omp \
			 (or providers in ~/.omp/agent/models.yml)",
		))
	}

	fn from_json(path: &Path) -> Result<Self> {
		let file = fs::File::open(path)
			.map_err(|err| Error::with_source(format!("cannot read {}", path.display()), err))?;
		let parsed: BTreeMap<String, BTreeMap<String, ModelEntry>> =
			serde_json::from_reader(BufReader::new(file))
				.map_err(|err| Error::new(format!("invalid models.json {}: {err}", path.display())))?;
		let mut providers = BTreeMap::new();
		for (provider_id, models) in parsed {
			let mut entries = BTreeMap::new();
			for (key, entry) in models {
				let id = entry.id.unwrap_or_else(|| key.clone());
				let provider = entry.provider.unwrap_or_else(|| provider_id.clone());
				let context_window = entry
					.context_window
					.and_then(|window| usize::try_from(window).ok())
					.filter(|window| *window > 0);
				entries.insert(id, ModelInfo { provider, context_window });
			}
			providers.insert(provider_id, entries);
		}
		if providers.is_empty() {
			return Err(Error::new(format!("no models in {}", path.display())));
		}
		Ok(Self { providers, source: path.to_path_buf() })
	}

	fn from_yml(path: &Path) -> Result<Self> {
		let text = fs::read_to_string(path)
			.map_err(|err| Error::with_source(format!("cannot read {}", path.display()), err))?;
		let value =
			yaml::parse(&text).map_err(|err| Error::new(format!("{}: {err}", path.display())))?;
		let mut providers = BTreeMap::new();
		if let Some(section) = value.get("providers") {
			let Some(pairs) = section.as_map() else {
				return Err(Error::new(format!("{}: `providers` must be a mapping", path.display())));
			};
			for (id, provider) in pairs {
				let mut models = BTreeMap::new();
				if let Some(list) = provider.get("models") {
					let Some(items) = list.as_list() else {
						return Err(Error::new(format!(
							"{}: providers.{id}.models must be a sequence",
							path.display()
						)));
					};
					for item in items {
						if let Some(model_id) = item.get("id").and_then(yaml::Value::as_str) {
							let context_window = item
								.get("contextWindow")
								.and_then(yaml::Value::to_text)
								.and_then(|window| window.parse::<usize>().ok())
								.filter(|window| *window > 0);
							models.insert(
								model_id.to_string(),
								ModelInfo { provider: id.clone(), context_window },
							);
						}
					}
				}
				providers.insert(id.clone(), models);
			}
		}
		if providers.is_empty() {
			return Err(Error::new(format!("no models in {}", path.display())));
		}
		Ok(Self { providers, source: path.to_path_buf() })
	}

	/// Provider that declares `model`, either `provider/model` or a bare id.
	#[must_use]
	pub fn provider_of(&self, model: &str) -> Option<&str> {
		if let Some((provider, id)) = model.split_once('/')
			&& self
				.providers
				.get(provider)
				.is_some_and(|models| models.contains_key(id))
		{
			return self
				.providers
				.get_key_value(provider)
				.map(|(key, _)| key.as_str());
		}
		self
			.providers
			.iter()
			.find(|(_, models)| models.contains_key(model))
			.map(|(provider, _)| provider.as_str())
	}

	/// Declared context window for a model, when the catalog knows it.
	#[must_use]
	pub fn context_window(&self, provider: &str, model: &str) -> Option<usize> {
		self.providers.get(provider)?.get(model)?.context_window
	}

	#[must_use]
	pub fn model_count(&self) -> usize {
		self.providers.values().map(BTreeMap::len).sum()
	}
}

fn json_candidates(cwd: &Path) -> Vec<PathBuf> {
	let mut paths = vec![cwd.join("models.json"), dirs::agent_dir().join("models.json")];
	paths.extend(
		dirs::share_dirs()
			.into_iter()
			.map(|dir| dir.join("models.json")),
	);
	paths
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::env;
	use std::io::Write as _;
	use std::sync::atomic::{AtomicU32, Ordering};

	static COUNTER: AtomicU32 = AtomicU32::new(0);

	fn temp_dir() -> PathBuf {
		let unique = format!(
			"omp-cli-catalog-{}-{}",
			std::process::id(),
			COUNTER.fetch_add(1, Ordering::Relaxed)
		);
		let dir = env::temp_dir().join(unique);
		fs::create_dir_all(&dir).expect("temp dir");
		dir
	}

	fn write(path: &Path, text: &str) {
		let mut file = fs::File::create(path).expect("create");
		file.write_all(text.as_bytes()).expect("write");
	}

	#[test]
	fn parses_json_and_prefers_workspace_file() {
		let dir = temp_dir();
		let workspace = dir.join("models.json");
		let user = dir.join("user-models.json");
		write(&user, r#"{"ignored":{"m1":{"id":"m1","provider":"ignored"}}}"#);
		write(
			&workspace,
			r#"{"openai":{"gpt-5":{"id":"gpt-5","provider":"openai","cost":{"input":1}}},"local":{"qwen":{"provider":"local"}}}"#,
		);
		let catalog =
			Catalog::load_from(&[workspace.clone(), user], &dir.join("models.yml")).expect("catalog");
		assert_eq!(catalog.source, workspace);
		assert_eq!(catalog.provider_of("gpt-5"), Some("openai"));
		assert_eq!(catalog.provider_of("openai/gpt-5"), Some("openai"));
		assert_eq!(catalog.provider_of("qwen"), Some("local"));
		assert_eq!(catalog.model_count(), 2);
		fs::remove_dir_all(dir).ok();
	}

	#[test]
	fn falls_back_to_models_yml_providers() {
		let dir = temp_dir();
		let yml = dir.join("models.yml");
		write(
			&yml,
			"providers:\n  ollama:\n    baseUrl: http://localhost:11434\n    models:\n      - id: llama3\n",
		);
		let catalog = Catalog::load_from(&[dir.join("missing.json")], &yml).expect("catalog");
		assert_eq!(catalog.provider_of("llama3"), Some("ollama"));
		fs::remove_dir_all(dir).ok();
	}

	#[test]
	fn missing_catalog_reports_no_models() {
		let dir = temp_dir();
		let err = Catalog::load_from(&[dir.join("missing.json")], &dir.join("missing.yml"))
			.expect_err("must fail");
		assert!(err.to_string().starts_with("no models"));
		fs::remove_dir_all(dir).ok();
	}
}
