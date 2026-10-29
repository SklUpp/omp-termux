//! RPC transport plumbing: ordered stdout writer, stdin frame reader, the
//! extension-UI roundtrip, and EOF shutdown coordination.
//!
//! One writer thread owns stdout so frames from the command loop and from
//! background work (bash, turn pump) keep their order. The writer also applies
//! protocol negotiation: a successful `negotiate_protocol` response flips the
//! encoder to v2 for everything after it, exactly like upstream.

use std::{
	io::{self, BufRead, Read, Write},
	sync::{
		Arc, Mutex, MutexGuard,
		atomic::{AtomicU64, Ordering},
		mpsc::{self, Receiver, Sender},
	},
	thread::{self, JoinHandle},
	time::{Duration, Instant},
};

use pi_agent::tools::ask::{
	AskAnswer, AskOutcome, AskQuestion, CHAT_ABOUT_THIS_OPTION, OTHER_OPTION, display_label,
	strip_recommended_suffix,
};
use serde_json::{Value, json};

use super::{
	frame::{FrameLines, RpcFrameDecoder, RpcFrameEncoder},
	session::FrameSink,
	types::{RpcControlFrame, RpcExtensionUiResponse, parse_control_frame},
};

/// Successful negotiation response; the writer flips to protocol v2 afterwards.
fn is_negotiation_success(frame: &Value) -> bool {
	frame.get("type").and_then(Value::as_str) == Some("response")
		&& frame.get("command").and_then(Value::as_str) == Some("negotiate_protocol")
		&& frame.get("success").and_then(Value::as_bool) == Some(true)
}

/// Ordered, single-owner stdout writer for logical frames.
///
/// Internals are `Arc`-shared so one instance can be handed to the frame sink,
/// the extension-UI bridge and the host bridges while the owner still controls
/// shutdown.
pub struct Output {
	tx:     Mutex<Option<Sender<Value>>>,
	error:  Arc<Mutex<Option<String>>>,
	handle: Mutex<Option<JoinHandle<()>>>,
}

impl Output {
	/// Spawn the writer thread. Frames are flushed one logical frame at a time.
	pub fn spawn<W: Write + Send + 'static>(mut writer: W) -> Self {
		let (tx, rx) = mpsc::channel::<Value>();
		let error = Arc::new(Mutex::new(None));
		let thread_error = Arc::clone(&error);
		let handle = thread::Builder::new()
			.name("omp-rpc-writer".to_string())
			.spawn(move || {
				let mut encoder = RpcFrameEncoder::new();
				for frame in rx {
					let negotiate = is_negotiation_success(&frame);
					let lines: FrameLines = encoder.encode_frames(&frame);
					for line in lines {
						if let Err(err) = writer.write_all(line.as_bytes()).and_then(|()| writer.flush()) {
							*thread_error.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
								Some(format!("cannot write RPC frame: {err}"));
							return;
						}
					}
					if negotiate {
						// Only reached when the response was written completely.
						encoder
							.set_protocol_version(2)
							.expect("protocol v2 is supported");
					}
				}
			})
			.expect("spawn rpc writer thread");
		Self { tx: Mutex::new(Some(tx)), error, handle: Mutex::new(Some(handle)) }
	}

	/// Queue one logical frame. Never blocks on the transport.
	pub fn send(&self, frame: Value) {
		let guard = self.tx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		if let Some(tx) = guard.as_ref() {
			let _ = tx.send(frame);
		}
	}

	/// Close the queue, join the writer and report the first transport error.
	pub fn finish(&self) -> Option<String> {
		self.tx.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
		let handle = self.handle.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
		if let Some(handle) = handle {
			let _ = handle.join();
		}
		self
			.error
			.lock()
			.unwrap_or_else(std::sync::PoisonError::into_inner)
			.clone()
	}
}

impl FrameSink for Output {
	fn send(&self, frame: Value) {
		Output::send(self, frame);
	}
}

impl Drop for Output {
	fn drop(&mut self) {
		let _ = self
			.tx
			.get_mut()
			.unwrap_or_else(std::sync::PoisonError::into_inner)
			.take();
	}
}

/// A stdin line that is either a side-channel frame or a command.
#[derive(Debug)]
pub enum Inbound {
	Control(RpcControlFrame),
	Command(Value),
}

/// Line reader producing parsed frames and reassembled chunk sequences.
pub struct RpcInput<R: BufRead> {
	reader:  R,
	decoder: RpcFrameDecoder,
	line:    Vec<u8>,
}

impl<R: BufRead> RpcInput<R> {
	#[must_use]
	pub fn new(reader: R) -> Self {
		Self { reader, decoder: RpcFrameDecoder::default(), line: Vec::new() }
	}

	/// Next inbound frame. `Ok(None)` is EOF; `Ok(Some(Err))` is a parse error
	/// the caller must answer with an error response and keep reading.
	pub fn next(&mut self) -> io::Result<Option<Result<Inbound, String>>> {
		loop {
			self.line.clear();
			if self.reader.read_until(b'\n', &mut self.line)? == 0 {
				return Ok(None);
			}
			let text = match std::str::from_utf8(&self.line) {
				Ok(text) => text,
				Err(error) => {
					return Ok(Some(Err(format!(
						"Failed to parse command: invalid UTF-8 ({error})"
					))));
				},
			};
			let trimmed = text.trim();
			if trimmed.is_empty() {
				continue;
			}
			let value: Value = match serde_json::from_str(trimmed) {
				Ok(value) => value,
				Err(err) => return Ok(Some(Err(format!("Failed to parse command: {err}")))),
			};
			match self.decoder.push(value) {
				Ok(Some(frame)) => {
					return Ok(Some(Ok(match parse_control_frame(&frame) {
						Some(control) => Inbound::Control(control),
						None => Inbound::Command(frame),
					})));
				},
				Ok(None) => continue,
				Err(err) => return Ok(Some(Err(err))),
			}
		}
	}
}

/// Pending `extension_ui_response` requests, resolved from the control loop.
pub struct ExtensionUiBridge {
	sink:    Arc<dyn FrameSink>,
	pending: Mutex<std::collections::BTreeMap<String, Sender<RpcExtensionUiResponse>>>,
	next_id: AtomicU64,
}

impl ExtensionUiBridge {
	#[must_use]
	pub fn new(sink: Arc<dyn FrameSink>) -> Self {
		Self {
			sink,
			pending: Mutex::new(std::collections::BTreeMap::new()),
			next_id: AtomicU64::new(1),
		}
	}

	/// Ask the host to confirm. `None` means cancelled, timed out or closed —
	/// callers must treat that as a denial.
	pub fn confirm(
		&self,
		title: &str,
		message: &str,
		timeout: Option<Duration>,
	) -> Option<bool> {
		let response = self.request(
			serde_json::json!({ "method": "confirm", "title": title, "message": message }),
			timeout,
		)?;
		response.confirmed
	}

	/// Send one `extension_ui_request` and wait for its response. `None` on
	/// cancel, timeout, shutdown or an unknown reply.
	pub fn request(
		&self,
		frame: serde_json::Value,
		timeout: Option<Duration>,
	) -> Option<RpcExtensionUiResponse> {
		self.request_cancellable(frame, timeout, &|| false)
	}

	/// [`Self::request`] bound to one owner's cancellation signal.
	///
	/// While waiting, `cancelled()` is polled; once it turns true this dialog —
	/// and only this dialog — is dismissed with the upstream `method:"cancel"`
	/// frame (`rpc-mode.ts#requestRpcDialog.onAbort`, `targetId` = dialog id),
	/// dropped from the pending table and the caller returns `None`. A signal that
	/// is already set never sends a frame, so a cancel racing a prompt cannot
	/// leave a live dialog behind; answers that arrive after the cancel are
	/// dropped by [`Self::handle`]. A timeout removes the entry without a cancel
	/// frame, exactly like upstream's `setTimeout` branch.
	pub fn request_cancellable(
		&self,
		mut frame: serde_json::Value,
		timeout: Option<Duration>,
		cancelled: &dyn Fn() -> bool,
	) -> Option<RpcExtensionUiResponse> {
		const POLL: Duration = Duration::from_millis(40);
		if cancelled() {
			return None;
		}
		let id = format!("ui-{}-{}", std::process::id(), self.next_id.fetch_add(1, Ordering::Relaxed));
		let (tx, rx) = mpsc::channel();
		self.lock_pending().insert(id.clone(), tx);
		if cancelled() {
			self.lock_pending().remove(&id);
			return None;
		}
		frame["type"] = serde_json::json!("extension_ui_request");
		frame["id"] = serde_json::json!(id);
		if let Some(timeout) = timeout {
			frame["timeout"] = serde_json::json!(timeout.as_millis() as u64);
		}
		self.sink.send(frame);

		let deadline = timeout.map(|timeout| Instant::now() + timeout);
		loop {
			let wait = match deadline {
				Some(deadline) => deadline.saturating_duration_since(Instant::now()).min(POLL),
				None => POLL,
			};
			match rx.recv_timeout(wait) {
				Ok(response) => {
					if response.id != id {
						continue;
					}
					if response.cancelled {
						return None;
					}
					return Some(response);
				},
				Err(mpsc::RecvTimeoutError::Disconnected) => {
					self.lock_pending().remove(&id);
					return None;
				},
				Err(mpsc::RecvTimeoutError::Timeout) => {
					if cancelled() {
						// A response may have raced the signal: it wins.
						if let Ok(response) = rx.try_recv() {
							if response.id == id {
								return if response.cancelled { None } else { Some(response) };
							}
						}
						self.cancel_dialog(&id);
						return None;
					}
					if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
						self.lock_pending().remove(&id);
						return None;
					}
				},
			}
		}
	}

	/// Resolve one inbound response; `false` when the id is unknown.
	pub fn handle(&self, response: RpcExtensionUiResponse) -> bool {
		let Some(sender) = self.lock_pending().remove(&response.id) else {
			return false;
		};
		sender.send(response).is_ok()
	}

	/// Dismiss one registered dialog with the upstream cancel notification.
	fn cancel_dialog(&self, id: &str) {
		self.lock_pending().remove(id);
		self.sink.send(serde_json::json!({
			"type": "extension_ui_request",
			"id": format!("ui-{}-{}", std::process::id(), self.next_id.fetch_add(1, Ordering::Relaxed)),
			"method": "cancel",
			"targetId": id,
		}));
	}

	/// Test-only: dialogs still waiting for a response.
	#[cfg(test)]
	pub(crate) fn pending_dialogs(&self) -> usize {
		self.lock_pending().len()
	}

	/// Fail every pending dialog (abort, EOF, shutdown) so callers fail closed.
	pub fn cancel_all(&self) {
		let pending: Vec<(String, Sender<RpcExtensionUiResponse>)> =
			std::mem::take(&mut *self.lock_pending()).into_iter().collect();
		for (id, sender) in pending {
			let _ = sender.send(RpcExtensionUiResponse {
				id,
				value: None,
				confirmed: None,
				cancelled: true,
				timed_out: None,
			});
		}
	}

	fn lock_pending(
		&self,
	) -> MutexGuard<'_, std::collections::BTreeMap<String, Sender<RpcExtensionUiResponse>>> {
		self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
	}
}

/// `BufRead` view over [`ExtensionUiBridge::confirm`], installed into
/// `Approvals` so a write/exec approval becomes an `extension_ui_request`
/// dialog and an unanswered dialog denies the call.
pub struct RpcApprovalInput {
	ui:      Arc<ExtensionUiBridge>,
	timeout: Option<Duration>,
	buffer:  Vec<u8>,
	served:  usize,
}

impl RpcApprovalInput {
	#[must_use]
	pub fn new(ui: Arc<ExtensionUiBridge>, timeout: Option<Duration>) -> Self {
		Self { ui, timeout, buffer: Vec::new(), served: 0 }
	}
}

impl Read for RpcApprovalInput {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		let available = self.fill_buf()?;
		let count = available.len().min(buf.len());
		buf[..count].copy_from_slice(&available[..count]);
		self.consume(count);
		Ok(count)
	}
}

impl BufRead for RpcApprovalInput {
	fn fill_buf(&mut self) -> io::Result<&[u8]> {
		if self.served >= self.buffer.len() {
			let approved = self.ui.confirm(
				"Approve tool execution",
				"An agent tool call needs approval. The request details were logged to stderr.",
				self.timeout,
			);
			self.buffer = if approved == Some(true) {
				b"yes\n".to_vec()
			} else {
				b"no\n".to_vec()
			};
			self.served = 0;
		}
		Ok(&self.buffer[self.served..])
	}

	fn consume(&mut self, amount: usize) {
		self.served = (self.served + amount).min(self.buffer.len());
	}
}

/// Tracks background work (bash, turn pumps) so EOF drains instead of racing.
#[derive(Default)]
pub struct ShutdownCoordinator {
	handles: Vec<JoinHandle<()>>,
	closing: bool,
}

impl ShutdownCoordinator {
	#[must_use]
	pub fn new() -> Self {
		Self::default()
	}

	/// Track a background task. The `on_shutdown` message is delivered by the
	/// caller before `drain` when it wants cooperative cancellation.
	pub fn track(&mut self, handle: JoinHandle<()>) {
		self.handles.push(handle);
		// Reap finished handles so long-lived processes do not accumulate them.
		let mut index = 0;
		while index < self.handles.len() {
			if self.handles[index].is_finished() {
				let handle = self.handles.remove(index);
				let _ = handle.join();
			} else {
				index += 1;
			}
		}
	}

	#[must_use]
	pub fn is_closing(&self) -> bool {
		self.closing
	}

	/// Mark shutdown started; later commands are refused.
	pub fn begin_close(&mut self) {
		self.closing = true;
	}

	/// Join every tracked task. Returns the number of tasks drained.
	pub fn drain(&mut self) -> usize {
		self.closing = true;
		let count = self.handles.len();
		for handle in self.handles.drain(..) {
			let _ = handle.join();
		}
		count
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[derive(Clone, Default)]
	struct SharedWriter(Arc<Mutex<Vec<u8>>>);

	impl Write for SharedWriter {
		fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
			self.0.lock().expect("writer").extend_from_slice(buf);
			Ok(buf.len())
		}

		fn flush(&mut self) -> io::Result<()> {
			Ok(())
		}
	}

	fn writer_contents(writer: &SharedWriter) -> String {
		String::from_utf8(writer.0.lock().expect("writer").clone()).expect("utf8")
	}

	#[test]
	fn writer_preserves_order_and_flips_protocol_after_negotiation() {
		let sink = SharedWriter::default();
		let output = Output::spawn(sink.clone());
		output.send(serde_json::json!({ "type": "ready", "protocolVersion": 1 }));
		output.send(serde_json::json!({
			"id": "1",
			"type": "response",
			"command": "negotiate_protocol",
			"success": true,
			"data": { "protocolVersion": 2 },
		}));
		let big = "x".repeat(super::super::frame::MAX_RPC_FRAME_BYTES + 10);
		output.send(serde_json::json!({ "type": "notice", "message": big }));
		assert_eq!(output.finish(), None);
		let text = writer_contents(&sink);
		let lines: Vec<&str> = text.lines().collect();
		assert_eq!(lines[0], r#"{"type":"ready","protocolVersion":1}"#);
		assert!(lines[1].contains(r#""command":"negotiate_protocol""#));
		// Everything after negotiation is chunked, never an oversized line.
		assert!(lines[2].contains(r#""type":"rpc_chunk""#));
		assert!(lines.iter().all(|line| line.len() + 1 <= super::super::frame::MAX_RPC_FRAME_BYTES));
	}

	#[test]
	fn reader_parses_commands_and_skips_blank_lines_and_garbage() {
		let input = b"\n  \n{\"type\":\"abort\"}\nnot json\n{\"type\":\"get_state\"}\n";
		let mut reader = RpcInput::new(&input[..]);
		let first = reader.next().expect("io").expect("frame").expect("ok");
		assert!(matches!(first, Inbound::Command(value) if value["type"] == "abort"));
		let error = reader.next().expect("io").expect("frame").expect_err("parse error");
		assert!(error.starts_with("Failed to parse command:"));
		let last = reader.next().expect("io").expect("frame").expect("ok");
		assert!(matches!(last, Inbound::Command(value) if value["type"] == "get_state"));
		assert!(reader.next().expect("io").is_none());
	}

	#[test]
	fn reader_routes_control_frames_before_commands() {
		let input = b"{\"type\":\"extension_ui_response\",\"id\":\"u1\",\"confirmed\":true}\n";
		let mut reader = RpcInput::new(&input[..]);
		match reader.next().expect("io").expect("frame").expect("ok") {
			Inbound::Control(RpcControlFrame::ExtensionUi(response)) => {
				assert_eq!(response.id, "u1");
				assert_eq!(response.confirmed, Some(true));
			},
			other => panic!("unexpected {other:?}"),
		}
	}

	#[test]
	fn extension_ui_roundtrip_and_cancel_all() {
		let (tx, rx) = mpsc::channel();
		struct TestSink(Mutex<Sender<Value>>);
		impl FrameSink for TestSink {
			fn send(&self, frame: Value) {
				let _ = self.0.lock().expect("sink").send(frame);
			}
		}
		let ui = Arc::new(ExtensionUiBridge::new(Arc::new(TestSink(Mutex::new(tx)))));
		let thread_ui = Arc::clone(&ui);
		let handle = thread::spawn(move || thread_ui.confirm("t", "m", Some(Duration::from_secs(5))));
		let request = rx.recv_timeout(Duration::from_secs(5)).expect("request");
		assert_eq!(request["method"], serde_json::json!("confirm"));
		let id = request["id"].as_str().expect("id").to_string();
		assert!(ui.handle(RpcExtensionUiResponse {
			id,
			value: None,
			confirmed: Some(true),
			cancelled: false,
			timed_out: None,
		}));
		assert_eq!(handle.join().expect("join"), Some(true));

		let thread_ui = Arc::clone(&ui);
		let handle = thread::spawn(move || thread_ui.confirm("t", "m", None));
		let _request = rx.recv_timeout(Duration::from_secs(5)).expect("request");
		ui.cancel_all();
		assert_eq!(handle.join().expect("join"), None);
	}

	#[test]
	fn approval_input_answers_yes_no_and_denies_unanswered() {
		let (tx, rx) = mpsc::channel();
		struct TestSink(Mutex<Sender<Value>>);
		impl FrameSink for TestSink {
			fn send(&self, frame: Value) {
				let _ = self.0.lock().expect("sink").send(frame);
			}
		}
		let ui = Arc::new(ExtensionUiBridge::new(Arc::new(TestSink(Mutex::new(tx)))));
		let mut input = RpcApprovalInput::new(Arc::clone(&ui), Some(Duration::from_secs(5)));
		let handle = thread::spawn(move || {
			let mut line = String::new();
			input.read_line(&mut line).expect("read");
			line
		});
		let request = rx.recv_timeout(Duration::from_secs(5)).expect("request");
		let id = request["id"].as_str().expect("id").to_string();
		ui.handle(RpcExtensionUiResponse {
			id,
			value: None,
			confirmed: Some(true),
			cancelled: false,
			timed_out: None,
		});
		assert_eq!(handle.join().expect("join"), "yes\n");

		// A cancelled dialog is a denial, never an approval.
		let mut input = RpcApprovalInput::new(Arc::clone(&ui), Some(Duration::from_secs(5)));
		let handle = thread::spawn(move || {
			let mut line = String::new();
			input.read_line(&mut line).expect("read");
			line
		});
		let _request = rx.recv_timeout(Duration::from_secs(5)).expect("request");
		ui.cancel_all();
		assert_eq!(handle.join().expect("join"), "no\n");
	}

	#[test]
	fn shutdown_drains_tracked_tasks() {
		let mut coordinator = ShutdownCoordinator::new();
		let marker = Arc::new(Mutex::new(Vec::new()));
		// Tasks park until every one of them is tracked, so none can finish (and
		// be reaped by `track`) before `drain` owns all three handles.
		let mut releases = Vec::new();
		for index in 0..3 {
			let marker = Arc::clone(&marker);
			let (release, wait) = mpsc::channel::<()>();
			releases.push(release);
			coordinator.track(thread::spawn(move || {
				let _ = wait.recv();
				marker.lock().expect("marker").push(index);
			}));
		}
		drop(releases);
		coordinator.begin_close();
		assert!(coordinator.is_closing());
		assert_eq!(coordinator.drain(), 3, "every tracked task is awaited by drain");
		let mut tasks = marker.lock().expect("marker").clone();
		tasks.sort_unstable();
		assert_eq!(tasks, vec![0, 1, 2]);
		assert_eq!(coordinator.drain(), 0);
	}

	fn test_ui() -> (Arc<ExtensionUiBridge>, mpsc::Receiver<Value>) {
		let (tx, rx) = mpsc::channel();
		struct TestSink(Mutex<Sender<Value>>);
		impl FrameSink for TestSink {
			fn send(&self, frame: Value) {
				let _ = self.0.lock().expect("sink").send(frame);
			}
		}
		(Arc::new(ExtensionUiBridge::new(Arc::new(TestSink(Mutex::new(tx))))), rx)
	}

	#[test]
	fn cancellable_request_dismisses_only_its_dialog_with_the_upstream_cancel_frame() {
		use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
		let (ui, rx) = test_ui();
		let cancelled = Arc::new(AtomicBool::new(false));
		let handle = {
			let ui = Arc::clone(&ui);
			let flag = Arc::clone(&cancelled);
			thread::spawn(move || {
				ui.request_cancellable(
					serde_json::json!({ "method": "input", "title": "Paste" }),
					None,
					&|| flag.load(AtomicOrdering::SeqCst),
				)
			})
		};
		let request = rx.recv_timeout(Duration::from_secs(5)).expect("request");
		let id = request["id"].as_str().expect("id").to_string();
		assert_eq!(ui.pending_dialogs(), 1, "the dialog is registered while shown");

		cancelled.store(true, AtomicOrdering::SeqCst);
		let cancel_frame = rx.recv_timeout(Duration::from_secs(5)).expect("cancel frame");
		assert_eq!(cancel_frame["type"], serde_json::json!("extension_ui_request"));
		assert_eq!(cancel_frame["method"], serde_json::json!("cancel"));
		assert_eq!(cancel_frame["targetId"], serde_json::json!(id), "targets exactly this dialog");
		assert_ne!(cancel_frame["id"], serde_json::json!(id), "the notification carries its own id");

		assert!(handle.join().expect("join").is_none());
		assert_eq!(ui.pending_dialogs(), 0);
		assert!(
			!ui.handle(RpcExtensionUiResponse {
				id,
				value: Some("late".to_string()),
				confirmed: None,
				cancelled: false,
				timed_out: None,
			}),
			"an answer arriving after the cancel is dropped"
		);
	}

	#[test]
	fn already_cancelled_request_emits_no_dialog_frame() {
		let (ui, rx) = test_ui();
		assert!(ui
			.request_cancellable(serde_json::json!({ "method": "input" }), Some(Duration::from_secs(5)), &|| true)
			.is_none());
		assert!(rx.try_recv().is_err(), "a cancelled token never sends the prompt");
		assert_eq!(ui.pending_dialogs(), 0);
	}

	#[test]
	fn timeout_removes_the_dialog_without_a_cancel_frame() {
		let (ui, rx) = test_ui();
		assert!(ui
			.request_cancellable(
				serde_json::json!({ "method": "input" }),
				Some(Duration::from_millis(60)),
				&|| false
			)
			.is_none());
		let _request = rx.recv_timeout(Duration::from_secs(5)).expect("request");
		assert!(rx.try_recv().is_err(), "upstream's timeout branch emits no cancel frame");
		assert_eq!(ui.pending_dialogs(), 0);
	}
}

/// `ask` tool surface over `extension_ui_request` `select` dialogs.
///
/// Installed before tools mount, so headless `ask` never aborts or reads stdin.
pub struct RpcAskChannel {
	ui: Arc<ExtensionUiBridge>,
}

/// Label for the runtime-added multi-select submit option
/// (upstream `getDoneOptionLabel`, minus terminal styling).
const DONE_SELECTING_LABEL: &str = "Done selecting";

impl RpcAskChannel {
	pub fn new(ui: Arc<ExtensionUiBridge>) -> Self {
		Self { ui }
	}

	/// Wire labels for one question, matching upstream
	/// `getExtensionUISelectOptionLabel`: positional strings with the
	/// `(Recommended)` suffix on the marked option.
	fn labels(question: &AskQuestion) -> Vec<String> {
		question
			.options
			.iter()
			.enumerate()
			.map(|(index, option)| display_label(option, index, question.recommended))
			.collect()
	}

	/// Positional `optionDetails`, aligned 1:1 with [`Self::labels`]. Options
	/// without a description become `{}` so indices stay aligned; labels the
	/// runtime appends (custom input, done) get `{}` too.
	fn details(question: &AskQuestion, extra: usize) -> Vec<Value> {
		let mut details: Vec<Value> = question
			.options
			.iter()
			.map(|option| match option.description.as_deref().map(str::trim) {
				Some(description) if !description.is_empty() => json!({ "description": description }),
				_ => json!({}),
			})
			.collect();
		details.resize(details.len() + extra, json!({}));
		details
	}

	fn title(question: &AskQuestion) -> String {
		question.header.clone().unwrap_or_else(|| question.question.clone())
	}

	/// One `select` dialog; `None` means cancelled or timed out.
	fn select(&self, title: String, options: Vec<String>, option_details: Vec<Value>) -> Option<String> {
		let response = self.ui.request(
			json!({
				"method": "select",
				"title": title,
				"options": options,
				"optionDetails": option_details,
			}),
			None,
		)?;
		if response.cancelled {
			return None;
		}
		response.value
	}

	/// Free-text answer through the protocol's `editor` dialog.
	fn custom_input(&self, question: &AskQuestion) -> Option<String> {
		let response = self.ui.request(
			json!({ "method": "editor", "title": Self::title(question) }),
			None,
		)?;
		if response.cancelled {
			return None;
		}
		response.value
	}

	/// Map one chosen label back to the raw option label (`(Recommended)` is
	/// display-only) or to free text.
	fn resolve(question: &AskQuestion, value: &str) -> Option<String> {
		let raw = strip_recommended_suffix(value);
		question
			.options
			.iter()
			.find(|option| option.label == raw)
			.map(|option| option.label.clone())
	}
}

/// One question's outcome, before it becomes an [`AskOutcome`].
enum AskFlow {
	Answer(AskAnswer),
	Chat,
	Cancelled,
}

#[async_trait::async_trait]
impl pi_agent::tools::ask::AskChannel for RpcAskChannel {
	async fn ask(
		&self,
		questions: &[AskQuestion],
		cancel: tokio_util::sync::CancellationToken,
	) -> AskOutcome {
		let mut answers = Vec::new();
		for question in questions {
			if cancel.is_cancelled() {
				return AskOutcome::Aborted;
			}
			match self.ask_question(question, CHAT_ABOUT_THIS_OPTION) {
				AskFlow::Answer(answer) => answers.push(answer),
				AskFlow::Chat => return AskOutcome::ChatRedirect,
				AskFlow::Cancelled => return AskOutcome::Cancelled,
			}
		}
		AskOutcome::Answered(answers)
	}
}

impl RpcAskChannel {
	/// One question over the real protocol.
	///
	/// Single-select is one dialog. Multi-select is repeated single selections —
	/// the wire `select` returns exactly one label — using upstream's checkbox
	/// loop: the runtime appends `Other (type your own)` (free text through the
	/// `editor` dialog) and, once something is selected, `Done selecting`.
	/// Selections are echoed in the title (`(2 selected) `) because the protocol
	/// has no checked-state field, and re-selecting a chosen label toggles it
	/// off, keeping multi semantics without inventing wire fields.
	fn ask_question(&self, question: &AskQuestion, chat_label: &str) -> AskFlow {
		let title = Self::title(question);
		let mut selected: Vec<String> = Vec::new();
		let mut custom_input: Option<String> = None;
		if !question.multi {
			// Upstream always appends the custom-input option, so free text is
			// reachable through a compliant host (`tools/ask.ts`).
			let mut options = Self::labels(question);
			options.push(OTHER_OPTION.to_string());
			let value = self.select(title.clone(), options, Self::details(question, 1));
			let Some(value) = value else {
				return AskFlow::Cancelled;
			};
			if value == chat_label {
				return AskFlow::Chat;
			}
			if value == OTHER_OPTION {
				return match self.custom_input(question) {
					Some(text) => AskFlow::Answer(AskAnswer {
						id: question.id.clone(),
						custom_input: Some(text),
						..Default::default()
					}),
					None => AskFlow::Cancelled,
				};
			}
			let mut answer = AskAnswer { id: question.id.clone(), ..Default::default() };
			match Self::resolve(question, &value) {
				Some(label) => answer.selected_options.push(label),
				None => answer.custom_input = Some(value),
			}
			return AskFlow::Answer(answer);
		}
		loop {
			let mut options = Self::labels(question);
			options.push(OTHER_OPTION.to_string());
			let mut extra = 1;
			if !selected.is_empty() {
				options.push(DONE_SELECTING_LABEL.to_string());
				extra += 1;
			}
			let prefix = if selected.is_empty() {
				String::new()
			} else {
				format!("({} selected) ", selected.len())
			};
			let Some(value) = self.select(
				format!("{prefix}{title}"),
				options,
				Self::details(question, extra),
			) else {
				return AskFlow::Cancelled;
			};
			if value == DONE_SELECTING_LABEL {
				break;
			}
			if value == chat_label {
				return AskFlow::Chat;
			}
			if value == OTHER_OPTION {
				// An editor dismissed without text returns to the checkbox loop
				// (upstream `tools/ask.ts`), it does not cancel the question.
				if let Some(text) = self.custom_input(question) {
					custom_input = Some(text);
					break;
				}
				continue;
			}
			match Self::resolve(question, &value) {
				Some(label) => {
					match selected.iter().position(|chosen| *chosen == label) {
						Some(index) => {
							selected.remove(index);
						},
						None => selected.push(label),
					}
				},
				None => {
					// A host may answer free text without the runtime having
					// offered `Other`; keep it as the custom answer.
					custom_input = Some(value);
					break;
				},
			}
		}
		AskFlow::Answer(AskAnswer {
			id: question.id.clone(),
			selected_options: selected,
			custom_input,
			..Default::default()
		})
	}
}

#[cfg(test)]
mod ask_wire_tests {
	use super::*;
	use pi_agent::tools::ask::{AskOption, AskQuestion, RECOMMENDED_SUFFIX};

	fn question() -> AskQuestion {
		AskQuestion {
			id:         "q1".to_string(),
			question:   "Ship it?".to_string(),
			header:     None,
			options:    vec![
				AskOption {
					label:       "Yes".to_string(),
					description: Some("  push to production  ".to_string()),
					preview:     None,
				},
				AskOption { label: "No".to_string(), description: None, preview: None },
			],
			multi:      false,
			recommended: Some(0),
		}
	}

	#[test]
	fn select_labels_and_option_details_stay_aligned() {
		let question = question();
		let labels = RpcAskChannel::labels(&question);
		assert_eq!(
			labels,
			vec![format!("Yes{RECOMMENDED_SUFFIX}"), "No".to_string()],
			"recommended suffix is display-only and positional"
		);
		let details = RpcAskChannel::details(&question, 0);
		assert_eq!(details.len(), labels.len(), "optionDetails must align with options");
		assert_eq!(details[0], json!({ "description": "push to production" }));
		assert_eq!(details[1], json!({}), "an option without a description keeps its slot");
		// Runtime-appended labels (`Other`, `Done selecting`) get empty entries.
		assert_eq!(RpcAskChannel::details(&question, 2).len(), labels.len() + 2);
	}

	#[test]
	fn chosen_labels_map_back_to_raw_option_labels() {
		let question = question();
		assert_eq!(
			RpcAskChannel::resolve(&question, &format!("Yes{RECOMMENDED_SUFFIX}")),
			Some("Yes".to_string())
		);
		assert_eq!(RpcAskChannel::resolve(&question, "No"), Some("No".to_string()));
		assert_eq!(
			RpcAskChannel::resolve(&question, "something else"),
			None,
			"free text becomes custom_input, never a selected option"
		);
	}
}
