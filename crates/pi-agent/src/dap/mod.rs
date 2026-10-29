//! Native DAP debug stack for the Rust agent.
//!
//! Port of `packages/coding-agent/src/dap/*` plus the `debug` tool. The
//! Content-Length decoder is deliberately *not* duplicated here: the LSP lane
//! exposes `crate::lsp::framing::{MessageFramer, FramingError}` and the DAP
//! reader consumes that seam.
//!
//! Integration state:
//! - the CLI mounts [`DapTool`] behind `debug.enabled` and `--tools`/`--no-tools`
//! - `RunnerSession` owns one `Arc<DapSessionManager>`; every per-prompt root
//!   registry registers `DapTool::shared(manager.clone()).with_max_timeout(<tools.maxTimeout>)`
//!   so sessions survive turns
//! - subagents construct `DapTool::new()` with a fresh manager: child isolation
//!   is explicit, unlike the TypeScript process-wide singleton
//! - approval classifies [`is_readonly_action`] as `read`, everything else as
//!   `exec`
//! - runner teardown calls `DapSessionManager::terminate`; dropping a manager
//!   disposes whatever is left, so child abort paths do not leak adapters

pub mod client;
pub mod config;
pub mod session;
pub mod tool;
pub mod types;

pub use client::{DapClient, DapError};
pub use config::{
	AdapterConfig, ConnectMode, LaunchAdapterSelection, LaunchProgramKind, ResolvedAdapter,
	configured_adapter_names, get_adapter_configs, resolve_adapter, select_attach_adapter,
	select_launch_adapter,
};
pub use session::{AttachOptions, DapSession, DapSessionManager, LaunchOptions};
pub use tool::{DapTool, DEBUG_READONLY_ACTIONS, is_readonly_action};
pub use types::{Capabilities, SessionStatus, SessionSummary};
