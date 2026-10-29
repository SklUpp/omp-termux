//! Reverse calls: requests the agent sends to the ACP client.
//!
//! `session/request_permission`, `fs/*`, `terminal/*` and `elicitation/create`
//! share one shape — send a correlated request, wait for the answer — so a
//! single caller covers them; method names, parameters and capability gates
//! live with the senders that use it.
//!
//! Ported from `RpcConnection#request` (`packages/utils/src/acp/transport.ts:
//! 112-125`) plus the abort semantics every ACP sender needs: upstream races a
//! reverse call against the session's abort signal, and a client that never
//! answers must not wedge a turn. The pending slot is registered before the
//! frame is written, so an instant answer cannot outrun registration, and every
//! exit path — answer, remote error, timeout, cancel, send failure, closed
//! transport — removes it. Errors name the method only, never parameters.

use std::{fmt, sync::mpsc::RecvTimeoutError, time::{Duration, Instant}};

use serde_json::Value;

use super::{protocol::{self, RequestError}, transport::{REPLY_POLL, Reply, Transport}};

/// A reverse call that produced no peer result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReverseError {
	/// The peer answered with a JSON-RPC error; the payload is untouched.
	Remote(RequestError),
	/// The connection closed before the peer answered.
	Closed(String),
	/// The caller's cancellation signal fired.
	Cancelled,
	/// The caller's deadline expired.
	Timeout,
	/// The request could not be queued. Carries the method name only.
	Send { method: String, reason: String },
}

impl fmt::Display for ReverseError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Remote(error) => write!(f, "{}", error.message),
			Self::Closed(reason) => write!(f, "connection closed: {reason}"),
			Self::Cancelled => f.write_str("reverse call cancelled"),
			Self::Timeout => f.write_str("reverse call timed out"),
			Self::Send { method, reason } => write!(f, "cannot send {method}: {reason}"),
		}
	}
}

impl std::error::Error for ReverseError {}

impl From<RequestError> for ReverseError {
	fn from(error: RequestError) -> Self {
		Self::Remote(error)
	}
}

/// Sends ACP requests to the client over one [`Transport`].
#[derive(Clone)]
pub struct ReverseCaller {
	transport: Transport,
}

impl ReverseCaller {
	pub fn new(transport: Transport) -> Self {
		Self { transport }
	}

	/// Request with no deadline and no cancellation signal.
	pub fn request(&self, method: &str, params: Option<Value>) -> Result<Value, ReverseError> {
		self.request_cancellable(method, params, None, &|| false)
	}

	/// Request bounded by `timeout` and the caller's cancellation signal.
	///
	/// The wait polls every `REPLY_POLL` so a cancellation set by another
	/// thread is observed without a second waker; a reply that races the signal
	/// wins, matching the RPC dialog bridge.
	pub fn request_cancellable(
		&self,
		method: &str,
		params: Option<Value>,
		timeout: Option<Duration>,
		cancelled: &dyn Fn() -> bool,
	) -> Result<Value, ReverseError> {
		let deadline = timeout.map(|timeout| Instant::now() + timeout);
		if cancelled() {
			return Err(ReverseError::Cancelled);
		}
		if let Some(reason) = self.transport.closed_reason() {
			return Err(ReverseError::Closed(reason));
		}
		let id = self.transport.next_id();
		let Some(replies) = self.transport.register(&id) else {
			return Err(self.closed());
		};
		if cancelled() {
			self.transport.unregister(&id);
			return Err(ReverseError::Cancelled);
		}
		if !self.transport.write(protocol::request_frame(&id, method, params.as_ref())) {
			self.transport.unregister(&id);
			return Err(ReverseError::Send {
				method: method.to_string(),
				reason: "connection closed".to_string(),
			});
		}
		loop {
			let wait = match deadline {
				Some(deadline) => deadline.saturating_duration_since(Instant::now()).min(REPLY_POLL),
				None => REPLY_POLL,
			};
			match replies.recv_timeout(wait) {
				Ok(Reply::Result(value)) => return Ok(value),
				Ok(Reply::Error(error)) => return Err(ReverseError::Remote(error)),
				Ok(Reply::Closed) => {
					self.transport.unregister(&id);
					return Err(self.closed());
				},
				Err(RecvTimeoutError::Disconnected) => {
					self.transport.unregister(&id);
					return Err(self.closed());
				},
				Err(RecvTimeoutError::Timeout) => {
					if cancelled() {
						// A reply that raced the signal wins.
						match replies.try_recv() {
							Ok(Reply::Result(value)) => return Ok(value),
							Ok(Reply::Error(error)) => return Err(ReverseError::Remote(error)),
							_ => {},
						}
						self.transport.unregister(&id);
						return Err(ReverseError::Cancelled);
					}
					if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
						// A reply that raced the deadline wins, as above.
						match replies.try_recv() {
							Ok(Reply::Result(value)) => return Ok(value),
							Ok(Reply::Error(error)) => return Err(ReverseError::Remote(error)),
							_ => {},
						}
						self.transport.unregister(&id);
						return Err(ReverseError::Timeout);
					}
				},
			}
		}
	}

	fn closed(&self) -> ReverseError {
		ReverseError::Closed(
			self.transport
				.closed_reason()
				.unwrap_or_else(|| "Connection closed".to_string()),
		)
	}
}

#[cfg(test)]
mod tests {
	use std::{sync::{Arc, atomic::{AtomicBool, Ordering}}, thread, time::Duration};

	use serde_json::json;

	use crate::acp::{
		protocol::RequestError,
		transport::{Transport, test_support::{RecordingWriter, line, transport_with}},
	};

	use super::{ReverseCaller, ReverseError};

	fn caller_and_writer() -> (ReverseCaller, RecordingWriter, Transport) {
		let writer = RecordingWriter::default();
		let (transport, _recorder) = transport_with(&writer);
		(ReverseCaller::new(transport.clone()), writer, transport)
	}

	#[test]
	fn concurrent_calls_correlate_by_id_with_out_of_order_replies() {
		let (caller, writer, transport) = caller_and_writer();
		let mut handles = Vec::new();
		for tag in ["a", "b"] {
			let caller = caller.clone();
			handles.push(thread::spawn(move || {
				let reply = caller.request("fs/read_text_file", Some(json!({ "tag": tag })));
				(tag, reply)
			}));
		}

		let frames = writer.wait_frames(2, |frame| frame["method"] == json!("fs/read_text_file"));
		for (index, frame) in frames.iter().enumerate().rev() {
			transport.handle_line(&line(json!({
				"jsonrpc": "2.0",
				"id": frame["id"],
				"result": { "index": index, "tag": frame["params"]["tag"] }
			})));
		}

		for handle in handles {
			let (tag, reply) = handle.join().expect("join");
			let reply = reply.expect("correlated reply");
			assert_eq!(reply["tag"], json!(tag));
		}
		assert_eq!(transport.pending_len(), 0);
	}

	#[test]
	fn remote_errors_are_returned_verbatim() {
		let (caller, writer, transport) = caller_and_writer();
		let handle = thread::spawn(move || {
			caller.request("terminal/create", Some(json!({ "command": "echo" })))
		});
		let frame = writer.wait_frame(|frame| frame["method"] == json!("terminal/create"));
		transport.handle_line(&line(json!({
			"jsonrpc": "2.0",
			"id": frame["id"],
			"error": {
				"code": -32601,
				"message": "\"Method not found\": terminal/create",
				"data": { "method": "terminal/create" }
			}
		})));

		let error = handle.join().expect("join").expect_err("peer error");
		match error {
			ReverseError::Remote(error) => {
				assert_eq!(error.code, -32601);
				assert_eq!(error.message, "\"Method not found\": terminal/create");
				assert_eq!(error.data, Some(json!({ "method": "terminal/create" })));
			},
			other => panic!("unexpected {other:?}"),
		}
		assert_eq!(transport.pending_len(), 0);
	}

	#[test]
	fn timeout_removes_the_slot_and_a_late_reply_is_ignored() {
		let (caller, writer, transport) = caller_and_writer();
		let error = caller
			.request_cancellable(
				"elicitation/create",
				Some(json!({ "mode": "url" })),
				Some(Duration::from_millis(40)),
				&|| false,
			)
			.expect_err("timeout");
		assert_eq!(error, ReverseError::Timeout);
		assert_eq!(transport.pending_len(), 0);

		let frame = writer.wait_frame(|frame| frame["method"] == json!("elicitation/create"));
		transport.handle_line(&line(json!({
			"jsonrpc": "2.0",
			"id": frame["id"],
			"result": { "action": "accept" }
		})));
		assert_eq!(transport.pending_len(), 0);
	}

	#[test]
	fn cancel_removes_the_slot() {
		let (caller, writer, transport) = caller_and_writer();
		let cancelled = Arc::new(AtomicBool::new(false));
		let flag = Arc::clone(&cancelled);
		let handle = thread::spawn(move || {
			caller.request_cancellable(
				"fs/write_text_file",
				Some(json!({ "path": "p" })),
				None,
				&|| flag.load(Ordering::SeqCst),
			)
		});
		writer.wait_frame(|frame| frame["method"] == json!("fs/write_text_file"));
		cancelled.store(true, Ordering::SeqCst);
		assert_eq!(handle.join().expect("join"), Err(ReverseError::Cancelled));
		assert_eq!(transport.pending_len(), 0);
	}

	#[test]
	fn eof_rejects_pending_calls() {
		let (caller, writer, transport) = caller_and_writer();
		let handle = thread::spawn(move || {
			caller.request("session/request_permission", Some(json!({ "sessionId": "s" })))
		});
		writer.wait_frame(|frame| frame["method"] == json!("session/request_permission"));

		transport.serve(&b""[..]);
		assert!(transport.is_closed());
		let error = handle.join().expect("join").expect_err("closed");
		assert!(matches!(error, ReverseError::Closed(_)));
		assert_eq!(transport.pending_len(), 0);
	}

	#[test]
	fn closed_transport_rejects_new_calls() {
		let (caller, _writer, transport) = caller_and_writer();
		transport.close(Some("peer gone".to_string()));
		assert_eq!(
			caller.request("session/request_permission", None).expect_err("closed"),
			ReverseError::Closed("peer gone".to_string())
		);
		// A peer error stays a peer error; it is never reported as a close.
		assert_eq!(
			ReverseError::from(RequestError::request_cancelled()),
			ReverseError::Remote(RequestError::request_cancelled())
		);
	}

	#[test]
	fn send_errors_name_the_method_only() {
		let error = ReverseError::Send {
			method: "fs/read_text_file".to_string(),
			reason: "connection closed".to_string(),
		};
		assert_eq!(error.to_string(), "cannot send fs/read_text_file: connection closed");
	}
}
