//! ACP mode stack (`omp acp`): JSON-RPC 2.0 over stdio for ACP clients.
//!
//! Build order follows upstream modules so a reviewer can diff them one at a
//! time; this crate carries the transport seam first.
//!
//! * `protocol` — `utils/src/acp/transport.ts` and `protocol.ts`: envelopes,
//!   error codes and the client capabilities the agent reads.
//! * `transport` — `utils/src/acp/stream.ts` and `transport.ts`: newline
//!   framing, one writer thread, request correlation, fail-closed close.
//! * `reverse` — `transport.ts` `RpcConnection#request`: agent -> client
//!   requests, cancellable and deadline bounded.
//!
//! Still to land before `acp` may be advertised by the CLI: the session map and
//! the `FrameSink` -> `session/update` mapper, the mode loop (single-flight
//! prompt, cancel barrier), plan mode, session listing, and the
//! `main.rs`/`args.rs` wiring. Nothing here registers a command or a method
//! table, so the binary keeps refusing `acp` until those modules exist.

pub mod protocol;
pub mod reverse;
pub mod transport;

pub use protocol::{ClientCapabilities, JsonRpcId, RequestError};
pub use reverse::{ReverseCaller, ReverseError};
pub use transport::{Dispatcher, Transport};
