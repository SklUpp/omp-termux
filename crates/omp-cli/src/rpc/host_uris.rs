//! Host URI bridge: RPC-owned URI schemes for the internal URL router.
//!
//! Mirrors `packages/coding-agent/src/modes/rpc/host-uris.ts`: the host
//! registers schemes, reads/writes are dispatched as `host_uri_request` frames
//! and resolved by `host_uri_result`. Scheme validation, reserved namespaces,
//! writable/immutable semantics and error text are identical to upstream.
//!
//! This build's native `read`/`write` tools reject every scheme URI (no
//! `InternalUrlRouter` exists yet), so installing the handlers is reported as a
//! precise prerequisite instead of pretending the schemes are live.

use std::{
	collections::BTreeMap,
	sync::{
		Arc, Mutex, MutexGuard,
		atomic::{AtomicU64, Ordering},
	},
};

use pi_agent::CancellationToken;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::{
	session::{FrameSink, Prerequisite},
	types::{RpcHostUriResult, RpcHostUriSchemeDefinition},
};

/// OMP-owned namespaces that RPC hosts may not replace.
const RESERVED_SCHEMES: [&str; 1] = ["security"];

enum UriReply {
	Result(RpcHostUriResult),
	Closed(String),
}

struct PendingUriCall {
	tx: mpsc::UnboundedSender<UriReply>,
}

/// One resolved internal resource, shape of `InternalResource`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InternalResource {
	pub url:          String,
	pub content:      String,
	pub content_type: String,
	pub size:         usize,
	pub notes:        Option<Vec<String>>,
	pub immutable:    bool,
}

/// Serializes host URI reads and writes over an RPC frame sink.
pub struct HostUriBridge {
	sink:        Arc<dyn FrameSink>,
	definitions: Mutex<BTreeMap<String, RpcHostUriSchemeDefinition>>,
	pending:     Mutex<BTreeMap<String, PendingUriCall>>,
	closed:      Mutex<Option<String>>,
	next_id:     AtomicU64,
}

impl std::fmt::Debug for HostUriBridge {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("HostUriBridge")
			.field("schemes", &self.lock_definitions().len())
			.finish()
	}
}

fn valid_scheme(scheme: &str) -> bool {
	let mut chars = scheme.chars();
	let Some(first) = chars.next() else {
		return false;
	};
	if !first.is_ascii_lowercase() {
		return false;
	}
	chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '+' | '.' | '-'))
}

impl HostUriBridge {
	#[must_use]
	pub fn new(sink: Arc<dyn FrameSink>) -> Self {
		Self {
			sink,
			definitions: Mutex::new(BTreeMap::new()),
			pending: Mutex::new(BTreeMap::new()),
			closed: Mutex::new(None),
			next_id: AtomicU64::new(1),
		}
	}

	/// Replace the registered scheme set, validating exactly like upstream.
	pub fn set_schemes(
		&self,
		schemes: &[RpcHostUriSchemeDefinition],
	) -> Result<Vec<String>, String> {
		let mut normalized = BTreeMap::new();
		for raw in schemes {
			let scheme = raw.scheme.trim().to_lowercase();
			if scheme.is_empty() {
				return Err("Host URI scheme must be a non-empty string".to_string());
			}
			if !valid_scheme(&scheme) {
				return Err(format!("Host URI scheme contains invalid characters: {}", raw.scheme));
			}
			if RESERVED_SCHEMES.contains(&scheme.as_str()) {
				return Err(format!("Host URI scheme is reserved by OMP: {scheme}://"));
			}
			normalized.insert(
				scheme.clone(),
				RpcHostUriSchemeDefinition {
					scheme,
					description: raw.description.clone(),
					writable: raw.writable,
					immutable: raw.immutable,
				},
			);
		}
		let mut store = self.lock_definitions();
		*store = normalized;
		Ok(store.keys().cloned().collect())
	}

	/// Currently registered schemes, sorted.
	#[must_use]
	pub fn schemes(&self) -> Vec<String> {
		self.lock_definitions().keys().cloned().collect()
	}

	/// Resolve an inbound `host_uri_result`; `false` when the id is unknown.
	pub fn handle_result(&self, frame: RpcHostUriResult) -> bool {
		let Some(pending) = self.lock_pending().remove(&frame.id) else {
			return false;
		};
		let _ = pending.tx.send(UriReply::Result(frame));
		true
	}

	/// Reject every pending request and every future request.
	pub fn close(&self, message: &str) {
		let mut closed = self.lock_closed();
		if closed.is_none() {
			*closed = Some(message.to_string());
		}
		drop(closed);
		let pending: Vec<PendingUriCall> =
			std::mem::take(&mut *self.lock_pending()).into_values().collect();
		for call in pending {
			let _ = call.tx.send(UriReply::Closed(message.to_string()));
		}
	}

	/// Dispatch a read; the response becomes an [`InternalResource`].
	pub async fn request_read(
		self: &Arc<Self>,
		scheme: &str,
		url: &str,
		cancel: &CancellationToken,
	) -> Result<InternalResource, String> {
		let result = self.dispatch("read", url, None, cancel).await?;
		if result.is_error {
			return Err(non_empty(
				result.error.as_deref().or(result.content.as_deref()),
				&format!("Host URI read failed for {url}"),
			));
		}
		let content = result.content.unwrap_or_default();
		let content_type = result.content_type.unwrap_or_else(|| "text/plain".to_string());
		let immutable = result
			.immutable
			.unwrap_or_else(|| {
				self
					.lock_definitions()
					.get(scheme)
					.is_some_and(|definition| definition.immutable)
			});
		let notes = result.notes.filter(|notes| !notes.is_empty());
		Ok(InternalResource {
			url: url.to_string(),
			size: content.len(),
			content,
			content_type,
			notes,
			immutable,
		})
	}

	/// Dispatch a write. Writable registration is enforced by the router, which
	/// only installs a write handler for `writable` schemes.
	pub async fn request_write(
		self: &Arc<Self>,
		url: &str,
		content: &str,
		cancel: &CancellationToken,
	) -> Result<(), String> {
		let result = self.dispatch("write", url, Some(content), cancel).await?;
		if result.is_error {
			return Err(non_empty(
				result.error.as_deref().or(result.content.as_deref()),
				&format!("Host URI write failed for {url}"),
			));
		}
		Ok(())
	}

	async fn dispatch(
		self: &Arc<Self>,
		operation: &'static str,
		url: &str,
		content: Option<&str>,
		cancel: &CancellationToken,
	) -> Result<RpcHostUriResult, String> {
		if cancel.is_cancelled() {
			return Err(format!("Host URI {operation} for {url} was aborted"));
		}
		if let Some(message) = self.lock_closed().clone() {
			return Err(message);
		}
		let id = format!("uri-{}-{}", std::process::id(), self.next_id.fetch_add(1, Ordering::Relaxed));
		let (tx, mut rx) = mpsc::unbounded_channel();
		self.lock_pending().insert(id.clone(), PendingUriCall { tx });
		let mut frame = json!({
			"type": "host_uri_request",
			"id": id,
			"operation": operation,
			"url": url,
		});
		if operation == "write" {
			frame["content"] = json!(content.unwrap_or_default());
		}
		self.sink.send(frame);

		let reply = tokio::select! {
			reply = rx.recv() => reply,
			() = cancel.cancelled() => {
				self.lock_pending().remove(&id);
				self.sink.send(json!({
					"type": "host_uri_cancel",
					"id": format!("uri-cancel-{}", self.next_id.fetch_add(1, Ordering::Relaxed)),
					"targetId": id,
				}));
				return Err(format!("Host URI {operation} for {url} was aborted"));
			},
		};
		match reply {
			Some(UriReply::Result(result)) => Ok(result),
			Some(UriReply::Closed(message)) => Err(message),
			None => {
				self.lock_pending().remove(&id);
				Err(format!("Host URI {operation} for {url} was aborted"))
			},
		}
	}

	fn lock_definitions(&self) -> MutexGuard<'_, BTreeMap<String, RpcHostUriSchemeDefinition>> {
		self.definitions.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
	}

	fn lock_pending(&self) -> MutexGuard<'_, BTreeMap<String, PendingUriCall>> {
		self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
	}

	fn lock_closed(&self) -> MutexGuard<'_, Option<String>> {
		self.closed.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
	}
}

fn non_empty(preferred: Option<&str>, fallback: &str) -> String {
	preferred
		.filter(|text| !text.is_empty())
		.map_or_else(|| fallback.to_string(), str::to_string)
}

/// The native seam host URI schemes still need: no internal URL router exists.
#[must_use]
pub fn mount_prerequisite(schemes: &[String]) -> Prerequisite {
	Prerequisite::new(
		"host_uri_schemes_not_mounted",
		format!(
			"host URI schemes {} were validated and stored, but this build's read/write tools reject \
			 scheme URIs: no InternalUrlRouter exists (native seam: pi_agent internal-URL router with \
			 ProtocolHandler registration, consulted by read and write)",
			schemes
				.iter()
				.map(|scheme| format!("{scheme}://"))
				.collect::<Vec<_>>()
				.join(", ")
		),
	)
}

/// Result payload for `set_host_uri_schemes`.
#[must_use]
pub fn schemes_payload(schemes: &[String]) -> Value {
	json!({ "schemes": schemes })
}

#[cfg(test)]
mod tests {
	use std::sync::{Arc, mpsc};

	use serde_json::Value;

	use super::*;
	use crate::rpc::session::FrameSink;

	#[derive(Default)]
	struct TestSink {
		tx: Mutex<Option<mpsc::Sender<Value>>>,
	}

	impl FrameSink for TestSink {
		fn send(&self, frame: Value) {
			if let Some(tx) = self.tx.lock().expect("sink").as_ref() {
				let _ = tx.send(frame);
			}
		}
	}

	fn definition(scheme: &str) -> RpcHostUriSchemeDefinition {
		RpcHostUriSchemeDefinition {
			scheme: scheme.to_string(),
			description: None,
			writable: false,
			immutable: false,
		}
	}

	fn runtime() -> tokio::runtime::Runtime {
		tokio::runtime::Builder::new_multi_thread()
			.enable_all()
			.build()
			.expect("runtime")
	}

	#[test]
	fn scheme_validation_matches_upstream() {
		let bridge = HostUriBridge::new(Arc::new(TestSink::default()));
		assert_eq!(
			bridge.set_schemes(&[definition("  ")]).expect_err("empty"),
			"Host URI scheme must be a non-empty string"
		);
		assert_eq!(
			bridge.set_schemes(&[definition("1bad")]).expect_err("invalid"),
			"Host URI scheme contains invalid characters: 1bad"
		);
		assert_eq!(
			bridge.set_schemes(&[definition("bad scheme")]).expect_err("invalid"),
			"Host URI scheme contains invalid characters: bad scheme"
		);
		assert_eq!(
			bridge.set_schemes(&[definition("security")]).expect_err("reserved"),
			"Host URI scheme is reserved by OMP: security://"
		);
		assert_eq!(
			bridge.set_schemes(&[definition("Docs")]).expect("normalized"),
			vec!["docs".to_string()]
		);
		assert_eq!(bridge.schemes(), vec!["docs".to_string()]);
	}

	#[test]
	fn read_round_trips_through_frames() {
		let (tx, rx) = mpsc::channel();
		let sink = Arc::new(TestSink { tx: Mutex::new(Some(tx)) });
		let bridge = Arc::new(HostUriBridge::new(sink));
		bridge.set_schemes(&[RpcHostUriSchemeDefinition {
			scheme: "docs".into(),
			description: None,
			writable: false,
			immutable: true,
		}])
		.expect("schemes");

		let runtime = runtime();
		let task = {
			let bridge = Arc::clone(&bridge);
			let cancel = CancellationToken::new();
			runtime.spawn(async move { bridge.request_read("docs", "docs://a", &cancel).await })
		};
		let request = rx.recv_timeout(std::time::Duration::from_secs(5)).expect("request");
		assert_eq!(request["type"], json!("host_uri_request"));
		assert_eq!(request["operation"], json!("read"));
		assert_eq!(request["url"], json!("docs://a"));
		let id = request["id"].as_str().expect("id").to_string();
		assert!(bridge.handle_result(RpcHostUriResult {
			id,
			content: Some("hello".into()),
			content_type: None,
			notes: Some(vec!["n".into()]),
			immutable: None,
			is_error: false,
			error: None,
		}));
		let resource = runtime.block_on(task).expect("join").expect("resource");
		assert_eq!(resource.content, "hello");
		assert_eq!(resource.content_type, "text/plain");
		assert_eq!(resource.size, 5);
		assert_eq!(resource.notes.as_deref(), Some(&["n".to_string()][..]));
		assert!(resource.immutable);
	}

	#[test]
	fn write_errors_and_unknown_results() {
		let (tx, rx) = mpsc::channel();
		let sink = Arc::new(TestSink { tx: Mutex::new(Some(tx)) });
		let bridge = Arc::new(HostUriBridge::new(sink));
		bridge.set_schemes(&[definition("docs")]).expect("schemes");
		assert!(!bridge.handle_result(RpcHostUriResult {
			id: "missing".into(),
			content: None,
			content_type: None,
			notes: None,
			immutable: None,
			is_error: false,
			error: None,
		}));

		let runtime = runtime();
		let task = {
			let bridge = Arc::clone(&bridge);
			let cancel = CancellationToken::new();
			runtime.spawn(async move { bridge.request_write("docs://a", "data", &cancel).await })
		};
		let request = rx.recv_timeout(std::time::Duration::from_secs(5)).expect("request");
		assert_eq!(request["operation"], json!("write"));
		assert_eq!(request["content"], json!("data"));
		let id = request["id"].as_str().expect("id").to_string();
		assert!(bridge.handle_result(RpcHostUriResult {
			id,
			content: None,
			content_type: None,
			notes: None,
			immutable: None,
			is_error: true,
			error: None,
		}));
		let error = runtime.block_on(task).expect("join").expect_err("write failed");
		assert_eq!(error, "Host URI write failed for docs://a");
	}

	#[test]
	fn cancellation_emits_cancel_frame() {
		let (tx, rx) = mpsc::channel();
		let sink = Arc::new(TestSink { tx: Mutex::new(Some(tx)) });
		let bridge = Arc::new(HostUriBridge::new(sink));
		bridge.set_schemes(&[definition("docs")]).expect("schemes");
		let runtime = runtime();
		let cancel = CancellationToken::new();
		let task = {
			let bridge = Arc::clone(&bridge);
			let cancel = cancel.clone();
			runtime.spawn(async move { bridge.request_read("docs", "docs://a", &cancel).await })
		};
		let _request = rx.recv_timeout(std::time::Duration::from_secs(5)).expect("request");
		cancel.cancel();
		let error = runtime.block_on(task).expect("join").expect_err("aborted");
		assert_eq!(error, "Host URI read for docs://a was aborted");
		let cancel_frame = rx.recv_timeout(std::time::Duration::from_secs(5)).expect("cancel");
		assert_eq!(cancel_frame["type"], json!("host_uri_cancel"));
	}
}
