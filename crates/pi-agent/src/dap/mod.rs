//! Native DAP debug stack for the Rust agent.
//!
//! Port of `packages/coding-agent/src/dap/*` plus the `debug` tool. The
//! Content-Length decoder is deliberately *not* duplicated here: the LSP lane
//! exposes `crate::lsp::framing::{MessageFramer, FramingError}` and the DAP
//! reader consumes that seam. The parent lands `pub mod lsp;` before `dap`.
//!
//! Module seam for parent integration:
//! - add `pub mod dap;` to `crates/pi-agent/src/lib.rs`
//! - `RunnerSession` owns one `Arc<DapSessionManager>`; every per-prompt
//!   registry registers `DapTool::shared(manager.clone()).with_max_timeout(<tools.maxTimeout>)`
//!   behind the existing `debug.enabled` gate, so sessions survive turns
//! - subagents construct `DapTool::new()` with a fresh manager: child isolation
//!   is explicit, unlike the TypeScript process-wide singleton
//! - classify approval with [`is_readonly_action`] (`read`) versus `exec`
//! - call `DapTool::manager().terminate(...)` (or drop the runner template) on
//!   shutdown
//!
//! No shared module is modified by this implementation.

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
pub use session::{DapSession, DapSessionManager, LaunchOptions, AttachOptions};
pub use tool::{DEBUG_READONLY_ACTIONS, DapTool, is_readonly_action};
pub use types::{Capabilities, SessionStatus, SessionSummary};
