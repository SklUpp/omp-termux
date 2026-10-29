//! Hidden LSP mux worker entry (`omp __omp_worker_lsp_mux`).
//!
//! Startup selector contract:
//! - invoked as `omp __omp_worker_lsp_mux` (no additional arguments);
//! - environment [`SOCKET_ENV`] carries the socket endpoint to bind, and
//!   [`PROJECT_DIR_ENV`] the canonical project directory the mux serves;
//! - stdout receives exactly one readiness line,
//!   `omp lsp mux listening on <endpoint>`, once the socket accepts links;
//! - the worker exits after 15 idle minutes, or on `SIGTERM`/`SIGINT`.
//!
//! The CLI dispatches this before clap/config/session boot (the same seam as
//! the daemon broker worker), and the daemon broker owns its lifecycle.

use std::{
	ffi::{OsStr, OsString},
	io::Write,
	path::PathBuf,
	sync::Arc,
};

use tokio::sync::oneshot;

use super::{PROJECT_DIR_ENV, SOCKET_ENV, WORKER_ARG, ready_banner, server::LspMuxServer};

/// True when the process was launched as the hidden LSP mux worker.
#[must_use]
pub fn is_worker_invocation() -> bool {
	std::env::args_os()
		.nth(1)
		.is_some_and(|arg| arg.as_os_str() == OsStr::new(WORKER_ARG))
}

/// Serve the mux described by the worker environment.
///
/// `args` are the CLI arguments *after* the selector; the contract takes none.
pub async fn run_mux_worker(args: &[OsString]) -> Result<(), String> {
	if let Some(extra) = args.first() {
		return Err(format!(
			"LSP mux worker takes no arguments, got '{}'",
			extra.to_string_lossy()
		));
	}
	let endpoint = required_path(SOCKET_ENV)?;
	// Validated but not used for binding: the endpoint is authoritative.
	let _project = required_path(PROJECT_DIR_ENV)?;
	let server = Arc::new(LspMuxServer::new());
	let (idle_tx, idle_rx) = oneshot::channel();
	server.set_idle_notify(idle_tx);
	server.listen(&endpoint).await?;
	let banner = ready_banner(&endpoint);
	println!("{banner}");
	let _ = std::io::stdout().flush();
	tokio::select! {
		_ = idle_rx => {},
		_ = shutdown_signal() => {},
	}
	server.shutdown().await;
	Ok(())
}

/// Blocking wrapper used by the CLI entry point.
pub fn run_mux_worker_blocking(args: &[OsString]) -> Result<(), String> {
	let runtime = tokio::runtime::Builder::new_multi_thread()
		.worker_threads(2)
		.enable_all()
		.thread_name("omp-lsp-mux")
		.build()
		.map_err(|error| format!("cannot start the LSP mux runtime: {error}"))?;
	runtime.block_on(run_mux_worker(args))
}

fn required_path(key: &str) -> Result<PathBuf, String> {
	std::env::var_os(key)
		.map(PathBuf::from)
		.filter(|path| !path.as_os_str().is_empty())
		.ok_or_else(|| format!("{key} is required for the LSP mux worker"))
}

async fn shutdown_signal() {
	#[cfg(unix)]
	{
		use tokio::signal::unix::{SignalKind, signal};
		let mut terminate = match signal(SignalKind::terminate()) {
			Ok(signal) => signal,
			Err(_) => {
				let _ = tokio::signal::ctrl_c().await;
				return;
			},
		};
		tokio::select! {
			_ = terminate.recv() => {},
			_ = tokio::signal::ctrl_c() => {},
		}
	}
	#[cfg(not(unix))]
	{
		let _ = tokio::signal::ctrl_c().await;
	}
}
