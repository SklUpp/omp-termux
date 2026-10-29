//! Debug adapter definitions, project config discovery, and auto-selection.
//!
//! Mirrors `packages/coding-agent/src/dap/config.ts` + `defaults.json`: the 14
//! built-in adapters stay embedded, project/user `dap.{json,yaml,yml}` overlays
//! merge lowest-to-highest priority, and adapter commands are resolved on
//! `PATH` without installing anything. A configured-but-missing adapter is
//! reported with the command the user must provide.

use std::{
	collections::HashMap,
	path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Embedded copy of the upstream `defaults.json`.
const DEFAULTS_JSON: &str = r#"
{
	"gdb": {
		"command": "gdb", "args": ["-i", "dap"],
		"languages": ["c", "cpp", "rust"],
		"fileTypes": [".c", ".cc", ".cpp", ".cxx", ".h", ".hh", ".hpp", ".hxx", ".rs"],
		"rootMarkers": ["Makefile", "CMakeLists.txt", "Cargo.toml", "compile_commands.json"],
		"launchDefaults": { "request": "launch", "stopOnEntry": true, "stopAtBeginningOfMainSubprogram": true },
		"attachDefaults": { "request": "attach" }
	},
	"lldb-dap": {
		"command": "lldb-dap", "args": [],
		"languages": ["c", "cpp", "objc", "swift", "rust", "zig"],
		"fileTypes": [".c", ".cc", ".cpp", ".cxx", ".m", ".mm", ".swift", ".rs", ".zig"],
		"rootMarkers": ["Package.swift", "Cargo.toml", "Makefile", "CMakeLists.txt", "build.zig"],
		"launchDefaults": { "request": "launch", "stopOnEntry": true },
		"attachDefaults": { "request": "attach" }
	},
	"codelldb": {
		"command": "codelldb", "args": ["--port", "0"],
		"languages": ["c", "cpp", "rust", "zig"],
		"fileTypes": [".c", ".cc", ".cpp", ".cxx", ".rs", ".zig"],
		"rootMarkers": ["Cargo.toml", "CMakeLists.txt", "Makefile", "compile_commands.json", "build.zig"],
		"launchDefaults": { "request": "launch", "stopOnEntry": true },
		"attachDefaults": { "request": "attach" }
	},
	"debugpy": {
		"command": "python", "args": ["-m", "debugpy.adapter"],
		"languages": ["python"],
		"fileTypes": [".py"],
		"rootMarkers": ["pyproject.toml", "setup.py", "requirements.txt", "Pipfile"],
		"launchDefaults": { "request": "launch", "justMyCode": false, "stopOnEntry": true },
		"attachDefaults": { "request": "attach", "justMyCode": false }
	},
	"dlv": {
		"command": "dlv", "args": ["dap"], "connectMode": "socket",
		"languages": ["go"],
		"fileTypes": [".go"],
		"rootMarkers": ["go.mod", "go.sum", "go.work"],
		"acceptsDirectoryProgram": true,
		"launchDefaults": { "request": "launch", "mode": "debug", "stopOnEntry": true },
		"attachDefaults": { "request": "attach", "mode": "local" }
	},
	"js-debug-adapter": {
		"command": "js-debug-adapter", "args": [],
		"languages": ["javascript", "typescript"],
		"fileTypes": [".js", ".jsx", ".ts", ".tsx", ".mjs", ".cjs"],
		"rootMarkers": ["package.json", "tsconfig.json", "jsconfig.json"],
		"launchDefaults": { "request": "launch", "type": "pwa-node", "stopOnEntry": true },
		"attachDefaults": { "request": "attach", "type": "pwa-node" }
	},
	"netcoredbg": {
		"command": "netcoredbg", "args": ["--interpreter=vscode"],
		"languages": ["csharp", "fsharp"],
		"fileTypes": [".cs", ".csx", ".fs", ".fsx"],
		"rootMarkers": ["*.sln", "*.csproj", "*.fsproj", "global.json"],
		"launchDefaults": { "request": "launch", "stopAtEntry": true },
		"attachDefaults": { "request": "attach" }
	},
	"kotlin-debug-adapter": {
		"command": "kotlin-debug-adapter", "args": [],
		"languages": ["kotlin"],
		"fileTypes": [".kt", ".kts"],
		"rootMarkers": ["build.gradle", "build.gradle.kts", "pom.xml", "settings.gradle", "settings.gradle.kts"],
		"launchDefaults": { "request": "launch", "mainClass": "", "projectRoot": "" },
		"attachDefaults": { "request": "attach" }
	},
	"rdbg": {
		"command": "rdbg", "args": ["--open", "--command", "--"],
		"languages": ["ruby"],
		"fileTypes": [".rb", ".rake", ".gemspec"],
		"rootMarkers": ["Gemfile", "Rakefile", ".ruby-version"],
		"launchDefaults": { "request": "launch", "type": "rdbg" },
		"attachDefaults": { "request": "attach", "type": "rdbg" }
	},
	"php-debug-adapter": {
		"command": "php-debug-adapter", "args": [],
		"languages": ["php"],
		"fileTypes": [".php", ".phtml"],
		"rootMarkers": ["composer.json", "composer.lock"],
		"launchDefaults": { "request": "launch", "stopOnEntry": true },
		"attachDefaults": { "request": "attach" }
	},
	"bash-debug-adapter": {
		"command": "bash-debug-adapter", "args": [],
		"languages": ["bash", "shell"],
		"fileTypes": [".sh", ".bash"],
		"rootMarkers": [".git"],
		"launchDefaults": { "request": "launch", "type": "bashdb", "pathBashdb": "bashdb", "pathBash": "bash" },
		"attachDefaults": { "request": "attach" }
	},
	"dart-debug-adapter": {
		"command": "dart", "args": ["debug_adapter"],
		"languages": ["dart"],
		"fileTypes": [".dart"],
		"rootMarkers": ["pubspec.yaml", "pubspec.lock"],
		"launchDefaults": { "request": "launch", "stopOnEntry": true },
		"attachDefaults": { "request": "attach" }
	},
	"flutter-debug-adapter": {
		"command": "dart", "args": ["debug_adapter", "--flutter-sdk-path", ""],
		"languages": ["dart"],
		"fileTypes": [".dart"],
		"rootMarkers": ["pubspec.yaml", "android", "ios", "lib/main.dart"],
		"launchDefaults": { "request": "launch" },
		"attachDefaults": { "request": "attach" }
	},
	"elixir-ls-debugger": {
		"command": "elixir-ls-debugger", "args": [],
		"languages": ["elixir"],
		"fileTypes": [".ex", ".exs", ".heex", ".eex"],
		"rootMarkers": ["mix.exs", "mix.lock"],
		"launchDefaults": { "request": "launch", "type": "mix_task", "task": "run", "stopOnEntry": true },
		"attachDefaults": { "request": "attach" }
	}
}
"#;

const DAP_PORT_ARGUMENT: &str = "${port}";
const JS_DEBUG_SERVER_ENV: &str = "JS_DEBUG_DAP_SERVER";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectMode {
	Stdio,
	Socket,
	Tcp,
}

impl ConnectMode {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Stdio => "stdio",
			Self::Socket => "socket",
			Self::Tcp => "tcp",
		}
	}
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdapterConfig {
	pub command: String,
	#[serde(default)]
	pub args: Vec<String>,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub languages: Vec<String>,
	#[serde(default)]
	pub file_types: Vec<String>,
	#[serde(default)]
	pub root_markers: Vec<String>,
	#[serde(default)]
	pub launch_defaults: Value,
	#[serde(default)]
	pub attach_defaults: Value,
	#[serde(default)]
	pub connect_mode: Option<ConnectMode>,
	#[serde(default)]
	pub accepts_directory_program: bool,
}

impl AdapterConfig {
	fn normalize(mut self) -> Self {
		self.file_types = self.file_types.into_iter().map(|entry| entry.to_lowercase()).collect();
		self.languages.retain(|entry| !entry.is_empty());
		self.args.retain(|entry| !entry.is_empty());
		self.root_markers.retain(|entry| !entry.is_empty());
		if !self.launch_defaults.is_object() {
			self.launch_defaults = Value::Object(Map::new());
		}
		if !self.attach_defaults.is_object() {
			self.attach_defaults = Value::Object(Map::new());
		}
		self
	}
}

#[derive(Debug, Clone)]
pub struct ResolvedAdapter {
	pub name: String,
	pub command: String,
	pub args: Vec<String>,
	pub resolved_command: String,
	pub languages: Vec<String>,
	pub file_types: Vec<String>,
	pub root_markers: Vec<String>,
	pub launch_defaults: Value,
	pub attach_defaults: Value,
	pub connect_mode: ConnectMode,
	pub accepts_directory_program: bool,
}

impl ResolvedAdapter {
	pub fn skip_attach_request(&self) -> bool {
		self.attach_defaults.get("skipAttachRequest").and_then(Value::as_bool) == Some(true)
	}
}

/// Adapter configs in discovery order (defaults first, overlays appended).
#[derive(Debug, Clone, Default)]
pub struct AdapterConfigs {
	entries: Vec<(String, AdapterConfig)>,
}

impl AdapterConfigs {
	pub fn get(&self, name: &str) -> Option<&AdapterConfig> {
		self.entries.iter().find(|(key, _)| key == name).map(|(_, config)| config)
	}

	pub fn names(&self) -> Vec<&str> {
		self.entries.iter().map(|(name, _)| name.as_str()).collect()
	}

	pub fn iter(&self) -> impl Iterator<Item = (&str, &AdapterConfig)> {
		self.entries.iter().map(|(name, config)| (name.as_str(), config))
	}

	fn set(&mut self, name: String, config: AdapterConfig) {
		if let Some(existing) = self.entries.iter_mut().find(|(key, _)| *key == name) {
			existing.1 = config;
		} else {
			self.entries.push((name, config));
		}
	}
}

fn defaults() -> AdapterConfigs {
	let mut configs = AdapterConfigs::default();
	let parsed: Value = serde_json::from_str(DEFAULTS_JSON).expect("embedded DAP defaults are valid JSON");
	let Some(object) = parsed.as_object() else {
		return configs;
	};
	for (name, raw) in object {
		if let Ok(config) = serde_json::from_value::<AdapterConfig>(raw.clone()) {
			configs.set(name.clone(), config.normalize());
		}
	}
	configs
}

fn merge_adapter(base: Option<&AdapterConfig>, raw: &Value) -> Option<AdapterConfig> {
	let Some(raw_object) = raw.as_object() else {
		return None;
	};
	let mut merged = base.map(|config| serde_json::to_value(AdapterConfigWire::from(config)).unwrap_or(Value::Null));
	let mut target = match merged.take() {
		Some(Value::Object(object)) => object,
		_ => Map::new(),
	};
	for (key, value) in raw_object {
		if key == "launchDefaults" || key == "attachDefaults" {
			let nested_key = if key == "launchDefaults" { "launchDefaults" } else { "attachDefaults" };
			let mut nested = target
				.get(nested_key)
				.and_then(Value::as_object)
				.cloned()
				.unwrap_or_default();
			if let Some(overrides) = value.as_object() {
				for (nested_name, nested_value) in overrides {
					nested.insert(nested_name.clone(), nested_value.clone());
				}
			}
			target.insert(nested_key.to_string(), Value::Object(nested));
			continue;
		}
		target.insert(key.clone(), value.clone());
	}
	serde_json::from_value::<AdapterConfig>(Value::Object(target)).ok().map(AdapterConfig::normalize)
}

/// Serialization bridge so merge keeps canonical camelCase field names.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AdapterConfigWire<'a> {
	command: &'a str,
	args: &'a Vec<String>,
	languages: &'a Vec<String>,
	file_types: &'a Vec<String>,
	root_markers: &'a Vec<String>,
	launch_defaults: &'a Value,
	attach_defaults: &'a Value,
	connect_mode: Option<ConnectMode>,
	accepts_directory_program: bool,
}

impl<'a> From<&'a AdapterConfig> for AdapterConfigWire<'a> {
	fn from(config: &'a AdapterConfig) -> Self {
		Self {
			command: &config.command,
			args: &config.args,
			languages: &config.languages,
			file_types: &config.file_types,
			root_markers: &config.root_markers,
			launch_defaults: &config.launch_defaults,
			attach_defaults: &config.attach_defaults,
			connect_mode: config.connect_mode,
			accepts_directory_program: config.accepts_directory_program,
		}
	}
}

fn env_value<'a>(env: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
	env.get(key).map(String::as_str)
}

fn home_dir(env: &HashMap<String, String>) -> Option<PathBuf> {
	env_value(env, "HOME").filter(|home| !home.is_empty()).map(PathBuf::from)
}

fn config_file_names() -> [&'static str; 6] {
	["dap.json", ".dap.json", "dap.yaml", ".dap.yaml", "dap.yml", ".dap.yml"]
}

fn push_config_files(sources: &mut Vec<PathBuf>, dir: &Path) {
	for name in config_file_names() {
		sources.push(dir.join(name));
	}
}

/// Nearest first: with the later reversal in `load_adapter_configs`, closer
/// directories override ancestors and the launch cwd overrides everything.
fn project_ancestors(cwd: &Path) -> Vec<PathBuf> {
	cwd.ancestors().map(Path::to_path_buf).collect()
}

fn config_sources(cwd: &Path, env: &HashMap<String, String>) -> Vec<PathBuf> {
	let mut sources = Vec::new();
	// Plain files directly in the launch cwd win over every directory overlay.
	push_config_files(&mut sources, cwd);
	for ancestor in project_ancestors(cwd) {
		for dir in [".omp", ".claude", ".codex", ".gemini"] {
			push_config_files(&mut sources, &ancestor.join(dir));
		}
	}
	if let Some(home) = home_dir(env) {
		for dir in [".omp/agent", ".claude", ".codex", ".gemini"] {
			push_config_files(&mut sources, &home.join(dir));
		}
		// Plugin roots are excluded by the Android boundary; the home fallback stays.
		push_config_files(&mut sources, &home);
	}
	sources
}

fn parse_yaml_scalar(text: &str) -> Value {
	let text = text.trim();
	if text.is_empty() {
		return Value::Null;
	}
	if (text.starts_with('"') && text.ends_with('"') && text.len() >= 2)
		|| (text.starts_with('\'') && text.ends_with('\'') && text.len() >= 2)
	{
		let inner = &text[1..text.len() - 1];
		return Value::String(if text.starts_with('"') {
			inner.replace("\\\"", "\"").replace("\\n", "\n").replace("\\\\", "\\")
		} else {
			inner.replace("''", "'")
		});
	}
	match text {
		"true" => return Value::Bool(true),
		"false" => return Value::Bool(false),
		"null" | "~" => return Value::Null,
		_ => {},
	}
	if let Ok(number) = text.parse::<i64>() {
		return Value::Number(number.into());
	}
	if let Ok(number) = text.parse::<f64>() {
		if let Some(number) = serde_json::Number::from_f64(number) {
			return Value::Number(number);
		}
	}
	if text.starts_with('[') && text.ends_with(']') {
		let inner = &text[1..text.len() - 1];
		return Value::Array(split_inline(inner).into_iter().map(|entry| parse_yaml_scalar(&entry)).collect());
	}
	if text.starts_with('{') && text.ends_with('}') {
		let mut object = Map::new();
		for entry in split_inline(&text[1..text.len() - 1]) {
			if let Some((key, value)) = entry.split_once(':') {
				object.insert(key.trim().trim_matches('"').to_string(), parse_yaml_scalar(value));
			}
		}
		return Value::Object(object);
	}
	Value::String(text.to_string())
}

fn split_inline(text: &str) -> Vec<String> {
	let mut parts = Vec::new();
	let mut current = String::new();
	let mut quote: Option<char> = None;
	for ch in text.chars() {
		match quote {
			Some(active) => {
				current.push(ch);
				if ch == active {
					quote = None;
				}
			},
			None if ch == '"' || ch == '\'' => {
				quote = Some(ch);
				current.push(ch);
			},
			None if ch == ',' => {
				parts.push(current.trim().to_string());
				current.clear();
			},
			None => current.push(ch),
		}
	}
	if !current.trim().is_empty() {
		parts.push(current.trim().to_string());
	}
	parts
}

fn strip_yaml_comment(line: &str) -> &str {
	let mut quote: Option<char> = None;
	for (index, ch) in line.char_indices() {
		match quote {
			Some(active) if ch == active => quote = None,
			Some(_) => {},
			None if ch == '"' || ch == '\'' => quote = Some(ch),
			None if ch == '#' => return &line[..index],
			None => {},
		}
	}
	line
}

/// Minimal YAML reader for adapter overlays: nested maps, scalar sequences,
/// inline arrays/objects, quoted/plain scalars. Anchors, multi-line scalars and
/// merge keys are intentionally unsupported.
fn parse_yaml_subset(text: &str) -> Result<Value, String> {
	let lines: Vec<(usize, String)> = text
		.lines()
		.filter_map(|line| {
			let trimmed = strip_yaml_comment(line).trim_end();
			if trimmed.trim().is_empty() {
				return None;
			}
			let indent = trimmed.len() - trimmed.trim_start().len();
			Some((indent, trimmed.trim_start().to_string()))
		})
		.collect();
	let mut index = 0usize;
	let value = parse_yaml_block(&lines, &mut index, lines.first().map(|(indent, _)| *indent).unwrap_or(0))?;
	Ok(value)
}

fn parse_yaml_block(lines: &[(usize, String)], index: &mut usize, indent: usize) -> Result<Value, String> {
	if *index >= lines.len() {
		return Ok(Value::Null);
	}
	if lines[*index].1.starts_with("- ") || lines[*index].1 == "-" {
		let mut array = Vec::new();
		while *index < lines.len() && lines[*index].0 == indent && (lines[*index].1.starts_with("- ") || lines[*index].1 == "-") {
			let content = lines[*index].1[1..].trim().to_string();
			*index += 1;
			if content.is_empty() {
				let next_indent = lines.get(*index).map(|(indent, _)| *indent).unwrap_or(indent + 2);
				array.push(parse_yaml_block(lines, index, next_indent)?);
			} else {
				array.push(parse_yaml_scalar(&content));
			}
		}
		return Ok(Value::Array(array));
	}
	let mut object = Map::new();
	while *index < lines.len() && lines[*index].0 == indent {
		let content = lines[*index].1.clone();
		let Some(split) = find_yaml_colon(&content) else {
			return Err(format!("unsupported YAML line: {content}"));
		};
		let key = content[..split].trim().trim_matches('"').trim_matches('\'').to_string();
		let rest = content[split + 1..].trim().to_string();
		*index += 1;
		let value = if rest.is_empty() {
			let next_indent = lines.get(*index).map(|(indent, _)| *indent).unwrap_or(indent);
			if next_indent > indent {
				parse_yaml_block(lines, index, next_indent)?
			} else {
				Value::Null
			}
		} else {
			parse_yaml_scalar(&rest)
		};
		object.insert(key, value);
	}
	Ok(Value::Object(object))
}

fn find_yaml_colon(text: &str) -> Option<usize> {
	let mut quote: Option<char> = None;
	for (index, ch) in text.char_indices() {
		match quote {
			Some(active) if ch == active => quote = None,
			Some(_) => {},
			None if ch == '"' || ch == '\'' => quote = Some(ch),
			None if ch == ':' => return Some(index),
			None => {},
		}
	}
	None
}

fn read_config_file(path: &Path) -> Option<Value> {
	let content = std::fs::read_to_string(path).ok()?;
	let extension = path.extension().and_then(|value| value.to_str()).unwrap_or("");
	let parsed = if extension.eq_ignore_ascii_case("yaml") || extension.eq_ignore_ascii_case("yml") {
		serde_json::from_str(&content).or_else(|_| parse_yaml_subset(&content)).ok()?
	} else {
		serde_json::from_str(&content).ok()?
	};
	let object = parsed.as_object()?;
	Some(match object.get("adapters") {
		Some(Value::Object(adapters)) => Value::Object(adapters.clone()),
		Some(_) => return None,
		None => parsed,
	})
}

fn load_adapter_configs(cwd: &Path, env: &HashMap<String, String>) -> AdapterConfigs {
	let mut configs = defaults();
	let mut sources = config_sources(cwd, env);
	sources.reverse();
	for source in sources {
		let Some(Value::Object(overrides)) = read_config_file(&source) else {
			continue;
		};
		for (name, raw) in overrides {
			let base = configs.get(&name);
			match merge_adapter(base, &raw) {
				Some(config) => configs.set(name, config),
				None => {
					eprintln!("Ignoring invalid DAP adapter config: {name} ({})", source.display());
				},
			}
		}
	}
	configs
}

pub fn get_adapter_configs(cwd: &Path, env: &HashMap<String, String>) -> AdapterConfigs {
	load_adapter_configs(cwd, env)
}

fn is_executable_file(path: &Path) -> bool {
	let Ok(metadata) = std::fs::metadata(path) else {
		return false;
	};
	if !metadata.is_file() {
		return false;
	}
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		metadata.permissions().mode() & 0o111 != 0
	}
	#[cfg(not(unix))]
	{
		true
	}
}

fn normalize_command_for_cwd(command: &str, cwd: &Path) -> PathBuf {
	let path = PathBuf::from(command);
	if path.is_absolute() {
		return path;
	}
	if command.starts_with("./") || command.starts_with("../") || command.starts_with(".\\") || command.starts_with("..\\") {
		return cwd.join(command);
	}
	path
}

/// Resolve an executable name to an absolute path, without instantiating it.
pub fn resolve_command(command: &str, cwd: &Path, env: &HashMap<String, String>) -> Option<String> {
	let looks_like_path = command.contains('/') || command.contains('\\') || Path::new(command).is_absolute();
	if looks_like_path {
		let path = normalize_command_for_cwd(command, cwd);
		return is_executable_file(&path).then(|| path.to_string_lossy().into_owned());
	}
	let path_var = env_value(env, "PATH")
		.map(str::to_string)
		.or_else(|| std::env::var("PATH").ok())
		.unwrap_or_default();
	let separator = if cfg!(windows) { ';' } else { ':' };
	for dir in path_var.split(separator) {
		let base = if dir.is_empty() { cwd.to_path_buf() } else { PathBuf::from(dir) };
		let candidate = base.join(command);
		if is_executable_file(&candidate) {
			return Some(candidate.to_string_lossy().into_owned());
		}
		if cfg!(windows) {
			for suffix in [".exe", ".cmd", ".bat"] {
				let candidate = base.join(format!("{command}{suffix}"));
				if is_executable_file(&candidate) {
					return Some(candidate.to_string_lossy().into_owned());
				}
			}
		}
	}
	None
}

fn resolve_js_debug_server(cwd: &Path, env: &HashMap<String, String>) -> Option<PathBuf> {
	let mut candidates = Vec::new();
	if let Some(configured) = env_value(env, JS_DEBUG_SERVER_ENV).filter(|value| !value.is_empty()) {
		let configured = PathBuf::from(configured);
		candidates.push(if configured.is_absolute() { configured } else { cwd.join(configured) });
	}
	let data_home = env_value(env, "XDG_DATA_HOME")
		.filter(|value| !value.is_empty())
		.map(PathBuf::from)
		.or_else(|| home_dir(env).map(|home| home.join(".local/share")));
	if let Some(data_home) = data_home {
		candidates.push(data_home.join("nvim/mason/packages/js-debug-adapter/js-debug/src/dapDebugServer.js"));
	}
	if let Some(home) = home_dir(env) {
		candidates.push(home.join(".local/opt/js-debug/src/dapDebugServer.js"));
	}
	candidates.into_iter().find(|candidate| candidate.is_file())
}

fn resolve_adapter_from_config(
	name: &str,
	configs: &AdapterConfigs,
	cwd: &Path,
	env: &HashMap<String, String>,
	local_roots: Option<&[PathBuf]>,
) -> Option<ResolvedAdapter> {
	let config = configs.get(name)?;
	if name == "js-debug-adapter" && config.command == "js-debug-adapter" {
		let server = resolve_js_debug_server(cwd, env)?;
		let node = resolve_command("node", cwd, env)?;
		return Some(ResolvedAdapter {
			name: name.to_string(),
			command: "node".to_string(),
			args: vec![server.to_string_lossy().into_owned(), DAP_PORT_ARGUMENT.to_string(), "127.0.0.1".to_string()],
			resolved_command: node,
			languages: config.languages.clone(),
			file_types: config.file_types.clone(),
			root_markers: config.root_markers.clone(),
			launch_defaults: config.launch_defaults.clone(),
			attach_defaults: config.attach_defaults.clone(),
			connect_mode: ConnectMode::Tcp,
			accepts_directory_program: config.accepts_directory_program,
		});
	}
	let normalized = normalize_command_for_cwd(&config.command, cwd);
	let command_is_bare = !Path::new(&config.command).is_absolute()
		&& !config.command.contains('/')
		&& !config.command.contains('\\');
	let search_cwd = if command_is_bare {
		local_roots
			.and_then(|roots| roots.last().cloned())
			.unwrap_or_else(|| cwd.to_path_buf())
	} else {
		cwd.to_path_buf()
	};
	let resolved_command = if command_is_bare {
		let mut resolved = None;
		if let Some(roots) = local_roots {
			for root in roots.iter() {
				if let Some(found) = resolve_command(&config.command, root, env) {
					resolved = Some(found);
					break;
				}
			}
		}
		resolved.or_else(|| resolve_command(&config.command, &search_cwd, env))
	} else {
		resolve_command(&normalized.to_string_lossy(), cwd, env)
	}?;
	Some(ResolvedAdapter {
		name: name.to_string(),
		command: config.command.clone(),
		args: config.args.clone(),
		resolved_command,
		languages: config.languages.clone(),
		file_types: config.file_types.clone(),
		root_markers: config.root_markers.clone(),
		launch_defaults: config.launch_defaults.clone(),
		attach_defaults: config.attach_defaults.clone(),
		connect_mode: config.connect_mode.unwrap_or(ConnectMode::Stdio),
		accepts_directory_program: config.accepts_directory_program,
	})
}

pub fn resolve_adapter(name: &str, cwd: &Path, env: &HashMap<String, String>) -> Option<ResolvedAdapter> {
	let configs = get_adapter_configs(cwd, env);
	resolve_adapter_from_config(name, &configs, cwd, env, None)
}

pub fn get_available_adapters(cwd: &Path, env: &HashMap<String, String>) -> Vec<ResolvedAdapter> {
	let configs = get_adapter_configs(cwd, env);
	configs
		.iter()
		.filter_map(|(name, _)| resolve_adapter_from_config(name, &configs, cwd, env, None))
		.collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchProgramKind {
	File,
	Directory,
	Missing,
}

#[derive(Debug, Clone)]
pub enum LaunchAdapterSelection {
	Adapter(ResolvedAdapter),
	Unavailable { adapter_name: String, command: String },
	None,
}

fn marker_matches(dir: &Path, marker: &str) -> bool {
	if let Some(suffix) = marker.strip_prefix('*') {
		let Ok(entries) = std::fs::read_dir(dir) else {
			return false;
		};
		return entries.filter_map(Result::ok).any(|entry| {
			entry
				.file_name()
				.to_string_lossy()
				.to_lowercase()
				.ends_with(&suffix.to_lowercase())
		});
	}
	dir.join(marker).exists()
}

fn has_root_markers(dir: &Path, markers: &[String]) -> bool {
	markers.iter().any(|marker| marker_matches(dir, marker))
}

fn find_root_marker_in_launch_ancestry(
	program: &Path,
	cwd: &Path,
	markers: &[String],
	program_kind: LaunchProgramKind,
) -> Option<PathBuf> {
	if markers.is_empty() {
		return None;
	}
	let resolved = if program.is_absolute() { program.to_path_buf() } else { cwd.join(program) };
	let mut dir = match program_kind {
		LaunchProgramKind::Directory => resolved,
		_ => resolved.parent().map(Path::to_path_buf).unwrap_or(resolved),
	};
	loop {
		if has_root_markers(&dir, markers) {
			return Some(dir);
		}
		let Some(parent) = dir.parent() else {
			return None;
		};
		if parent == dir {
			return None;
		}
		dir = parent.to_path_buf();
	}
}

fn resolve_adapter_for_launch(
	adapter_name: &str,
	configs: &AdapterConfigs,
	cwd: &Path,
	env: &HashMap<String, String>,
	root_dir: Option<&PathBuf>,
) -> Option<ResolvedAdapter> {
	let roots = root_dir.filter(|root| root.as_path() != cwd).map(|root| vec![root.clone(), cwd.to_path_buf()]);
	resolve_adapter_from_config(adapter_name, configs, cwd, env, roots.as_deref())
}

fn extensionless_rank(name: &str) -> Option<usize> {
	["gdb", "lldb-dap"].iter().position(|candidate| *candidate == name)
}

fn sort_adapters_for_launch(
	program: &Path,
	cwd: &Path,
	program_kind: LaunchProgramKind,
	mut adapters: Vec<ResolvedAdapter>,
) -> Vec<ResolvedAdapter> {
	let extension = program
		.extension()
		.map(|value| format!(".{}", value.to_string_lossy().to_lowercase()))
		.unwrap_or_default();
	adapters.sort_by(|left, right| {
		let left_extension = !extension.is_empty() && left.file_types.contains(&extension);
		let right_extension = !extension.is_empty() && right.file_types.contains(&extension);
		if left_extension != right_extension {
			return right_extension.cmp(&left_extension);
		}
		let left_root =
			find_root_marker_in_launch_ancestry(program, cwd, &left.root_markers, program_kind).is_some();
		let right_root =
			find_root_marker_in_launch_ancestry(program, cwd, &right.root_markers, program_kind).is_some();
		if left_root != right_root {
			return right_root.cmp(&left_root);
		}
		let left_rank = extensionless_rank(&left.name).unwrap_or(usize::MAX);
		let right_rank = extensionless_rank(&right.name).unwrap_or(usize::MAX);
		left_rank.cmp(&right_rank).then_with(|| left.name.cmp(&right.name))
	});
	adapters
}

fn select_automatic_launch_adapter(
	program: &Path,
	cwd: &Path,
	program_kind: LaunchProgramKind,
	configs: &AdapterConfigs,
	env: &HashMap<String, String>,
) -> LaunchAdapterSelection {
	let extension = program
		.extension()
		.map(|value| format!(".{}", value.to_string_lossy().to_lowercase()))
		.unwrap_or_default();
	if !extension.is_empty() {
		let mut configured: Vec<(String, Option<PathBuf>)> = Vec::new();
		let mut available = Vec::new();
		for (name, config) in configs.iter() {
			if !config.file_types.contains(&extension) {
				continue;
			}
			let root = find_root_marker_in_launch_ancestry(program, cwd, &config.root_markers, program_kind);
			configured.push((name.to_string(), root.clone()));
			if let Some(adapter) = resolve_adapter_for_launch(name, configs, cwd, env, root.as_ref()) {
				available.push(adapter);
			}
		}
		if let Some(selected) = sort_adapters_for_launch(program, cwd, program_kind, available).into_iter().next() {
			return LaunchAdapterSelection::Adapter(selected);
		}
		let root_match = configured.iter().find(|(_, root)| root.is_some());
		if let Some((name, _)) = root_match.or_else(|| configured.first()) {
			return unavailable_adapter(name, configs);
		}
	}

	let mut available = Vec::new();
	let mut root_matches = Vec::new();
	let mut directory_matches = Vec::new();
	for (name, config) in configs.iter() {
		let root = find_root_marker_in_launch_ancestry(program, cwd, &config.root_markers, program_kind);
		if root.is_some() {
			root_matches.push(name.to_string());
			if config.accepts_directory_program {
				directory_matches.push(name.to_string());
			}
		}
		if extensionless_rank(name).is_none() && root.is_none() {
			continue;
		}
		if let Some(adapter) = resolve_adapter_for_launch(name, configs, cwd, env, root.as_ref()) {
			available.push(adapter);
		}
	}

	if program_kind == LaunchProgramKind::Directory && !directory_matches.is_empty() {
		let directory_adapters: Vec<ResolvedAdapter> = available
			.iter()
			.filter(|adapter| adapter.accepts_directory_program && directory_matches.contains(&adapter.name))
			.cloned()
			.collect();
		if let Some(selected) = sort_adapters_for_launch(program, cwd, program_kind, directory_adapters)
			.into_iter()
			.next()
		{
			return LaunchAdapterSelection::Adapter(selected);
		}
		return unavailable_adapter(&directory_matches[0], configs);
	}

	let candidates: Vec<ResolvedAdapter> = if program_kind == LaunchProgramKind::Directory {
		let directory_adapters: Vec<ResolvedAdapter> =
			available.iter().filter(|adapter| adapter.accepts_directory_program).cloned().collect();
		if directory_adapters.is_empty() {
			available
		} else {
			directory_adapters
		}
	} else {
		available
	};
	if let Some(selected) = sort_adapters_for_launch(program, cwd, program_kind, candidates).into_iter().next() {
		return LaunchAdapterSelection::Adapter(selected);
	}
	match root_matches.first() {
		Some(name) => unavailable_adapter(name, configs),
		None => LaunchAdapterSelection::None,
	}
}

fn unavailable_adapter(name: &str, configs: &AdapterConfigs) -> LaunchAdapterSelection {
	match configs.get(name) {
		Some(config) => LaunchAdapterSelection::Unavailable {
			adapter_name: name.to_string(),
			command: config.command.clone(),
		},
		None => LaunchAdapterSelection::None,
	}
}

/// Pick a launch adapter. Explicit names win; otherwise ranking is extension,
/// root-marker, native debugger preference (`gdb`, `lldb-dap`), then name.
pub fn select_launch_adapter(
	program: &Path,
	cwd: &Path,
	env: &HashMap<String, String>,
	adapter_name: Option<&str>,
	program_kind: LaunchProgramKind,
) -> LaunchAdapterSelection {
	let configs = get_adapter_configs(cwd, env);
	if let Some(name) = adapter_name {
		let Some(config) = configs.get(name) else {
			return LaunchAdapterSelection::None;
		};
		let root = find_root_marker_in_launch_ancestry(program, cwd, &config.root_markers, program_kind);
		return match resolve_adapter_for_launch(name, &configs, cwd, env, root.as_ref()) {
			Some(adapter) => LaunchAdapterSelection::Adapter(adapter),
			None => LaunchAdapterSelection::Unavailable {
				adapter_name: name.to_string(),
				command: config.command.clone(),
			},
		};
	}
	select_automatic_launch_adapter(program, cwd, program_kind, &configs, env)
}

/// Attach selection: explicit name, `debugpy` when a remote port is given,
/// native debuggers in order, then the first resolvable adapter.
pub fn select_attach_adapter(
	cwd: &Path,
	env: &HashMap<String, String>,
	adapter_name: Option<&str>,
	port: Option<u16>,
) -> Option<ResolvedAdapter> {
	if let Some(name) = adapter_name {
		return resolve_adapter(name, cwd, env);
	}
	let available = get_available_adapters(cwd, env);
	if port.is_some() {
		if let Some(debugpy) = available.iter().find(|adapter| adapter.name == "debugpy") {
			return Some(debugpy.clone());
		}
	}
	for preferred in ["gdb", "lldb-dap"] {
		if let Some(found) = available.iter().find(|adapter| adapter.name == preferred) {
			return Some(found.clone());
		}
	}
	available.into_iter().next()
}

pub fn classify_program(program: &Path) -> LaunchProgramKind {
	match std::fs::metadata(program) {
		Ok(metadata) if metadata.is_dir() => LaunchProgramKind::Directory,
		Ok(_) => LaunchProgramKind::File,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => LaunchProgramKind::Missing,
		Err(_) => LaunchProgramKind::File,
	}
}

/// dlv interprets directories and `.go` files as packages, binaries as `exec`.
pub fn resolve_launch_overrides(
	adapter: &ResolvedAdapter,
	program: &Path,
	program_kind: LaunchProgramKind,
) -> Value {
	let mut overrides = Map::new();
	if adapter.name == "dlv" {
		let extension = program
			.extension()
			.map(|value| value.to_string_lossy().to_lowercase())
			.unwrap_or_default();
		if program_kind == LaunchProgramKind::Directory || extension == ".go" {
			overrides.insert("mode".to_string(), Value::String("debug".to_string()));
		} else if program_kind == LaunchProgramKind::File {
			overrides.insert("mode".to_string(), Value::String("exec".to_string()));
		}
	}
	Value::Object(overrides)
}

/// Human-readable `Installed adapters: ...` fallback list.
pub fn configured_adapter_names(cwd: &Path, env: &HashMap<String, String>) -> Vec<String> {
	let mut names: Vec<String> = get_available_adapters(cwd, env).into_iter().map(|adapter| adapter.name).collect();
	names.sort();
	names.dedup();
	names
}

/// Accurate missing-prerequisite message for a configured adapter.
pub fn adapter_unavailable_message(adapter_name: &str, command: &str, cwd: &Path, env: &HashMap<String, String>) -> String {
	let canonical_command = match adapter_name {
		"debugpy" => "python",
		"dlv" => "dlv",
		"rdbg" => "rdbg",
		"js-debug-adapter" => "node",
		_ => adapter_name,
	};
	if command != canonical_command {
		return format!(
			"adapter '{adapter_name}' is not available: configured command '{command}' did not resolve. Check the DAP adapter config for this workspace."
		);
	}
	if adapter_name == "debugpy" {
		return "adapter 'debugpy' is not available: python not found in PATH".to_string();
	}
	if adapter_name == "js-debug-adapter" {
		return "adapter 'js-debug-adapter' is not available: node and a local dapDebugServer.js are required (set JS_DEBUG_DAP_SERVER)".to_string();
	}
	let installed = configured_adapter_names(cwd, env);
	let names = if installed.is_empty() {
		"none".to_string()
	} else {
		installed.join(", ")
	};
	format!(
		"adapter '{adapter_name}' is not available: '{command}' did not resolve. Verify it with 'command -v {command}' or configure a DAP adapter path. Installed adapters: {names}"
	)
}

/// Resolve `${port}` placeholders for TCP adapters.
pub fn substitute_port(args: &[String], port: u16) -> Vec<String> {
	args.iter().map(|arg| arg.replace(DAP_PORT_ARGUMENT, &port.to_string())).collect()
}

/// Root-marker scan used by launch ranking; exposed for tests.
pub fn root_marker_dirs(program: &Path, cwd: &Path, markers: &[String], kind: LaunchProgramKind) -> Option<PathBuf> {
	find_root_marker_in_launch_ancestry(program, cwd, markers, kind)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn env_with(path: &str) -> HashMap<String, String> {
		let mut env = HashMap::new();
		env.insert("PATH".to_string(), path.to_string());
		env
	}

	#[test]
	fn embedded_defaults_cover_all_fourteen_adapters() {
		let configs = defaults();
		let names: Vec<&str> = configs.names();
		assert_eq!(names.len(), 14);
		for name in [
			"gdb",
			"lldb-dap",
			"codelldb",
			"debugpy",
			"dlv",
			"js-debug-adapter",
			"netcoredbg",
			"kotlin-debug-adapter",
			"rdbg",
			"php-debug-adapter",
			"bash-debug-adapter",
			"dart-debug-adapter",
			"flutter-debug-adapter",
			"elixir-ls-debugger",
		] {
			assert!(names.contains(&name), "missing adapter {name}");
		}
	}

	#[test]
	fn yaml_subset_parses_adapter_overlay() {
		let parsed = parse_yaml_subset(
			r#"
adapters:
  custom:
    command: sh
    args:
      - -c
      - echo
    fileTypes: [".txt"]
    launchDefaults:
      request: launch
      stopOnEntry: true
"#,
		)
		.expect("yaml parses");
		let custom = &parsed["adapters"]["custom"];
		assert_eq!(custom["command"], "sh");
		assert_eq!(custom["fileTypes"][0], ".txt");
		assert_eq!(custom["launchDefaults"]["stopOnEntry"], true);
	}

	#[test]
	fn adapter_unavailable_names_the_missing_command() {
		let cwd = std::env::temp_dir();
		let message = adapter_unavailable_message("gdb", "gdb", &cwd, &env_with(""));
		assert!(message.contains("command -v gdb"), "{message}");
		assert!(message.contains("not available"), "{message}");
	}

	#[test]
	fn port_placeholder_is_substituted() {
		let args = vec!["--port".to_string(), "${port}".to_string()];
		assert_eq!(substitute_port(&args, 4242), vec!["--port", "4242"]);
	}
}
