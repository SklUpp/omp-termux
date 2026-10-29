//! Native LSP stack for the Rust agent.
//!
//! Port of `packages/coding-agent/src/lsp/*`. The CLI mounts [`LspTool`] behind
//! `lsp.enabled`; `lsp.shared` selects [`LspSession`]'s broker transport.
//!
//! Ownership is explicit because the client registry is process-global: every
//! acquisition through [`get_or_create_client_for`] records the calling
//! [`LspSession`], and only the last owner to release closes the connection.
//! Root turns share the runner's session, children own theirs, and a shared mux
//! server is never killed — the broker answers `shutdown` and `exit` closes
//! just that link. The mux daemon is the hidden CLI worker
//! `omp __omp_worker_lsp_mux` (see [`mux::worker`]).
//!
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
	get_active_clients, get_config, get_or_create_client, get_or_create_client_for, shutdown_all,
	LspClient, LspError, LspSession,
};
pub use config::load_config;
pub use mux::worker::{run_mux_worker, run_mux_worker_blocking};
pub use tool::LspTool;
pub use types::{is_readonly_action as is_readonly_lsp_action, LSP_READONLY_ACTIONS};
