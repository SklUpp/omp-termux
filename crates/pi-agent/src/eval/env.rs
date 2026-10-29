//! Environment filtering, interpreter path resolution, and PATH discovery.
//!
//! Mirrors the upstream per-language allowlist/denylist/prefix filter. Secret
//! shaped names never cross into a cell even when they match a broad prefix.

use std::{
	collections::{HashMap, HashSet},
	path::{Path, PathBuf},
	sync::LazyLock,
};

use regex::Regex;

use super::types::EvalLanguage;

/// Names that never leak into eval cells, independent of allow prefixes.
pub const SECRET_PATTERN: &str =
	r"(?i)API[_-]?KEY|APIKEY|SECRET|TOKEN|PASSWORD|PASSWD|CREDENTIAL|ACCESS[_-]?KEY|PRIVATE[_-]?KEY";

/// Provider/session credentials explicit denylisted by every backend.
const SHARED_DENY: &[&str] = &[
	"OPENAI_API_KEY",
	"ANTHROPIC_API_KEY",
	"GOOGLE_API_KEY",
	"GEMINI_API_KEY",
	"OPENROUTER_API_KEY",
	"PERPLEXITY_API_KEY",
	"PERPLEXITY_COOKIES",
	"EXA_API_KEY",
	"AZURE_OPENAI_API_KEY",
	"MISTRAL_API_KEY",
	"PI_API_KEY",
	"PI_TOKEN",
	"PI_PASSWORD",
	"PI_SESSION",
	"PI_TOOL_BRIDGE_TOKEN",
];

const COMMON_ALLOW: &[&str] = &[
	"PATH",
	"HOME",
	"USER",
	"USERNAME",
	"LOGNAME",
	"SHELL",
	"LANG",
	"LC_ALL",
	"LC_CTYPE",
	"LC_MESSAGES",
	"TERM",
	"TERM_PROGRAM",
	"TERM_PROGRAM_VERSION",
	"TMPDIR",
	"TEMP",
	"TMP",
	"XDG_CACHE_HOME",
	"XDG_CONFIG_HOME",
	"XDG_DATA_HOME",
	"XDG_RUNTIME_DIR",
	"SSH_AUTH_SOCK",
	"SSH_AGENT_PID",
	"SSH_CONNECTION",
	"SSH_CLIENT",
	"SSH_TTY",
	"LD_LIBRARY_PATH",
	"DYLD_LIBRARY_PATH",
	"TZ",
	"SYSTEMROOT",
	"WINDIR",
	"COMSPEC",
	"PATHEXT",
];

const PY_ALLOW: &[&str] = &["CONDA_PREFIX", "CONDA_DEFAULT_ENV", "VIRTUAL_ENV", "PYTHONPATH"];
const JL_ALLOW: &[&str] = &["DISPLAY", "XAUTHORITY"];
// `LD_PRELOAD` is loader-critical for Bun on Termux, where the installed
// compat shim is injected by the user's ambient environment; it stays JS-only.
const JS_ALLOW: &[&str] = &["BUN_INSTALL", "NODE_OPTIONS", "LD_PRELOAD"];

static SECRET_REGEX: LazyLock<Regex> =
	LazyLock::new(|| Regex::new(SECRET_PATTERN).expect("secret pattern is valid"));

fn secret_regex() -> &'static Regex {
	&SECRET_REGEX
}

fn allow_for(language: EvalLanguage) -> (&'static [&'static str], &'static [&'static str]) {
	match language {
		EvalLanguage::Py => (PY_ALLOW, &["LC_", "XDG_", "PI_"]),
		EvalLanguage::Rb => (
			&[],
			&["LC_", "XDG_", "PI_", "GEM_", "BUNDLE", "RBENV_", "RUBY", "CHRUBY_", "ASDF_"],
		),
		EvalLanguage::Jl => (JL_ALLOW, &["LC_", "XDG_", "PI_", "JULIA_", "OPENBLAS_", "MKL_"]),
		EvalLanguage::Js => (JS_ALLOW, &["LC_", "XDG_", "PI_", "BUN_", "NODE_"]),
	}
}

/// Filter an ambient environment for one runtime.
///
/// Denylist wins, allowlist preserves the original key spelling (except a
/// normalized `PATH`), then `SECRET_PATTERN` rejects secret-shaped names even
/// under an allow prefix, and finally allow prefixes admit version-manager
/// layout variables.
pub fn filter_env(language: EvalLanguage, env: &HashMap<String, String>) -> HashMap<String, String> {
	let (extra_allow, prefixes) = allow_for(language);
	let allow: HashSet<&str> = COMMON_ALLOW
		.iter()
		.chain(extra_allow)
		.copied()
		.collect();
	let deny: HashSet<&str> = SHARED_DENY.iter().copied().collect();
	let regex = secret_regex();

	let mut filtered = HashMap::new();
	for (key, value) in env {
		if deny.contains(key.as_str()) {
			continue;
		}
		if key == "PATH" || allow.contains(key.as_str()) {
			filtered.insert(key.clone(), value.clone());
			continue;
		}
		if regex.is_match(key) {
			continue;
		}
		if prefixes.iter().any(|prefix| key.starts_with(prefix)) {
			filtered.insert(key.clone(), value.clone());
		}
	}
	filtered
}

/// Expand `~`/`~/…` and resolve relative paths against `cwd`.
pub fn resolve_explicit_path(interpreter: &str, cwd: &Path, env: &HashMap<String, String>) -> PathBuf {
	let expanded = if interpreter == "~" {
		env.get("HOME").map_or_else(|| PathBuf::from(interpreter), PathBuf::from)
	} else if let Some(rest) = interpreter.strip_prefix("~/") {
		env.get("HOME").map_or_else(|| PathBuf::from(interpreter), |home| Path::new(home).join(rest))
	} else {
		PathBuf::from(interpreter)
	};
	if expanded.is_absolute() {
		expanded
	} else {
		cwd.join(expanded)
	}
}

/// Every executable `binary` on the filtered `PATH`, in priority order.
pub fn which_all(binary: &str, env: &HashMap<String, String>) -> Vec<PathBuf> {
	let Some(path) = env.get("PATH") else {
		return Vec::new();
	};
	let mut seen = HashSet::new();
	path.split(':')
		.filter(|entry| !entry.is_empty())
		.map(|entry| Path::new(entry).join(binary))
		.filter(|candidate| is_executable(candidate))
		.filter(|candidate| seen.insert(candidate.canonicalize().unwrap_or_else(|_| candidate.clone())))
		.collect()
}

/// First executable named `binary` in the filtered `PATH`.
pub fn which(binary: &str, env: &HashMap<String, String>) -> Option<PathBuf> {
	which_all(binary, env).into_iter().next()
}

pub(crate) fn is_executable(path: &Path) -> bool {
	let Ok(metadata) = std::fs::metadata(path) else {
		return false;
	};
	if !metadata.is_file() {
		return false;
	}
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt as _;
		metadata.permissions().mode() & 0o111 != 0
	}
	#[cfg(not(unix))]
	{
		true
	}
}

/// Candidate runtime for one language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterpreterRuntime {
	pub program: PathBuf,
	pub env: HashMap<String, String>,
	/// Label used in prerequisite errors.
	pub label: String,
	pub venv: Option<PathBuf>,
}

impl InterpreterRuntime {
	pub fn new(program: PathBuf, env: HashMap<String, String>) -> Self {
		let label = program.display().to_string();
		Self { program, env, label, venv: None }
	}
}

fn apply_venv_env(base: &HashMap<String, String>, venv: &Path, bin_dir: &Path) -> HashMap<String, String> {
	let mut env = base.clone();
	env.insert("VIRTUAL_ENV".to_string(), venv.display().to_string());
	let bin = bin_dir.display().to_string();
	let path = match env.get("PATH") {
		Some(current) if !current.is_empty() => format!("{bin}:{current}"),
		_ => bin,
	};
	env.insert("PATH".to_string(), path);
	env
}

fn venv_bin(venv: &Path) -> PathBuf {
	#[cfg(windows)]
	{
		venv.join("Scripts")
	}
	#[cfg(not(windows))]
	{
		venv.join("bin")
	}
}

fn python_binary(bin_dir: &Path) -> PathBuf {
	#[cfg(windows)]
	{
		bin_dir.join("python.exe")
	}
	#[cfg(not(windows))]
	{
		bin_dir.join("python")
	}
}

/// Explicit interpreter is the only candidate; venv activation is detected from
/// `pyvenv.cfg` above its bin directory.
pub fn explicit_python(interpreter: &str, cwd: &Path, base: &HashMap<String, String>) -> InterpreterRuntime {
	let program = resolve_explicit_path(interpreter, cwd, base);
	let bin_dir = program.parent().unwrap_or(Path::new("."));
	let venv = bin_dir.parent().and_then(|parent| {
		parent.join("pyvenv.cfg").is_file().then(|| parent.to_path_buf())
	});
	match venv {
		Some(venv) => {
			let env = apply_venv_env(base, &venv, bin_dir);
			let label = program.display().to_string();
			InterpreterRuntime { program, env, label, venv: Some(venv) }
		},
		None => InterpreterRuntime::new(program, base.clone()),
	}
}

/// Interpreter candidates in upstream priority order: active/project venv,
/// managed `~/.omp/python-env`, then `python` and `python3` on `PATH`.
pub fn enumerate_python(cwd: &Path, base: &HashMap<String, String>, interpreter: Option<&str>) -> Vec<InterpreterRuntime> {
	if let Some(interpreter) = interpreter {
		return vec![explicit_python(interpreter, cwd, base)];
	}
	let mut candidates = Vec::new();
	let mut seen = HashSet::new();
	let mut push = |runtime: InterpreterRuntime| {
		let key = runtime.program.clone();
		if seen.insert(key) {
			candidates.push(runtime);
		}
	};
	let venv = base.get("VIRTUAL_ENV").map(PathBuf::from).or_else(|| {
		base.get("CONDA_PREFIX").map(PathBuf::from).or_else(|| {
			[cwd.join(".venv"), cwd.join("venv")]
				.into_iter()
				.find(|candidate| candidate.is_dir())
		})
	});
	if let Some(venv) = venv {
		let bin = venv_bin(&venv);
		let program = python_binary(&bin);
		if program.is_file() {
			let env = apply_venv_env(base, &venv, &bin);
			let label = program.display().to_string();
			push(InterpreterRuntime { program, env, label, venv: Some(venv) });
		}
	}
	if let Some(home) = base.get("HOME") {
		let venv = Path::new(home).join(".omp").join("python-env");
		let bin = venv_bin(&venv);
		let program = python_binary(&bin);
		if program.is_file() {
			let env = apply_venv_env(base, &venv, &bin);
			let label = program.display().to_string();
			push(InterpreterRuntime { program, env, label, venv: Some(venv) });
		}
	}
	for binary in ["python", "python3"] {
		if let Some(program) = which(binary, base) {
			push(InterpreterRuntime::new(program, base.clone()));
		}
	}
	candidates
}

pub fn enumerate_binary(
	cwd: &Path,
	base: &HashMap<String, String>,
	binary: &str,
	interpreter: Option<&str>,
) -> Vec<InterpreterRuntime> {
	if let Some(interpreter) = interpreter {
		return vec![InterpreterRuntime::new(
			resolve_explicit_path(interpreter, cwd, base),
			base.clone(),
		)];
	}
	which(binary, base)
		.map(|program| InterpreterRuntime::new(program, base.clone()))
		.into_iter()
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
		pairs.iter().map(|(key, value)| ((*key).to_string(), (*value).to_string())).collect()
	}

	#[test]
	fn secrets_never_cross_even_under_pi_prefix() {
		let filtered = filter_env(
			EvalLanguage::Py,
			&env(&[
				("PATH", "/bin"),
				("PI_API_KEY", "secret"),
				("PI_TOOL_BRIDGE_TOKEN", "secret"),
				("PI_DEBUG", "1"),
				("OPENAI_API_KEY", "secret"),
				("CUSTOM_TOKEN", "secret"),
				("VIRTUAL_ENV", "/venv"),
			]),
		);
		assert_eq!(filtered.get("PATH").map(String::as_str), Some("/bin"));
		assert_eq!(filtered.get("PI_DEBUG").map(String::as_str), Some("1"));
		assert_eq!(filtered.get("VIRTUAL_ENV").map(String::as_str), Some("/venv"));
		assert!(!filtered.contains_key("PI_API_KEY"));
		assert!(!filtered.contains_key("PI_TOOL_BRIDGE_TOKEN"));
		assert!(!filtered.contains_key("OPENAI_API_KEY"));
		assert!(!filtered.contains_key("CUSTOM_TOKEN"));
	}

	#[test]
	fn js_keeps_loader_preload_and_other_languages_drop_it() {
		let base = env(&[
			("PATH", "/bin"),
			("LD_PRELOAD", "/lib/libomp-compat.so"),
			("BUN_INSTALL", "/bun"),
			("BUN_TOKEN", "secret"),
			("OPENAI_API_KEY", "secret"),
		]);
		let js = filter_env(EvalLanguage::Js, &base);
		assert_eq!(js.get("LD_PRELOAD").map(String::as_str), Some("/lib/libomp-compat.so"));
		assert_eq!(js.get("BUN_INSTALL").map(String::as_str), Some("/bun"));
		// Secret filtering still applies to the JS allow prefixes.
		assert!(!js.contains_key("BUN_TOKEN"));
		assert!(!js.contains_key("OPENAI_API_KEY"));
		for language in [EvalLanguage::Py, EvalLanguage::Rb, EvalLanguage::Jl] {
			assert!(
				!filter_env(language, &base).contains_key("LD_PRELOAD"),
				"{language:?} must not inherit the JS loader hook"
			);
		}
	}

	#[test]
	fn ruby_reaches_gem_layout_prefixes() {
		let filtered = filter_env(
			EvalLanguage::Rb,
			&env(&[("GEM_HOME", "/gems"), ("RUBY_VERSION", "3.3"), ("HOME", "/home/u")]),
		);
		assert_eq!(filtered.get("GEM_HOME").map(String::as_str), Some("/gems"));
		assert_eq!(filtered.get("RUBY_VERSION").map(String::as_str), Some("3.3"));
	}

	#[test]
	fn explicit_path_expands_home_and_relative() {
		let base = env(&[("HOME", "/home/u")]);
		assert_eq!(resolve_explicit_path("~/bin/python", Path::new("/work"), &base), PathBuf::from("/home/u/bin/python"));
		assert_eq!(resolve_explicit_path("./py", Path::new("/work"), &base), PathBuf::from("/work/./py"));
	}

	#[cfg(unix)]
	#[test]
	fn which_all_keeps_path_order_and_skips_non_executables() {
		use std::os::unix::fs::PermissionsExt as _;

		let root = std::env::temp_dir().join(format!("omp-which-{}", crate::eval::util::random_token()));
		let first = root.join("first");
		let second = root.join("second");
		let empty = root.join("empty");
		std::fs::create_dir_all(&second).expect("second dir");
		std::fs::create_dir_all(&first).expect("first dir");
		std::fs::create_dir_all(&empty).expect("empty dir");
		std::fs::write(first.join("julia"), "#!/bin/sh\nexit 0\n").expect("first julia");
		std::fs::set_permissions(first.join("julia"), std::fs::Permissions::from_mode(0o755))
			.expect("chmod");
		std::fs::write(second.join("julia"), "#!/bin/sh\nexit 0\n").expect("second julia");
		std::fs::set_permissions(second.join("julia"), std::fs::Permissions::from_mode(0o644))
			.expect("chmod");
		let search = format!("{}:{}:{}", empty.display(), first.display(), second.display());
		let path = env(&[("PATH", search.as_str())]);
		assert_eq!(which_all("julia", &path), vec![first.join("julia")]);
		assert_eq!(which("julia", &path), Some(first.join("julia")));
		let _ = std::fs::remove_dir_all(&root);
	}
}
