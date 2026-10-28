//! Hidden daemon broker worker entry (`omp __omp_worker_daemon_broker`).
//!
//! The worker is spawned by `pi_agent::process_broker::client` with a private
//! project scope in its environment and no inherited stdio. This module is the
//! CLI-owned startup seam: it parses those variables *before* any thread or
//! session state exists, removes them so child processes never inherit worker
//! wiring, and serves the broker on a small runtime.
//!
//! Anchors: `packages/coding-agent/src/cli.ts` worker dispatch and
//! `packages/coding-agent/src/subprocess/worker-client.ts`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use pi_agent::process_broker::{
	broker,
	protocol::{self, DEFAULT_IDLE_GRACE_MS, IDLE_GRACE_ENV, PROJECT_DIR_ENV, RUNTIME_DIR_ENV, WORKER_ARG},
};

pub use pi_agent::process_broker::WORKER_ARG as SELECTOR;

/// True when the process was launched as the hidden broker worker.
#[must_use]
pub fn is_worker_invocation() -> bool {
	std::env::args_os().nth(1).is_some_and(|arg| arg.as_os_str() == std::ffi::OsStr::new(WORKER_ARG))
}

/// Publish the CLI's exact profile-aware runtime root to `pi-agent`, so the
/// in-process broker client and the CLI resolve the same scope directory.
///
/// # Safety
///
/// Called from the single-threaded CLI bootstrap before a runtime exists and
/// before any other thread could read the environment.
pub fn share_runtime_root(agent_dir: &Path) {
	let root = agent_dir.join("run").join("daemons");
	// SAFETY: see above; this is the only mutation of the process environment,
	// on the main thread, before the tokio runtime is built.
	unsafe { std::env::set_var(protocol::RUNTIME_ROOT_ENV, root) };
}

fn take_env(key: &str) -> Option<OsString> {
	let value = std::env::var_os(key);
	// SAFETY: the worker entry runs before the runtime and any other thread;
	// no reader can observe the environment concurrently.
	unsafe { std::env::remove_var(key) };
	value
}

fn required_path(key: &str) -> Result<PathBuf, String> {
	take_env(key)
		.filter(|value| !value.is_empty())
		.map(PathBuf::from)
		.ok_or_else(|| format!("{key} is required for the daemon broker worker"))
}

/// Serve the broker for the scope described by the worker environment.
///
/// Returns only when the broker shuts down; every failure is a startup error.
pub fn run_worker() -> Result<(), String> {
	let project_dir = required_path(PROJECT_DIR_ENV)?;
	let runtime_dir = required_path(RUNTIME_DIR_ENV)?;
	let idle_grace = take_env(IDLE_GRACE_ENV)
		.and_then(|raw| raw.to_string_lossy().trim().parse::<u64>().ok())
		.unwrap_or(DEFAULT_IDLE_GRACE_MS);
	// Logical identity for long-root checkpoints: capture before removing the
	// worker-only wiring so it never leaks into children.
	let logical_root = std::env::var_os(protocol::RUNTIME_ROOT_ENV)
		.map(PathBuf::from)
		.filter(|path| !path.as_os_str().is_empty())
		.unwrap_or_else(protocol::runtime_root);
	let _ = take_env(protocol::RUNTIME_ROOT_ENV);
	protocol::verify_scope(&runtime_dir, &logical_root, &project_dir).map_err(|err| {
		format!("daemon broker scope verification failed for {}: {err}", runtime_dir.display())
	})?;

	let runtime = tokio::runtime::Builder::new_multi_thread()
		.worker_threads(2)
		.enable_all()
		.thread_name("omp-broker")
		.build()
		.map_err(|err| format!("cannot start the daemon broker runtime: {err}"))?;
	runtime.block_on(broker::start(project_dir, runtime_dir, Duration::from_millis(idle_grace)))
}
