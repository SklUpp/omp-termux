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
	sync::{
		Arc, Mutex, MutexGuard,
		atomic::{AtomicBool, Ordering},
	},
	thread,
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use pi_agent::CancellationToken;
use serde_json::{json, Value};

use crate::{
	agent::Event as AgentEvent,
	deadline::{self, Deadline},
	session::canonical_stop_reason,
};

use super::{
	frame::{MAX_RPC_FRAME_BYTES, MAX_RPC_REASSEMBLED_BYTES},
	host_tools::{self, HostToolBridge},
	host_uris::{self, HostUriBridge},
	input::{ExtensionUiBridge, Inbound, Output, RpcInput, ShutdownCoordinator},
	messages::{self, MessageSnapshot},
	session::{
		ActiveTurn, FrameSink, Prerequisite, RpcLaunch, RpcSession, SteeringMessage, TurnAdvance,
		TurnInput, TurnOutcome,
	},
	subagents::{self, SubagentRegistry},
	types::{
		InterruptMode, RpcCommand, RpcControlFrame, RpcResponse, StreamingBehavior,
		SubagentSubscriptionLevel,
	},
};

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
	text:      String,
	turn_open: bool,
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
				sink.send(json!({
					"type": "tool_execution_start",
					"toolCallId": id,
					"toolName": tool,
					"args": args,
				}));
			},
			AgentEvent::ToolEnd { id, tool, result, error } => {
				let content = match error {
					Some(error) => format!("{result}\n{error}"),
					None => result.clone(),
				};
				sink.send(json!({
					"type": "tool_execution_end",
					"toolCallId": id,
					"toolName": tool,
					"result": { "content": [{ "type": "text", "text": content }] },
					"isError": error.is_some(),
				}));
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

/// The RPC transport plus the session it drives.
pub struct RpcMode {
	session:  Arc<Mutex<RpcSession>>,
	sink:     Arc<Output>,
	active:   Arc<ActiveTurn>,
	ui:       Arc<ExtensionUiBridge>,
	shutdown: ShutdownCoordinator,
	aborted:  Arc<AtomicBool>,
	registry: SubagentRegistry,
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
		let host_tools = Arc::new(Mutex::new(HostToolBridge::new(Arc::clone(&sink))));
		let host_uris = Arc::new(Mutex::new(HostUriBridge::new(Arc::clone(&sink))));
		let session = RpcSession::open(launch, approvals, host_tools, host_uris)?;
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
			aborted: Arc::new(AtomicBool::new(false)),
			registry: SubagentRegistry::new(),
		};
		mode.event_loop(&mut reader);
		mode.finish();
		match output.finish() {
			Some(error) => Err(crate::Error::new(error)),
			None => Ok(()),
		}
	}

	fn event_loop(&mut self, reader: &mut impl std::io::BufRead) {
		let mut input = RpcInput::new(reader);
		loop {
			match input.next() {
				Ok(None) => return,
				Ok(Some(Err(error))) => {
					// Upstream logs a malformed line and keeps reading.
					eprintln!("warning: {error}");
				},
				Ok(Some(Ok(Inbound::Control(control)))) => self.handle_control(control),
				Ok(Some(Ok(Inbound::Command(value)))) => self.handle_value(value),
			}
		}
	}

	fn handle_control(&mut self, control: RpcControlFrame) {
		match control {
			RpcControlFrame::ExtensionUi(response) => {
				self.ui.handle(response);
			},
			RpcControlFrame::HostToolResult(frame) => {
				let bridge = self.lock_session().host_tools.clone();
				let bridge = bridge.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
				bridge.handle_result(frame);
			},
			RpcControlFrame::HostToolUpdate(frame) => {
				let bridge = self.lock_session().host_tools.clone();
				let bridge = bridge.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
				bridge.handle_update(frame);
			},
			RpcControlFrame::HostUriResult(frame) => {
				let bridge = self.lock_session().host_uris.clone();
				let bridge = bridge.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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

	fn finish(&mut self) {
		self.ui.cancel_all();
		self.aborted.store(true, Ordering::SeqCst);
		self.active.abort();
		self.active.abort_bash();
		{
			let session = self.lock_session();
			session
				.host_tools
				.lock()
				.unwrap_or_else(std::sync::PoisonError::into_inner)
				.close("RPC session closed");
			session
				.host_uris
				.lock()
				.unwrap_or_else(std::sync::PoisonError::into_inner)
				.close("RPC session closed");
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
				self.abort_active();
				Some(ok())
			},
			RpcCommand::AbortAndPrompt { message, images, .. } => {
				let input = TurnInput { text: self.expand_skill(&message), images: images.unwrap_or_default() };
				let was_streaming = self.active.is_streaming();
				self.abort_active();
				if was_streaming {
					self.lock_session().queue_front(input);
					Some(ok())
				} else {
					let response = ok();
					self.start_turn(input, id);
					Some(response)
				}
			},
			RpcCommand::NewSession { parent_session, .. } => {
				if self.active.is_streaming() {
					return Some(err(
						"Cannot start a new session while a response is streaming".to_string(),
					));
				}
				Some(match self.lock_session().new_session(parent_session.as_deref()) {
					Ok(()) => ok_data(json!({ "cancelled": false })),
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
				let bridge = self.lock_session().host_tools.clone();
				Some(match bridge.set_tools(tools) {
					Err(error) => err(error),
					Ok(adapters) => {
						let names: Vec<String> = adapters
							.iter()
							.map(|adapter| adapter.name().to_string())
							.collect();
						let mut response = prereq(host_tools::mount_prerequisite());
						response.error = Some(format!(
							"{} (validated tools: {})",
							response.error.unwrap_or_default(),
							names.join(", ")
						));
						response
					},
				})
			},
			RpcCommand::SetHostUriSchemes { schemes, .. } => {
				let bridge = self.lock_session().host_uris.clone();
				let bridge = bridge.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
				Some(match bridge.set_schemes(&schemes) {
					Err(error) => err(error),
					Ok(schemes) => prereq(host_uris::mount_prerequisite(&schemes)),
				})
			},
			RpcCommand::SetSubagentSubscription { level, .. } => {
				self.lock_session().subagent_level = level;
				self.registry.set_subscription_level(level);
				if level == SubagentSubscriptionLevel::Off {
					Some(ok_data(json!({ "level": "off" })))
				} else {
					Some(prereq(subagents::event_bus_prerequisite()))
				}
			},
			RpcCommand::GetSubagents { .. } => {
				if self.registry.subscription_level() == SubagentSubscriptionLevel::Off {
					return Some(ok_data(json!({ "subagents": [] })));
				}
				Some(prereq(subagents::event_bus_prerequisite()))
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
				if self.active.is_streaming() {
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
				if self.active.is_streaming() {
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
				Some(match self.lock_session().branch(&entry_id) {
					Ok(result) => ok_data(result),
					Err(prerequisite) => prereq(prerequisite),
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
				if self.active.is_streaming() {
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
		if self.active.is_streaming() {
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
		if !self.active.is_streaming() {
			let response = RpcResponse::success(prompt_id.as_deref(), "steer");
			self.start_turn(input, prompt_id);
			return response;
		}
		let message = SteeringMessage { text: input.text.clone(), images: input.images.clone() };
		match self.lock_session().interrupt_mode {
			InterruptMode::Wait => {
				self.lock_session().queue_follow_up(input);
				RpcResponse::success(id_ref, "steer")
			},
			InterruptMode::Immediate => match self.active.steer(&message) {
				Ok(()) => RpcResponse::success(id_ref, "steer"),
				Err(prerequisite) => {
					RpcResponse::error_code(id_ref, "steer", prerequisite.message, prerequisite.code)
				},
			},
		}
	}

	fn follow_up(&mut self, input: TurnInput, prompt_id: Option<String>) -> RpcResponse {
		let id_ref = prompt_id.as_deref();
		if !self.active.is_streaming() {
			let response = RpcResponse::success(prompt_id.as_deref(), "follow_up");
			self.start_turn(input, prompt_id);
			return response;
		}
		self.lock_session().queue_follow_up(input);
		RpcResponse::success(id_ref, "follow_up")
	}

	fn abort_active(&mut self) {
		self.active.abort();
		self.aborted.store(true, Ordering::SeqCst);
		self.ui.cancel_all();
	}

	// ------------------------------------------------------------------ turns --

	fn start_turn(&mut self, first: TurnInput, prompt_id: Option<String>) {
		let session = Arc::clone(&self.session);
		let sink: Arc<dyn FrameSink> = self.sink.clone();
		let active = Arc::clone(&self.active);
		let aborted = Arc::clone(&self.aborted);
		let handle = thread::Builder::new()
			.name("omp-rpc-turn".to_string())
			.spawn(move || {
				let mut next = Some((first, prompt_id));
				sink.send(json!({ "type": "agent_start" }));
				while let Some((input, prompt_id)) = next {
					aborted.store(false, Ordering::SeqCst);
					let outcome =
						run_turn(&session, &sink, &active, &aborted, input, prompt_id.clone());
					if let Some(error) = &outcome.error {
						sink.send(
							RpcResponse::error(prompt_id.as_deref(), "prompt", error.clone())
								.into_value(),
						);
					}
					if outcome.error.is_none() && !outcome.aborted {
						maybe_auto_compact(&session, &sink);
					}
					next = match session
						.lock()
						.unwrap_or_else(std::sync::PoisonError::into_inner)
						.next_queued()
					{
						TurnAdvance::Run(input) => Some((input, None)),
						TurnAdvance::Idle => None,
					};
				}
				// One `agent_end` closes the agent run; queued follow-ups were
				// already drained above, so `willContinue` is false.
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
		if self.active.is_streaming() || self.lock_session().is_compacting {
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
		if self.registry.subscription_level() == SubagentSubscriptionLevel::Off
			&& subagent_id.is_none()
			&& session_file.is_none()
		{
			return RpcResponse::error_code(
				id,
				"get_subagent_messages",
				subagents::event_bus_prerequisite().message,
				subagents::event_bus_prerequisite().code,
			);
		}
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

/// One pumped turn: prepare, stream frames, persist, then announce the end.
fn run_turn(
	session: &Arc<Mutex<RpcSession>>,
	sink: &Arc<dyn FrameSink>,
	active: &Arc<ActiveTurn>,
	aborted: &Arc<AtomicBool>,
	input: TurnInput,
	_prompt_id: Option<String>,
) -> TurnOutcome {
	let prepared = {
		let mut guard = session.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		guard.prepare_turn(&input)
	};
	let (request, mut prepared) = match prepared {
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
	active.begin(prepared.token.clone(), steer);

	let runner = {
		let guard = session.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		Arc::clone(&guard.runner)
	};
	let mut outcome = TurnOutcome::default();
	let mut attempt = 0u32;
	const MAX_RETRY_ATTEMPTS: u32 = 3;
	loop {
		let auto_retry = {
			let guard = session.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
			guard.auto_retry
		};
		match runner.run(&request) {
			Err(error) => {
				// No event was consumed, so the prepared turn is still clean and
				// the same request can be retried without duplicating the user
				// entry.
				if auto_retry
					&& attempt < MAX_RETRY_ATTEMPTS
					&& !aborted.load(Ordering::SeqCst)
				{
					attempt += 1;
					let delay_ms = 1000u64 << (attempt - 1).min(3);
					sink.send(json!({
						"type": "auto_retry_start",
						"attempt": attempt,
						"maxAttempts": MAX_RETRY_ATTEMPTS,
						"delayMs": delay_ms,
						"errorMessage": error.to_string(),
					}));
					let token = CancellationToken::new();
					{
						let mut guard =
							session.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
						guard.retry_abort = Some(token.clone());
					}
					let mut cancelled = false;
					for _ in 0..delay_ms.div_ceil(50) {
						if token.is_cancelled() || aborted.load(Ordering::SeqCst) {
							cancelled = true;
							break;
						}
						thread::sleep(Duration::from_millis(50));
					}
					{
						let mut guard =
							session.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
						guard.retry_abort = None;
					}
					if cancelled {
						sink.send(json!({
							"type": "auto_retry_end",
							"success": false,
							"attempt": attempt,
							"finalError": error.to_string(),
						}));
						outcome.error = Some(error.to_string());
						break;
					}
					continue;
				}
				if attempt > 0 {
					sink.send(json!({
						"type": "auto_retry_end",
						"success": false,
						"attempt": attempt,
						"finalError": error.to_string(),
					}));
				}
				outcome.error = Some(error.to_string());
				break;
			},
			Ok(events) => {
				if attempt > 0 {
					sink.send(json!({
						"type": "auto_retry_end",
						"success": true,
						"attempt": attempt,
					}));
				}
				let max_time = {
					let guard = session.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
							let mut guard =
								session.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
							guard.observe_event(&mut prepared, event);
							frames.emit(event, sink.as_ref(), &meta, &guard);
						}
						Ok(())
					},
				);
				if let Err(error) = result {
					outcome.error = Some(error.to_string());
				}
				break;
			},
		}
	}
	outcome.aborted = aborted.load(Ordering::SeqCst);
	{
		let mut guard = session.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		guard.finish_turn(&mut prepared, &outcome);
	}
	active.end();
	outcome
}
