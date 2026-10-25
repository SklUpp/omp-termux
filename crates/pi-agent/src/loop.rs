use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
	compaction::Compactor,
	provider::{ModelProvider, ProviderError},
	state::{AgentConfig, AgentContext, AgentState, Message, ToolCall, ToolResult, TurnResult},
	tools::{registry::ToolRegistry, scheduler::ToolScheduler},
};

/// Messages that must survive compaction even when over budget.
const MIN_RECENT_MESSAGES: usize = 4;

/// Tool result recorded for calls that were cancelled before producing output.
///
/// OpenAI history requires every `tool_call` to be answered, so an interrupted
/// batch is closed with this marker instead of leaving orphan calls behind.
const INTERRUPTED_TOOL_RESULT: &str = "[Tool execution interrupted by user]";

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

	/// Run turns until the assistant answers without tool calls or the turn budget runs out.
	///
	/// History invariants: every assistant `tool_call` is followed by exactly one
	/// tool message with the same id, and injected user messages are appended
	/// after a completed tool batch, never between an assistant call and its
	/// results.
	pub async fn run_dialog(
		&self,
		user_input: &str,
		state: &mut AgentState,
		mut steering_rx: Option<mpsc::Receiver<SteeringEvent>>,
	) -> Result<Vec<TurnResult>, AgentError> {
		if state.messages.is_empty()
			&& let Some(system) = &self.config.system_prompt
		{
			state.messages.push(Message::system(system.clone()));
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
			let assistant = self.provider.complete(&state.messages, &schemas).await?;
			state.messages.push(assistant.clone());
			state.total_turns += 1;

			if assistant.tool_calls.is_empty() {
				turn_results.push(TurnResult {
					message: assistant,
					tool_results: Vec::new(),
					is_completed: true,
				});
				return Ok(turn_results);
			}

			let outcome = self
				.run_tools(&assistant.tool_calls, &ctx, &cancellation_token, steering_rx.as_mut())
				.await;

			for result in &outcome.results {
				state
					.messages
					.push(Message::tool(&result.tool_call_id, &result.content));
			}
			// Injected messages land after the tool responses, so the history stays valid.
			for message in outcome.injections {
				state.messages.push(Message::user(message));
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
}
