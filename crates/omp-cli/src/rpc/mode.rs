//! RPC mode: the NDJSON command loop, turn pump and event-to-frame bridge.
//!
//! The transport is line-oriented and serial: one command is handled at a time,
//! while bash runs in the background (so `abort_bash` can overtake it) and turns
//! are pumped by a dedicated thread that continues with queued messages. All
//! stdout writes go through one [`Output`] writer, so frames from the command
//! loop, the turn pump and the extension-UI roundtrip keep their order.

use std::{
	os::unix::process::CommandExt,
	path::Path,
	process::{Command, Stdio},
	sync::{Arc, Mutex, MutexGuard},
	thread,
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use pi_agent::CancellationToken;
use serde_json::{json, Value};

use crate::{
	agent::{Event as AgentEvent, RunRequest},
	deadline::{self, Deadline},
	session::canonical_stop_reason,
};

use super::{
	event_wire,
	frame::{MAX_RPC_FRAME_BYTES, MAX_RPC_REASSEMBLED_BYTES},
	host_tools::HostToolBridge,
	host_uris::{self, HostUriBridge},
	input::{ExtensionUiBridge, Inbound, Output, RpcInput, ShutdownCoordinator},
	messages::{self, MessageSnapshot},
	session::{
		ActiveTurn, FrameSink, PreparedTurn, Prerequisite, RpcLaunch, RpcSession, SteeringMessage,
		TurnAdvance, TurnInput, TurnOutcome,
	},
	subagents::{self, SubagentRegistry},
	types::{
		InterruptMode, RpcCommand, RpcControlFrame, RpcResponse, StreamingBehavior,
		SubagentSubscriptionLevel,
	},
};

use pi_agent::{supervisor::SubagentObserver, tools::internal_urls::InternalUrlRouter};

fn now_ms() -> u64 {
	SystemTime::now()
		.duration_since(SystemTime::UNIX_EPOCH)
		.unwrap_or_default()
		.as_millis() as u64
}

/// Fallback metadata for frames that carry no provider identity themselves.
#[derive(Debug, Clone, Default)]
struct Meta {
	api:      Option<String>,
	provider: Option<String>,
	model:    Option<String>,
}

/// Translates runner events into the wire event stream, preserving upstream
/// order: `message_start`, `message_update`*, `message_end`, `tool_execution_*`,
/// `turn_end`.
#[derive(Default)]
struct EventFrames {
	text:          String,
	turn_open:     bool,
	/// Whether an assistant `message_end` already reached the host. A retry
	/// after that frame would deliver a second end for the same prompt.
	message_ended: bool,
	/// Whether any tool frame was emitted. Tool frames are replay-unsafe for the
	/// same reason committed text is: nothing on the wire retracts them.
	saw_tool:      bool,
}

impl EventFrames {
	/// Upstream `#hasReplayUnsafeOutput` veto: a retry re-streams the turn from
	/// `message_start`, so it is only safe while nothing user-visible was
	/// emitted. Whitespace-only deltas stay replayable (thinking-only partials),
	/// matching `turn-recovery.ts`.
	fn replay_safe(&self) -> bool {
		self.text.trim().is_empty() && !self.saw_tool
	}
}

impl EventFrames {
	fn emit(&mut self, event: &AgentEvent, sink: &dyn FrameSink, meta: &Meta, session: &RpcSession) {
		match event {
			AgentEvent::MessageStart { role } => {
				if role != "assistant" {
					return;
				}
				if !self.turn_open {
					sink.send(json!({ "type": "turn_start" }));
					self.turn_open = true;
				}
				sink.send(json!({
					"type": "message_start",
					"message": {
						"role": "assistant",
						"content": [],
						"timestamp": now_ms(),
					},
				}));
			},
			AgentEvent::MessageUpdate { delta } => {
				self.text.push_str(delta);
				sink.send(json!({
					"type": "message_update",
					"message": {
						"role": "assistant",
						"content": [{ "type": "text", "text": self.text }],
						"timestamp": now_ms(),
					},
					"assistantMessageEvent": { "type": "text_delta", "delta": delta },
				}));
			},
			AgentEvent::MessageEnd { message, stop_reason, api, provider, model } => {
				self.message_ended = true;
				sink.send(json!({
					"type": "message_end",
					"message": {
						"role": "assistant",
						"content": [{ "type": "text", "text": self.text }],
						"api": api.as_deref().or(meta.api.as_deref()),
						"provider": provider.as_deref().or(meta.provider.as_deref()),
						"model": model.as_deref().or(meta.model.as_deref()),
						"stopReason": stop_reason.as_deref().map(canonical_stop_reason),
						"usage": crate::session::reported_usage(message),
						"timestamp": now_ms(),
					},
				}));
			},
			AgentEvent::ToolStart { id, tool, args } => {
				self.saw_tool = true;
				sink.send(event_wire::tool_start(&id, &tool, &args));
			},
			AgentEvent::ToolUpdate { id, tool, args, partial } => {
				sink.send(event_wire::tool_update(&id, &tool, &args, &partial));
			},
			AgentEvent::ToolEnd { id, tool, result, error, images, details } => {
				self.saw_tool = true;
				let is_error = error.is_some();
				let content = match &error {
					Some(error) => format!("{result}\n{error}"),
					None => result.clone(),
				};
				sink.send(event_wire::tool_end(
					&id,
					&tool,
					&content,
					&images,
					details.as_ref(),
					is_error,
				));
			},
			AgentEvent::TurnEnd { tool_results, .. } => {
				let (message, results) = session.last_turn_wire(tool_results.len());
				sink.send(json!({
					"type": "turn_end",
					"message": message.unwrap_or(Value::Null),
					"toolResults": results,
				}));
				self.text.clear();
				self.turn_open = false;
			},
			// TS `emitInputMessages`: an accepted steering message reaches the host
			// as a normal user-message pair, so "accepted into the queue" and
			// "actually injected" are distinguishable from the wire alone.
			AgentEvent::UserSteer { parts } => {
				for frame in event_wire::user_message_pair(parts, event_wire::now_ms()) {
					sink.send(frame);
				}
			},
			AgentEvent::AgentEnd { .. } => {},
		}
	}
}

/// `bash` result payload, matching `BashResult` field-for-field.
fn bash_payload(result: &BashResult) -> Value {
	let mut value = json!({
		"output": result.output,
		"cancelled": result.cancelled,
		"truncated": result.truncated,
		"totalLines": result.total_lines,
		"totalBytes": result.total_bytes,
		"outputLines": result.output_lines,
		"outputBytes": result.output_bytes,
	});
	if let Some(code) = result.exit_code {
		value["exitCode"] = json!(code);
	}
	value
}

/// One finished `bash` run. `output` is the combined stream in arrival order.
struct BashResult {
	output:       String,
	exit_code:    Option<i32>,
	cancelled:    bool,
	truncated:    bool,
	total_lines:  usize,
	total_bytes:  usize,
	output_lines: usize,
	output_bytes: usize,
}

fn count_lines(text: &str) -> usize {
	if text.is_empty() {
		return 0;
	}
	text.lines().count() + usize::from(text.ends_with('\n'))
}

const MAX_BASH_OUTPUT_BYTES: usize = 1024 * 1024;

struct BashCapture {
	kept:        Vec<u8>,
	total_bytes: usize,
	total_lines: usize,
	truncated:   bool,
}

fn read_capped(mut stdout: std::process::ChildStdout) -> BashCapture {
	use std::io::Read;
	let mut kept = Vec::new();
	let mut total_bytes = 0usize;
	let mut newlines = 0usize;
	let mut last_newline = true;
	let mut chunk = [0u8; 8192];
	loop {
		let read = match stdout.read(&mut chunk) {
			Ok(0) => break,
			Ok(read) => read,
			Err(_) => break,
		};
		total_bytes += read;
		for byte in &chunk[..read] {
			if *byte == b'\n' {
				newlines += 1;
				last_newline = true;
			} else {
				last_newline = false;
			}
		}
		if kept.len() < MAX_BASH_OUTPUT_BYTES {
			let room = MAX_BASH_OUTPUT_BYTES - kept.len();
			kept.extend_from_slice(&chunk[..read.min(room)]);
		}
	}
	let total_lines = newlines + usize::from(!last_newline && total_bytes > 0);
	BashCapture {
		truncated: total_bytes > kept.len(),
		kept,
		total_bytes,
		total_lines,
	}
}

/// Run a shell command, killing it when the token is cancelled.
fn exec_bash(command: &str, cwd: &Path, token: &CancellationToken) -> BashResult {
	let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
	// `{ ... } 2>&1` merges stderr into the same pipe, so ordering is preserved
	// without a second reader thread.
	let script = format!("{{ {command}\n}} 2>&1");
	let mut child = match Command::new(&shell)
		.arg("-c")
		.arg(script)
		.current_dir(cwd)
		.stdin(Stdio::null())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		// Own process group so `abort_bash` kills the whole pipeline, not just
		// the shell that `Command::kill` reaches.
		.process_group(0)
		.spawn()
	{
		Ok(child) => child,
		Err(err) => {
			let message = format!("cannot start {shell}: {err}");
			return BashResult {
				output: message.clone(),
				exit_code: None,
				cancelled: false,
				truncated: false,
				total_lines: 1,
				total_bytes: message.len(),
				output_lines: 1,
				output_bytes: message.len(),
			};
		},
	};
	let child_pid = child.id();
	let stdout = child.stdout.take();
	let reader = stdout.map(|stdout| thread::spawn(move || read_capped(stdout)));
	let mut cancelled = false;
	let status = loop {
		match child.try_wait() {
			Ok(Some(status)) => break Some(status),
			Ok(None) => {},
			Err(_) => break None,
		}
		if token.is_cancelled() {
			cancelled = true;
			// Negative pid targets the group created by `process_group(0)`.
			unsafe {
				libc::kill(-(child_pid as i32), libc::SIGKILL);
			}
			let _ = child.kill();
			break child.wait().ok();
		}
		thread::sleep(Duration::from_millis(15));
	};
	let capture = reader
		.and_then(|handle| handle.join().ok())
		.unwrap_or(BashCapture {
			kept:        Vec::new(),
			total_bytes: 0,
			total_lines: 0,
			truncated:   false,
		});
	let output = String::from_utf8_lossy(&capture.kept).into_owned();
	BashResult {
		output_lines: count_lines(&output),
		output_bytes: capture.kept.len(),
		total_lines: capture.total_lines,
		total_bytes: capture.total_bytes,
		truncated: capture.truncated,
		output,
		exit_code: status.and_then(|status| status.code()),
		cancelled,
	}
}

/// One authoritative admission surface: liveness, cancellation token,
/// shutdown, generation and an `abort_and_prompt` replacement.
///
/// A single mutex (never separate atomics) so the command thread's admission
/// decisions and the worker's boundary commit cannot interleave: a replacement
/// can never appear after EOF, never be lost to a later abort, and a new turn
/// can never take a token that a stale abort is about to cancel.
#[derive(Default)]
struct RunState {
	/// Sticky shutdown: set at EOF/read error, never cleared.
	closed:      bool,
	/// Cancels the live turn and its queued drain.
	aborted:     bool,
	/// A run is live: a turn executes, or queued work may still run. This is the
	/// only admission source for commands (`stream`).
	running:     bool,
	/// Bumped by every `begin_run`; a worker only acts while its own generation
	/// still owns the run.
	generation:  u64,
	/// Cancellation token of the turn currently executing.
	token:       Option<CancellationToken>,
	/// Explicit replacement installed by `abort_and_prompt`.
	replacement: Option<TurnInput>,
}

/// What the run does at a turn boundary.
enum Boundary {
	/// Run this queued turn.
	Turn(TurnInput),
	/// Run this input instead; the cancelled queue dies with the old run.
	Replacement(TurnInput),
	/// The run is over: `agent_end` was already emitted by the same critical
	/// section that closed admission.
	End,
	/// A newer run owns the transport: emit nothing and touch nothing (an old
	/// worker must not clear the queue of the run that replaced it).
	Superseded,
}

#[derive(Default)]
struct RunControl {
	state: Mutex<RunState>,
}

impl RunControl {
	fn lock(&self) -> MutexGuard<'_, RunState> {
		self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
	}

	fn aborted(&self) -> bool {
		self.lock().aborted
	}

	fn closed(&self) -> bool {
		self.lock().closed
	}

	/// Admission: is a run live? The only source commands route on.
	fn stream(&self) -> bool {
		self.lock().running
	}

	/// Reserve a new run. Refuses after shutdown, so a command that arrives at or
	/// after EOF never resurrects the transport.
	fn begin_run(&self) -> Result<u64, ()> {
		let (generation, stale) = {
			let mut state = self.lock();
			if state.closed {
				return Err(());
			}
			state.generation = state.generation.wrapping_add(1);
			state.running = true;
			state.aborted = false;
			state.replacement = None;
			(state.generation, state.token.take())
		};
		if let Some(token) = stale {
			token.cancel();
		}
		Ok(generation)
	}

	/// Install the current turn's token for the run `generation` owns. A stale
	/// offer (superseded run, aborted or closed) is cancelled instead of adopted,
	/// so an abort can never be lost to a late install.
	fn install_token(&self, generation: u64, token: CancellationToken) -> bool {
		let accepted = {
			let mut state = self.lock();
			let live = state.generation == generation
				&& state.running
				&& !state.closed
				&& !state.aborted;
			if live {
				state.token = Some(token.clone());
			}
			live
		};
		if !accepted {
			token.cancel();
		}
		accepted
	}

	/// Release a run whose first turn never started (an invalid prompt or image
	/// rejected during preparation). Only the owning generation is released: a
	/// newer run or a shutdown is never disturbed, so a failed command cannot
	/// leave a phantom live run that blocks the next prompt.
	fn abandon_run(&self, generation: u64) -> bool {
		let token = {
			let mut state = self.lock();
			if state.closed || state.generation != generation || !state.running {
				return false;
			}
			state.running = false;
			state.aborted = false;
			state.replacement = None;
			state.token.take()
		};
		if let Some(token) = token {
			token.cancel();
		}
		true
	}

	/// Plain abort: cancel the live turn and drop everything pending with it.
	/// Returns whether a run was live. The token is captured here, so the caller
	/// must not cancel anything afterwards (a replacement may already own a new
	/// scope).
	fn abort(&self) -> bool {
		let token = {
			let mut state = self.lock();
			if !state.running {
				return false;
			}
			state.aborted = true;
			state.replacement = None;
			state.token.take()
		};
		if let Some(token) = token {
			token.cancel();
		}
		true
	}

	/// `abort_and_prompt`: retire the current token and make `input` the next
	/// thing this run executes, in one critical section. `Err(input)` means no run
	/// is live, so the caller starts a fresh one instead of losing the prompt.
	fn replace(&self, input: TurnInput) -> Result<(), TurnInput> {
		let token = {
			let mut state = self.lock();
			if !state.running || state.closed {
				return Err(input);
			}
			state.aborted = true;
			state.replacement = Some(input);
			state.token.take()
		};
		if let Some(token) = token {
			token.cancel();
		}
		Ok(())
	}

	/// Queue a follow-up only while the run is live, in one critical section with
	/// the drain that consumes the queue: an acknowledged follow-up can therefore
	/// never fall between `next_queued` and the boundary commit.
	fn admit_follow_up(
		&self,
		input: TurnInput,
		queue: impl FnOnce(TurnInput),
	) -> Result<(), TurnInput> {
		let state = self.lock();
		if !state.running || state.closed || state.aborted {
			return Err(input);
		}
		queue(input);
		Ok(())
	}

	/// Sticky shutdown: outranks any replacement and is never cleared.
	fn close(&self) {
		let token = {
			let mut state = self.lock();
			state.closed = true;
			state.aborted = true;
			state.running = false;
			state.replacement = None;
			state.token.take()
		};
		if let Some(token) = token {
			token.cancel();
		}
	}

	/// Turn boundary commit. The drain, the terminal `agent_end` and the queue
	/// clear all run inside this critical section, so: a queued follow-up cannot
	/// slip past the drain, a replacement published before the commit is executed,
	/// the cancelled queue is gone *before* admission reopens (a follow-up admitted
	/// after `running = false` can never be wiped by the worker that just ended),
	/// and a later command is admitted as a fresh run instead of vanishing into a
	/// dead worker.
	///
	/// `clear_queue` receives the state from inside this lock: callers read
	/// `running` there instead of calling a `RunControl` accessor, which would
	/// re-lock the same mutex and deadlock.
	fn boundary(
		&self,
		generation: u64,
		drain: impl FnOnce() -> Option<TurnInput>,
		emit_end: impl FnOnce(),
		clear_queue: impl FnOnce(&RunState),
	) -> Boundary {
		let mut state = self.lock();
		if state.generation != generation {
			// A newer run owns the transport: this worker emits nothing and
			// releases nothing.
			return Boundary::Superseded;
		}
		let end = |state: &mut RunState| {
			state.running = false;
			state.token = None;
			state.replacement = None;
		};
		if state.closed {
			emit_end();
			clear_queue(&state);
			end(&mut state);
			return Boundary::End;
		}
		if let Some(input) = state.replacement.take() {
			// The aborted run's queued follow-ups die with it, before admission
			// reopens.
			clear_queue(&state);
			state.aborted = false;
			return Boundary::Replacement(input);
		}
		if state.aborted {
			emit_end();
			clear_queue(&state);
			end(&mut state);
			return Boundary::End;
		}
		match drain() {
			Some(input) => Boundary::Turn(input),
			None => {
				emit_end();
				clear_queue(&state);
				end(&mut state);
				Boundary::End
			},
		}
	}
}

/// The RPC transport plus the session it drives.
pub struct RpcMode {
	session:  Arc<Mutex<RpcSession>>,
	sink:     Arc<Output>,
	active:   Arc<ActiveTurn>,
	ui:       Arc<ExtensionUiBridge>,
	shutdown: ShutdownCoordinator,
	/// Shutdown, abort and replacement as one locked state.
	control:  Arc<RunControl>,
	registry: Arc<SubagentRegistry>,
	/// Supervisor-agnostic handle handed to the runner through
	/// `RunRequest.subagent_observer`; the runner subscribes it once per
	/// supervisor. Forwards nothing after the registry is detached on close.
	observer: Arc<dyn SubagentObserver>,
}

impl RpcMode {
	/// Announce the transport bounds, open the session and drive stdin until
	/// EOF.
	///
	/// The `ready` frame is written here, through the same [`Output`] writer as
	/// every later frame, so a caller must never write to stdout itself: two
	/// writers would race the negotiated protocol state and frame order.
	pub fn run(
		launch: RpcLaunch,
		mut reader: impl std::io::BufRead,
		writer: impl std::io::Write + Send + 'static,
	) -> crate::Result<()> {
		let output = Arc::new(Output::spawn(writer));
		let sink: Arc<dyn FrameSink> = output.clone();
		// The extension-UI bridge exists before the approval surface, so every
		// permission prompt is an `extension_ui_request` answered by the host
		// instead of a read of the process terminal.
		let ui = Arc::new(ExtensionUiBridge::new(Arc::clone(&sink)));
		let approval_input: crate::approve::ApprovalInput = Arc::new(Mutex::new(Box::new(
			super::input::RpcApprovalInput::new(Arc::clone(&ui), None),
		)));
		let mut approvals = crate::approve::Approvals::new(
			launch.approval_mode,
			launch.allow.iter().cloned(),
		)
		.with_input(Some(approval_input));
		approvals.set_ask(Some(Arc::new(super::input::RpcAskChannel::new(
			Arc::clone(&ui),
		))));
		let host_tools = Arc::new(HostToolBridge::new(Arc::clone(&sink)));
		let host_uris = Arc::new(HostUriBridge::new(Arc::clone(&sink)));
		// One router for the whole session: the bridge mounts the host schemes it
		// validated, and the runner resolves them through the same handle.
		let internal_urls = Arc::new(InternalUrlRouter::new());
		host_uris.bind(Arc::clone(&internal_urls));
		let registry = Arc::new(SubagentRegistry::new());
		let observer = registry.observer(Arc::clone(&sink));
		let session = RpcSession::open(
			launch,
			approvals,
			host_tools,
			host_uris,
			internal_urls,
			Arc::clone(&observer),
		)?;
		sink.send(super::types::ready_frame(
			MAX_RPC_FRAME_BYTES,
			MAX_RPC_REASSEMBLED_BYTES,
		));
		let mut mode = Self {
			session: Arc::new(Mutex::new(session)),
			sink: output.clone(),
			active: Arc::new(ActiveTurn::default()),
			ui,
			shutdown: ShutdownCoordinator::new(),
			control: Arc::new(RunControl::default()),
			registry,
			observer,
		};
		let read_error = mode.event_loop(&mut reader);
		mode.finish();
		let flush_error = output.finish();
		read_error
			.map(|error| crate::Error::new(format!("RPC input read failed: {error}")))
			.or_else(|| flush_error.map(crate::Error::new))
			.map_or(Ok(()), Err)
	}

	/// Pump inbound frames until EOF or a fatal stdin error.
	///
	/// EOF is a clean shutdown. A read error is fatal: it is returned so the
	/// caller runs the same teardown as EOF (in-flight turns and bash runs
	/// aborted, pending host tool/URI calls and approval dialogs failed closed)
	/// and flushes the writer — never logged and swallowed, never panicked.
	fn event_loop(&mut self, reader: &mut impl std::io::BufRead) -> Option<std::io::Error> {
		let mut input = RpcInput::new(reader);
		loop {
			match input.next() {
				Ok(None) => return None,
				Ok(Some(Err(error))) => {
					// Upstream logs a malformed line and keeps reading.
					eprintln!("warning: {error}");
				},
				Ok(Some(Ok(Inbound::Control(control)))) => self.handle_control(control),
				Ok(Some(Ok(Inbound::Command(value)))) => self.handle_value(value),
				Err(error) => return Some(error),
			}
		}
	}

	fn handle_control(&mut self, control: RpcControlFrame) {
		match control {
			RpcControlFrame::ExtensionUi(response) => {
				self.ui.handle(response);
			},
			RpcControlFrame::HostToolResult(frame) => {
				let bridge = Arc::clone(&self.lock_session().host_tools);
				bridge.handle_result(frame);
			},
			RpcControlFrame::HostToolUpdate(frame) => {
				let bridge = Arc::clone(&self.lock_session().host_tools);
				bridge.handle_update(frame);
			},
			RpcControlFrame::HostUriResult(frame) => {
				let bridge = Arc::clone(&self.lock_session().host_uris);
				bridge.handle_result(frame);
			},
		}
	}

	fn handle_value(&mut self, value: Value) {
		let command: RpcCommand = match serde_json::from_value(value) {
			Ok(command) => command,
			Err(err) => {
				self.sink.send(
					RpcResponse::error(None, "unknown", format!("Invalid RPC command: {err}"))
						.into_value(),
				);
				return;
			},
		};
		if let Some(response) = self.handle_command(command) {
			self.sink.send(response.into_value());
		}
	}

	/// Observer handle for `RunRequest.subagent_observer`. The runner subscribes
	/// it once, right after its lazily built supervisor exists; the handle is
	/// supervisor-agnostic so a replacement supervisor can reuse it.
	#[must_use]
	pub fn subagent_observer(&self) -> Arc<dyn SubagentObserver> {
		Arc::clone(&self.observer)
	}

	fn finish(&mut self) {
		// Sticky shutdown first: it outranks an `abort_and_prompt` replacement, so
		// a worker draining the last turn can never start one after EOF.
		// `close()` is the authoritative stop: it cancels the live token and the
		// queued drain, so no separate `ActiveTurn::abort` exists any more.
		self.control.close();
		self.ui.cancel_all();
		self.active.abort_bash();
		// Closed: stop forwarding subagent events and drop live snapshots.
		self.registry.detach();
		self.registry.clear();
		{
			let session = self.lock_session();
			session.host_tools.close("RPC session closed");
			session.host_uris.close("RPC session closed");
			session.host_uris.clear_schemes();
		}
		{
			let mut session = self.lock_session();
			session.is_streaming = false;
		}
		self.shutdown.drain();
	}

	fn lock_session(&self) -> MutexGuard<'_, RpcSession> {
		self.session.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
	}

	// --------------------------------------------------------------- command --

	/// Handle one command. `None` means the response is sent asynchronously.
	fn handle_command(&mut self, command: RpcCommand) -> Option<RpcResponse> {
		let id = command.id().map(str::to_string);
		let id_ref = id.as_deref();
		let kind = command.kind();
		let ok = || RpcResponse::success(id_ref, kind);
		let ok_data = |data: Value| RpcResponse::success_data(id_ref, kind, data);
		let err = |message: String| RpcResponse::error(id_ref, kind, message);
		let prereq = |prerequisite: Prerequisite| {
			RpcResponse::error_code(id_ref, kind, prerequisite.message, prerequisite.code)
		};

		match command {
			RpcCommand::NegotiateProtocol { protocol_version, .. } => {
				if protocol_version != 2 {
					return Some(err(format!(
						"Unsupported protocol version: {protocol_version}"
					)));
				}
				// The writer flips to v2 once this response is written.
				Some(ok_data(json!({ "protocolVersion": 2 })))
			},

			RpcCommand::Prompt { message, images, streaming_behavior, .. } => {
				let input = TurnInput {
					text:   self.expand_skill(&message),
					images: images.unwrap_or_default(),
				};
				Some(self.prompt(input, streaming_behavior, id))
			},
			RpcCommand::Steer { message, images, .. } => {
				let input = TurnInput { text: message, images: images.unwrap_or_default() };
				Some(self.steer(input, id))
			},
			RpcCommand::FollowUp { message, images, .. } => {
				let input = TurnInput { text: message, images: images.unwrap_or_default() };
				Some(self.follow_up(input, id))
			},
			RpcCommand::Abort { .. } => {
				// Cancelled inside the run lock; nothing here touches `ActiveTurn`, or a
				// replacement's fresh scope could be cancelled by a stale abort.
				self.control.abort();
				self.ui.cancel_all();
				Some(ok())
			},
			RpcCommand::AbortAndPrompt { message, images, .. } => {
				let input = TurnInput { text: self.expand_skill(&message), images: images.unwrap_or_default() };
				// Admission and the replacement are one transition: if the run already
				// ended, this prompt starts a fresh run instead of being lost.
				match self.control.replace(input) {
					Ok(()) => {
						self.ui.cancel_all();
						Some(ok())
					},
					Err(input) => {
						let response = ok();
						self.start_turn(input, id);
						Some(response)
					},
				}
			},
			RpcCommand::NewSession { parent_session, .. } => {
				if self.control.stream() {
					return Some(err(
						"Cannot start a new session while a response is streaming".to_string(),
					));
				}
				Some(match self.lock_session().new_session(parent_session.as_deref()) {
					Ok(()) => {
						// A new session must not surface the previous session's
						// agents: live snapshots drop and their ids go stale, so
						// late events for them are ignored. The observer handle
						// stays attached for the next supervisor's subscription.
						self.registry.clear();
						ok_data(json!({ "cancelled": false }))
					},
					Err(error) => err(error.to_string()),
				})
			},

			RpcCommand::GetState { .. } => {
				let session = self.lock_session();
				let system_prompt = session.prompts.system.clone().map(|prompt| vec![prompt]);
				Some(ok_data(
					serde_json::to_value(session.state(system_prompt)).unwrap_or(Value::Null),
				))
			},
			RpcCommand::SetFastMode { enabled, .. } => {
				let supported = self.lock_session().set_fast_mode(enabled);
				if enabled && !supported {
					return Some(err(
						"Fast mode is unavailable for the current model.".to_string(),
					));
				}
				let session = self.lock_session();
				Some(ok_data(json!({
					"enabled": session.fast_mode_enabled(),
					"active": session.fast_mode_active(),
				})))
			},
			RpcCommand::GetAvailableCommands { .. } => Some(ok_data(json!({
				"commands": self
					.lock_session()
					.skills
					.iter()
					.map(|skill| json!({
						"name": format!("skill:{}", skill.name),
						"description": skill.description,
						"source": "skill",
					}))
					.collect::<Vec<Value>>(),
			}))),
			RpcCommand::SetTodos { phases, .. } => {
				self.lock_session().set_todos(phases);
				let phases = self.lock_session().todo_phases.clone();
				Some(ok_data(json!({ "todoPhases": phases })))
			},
			RpcCommand::SetHostTools { tools, .. } => {
				let bridge = Arc::clone(&self.lock_session().host_tools);
				Some(match bridge.set_tools(tools) {
					Err(error) => err(error),
					// Mounted by `RpcSession::request` into the next turn's
					// registry (`RunRequest.extra_tools`), so the response is the
					// upstream one: the accepted names.
					Ok(adapters) => ok_data(json!({
						"toolNames": adapters
							.iter()
							.map(|adapter| adapter.name().to_string())
							.collect::<Vec<String>>(),
					})),
				})
			},
			RpcCommand::SetHostUriSchemes { schemes, .. } => {
				let bridge = Arc::clone(&self.lock_session().host_uris);
				Some(match bridge.set_schemes(&schemes) {
					Err(error) => err(error),
					// The session's router is mounted for its lifetime, so the
					// validated set is live: report it instead of a prerequisite.
					Ok(schemes) => ok_data(host_uris::schemes_payload(&schemes)),
				})
			},
			RpcCommand::SetSubagentSubscription { level, .. } => {
				self.lock_session().subagent_level = level;
				self.registry.set_subscription_level(level);
				Some(ok_data(json!({ "level": level.as_str() })))
			},
			RpcCommand::GetSubagents { .. } => {
				if self.registry.subscription_level() == SubagentSubscriptionLevel::Off {
					return Some(ok_data(json!({ "subagents": [] })));
				}
				Some(ok_data(self.registry.snapshots_wire()))
			},
			RpcCommand::GetSubagentMessages { subagent_id, session_file, from_byte, .. } => {
				Some(self.subagent_messages(
					id_ref,
					subagent_id.as_deref(),
					session_file.as_deref(),
					from_byte,
				))
			},

			RpcCommand::SetModel { provider, model_id, .. } => {
				let result = self.lock_session().set_model(&provider, &model_id);
				Some(match result {
					Err(error) => err(error.to_string()),
					Ok(()) => match self.lock_session().model_object() {
						Some(model) => ok_data(model),
						None => err(format!("Model not found: {provider}/{model_id}")),
					},
				})
			},
			RpcCommand::CycleModel { .. } => {
				Some(match self.lock_session().cycle_model() {
					Some(model) => ok_data(model),
					None => ok(),
				})
			},
			RpcCommand::GetAvailableModels { .. } => {
				let models = self.lock_session().available_models();
				Some(ok_data(json!({ "models": models })))
			},

			RpcCommand::SetThinkingLevel { level, .. } => {
				Some(match self.lock_session().set_thinking_level(&level) {
					Ok(()) => ok(),
					Err(error) => err(error.to_string()),
				})
			},
			RpcCommand::CycleThinkingLevel { .. } => {
				Some(match self.lock_session().cycle_thinking_level() {
					Ok(level) => ok_data(level),
					Err(error) => err(error.to_string()),
				})
			},

			RpcCommand::SetSteeringMode { mode, .. } => {
				self.lock_session().steering_mode = mode;
				Some(ok_data(Value::Null))
			},
			RpcCommand::SetFollowUpMode { mode, .. } => {
				self.lock_session().follow_up_mode = mode;
				Some(ok_data(Value::Null))
			},
			RpcCommand::SetInterruptMode { mode, .. } => {
				self.lock_session().interrupt_mode = mode;
				Some(ok_data(Value::Null))
			},

			RpcCommand::Compact { custom_instructions, .. } => {
				if self.control.stream() {
					return Some(err(
						"Cannot compact while a response is streaming".to_string(),
					));
				}
				Some(match self.lock_session().compact(custom_instructions) {
					Err(error) => err(error.to_string()),
					Ok(result) => ok_data(result),
				})
			},
			// Both settings are policy the native turn loop owns: the flag is
			// forwarded on `RunRequest` (parent seam) and the loop emits
			// `auto_compaction_start/end` / `auto_retry_start/end`, which RPC
			// relays. RPC never runs a second compaction or retry loop.
			RpcCommand::SetAutoCompaction { enabled, .. } => {
				self.lock_session().auto_compaction = enabled;
				Some(ok())
			},

			RpcCommand::SetAutoRetry { enabled, .. } => {
				self.lock_session().auto_retry = enabled;
				Some(ok())
			},
			RpcCommand::AbortRetry { .. } => {
				if let Some(token) = self.lock_session().retry_abort.take() {
					token.cancel();
				}
				Some(ok())
			},

			RpcCommand::Bash { command, .. } => {
				self.spawn_bash(command, id);
				None
			},
			RpcCommand::AbortBash { .. } => {
				self.active.abort_bash();
				Some(ok())
			},

			RpcCommand::GetSessionStats { .. } => Some(ok_data(self.lock_session().stats())),
			RpcCommand::ExportHtml { output_path, .. } => {
				Some(match self.lock_session().export_html(output_path.as_deref()) {
					Ok(path) => ok_data(json!({ "path": path })),
					Err(error) => err(error.to_string()),
				})
			},
			RpcCommand::SwitchSession { session_path, .. } => {
				if self.control.stream() {
					return Some(err(
						"Cannot switch sessions while a response is streaming".to_string(),
					));
				}
				Some(match self.lock_session().switch_session(Path::new(&session_path)) {
					Ok(()) => ok_data(json!({ "cancelled": false })),
					Err(error) => err(error.to_string()),
				})
			},
			RpcCommand::Branch { entry_id, .. } => {
				// The run, not the session flag, decides: the whole worker (queued
				// drain and closing `agent_end` included) stays reserved.
				Some(if self.control.stream() {
					prereq(Prerequisite::new(
						"busy",
						"cannot branch while a turn is streaming: abort it first",
					))
				} else {
					match self.lock_session().branch(&entry_id) {
						Ok(result) => ok_data(result),
						Err(prerequisite) => prereq(prerequisite),
					}
				})
			},
			RpcCommand::GetBranchMessages { .. } => {
				let messages = self.lock_session().branch_messages();
				Some(ok_data(json!({ "messages": messages })))
			},
			RpcCommand::GetLastAssistantText { .. } => {
				let text = self.lock_session().last_assistant_text().map(str::to_string);
				Some(ok_data(json!({ "text": text })))
			},
			RpcCommand::SetSessionName { name, .. } => {
				let trimmed = name.trim();
				if trimmed.is_empty() {
					return Some(err("Session name cannot be empty".to_string()));
				}
				Some(match self.lock_session().set_session_name(trimmed) {
					Ok(()) => ok(),
					Err(error) => err(error.to_string()),
				})
			},
			RpcCommand::Handoff { custom_instructions, .. } => {
				if self.control.stream() {
					return Some(err(
						"Cannot hand off while a response is in progress".to_string(),
					));
				}
				Some(match self.lock_session().handoff(custom_instructions) {
					Err(error) => err(error.to_string()),
					Ok(Some(saved_path)) => ok_data(json!({ "savedPath": saved_path })),
					Ok(None) => ok(),
				})
			},

			RpcCommand::GetMessages { .. } => {
				let messages = self.lock_session().wire_messages().to_vec();
				Some(ok_data(json!({ "messages": messages })))
			},
			RpcCommand::GetMessagesPage { cursor, limit, .. } => {
				Some(self.messages_page(id_ref, cursor.as_deref(), limit))
			},

			RpcCommand::GetLoginProviders { .. } => Some(RpcResponse::error_code(
				id_ref,
				kind,
				"No OAuth provider registry or credential store exists in this build (native \
				 seam: provider auth registry + credential store; RPC maps its providers and \
				 the login callbacks to extension_ui_request frames)",
				"auth_unavailable",
			)),
			RpcCommand::Login { provider_id, .. } => Some(RpcResponse::error_code(
				id_ref,
				kind,
				format!(
					"Cannot log in to '{provider_id}': this build has no credential store (native \
					 seam: provider auth registry + credential store; RPC maps the login \
					 callbacks to extension_ui_request frames)"
				),
				"auth_unavailable",
			)),
		}
	}

	// --------------------------------------------------------------- prompts --

	fn expand_skill(&self, text: &str) -> String {
		let trimmed = text.trim_start();
		let Some(rest) = trimmed.strip_prefix("/skill:") else {
			return text.to_string();
		};
		let (name, arguments) = match rest.find(char::is_whitespace) {
			Some(index) => (&rest[..index], rest[index..].trim()),
			None => (rest, ""),
		};
		let session = self.lock_session();
		let Some(skill) = session.skills.iter().find(|skill| skill.name == name) else {
			return text.to_string();
		};
		let mut expanded = skill.body.trim().to_string();
		if !arguments.is_empty() {
			expanded.push_str("\n\nUser arguments: ");
			expanded.push_str(arguments);
		}
		expanded
	}

	fn prompt(
		&mut self,
		input: TurnInput,
		behavior: Option<StreamingBehavior>,
		prompt_id: Option<String>,
	) -> RpcResponse {
		// Admission, not the live token: a run that is still draining is live even
		// between two of its turns.
		if self.control.stream() {
			return match behavior.unwrap_or(StreamingBehavior::Steer) {
				StreamingBehavior::Steer => self.steer(input, prompt_id),
				StreamingBehavior::FollowUp => self.follow_up(input, prompt_id),
			};
		}
		let response = RpcResponse::success(prompt_id.as_deref(), "prompt");
		self.start_turn(input, prompt_id);
		response
	}

	fn steer(&mut self, input: TurnInput, prompt_id: Option<String>) -> RpcResponse {
		let id_ref = prompt_id.as_deref();
		if !self.control.stream() {
			let response = RpcResponse::success(prompt_id.as_deref(), "steer");
			self.start_turn(input, prompt_id);
			return response;
		}
		let message = SteeringMessage { text: input.text.clone(), images: input.images.clone() };
		let validation = self.lock_session().validate_steering_images(&input.images);
		if let Err(prerequisite) = validation {
			return RpcResponse::error_code(
				id_ref,
				"steer",
				prerequisite.message,
				prerequisite.code,
			);
		}
		// Snapshot under a short-lived guard: a `match` on the guard's field would
		// keep the session locked while `queue_live` needs it again.
		let interrupt_mode = self.lock_session().interrupt_mode;
		match interrupt_mode {
			InterruptMode::Wait => {
				self.queue_live(input);
				RpcResponse::success(id_ref, "steer")
			},
			InterruptMode::Immediate => match self.active.steer(&message) {
				Ok(()) => RpcResponse::success(id_ref, "steer"),
				// The run is live but the current turn's channel is momentarily
				// absent (between two queued turns): queue it for the next turn
				// instead of starting a second worker.
				Err(prerequisite) if prerequisite.code == "not_streaming" => {
					self.queue_live(input);
					RpcResponse::success(id_ref, "steer")
				},
				Err(prerequisite) => {
					RpcResponse::error_code(id_ref, "steer", prerequisite.message, prerequisite.code)
				},
			},
		}
	}

	fn follow_up(&mut self, input: TurnInput, prompt_id: Option<String>) -> RpcResponse {
		let id_ref = prompt_id.as_deref();
		let response = RpcResponse::success(id_ref, "follow_up");
		// Admission and the append are one critical section with the worker's
		// drain, so an acknowledged follow-up is never stranded.
		match self
			.control
			.admit_follow_up(input, |input| {
				self.lock_session().queue_follow_up(input);
			})
		{
			Ok(()) => response,
			Err(input) => {
				self.start_turn(input, prompt_id);
				response
			},
		}
	}

	/// Queue into the live run; a run that ended in the meantime starts fresh.
	fn queue_live(&mut self, input: TurnInput) {
		if self
			.control
			.admit_follow_up(input.clone(), |input| {
				self.lock_session().queue_follow_up(input);
			})
			.is_err()
		{
			self.start_turn(input, None);
		}
	}

	// ------------------------------------------------------------------ turns --

	fn start_turn(&mut self, first: TurnInput, prompt_id: Option<String>) {
		// Reserve the run on the command thread, before the worker exists:
		// `steer`/`abort` sent right after this `prompt` must see a live turn, the
		// prompt entry is journaled in command order, and an invalid prompt is
		// answered before `agent_start`. The reservation lasts the whole run —
		// queued drain and the closing `agent_end` included.
		let session = Arc::clone(&self.session);
		let sink: Arc<dyn FrameSink> = self.sink.clone();
		let active = Arc::clone(&self.active);
		let control = Arc::clone(&self.control);
		// A new run owns the scope: admission opens here, once, and the worker never
		// resets it. `begin_run` refuses after shutdown, so a prompt arriving at or
		// after EOF cannot resurrect the transport.
		let generation = match self.control.begin_run() {
			Ok(generation) => generation,
			Err(()) => {
				sink.send(
					RpcResponse::error(
						prompt_id.as_deref(),
						"prompt",
						"the session is shutting down".to_string(),
					)
					.into_value(),
				);
				return;
			},
		};
		let prepared = {
			let mut guard = session.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
			guard.prepare_turn(&first)
		};
		let (request, prepared) = match prepared {
			Ok(pair) => pair,
			Err(error) => {
				// No worker exists for this run: release the reservation so the next
				// prompt is admitted instead of hitting a phantom busy run. The
				// rollback is generation-guarded, so a shutdown or a newer run that
				// landed meanwhile is never disturbed. The session guard is already
				// released here, keeping the run -> session lock order.
				self.control.abandon_run(generation);
				sink.send(
					RpcResponse::error(prompt_id.as_deref(), "prompt", error.to_string())
						.into_value(),
				);
				return;
			},
		};
		let steer = {
			let guard = session.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
			guard.steer.clone()
		};
		active.begin(generation, prepared.token.clone(), steer);
		// The reservation's token is the run's cancellation authority until the
		// worker installs the next turn's token.
		let _ = control.install_token(generation, prepared.token.clone());
		let handle = thread::Builder::new()
			.name("omp-rpc-turn".to_string())
			.spawn(move || {
				let mut next = Some((first, prompt_id, Some((request, prepared))));
				sink.send(json!({ "type": "agent_start" }));
				while let Some((input, prompt_id, ready)) = next {
					let outcome = run_turn(
						&session,
						&sink,
						&active,
						&control,
						generation,
						input,
						prompt_id.clone(),
						ready,
					);
					if let Some(error) = &outcome.error {
						sink.send(
							RpcResponse::error(prompt_id.as_deref(), "prompt", error.clone())
								.into_value(),
						);
					}
					if outcome.error.is_none() && !outcome.aborted {
						maybe_auto_compact(&session, &sink);
					}
					// Turn boundary, committed inside the run lock: the queue drain and
					// the terminal `agent_end` are one critical section, so a queued
					// follow-up cannot slip past and a command arriving after the commit
					// is admitted as a fresh run instead of vanishing here.
					next = match control.boundary(
						generation,
						|| {
							let mut guard = session
								.lock()
								.unwrap_or_else(std::sync::PoisonError::into_inner);
							match guard.next_queued() {
								TurnAdvance::Run(input) => Some(input),
								TurnAdvance::Idle => None,
							}
						},
						|| {
							let messages = session
								.lock()
								.unwrap_or_else(std::sync::PoisonError::into_inner)
								.wire_messages()
								.to_vec();
							sink.send(json!({
								"type": "agent_end",
								"messages": messages,
								"willContinue": false,
							}));
						},
						// Inside the commit, before admission reopens: a follow-up
						// admitted right after the release is never wiped here. The
						// state argument is the locked one; never re-lock `control`.
						|_| {
							session
								.lock()
								.unwrap_or_else(std::sync::PoisonError::into_inner)
								.queued
								.clear();
						},
					) {
						Boundary::Turn(input) => Some((input, None, None)),
						Boundary::Replacement(replacement) => Some((replacement, None, None)),
						// A newer run owns the queue and the surface: leave both alone.
						Boundary::End | Boundary::Superseded => None,
					};
				}
				// `agent_end` and admission release already happened inside the boundary
				// commit; this only releases the steering surface, and only if this
				// generation still owns it (a newer run may have taken over).
				let _ = active.end_if(generation);
			})
			.expect("spawn rpc turn thread");
		self.shutdown.track(handle);
	}

	// ------------------------------------------------------------------- bash --

	fn spawn_bash(&mut self, command: String, id: Option<String>) {
		let (bash_id, token) = self.active.register_bash();
		let cwd = self.lock_session().cwd.clone();
		let sink: Arc<dyn FrameSink> = self.sink.clone();
		let active = Arc::clone(&self.active);
		let handle = thread::Builder::new()
			.name("omp-rpc-bash".to_string())
			.spawn(move || {
				let result = exec_bash(&command, &cwd, &token);
				active.clear_bash(bash_id);
				sink.send(
					RpcResponse::success_data(id.as_deref(), "bash", bash_payload(&result))
						.into_value(),
				);
			})
			.expect("spawn rpc bash thread");
		self.shutdown.track(handle);
	}

	// --------------------------------------------------------------- messages --

	fn messages_page(
		&mut self,
		id: Option<&str>,
		cursor: Option<&str>,
		limit: Option<i64>,
	) -> RpcResponse {
		if self.control.stream() || self.lock_session().is_compacting {
			return RpcResponse::error_code(
				id,
				"get_messages_page",
				"Cannot page messages while the session is streaming or compacting",
				"session_busy",
			);
		}
		let session = self.lock_session();
		let snapshot = MessageSnapshot {
			session_id:    session.session_id(),
			leaf_id:       session.store.leaf_id().map(str::to_string),
			message_count: session.wire_messages().len(),
		};
		match messages::page_messages(session.wire_messages(), &snapshot, (cursor, limit)) {
			Ok(page) => RpcResponse::success_data(id, "get_messages_page", page.to_wire()),
			Err(error) => match error.code {
				Some(code) => RpcResponse::error_code(
					id,
					"get_messages_page",
					error.message,
					code.as_str(),
				),
				None => RpcResponse::error(id, "get_messages_page", error.message),
			},
		}
	}

	fn subagent_messages(
		&self,
		id: Option<&str>,
		subagent_id: Option<&str>,
		session_file: Option<&str>,
		from_byte: Option<i64>,
	) -> RpcResponse {
		let from_byte = from_byte.unwrap_or(0).max(0);
		let file = match self.registry.resolve_session_file(subagent_id, session_file) {
			Ok(file) => file,
			Err(error) => return RpcResponse::error(id, "get_subagent_messages", error),
		};
		match subagents::read_transcript(&file, from_byte) {
			Ok(slice) => RpcResponse::success_data(id, "get_subagent_messages", slice.to_wire()),
			Err(error) => RpcResponse::error(id, "get_subagent_messages", error),
		}
	}
}

/// Post-turn auto-compaction against persisted context usage.
///
/// ponytail: 90% of the advertised window until the compaction-settings seam
/// (threshold tokens/percent) lands; the summary itself is the real model path
/// (`RpcSession::compact`), never a stub.
fn maybe_auto_compact(session: &Arc<Mutex<RpcSession>>, sink: &Arc<dyn FrameSink>) {
	let (enabled, percent, busy) = {
		let guard = session.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		(
			guard.auto_compaction,
			guard
				.context_usage()
				.and_then(|usage| usage["percent"].as_f64()),
			guard.is_compacting,
		)
	};
	if !enabled || busy || percent.map_or(true, |percent| percent < 90.0) {
		return;
	}
	sink.send(json!({
		"type": "auto_compaction_start",
		"reason": "threshold",
		"action": "context-full",
	}));
	let result = {
		let mut guard = session.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		guard.compact(None)
	};
	match result {
		Ok(value) => sink.send(json!({
			"type": "auto_compaction_end",
			"action": "context-full",
			"result": value,
			"aborted": false,
			"willRetry": false,
			"errorMessage": null,
		})),
		Err(error) => sink.send(json!({
			"type": "auto_compaction_end",
			"action": "context-full",
			"result": null,
			"aborted": false,
			"willRetry": false,
			"errorMessage": error.to_string(),
		})),
	}
}

/// Whether a stream-failure message is transient enough to retry.
///
/// Mirrors upstream `isRetryableError` (`turn-recovery.ts`): only provider
/// transport/limit failures retry. Permanent failures — auth/account, request
/// validation, model or image rejection, context overflow (compaction owns it),
/// the user's own max-time budget, and interrupts — are reported, never retried.
/// Unknown text is treated as permanent, like upstream's default-deny
/// [`AIError::retriable`].
fn is_retryable_stream_error(error: &str) -> bool {
	let lower = error.to_ascii_lowercase();
	const PERMANENT: [&str; 14] = [
		"unauthorized",
		"forbidden",
		"authentication",
		"api key",
		"permission",
		"invalid_request",
		"invalid request",
		"context length",
		"context window",
		"context overflow",
		"does not support",
		"not found",
		"interrupted",
		"dialog stream closed unexpectedly",
	];
	if error == crate::deadline::TIMEOUT_MESSAGE || PERMANENT.iter().any(|needle| lower.contains(needle)) {
		return false;
	}
	const TRANSIENT: [&str; 14] = [
		"429",
		"rate limit",
		"overloaded",
		"500",
		"502",
		"503",
		"504",
		"timed out",
		"timeout",
		"connection",
		"reset by peer",
		"network",
		"stream ended before completion",
		"upstream",
	];
	TRANSIENT.iter().any(|needle| lower.contains(needle))
}

/// One pumped turn: prepare, stream frames, persist, then announce the end.
fn run_turn(
	session: &Arc<Mutex<RpcSession>>,
	sink: &Arc<dyn FrameSink>,
	active: &Arc<ActiveTurn>,
	control: &Arc<RunControl>,
	generation: u64,
	input: TurnInput,
	_prompt_id: Option<String>,
	ready: Option<(RunRequest, PreparedTurn)>,
) -> TurnOutcome {
	// The first turn arrives already prepared (and reserved) from the command
	// thread; a queued turn is prepared here under the run's existing guard.
	let prepared = match ready {
		Some(pair) => Ok(pair),
		None => {
			// Decide before taking the session guard: the run lock is never held
			// while acquiring the session lock, which is the only nesting order used
			// in this module.
			// An aborted or shutting-down run drops queued turns before they are
			// prepared, so such a prompt is never journaled as a stray user message.
			if control.aborted() || control.closed() {
				return TurnOutcome { aborted: true, ..TurnOutcome::default() };
			}
			let mut guard = session.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
			guard.prepare_turn(&input)
		},
	};
	let (mut request, mut prepared) = match prepared {
		Ok(pair) => pair,
		Err(error) => {
			return TurnOutcome { error: Some(error.to_string()), ..TurnOutcome::default() };
		},
	};
	let (meta, steer) = {
		let guard = session.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		(
			Meta {
				api:      guard.model_selector(),
				provider: guard.provider.clone(),
				model:    guard.model.clone(),
			},
			guard.steer.clone(),
		)
	};
	// Cloned because a retry re-registers the same steering hook on its own token.
	active.begin(generation, prepared.token.clone(), steer.clone());

	let runner = {
		let guard = session.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		Arc::clone(&guard.runner)
	};
	let mut outcome = TurnOutcome::default();
	let mut attempt = 0u32;
	const MAX_RETRY_ATTEMPTS: u32 = 3;
	loop {
		let auto_retry = {
			let guard = session
				.lock()
				.unwrap_or_else(std::sync::PoisonError::into_inner);
			guard.auto_retry
		};
		// Hand this turn's token to the run. A stale offer (the run was superseded,
		// aborted or closed while we prepared) is cancelled by `install_token`
		// instead of adopted, so a fresh provider request can never start for a run
		// that no longer owns the transport.
		let owns_run = control.install_token(generation, prepared.token.clone());
		// Never reach the provider for a cancelled run: a replacement cleared the
		// abort inside the lock, but shutdown and a plain abort must stop here even
		// when they landed while this turn was being prepared.
		if !owns_run || control.aborted() || control.closed() || prepared.token.is_cancelled() {
			outcome.aborted = true;
			let mut guard = session
				.lock()
				.unwrap_or_else(std::sync::PoisonError::into_inner);
			guard.finish_turn(&mut prepared, &outcome);
			return outcome;
		}
		// Provider failures do not surface here: `runner.run` only rejects setup
		// (model plan, MCP discovery, provider construction), which is permanent
		// and must never be retried. The turn's own failures arrive as stream
		// `Err` items, which is what the retry policy classifies.
		let (attempt_error, message_ended, replay_safe, from_stream) = match runner.run(&request) {
			Err(error) => (Some(error.to_string()), false, false, false),
			Ok(events) => {
				let max_time = {
					let guard = session
						.lock()
						.unwrap_or_else(std::sync::PoisonError::into_inner);
					guard.max_time
				};
				let deadline = Deadline::new(max_time);
				let started = prepared.started;
				let mut frames = EventFrames::default();
				let result = deadline::drive(
					events,
					deadline,
					|| started.elapsed(),
					|event| {
						{
							let mut guard = session
								.lock()
								.unwrap_or_else(std::sync::PoisonError::into_inner);
							guard.observe_event(&mut prepared, event);
							frames.emit(event, sink.as_ref(), &meta, &guard);
						}
						Ok(())
					},
				);
				(
					result.err().map(|error| error.to_string()),
					frames.message_ended,
					frames.replay_safe(),
					true,
				)
			},
		};
		let Some(error) = attempt_error else {
			if attempt > 0 {
				sink.send(json!({
					"type": "auto_retry_end",
					"success": true,
					"attempt": attempt,
				}));
			}
			break;
		};
		// Retry only a classified transient provider failure from the stream,
		// while nothing user-visible was emitted: no assistant `message_end`, no
		// non-whitespace delta, no tool frame. The wire has no frame that
		// retracts any of those, so a retry would duplicate them for the same
		// prompt (upstream `#hasReplayUnsafeOutput`). `reset_for_retry` keeps the
		// user entry.
		if auto_retry
			&& from_stream
			&& !message_ended
			&& replay_safe
			&& is_retryable_stream_error(&error)
			&& attempt < MAX_RETRY_ATTEMPTS
			&& !control.aborted()
			&& !control.closed()
		{
			attempt += 1;
			let delay_ms = 1000u64 << (attempt - 1).min(3);
			sink.send(json!({
				"type": "auto_retry_start",
				"attempt": attempt,
				"maxAttempts": MAX_RETRY_ATTEMPTS,
				"delayMs": delay_ms,
				"errorMessage": error,
			}));
			let token = CancellationToken::new();
			{
				let mut guard = session
					.lock()
					.unwrap_or_else(std::sync::PoisonError::into_inner);
				guard.retry_abort = Some(token.clone());
			}
			let mut cancelled = false;
			for _ in 0..delay_ms.div_ceil(50) {
				if token.is_cancelled() || control.aborted() || control.closed() {
					cancelled = true;
					break;
				}
				thread::sleep(Duration::from_millis(50));
			}
			{
				let mut guard = session
					.lock()
					.unwrap_or_else(std::sync::PoisonError::into_inner);
				guard.retry_abort = None;
			}
			if cancelled {
				sink.send(json!({
					"type": "auto_retry_end",
					"success": false,
					"attempt": attempt,
					"finalError": error,
				}));
				outcome.error = Some(error);
				break;
			}
			prepared.reset_for_retry(&input, &request.parts);
			// The finished attempt's stream cancels its own request token when it
			// drops (`StreamHandles::drop`), and that must not read as an abort: the
			// retry gets a fresh token, re-registered so `abort` still reaches it.
			let retry_token = CancellationToken::new();
			request.cancellation_token = Some(retry_token.clone());
			prepared.token = retry_token.clone();
			active.begin(generation, retry_token.clone(), steer.clone());
			if !control.install_token(generation, retry_token) {
				// The run was aborted or superseded during the backoff.
				outcome.aborted = true;
				break;
			}
			continue;
		}
		if attempt > 0 {
			sink.send(json!({
				"type": "auto_retry_end",
				"success": false,
				"attempt": attempt,
				"finalError": error,
			}));
		}
		outcome.error = Some(error);
		break;
	}
	// Keep a local abort from the retry path: a superseded run may be neither
	// aborted nor closed yet must still not compact or continue.
	outcome.aborted = outcome.aborted || control.aborted() || control.closed();
	{
		let mut guard = session
			.lock()
			.unwrap_or_else(std::sync::PoisonError::into_inner);
		guard.finish_turn(&mut prepared, &outcome);
	}
	outcome
}

#[cfg(test)]
mod retry_policy_tests {
	use super::is_retryable_stream_error;

	#[test]
	fn transient_provider_failures_retry() {
		for error in [
			"pi-agent run failed: 503 Service Unavailable",
			"pi-agent run failed: 429 rate limit exceeded",
			"pi-agent run failed: connection reset by peer",
			"pi-agent run failed: stream ended before completion",
		] {
			assert!(is_retryable_stream_error(error), "{error} should retry");
		}
	}

	#[test]
	fn permanent_failures_never_retry() {
		for error in [
			"pi-agent run failed: 401 Unauthorized: invalid api key",
			"pi-agent run failed: 403 Forbidden",
			"pi-agent run failed: 400 invalid_request_error",
			"pi-agent run failed: context length exceeded",
			"pi-agent run failed: model does not support image input",
			crate::deadline::TIMEOUT_MESSAGE,
			"interrupted",
			"pi-agent run failed: something nobody classified",
		] {
			assert!(!is_retryable_stream_error(error), "{error} must not retry");
		}
	}
}

#[cfg(test)]
mod run_control_tests {
	use super::{Boundary, RunControl};
	use crate::rpc::session::TurnInput;
	use pi_agent::CancellationToken;

	fn input(text: &str) -> TurnInput {
		TurnInput { text: text.to_string(), images: Vec::new() }
	}

	#[test]
	fn begin_run_refuses_after_close() {
		let control = RunControl::default();
		control.close();
		assert!(control.begin_run().is_err(), "EOF is sticky: no new run");
		assert!(!control.stream());
	}

	#[test]
	fn abort_cancels_the_installed_token() {
		let control = RunControl::default();
		let generation = control.begin_run().expect("run");
		let token = CancellationToken::new();
		assert!(control.install_token(generation, token.clone()));
		assert!(control.abort());
		assert!(token.is_cancelled(), "the live turn's token is cancelled");
	}

	#[test]
	fn a_token_installed_after_abort_is_cancelled_instead_of_adopted() {
		let control = RunControl::default();
		let generation = control.begin_run().expect("run");
		assert!(control.abort());
		let late = CancellationToken::new();
		assert!(!control.install_token(generation, late.clone()));
		assert!(late.is_cancelled(), "an abort that raced the provider request still cancels it");
	}

	#[test]
	fn begin_run_retires_the_previous_token() {
		let control = RunControl::default();
		let first = control.begin_run().expect("first");
		let token = CancellationToken::new();
		assert!(control.install_token(first, token.clone()));
		let _second = control.begin_run().expect("replacement run");
		assert!(token.is_cancelled(), "a superseded run cannot keep its token");
	}

	#[test]
	fn a_superseded_worker_neither_drains_nor_ends_the_run() {
		let control = RunControl::default();
		let stale = control.begin_run().expect("old run");
		let _current = control.begin_run().expect("new run");
		let mut drained = false;
		let mut emitted = false;
		let mut cleared = false;
		let boundary = control.boundary(
			stale,
			|| {
				drained = true;
				Some(input("queued"))
			},
			|| emitted = true,
			|_| cleared = true,
		);
		assert!(matches!(boundary, Boundary::Superseded));
		assert!(!drained, "a stale worker must not consume the new run's queue");
		assert!(!emitted, "a stale worker must not emit agent_end");
		assert!(!cleared, "a stale worker must not clear the new run's queue");
		assert!(control.stream(), "the new run is still live");
	}

	#[test]
	fn a_replacement_runs_next_and_keeps_the_run_live() {
		let control = RunControl::default();
		let generation = control.begin_run().expect("run");
		let token = CancellationToken::new();
		assert!(control.install_token(generation, token.clone()));
		assert!(control.replace(input("next")).is_ok());
		assert!(token.is_cancelled(), "the replaced turn is cancelled");
		let mut drained = false;
		let mut cleared = false;
		let boundary = control.boundary(
			generation,
			|| {
				drained = true;
				None
			},
			|| {},
			|_| cleared = true,
		);
		assert!(matches!(boundary, Boundary::Replacement(_)), "the replacement wins");
		if let Boundary::Replacement(next) = boundary {
			assert_eq!(next.text, "next");
		}
		assert!(!drained, "a replacement does not wait for the queue drain");
		assert!(cleared, "the aborted run's queue dies with it");
		assert!(control.stream(), "the replacement keeps the run live");
	}

	#[test]
	fn close_outranks_a_pending_replacement() {
		let control = RunControl::default();
		let generation = control.begin_run().expect("run");
		assert!(control.replace(input("next")).is_ok());
		control.close();
		let mut emitted = false;
		let boundary =
			control.boundary(generation, || Some(input("queued")), || emitted = true, |_| {});
		assert!(matches!(boundary, Boundary::End));
		assert!(emitted, "shutdown still ends the run exactly once");
		assert!(!control.stream());
		assert!(control.replace(input("after eof")).is_err(), "nothing may be replaced after EOF");
	}

	#[test]
	fn a_queued_follow_up_cannot_slip_past_the_run_end() {
		let control = RunControl::default();
		let generation = control.begin_run().expect("run");
		let mut queued = Vec::new();
		assert!(
			control
				.admit_follow_up(input("queued"), |input| {
					queued.push(input);
				})
				.is_ok()
		);
		assert!(matches!(
			control.boundary(generation, || queued.pop(), || {}, |_| {}),
			Boundary::Turn(_)
		));
		assert!(matches!(
			control.boundary(generation, || queued.pop(), || {}, |_| {}),
			Boundary::End
		));
		assert!(
			control
				.admit_follow_up(input("late"), |input| {
					queued.push(input);
				})
				.is_err(),
			"after the run ends a command starts a fresh run instead of queueing into a dead worker"
		);
	}

	#[test]
	fn a_follow_up_racing_the_commit_is_decided_after_the_release() {
		use std::sync::{Arc, mpsc};
		use std::time::Duration;

		let control = Arc::new(RunControl::default());
		let generation = control.begin_run().expect("run");
		// The participant is started from inside the commit, so its admission attempt
		// can only run once that lock is released: it must see the finished run and be
		// refused instead of being queued into a worker that already ended.
		let (go_tx, go_rx) = mpsc::channel::<()>();
		let (done_tx, done_rx) = mpsc::channel::<bool>();
		let participant = {
			let control = Arc::clone(&control);
			std::thread::spawn(move || {
				go_rx.recv().expect("commit signalled");
				let mut queue = Vec::new();
				let admitted = control
					.admit_follow_up(input("racing follow-up"), |input| {
						queue.push(input);
					})
					.is_ok();
				done_tx.send(admitted).expect("report admission");
				queue
			})
		};
		let boundary = control.boundary(
			generation,
			|| None,
			|| {},
			|_| {
				go_tx.send(()).expect("start the participant");
			},
		);
		assert!(matches!(boundary, Boundary::End));
		let admitted = done_rx
			.recv_timeout(Duration::from_secs(10))
			.expect("participant finished");
		assert!(!admitted, "a follow-up that races the commit is refused");
		assert!(participant.join().expect("participant thread").is_empty(), "nothing was queued");
		assert!(!control.stream(), "the run is over");
	}

	#[test]
	fn an_old_worker_never_clears_the_new_runs_queue() {
		let control = RunControl::default();
		let stale = control.begin_run().expect("old run");
		let _current = control.begin_run().expect("new run");
		let mut queue = Vec::new();
		assert!(
			control
				.admit_follow_up(input("new run follow-up"), |input| {
					queue.push(input);
				})
				.is_ok()
		);
		let mut cleared = false;
		let boundary = control.boundary(
			stale,
			|| None,
			|| {},
			|_| {
				cleared = true;
				queue.clear();
			},
		);
		assert!(matches!(boundary, Boundary::Superseded));
		assert!(!cleared, "the stale worker must not clear the new run's queue");
		assert_eq!(queue.len(), 1, "the admitted follow-up survives the old worker");
		assert!(control.stream(), "the new run is still live");
	}

	#[test]
	fn a_failed_prompt_does_not_leave_the_run_busy() {
		let control = RunControl::default();
		let failed = control.begin_run().expect("reserve the run");
		// Preparation failed (invalid prompt or image): no worker will ever exist.
		assert!(control.abandon_run(failed));
		assert!(!control.stream(), "no phantom live run blocks the next prompt");
		let next = control.begin_run().expect("the next prompt is admitted");
		assert_ne!(next, failed);
		assert!(control.stream());
	}

	#[test]
	fn abandon_run_never_disturbs_a_newer_generation() {
		let control = RunControl::default();
		let stale = control.begin_run().expect("old run");
		let _current = control.begin_run().expect("new run");
		assert!(!control.abandon_run(stale));
		assert!(control.stream(), "the newer run stays live");
	}

	#[test]
	fn abandon_run_never_resurrects_a_closed_transport() {
		let control = RunControl::default();
		let generation = control.begin_run().expect("run");
		control.close();
		assert!(!control.abandon_run(generation));
		assert!(!control.stream());
		assert!(control.begin_run().is_err());
	}
}
