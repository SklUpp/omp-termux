//! Per-language scripts, argv, init snippets, and availability resolution.

use std::{
	collections::HashMap,
	io,
	path::{Path, PathBuf},
	sync::{LazyLock, Mutex},
	time::Duration,
};

use tokio_util::sync::CancellationToken;

use super::{
	env::{self, InterpreterRuntime},
	probe::{probe_candidates, CandidateProbe, ProbeCandidate},
	types::{EvalError, EvalLanguage},
	util,
};

pub(crate) const PY_RUNNER: &str = include_str!("runners/runner.py");
pub(crate) const PY_PRELUDE: &str = include_str!("runners/prelude.py");
pub(crate) const RB_RUNNER: &str = include_str!("runners/runner.rb");
pub(crate) const RB_PRELUDE: &str = include_str!("runners/prelude.rb");
pub(crate) const JL_RUNNER: &str = include_str!("runners/runner.jl");
pub(crate) const JL_PRELUDE: &str = include_str!("runners/prelude.jl");
pub(crate) const JS_RUNNER: &str = include_str!("runners/runner.js");

pub(crate) const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const JULIA_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
pub(crate) const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);
pub(crate) const INTERRUPT_ESCALATION: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy)]
pub(crate) struct ScriptAssets {
	pub runner: &'static str,
	pub prelude: &'static str,
	pub extension: &'static str,
}

pub(crate) fn assets(language: EvalLanguage) -> ScriptAssets {
	match language {
		EvalLanguage::Py => ScriptAssets { runner: PY_RUNNER, prelude: PY_PRELUDE, extension: "py" },
		EvalLanguage::Rb => ScriptAssets { runner: RB_RUNNER, prelude: RB_PRELUDE, extension: "rb" },
		EvalLanguage::Jl => ScriptAssets { runner: JL_RUNNER, prelude: JL_PRELUDE, extension: "jl" },
		EvalLanguage::Js => ScriptAssets { runner: JS_RUNNER, prelude: "", extension: "js" },
	}
}

pub(crate) fn startup_timeout(language: EvalLanguage) -> Duration {
	match language {
		EvalLanguage::Jl => JULIA_STARTUP_TIMEOUT,
		_ => STARTUP_TIMEOUT,
	}
}

fn temp_root() -> PathBuf {
	std::env::var_os("TMPDIR").map_or_else(std::env::temp_dir, PathBuf::from)
}

/// Stage a runner script under the temp dir, keyed by content hash.
pub(crate) fn stage_runner(language: EvalLanguage) -> io::Result<PathBuf> {
	let assets = assets(language);
	let hash = xxhash_rust::xxh64::xxh64(assets.runner.as_bytes(), 0);
	let dir = temp_root().join(format!("omp-{}-runner", language.token()));
	std::fs::create_dir_all(&dir)?;
	let path = dir.join(format!("runner-{hash:016x}.{}", assets.extension));
	if !path.is_file() {
		std::fs::write(&path, assets.runner)?;
	}
	Ok(path)
}

/// Spawn argv for one language, given the resolved interpreter and staged script.
pub(crate) fn argv(language: EvalLanguage, program: &Path, script: &Path) -> Vec<String> {
	let program = program.display().to_string();
	let script = script.display().to_string();
	match language {
		EvalLanguage::Py => vec![program, "-u".to_string(), script],
		EvalLanguage::Rb => vec![program, script],
		EvalLanguage::Jl => vec![
			program,
			"--startup-file=no".to_string(),
			"--history-file=no".to_string(),
			"--color=no".to_string(),
			"--project=@.".to_string(),
			script,
		],
		EvalLanguage::Js => vec![program, script],
	}
}

/// Runtime init cell executed once after spawn.
pub(crate) fn init_script(language: EvalLanguage, cwd: &Path) -> String {
	let literal = serde_json::to_string(&cwd.display().to_string()).unwrap_or_else(|_| "\".\"".to_string());
	match language {
		EvalLanguage::Py => format!(
			"import os, sys\n__omp_cwd = {literal}\nos.chdir(__omp_cwd)\n\
			 if __omp_cwd not in sys.path:\n    sys.path.insert(0, __omp_cwd)"
		),
		EvalLanguage::Rb => format!(
			"__omp_init_cwd = {literal}\nDir.chdir(__omp_init_cwd) rescue nil\n\
			 $LOAD_PATH.delete(__omp_init_cwd)\n$LOAD_PATH.unshift(__omp_init_cwd)"
		),
		EvalLanguage::Jl => {
			let encoded = util::base64_encode(cwd.display().to_string().as_bytes());
			format!(
				"__omp_init_cwd = String(Base64.base64decode(\"{encoded}\"))\n\
				 try cd(__omp_init_cwd) catch; end\n\
				 if !(__omp_init_cwd in LOAD_PATH); pushfirst!(LOAD_PATH, __omp_init_cwd); end"
			)
		},
		EvalLanguage::Js => format!("process.chdir({literal});"),
	}
}

pub(crate) fn exit_payload(language: EvalLanguage) -> &'static str {
	match language {
		EvalLanguage::Jl => "exit",
		_ => "{\"type\":\"exit\"}",
	}
}

/// Message shown when the interpreter is genuinely absent.
pub(crate) fn missing_runtime_message(language: EvalLanguage) -> String {
	match language {
		EvalLanguage::Py => "Python executable not found on PATH".to_string(),
		EvalLanguage::Js => {
			"JavaScript runtime not found on PATH. eval js requires Bun (https://bun.sh/).".to_string()
		},
		EvalLanguage::Rb => "Ruby executable not found on PATH".to_string(),
		EvalLanguage::Jl => {
			"Julia executable not found on PATH. Please install Julia (https://julialang.org/).".to_string()
		},
	}
}

static AVAILABILITY: LazyLock<Mutex<HashMap<String, InterpreterRuntime>>> =
	LazyLock::new(|| Mutex::new(HashMap::new()));

fn cache_key(language: EvalLanguage, cwd: &Path, interpreter: Option<&str>) -> String {
	format!(
		"{}\0{}\0{}",
		language.token(),
		cwd.display(),
		interpreter.unwrap_or("")
	)
}

/// Candidate runtimes, filtered env included, in priority order.
fn candidates(
	language: EvalLanguage,
	cwd: &Path,
	base_env: &HashMap<String, String>,
	interpreter: Option<&str>,
) -> Vec<InterpreterRuntime> {
	let base = env::filter_env(language, base_env);
	match language {
		EvalLanguage::Py => env::enumerate_python(cwd, &base, interpreter),
		EvalLanguage::Rb => env::enumerate_binary(cwd, &base, "ruby", interpreter),
		EvalLanguage::Jl => env::enumerate_binary(cwd, &base, "julia", interpreter),
		// Bun only. A Node fallback would change the kernel semantics; refuse
		// instead of silently substituting a different runtime.
		EvalLanguage::Js => env::enumerate_binary(cwd, &base, "bun", None),
	}
}

fn probe_command(language: EvalLanguage, program: &Path) -> Vec<String> {
	let program = program.display().to_string();
	match language {
		EvalLanguage::Py => vec![program, "-c".to_string(), "import sys;sys.exit(0)".to_string()],
		EvalLanguage::Rb => vec![program, "-e".to_string(), "exit 0".to_string()],
		EvalLanguage::Jl => vec![
			program,
			"--startup-file=no".to_string(),
			"--history-file=no".to_string(),
			"--color=no".to_string(),
			"-e".to_string(),
			"exit(0)".to_string(),
		],
		EvalLanguage::Js => vec![program, "--version".to_string()],
	}
}

/// Resolve and probe a runtime, caching successes per (language, cwd,
/// interpreter). Failures are never cached so a mid-session install is picked up.
pub(crate) async fn resolve_runtime(
	language: EvalLanguage,
	cwd: &Path,
	base_env: &HashMap<String, String>,
	interpreter: Option<&str>,
	cancellation: &CancellationToken,
	timeout_ms: Option<u64>,
) -> Result<InterpreterRuntime, EvalError> {
	let key = cache_key(language, cwd, interpreter);
	if let Some(runtime) = AVAILABILITY
		.lock()
		.unwrap_or_else(|poison| poison.into_inner())
		.get(&key)
		.cloned()
	{
		return Ok(runtime);
	}
	let candidates = candidates(language, cwd, base_env, interpreter);
	if candidates.is_empty() {
		return Err(EvalError::Unavailable(missing_runtime_message(language)));
	}
	let probes: Vec<ProbeCandidate> = candidates
		.iter()
		.map(|runtime| ProbeCandidate {
			command: probe_command(language, &runtime.program),
			env: runtime.env.clone(),
			label: runtime.label.clone(),
		})
		.collect();
	match probe_candidates(&probes, cwd, cancellation, timeout_ms).await {
		CandidateProbe::Ok(index) => {
			let runtime = candidates[index].clone();
			AVAILABILITY
				.lock()
				.unwrap_or_else(|poison| poison.into_inner())
				.insert(key, runtime.clone());
			Ok(runtime)
		},
		CandidateProbe::Aborted => Err(EvalError::Aborted(format!(
			"{} availability probe was cancelled",
			language.display_name()
		))),
		CandidateProbe::Failed(failures) => Err(EvalError::Unavailable(format!(
			"No working {} interpreter found. Tried: {}",
			language.display_name(),
			failures.join("; ")
		))),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn runners_are_staged_and_reused() {
		let first = stage_runner(EvalLanguage::Py).expect("stage py");
		let second = stage_runner(EvalLanguage::Py).expect("stage py again");
		assert_eq!(first, second);
		assert!(first.is_file());
	}

	#[test]
	fn julia_init_base64_round_trips_path() {
		let script = init_script(EvalLanguage::Jl, Path::new("/tmp/a b"));
		assert!(script.contains(&util::base64_encode(b"/tmp/a b")));
	}

	#[tokio::test]
	async fn missing_julia_has_actionable_prerequisite() {
		let cancellation = CancellationToken::new();
		let base = HashMap::from([("PATH".to_string(), "/nonexistent".to_string())]);
		let error = resolve_runtime(
			EvalLanguage::Jl,
			Path::new("."),
			&base,
			None,
			&cancellation,
			Some(1000),
		)
		.await
		.expect_err("no julia");
		assert!(error.to_string().contains("install Julia"));
	}
}
