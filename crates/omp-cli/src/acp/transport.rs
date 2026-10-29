//! ACP stdio transport: newline-delimited JSON-RPC 2.0 with one serialized
//! writer and a reader that never waits for a handler.
//!
//! Ported from `packages/utils/src/acp/stream.ts` (`ndJsonStream`: `\n`
//! delimited JSON, trailing whitespace trimmed, blank lines skipped, a final
//! unterminated line still delivered) and `packages/utils/src/acp/transport.ts`
//! (`RpcConnection`: monotonic request ids, pending-table correlation,
//! `close()` rejecting every pending call).
//!
//! Three properties this transport must keep:
//!
//! * **Ordering.** Every frame leaves through one writer thread, so updates a
//!   turn emits and the response that ends it cannot interleave on the wire.
//! * **Liveness.** Responses are correlated on the reader thread, before any
//!   handler runs. A handler may block for a whole turn — or wait for the
//!   client — without delaying the answer that unblocks it.
//! * **Fail closed.** EOF, a read error, a write error and an explicit close
//!   all reject every pending reverse call. Nothing is fabricated. `close` (or
//!   EOF) is the shutdown path; dropping the last handle ends the writer queue
//!   and disconnects waiters that no running handler still holds.
//!
//! Local limits, none of which upstream has (it buffers an unbounded string and
//! dispatches an unbounded number of promises): one inbound line is capped at
//! [`MAX_LINE_BYTES`] and inbound frames wait in a bounded owner queue
//! (`HANDLER_WORKERS` threads, `MAX_QUEUED_FRAMES` slots). Behaviour on
//! refusal: a request gets `-32603` (`data.details` names the queue), a
//! notification is dropped, and an over-long line is answered with `-32600`
//! while the rest of that physical line is drained so the next frame still
//! parses. None of these tear the connection down.

use std::{
	collections::{HashMap, VecDeque},
	io::{self, BufRead, Write},
	panic::{AssertUnwindSafe, catch_unwind},
	sync::{
		Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak,
		atomic::{AtomicI64, Ordering},
		mpsc::{self, Receiver, Sender},
	},
	thread::{self, JoinHandle},
	time::Duration,
};

use serde_json::{Value, json};

use crate::rpc::session::FrameSink;

use super::protocol::{self, Incoming, JsonRpcId, LineError, RequestError};

/// Largest accepted inbound line.
///
/// ACP has no frame chunking, so one line must carry a whole prompt including
/// base64 images: the cap sits above the RPC transport's 1 MiB frame and below
/// its 64 MiB reassembled ceiling (`rpc::frame`). It exists so an unterminated
/// line cannot grow the process without bound. An over-long line is answered
/// with `-32600` and the remainder of that line is discarded.
pub const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// Threads that run inbound handlers.
///
/// Handlers block for a whole prompt turn, so more than one is required for
/// `session/cancel` (and any reverse-call answer) to make progress. The count
/// is fixed so a client cannot pin an arbitrary number of threads.
const HANDLER_WORKERS: usize = 8;

/// Inbound frames waiting for a free handler. Beyond this the peer is
/// outrunning the agent; requests are refused with `-32603`, notifications are
/// dropped.
const MAX_QUEUED_FRAMES: usize = 64;

/// Cancel-observation interval for a blocking reverse call.
pub(crate) const REPLY_POLL: Duration = Duration::from_millis(20);

/// Handler for inbound requests and notifications.
///
/// Runs on a transport-owned thread: it may block for a whole turn and may send
/// reverse requests, because the reader never waits for it. `notification` is
/// true for frames without an id; their errors are dropped, never written.
pub trait Dispatcher: Send + Sync + 'static {
	fn dispatch(
		&self,
		method: &str,
		params: Option<&Value>,
		notification: bool,
	) -> Result<Value, RequestError>;
}

/// Resolution of one outbound request.
pub(crate) enum Reply {
	Result(Value),
	Error(RequestError),
	/// Transport closed before the peer answered.
	Closed,
}

/// Bidirectional JSON-RPC 2.0 connection over a byte stream.
#[derive(Clone)]
pub struct Transport {
	inner: Arc<Inner>,
}

struct Inner {
	state:    Mutex<State>,
	wake:     Condvar,
	next_id:  AtomicI64,
	dispatch: Arc<dyn Dispatcher>,
	pool:     Pool,
	writer:   Mutex<Option<JoinHandle<()>>>,
}

#[derive(Default)]
struct State {
	closed:   Option<String>,
	write_tx: Option<Sender<Value>>,
	pending:  HashMap<JsonRpcId, Sender<Reply>>,
}

impl Transport {
	/// Start the writer thread. `writer` must be the process stdout in ACP mode:
	/// nothing else may put bytes on it.
	pub fn new<W: Write + Send + 'static>(
		writer: W,
		dispatcher: Arc<dyn Dispatcher>,
	) -> io::Result<Self> {
		let inner = Arc::new(Inner {
			state:    Mutex::new(State::default()),
			wake:     Condvar::new(),
			next_id:  AtomicI64::new(0),
			dispatch: dispatcher,
			pool:     Pool::new(HANDLER_WORKERS)?,
			writer:   Mutex::new(None),
		});
		let (tx, lines) = mpsc::channel();
		inner.lock().write_tx = Some(tx);
		let shared = Arc::downgrade(&inner);
		let handle = thread::Builder::new()
			.name("omp-acp-writer".to_string())
			.spawn(move || write_loop(writer, lines, shared))?;
		*inner.writer.lock().unwrap_or_else(PoisonError::into_inner) = Some(handle);
		Ok(Self { inner })
	}

	/// Read frames until EOF, then close. Blocks the calling thread; handlers
	/// run elsewhere, so this is the only place inbound frames are decoded.
	pub fn serve<R: BufRead>(&self, mut reader: R) {
		let mut line = Vec::new();
		loop {
			if self.inner.is_closed() {
				return;
			}
			match read_bounded(&mut reader, &mut line) {
				Ok(ReadState::Eof) => {
					self.inner.close(None);
					return;
				},
				Ok(ReadState::Overflow) => {
					let error = RequestError::invalid_request().with_detail(format!(
						"line exceeds {MAX_LINE_BYTES} bytes and was discarded"
					));
					self.inner.write(protocol::error_frame(None, &error));
				},
				Ok(ReadState::Line) => {
					// Upstream decodes with a non-fatal `TextDecoder`, so an
					// invalid byte becomes U+FFFD and JSON parsing rejects it.
					let text = String::from_utf8_lossy(&line);
					let text = text.trim_end();
					if !text.is_empty() {
						self.handle_line(text);
					}
				},
				Err(error) => {
					self.inner.close(Some(format!("cannot read ACP input: {error}")));
					return;
				},
			}
		}
	}

	/// Decode and route one inbound frame.
	///
	/// Crate-visible so the reverse-call tests can drive inbound frames without
	/// a reader thread; `serve` is the production caller.
	pub(crate) fn handle_line(&self, line: &str) {
		match protocol::parse_line(line) {
			Ok(None) => {},
			Err(LineError { id, error }) => {
				self.inner.write(protocol::error_frame(id.as_ref(), &error));
			},
			Ok(Some(Incoming::Response { id, outcome })) => {
				self.inner.deliver(&id, outcome);
			},
			Ok(Some(Incoming::Notification { method, params })) => {
				if method != protocol::CANCEL_REQUEST_METHOD {
					self.queue(method, params, None);
				}
			},
			Ok(Some(Incoming::Request { id, method, params })) => {
				if method == protocol::CANCEL_REQUEST_METHOD {
					// `connection.ts:266`: upstream returns without failing, so
					// the request form gets `{}` (`result ?? {}`,
					// `transport.ts:213`) and the notification form is silent.
					// The dispatcher never sees this method.
					self.inner.write(protocol::result_frame(&id, Value::Null));
					return;
				}
				self.queue(method, params, Some(id));
			},
		}
	}

	/// Queue one frame for the writer. `false` once the connection is closed.
	pub fn write(&self, message: Value) -> bool {
		self.inner.write(message)
	}

	/// Queue a notification (`session/update` and friends).
	pub fn notify(&self, method: &str, params: Option<&Value>) -> bool {
		self.inner.write(protocol::notification_frame(method, params))
	}

	/// Close the connection and reject every pending reverse call.
	pub fn close(&self, reason: Option<String>) {
		self.inner.close(reason);
	}

	/// True once closed, by EOF, write error or [`Self::close`].
	pub fn is_closed(&self) -> bool {
		self.inner.is_closed()
	}

	/// Why the connection closed, if it has.
	pub fn closed_reason(&self) -> Option<String> {
		self.inner.lock().closed.clone()
	}

	/// Wait for close. `true` when closed, `false` on timeout.
	pub fn wait_closed(&self, timeout: Option<Duration>) -> bool {
		let mut state = self.inner.lock();
		if state.closed.is_some() {
			return true;
		}
		match timeout {
			Some(timeout) => {
				let (state, _) = self
					.inner
					.wake
					.wait_timeout_while(state, timeout, |state| state.closed.is_none())
					.unwrap_or_else(PoisonError::into_inner);
				state.closed.is_some()
			},
			None => {
				let state = self
					.inner
					.wake
					.wait_while(state, |state| state.closed.is_none())
					.unwrap_or_else(PoisonError::into_inner);
				state.closed.is_some()
			},
		}
	}

	/// Close and wait for queued frames to flush, returning the close reason.
	pub fn finish(&self) -> Option<String> {
		self.inner.close(None);
		let writer = self.inner.writer.lock().unwrap_or_else(PoisonError::into_inner).take();
		if let Some(writer) = writer {
			let _ = writer.join();
		}
		self.closed_reason()
	}

	/// Outbound requests still waiting for a reply.
	pub fn pending_len(&self) -> usize {
		self.inner.lock().pending.len()
	}

	/// Next numeric request id; upstream starts at `0` (`transport.ts:113`).
	pub(crate) fn next_id(&self) -> JsonRpcId {
		JsonRpcId::number(self.inner.next_id.fetch_add(1, Ordering::Relaxed))
	}

	/// Register a pending slot before the request is written.
	pub(crate) fn register(&self, id: &JsonRpcId) -> Option<Receiver<Reply>> {
		self.inner.register(id)
	}

	/// Drop a pending slot (timeout, cancel, send failure).
	pub(crate) fn unregister(&self, id: &JsonRpcId) {
		self.inner.lock().pending.remove(id);
	}

	fn queue(&self, method: String, params: Option<Value>, reply_to: Option<JsonRpcId>) {
		let notification = reply_to.is_none();
		let failure_id = reply_to.clone();
		// Weak so a queued frame cannot keep the connection alive after the
		// last handle is dropped.
		let inner = Arc::downgrade(&self.inner);
		let queued = self.inner.pool.enqueue(Box::new(move || {
			let Some(inner) = inner.upgrade() else {
				return;
			};
			let dispatch = inner.dispatch.as_ref();
			let frame = match dispatch_guarded(dispatch, &method, params.as_ref(), notification) {
				Ok(result) => reply_to.map(|id| protocol::result_frame(&id, result)),
				Err(error) => reply_to.map(|id| protocol::error_frame(Some(&id), &error)),
			};
			if let Some(frame) = frame {
				inner.write(frame);
			}
		}));
		if queued {
			return;
		}
		// Refused: the connection is closing (the write below is a no-op then)
		// or the peer has outrun the queue.
		if let Some(id) = failure_id {
			let error = RequestError::internal_error().with_data(json!({
				"details": format!("handler queue is full; at most {MAX_QUEUED_FRAMES} frames wait"),
			}));
			self.inner.write(protocol::error_frame(Some(&id), &error));
		}
	}
}

impl FrameSink for Transport {
	fn send(&self, frame: Value) {
		self.inner.write(frame);
	}
}

impl Inner {
	fn lock(&self) -> MutexGuard<'_, State> {
		self.state.lock().unwrap_or_else(PoisonError::into_inner)
	}

	fn is_closed(&self) -> bool {
		self.lock().closed.is_some()
	}

	/// Queue one frame. Enqueueing under the state lock orders frames against
	/// close: a frame accepted before close is still flushed by the writer.
	fn write(&self, message: Value) -> bool {
		let state = self.lock();
		match state.write_tx.as_ref() {
			Some(tx) => tx.send(message).is_ok(),
			None => false,
		}
	}

	fn close(&self, reason: Option<String>) {
		let pending: Vec<Sender<Reply>> = {
			let mut state = self.lock();
			if state.closed.is_some() {
				return;
			}
			state.closed = Some(reason.unwrap_or_else(|| "Connection closed".to_string()));
			// Dropping the sender ends the writer thread after it flushes what
			// is already queued.
			state.write_tx = None;
			std::mem::take(&mut state.pending).into_values().collect()
		};
		self.wake.notify_all();
		self.pool.close();
		for sender in pending {
			let _ = sender.send(Reply::Closed);
		}
	}

	fn register(&self, id: &JsonRpcId) -> Option<Receiver<Reply>> {
		let (tx, rx) = mpsc::channel();
		let mut state = self.lock();
		if state.closed.is_some() {
			return None;
		}
		state.pending.insert(id.clone(), tx);
		Some(rx)
	}

	/// Resolve one pending request; `false` when the id is unknown (late reply
	/// after a timeout, or a reply to a request we never sent).
	fn deliver(&self, id: &JsonRpcId, outcome: Result<Value, RequestError>) -> bool {
		let sender = self.lock().pending.remove(id);
		let Some(sender) = sender else {
			return false;
		};
		let reply = match outcome {
			Ok(value) => Reply::Result(value),
			Err(error) => Reply::Error(error),
		};
		sender.send(reply).is_ok()
	}
}

fn write_loop<W: Write>(mut writer: W, lines: Receiver<Value>, shared: Weak<Inner>) {
	for message in lines {
		let mut line = message.to_string();
		line.push('\n');
		if let Err(error) = writer.write_all(line.as_bytes()).and_then(|()| writer.flush()) {
			if let Some(inner) = shared.upgrade() {
				inner.close(Some(format!("cannot write ACP frame: {error}")));
			}
			return;
		}
	}
}

/// Run one handler, turning an escaping panic into `-32603` with the message in
/// `data.details` (`transport.ts:216-220`).
fn dispatch_guarded(
	dispatch: &dyn Dispatcher,
	method: &str,
	params: Option<&Value>,
	notification: bool,
) -> Result<Value, RequestError> {
	match catch_unwind(AssertUnwindSafe(|| dispatch.dispatch(method, params, notification))) {
		Ok(result) => result,
		Err(payload) => {
			let detail = payload
				.downcast_ref::<&str>()
				.map(|detail| (*detail).to_string())
				.or_else(|| payload.downcast_ref::<String>().cloned())
				.unwrap_or_else(|| format!("handler panicked in {method}"));
			Err(RequestError::internal_error().with_data(json!({ "details": detail })))
		},
	}
}

enum ReadState {
	Line,
	/// Line longer than [`MAX_LINE_BYTES`]; the remainder was drained.
	Overflow,
	Eof,
}

/// Read one physical line, bounded by [`MAX_LINE_BYTES`].
///
/// The cap is enforced while consuming, so an unterminated line can never grow
/// the buffer past the cap; the remainder of an over-long line is drained here
/// so the next frame starts at a line boundary.
fn read_bounded<R: BufRead>(reader: &mut R, line: &mut Vec<u8>) -> io::Result<ReadState> {
	line.clear();
	let mut overflow = false;
	loop {
		let available = reader.fill_buf()?;
		if available.is_empty() {
			if overflow {
				return Ok(ReadState::Overflow);
			}
			return Ok(if line.is_empty() { ReadState::Eof } else { ReadState::Line });
		}
		let newline = available.iter().position(|byte| *byte == b'\n');
		let take = newline.map_or(available.len(), |index| index + 1);
		if !overflow {
			if line.len() + take > MAX_LINE_BYTES {
				overflow = true;
			} else {
				line.extend_from_slice(&available[..take]);
			}
		}
		reader.consume(take);
		if newline.is_some() {
			return Ok(if overflow { ReadState::Overflow } else { ReadState::Line });
		}
	}
}

/// Bounded owner queue for inbound frames.
struct Pool {
	shared: Arc<PoolState>,
}

struct PoolState {
	queued: Mutex<Queue>,
	wake:   Condvar,
}

#[derive(Default)]
struct Queue {
	jobs:   VecDeque<Job>,
	closed: bool,
}

type Job = Box<dyn FnOnce() + Send + 'static>;

impl Pool {
	fn new(workers: usize) -> io::Result<Self> {
		let shared = Arc::new(PoolState {
			queued: Mutex::new(Queue::default()),
			wake:   Condvar::new(),
		});
		for index in 0..workers {
			let state = Arc::clone(&shared);
			let spawned = thread::Builder::new()
				.name(format!("omp-acp-handler-{index}"))
				.spawn(move || run_worker(state));
			if let Err(error) = spawned {
				close_workers(&shared);
				return Err(error);
			}
		}
		Ok(Self { shared })
	}

	fn enqueue(&self, job: Job) -> bool {
		let mut queued = self.shared.lock();
		if queued.closed || queued.jobs.len() >= MAX_QUEUED_FRAMES {
			return false;
		}
		queued.jobs.push_back(job);
		drop(queued);
		self.shared.wake.notify_one();
		true
	}

	fn close(&self) {
		close_workers(&self.shared);
	}
}

impl Drop for Pool {
	fn drop(&mut self) {
		// Handlers finish their frame and exit; the threads are detached so a
		// turn in flight cannot block shutdown here.
		close_workers(&self.shared);
	}
}

impl PoolState {
	fn lock(&self) -> MutexGuard<'_, Queue> {
		self.queued.lock().unwrap_or_else(PoisonError::into_inner)
	}
}

fn close_workers(state: &PoolState) {
	{
		let mut queued = state.lock();
		queued.closed = true;
		queued.jobs.clear();
	}
	state.wake.notify_all();
}

fn run_worker(state: Arc<PoolState>) {
	loop {
		let job = {
			let mut queued = state.lock();
			while queued.jobs.is_empty() && !queued.closed {
				queued = state.wake.wait(queued).unwrap_or_else(PoisonError::into_inner);
			}
			if queued.closed {
				return;
			}
			queued.jobs.pop_front()
		};
		if let Some(job) = job {
			job();
		}
	}
}

#[cfg(test)]
pub(crate) mod test_support {
	use std::{io::{self, Write}, sync::{Arc, Condvar, Mutex, PoisonError}};

	use serde_json::{Value, json};

	use crate::acp::protocol::RequestError;

	use super::{Dispatcher, Transport};

	/// In-memory stdout that wakes waiters on every flush.
	#[derive(Clone, Default)]
	pub(crate) struct RecordingWriter {
		bytes: Arc<(Mutex<Vec<u8>>, Condvar)>,
	}

	impl RecordingWriter {
		pub(crate) fn frames(&self) -> Vec<Value> {
			let (bytes, _) = &*self.bytes;
			let bytes = bytes.lock().unwrap_or_else(PoisonError::into_inner);
			parse_frames(&bytes[..])
		}

		/// Block until a frame matching `predicate` has been written.
		pub(crate) fn wait_frame(&self, predicate: impl Fn(&Value) -> bool) -> Value {
			self.wait_frames(1, predicate).remove(0)
		}

		/// Block until `count` frames matching `predicate` have been written.
		pub(crate) fn wait_frames(
			&self,
			count: usize,
			predicate: impl Fn(&Value) -> bool,
		) -> Vec<Value> {
			let (bytes, wake) = &*self.bytes;
			let mut bytes = bytes.lock().unwrap_or_else(PoisonError::into_inner);
			loop {
				let matched: Vec<Value> = parse_frames(&bytes[..])
					.into_iter()
					.filter(|frame| predicate(frame))
					.collect();
				if matched.len() >= count {
					return matched;
				}
				bytes = wake.wait(bytes).unwrap_or_else(PoisonError::into_inner);
			}
		}
	}

	fn parse_frames(bytes: &[u8]) -> Vec<Value> {
		bytes
			.split(|byte| *byte == b'\n')
			.filter(|line| !line.is_empty())
			.map(|line| serde_json::from_slice(line).expect("written frame is JSON"))
			.collect()
	}

	impl Write for RecordingWriter {
		fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
			let (bytes, wake) = &*self.bytes;
			bytes.lock().unwrap_or_else(PoisonError::into_inner).extend_from_slice(buf);
			wake.notify_all();
			Ok(buf.len())
		}

		fn flush(&mut self) -> io::Result<()> {
			Ok(())
		}
	}

	/// Dispatcher that records what it was asked and answers `{"ok":true}`.
	#[derive(Default)]
	pub(crate) struct Recorder {
		seen: Mutex<Vec<(String, bool)>>,
		wake: Condvar,
	}

	impl Dispatcher for Recorder {
		fn dispatch(
			&self,
			method: &str,
			_params: Option<&Value>,
			notification: bool,
		) -> Result<Value, RequestError> {
			self
				.seen
				.lock()
				.unwrap_or_else(PoisonError::into_inner)
				.push((method.to_string(), notification));
			self.wake.notify_all();
			Ok(json!({ "ok": true }))
		}
	}

	impl Recorder {
		pub(crate) fn wait_seen(&self, count: usize) -> Vec<(String, bool)> {
			let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
			while seen.len() < count {
				seen = self.wake.wait(seen).unwrap_or_else(PoisonError::into_inner);
			}
			seen.clone()
		}

		pub(crate) fn seen(&self) -> Vec<(String, bool)> {
			self.seen.lock().unwrap_or_else(PoisonError::into_inner).clone()
		}
	}

	pub(crate) fn transport_with(writer: &RecordingWriter) -> (Transport, Arc<Recorder>) {
		let recorder = Arc::new(Recorder::default());
		let transport = Transport::new(writer.clone(), Arc::clone(&recorder))
			.expect("transport starts its writer thread");
		(transport, recorder)
	}

	/// One newline-delimited frame, as a peer would write it.
	pub(crate) fn line(value: Value) -> String {
		value.to_string()
	}
}

#[cfg(test)]
mod tests {
	use serde_json::{Value, json};

	use super::{MAX_LINE_BYTES, test_support::{RecordingWriter, line, transport_with}};

	#[test]
	fn request_ids_are_echoed_and_notifications_get_no_reply() {
		let writer = RecordingWriter::default();
		let (transport, recorder) = transport_with(&writer);

		transport.handle_line(&line(json!({
			"jsonrpc": "2.0",
			"id": "client-1",
			"method": "session/cancel",
			"params": { "sessionId": "s" }
		})));
		let reply = writer.wait_frame(|frame| frame["id"] == json!("client-1"));
		assert_eq!(reply["result"], json!({ "ok": true }));
		assert_eq!(recorder.wait_seen(1), vec![("session/cancel".to_string(), false)]);

		transport.handle_line(&line(json!({ "jsonrpc": "2.0", "method": "session/cancel" })));
		assert_eq!(
			recorder.wait_seen(2),
			vec![("session/cancel".to_string(), false), ("session/cancel".to_string(), true)]
		);
		assert_eq!(writer.frames().len(), 1);
	}

	#[test]
	fn malformed_lines_are_answered_and_the_connection_stays_up() {
		let writer = RecordingWriter::default();
		let (transport, recorder) = transport_with(&writer);

		transport.handle_line("not json");
		let reply = writer.wait_frame(|frame| frame["error"]["code"] == json!(-32700));
		assert_eq!(reply["id"], Value::Null);

		transport.handle_line(&line(json!({ "jsonrpc": "1.0", "id": 4, "method": "ping" })));
		let reply = writer.wait_frame(|frame| frame["id"] == json!(4));
		assert_eq!(reply["error"]["code"], json!(-32600));

		transport.handle_line(&line(json!({ "jsonrpc": "2.0", "id": 5, "method": "ping" })));
		let reply = writer.wait_frame(|frame| frame["id"] == json!(5));
		assert_eq!(reply["result"], json!({ "ok": true }));
		assert_eq!(recorder.seen(), vec![("ping".to_string(), false)]);
	}

	#[test]
	fn cancel_request_is_answered_without_reaching_the_dispatcher() {
		let writer = RecordingWriter::default();
		let (transport, recorder) = transport_with(&writer);

		transport.handle_line(&line(json!({
			"jsonrpc": "2.0",
			"id": 1,
			"method": "$/cancel_request"
		})));
		let reply = writer.wait_frame(|frame| frame["id"] == json!(1));
		assert_eq!(reply["result"], json!({}));

		// The notification form produces no frame at all.
		transport.handle_line(&line(json!({ "jsonrpc": "2.0", "method": "$/cancel_request" })));
		transport.handle_line(&line(json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" })));
		writer.wait_frame(|frame| frame["id"] == json!(2));
		assert_eq!(recorder.seen(), vec![("ping".to_string(), false)]);
	}

	#[test]
	fn serve_reports_parse_errors_and_flushes_before_close() {
		let writer = RecordingWriter::default();
		let (transport, _recorder) = transport_with(&writer);

		// Invalid UTF-8 decodes to U+FFFD, which is not JSON: -32700, then EOF.
		transport.serve(&[0xff_u8, b'\n'][..]);
		assert!(transport.is_closed());
		transport.finish();

		let frames = writer.frames();
		assert_eq!(frames.len(), 1);
		assert_eq!(frames[0]["error"]["code"], json!(-32700));
	}

	#[test]
	fn overlong_lines_are_discarded_without_desynchronising() {
		let writer = RecordingWriter::default();
		let (transport, _recorder) = transport_with(&writer);

		// The second line is a parse error, answered inline, so this asserts the
		// reader resumed exactly at the next line boundary after draining.
		let mut input = vec![b'a'; MAX_LINE_BYTES + 2];
		input.push(b'\n');
		input.extend_from_slice(b"zzz\n");
		transport.serve(&input[..]);
		assert!(transport.is_closed());
		transport.finish();

		let frames = writer.frames();
		assert_eq!(frames.len(), 2);
		assert_eq!(frames[0]["error"]["code"], json!(-32600));
		assert_eq!(frames[0]["id"], Value::Null);
		assert_eq!(frames[1]["error"]["code"], json!(-32700));
	}
}
