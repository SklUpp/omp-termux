//! Language-server configuration: built-in defaults, JSON/YAML overrides,
//! precedence, project-local bin resolution, and server selection.
//!
//! Port of `packages/coding-agent/src/lsp/config.ts` and the routing part of
//! `servers.ts`. Configuration is resolve-only: this module never downloads or
//! installs a language server. An unresolvable binary simply removes the server
//! from the active set.

use std::{
	fs,
	path::{Path, PathBuf},
};

use serde_json::{Map, Value};

use super::types::{CustomLinter, LspConfig, ServerConfig, ServerCapabilities, WorkspaceReadyTimings};

const DEFAULTS_JSON: &str = include_str!("defaults.json");
const CONFIG_FILENAMES: &[&str] =
	&["lsp.json", ".lsp.json", "lsp.yaml", ".lsp.yaml", "lsp.yml", ".lsp.yml"];
const PID_TOKEN: &str = "$PID";

const PYTHON_ROOT_MARKERS: &[&str] = &[
	"pyproject.toml",
	"ty.toml",
	"requirements.txt",
	"setup.py",
	"setup.cfg",
	"Pipfile",
	"pyrightconfig.json",
	"ruff.toml",
	".ruff.toml",
];

const LOCAL_BIN_PATHS: &[(&[&str], &str)] = &[
	(&["package.json", "package-lock.json", "yarn.lock", "pnpm-lock.yaml"], "node_modules/.bin"),
	(PYTHON_ROOT_MARKERS, ".venv/bin"),
	(PYTHON_ROOT_MARKERS, ".venv/Scripts"),
	(PYTHON_ROOT_MARKERS, "venv/bin"),
	(PYTHON_ROOT_MARKERS, "venv/Scripts"),
	(PYTHON_ROOT_MARKERS, ".env/bin"),
	(PYTHON_ROOT_MARKERS, ".env/Scripts"),
	(&["Gemfile", "Gemfile.lock"], "vendor/bundle/bin"),
	(&["Gemfile", "Gemfile.lock"], "bin"),
	(&["go.mod", "go.sum", "go.work"], "bin"),
];

#[derive(Debug, Default)]
struct NormalizedConfig {
	servers: Map<String, Value>,
	idle_timeout_ms: Option<u64>,
}

// =============================================================================
// Loading
// =============================================================================

/// Load and merge LSP configuration for `cwd`.
///
/// Lowest to highest precedence: home root files, user config dirs, project
/// config dirs, project root files, then auto-detection when no readable source
/// contributed a server map. Within one location the filename precedence is
/// `lsp.json` > `.lsp.json` > `lsp.yaml` > `.lsp.yaml` > `lsp.yml` > `.lsp.yml`.
#[must_use]
pub fn load_config(cwd: &Path) -> LspConfig {
	let defaults = parse_defaults();
	let mut merged = defaults;
	let mut idle_timeout_ms = None;
	let mut has_overrides = false;

	for source in config_sources(cwd).into_iter().rev() {
		let Some(parsed) = read_config_file(&source) else {
			continue;
		};
		if !parsed.servers.is_empty() {
			has_overrides = true;
			merge_servers(&mut merged, &parsed.servers);
		}
		if parsed.idle_timeout_ms.is_some() {
			idle_timeout_ms = parsed.idle_timeout_ms;
		}
	}

	let mut servers = Vec::new();
	for (name, raw) in &merged {
		let Some(mut config) = normalize_server(raw) else {
			continue;
		};
		config.custom_linter = match name.as_str() {
			"biome" => Some(CustomLinter::Biome),
			"swiftlint" => Some(CustomLinter::SwiftLint),
			_ => None,
		};
		if name == "omnisharp" {
			let pid = std::process::id().to_string();
			for arg in &mut config.args {
				if arg == PID_TOKEN {
					*arg = pid.clone();
				}
			}
		}
		if !has_overrides {
			if !has_root_markers(cwd, &config.root_markers) {
				continue;
			}
		} else {
			if config.disabled.unwrap_or(false) || !has_root_markers(cwd, &config.root_markers) {
				continue;
			}
		}
		let Some(resolved) = resolve_command(&config.command, cwd) else {
			continue;
		};
		config.resolved_command = Some(resolved);
		servers.push((name.clone(), config));
	}

	LspConfig::new(servers, idle_timeout_ms)
}

fn parse_defaults() -> Map<String, Value> {
	serde_json::from_str::<Value>(DEFAULTS_JSON)
		.ok()
		.and_then(|value| value.as_object().cloned())
		.unwrap_or_default()
}

fn merge_servers(base: &mut Map<String, Value>, overrides: &Map<String, Value>) {
	for (name, override_config) in overrides {
		let candidate = match base.get(name) {
			Some(existing) if existing.is_object() && override_config.is_object() => {
				let mut merged = existing.as_object().cloned().unwrap_or_default();
				for (key, value) in override_config.as_object().into_iter().flatten() {
					merged.insert(key.clone(), value.clone());
				}
				Value::Object(merged)
			}
			_ => override_config.clone(),
		};
		if normalize_server(&candidate).is_some() {
			base.insert(name.clone(), candidate);
		}
		// Invalid override keeps the previous config, matching upstream.
	}
}

fn normalize_server(raw: &Value) -> Option<ServerConfig> {
	let object = raw.as_object()?;
	let command = object.get("command").and_then(Value::as_str)?.trim();
	if command.is_empty() {
		return None;
	}

	let extension_to_language = object.get("extensionToLanguage").and_then(Value::as_object);
	let file_types = normalize_string_array(object.get("fileTypes")).or_else(|| {
		extension_to_language.map(|extensions| extensions.keys().cloned().collect::<Vec<_>>())
	})?;
	if file_types.is_empty() {
		return None;
	}
	let root_markers = normalize_string_array(object.get("rootMarkers"))
		.or_else(|| extension_to_language.map(|_| vec![".".to_string()]))?;
	if root_markers.is_empty() {
		return None;
	}

	let args = object
		.get("args")
		.and_then(Value::as_array)
		.map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
		.unwrap_or_default();

	let init_options = object
		.get("initOptions")
		.filter(|value| value.is_object())
		.or_else(|| object.get("initializationOptions").filter(|value| value.is_object()))
		.cloned();
	let settings = object.get("settings").filter(|value| value.is_object()).cloned();
	let capabilities = object
		.get("capabilities")
		.and_then(|value| serde_json::from_value::<ServerCapabilities>(value.clone()).ok());
	let workspace_ready_timings = object
		.get("workspaceReadyTimings")
		.and_then(|value| serde_json::from_value::<WorkspaceReadyTimings>(value.clone()).ok());

	Some(ServerConfig {
		command: command.to_string(),
		args,
		file_types,
		language_id: object
			.get("languageId")
			.and_then(Value::as_str)
			.filter(|value| !value.is_empty())
			.map(str::to_string),
		root_markers,
		init_options,
		settings,
		disabled: object.get("disabled").and_then(Value::as_bool),
		warmup_timeout_ms: object.get("warmupTimeoutMs").and_then(Value::as_u64),
		workspace_ready_timings,
		capabilities,
		is_linter: object.get("isLinter").and_then(Value::as_bool),
		resolved_command: None,
		custom_linter: None,
	})
}

fn normalize_string_array(value: Option<&Value>) -> Option<Vec<String>> {
	let array = value?.as_array()?;
	let items: Vec<String> = array
		.iter()
		.filter_map(Value::as_str)
		.map(str::trim)
		.filter(|item| !item.is_empty())
		.map(str::to_string)
		.collect();
	(!items.is_empty()).then_some(items)
}

// =============================================================================
// Config sources
// =============================================================================

/// Config paths in highest-to-lowest precedence order.
fn config_sources(cwd: &Path) -> Vec<PathBuf> {
	let mut sources = Vec::new();
	push_location(&mut sources, cwd);
	for dir in project_config_dirs(cwd) {
		push_location(&mut sources, &dir);
	}
	for dir in user_config_dirs() {
		push_location(&mut sources, &dir);
	}
	if let Some(home) = home_dir() {
		push_location(&mut sources, &home);
	}
	sources
}

fn push_location(sources: &mut Vec<PathBuf>, dir: &Path) {
	for filename in CONFIG_FILENAMES {
		sources.push(dir.join(filename));
	}
}

fn project_config_dirs(cwd: &Path) -> Vec<PathBuf> {
	[".omp", ".claude", ".codex", ".gemini"].iter().map(|dir| cwd.join(dir)).collect()
}

fn user_config_dirs() -> Vec<PathBuf> {
	let mut dirs = Vec::new();
	if let Some(agent) = native_agent_dir() {
		dirs.push(agent);
	}
	if let Some(home) = home_dir() {
		for dir in [".claude", ".codex", ".gemini"] {
			dirs.push(home.join(dir));
		}
	}
	dirs
}

fn native_agent_dir() -> Option<PathBuf> {
	if let Some(dir) = std::env::var_os("PI_CODING_AGENT_DIR").map(PathBuf::from) {
		if !dir.as_os_str().is_empty() {
			return Some(dir);
		}
	}
	let base = std::env::var_os("PI_CONFIG_DIR")
		.map(PathBuf::from)
		.filter(|path| !path.as_os_str().is_empty())
		.or_else(|| home_dir().map(|home| home.join(".omp")))?;
	let profile = ["OMP_PROFILE", "PI_PROFILE"]
		.iter()
		.find_map(|key| std::env::var(key).ok())
		.filter(|value| !value.is_empty());
	Some(match profile {
		Some(profile) => base.join("profiles").join(profile).join("agent"),
		None => base.join("agent"),
	})
}

fn home_dir() -> Option<PathBuf> {
	std::env::var_os("HOME")
		.or_else(|| std::env::var_os("USERPROFILE"))
		.map(PathBuf::from)
		.filter(|path| !path.as_os_str().is_empty())
}

fn read_config_file(path: &Path) -> Option<NormalizedConfig> {
	let content = fs::read_to_string(path).ok()?;
	let extension = path.extension().and_then(|value| value.to_str()).unwrap_or("").to_ascii_lowercase();
	let parsed = if extension == "yaml" || extension == "yml" {
		match parse_yaml_subset(&content) {
			Ok(parsed) => parsed,
			Err(error) => {
				eprintln!("[lsp] ignoring {}: {error}", path.display());
				return None;
			}
		}
	} else {
		serde_json::from_str::<Value>(&content).ok()?
	};
	normalize_config(parsed)
}

fn normalize_config(value: Value) -> Option<NormalizedConfig> {
	let object = value.as_object()?;
	let idle_timeout_ms = object
		.get("idleTimeoutMs")
		.and_then(Value::as_u64)
		.filter(|value| *value > 0);
	if let Some(raw_servers) = object.get("servers") {
		let servers = raw_servers.as_object()?.clone();
		return Some(NormalizedConfig { servers, idle_timeout_ms });
	}
	let servers = object
		.iter()
		.filter(|(key, _)| key.as_str() != "idleTimeoutMs")
		.map(|(key, value)| (key.clone(), value.clone()))
		.collect();
	Some(NormalizedConfig { servers, idle_timeout_ms })
}

// =============================================================================
// Root markers and binary resolution
// =============================================================================

/// True when any marker exists directly inside `dir`. Wildcard markers match a
/// single directory level (upstream deliberately avoids recursive globbing).
#[must_use]
pub fn has_root_markers(dir: &Path, markers: &[String]) -> bool {
	let mut entries: Option<Vec<String>> = None;
	for marker in markers {
		if marker.contains('*') {
			let names = entries.get_or_insert_with(|| {
				fs::read_dir(dir)
					.ok()
					.map(|read| {
						read.filter_map(Result::ok)
							.filter_map(|entry| entry.file_name().into_string().ok())
							.collect()
					})
					.unwrap_or_default()
			});
			if names.iter().any(|name| wildcard_match(marker, name)) {
				return true;
			}
			continue;
		}
		if dir.join(marker).exists() {
			return true;
		}
	}
	false
}

/// True when any ancestor directory of `file_path` contains a root marker.
#[must_use]
pub fn has_root_marker_ancestor(file_path: &Path, markers: &[String]) -> bool {
	if markers.is_empty() {
		return false;
	}
	let mut dir = file_path.parent().map(Path::to_path_buf);
	loop {
		let Some(current) = dir else { return false };
		if has_root_markers(&current, markers) {
			return true;
		}
		dir = current.parent().map(Path::to_path_buf);
		if current.parent().is_none() {
			return false;
		}
	}
}

fn wildcard_match(pattern: &str, name: &str) -> bool {
	// One-level markers only need `*` and `?`; avoid compiling a regex for the
	// handful of built-ins that use wildcards (`*.cabal`, `*.sln`, ...).
	fn walk(pattern: &[u8], name: &[u8]) -> bool {
		match pattern.first() {
			None => name.is_empty(),
			Some(b'*') => {
				if name.is_empty() {
					walk(&pattern[1..], name)
				} else {
					walk(&pattern[1..], name) || walk(pattern, &name[1..])
				}
			}
			Some(b'?') => !name.is_empty() && walk(&pattern[1..], &name[1..]),
			Some(byte) => name.first() == Some(byte) && walk(&pattern[1..], &name[1..]),
		}
	}
	walk(pattern.as_bytes(), name.as_bytes())
}

/// Resolve `command` to an executable path: project-local bin dirs first, then
/// `$PATH`.
#[must_use]
pub fn resolve_command(command: &str, cwd: &Path) -> Option<PathBuf> {
	if Path::new(command).is_absolute() {
		return executable_path(Path::new(command));
	}
	if command.contains('/') {
		return executable_path(&cwd.join(command));
	}
	for (markers, bin_dir) in LOCAL_BIN_PATHS {
		let markers_owned: Vec<String> = markers.iter().map(|marker| (*marker).to_string()).collect();
		if !has_root_markers(cwd, &markers_owned) {
			continue;
		}
		if let Some(path) = executable_path(&cwd.join(bin_dir).join(command)) {
			return Some(path);
		}
	}
	let path_var = std::env::var_os("PATH")?;
	for dir in std::env::split_paths(&path_var) {
		if let Some(path) = executable_path(&dir.join(command)) {
			return Some(path);
		}
	}
	None
}

fn executable_path(path: &Path) -> Option<PathBuf> {
	if !path.is_file() {
		return None;
	}
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		let mode = fs::metadata(path).ok()?.permissions().mode();
		(mode & 0o111 != 0).then(|| path.to_path_buf())
	}
	#[cfg(not(unix))]
	{
		Some(path.to_path_buf())
	}
}

// =============================================================================
// Server selection
// =============================================================================

/// All configured non-custom server entries, in configuration order.
///
/// Custom linters (`createClient`, e.g. biome) are diagnostics-only adapters,
/// not LSP servers; callers that need them use [`get_servers_for_file`] and the
/// linter pipeline.
#[must_use]
pub fn get_lsp_servers(config: &LspConfig) -> Vec<(String, ServerConfig)> {
	config
		.servers()
		.iter()
		.filter(|(_, server)| !server.is_custom_linter())
		.cloned()
		.collect()
}

/// Servers whose `fileTypes` match the file extension or basename, primary
/// (non-linter) servers first.
#[must_use]
pub fn get_servers_for_file(config: &LspConfig, file_path: &Path) -> Vec<(String, ServerConfig)> {
	let extension = file_path
		.extension()
		.and_then(|value| value.to_str())
		.map(|value| format!(".{}", value.to_ascii_lowercase()))
		.unwrap_or_default();
	let extension_no_dot = extension.trim_start_matches('.').to_string();
	let file_name = file_path
		.file_name()
		.and_then(|value| value.to_str())
		.map(str::to_ascii_lowercase)
		.unwrap_or_default();

	let mut matches: Vec<(String, ServerConfig)> = config
		.servers()
		.iter()
		.filter(|(_, server)| {
			server.file_types.iter().any(|file_type| {
				let normalized = file_type.to_ascii_lowercase();
				let normalized_no_dot = normalized.trim_start_matches('.');
				normalized == extension
					|| normalized == file_name
					|| normalized_no_dot == extension_no_dot
					|| normalized_no_dot == file_name
			})
		})
		.cloned()
		.collect();
	matches.sort_by_key(|(_, server)| u8::from(server.is_linter.unwrap_or(false)));
	matches
}

/// Primary server for a file. Custom linter adapters are excluded because
/// navigation and refactor actions need type intelligence.
#[must_use]
pub fn get_lsp_servers_for_file(config: &LspConfig, file_path: &Path) -> Vec<(String, ServerConfig)> {
	get_servers_for_file(config, file_path)
		.into_iter()
		.filter(|(_, server)| !server.is_custom_linter())
		.collect()
}

#[must_use]
pub fn get_lsp_server_for_file(config: &LspConfig, file_path: &Path) -> Option<(String, ServerConfig)> {
	get_lsp_servers_for_file(config, file_path).into_iter().next()
}

#[must_use]
pub fn split_servers(
	servers: Vec<(String, ServerConfig)>,
) -> (Vec<(String, ServerConfig)>, Vec<(String, ServerConfig)>) {
	servers.into_iter().partition(|(_, server)| !server.is_custom_linter())
}

/// True when `err` reports an unsupported JSON-RPC method.
#[must_use]
pub fn is_method_not_found_error(message: &str) -> bool {
	let lower = message.to_ascii_lowercase();
	lower.contains("method not found")
		|| lower.contains("unhandled method")
		|| lower.contains("not supported")
		|| lower.contains("-32601")
}

// =============================================================================
// Minimal YAML subset
// =============================================================================

/// Parse the shared YAML reader and convert it to the JSON tree used by the
/// config normalizer. Unsupported YAML constructs surface as errors.
fn parse_yaml_subset(text: &str) -> Result<Value, String> {
	super::yaml::parse(text).map(|value| value.to_json()).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn server(value: &str) -> Value {
		serde_json::from_str(value).expect("valid JSON fixture")
	}

	#[test]
	fn yaml_parses_nested_servers_and_sequences() {
		let parsed = parse_yaml_subset(
			"servers:\n  my-lsp:\n    command: my-lsp\n    args:\n      - --stdio\n    fileTypes: ['.xyz', '.abc']\n    rootMarkers:\n      - .xyz-project\n    settings:\n      analyze: true\n",
		)
		.expect("valid yaml");
		let config = normalize_config(parsed).expect("normalized");
		let raw = config.servers.get("my-lsp").expect("server");
		let normalized = normalize_server(raw).expect("valid server");
		assert_eq!(normalized.command, "my-lsp");
		assert_eq!(normalized.args, vec!["--stdio"]);
		assert_eq!(normalized.file_types, vec![".xyz", ".abc"]);
		assert_eq!(normalized.root_markers, vec![".xyz-project"]);
		assert_eq!(normalized.settings.unwrap()["analyze"], Value::Bool(true));
	}

	#[test]
	fn flat_config_is_accepted() {
		let parsed = parse_yaml_subset("my-lsp:\n  command: my-lsp\n  fileTypes:\n    - .xyz\n  rootMarkers:\n    - .git\n").unwrap();
		let config = normalize_config(parsed).unwrap();
		assert!(config.servers.contains_key("my-lsp"));
	}

	#[test]
	fn invalid_override_keeps_previous_server() {
		let mut base = Map::new();
		base.insert(
			"s".to_string(),
			server(r#"{"command":"one","fileTypes":[".x"],"rootMarkers":["."]}"#),
		);
		let mut overrides = Map::new();
		overrides.insert("s".to_string(), server(r#"{"command":""}"#));
		merge_servers(&mut base, &overrides);
		let normalized = normalize_server(base.get("s").unwrap()).unwrap();
		assert_eq!(normalized.command, "one");
	}

	#[test]
	fn unsupported_yaml_constructs_error_instead_of_misparsing() {
		assert!(parse_yaml_subset("servers:\n  &anchor x\n").is_err());
		assert!(parse_yaml_subset("settings: {a: 1}\n").is_err());
		assert!(parse_yaml_subset("a: 1\n---\nb: 2\n").is_err());
		assert!(parse_yaml_subset("\tbad: 1\n").is_err());
		assert!(parse_yaml_subset("a: 1\na: 2\n").is_err());
		let block = parse_yaml_subset("prompt: |\n  line one\n  line two\n").expect("block scalar");
		assert_eq!(block["prompt"], Value::String("line one\nline two\n".to_string()));
	}

	#[test]
	fn wildcard_root_marker_matches_one_level() {
		assert!(wildcard_match("*.cabal", "pkg.cabal"));
		assert!(!wildcard_match("*.cabal", "sub.pkg.cabal"));
		assert!(wildcard_match("*.sln", "App.sln"));
	}
}
