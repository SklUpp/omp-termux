//! ACP stdio transport: newline-delimited JSON-RPC 2.0 with one serialized
//! writer and a reader that never waits for a handler.
//!
//! Ported from `packages/utils/src/acp/stream.ts` (`ndJsonStream`: `\n`
//! delimited JSON, trailing whitespace trimmed, blank lines skipped, a final
//! unterminated line still delivered) and `packages/utils/src/acp/transport.ts`
//! (`RpcConnection`: monotonic request ids, pending-table correlation,
//! `close()` rejecting every pending call).
//!
//! Four properties this transport must keep:
//!
//! * **Ordering.** Every frame leaves through one writer thread, so updates a
//!   turn emits and the response that ends it cannot interleave on the wire.
//! * **Liveness.** Responses are correlated on the reader thread, before any
//!   handler runs. A handler may block for a whole turn — or wait for the
//!   client — without delaying the answer that unblocks it.
//! * **Fail closed.** EOF, a read error, a write error, outbound queue
//!   saturation, inbound queue saturation and an explicit close all reject
//!   every pending reverse call and fire [`Dispatcher::on_closed`] exactly once.
//!   Nothing is fabricated. `close` (or EOF) is the shutdown path; dropping the
//!   last handle ends the writer queue and disconnects waiters that no running
//!   handler still holds.
//! * **Cancellation is not queued.** Every inbound request and notification
//!   first reaches [`Dispatcher::notify_control`] on the reader thread, so a
//!   `session/cancel` is signalled even while every handler is mid-turn and the
//!   inbound queue is full. The hook only signals; it never waits for a turn.
//!
//! Local limits, none of which upstream has (it buffers an unbounded string and
//! dispatches an unbounded number of promises): one inbound line is capped at
//! [`MAX_LINE_BYTES`], outbound frames wait in a bounded channel
//! ([`MAX_QUEUED_OUTBOUND`]) and inbound frames wait in a bounded owner queue
//! (`HANDLER_WORKERS` threads, `MAX_QUEUED_FRAMES` slots). Behaviour on
//! refusal: an inbound request gets `-32603` (`data.details` names the queue),
//! a notification is dropped, and either way the saturated connection then
//! closes so no cancel can hide behind a full queue; an over-long line is
//! answered with `-32600` while the rest of that physical line is drained so
//! the next frame still parses, and a full outbound queue closes the connection
//! instead of growing without bound. [`Transport::finish`] bounds the writer
//! join; see [`FinishOutcome`].

use std::{
	collections::{HashMap, VecDeque},
	io::{self, BufRead, Write},
	panic::{AssertUnwindSafe, catch_unwind},
	sync::{
		Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak,
		atomic::{AtomicI64, Ordering},
		mpsc::{self, Receiver, Sender, SyncSender, TrySendError},
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

/// Outbound frames waiting for the writer. A peer that stops draining stdout
/// (blocked pipe, suspended terminal) must not grow the process without bound;
/// on saturation the connection fails closed rather than buffering.
const MAX_QUEUED_OUTBOUND: usize = 256;

/// Cancel-observation interval for a blocking reverse call.
pub(crate) const REPLY_POLL: Duration = Duration::from_millis(20);

/// Handler for inbound requests and notifications.
///
/// Runs on a transport-owned thread: it may block for a whole turn and may send
/// reverse requests, because the reader never waits for it. `notification` is
/// true for frames without an id; their errors are dropped, never written.
///
/// The two hooks run on other threads and must follow their contracts: a hook
/// may run while a `dispatch` on another thread holds the adapter's session
/// lock, and [`Self::on_closed`] may run on a handler thread (an adapter that
/// closes from inside `dispatch`), so neither may wait for a turn or call
/// [`Transport::finish`]/[`Transport::write`].
pub trait Dispatcher: Send + Sync + 'static {
	fn dispatch(
		&self,
		method: &str,
		params: Option<&Value>,
		notification: bool,
	) -> Result<Value, RequestError>;

	/// Called right after a successful request response frame entered the
	/// writer queue, so it is FIFO-ordered before any later session/update
	/// frame. Used to start a detached post-response task (plan reminder
	/// continuation); never for notifications or failures.
	fn after_response(&self, _method: &str, _params: Option<&Value>) {}

	/// Reader-thread control hook, called for every inbound request and
	/// notification before it is queued (responses are correlated and never
	/// reach it). `notification` distinguishes the two frame forms so the
	/// dispatcher can validate which methods are legal as a request.
	///
	/// `true` means the dispatcher handled it: a notification gets no reply, a
	/// request gets `{}` (only methods that are contractually idempotent in
	/// their request form, like `session/cancel`, may return `true`); the
	/// method never enters the handler pool.
	///
	/// This is how `session/cancel` keeps liveness: handlers may all be
	/// mid-turn and the inbound queue full, while the reader still delivers the
	/// signal. MUST only signal — set the session's cancel flag, wake waiters,
	/// return. MUST NOT block, wait for a turn, or take a lock a running
	/// `dispatch` holds for the length of the turn. A panic here is contained
	/// and fails the connection closed, so a cancel is never silently lost.
	fn notify_control(&self, method: &str, params: Option<&Value>, notification: bool) -> bool;

	/// Called exactly once, on the thread that observed the first close (EOF,
	/// read error, write error, outbound queue saturation, explicit close).
	///
	/// Runs after the latch is set, the writer queue is dropped, the handler
	/// pool is cleared and every pending reverse call has been rejected. MUST
	/// only signal: cancel every active session, wake its waiters, return. MUST
	/// NOT wait for a turn, acquire a turn lock, or call [`Transport::finish`]
	/// (self-join from the writer thread) or [`Transport::write`] (the sender is
	/// already gone). A panic here is contained; it never suppresses the close.
	fn on_closed(&self, reason: &str);
}

/// Resolution of one outbound request.
pub(crate) enum Reply {
	Result(Value),
	Error(RequestError),
	/// Transport closed before the peer answered.
	Closed,
}

/// Result of [`Transport::finish`].
///
/// A generic [`std::io::Write`] writer thread cannot be cancelled or killed:
/// once it is blocked inside `write`, only the peer unblocking it makes
/// progress. So the join is bounded by the caller's budget. `flushed: false`
/// means either the writer did not exit in time (frames still queued may never
/// reach the peer; the thread stays blocked and detached — it is NOT reaped)
/// or it exited after a write/flush error (the tail was lost). Only a clean
/// drain within the budget reports `true`; the caller must not assume output
/// is complete otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinishOutcome {
	/// Why the connection closed.
	pub reason: Option<String>,
	/// True only when the writer drained its queue and exited without error
	/// within the budget.
	pub flushed: bool,
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
	/// `Some(true)` once the writer drained its queue and exited without a
	/// write/flush error, `Some(false)` after an error, `None` while running.
	/// Waited on by [`Transport::finish`] so the writer join has a budget.
	writer_gone: Mutex<Option<bool>>,
	writer_wake: Condvar,
}

#[derive(Default)]
struct State {
	closed:   Option<String>,
	write_tx: Option<SyncSender<Value>>,
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
			writer_gone: Mutex::new(None),
			writer_wake: Condvar::new(),
		});
		let (tx, lines) = mpsc::sync_channel(MAX_QUEUED_OUTBOUND);
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
			Err(LineError { id, error, response_shaped }) => {
				if response_shaped {
					// A malformed answer to one of our calls: fail the matching
					// pending call, never echo `-32600`, and never touch a
					// pending slot for an unknown or missing id.
					if let Some(id) = &id {
						self.inner.deliver(id, Err(error));
					}
				} else {
					// A malformed peer request: answer it; our pending calls
					// are unrelated even when the id numerically collides.
					self.inner.write(protocol::error_frame(id.as_ref(), &error));
				}
			},
			Ok(Some(Incoming::Response { id, outcome })) => {
				self.inner.deliver(&id, outcome);
			},
			Ok(Some(Incoming::Notification { method, params })) => {
				if method == protocol::CANCEL_REQUEST_METHOD {
					return;
				}
				if self.control(&method, params.as_ref(), true) {
					return;
				}
				self.queue(method, params, None);
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
				if self.control(&method, params.as_ref(), false) {
					// The dispatcher claimed this method in its request form
					// (e.g. `session/cancel`); it is contractually idempotent,
					// so `{}` matches the `$/cancel_request` answer. A close
					// caused by a panicking hook makes this write a no-op
					// instead of fabricating success.
					self.inner.write(protocol::result_frame(&id, Value::Null));
					return;
				}
				self.queue(method, params, Some(id));
			},
		}
	}

	/// Offer one inbound request/notification to the reader-thread control
	/// hook. `true` means consumed (handled, or the hook panicked and the
	/// connection was closed fail-closed so the cancel is not lost).
	fn control(&self, method: &str, params: Option<&Value>, notification: bool) -> bool {
		let result = catch_unwind(AssertUnwindSafe(|| {
			self.inner.dispatch.notify_control(method, params, notification)
		}));
		match result {
			Ok(handled) => handled,
			Err(_) => {
				self.inner.close(Some(format!("dispatcher control hook panicked in {method}")));
				true
			},
		}
	}

	/// Queue one frame for the writer. `false` once the connection is closed,
	/// including when this frame saturated the outbound queue and closed it.
	pub fn write(&self, message: Value) -> bool {
		self.inner.write(message)
	}

	/// Queue a notification (`session/update` and friends); same close
	/// semantics as [`Self::write`].
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

	/// Close, then wait at most `flush_budget` for the writer to drain and
	/// exit.
	///
	/// The writer is a thread blocked in a generic [`Write`]; it cannot be
	/// killed, so the join is bounded. On budget exhaustion the thread is
	/// detached and reported as `flushed: false` — never claim it was reaped.
	/// `flushed` also stays `false` when the writer exited after a write or
	/// flush error: the queued tail never reached the peer.
	/// Handler threads are never joined here; their quiescence is the adapter's
	/// turn barrier (see [`Dispatcher::on_closed`]).
	pub fn finish(&self, flush_budget: Duration) -> FinishOutcome {
		self.inner.close(None);
		let writer = self.inner.writer.lock().unwrap_or_else(PoisonError::into_inner).take();
		let flushed = match writer {
			None => self.inner.writer_clean() == Some(true),
			Some(writer) => {
				let gone = self.inner.writer_gone.lock().unwrap_or_else(PoisonError::into_inner);
				let (gone, _) = self
					.inner
					.writer_wake
					.wait_timeout_while(gone, flush_budget, |gone| gone.is_none())
					.unwrap_or_else(PoisonError::into_inner);
				let clean = *gone == Some(true);
				let exited = gone.is_some();
				drop(gone);
				if exited {
					let _ = writer.join();
				} else {
					// Detached: dropping the handle does not stop the blocked
					// `write`, and the queued bytes may be lost.
					drop(writer);
				}
				clean
			},
		};
		FinishOutcome { reason: self.closed_reason(), flushed }
	}

	/// Writer exit state: `None` while running, `Some(true)` after a clean
	/// drain, `Some(false)` after a write or flush error.
	pub(crate) fn writer_finished(&self) -> Option<bool> {
		self.inner.writer_clean()
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
			let outcome = dispatch_guarded(dispatch, &method, params.as_ref(), notification);
			let ok = outcome.is_ok();
			let frame = match outcome {
				Ok(result) => reply_to.map(|id| protocol::result_frame(&id, result)),
				Err(error) => reply_to.map(|id| protocol::error_frame(Some(&id), &error)),
			};
			if let Some(frame) = frame {
				inner.write(frame);
				if ok && !notification {
					dispatch.after_response(&method, params.as_ref());
				}
			}
		}));
		if queued {
			return;
		}
		// Refused: the connection is already closing (the write above is then a
		// no-op) or the peer has outrun the handlers. Answer the request and
		// fail closed: queued frames were cleared, so nothing else will ever
		// run, and the adapter's `on_closed` is the only way a wedged turn
		// learns to stop. The error frame is enqueued before the write sender
		// is dropped, so it still flushes.
		if let Some(id) = failure_id {
			let error = RequestError::internal_error().with_data(json!({
				"details": format!("handler queue is full; at most {MAX_QUEUED_FRAMES} frames wait"),
			}));
			self.inner.write(protocol::error_frame(Some(&id), &error));
		}
		self.inner.close(Some(format!(
			"inbound handler queue is full; at most {MAX_QUEUED_FRAMES} frames wait"
		)));
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

	/// Queue one frame without blocking. Enqueueing under the state lock orders
	/// frames against close: a frame accepted before close is still flushed by
	/// the writer. A full queue means the peer stopped draining: fail closed
	/// outside the state lock instead of buffering without bound.
	fn write(&self, message: Value) -> bool {
		enum Sent {
			Queued,
			Closed,
			Full,
		}
		let sent = {
			let state = self.lock();
			match state.write_tx.as_ref() {
				Some(tx) => match tx.try_send(message) {
					Ok(()) => Sent::Queued,
					Err(TrySendError::Full(_)) => Sent::Full,
					Err(TrySendError::Disconnected(_)) => Sent::Closed,
				},
				None => Sent::Closed,
			}
		};
		match sent {
			Sent::Queued => true,
			Sent::Closed => false,
			Sent::Full => {
				self.close(Some(format!(
					"outbound frame queue is full; at most {MAX_QUEUED_OUTBOUND} frames wait"
				)));
				false
			},
		}
	}

	/// First close wins. Pending reverse calls are rejected, the handler pool is
	/// cleared and [`Dispatcher::on_closed`] runs exactly once, after that
	/// teardown and outside the state lock (an adapter may call `close` from a
	/// handler, so the hook must not need a turn lock). The hook's panic is
	/// contained: it never re-enters close and never suppresses the rejection.
	fn close(&self, reason: Option<String>) {
		let reason = reason.unwrap_or_else(|| "Connection closed".to_string());
		let pending: Vec<Sender<Reply>> = {
			let mut state = self.lock();
			if state.closed.is_some() {
				return;
			}
			state.closed = Some(reason.clone());
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
		let _ = catch_unwind(AssertUnwindSafe(|| self.dispatch.on_closed(&reason)));
	}

	/// Writer exit state; see [`FinishOutcome::flushed`].
	fn writer_clean(&self) -> Option<bool> {
		*self.writer_gone.lock().unwrap_or_else(PoisonError::into_inner)
	}

	/// Writer-thread exit signal for [`Transport::finish`]; `clean` is false
	/// after a write or flush error.
	fn writer_finished(&self, clean: bool) {
		*self.writer_gone.lock().unwrap_or_else(PoisonError::into_inner) = Some(clean);
		self.writer_wake.notify_all();
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
	/// after a timeout, or a reply to a request we never sent). The slot is
	/// removed even when its waiter already dropped the receiver.
	fn deliver(&self, id: &JsonRpcId, outcome: Result<Value, RequestError>) -> bool {
		let sender = self.lock().pending.remove(id);
		let Some(sender) = sender else {
			return false;
		};
		let reply = match outcome {
			Ok(value) => Reply::Result(value),
			Err(error) => Reply::Error(error),
		};
		let _ = sender.send(reply);
		true
	}
}

fn write_loop<W: Write>(mut writer: W, lines: Receiver<Value>, shared: Weak<Inner>) {
	let mut clean = true;
	for message in lines {
		let mut line = message.to_string();
		line.push('\n');
		if let Err(error) = writer.write_all(line.as_bytes()).and_then(|()| flush_retry(&mut writer)) {
			clean = false;
			if let Some(inner) = shared.upgrade() {
				inner.close(Some(format!("cannot write ACP frame: {error}")));
			}
			break;
		}
	}
	// Every exit path signals: `finish` joins only after this latch and
	// reports `flushed` only for a clean drain.
	if let Some(inner) = shared.upgrade() {
		inner.writer_finished(clean);
	}
}

/// Flush, retrying a signal-interrupted flush instead of treating it as a
/// broken stdout. (`write_all` already retries `Interrupted`.)
fn flush_retry<W: Write>(writer: &mut W) -> io::Result<()> {
	loop {
		match writer.flush() {
			Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
			other => return other,
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
		let available = match reader.fill_buf() {
			Ok(available) => available,
			// A signal does not break the stream; only a real error does.
			Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
			Err(error) => return Err(error),
		};
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

		fn notify_control(
			&self,
			_method: &str,
			_params: Option<&Value>,
			_notification: bool,
		) -> bool {
			false
		}

		fn on_closed(&self, _reason: &str) {}
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
		let transport = Transport::new(writer.clone(), recorder.clone())
			.expect("transport starts its writer thread");
		(transport, recorder)
	}

	/// Transport over `writer` running a caller-supplied dispatcher.
	pub(crate) fn transport_with_dispatcher<W: Write + Send + 'static, D: Dispatcher>(
		writer: W,
		dispatcher: Arc<D>,
	) -> Transport {
		Transport::new(writer, dispatcher).expect("transport starts its writer thread")
	}

	/// One newline-delimited frame, as a peer would write it.
	pub(crate) fn line(value: Value) -> String {
		value.to_string()
	}
}

#[cfg(test)]
mod tests {
	use std::{
		io::{self, Write},
		panic::{AssertUnwindSafe, catch_unwind},
		sync::{Arc, Condvar, Mutex, PoisonError, mpsc},
		thread,
		time::{Duration, Instant},
	};

	use serde_json::{Value, json};

	use crate::acp::protocol::RequestError;

	use super::{
		FinishOutcome, HANDLER_WORKERS, JsonRpcId, MAX_LINE_BYTES, MAX_QUEUED_FRAMES,
		MAX_QUEUED_OUTBOUND, Reply,
		test_support::{RecordingWriter, line, transport_with, transport_with_dispatcher},
	};

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
		transport.finish(Duration::from_secs(1));

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
		transport.finish(Duration::from_secs(1));

		let frames = writer.frames();
		assert_eq!(frames.len(), 2);
		assert_eq!(frames[0]["error"]["code"], json!(-32600));
		assert_eq!(frames[0]["id"], Value::Null);
		assert_eq!(frames[1]["error"]["code"], json!(-32700));
	}

	/// Handler that wedges every worker until `on_closed` cancels it, and then
	/// until the test releases it. `on_closed` only signals, like a real
	/// adapter: set the flag, wake waiters, return.
	#[derive(Default)]
	struct Gate {
		started:      Mutex<usize>,
		start_wake:   Condvar,
		cancel:       Mutex<bool>,
		cancel_wake:  Condvar,
		release:      Mutex<bool>,
		release_wake: Condvar,
		finished:     Mutex<usize>,
		finish_wake:  Condvar,
		closes:       Mutex<Vec<String>>,
		controls:     Mutex<Vec<bool>>,
	}

	impl Gate {
		fn wait_started(&self, count: usize) {
			let mut started = self.started.lock().unwrap_or_else(PoisonError::into_inner);
			while *started < count {
				started = self.start_wake.wait(started).unwrap_or_else(PoisonError::into_inner);
			}
		}

		fn count_started(&self) -> usize {
			*self.started.lock().unwrap_or_else(PoisonError::into_inner)
		}

		fn count_finished(&self) -> usize {
			*self.finished.lock().unwrap_or_else(PoisonError::into_inner)
		}

		fn wait_finished(&self, count: usize) {
			let mut finished = self.finished.lock().unwrap_or_else(PoisonError::into_inner);
			while *finished < count {
				finished = self.finish_wake.wait(finished).unwrap_or_else(PoisonError::into_inner);
			}
		}

		fn release(&self) {
			*self.release.lock().unwrap_or_else(PoisonError::into_inner) = true;
			self.release_wake.notify_all();
		}

		fn close_reasons(&self) -> Vec<String> {
			self.closes.lock().unwrap_or_else(PoisonError::into_inner).clone()
		}

		fn controls(&self) -> Vec<bool> {
			self.controls.lock().unwrap_or_else(PoisonError::into_inner).clone()
		}
	}

	impl super::Dispatcher for Gate {
		fn dispatch(
			&self,
			_method: &str,
			_params: Option<&Value>,
			_notification: bool,
		) -> Result<Value, RequestError> {
			*self.started.lock().unwrap_or_else(PoisonError::into_inner) += 1;
			self.start_wake.notify_all();
			let mut cancelled = self.cancel.lock().unwrap_or_else(PoisonError::into_inner);
			while !*cancelled {
				cancelled = self.cancel_wake.wait(cancelled).unwrap_or_else(PoisonError::into_inner);
			}
			drop(cancelled);
			let mut released = self.release.lock().unwrap_or_else(PoisonError::into_inner);
			while !*released {
				released = self.release_wake.wait(released).unwrap_or_else(PoisonError::into_inner);
			}
			drop(released);
			*self.finished.lock().unwrap_or_else(PoisonError::into_inner) += 1;
			self.finish_wake.notify_all();
			Err(RequestError::request_cancelled())
		}

		fn notify_control(
			&self,
			method: &str,
			_params: Option<&Value>,
			notification: bool,
		) -> bool {
			if method != "session/cancel" {
				return false;
			}
			self.controls.lock().unwrap_or_else(PoisonError::into_inner).push(notification);
			true
		}

		fn on_closed(&self, reason: &str) {
			self.closes.lock().unwrap_or_else(PoisonError::into_inner).push(reason.to_string());
			*self.cancel.lock().unwrap_or_else(PoisonError::into_inner) = true;
			self.cancel_wake.notify_all();
		}
	}

	/// Writer that blocks inside `write` until [`Self::release`].
	#[derive(Clone, Default)]
	struct StalledWriter {
		entered: Arc<(Mutex<bool>, Condvar)>,
		release: Arc<(Mutex<bool>, Condvar)>,
	}

	impl StalledWriter {
		fn wait_entered(&self) {
			let (entered, wake) = &*self.entered;
			let mut entered = entered.lock().unwrap_or_else(PoisonError::into_inner);
			while !*entered {
				entered = wake.wait(entered).unwrap_or_else(PoisonError::into_inner);
			}
		}

		fn release(&self) {
			let (release, wake) = &*self.release;
			*release.lock().unwrap_or_else(PoisonError::into_inner) = true;
			wake.notify_all();
		}
	}

	impl Write for StalledWriter {
		fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
			let (entered, wake) = &*self.entered;
			*entered.lock().unwrap_or_else(PoisonError::into_inner) = true;
			wake.notify_all();
			let (release, wake) = &*self.release;
			let mut released = release.lock().unwrap_or_else(PoisonError::into_inner);
			while !*released {
				released = wake.wait(released).unwrap_or_else(PoisonError::into_inner);
			}
			Ok(buf.len())
		}

		fn flush(&mut self) -> io::Result<()> {
			Ok(())
		}
	}

	struct PanicClose;

	impl super::Dispatcher for PanicClose {
		fn dispatch(
			&self,
			_method: &str,
			_params: Option<&Value>,
			_notification: bool,
		) -> Result<Value, RequestError> {
			Ok(json!({}))
		}

		fn notify_control(
			&self,
			_method: &str,
			_params: Option<&Value>,
			_notification: bool,
		) -> bool {
			false
		}

		fn on_closed(&self, _reason: &str) {
			panic!("close hook panic");
		}
	}

	struct PanicControl;

	impl super::Dispatcher for PanicControl {
		fn dispatch(
			&self,
			_method: &str,
			_params: Option<&Value>,
			_notification: bool,
		) -> Result<Value, RequestError> {
			Ok(json!({}))
		}

		fn notify_control(
			&self,
			_method: &str,
			_params: Option<&Value>,
			_notification: bool,
		) -> bool {
			panic!("control hook panic");
		}

		fn on_closed(&self, _reason: &str) {}
	}

	#[test]
	fn inbound_saturation_answers_then_closes_once_and_drops_queued_frames() {
		let writer = RecordingWriter::default();
		let gate = Arc::new(Gate::default());
		let transport = transport_with_dispatcher(writer.clone(), Arc::clone(&gate));

		for index in 0..HANDLER_WORKERS {
			transport.handle_line(&line(json!({ "jsonrpc": "2.0", "id": index, "method": "turn" })));
		}
		gate.wait_started(HANDLER_WORKERS);
		for index in 0..MAX_QUEUED_FRAMES {
			transport.handle_line(&line(json!({
				"jsonrpc": "2.0",
				"id": 1000 + index,
				"method": "turn"
			})));
		}
		transport.handle_line(&line(json!({ "jsonrpc": "2.0", "id": 9999, "method": "turn" })));

		// The refused request is answered before the sender is dropped.
		let overflow = writer.wait_frame(|frame| frame["id"] == json!(9999));
		assert_eq!(overflow["error"]["code"], json!(-32603));
		assert!(transport.is_closed());
		let reason = transport.closed_reason().expect("saturation closes");
		assert!(reason.contains("inbound handler queue is full"), "{reason}");
		// Exactly one hook call, carrying the saturation reason, after the
		// queued frames were cleared: they never reach a handler.
		assert_eq!(gate.close_reasons(), vec![reason.clone()]);
		assert_eq!(gate.count_started(), HANDLER_WORKERS);

		// A later close is a no-op for the hook.
		transport.close(Some("again".to_string()));
		assert_eq!(gate.close_reasons(), vec![reason.clone()]);

		gate.release();
		gate.wait_finished(HANDLER_WORKERS);
		let outcome = transport.finish(Duration::from_secs(1));
		assert!(outcome.flushed);
		assert_eq!(outcome.reason, Some(reason));
	}

	#[test]
	fn eof_cancels_the_running_turn_and_finish_never_joins_handlers() {
		let writer = RecordingWriter::default();
		let gate = Arc::new(Gate::default());
		let transport = transport_with_dispatcher(writer.clone(), Arc::clone(&gate));
		transport.handle_line(&line(json!({ "jsonrpc": "2.0", "id": 1, "method": "turn" })));
		gate.wait_started(1);

		transport.serve(&b""[..]);
		assert!(transport.is_closed());
		assert_eq!(gate.close_reasons(), vec!["Connection closed".to_string()]);

		// `finish` returns while the handler is still blocked: it joins the
		// writer only, and the adapter owns the turn barrier.
		let (tx, rx) = mpsc::channel();
		let finisher = transport.clone();
		let join = thread::spawn(move || {
			let _ = tx.send(finisher.finish(Duration::from_secs(1)));
		});
		let outcome = rx.recv_timeout(Duration::from_secs(2)).expect("finish is not blocked on handlers");
		assert!(outcome.flushed);
		assert_eq!(gate.count_finished(), 0);
		join.join().expect("finish thread");

		gate.release();
		gate.wait_finished(1);
	}

	#[test]
	fn cancel_reaches_the_control_hook_with_every_worker_wedged() {
		let writer = RecordingWriter::default();
		let gate = Arc::new(Gate::default());
		let transport = transport_with_dispatcher(writer.clone(), Arc::clone(&gate));

		for index in 0..HANDLER_WORKERS {
			transport.handle_line(&line(json!({ "jsonrpc": "2.0", "id": index, "method": "turn" })));
		}
		gate.wait_started(HANDLER_WORKERS);
		for index in 0..MAX_QUEUED_FRAMES {
			transport.handle_line(&line(json!({
				"jsonrpc": "2.0",
				"id": 1000 + index,
				"method": "turn"
			})));
		}

		// The handler queue is full and every worker is mid-turn, yet the
		// reader-thread hook still delivers the cancel.
		transport.handle_line(&line(json!({
			"jsonrpc": "2.0",
			"method": "session/cancel",
			"params": { "sessionId": "s" }
		})));
		assert_eq!(gate.controls(), vec![true]);

		transport.handle_line(&line(json!({ "jsonrpc": "2.0", "id": 77, "method": "session/cancel" })));
		let reply = writer.wait_frame(|frame| frame["id"] == json!(77));
		assert_eq!(reply["result"], json!({}));
		assert_eq!(gate.controls(), vec![true, false]);
		assert!(!transport.is_closed());

		gate.release();
		transport.close(Some("test done".to_string()));
		let outcome = transport.finish(Duration::from_secs(1));
		assert!(outcome.flushed);
	}

	#[test]
	fn panicking_close_hook_is_contained_and_pending_calls_still_reject() {
		let writer = RecordingWriter::default();
		let transport = transport_with_dispatcher(writer.clone(), Arc::new(PanicClose));
		let id = JsonRpcId::number(5);
		let replies = transport.register(&id).expect("connection open");

		let closed = catch_unwind(AssertUnwindSafe(|| transport.close(Some("boom".to_string()))));
		assert!(closed.is_ok());
		assert_eq!(transport.closed_reason(), Some("boom".to_string()));
		assert_eq!(transport.pending_len(), 0);
		assert!(matches!(
			replies.recv_timeout(Duration::from_secs(1)),
			Ok(Reply::Closed)
		));
		transport.finish(Duration::from_secs(1));
	}

	#[test]
	fn panicking_control_hook_fails_the_connection_closed() {
		let writer = RecordingWriter::default();
		let transport = transport_with_dispatcher(writer.clone(), Arc::new(PanicControl));

		transport.handle_line(&line(json!({ "jsonrpc": "2.0", "method": "session/cancel" })));

		assert!(transport.is_closed());
		let reason = transport.closed_reason().expect("closed");
		assert!(reason.contains("control hook panicked"), "{reason}");
		let outcome = transport.finish(Duration::from_secs(1));
		assert!(outcome.flushed);
	}

	#[test]
	fn responses_are_correlated_without_ever_running_the_control_hook() {
		let writer = RecordingWriter::default();
		let transport = transport_with_dispatcher(writer.clone(), Arc::new(PanicControl));
		let id = JsonRpcId::number(3);
		let replies = transport.register(&id).expect("connection open");

		transport.handle_line(r#"{"jsonrpc":"2.0","id":3,"result":{"ok":true}}"#);

		assert!(!transport.is_closed());
		assert!(matches!(
			replies.recv_timeout(Duration::from_secs(1)),
			Ok(Reply::Result(_))
		));
		transport.finish(Duration::from_secs(1));
	}

	#[test]
	fn responses_still_correlate_while_every_handler_is_wedged() {
		let writer = RecordingWriter::default();
		let gate = Arc::new(Gate::default());
		let transport = transport_with_dispatcher(writer.clone(), Arc::clone(&gate));
		transport.handle_line(&line(json!({ "jsonrpc": "2.0", "id": 1, "method": "turn" })));
		gate.wait_started(1);

		let id = JsonRpcId::number(4);
		let replies = transport.register(&id).expect("connection open");
		transport.handle_line(r#"{"jsonrpc":"2.0","id":4,"result":{"ok":true}}"#);
		assert!(matches!(
			replies.recv_timeout(Duration::from_secs(1)),
			Ok(Reply::Result(_))
		));
		assert_eq!(gate.count_finished(), 0);

		gate.release();
		transport.close(Some("test done".to_string()));
		transport.finish(Duration::from_secs(1));
	}

	#[test]
	fn malformed_requests_are_answered_and_malformed_responses_are_not() {
		let writer = RecordingWriter::default();
		let (transport, recorder) = transport_with(&writer);

		// Request-shaped: answered, even though the id is unknown.
		transport.handle_line(r#"{"jsonrpc":"1.0","id":42,"method":"ping"}"#);
		let reply = writer.wait_frame(|frame| frame["id"] == json!(42));
		assert_eq!(reply["error"]["code"], json!(-32600));
		assert!(!transport.is_closed());

		// Response-shaped frames are answers to our calls: never echoed, even
		// with an id nothing is waiting for.
		transport.handle_line(r#"{"jsonrpc":"2.0","id":43}"#);
		transport.handle_line(&line(json!({ "jsonrpc": "2.0", "id": 44, "method": "ping" })));
		let result = writer.wait_frame(|frame| frame["id"] == json!(44));
		assert_eq!(result["result"], json!({ "ok": true }));
		assert_eq!(recorder.wait_seen(1), vec![("ping".to_string(), false)]);
		assert_eq!(writer.frames().len(), 2);
		transport.finish(Duration::from_secs(1));
	}

	#[test]
	fn a_malformed_request_never_consumes_a_pending_call_with_the_same_id() {
		let writer = RecordingWriter::default();
		let (transport, _recorder) = transport_with(&writer);
		let id = JsonRpcId::number(0);
		let replies = transport.register(&id).expect("connection open");

		// Both frames are malformed *requests* whose id collides with the
		// pending reverse call: the call must survive, both get `-32600`.
		transport.handle_line(r#"{"jsonrpc":"1.0","id":0,"method":"ping"}"#);
		transport.handle_line(r#"{"jsonrpc":"2.0","id":0,"method":5}"#);
		assert_eq!(transport.pending_len(), 1);
		let errors = writer.wait_frames(2, |frame| frame["id"] == json!(0));
		assert!(errors.iter().all(|frame| frame["error"]["code"] == json!(-32600)));
		assert!(!transport.is_closed());

		// The real answer still resolves the pending call.
		transport.handle_line(r#"{"jsonrpc":"2.0","id":0,"result":{"ok":true}}"#);
		assert!(matches!(
			replies.recv_timeout(Duration::from_secs(1)),
			Ok(Reply::Result(_))
		));
		assert_eq!(transport.pending_len(), 0);
		transport.finish(Duration::from_secs(1));
	}

	#[test]
	fn malformed_response_for_a_pending_call_fails_it_instead_of_leaking() {
		let writer = RecordingWriter::default();
		let (transport, _recorder) = transport_with(&writer);
		let id = JsonRpcId::number(9);
		let replies = transport.register(&id).expect("connection open");

		// The id matches, the frame is not a legal response: the pending call
		// fails with `-32600`, no frame is echoed back for our own request id.
		transport.handle_line(r#"{"jsonrpc":"2.0","id":9}"#);
		assert!(matches!(
			replies.recv_timeout(Duration::from_secs(1)),
			Ok(Reply::Error(error)) if error.code == crate::acp::protocol::INVALID_REQUEST
		));
		assert_eq!(transport.pending_len(), 0);
		assert!(writer.frames().is_empty());
		assert!(!transport.is_closed());
		transport.finish(Duration::from_secs(1));
	}

	/// BufRead that reports one `EINTR` before handing out its bytes.
	struct InterruptOnce {
		interrupted: bool,
		data:        &'static [u8],
	}

	impl io::Read for InterruptOnce {
		fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
			let take = self.data.len().min(buf.len());
			buf[..take].copy_from_slice(&self.data[..take]);
			self.data = &self.data[take..];
			Ok(take)
		}
	}

	impl io::BufRead for InterruptOnce {
		fn fill_buf(&mut self) -> io::Result<&[u8]> {
			if !self.interrupted {
				self.interrupted = true;
				return Err(io::Error::from(io::ErrorKind::Interrupted));
			}
			Ok(self.data)
		}

		fn consume(&mut self, amount: usize) {
			self.data = &self.data[amount..];
		}
	}

	#[test]
	fn read_bounded_retries_an_interrupted_fill() {
		let mut reader = InterruptOnce { interrupted: false, data: b"ping\n" };
		let mut line = Vec::new();
		assert!(matches!(
			super::read_bounded(&mut reader, &mut line),
			Ok(super::ReadState::Line)
		));
		assert_eq!(line, b"ping\n".to_vec());
	}

	#[derive(Default)]
	struct InterruptedFlush {
		calls: usize,
	}

	impl Write for InterruptedFlush {
		fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
			Ok(buf.len())
		}

		fn flush(&mut self) -> io::Result<()> {
			self.calls += 1;
			if self.calls == 1 {
				return Err(io::Error::from(io::ErrorKind::Interrupted));
			}
			Ok(())
		}
	}

	#[test]
	fn flush_retry_survives_a_signal_interrupt() {
		let mut writer = InterruptedFlush::default();
		super::flush_retry(&mut writer).expect("interrupted flush is retried");
		assert_eq!(writer.calls, 2);
	}

	#[test]
	fn outbound_saturation_fails_closed_instead_of_buffering() {
		let writer = StalledWriter::default();
		let transport = transport_with_dispatcher(
			writer.clone(),
			Arc::new(super::test_support::Recorder::default()),
		);
		assert!(transport.write(json!({ "jsonrpc": "2.0", "method": "first" })));
		writer.wait_entered();
		for _ in 0..MAX_QUEUED_OUTBOUND {
			assert!(transport.write(json!({ "jsonrpc": "2.0", "method": "more" })));
		}

		// The peer stopped draining: the next frame fails the connection closed
		// rather than growing the queue.
		assert!(!transport.write(json!({ "jsonrpc": "2.0", "method": "overflow" })));
		assert!(transport.is_closed());
		let reason = transport.closed_reason().expect("closed");
		assert!(reason.contains("outbound frame queue is full"), "{reason}");

		writer.release();
		let outcome = transport.finish(Duration::from_secs(2));
		assert!(outcome.flushed);
		assert_eq!(outcome.reason, Some(reason));
	}

	struct FailingWriter;

	impl Write for FailingWriter {
		fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
			Err(io::Error::new(io::ErrorKind::BrokenPipe, "peer gone"))
		}

		fn flush(&mut self) -> io::Result<()> {
			Ok(())
		}
	}

	#[test]
	fn a_write_error_reports_a_dirty_flush() {
		let transport = transport_with_dispatcher(
			FailingWriter,
			Arc::new(super::test_support::Recorder::default()),
		);
		assert!(transport.write(json!({ "jsonrpc": "2.0", "method": "x" })));
		assert!(transport.wait_closed(Some(Duration::from_secs(2))));

		// Exiting after an error is completion, not a clean flush.
		let outcome = transport.finish(Duration::from_secs(1));
		assert!(!outcome.flushed);
		let reason = outcome.reason.expect("close reason");
		assert!(reason.contains("cannot write ACP frame"), "{reason}");
	}

	#[test]
	fn finish_reports_a_stalled_writer_without_claiming_it_was_reaped() {
		let writer = StalledWriter::default();
		let transport = transport_with_dispatcher(writer.clone(), Arc::new(super::test_support::Recorder::default()));
		assert!(transport.write(json!({ "jsonrpc": "2.0", "method": "x" })));
		writer.wait_entered();

		let outcome = transport.finish(Duration::from_millis(50));
		assert!(!outcome.flushed);
		assert_eq!(outcome.reason, Some("Connection closed".to_string()));

		// The writer is only detached; once unblocked it drains and exits, and
		// a second `finish` reports the flush from the latch.
		writer.release();
		let deadline = Instant::now() + Duration::from_secs(2);
		while transport.writer_finished().is_none() && Instant::now() < deadline {
			thread::sleep(Duration::from_millis(5));
		}
		assert_eq!(transport.writer_finished(), Some(true));
		assert_eq!(
			transport.finish(Duration::ZERO),
			FinishOutcome { reason: Some("Connection closed".to_string()), flushed: true }
		);
	}
}
