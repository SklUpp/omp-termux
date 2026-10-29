//! Native LSP stack for the Rust agent.
//!
//! Module seam for parent integration (already declared as `pub mod lsp;`):
//! - register `Arc::new(crate::lsp::LspTool::with_shared(shared))` behind the
//!   existing `lsp.enabled` gate in the tool registry
//! - call `crate::lsp::client::get_config(cwd)` for cached configuration and
//!   `crate::lsp::client::shutdown_all()` on session teardown
//! - map `lsp.shared` to `LspTool::with_shared(bool)`; the mux daemon itself is
//!   the hidden CLI worker `omp __omp_worker_lsp_mux` (see [`mux::worker`])
//!
//! The module tree can be compiled and reviewed on its own; only the `lib.rs`
//! declaration, the tool registration, and the CLI worker hunk live outside it.
//! See `docs/tools/lsp.md`.

pub mod client;
pub mod config;
pub mod diagnostics;
pub mod edits;
pub mod editorconfig;
pub mod framing;
pub mod linter;
pub mod lspmux;
pub mod mux;
pub mod tool;
pub mod types;
pub mod utils;
pub mod workspace_diagnostics;
pub mod yaml;

pub use client::{
	get_active_clients, get_config, get_or_create_client, shutdown_all, LspClient, LspError,
};
pub use config::load_config;
pub use mux::worker::{run_mux_worker, run_mux_worker_blocking};
pub use tool::LspTool;
