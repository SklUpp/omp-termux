//! RPC mode (`--mode rpc`): NDJSON command/event transport over the native
//! agent runner.
//!
//! Layout mirrors the upstream TypeScript modules so a reviewer can diff the
//! contract one file at a time:
//!
//! | module      | upstream                  | role                                   |
//! |-------------|---------------------------|----------------------------------------|
//! | `types`     | `rpc-types.ts`            | commands, responses, frames, state     |
//! | `frame`     | `rpc-frame.ts`            | v1/v2 chunking + reassembly            |
//! | `input`     | `rpc-input.ts`            | stdin reader, writer, extension UI, EOF |
//! | `messages`  | `rpc-messages.ts`         | snapshot paging                        |
//! | `host_tools`| `host-tools.ts`           | host tool bridge + native adapters     |
//! | `host_uris` | `host-uris.ts`            | host URI bridge + scheme validation    |
//! | `subagents` | `rpc-subagents.ts`        | subagent snapshots + transcript slices |
//! | `session`   | `agent-session` + `rpc-mode` state | long-lived session state       |
//! | `mode`      | `rpc-mode.ts`             | command loop, turn pump, frames        |

pub mod frame;
pub mod host_tools;
pub mod host_uris;
pub mod input;
pub mod messages;
pub mod mode;
pub mod session;
pub mod subagents;
pub mod types;

pub use mode::RpcMode;
pub use session::{RpcLaunch, RpcSession};
pub use types::ready_frame;
