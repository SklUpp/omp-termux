//! Native broker-shared LSP mux.
//!
//! Port of `packages/coding-agent/src/lsp/mux/{protocol,server,daemon}.ts`.
//! One mux daemon serves a project scope: each connected link gets its own
//! language-server child, idle children linger briefly for reuse, and the link
//! speaks ordinary Content-Length-framed LSP after a one-request
//! [`MUX_CONNECT_METHOD`] handshake.
//!
//! The daemon is a hidden CLI worker (see [`worker`]); it is started under the
//! existing daemon broker, so no new supervisor or socket protocol is added.
//! Every shared-path failure in [`client`] degrades to a process-local spawn.

pub mod client;
pub mod server;
pub mod worker;

use std::{
	collections::BTreeMap,
	path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

/// Hidden CLI selector used to re-enter the LSP mux worker.
pub const WORKER_ARG: &str = "__omp_worker_lsp_mux";
/// Environment key carrying the socket endpoint the mux listens on.
pub const SOCKET_ENV: &str = "OMP_LSP_MUX_SOCKET";
/// Environment key carrying the canonical project directory the mux serves.
pub const PROJECT_DIR_ENV: &str = "OMP_LSP_MUX_PROJECT_DIR";
/// Stable broker daemon name for the shared LSP mux.
pub const DAEMON_NAME: &str = "omp.lsp.mux";
/// Prefix of the readiness banner printed once the mux accepts connections.
pub const READY_PATTERN: &str = "omp lsp mux listening on ";
/// Socket file name inside the project runtime directory.
pub const SOCKET_FILE: &str = "lsp-mux.sock";
/// Socket path budget (Linux/Android `sun_path` is 108 bytes including NUL).
const MAX_SOCKET_PATH_BYTES: usize = 104;

/// First (and only pre-LSP) request on a fresh link.
pub const MUX_CONNECT_METHOD: &str = "omp/muxConnect";
/// Liveness probe answered without binding the link to a server.
pub const MUX_PING_METHOD: &str = "omp/muxPing";
/// Probe response value.
pub const MUX_PING_RESULT: &str = "pong";
/// Notification on a bound link: kill that link's server process.
pub const MUX_RESTART_METHOD: &str = "omp/muxRestartServer";

/// Frame one JSON-RPC message with an LSP `Content-Length` header.
pub(crate) fn frame(message: &serde_json::Value) -> String {
	let body = serde_json::to_string(message).unwrap_or_else(|_| "null".to_string());
	format!("Content-Length: {}\r\n\r\n{body}", body.len())
}

/// Banner printed on stdout once the mux socket accepts connections.
#[must_use]
pub fn ready_banner(endpoint: &Path) -> String {
	format!("{READY_PATTERN}{}", endpoint.display())
}

/// Resolve the mux socket for one project scope.
///
/// Uses the broker's short physical runtime directory so Android/long-home
/// scopes stay inside the `sun_path` budget. The worker receives the resolved
/// path in [`SOCKET_ENV`] and never recomputes it.
pub fn endpoint_for_project(project_dir: &Path) -> Result<PathBuf, String> {
	let runtime = crate::process_broker::protocol::physical_runtime_dir(project_dir)?;
	let path = runtime.join(SOCKET_FILE);
	if path.as_os_str().as_encoded_bytes().len() >= MAX_SOCKET_PATH_BYTES {
		return Err(format!(
			"LSP mux socket path {} exceeds {MAX_SOCKET_PATH_BYTES} bytes",
			path.display()
		));
	}
	Ok(path)
}

/// Handshake parameters identifying a reusable server process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MuxConnectParams {
	/// Executable to spawn (the client's `resolved_command`, else `command`).
	pub command: String,
	/// Arguments passed verbatim to the executable.
	#[serde(default)]
	pub args: Vec<String>,
	/// Workspace root the server runs in; part of the server identity key.
	pub cwd: String,
	/// Extra environment overlaid on the mux daemon's own env for the spawn.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub env: Option<BTreeMap<String, String>>,
}

/// Handshake result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MuxConnectResult {
	/// Server identity key inside the mux.
	pub key: String,
	/// True when this handshake spawned the server process.
	pub spawned: bool,
	/// Pid of the server process assigned to this link.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub pid: Option<u32>,
}

/// Server identity key used by the mux registry.
///
/// Every input that changes what the spawned process *is* belongs in this key:
/// command, args, cwd, and the sorted environment overlay, JSON-encoded so no
/// separator can be forged from a value. The identity is hashed because the key
/// travels over the handshake and mux logs; raw environment values may contain
/// credentials. Two `xxh64` lanes with distinct seeds provide the 128-bit width
/// (the workspace has no SHA-256 crate and no dependency may be added).
#[must_use]
pub fn server_key(params: &MuxConnectParams) -> String {
	let env: Vec<(&String, &String)> = params
		.env
		.iter()
		.flatten()
		.collect::<BTreeMap<_, _>>()
		.into_iter()
		.collect();
	let identity = serde_json::json!([params.command, params.args, params.cwd, env]);
	let encoded = identity.to_string();
	let high = xxhash_rust::xxh64::xxh64(encoded.as_bytes(), 0x9e37_79b9_7f4a_7c15);
	let low = xxhash_rust::xxh64::xxh64(encoded.as_bytes(), 0xc2b2_ae3d_27d4_eb4f);
	format!("xxh128:{high:016x}{low:016x}")
}

#[cfg(test)]
mod tests {
	use super::*;

	fn params(args: &[&str], env: Option<(&str, &str)>) -> MuxConnectParams {
		MuxConnectParams {
			command: "rust-analyzer".to_string(),
			args: args.iter().map(|arg| (*arg).to_string()).collect(),
			cwd: "/tmp/project".to_string(),
			env: env.map(|(key, value)| {
				BTreeMap::from([(key.to_string(), value.to_string())])
			}),
		}
	}

	#[test]
	fn identity_covers_args_and_env() {
		let base = server_key(&params(&[], None));
		assert_ne!(base, server_key(&params(&["--log-level", "4"], None)));
		assert_ne!(base, server_key(&params(&[], Some(("A", "1")))));
		assert_ne!(
			server_key(&params(&[], Some(("A", "1")))),
			server_key(&params(&[], Some(("A", "2")))),
		);
		assert_eq!(base, server_key(&params(&[], None)));
	}

	#[test]
	fn env_order_does_not_split_identity() {
		let mut left = params(&[], None);
		left.env = Some(BTreeMap::from([
			("B".to_string(), "2".to_string()),
			("A".to_string(), "1".to_string()),
		]));
		let mut right = params(&[], None);
		right.env = Some(BTreeMap::from([
			("A".to_string(), "1".to_string()),
			("B".to_string(), "2".to_string()),
		]));
		assert_eq!(server_key(&left), server_key(&right));
	}

	#[test]
	fn key_is_hashed_not_raw_env() {
		let key = server_key(&params(&[], Some(("TOKEN", "hunter2"))));
		assert!(!key.contains("hunter2"));
		assert!(key.starts_with("xxh128:"));
	}

	#[test]
	fn banner_matches_ready_pattern() {
		let banner = ready_banner(Path::new("/tmp/lsp-mux.sock"));
		assert!(banner.starts_with(READY_PATTERN));
	}
}
