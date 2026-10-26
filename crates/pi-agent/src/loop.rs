use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
	compaction::Compactor,
	provider::{ModelProvider, ProviderError, StreamAssembler},
	state::{
		AgentConfig, AgentContext, AgentState, Message, Role, ToolCall, ToolResult, TurnResult,
		TurnStatus,
	},
	tools::{registry::ToolRegistry, scheduler::ToolScheduler},
};

/// Messages that must survive compaction even when over budget.
const MIN_RECENT_MESSAGES: usize = 4;

/// Tool result recorded for calls that were cancelled before producing output.
///
/// OpenAI history requires every `tool_call` to be answered, so an interrupted
/// batch is closed with this marker instead of leaving orphan calls behind.
pub const INTERRUPTED_TOOL_RESULT: &str = "[Tool execution interrupted by user]";

/// Live event emitted while a dialog runs.
///
/// Only assistant text is streamed: tool calls are delivered finalized, in the
/// `TurnResult` that closes the turn. Consumers accumulate `delta` themselves,
/// so no O(n²) full-text clones travel over the channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
	/// Incremental assistant text, exactly as the provider streamed it.
	MessageUpdate { delta: String },
	/// Provider finalization (assembler.finish()), BEFORE tool batch runs. Once per assistant message.
	MessageEnd { message: Message, status: TurnStatus },
	/// Tool invocation started before batch execution.
	ToolStart { id: String, name: String, arguments: serde_json::Value },
	/// Tool invocation finished with result or error.
	ToolEnd { id: String, name: String, result: String, is_error: bool },
	/// After tool results resolved (or immediately when tool_calls empty / on abort). Once per assistant message.
	TurnEnd { message: Message, tool_results: Vec<ToolResult>, status: TurnStatus },
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
	#[error("Max turns exceeded: {0}")]
	MaxTurnsExceeded(usize),
	#[error("Execution interrupted by steering")]
	Interrupted,
	#[error("Model provider error: {0}")]
	Provider(#[from] ProviderError),
}

/// Control event to steer or interrupt a running dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteeringEvent {
	Interrupt,
	InjectMessage(String),
}

/// Events drained from the steering channel at one point in time.
#[derive(Default)]
struct Steering {
	injections: Vec<String>,
	interrupted: bool,
}

impl Steering {
	fn absorb(&mut self, event: SteeringEvent) {
		match event {
			SteeringEvent::Interrupt => self.interrupted = true,
			SteeringEvent::InjectMessage(message) => self.injections.push(message),
		}
	}

	/// Drain whatever is already queued, without awaiting new events.
	fn drain(rx: &mut mpsc::Receiver<SteeringEvent>) -> Self {
		let mut steering = Self::default();
		while let Ok(event) = rx.try_recv() {
			steering.absorb(event);
		}
		steering
	}
}

/// Outcome of one tool batch.
struct BatchOutcome {
	results: Vec<ToolResult>,
	injections: Vec<String>,
	interrupted: bool,
}

/// Agent execution engine: provider turn, tool batch, repeat.
pub struct AgentLoop {
	config: AgentConfig,
	provider: Arc<dyn ModelProvider>,
	scheduler: ToolScheduler,
	registry: ToolRegistry,
}

impl AgentLoop {
	pub fn new(
		config: AgentConfig,
		provider: Arc<dyn ModelProvider>,
		registry: ToolRegistry,
	) -> Self {
		let scheduler = ToolScheduler::new(registry.clone());
		Self { config, provider, scheduler, registry }
	}

	/// Run turns without emitting live events.
	pub async fn run_dialog(
		&self,
		user_input: &str,
		state: &mut AgentState,
		steering_rx: Option<mpsc::Receiver<SteeringEvent>>,
	) -> Result<Vec<TurnResult>, AgentError> {
		self
			.run_dialog_with(user_input, state, steering_rx, None)
			.await
	}

	/// Run turns until the assistant answers without tool calls or the turn budget runs out.
	///
	/// With `events` set, the assistant turn is requested as a provider stream and
	/// every text delta is forwarded as [`AgentEvent::MessageUpdate`] the moment
	/// it arrives; tool calls still arrive only in the returned [`TurnResult`].
	/// The channel is bounded by the caller, so a slow consumer applies
	/// backpressure instead of growing memory. The consumer must therefore
	/// drain the channel concurrently (or size it for a full turn): a stalled
	/// consumer blocks the turn.
	///
	/// History invariants: every assistant `tool_call` is followed by exactly one
	/// tool message with the same id, and injected user messages are appended
	/// after a completed tool batch, never between an assistant call and its
	/// results.
	pub async fn run_dialog_with(
		&self,
		user_input: &str,
		state: &mut AgentState,
		mut steering_rx: Option<mpsc::Receiver<SteeringEvent>>,
		events: Option<mpsc::Sender<AgentEvent>>,
	) -> Result<Vec<TurnResult>, AgentError> {
		// The configured prompt belongs at the head of the history; resumed
		// sessions may already carry messages, so test for the role, not emptiness.
		if let Some(system) = &self.config.system_prompt
			&& state
				.messages
				.first()
				.is_none_or(|message| message.role != Role::System)
		{
			state.messages.insert(0, Message::system(system.clone()));
		}
		state.messages.push(Message::user(user_input));

		let cancellation_token = CancellationToken::new();
		let ctx =
			AgentContext::with_cancellation(self.config.cwd.clone(), cancellation_token.clone());
		let mut turn_results = Vec::new();

		while state.total_turns < self.config.max_turns {
			if let Some(rx) = &mut steering_rx {
				let steering = Steering::drain(rx);
				if steering.interrupted {
					cancellation_token.cancel();
					return Err(AgentError::Interrupted);
				}
				for message in steering.injections {
					state.messages.push(Message::user(message));
				}
			}

			state.messages =
				Compactor::compact(&state.messages, self.config.context_limit, MIN_RECENT_MESSAGES);

			let schemas = self.registry.schemas();
			let mut pending_injections = Vec::new();
			let assistant = self
				.next_assistant(
					&state.messages,
					&schemas,
					events.as_ref(),
					&cancellation_token,
					steering_rx.as_mut(),
					&mut pending_injections,
				)
				.await?;
			state.messages.push(assistant.clone());
			state.total_turns += 1;
			if let Some(events_sink) = events.as_ref() {
				let status = if assistant.tool_calls.is_empty() {
					TurnStatus::EndTurn
				} else {
					TurnStatus::ToolUse
				};
				let _ = events_sink
					.send(AgentEvent::MessageEnd { message: assistant.clone(), status })
					.await;
			}

			if assistant.tool_calls.is_empty() {
				for message in pending_injections {
					state.messages.push(Message::user(message));
				}
				if let Some(events_sink) = events.as_ref() {
					let _ = events_sink
						.send(AgentEvent::TurnEnd {
							message: assistant.clone(),
							tool_results: Vec::new(),
							status: TurnStatus::EndTurn,
						})
						.await;
				}
				turn_results.push(TurnResult {
					message: assistant,
					tool_results: Vec::new(),
					is_completed: true,
				});
				return Ok(turn_results);
			}

			if let Some(events_sink) = events.as_ref() {
				for call in &assistant.tool_calls {
					let _ = events_sink
						.send(AgentEvent::ToolStart {
							id: call.id.clone(),
							name: call.name.clone(),
							arguments: call.arguments.clone(),
						})
						.await;
				}
			}

			let outcome = self
				.run_tools(&assistant.tool_calls, &ctx, &cancellation_token, steering_rx.as_mut())
				.await;

			if let Some(events_sink) = events.as_ref() {
				let tool_names: std::collections::HashMap<&str, &str> = assistant
					.tool_calls
					.iter()
					.map(|c| (c.id.as_str(), c.name.as_str()))
					.collect();
				for result in &outcome.results {
					let name = tool_names
						.get(result.tool_call_id.as_str())
						.copied()
						.unwrap_or("tool");
					let _ = events_sink
						.send(AgentEvent::ToolEnd {
							id: result.tool_call_id.clone(),
							name: name.to_string(),
							result: result.content.clone(),
							is_error: result.is_error,
						})
						.await;
				}
			}

			let tool_names: std::collections::HashMap<&str, &str> = assistant
				.tool_calls
				.iter()
				.map(|c| (c.id.as_str(), c.name.as_str()))
				.collect();
			for result in &outcome.results {
				let name = tool_names.get(result.tool_call_id.as_str()).copied().unwrap_or("tool");
				state
					.messages
					.push(Message::tool_with_status(&result.tool_call_id, name, &result.content, result.is_error));
			}
			// Injected messages land after the tool responses, so the history stays valid.
			for message in pending_injections.into_iter().chain(outcome.injections) {
				state.messages.push(Message::user(message));
			}

			let status = if outcome.interrupted {
				TurnStatus::Aborted
			} else {
				TurnStatus::ToolUse
			};

			if let Some(events_sink) = events.as_ref() {
				let _ = events_sink
					.send(AgentEvent::TurnEnd {
						message: assistant.clone(),
						tool_results: outcome.results.clone(),
						status,
					})
					.await;
			}

			turn_results.push(TurnResult {
				message: assistant,
				tool_results: outcome.results,
				is_completed: false,
			});


			if outcome.interrupted {
				cancellation_token.cancel();
				return Err(AgentError::Interrupted);
			}
		}

		Err(AgentError::MaxTurnsExceeded(self.config.max_turns))
	}

	/// Execute one tool batch, honouring steering while it runs.
	///
	/// Results come back in call order, one per call: a call that produced no
	/// result (interrupted batch) gets a synthetic one so history never has an
	/// orphan `tool_call`.
	async fn run_tools(
		&self,
		calls: &[ToolCall],
		ctx: &AgentContext,
		cancellation_token: &CancellationToken,
		mut steering_rx: Option<&mut mpsc::Receiver<SteeringEvent>>,
	) -> BatchOutcome {
		let execute = self.scheduler.execute_batch(calls, ctx);
		tokio::pin!(execute);

		let mut outcome =
			BatchOutcome { results: Vec::new(), injections: Vec::new(), interrupted: false };
		let mut finished = false;
		let mut rx_open = steering_rx.is_some();

		while !finished {
			match steering_rx.as_deref_mut() {
				Some(rx) if rx_open => {
					tokio::select! {
						batch = execute.as_mut() => {
							outcome.results = batch;
							finished = true;
						},
						event = rx.recv() => match event {
							Some(SteeringEvent::InjectMessage(message)) => outcome.injections.push(message),
							Some(SteeringEvent::Interrupt) => outcome.interrupted = true,
							None => rx_open = false,
						},
					}
				},
				_ => {
					outcome.results = execute.as_mut().await;
					finished = true;
				},
			}

			if outcome.interrupted && !finished {
				// Abort in-flight work, then collect whatever the tools reported.
				cancellation_token.cancel();
				outcome.results = execute.as_mut().await;
				finished = true;
			}
		}

		let mut ordered = Vec::with_capacity(calls.len());
		for call in calls {
			match outcome
				.results
				.iter()
				.find(|result| result.tool_call_id == call.id)
			{
				Some(result) => ordered.push(result.clone()),
				None => ordered.push(ToolResult {
					tool_call_id: call.id.clone(),
					content: INTERRUPTED_TOOL_RESULT.to_string(),
					is_error: true,
				}),
			}
		}
		outcome.results = ordered;
		outcome
	}

	/// Ask the provider for one assistant turn.
	///
	/// With an event sink the turn is streamed, so text reaches the consumer
	/// before the turn finishes; without one it is a single completion request
	/// and nothing but the final message is produced.
	async fn next_assistant(
		&self,
		messages: &[Message],
		schemas: &[serde_json::Value],
		events: Option<&mpsc::Sender<AgentEvent>>,
		cancellation_token: &CancellationToken,
		mut steering_rx: Option<&mut mpsc::Receiver<SteeringEvent>>,
		injections: &mut Vec<String>,
	) -> Result<Message, AgentError> {
		let Some(events) = events else {
			let complete_fut = self.provider.complete(messages, schemas);
			tokio::pin!(complete_fut);
			loop {
				match steering_rx.as_deref_mut() {
					Some(rx) => {
						tokio::select! {
							_ = cancellation_token.cancelled() => return Err(AgentError::Interrupted),
							event = rx.recv() => match event {
								Some(SteeringEvent::Interrupt) => {
									cancellation_token.cancel();
									return Err(AgentError::Interrupted);
								}
								Some(SteeringEvent::InjectMessage(msg)) => {
									injections.push(msg);
								}
								None => {
									tokio::select! {
										_ = cancellation_token.cancelled() => return Err(AgentError::Interrupted),
										res = &mut complete_fut => return Ok(res?),
									}
								}
							},
							res = &mut complete_fut => return Ok(res?),
						}
					}
					None => {
						tokio::select! {
							_ = cancellation_token.cancelled() => return Err(AgentError::Interrupted),
							res = &mut complete_fut => return Ok(res?),
						}
					}
				}
			}
		};

		let stream_fut = self.provider.stream(messages, schemas);
		tokio::pin!(stream_fut);
		let mut stream = loop {
			match steering_rx.as_deref_mut() {
				Some(rx) => {
					tokio::select! {
						_ = cancellation_token.cancelled() => {
							let partial = Message::assistant("");
							let _ = events.send(AgentEvent::MessageEnd { message: partial.clone(), status: TurnStatus::Aborted }).await;
							let _ = events.send(AgentEvent::TurnEnd {
								message: partial,
								tool_results: Vec::new(),
								status: TurnStatus::Aborted,
							}).await;
							return Err(AgentError::Interrupted);
						},
						event = rx.recv() => match event {
							Some(SteeringEvent::Interrupt) => {
								cancellation_token.cancel();
								let partial = Message::assistant("");
								let _ = events.send(AgentEvent::MessageEnd { message: partial.clone(), status: TurnStatus::Aborted }).await;
								let _ = events.send(AgentEvent::TurnEnd {
									message: partial,
									tool_results: Vec::new(),
									status: TurnStatus::Aborted,
								}).await;
								return Err(AgentError::Interrupted);
							}
							Some(SteeringEvent::InjectMessage(msg)) => {
								injections.push(msg);
							}
							None => {
								tokio::select! {
									_ = cancellation_token.cancelled() => return Err(AgentError::Interrupted),
									res = &mut stream_fut => break res?,
								}
							}
						},
						res = &mut stream_fut => break res?,
					}
				}
				None => {
					tokio::select! {
						_ = cancellation_token.cancelled() => {
							let partial = Message::assistant("");
							let _ = events.send(AgentEvent::MessageEnd { message: partial.clone(), status: TurnStatus::Aborted }).await;
							let _ = events.send(AgentEvent::TurnEnd {
								message: partial,
								tool_results: Vec::new(),
								status: TurnStatus::Aborted,
							}).await;
							return Err(AgentError::Interrupted);
						},
						res = &mut stream_fut => break res?,
					}
				}
			}
		};

		let mut assembler = StreamAssembler::new();
		let mut done = false;
		let mut rx_open = steering_rx.is_some();

		while !done {
			let next_chunk = match steering_rx.as_deref_mut() {
				Some(rx) if rx_open => {
					tokio::select! {
						_ = cancellation_token.cancelled() => {
							let partial = assembler.finish_partial();
							let _ = events.send(AgentEvent::MessageEnd { message: partial.clone(), status: TurnStatus::Aborted }).await;
							let _ = events.send(AgentEvent::TurnEnd {
								message: partial,
								tool_results: Vec::new(),
								status: TurnStatus::Aborted,
							}).await;
							return Err(AgentError::Interrupted);
						},
						event = rx.recv() => match event {
							Some(SteeringEvent::Interrupt) => {
								cancellation_token.cancel();
								let partial = assembler.finish_partial();
								let _ = events.send(AgentEvent::MessageEnd { message: partial.clone(), status: TurnStatus::Aborted }).await;
								let _ = events.send(AgentEvent::TurnEnd {
									message: partial,
									tool_results: Vec::new(),
									status: TurnStatus::Aborted,
								}).await;
								return Err(AgentError::Interrupted);
							}
							Some(SteeringEvent::InjectMessage(msg)) => {
								injections.push(msg);
								continue;
							}
							None => {
								rx_open = false;
								continue;
							}
						},
						chunk = stream.next() => chunk,
					}
				}
				_ => {
					tokio::select! {
						_ = cancellation_token.cancelled() => {
							let partial = assembler.finish_partial();
							let _ = events.send(AgentEvent::MessageEnd { message: partial.clone(), status: TurnStatus::Aborted }).await;
							let _ = events.send(AgentEvent::TurnEnd {
								message: partial,
								tool_results: Vec::new(),
								status: TurnStatus::Aborted,
							}).await;
							return Err(AgentError::Interrupted);
						},
						chunk = stream.next() => chunk,
					}
				}
			};

			let Some(chunk) = next_chunk else {
				break;
			};
			let chunk = match chunk {
				Ok(c) => c,
				Err(err) => {
					let partial = assembler.finish_partial();
					let _ = events.send(AgentEvent::MessageEnd { message: partial.clone(), status: TurnStatus::Error(err.to_string()) }).await;
					let _ = events.send(AgentEvent::TurnEnd {
						message: partial,
						tool_results: Vec::new(),
						status: TurnStatus::Error(err.to_string()),
					}).await;
					return Err(err.into());
				}
			};
			if let Some(delta) = &chunk.content
				&& !delta.is_empty()
			{
				let _ = events
					.send(AgentEvent::MessageUpdate { delta: delta.clone() })
					.await;
			}
			done = chunk.is_done;
			assembler.push(&chunk);
			if done {
				break;
			}
		}

		if !done {
			let partial = assembler.finish_partial();
			let err_msg = "stream ended before completion".to_string();
			let _ = events.send(AgentEvent::MessageEnd { message: partial.clone(), status: TurnStatus::Error(err_msg.clone()) }).await;
			let _ = events.send(AgentEvent::TurnEnd {
				message: partial,
				tool_results: Vec::new(),
				status: TurnStatus::Error(err_msg.clone()),
			}).await;
			return Err(ProviderError::Protocol(err_msg).into());
		}
		let partial = assembler.finish_partial();
		let message = match assembler.finish() {
			Ok(message) => message,
			Err(err) => {
				let _ = events.send(AgentEvent::MessageEnd { message: partial.clone(), status: TurnStatus::Error(err.to_string()) }).await;
				let _ = events.send(AgentEvent::TurnEnd {
					message: partial,
					tool_results: Vec::new(),
					status: TurnStatus::Error(err.to_string()),
				}).await;
				return Err(err.into());
			}
		};
		Ok(message)
	}
}
