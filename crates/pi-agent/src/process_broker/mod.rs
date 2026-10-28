//! Cross-process daemon broker: authenticated Unix-socket transport around ONE
//! keepalive [`crate::tools::process::ProcessManager`], plus the client used by
//! the CLI/hub to reach it.
//!
//! `protocol` is the wire contract, `broker` the server/worker (its
//! `BrokerBackend` trait is the server-side supervision surface), `client` the
//! connect-or-spawn side with the [`crate::tools::process::ProcessBackend`]
//! adapter the supervisor/hub consumes. The CLI dispatches the hidden worker
//! selector before clap; the parent owns that startup hunk.

pub mod broker;
pub mod client;
pub mod protocol;

pub use broker::{Broker, BrokerBackend, ManagerBackend, start};
pub use client::{BrokerClient, BrokerProcessBackend, DescribeResult, StartResult, WaitResult};
pub use protocol::{Operation, PublicSpec, WireSpec, WORKER_ARG};
