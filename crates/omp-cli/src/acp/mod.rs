//! ACP mode stack (`omp acp`): JSON-RPC 2.0 over stdio for ACP clients.
//!
//! Build order follows upstream modules so a reviewer can diff them one at a
//! time:
//!
//! * `protocol` — `utils/src/acp/transport.ts` and `protocol.ts`: envelopes,
//!   error codes and the client capabilities the agent reads.
//! * `transport` — `utils/src/acp/stream.ts` and `transport.ts`: newline
//!   framing, one writer thread, request correlation, fail-closed close.
//! * `reverse` — `transport.ts` `RpcConnection#request`: agent -> client
//!   requests, cancellable and deadline bounded.
//! * `capabilities` / `auth` — `initialize`/`authenticate` surface and the
//!   shared `agent.db` credential resolver.
//! * `session`, `session_list`, `plan` — the session map, `FrameSink` ->
//!   `session/update` mapper, single-flight prompts, plan mode and listing.
//! * `mode` — the method table, `_omp/*` extensions and the stdio entry point.

pub mod auth;
pub mod bridge;
pub mod capabilities;
pub mod mode;
pub mod plan;
pub mod protocol;
pub mod reverse;
pub mod session;
pub mod session_list;
pub mod transport;

pub use auth::AcpAuth;
pub use mode::run;
pub use protocol::{ClientCapabilities, JsonRpcId, RequestError};
pub use reverse::{ReverseCaller, ReverseError};
pub use session::{AcpRecipe, AcpRuntime, AcpSession, AcpReverseClient, SessionManager};
pub use transport::{Dispatcher, Transport};
