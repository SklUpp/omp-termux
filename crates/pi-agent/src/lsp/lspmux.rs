//! Optional external `lspmux` wrapper, ported from
//! `packages/coding-agent/src/lsp/lspmux.ts`.
//!
//! Detection uses `lspmux status` and wrapping uses `lspmux client`. When an
//! external wrapper handles the command it takes precedence over the native
//! broker mux (`super::mux`), so this module reports `shared = true` only when
//! the server is handed to a running lspmux daemon.

use std::{
	path::{Path, PathBuf},
	sync::LazyLock,
	time::{Duration, Instant},
};

use tokio::sync::Mutex;

const STATE_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const LIVENESS_TIMEOUT: Duration = Duration::from_millis(1_000);

#[derive(Debug, Clone, Default)]
pub struct LspmuxState {
	pub available: bool,
	pub running: bool,
	pub binary_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct LspmuxWrappedCommand {
	pub command: String,
	pub args: Vec<String>,
	pub env: Vec<(String, String)>,
	pub shared: bool,
}

static CACHE: LazyLock<Mutex<Option<(Instant, LspmuxState)>>> = LazyLock::new(|| Mutex::new(None));

fn env_flag(name: &str) -> bool {
	std::env::var(name).is_ok_and(|value| {
		let value = value.to_ascii_lowercase();
		matches!(value.as_str(), "1" | "true" | "yes" | "on")
	})
}

fn which(command: &str) -> Option<PathBuf> {
	let path = std::env::var_os("PATH")?;
	for dir in std::env::split_paths(&path) {
		let candidate = dir.join(command);
		if is_executable(&candidate) {
			return Some(candidate);
		}
	}
	None
}

fn is_executable(path: &Path) -> bool {
	if !path.is_file() {
		return false;
	}
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		std::fs::metadata(path)
			.map(|metadata| metadata.permissions().mode() & 0o111 != 0)
			.unwrap_or(false)
	}
	#[cfg(not(unix))]
	{
		true
	}
}

pub async fn detect_lspmux() -> LspmuxState {
	let mut cache = CACHE.lock().await;
	if let Some((at, state)) = cache.as_ref() {
		if at.elapsed() < STATE_CACHE_TTL {
			return state.clone();
		}
	}
	if env_flag("PI_DISABLE_LSPMUX") {
		let state = LspmuxState::default();
		*cache = Some((Instant::now(), state.clone()));
		return state;
	}
	let Some(binary_path) = which("lspmux") else {
		let state = LspmuxState::default();
		*cache = Some((Instant::now(), state.clone()));
		return state;
	};
	let running = check_server_running(&binary_path).await;
	let state = LspmuxState { available: true, running, binary_path: Some(binary_path) };
	*cache = Some((Instant::now(), state.clone()));
	state
}

async fn check_server_running(binary_path: &Path) -> bool {
	let mut command = tokio::process::Command::new(binary_path);
	command
		.arg("status")
		.stdin(std::process::Stdio::null())
		.stdout(std::process::Stdio::null())
		.stderr(std::process::Stdio::null())
		.kill_on_drop(true);
	#[cfg(unix)]
	command.process_group(0);
	let Ok(mut child) = command.spawn() else {
		return false;
	};
	match tokio::time::timeout(LIVENESS_TIMEOUT, child.wait()).await {
		Ok(Ok(status)) => status.success(),
		_ => {
			let _ = child.start_kill();
			false
		}
	}
}

#[must_use]
pub fn is_lspmux_supported(command: &str) -> bool {
	let base = command.rsplit('/').next().unwrap_or(command);
	base == "rust-analyzer"
}

#[must_use]
pub fn wrap_with_lspmux(
	original_command: &str,
	original_args: &[String],
	state: &LspmuxState,
) -> LspmuxWrappedCommand {
	let passthrough = || LspmuxWrappedCommand {
		command: original_command.to_string(),
		args: original_args.to_vec(),
		env: Vec::new(),
		shared: false,
	};
	let (true, Some(binary_path)) = (state.available && state.running, state.binary_path.as_ref()) else {
		return passthrough();
	};
	if !is_lspmux_supported(original_command) {
		return passthrough();
	}
	let base = original_command.rsplit('/').next().unwrap_or(original_command);
	let default_rust_analyzer = base == "rust-analyzer" && original_command == "rust-analyzer";
	if default_rust_analyzer && original_args.is_empty() {
		return LspmuxWrappedCommand {
			command: binary_path.to_string_lossy().into_owned(),
			args: Vec::new(),
			env: Vec::new(),
			shared: true,
		};
	}
	let mut args = vec!["client".to_string()];
	if !original_args.is_empty() {
		args.push("--".to_string());
		args.extend(original_args.iter().cloned());
	}
	LspmuxWrappedCommand {
		command: binary_path.to_string_lossy().into_owned(),
		args,
		env: vec![("LSPMUX_SERVER".to_string(), original_command.to_string())],
		shared: true,
	}
}

pub async fn get_lspmux_command(command: &str, args: &[String]) -> LspmuxWrappedCommand {
	let state = detect_lspmux().await;
	wrap_with_lspmux(command, args, &state)
}
