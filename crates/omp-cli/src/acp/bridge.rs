//! `pi_agent::AcpClientBridge` over [`AcpReverseClient`].
//!
//! The reverse client is synchronous: `ReverseCaller` waits on a reply channel
//! poll loop, so every call runs on `spawn_blocking` instead of occupying a
//! Tokio worker. Capability gates answer from the exact
//! `ClientCapabilities` the session advertised; a reverse call that was never
//! sent maps to [`AcpClientError::Unsupported`], and a cancellation maps to
//! [`AcpClientError::Cancelled`] so a native tool never retries a side effect.
//!
//! Mounted by [`super::session`] on `RpcSession` and copied into the
//! `RunRequest`, then onto the root `read`/`write`/`bash` tools and the
//! supervisor's child registries.

use async_trait::async_trait;
use pi_agent::{AcpClientBridge, AcpClientError, CancellationToken};

use super::{protocol::ClientCapabilities, reverse::ReverseError, session::AcpReverseClient};

/// Reverse sender bound to one ACP session's capabilities.
pub struct AcpToolBridge {
	client: AcpReverseClient,
	caps: ClientCapabilities,
}

impl AcpToolBridge {
	#[must_use]
	pub fn new(client: AcpReverseClient, caps: ClientCapabilities) -> Self {
		Self { client, caps }
	}
}

/// Run one blocking reverse call off the Tokio worker.
async fn off<T, F>(work: F) -> Result<T, AcpClientError>
where
	F: FnOnce() -> Result<T, ReverseError> + Send + 'static,
	T: Send + 'static,
{
	tokio::task::spawn_blocking(work)
		.await
		.map_err(|error| AcpClientError::Remote(format!("ACP reverse worker failed: {error}")))?
		.map_err(map_reverse)
}

fn map_reverse(error: ReverseError) -> AcpClientError {
	match error {
		ReverseError::Cancelled => AcpClientError::Cancelled,
		// `AcpReverseClient` reports an unadvertised capability through
		// `ReverseError::Send` with this exact prefix; nothing reached the wire.
		ReverseError::Send { ref reason, .. } if reason.starts_with("client did not advertise") => {
			AcpClientError::Unsupported
		},
		other => AcpClientError::Remote(other.to_string()),
	}
}

#[async_trait]
impl AcpClientBridge for AcpToolBridge {
	fn supports_fs_read(&self) -> bool {
		self.caps.fs.read_text_file
	}

	fn supports_fs_write(&self) -> bool {
		self.caps.fs.write_text_file
	}

	fn supports_terminal(&self) -> bool {
		self.caps.terminal
	}

	async fn read_text_file(
		&self,
		path: &str,
		line: Option<u64>,
		limit: Option<u64>,
		cancel: &CancellationToken,
	) -> Result<String, AcpClientError> {
		let client = self.client.clone();
		let path = path.to_string();
		let cancel = cancel.clone();
		off(move || client.read_text_file(&path, line, limit, &cancel)).await
	}

	async fn write_text_file(
		&self,
		path: &str,
		content: &str,
		cancel: &CancellationToken,
	) -> Result<(), AcpClientError> {
		let client = self.client.clone();
		let path = path.to_string();
		let content = content.to_string();
		let cancel = cancel.clone();
		off(move || client.write_text_file(&path, &content, &cancel)).await
	}

	async fn terminal_create(
		&self,
		command: &str,
		args: Vec<String>,
		env: Vec<(String, String)>,
		cwd: Option<&str>,
		output_byte_limit: Option<u64>,
		cancel: &CancellationToken,
	) -> Result<String, AcpClientError> {
		let client = self.client.clone();
		let command = command.to_string();
		let cwd = cwd.map(str::to_string);
		let cancel = cancel.clone();
		off(move || {
			client.terminal_create(
				&command,
				Some(args),
				Some(env),
				cwd.as_deref(),
				output_byte_limit,
				&cancel,
			)
		})
		.await
	}

	async fn terminal_output(
		&self,
		terminal_id: &str,
		cancel: &CancellationToken,
	) -> Result<serde_json::Value, AcpClientError> {
		let client = self.client.clone();
		let terminal_id = terminal_id.to_string();
		let cancel = cancel.clone();
		off(move || client.terminal_output(&terminal_id, &cancel)).await
	}

	async fn terminal_wait_for_exit(
		&self,
		terminal_id: &str,
		cancel: &CancellationToken,
	) -> Result<serde_json::Value, AcpClientError> {
		let client = self.client.clone();
		let terminal_id = terminal_id.to_string();
		let cancel = cancel.clone();
		off(move || client.terminal_wait_for_exit(&terminal_id, &cancel)).await
	}

	async fn terminal_kill(
		&self,
		terminal_id: &str,
		cancel: &CancellationToken,
	) -> Result<serde_json::Value, AcpClientError> {
		let client = self.client.clone();
		let terminal_id = terminal_id.to_string();
		let cancel = cancel.clone();
		off(move || client.terminal_kill(&terminal_id, &cancel)).await
	}

	async fn terminal_release(
		&self,
		terminal_id: &str,
		cancel: &CancellationToken,
	) -> Result<serde_json::Value, AcpClientError> {
		let client = self.client.clone();
		let terminal_id = terminal_id.to_string();
		let cancel = cancel.clone();
		off(move || client.terminal_release(&terminal_id, &cancel)).await
	}
}
