//! `pi-agent` backed runner: OpenAI-compatible provider plus builtin tools.
//!
//! The CLI resolves data (config, catalog, prompts, skills) into
//! [`RunRequest`]; this module maps it onto `pi-agent`'s `AgentConfig` and
//! converts the loop's turn results back into the CLI [`Event`] stream.

use std::{
	path::Path,
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
};

use async_trait::async_trait;
use pi_agent::{
	AgentConfig, AgentError, AgentEvent, AgentLoop, AgentState, BashTool, EditTool, GlobTool,
	GrepTool, ModelProvider, OpenAiCompatibleProvider, ReadTool, SteeringEvent, TodoTool, Tool,
	ToolRegistry, WriteTool,
	mcp::{
		MCPContent, MCPServerConfig, MCPStdioServerConfig, McpConnection, McpToolOriginSource,
		connect_to_server, create_mcp_tool_name, get_mcp_tool_origin_key,
		sanitize_mcp_tool_name_part,
	},
};

use crate::{
	Error, Result,
	agent::{AgentRunner, Event, EventStream, RunRequest},
};

/// Assistant-turn budget the CLI hands to the agent loop.
pub const DEFAULT_MAX_TURNS: usize = 32;
/// Context window assumed when the catalog declares none (matches `pi-agent`).
pub const DEFAULT_CONTEXT_LIMIT: usize = 32_768;

/// Builtin tools: name and whether the tool only reads.
///
/// `pi-agent` has no approval hook, so the registry *is* the approval gate:
/// a tool is offered to the model only when the approval matrix would
/// auto-approve it.
const BUILTIN_TOOLS: [(&str, bool); 7] = [
	("read", true),
	("glob", true),
	("grep", true),
	("write", false),
	("edit", false),
	("bash", false),
	("todo", false),
];

/// Synchronous `AgentRunner` that drives `pi-agent` on its own tokio runtime.
pub struct PiAgentRunner {
	runtime:    tokio::runtime::Runtime,
	dispatcher: Arc<SignalDispatcher>,
}

impl PiAgentRunner {
	pub fn new() -> Result<Self> {
		let runtime = tokio::runtime::Builder::new_multi_thread()
			.enable_all()
			.build()
			.map_err(|err| Error::with_source("cannot start tokio runtime for pi-agent", err))?;
		let dispatcher = Arc::new(SignalDispatcher::start(&runtime));
		Ok(Self { runtime, dispatcher })
	}
}

struct SignalDispatcher {
	active_turn: Arc<std::sync::Mutex<Option<tokio::sync::mpsc::Sender<SteeringEvent>>>>,
	_handle:     tokio::task::JoinHandle<()>,
}

struct TurnSignalRegistration {
	active_turn: Arc<std::sync::Mutex<Option<tokio::sync::mpsc::Sender<SteeringEvent>>>>,
}

impl Drop for TurnSignalRegistration {
	fn drop(&mut self) {
		if let Ok(mut guard) = self.active_turn.lock() {
			*guard = None;
		}
	}
}

impl SignalDispatcher {
	fn start(runtime: &tokio::runtime::Runtime) -> Self {
		let active_turn: Arc<std::sync::Mutex<Option<tokio::sync::mpsc::Sender<SteeringEvent>>>> =
			Arc::new(std::sync::Mutex::new(None));
		let active_turn_clone = Arc::clone(&active_turn);
		let _handle = runtime.spawn(async move {
			loop {
				if tokio::signal::ctrl_c().await.is_err() {
					break;
				}
				let maybe_tx = {
					if let Ok(guard) = active_turn_clone.lock() {
						guard.clone()
					} else {
						None
					}
				};
				if let Some(tx) = maybe_tx {
					let _ = tx.send(SteeringEvent::Interrupt).await;
				} else {
					std::process::exit(130);
				}
			}
		});
		Self { active_turn, _handle }
	}

	fn register(&self, tx: tokio::sync::mpsc::Sender<SteeringEvent>) -> TurnSignalRegistration {
		if let Ok(mut guard) = self.active_turn.lock() {
			*guard = Some(tx);
		}
		TurnSignalRegistration { active_turn: Arc::clone(&self.active_turn) }
	}
}
struct TimerGuard(Option<tokio::task::JoinHandle<()>>);

impl Drop for TimerGuard {
	fn drop(&mut self) {
		if let Some(handle) = &self.0 {
			handle.abort();
		}
	}
}

impl TimerGuard {
	fn disarm(&mut self) -> Option<tokio::task::JoinHandle<()>> {
		self.0.take()
	}
}

impl AgentRunner for PiAgentRunner {
	fn run(&self, request: &RunRequest) -> Result<EventStream> {
		let cancellation_token = request.cancellation_token.clone().unwrap_or_default();
		let timed_out = Arc::new(AtomicBool::new(false));

		let (discovery_steering_tx, mut discovery_steering_rx) = tokio::sync::mpsc::channel(4);
		let discovery_sig_reg = self.dispatcher.register(discovery_steering_tx);
		let cancel_on_sig = cancellation_token.clone();
		let sig_watcher = self.runtime.spawn(async move {
			if let Some(SteeringEvent::Interrupt) = discovery_steering_rx.recv().await {
				cancel_on_sig.cancel();
			}
		});

		let timeout_cancel = cancellation_token.clone();
		let timed_out_clone = Arc::clone(&timed_out);
		let mut timer_guard = TimerGuard(request.max_time.map(|budget| {
			self.runtime.spawn(async move {
				tokio::time::sleep(budget).await;
				timed_out_clone.store(true, Ordering::SeqCst);
				timeout_cancel.cancel();
			})
		}));

		let mcp_tools = if request.tools_enabled {
			match self.runtime.block_on(load_and_filter_mcp_tools(
				&request.cwd,
				&request.config,
				&request.approvals,
				&cancellation_token,
			)) {
				Ok(tools) => tools,
				Err(err) => {
					drop(discovery_sig_reg);
					sig_watcher.abort();
					return Err(err);
				}
			}
		} else {
			Vec::new()
		};

		drop(discovery_sig_reg);
		sig_watcher.abort();

		if cancellation_token.is_cancelled() {
			if timed_out.load(Ordering::SeqCst) {
				return Err(Error::new("execution timed out during MCP discovery"));
			}
			return Err(Error::new("MCP discovery interrupted"));
		}

		let plan = RunPlan::resolve_with_mcp(request, !mcp_tools.is_empty())?;
		let registry = build_registry_with_mcp(&plan.tools, mcp_tools)?;
		let cwd = request.cwd.clone();
		let prompt = request.prompt.clone();
		let mut messages = history_messages(&request.history)?;
		if let Some(system) = plan.system_prompt {
			messages.insert(0, pi_agent::Message::system(system));
		}
		let plan_api = plan.api.clone();
		let plan_provider = plan.provider.clone();
		let plan_model = plan.model.clone();
		let provider = OpenAiCompatibleProvider::new(plan.base_url, plan.api_key, plan.model)
			.map_err(|err| Error::with_source("pi-agent provider setup failed", err))?
			.with_effort(plan.effort)
			.with_images(plan.supports_images);
		let config = AgentConfig {
			max_turns: plan.max_turns,
			context_limit: plan.context_limit,
			system_prompt: None,
			cwd,
		};
		let provider: Arc<dyn ModelProvider> = Arc::new(provider);
		let agent = AgentLoop::with_approver(
			config,
			provider,
			registry,
			Arc::new(request.approvals.clone()),
		);
		let mut state = AgentState { messages, total_turns: 0 };

		let (event_tx, event_rx) = tokio::sync::mpsc::channel::<Result<Event>>(64);
		let (pi_events_tx, mut pi_events_rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
		let (steering_tx, steering_rx) = tokio::sync::mpsc::channel::<SteeringEvent>(8);
		let completed = Arc::new(AtomicBool::new(false));
		let completed_clone = Arc::clone(&completed);

		let _guard = self.runtime.enter();

		let cancel_watcher_token = cancellation_token.clone();
		let steering_tx_cancel = steering_tx.clone();
		let cancel_watcher = self.runtime.spawn(async move {
			cancel_watcher_token.cancelled().await;
			let _ = steering_tx_cancel.send(SteeringEvent::Interrupt).await;
		});

		let signal_reg = self.dispatcher.register(steering_tx.clone());

		let dialog_prompt = prompt.clone();
		let dialog_handle = self.runtime.spawn(async move {
			agent
				.run_dialog_with(&dialog_prompt, &mut state, Some(steering_rx), Some(pi_events_tx))
				.await
		});
		let dialog_abort = dialog_handle.abort_handle();
		let dialog_abort_clone = dialog_abort.clone();

		let event_tx_clone = event_tx.clone();
		let cancel_on_err = cancellation_token.clone();
		let steering_tx_err = steering_tx.clone();
		self.runtime.spawn(async move {
			let mut message_started = false;
			let abort_after_grace = |dialog_abort: tokio::task::AbortHandle| {
				tokio::spawn(async move {
					tokio::time::sleep(std::time::Duration::from_millis(500)).await;
					dialog_abort.abort();
				});
			};

			while let Some(pi_event) = pi_events_rx.recv().await {
				match pi_event {
					AgentEvent::MessageUpdate { delta } => {
						if !message_started {
							if event_tx_clone
								.send(Ok(Event::MessageStart { role: "assistant".to_string() }))
								.await
								.is_err()
							{
								cancel_on_err.cancel();
								let _ = steering_tx_err.try_send(SteeringEvent::Interrupt);
								abort_after_grace(dialog_abort_clone);
								return;
							}
							message_started = true;
						}
						if event_tx_clone
							.send(Ok(Event::MessageUpdate { delta }))
							.await
							.is_err()
						{
							cancel_on_err.cancel();
							let _ = steering_tx_err.try_send(SteeringEvent::Interrupt);
							abort_after_grace(dialog_abort_clone);
							return;
						}
					},
					AgentEvent::MessageEnd { message, status } => {
						if !message_started {
							if event_tx_clone
								.send(Ok(Event::MessageStart { role: "assistant".to_string() }))
								.await
								.is_err()
							{
								cancel_on_err.cancel();
								let _ = steering_tx_err.try_send(SteeringEvent::Interrupt);
								abort_after_grace(dialog_abort_clone);
								return;
							}
						}
						message_started = false;
						let stop_reason = match status {
							pi_agent::TurnStatus::EndTurn => "stop",
							pi_agent::TurnStatus::ToolUse => "toolUse",
							pi_agent::TurnStatus::Aborted => "aborted",
							pi_agent::TurnStatus::Error(_) => "error",
						}
						.to_string();
						if event_tx_clone
							.send(Ok(Event::MessageEnd {
								message,
								stop_reason: Some(stop_reason),
								api: Some(plan_api.clone()),
								provider: Some(plan_provider.clone()),
								model: Some(plan_model.clone()),
							}))
							.await
							.is_err()
						{
							cancel_on_err.cancel();
							let _ = steering_tx_err.try_send(SteeringEvent::Interrupt);
							abort_after_grace(dialog_abort_clone);
							return;
						}
					},
					AgentEvent::ToolStart { id, name, arguments } => {
						if event_tx_clone
							.send(Ok(Event::ToolStart { id, tool: name, args: arguments }))
							.await
							.is_err()
						{
							cancel_on_err.cancel();
							let _ = steering_tx_err.try_send(SteeringEvent::Interrupt);
							abort_after_grace(dialog_abort_clone);
							return;
						}
					},
					AgentEvent::ToolEnd { id, name, result, is_error } => {
						let error = is_error.then(|| result.clone());
						if event_tx_clone
							.send(Ok(Event::ToolEnd { id, tool: name, result, error }))
							.await
							.is_err()
						{
							cancel_on_err.cancel();
							let _ = steering_tx_err.try_send(SteeringEvent::Interrupt);
							abort_after_grace(dialog_abort_clone);
							return;
						}
					},
					AgentEvent::TurnEnd { message, tool_results, status } => {
						let stop_reason = match status {
							pi_agent::TurnStatus::EndTurn => "stop",
							pi_agent::TurnStatus::ToolUse => "toolUse",
							pi_agent::TurnStatus::Aborted => "aborted",
							pi_agent::TurnStatus::Error(_) => "error",
						}
						.to_string();
						if event_tx_clone
							.send(Ok(Event::TurnEnd {
								message,
								tool_results,
								stop_reason,
								api: Some(plan_api.clone()),
								provider: Some(plan_provider.clone()),
								model: Some(plan_model.clone()),
							}))
							.await
							.is_err()
						{
							cancel_on_err.cancel();
							let _ = steering_tx_err.try_send(SteeringEvent::Interrupt);
							abort_after_grace(dialog_abort_clone);
							return;
						}
					},
				}
			}

			let dialog_result = match dialog_handle.await {
				Ok(res) => res,
				Err(join_err) => {
					completed_clone.store(true, Ordering::SeqCst);
					let _ = event_tx_clone
						.send(Err(Error::with_source("dialog task panicked", join_err)))
						.await;
					return;
				},
			};

			match dialog_result {
				Ok(turns) => {
					let mut new_messages = vec![pi_agent::Message::user(prompt.clone())];
					for turn in turns {
						let tool_names: std::collections::HashMap<String, String> = turn
							.message
							.tool_calls
							.iter()
							.map(|c| (c.id.clone(), c.name.clone()))
							.collect();
						new_messages.push(turn.message);
						for result in turn.tool_results {
							let name = tool_names
								.get(&result.tool_call_id)
								.cloned()
								.unwrap_or_else(|| "tool".to_string());
							new_messages.push(pi_agent::Message::tool_with_status(
								result.tool_call_id,
								name,
								result.content,
								result.is_error,
							));
						}
					}
					let _ = event_tx_clone
						.send(Ok(Event::AgentEnd {
							messages: new_messages,
							api:      Some(plan_api),
							provider: Some(plan_provider),
							model:    Some(plan_model),
						}))
						.await;
					completed_clone.store(true, Ordering::SeqCst);
				},
				Err(AgentError::Interrupted) => {
					completed_clone.store(true, Ordering::SeqCst);
					if timed_out.load(Ordering::SeqCst) {
						let _ = event_tx_clone
							.send(Err(Error::new(crate::deadline::TIMEOUT_MESSAGE)))
							.await;
					} else {
						let _ = event_tx_clone.send(Err(Error::new("interrupted"))).await;
					}
				},
				Err(err) => {
					completed_clone.store(true, Ordering::SeqCst);
					let _ = event_tx_clone
						.send(Err(Error::with_source("pi-agent run failed", err)))
						.await;
				},
			}
		});

		let runtime_handle = self.runtime.handle().clone();
		let handles = Arc::new(StreamHandles {
			cancellation_token,
			steering_tx,
			cancel_watcher,
			dialog_abort,
			timer_handle: timer_guard.disarm(),
			_signal_reg: signal_reg,
			runtime_handle,
		});

		Ok(Box::new(BoundedEventStream {
			rx: Arc::new(std::sync::Mutex::new(event_rx)),
			completed,
			yielded_closure_error: false,
			_handles: handles,
		}))
	}
}

struct StreamHandles {
	cancellation_token: pi_agent::CancellationToken,
	steering_tx:        tokio::sync::mpsc::Sender<SteeringEvent>,
	cancel_watcher:     tokio::task::JoinHandle<()>,
	dialog_abort:       tokio::task::AbortHandle,
	timer_handle:       Option<tokio::task::JoinHandle<()>>,
	_signal_reg:        TurnSignalRegistration,
	runtime_handle:     tokio::runtime::Handle,
}

impl Drop for StreamHandles {
	fn drop(&mut self) {
		self.cancellation_token.cancel();
		let _ = self.steering_tx.try_send(SteeringEvent::Interrupt);
		let dialog_abort = self.dialog_abort.clone();
		self.runtime_handle.spawn(async move {
			tokio::time::sleep(std::time::Duration::from_millis(500)).await;
			dialog_abort.abort();
		});
		self.cancel_watcher.abort();
		if let Some(handle) = &self.timer_handle {
			handle.abort();
		}
	}
}

struct BoundedEventStream {
	rx:                    Arc<std::sync::Mutex<tokio::sync::mpsc::Receiver<Result<Event>>>>,
	completed:             Arc<AtomicBool>,
	yielded_closure_error: bool,
	_handles:              Arc<StreamHandles>,
}

impl Iterator for BoundedEventStream {
	type Item = Result<Event>;

	fn next(&mut self) -> Option<Self::Item> {
		let res = if let Ok(handle) = tokio::runtime::Handle::try_current() {
			match handle.runtime_flavor() {
				tokio::runtime::RuntimeFlavor::MultiThread => {
					tokio::task::block_in_place(|| self.rx.lock().expect("rx lock").blocking_recv())
				},
				_ => {
					let rx = Arc::clone(&self.rx);
					std::thread::spawn(move || rx.lock().expect("rx lock").blocking_recv())
						.join()
						.expect("join")
				},
			}
		} else {
			self.rx.lock().expect("rx lock").blocking_recv()
		};
		match res {
			Some(item) => Some(item),
			None => {
				if !self.completed.load(Ordering::SeqCst) && !self.yielded_closure_error {
					self.yielded_closure_error = true;
					Some(Err(Error::new("dialog stream closed unexpectedly")))
				} else {
					None
				}
			},
		}
	}
}

/// Provider/model/budget resolution from a [`RunRequest`].
struct RunPlan {
	base_url:      String,
	api_key:       String,
	api:           String,
	provider:      String,
	model:         String,
	effort:        Option<String>,
	max_turns:     usize,
	context_limit: usize,
	system_prompt: Option<String>,
	tools:         Vec<&'static str>,
	supports_images: bool,
}

impl RunPlan {
	#[allow(dead_code, reason = "backwards-compatible resolve helper for tests")]
	pub fn resolve(request: &RunRequest) -> Result<Self> {
		Self::resolve_with_mcp(request, false)
	}

	pub fn resolve_with_mcp(request: &RunRequest, has_mcp_tools: bool) -> Result<Self> {
		let requested = request
			.model
			.clone()
			.ok_or_else(|| Error::new("no model: pass --model or set defaultModel in config"))?;
		// `provider/id` (first segment is the provider, as in TS model roles),
		// `provider/id:effort`, or a bare id resolved through the catalog.
		let effort: Option<&str>;
		let (provider, model) = match request.provider.as_deref() {
			Some(provider) => {
				let prefix = format!("{provider}/");
				let raw = requested
					.strip_prefix(prefix.as_str())
					.unwrap_or(&requested);
				let (id, parsed) = split_effort(raw);
				effort = parsed;
				(provider.to_string(), id.to_string())
			},
			None => match requested.split_once('/') {
				Some((provider, id)) if !provider.is_empty() && !id.is_empty() => {
					let (id, parsed) = split_effort(id);
					effort = parsed;
					(provider.to_string(), id.to_string())
				},
				_ => {
					let (id, parsed) = split_effort(&requested);
					effort = parsed;
					let provider = request
						.catalog
						.provider_of(id)
						.map(str::to_string)
						.ok_or_else(|| {
							Error::new(format!(
								"no provider for model `{id}`: pass --provider or use `provider/{id}`"
							))
						})?;
					(provider, id.to_string())
				},
			},
		};
		// Connection block: config.yml first, then models.yml providers.<id>.
		let connection = request
			.config
			.providers
			.get(&provider)
			.cloned()
			.or_else(|| request.catalog.connections.get(&provider).cloned());
		let base_url = connection
			.as_ref()
			.and_then(|connection| connection.base_url.clone())
			.ok_or_else(|| {
				Error::new(format!("provider `{provider}` has no baseUrl in config.yml or models.yml"))
			})?;
		// A keyless provider (local gateway, `auth: none`) is legal: the OpenAI
		// client sends an empty bearer token.
		let api_key = request
			.api_key
			.clone()
			.or_else(|| {
				connection
					.as_ref()
					.and_then(|connection| connection.resolve_api_key())
			})
			.unwrap_or_default();
		let context_limit = request
			.catalog
			.context_window(&provider, &model)
			.unwrap_or(DEFAULT_CONTEXT_LIMIT);
		let api = connection
			.as_ref()
			.and_then(|c| c.api.clone())
			.unwrap_or_else(|| "openai-completions".to_string());
		// Model image capabilities resolution:
		// - If catalog explicitly declares inputs without "image" -> fail closed (`supports_images = false`).
		// - If catalog explicitly declares "image" -> `supports_images = true`.
		// - If capabilities unknown (unlisted model / missing `input` in catalog) -> documented unknown,
		//   default to permissive (`supports_images = true`) so custom endpoints and unlisted models work.
		let supports_images = match request.catalog.supports_images(&provider, &model) {
			Some(supported) => supported,
			None => {
				// Unknown model capabilities: catalog does not declare input modalities for
				// `{provider}/{model}`. Defaulting to true for permissive unlisted gateway compatibility.
				true
			},
		};
		Ok(Self {
			base_url,
			api_key,
			api,
			provider,
			model,
			effort: effort.map(str::to_string),
			max_turns: request.max_turns.max(1),
			context_limit,
			system_prompt: compose_system_prompt(request),
			tools: allowed_tools(request, has_mcp_tools)?,
			supports_images,
		})
	}
}
/// Trailing `:effort` selector (`:high`, `:xhi`, …) is not part of the model
/// id.
const EFFORT_SELECTORS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

fn split_effort(model: &str) -> (&str, Option<&str>) {
	let Some((id, suffix)) = model.rsplit_once(':') else {
		return (model, None);
	};
	if id.is_empty() || suffix.len() < 2 {
		return (model, None);
	}
	let mut matching = EFFORT_SELECTORS
		.iter()
		.filter(|selector| selector.starts_with(suffix))
		.copied();
	let Some(selector) = matching.next() else {
		return (model, None);
	};
	if matching.next().is_some() {
		return (model, None);
	}
	(id, Some(selector))
}

/// Tools the model may call, preserving explicit configuration restrictions.
fn allowed_tools(request: &RunRequest, _has_mcp_tools: bool) -> Result<Vec<&'static str>> {
	if !request.tools_enabled {
		return Ok(Vec::new());
	}
	let mut tools: Vec<&'static str> = BUILTIN_TOOLS
		.iter()
		.map(|(name, _)| *name)
		.collect();
	if let Some(allowed) = &request.config.tools {
		tools.retain(|name| allowed.iter().any(|a| a == *name));
	}
	tools.sort_unstable();
	Ok(tools)
}

pub fn build_registry(tools: &[&'static str]) -> Result<ToolRegistry> {
	build_registry_with_mcp(tools, Vec::new())
}

pub fn build_registry_with_mcp(
	tools: &[&'static str],
	mcp_tools: Vec<McpTool>,
) -> Result<ToolRegistry> {
	let mut registry = ToolRegistry::new();
	for name in tools {
		let tool: Arc<dyn Tool> = match *name {
			"read" => Arc::new(ReadTool),
			"glob" => Arc::new(GlobTool),
			"grep" => Arc::new(GrepTool),
			"write" => Arc::new(WriteTool),
			"edit" => Arc::new(EditTool),
			"bash" => Arc::new(BashTool),
			"todo" => Arc::new(TodoTool::new()),
			other => {
				return Err(Error::new(format!(
					"unknown builtin tool `{other}`: BUILTIN_TOOLS and build_registry drifted"
				)));
			},
		};
		registry.register(tool);
	}
	for mcp_tool in mcp_tools {
		registry.register(Arc::new(mcp_tool));
	}
	Ok(registry)
}

/// Dynamic MCP tool implementing pi-agent's `Tool` trait.
#[derive(Clone)]
pub struct McpTool {
	name:        String,
	description: String,
	server_name: String,
	tool_name:   String,
	schema:      serde_json::Value,
	connection:  Arc<McpConnection>,
}

impl std::fmt::Debug for McpTool {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("McpTool")
			.field("name", &self.name)
			.field("server_name", &self.server_name)
			.field("tool_name", &self.tool_name)
			.field("description", &self.description)
			.finish()
	}
}

impl McpTool {
	pub fn new(
		server_name: &str,
		tool_name: &str,
		description: &str,
		schema: serde_json::Value,
		connection: Arc<McpConnection>,
	) -> Self {
		let name = create_mcp_tool_name(server_name, tool_name);
		Self {
			name,
			description: description.to_string(),
			server_name: server_name.to_string(),
			tool_name: tool_name.to_string(),
			schema,
			connection,
		}
	}

	pub fn name(&self) -> &str {
		&self.name
	}

	pub fn server_name(&self) -> &str {
		&self.server_name
	}

	pub fn tool_name(&self) -> &str {
		&self.tool_name
	}
}

impl McpToolOriginSource for McpTool {
	fn name(&self) -> &str {
		&self.name
	}

	fn origin_key(&self) -> Option<String> {
		Some(get_mcp_tool_origin_key(&self.server_name, &self.tool_name))
	}
}

#[async_trait]
impl Tool for McpTool {
	fn name(&self) -> &str {
		&self.name
	}

	fn description(&self) -> &str {
		&self.description
	}

	fn schema(&self) -> serde_json::Value {
		self.schema.clone()
	}

	async fn execute(
		&self,
		args: &serde_json::Value,
		ctx: &pi_agent::AgentContext,
	) -> std::result::Result<pi_agent::ToolResult, pi_agent::ToolError> {
		if ctx.cancellation_token.is_cancelled() {
			return Ok(pi_agent::ToolResult {
				tool_call_id: String::new(),
				content:      format!("MCP tool {} cancelled", self.name),
				is_error:     true,
			});
		}

		tokio::select! {
			_ = ctx.cancellation_token.cancelled() => {
				Ok(pi_agent::ToolResult {
					tool_call_id: String::new(),
					content: format!("MCP tool {} cancelled", self.name),
					is_error: true,
				})
			}
			res = self.connection.call_tool(&self.tool_name, args.clone()) => {
				match res {
					Ok(call_result) => {
						let mut content = String::new();
						for item in call_result.content {
							match item {
								MCPContent::Text { text } => {
									if !content.is_empty() {
										content.push_str("\n\n");
									}
									content.push_str(&text);
								},
								MCPContent::Image { data, mime_type } => {
									if !content.is_empty() {
										content.push_str("\n\n");
									}
									content.push_str(&format!("[Image: {mime_type}; base64={data}]"));
								},
								MCPContent::Resource { uri, text, blob } => {
									if !content.is_empty() {
										content.push_str("\n\n");
									}
									if let Some(t) = text {
										content.push_str(&format!("[Resource: {uri}]\n{t}"));
									} else if let Some(b) = blob {
										content.push_str(&format!("[Resource: {uri}; base64={b}]"));
									} else {
										content.push_str(&format!("[Resource: {uri}]"));
									}
								},
							}
						}
						let is_error = call_result.is_error.unwrap_or(false);
						Ok(pi_agent::ToolResult { tool_call_id: String::new(), content, is_error })
					},
					Err(err) => Ok(pi_agent::ToolResult {
						tool_call_id: String::new(),
						content: format!("MCP error: {err}"),
						is_error: true,
					}),
				}
			}
		}
	}
}

pub fn deduplicate_mcp_tools_with_warnings(tools: Vec<McpTool>) -> (Vec<McpTool>, Vec<String>) {
	let mut deduplicated: Vec<McpTool> = Vec::new();
	let mut warnings: Vec<String> = Vec::new();
	let mut registered: std::collections::HashMap<String, (McpTool, String, usize)> =
		std::collections::HashMap::new();

	for tool in tools {
		let origin_key = match tool.origin_key() {
			Some(k) => k,
			None => {
				deduplicated.push(tool);
				continue;
			},
		};

		if let Some((existing_tool, existing_origin_key, idx)) = registered.get_mut(tool.name()) {
			if *existing_origin_key == origin_key {
				continue;
			}
			let keep_existing = *existing_origin_key < origin_key;
			let (winner, loser) = if keep_existing {
				(&*existing_tool, &tool)
			} else {
				(&tool, &*existing_tool)
			};

			let msg = format!(
				"MCP tool name collision; keeping stable winner: name=\"{}\", kept=\"{}/{}\", \
				 ignored=\"{}/{}\"",
				tool.name(),
				winner.server_name(),
				winner.tool_name(),
				loser.server_name(),
				loser.tool_name(),
			);
			eprintln!("{msg}");
			warnings.push(msg);

			if !keep_existing {
				deduplicated[*idx] = tool.clone();
				*existing_tool = tool;
				*existing_origin_key = origin_key;
			}
		} else {
			let idx = deduplicated.len();
			registered.insert(tool.name().to_string(), (tool.clone(), origin_key, idx));
			deduplicated.push(tool);
		}
	}

	(deduplicated, warnings)
}

pub fn deduplicate_mcp_tools_with_warning(tools: Vec<McpTool>) -> Vec<McpTool> {
	let (dedup, _) = deduplicate_mcp_tools_with_warnings(tools);
	dedup
}

fn collect_all_configured_server_names(
	cwd: &Path,
	discovered: &[crate::config::DiscoveredMcpServer],
) -> Vec<String> {
	let mut names = std::collections::HashSet::new();
	for s in discovered {
		names.insert(s.name.clone());
	}
	let candidate_paths = [
		cwd.join(".omp").join("mcp.json"),
		cwd.join(".omp").join(".mcp.json"),
		cwd.join("mcp.json"),
		cwd.join(".mcp.json"),
	];
	for path in &candidate_paths {
		if let Ok(content) = std::fs::read_to_string(path) {
			if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
				if let Some(obj) = val.get("mcpServers").and_then(|v| v.as_object()) {
					for key in obj.keys() {
						names.insert(key.clone());
					}
				}
			}
		}
	}
	names.into_iter().collect()
}
fn mcp_tool_match_score(server_name: &str, tool_name: &str) -> usize {
	let sanitized = sanitize_mcp_tool_name_part(server_name, "server");
	let exact_prefix = format!("mcp__{sanitized}_");
	if tool_name.starts_with(&exact_prefix) {
		return exact_prefix.len();
	}

	// Bounded tool name: create_mcp_tool_name caps at 64 chars with _{hash}.
	// If the server prefix was truncated, match the prefix preserved before the hash.
	if tool_name.len() == 64 && tool_name.starts_with("mcp__") {
		if let Some(last_underscore) = tool_name.rfind('_') {
			if last_underscore >= 5 {
				let prefix_in_tool = &tool_name[..last_underscore];
				let full_prefix = format!("mcp__{sanitized}");
				if full_prefix.starts_with(prefix_in_tool) {
					return prefix_in_tool.len();
				}
			}
		}
	}

	0
}

fn mcp_server_matches_tools(
	server_name: &str,
	all_servers: &[String],
	approvals: &crate::approve::Approvals,
	requested_tools: &[String],
) -> bool {
	requested_tools.iter().any(|req| {
		let my_score = mcp_tool_match_score(server_name, req);
		if my_score == 0 {
			return false;
		}

		let mut top_score = 0;
		let mut candidates = Vec::new();
		for s in all_servers {
			let score = mcp_tool_match_score(s, req);
			if score > 0 {
				if score > top_score {
					top_score = score;
					candidates.clear();
					candidates.push(s.as_str());
				} else if score == top_score {
					candidates.push(s.as_str());
				}
			}
		}

		if my_score < top_score {
			return false;
		}

		if candidates.len() == 1 {
			return candidates[0] == server_name;
		}

		let explicitly_allowed: Vec<&str> = candidates
			.iter()
			.copied()
			.filter(|s| {
				approvals.allow.contains(*s)
					|| approvals.allow.contains(&format!("mcp:{s}"))
					|| approvals.allow.contains(&format!("mcp-server:{s}"))
			})
			.collect();

		explicitly_allowed.len() == 1 && explicitly_allowed[0] == server_name
	})
}

pub fn is_stdio_mcp_launch_allowed(
	server_name: &str,
	approvals: &crate::approve::Approvals,
) -> bool {
	is_stdio_mcp_launch_allowed_with_servers(server_name, &[server_name.to_string()], approvals)
}

pub fn is_stdio_mcp_launch_allowed_with_servers(
	server_name: &str,
	all_servers: &[String],
	approvals: &crate::approve::Approvals,
) -> bool {
	if approvals.mode == crate::approve::ApprovalMode::Yolo {
		return true;
	}

	// Explicit server-level allow is always definitive and unambiguous.
	let prompt_name = format!("mcp-server:{server_name}");
	if approvals.allow.contains(server_name)
		|| approvals.allow.contains(&format!("mcp:{server_name}"))
		|| approvals.allow.contains(&prompt_name)
	{
		return true;
	}

	// Unambiguous colon-delimited tool allow (e.g. mcp:foo:read).
	let colon_prefix = format!("mcp:{server_name}:");
	if approvals.allow.iter().any(|a| a.starts_with(&colon_prefix)) {
		return true;
	}

	// Per-entry tool allow matching:
	// A tool-level allow entry (e.g. `mcp__foo_bar_read` or bounded `mcp__common..._{hash}`)
	// authorizes server_name ONLY if server_name is the sole matching candidate among all_servers.
	// If multiple servers match the entry prefix (e.g. `foo` and `foo_bar` both match
	// `mcp__foo_bar_read`, or two servers collide at 55-byte cap limit), identity is ambiguous
	// and fail-closed: requires explicit server-level allow.
	// Bare server form `mcp__{X}` matching any configured server name (including disabled/suppressed
	// servers collected from config layers) is excluded from tool-matching to prevent discovery-shaping
	// bypasses.
	// ponytail: tool-level allow maps to server launch before tools/list; add per-tool RPC authorization once protocol supports it.
	approvals.allow.iter().any(|entry| {
		if let Some(rest) = entry.strip_prefix("mcp__") {
			if all_servers.iter().any(|s| {
				let s_san = sanitize_mcp_tool_name_part(s, "server");
				rest == s_san || rest == s.as_str()
			}) {
				eprintln!("ambiguous MCP allow \"{entry}\", use mcp:<server> for server-level");
				return false;
			}
		}

		let my_score = mcp_tool_match_score(server_name, entry);
		if my_score == 0 {
			return false;
		}

		let matching_candidates = all_servers
			.iter()
			.filter(|s| {
				mcp_tool_match_score(s, entry) > 0
					|| entry == &format!("mcp__{}", sanitize_mcp_tool_name_part(s, "server"))
					|| entry == &format!("mcp__{s}")
			})
			.count();

		matching_candidates == 1
	})
}

#[cfg(unix)]
fn poll_stdin_readable(timeout_ms: i32) -> bool {
	#[repr(C)]
	struct PollFd {
		fd: std::os::raw::c_int,
		events: std::os::raw::c_short,
		revents: std::os::raw::c_short,
	}
	const POLLIN: std::os::raw::c_short = 0x0001;
	unsafe extern "C" {
		fn poll(fds: *mut PollFd, nfds: usize, timeout: std::os::raw::c_int) -> std::os::raw::c_int;
	}
	let mut pfd = PollFd {
		fd: 0,
		events: POLLIN,
		revents: 0,
	};
	let ret = unsafe { poll(&mut pfd, 1, timeout_ms) };
	ret > 0 && (pfd.revents & POLLIN) != 0
}

#[cfg(unix)]
fn read_stdin_byte() -> Option<u8> {
	unsafe extern "C" {
		fn read(fd: std::os::raw::c_int, buf: *mut std::os::raw::c_void, count: usize) -> isize;
	}
	let mut b = 0u8;
	let ret = unsafe { read(0, &mut b as *mut u8 as *mut std::os::raw::c_void, 1) };
	if ret == 1 { Some(b) } else { None }
}

async fn check_or_prompt_stdio_mcp_launch(
	server_name: &str,
	all_servers: &[String],
	stdio: &MCPStdioServerConfig,
	approvals: &crate::approve::Approvals,
	cancel_token: &pi_agent::CancellationToken,
) -> Result<bool> {
	if cancel_token.is_cancelled() {
		return Err(Error::new("MCP discovery interrupted"));
	}

	if is_stdio_mcp_launch_allowed_with_servers(server_name, all_servers, approvals) {
		return Ok(true);
	}

	use std::io::IsTerminal;
	if !std::io::stdin().is_terminal() {
		return Ok(false);
	}

	use std::io::Write;
	let prompt_name = format!("mcp-server:{server_name}");
	let detail = format!("{} {}", stdio.command, stdio.args.join(" "));
	eprint!("Approve {prompt_name} ({detail})? [y/N] ");
	let _ = std::io::stderr().flush();

	let cancel_clone = cancel_token.clone();
	let mut prompt_fut = Box::pin(tokio::task::spawn_blocking(move || -> std::io::Result<bool> {
		let mut buf: Vec<u8> = Vec::new();
		loop {
			if cancel_clone.is_cancelled() {
				return Ok(false);
			}
			#[cfg(unix)]
			{
				if poll_stdin_readable(100) {
					match read_stdin_byte() {
						Some(b'\n') => break,
						Some(b'\r') => {},
						Some(b) => buf.push(b),
						None => return Ok(false),
					}
				}
			}
			#[cfg(not(unix))]
			{
				break;
			}
		}

		if cancel_clone.is_cancelled() {
			return Ok(false);
		}

		let answer = String::from_utf8_lossy(&buf).trim().to_ascii_lowercase();
		Ok(matches!(answer.as_str(), "y" | "yes"))
	}));

	tokio::select! {
		_ = cancel_token.cancelled() => {
			let _ = prompt_fut.await;
			Err(Error::new("MCP discovery interrupted"))
		}
		res = &mut prompt_fut => {
			match res {
				Ok(Ok(confirmed)) => Ok(confirmed),
				Ok(Err(err)) => Err(Error::with_source("failed reading approval", err)),
				Err(_) => Ok(false),
			}
		}
	}
}

pub async fn filter_and_spawn_mcp_servers(
	discovered: Vec<crate::config::DiscoveredMcpServer>,
	cwd: &Path,
	config: &crate::config::Config,
	approvals: &crate::approve::Approvals,
	cancel_token: &pi_agent::CancellationToken,
) -> Result<Vec<McpTool>> {
	let mut all_tools = Vec::new();
	let mut server_errors: Vec<(String, String)> = Vec::new();
	let all_server_names = collect_all_configured_server_names(cwd, &discovered);
	if let Some(requested_tools) = &config.tools {
		for req in requested_tools {
			let mut top_score = 0;
			let mut candidates = Vec::new();
			for s in &all_server_names {
				let score = mcp_tool_match_score(s, req);
				if score > 0 {
					if score > top_score {
						top_score = score;
						candidates.clear();
						candidates.push(s.as_str());
					} else if score == top_score {
						candidates.push(s.as_str());
					}
				}
			}
			if candidates.len() > 1 {
				let explicitly_allowed: Vec<&str> = candidates
					.iter()
					.copied()
					.filter(|s| {
						approvals.allow.contains(*s)
							|| approvals.allow.contains(&format!("mcp:{s}"))
							|| approvals.allow.contains(&format!("mcp-server:{s}"))
					})
					.collect();
				if explicitly_allowed.len() != 1 {
					return Err(Error::new(format!(
						"Ambiguous tool \"{}\" in --tools: matches multiple servers ({}). Disambiguate by explicitly allowing the intended server with --allow <server>.",
						req,
						candidates.join(", ")
					)));
				}
			}
		}
	}

	for server in discovered {
		if cancel_token.is_cancelled() {
			break;
		}

		if let Some(requested_tools) = &config.tools {
			if !mcp_server_matches_tools(&server.name, &all_server_names, approvals, requested_tools) {
				continue;
			}
		}

		let mut server_config = server.config;
		if let MCPServerConfig::Stdio(stdio) = &mut server_config {
			let allowed = check_or_prompt_stdio_mcp_launch(
				&server.name,
				&all_server_names,
				stdio,
				approvals,
				cancel_token,
			).await?;
			if !allowed {
				let err_msg = format!(
					"MCP server \"{}\" was not launched: stdio launch was not approved",
					server.name
				);
				eprintln!("{err_msg}");
				server_errors.push((server.name.clone(), err_msg));
				continue;
			}
			if stdio.cwd.is_none() {
				stdio.cwd = Some(cwd.to_string_lossy().to_string());
			}
		}

		let connect_fut = connect_to_server(&server.name, server_config);
		let conn = tokio::select! {
			_ = cancel_token.cancelled() => break,
			res = connect_fut => match res {
				Ok(connection) => Arc::new(connection),
				Err(err) => {
					let err_msg = format!("Failed to connect to MCP server \"{}\": {err}", server.name);
					eprintln!("{err_msg}");
					server_errors.push((server.name.clone(), err_msg));
					continue;
				}
			}
		};

		let list_fut = conn.list_tools();
		let tool_defs = tokio::select! {
			_ = cancel_token.cancelled() => break,
			res = list_fut => match res {
				Ok(defs) => defs,
				Err(err) => {
					let err_msg = format!("Failed to list tools from MCP server \"{}\": {err}", server.name);
					eprintln!("{err_msg}");
					server_errors.push((server.name.clone(), err_msg));
					continue;
				}
			}
		};

		for def in tool_defs {
			let desc = def.description.as_deref().unwrap_or("");
			let mcp_tool =
				McpTool::new(&server.name, &def.name, desc, def.input_schema, Arc::clone(&conn));
			all_tools.push(mcp_tool);
		}
	}

	let mut unique_tools = deduplicate_mcp_tools_with_warning(all_tools);
	if cancel_token.is_cancelled() {
		return Ok(unique_tools);
	}


	if let Some(requested_tools) = &config.tools {
		let mut known_tools: Vec<String> = BUILTIN_TOOLS.iter().map(|(n, _)| n.to_string()).collect();
		for tool in &unique_tools {
			known_tools.push(tool.name().to_string());
		}
		let unknown: Vec<String> = requested_tools
			.iter()
			.filter(|r| !known_tools.iter().any(|k| k == *r))
			.cloned()
			.collect();
		if !unknown.is_empty() {
			for (failed_server, err_msg) in &server_errors {
				if mcp_server_matches_tools(failed_server, &all_server_names, approvals, &unknown) {
					return Err(Error::new(err_msg.clone()));
				}
			}
			let plural = if unknown.len() == 1 { "" } else { "s" };
			return Err(Error::new(format!(
				"Unknown tool{} in --tools: {}. Valid tools: {}.",
				plural,
				unknown.join(", "),
				known_tools.join(", ")
			)));
		}

		unique_tools.retain(|tool| requested_tools.iter().any(|allowed| allowed == tool.name()));
	}

	Ok(unique_tools)
}

pub async fn load_and_filter_mcp_tools(
	cwd: &Path,
	config: &crate::config::Config,
	approvals: &crate::approve::Approvals,
	cancel_token: &pi_agent::CancellationToken,
) -> Result<Vec<McpTool>> {
	let discovered = match crate::config::discover_mcp_servers(cwd) {
		Ok(s) => s,
		Err(err) => {
			eprintln!("Error discovering MCP servers: {err}");
			Vec::new()
		},
	};
	filter_and_spawn_mcp_servers(discovered, cwd, config, approvals, cancel_token).await
}

pub async fn load_and_filter_mcp_tools_with_paths(
	cwd: &Path,
	agent_dir: &Path,
	config: &crate::config::Config,
	approvals: &crate::approve::Approvals,
	cancel_token: &pi_agent::CancellationToken,
) -> Result<Vec<McpTool>> {
	let report = crate::config::discover_mcp_servers_with_paths(cwd, agent_dir);
	for warning in &report.warnings {
		eprintln!("{warning}");
	}
	filter_and_spawn_mcp_servers(report.servers, cwd, config, approvals, cancel_token).await
}

/// File-resolved prompt pieces joined into one system message.
fn compose_system_prompt(request: &RunRequest) -> Option<String> {
	let mut parts: Vec<String> = Vec::new();
	for text in [request.system_prompt.as_deref(), request.append_system_prompt.as_deref()]
		.into_iter()
		.flatten()
	{
		if !text.trim().is_empty() {
			parts.push(text.trim_end().to_string());
		}
	}
	for file in &request.context_files {
		parts.push(format!("# {}\n\n{}", file.path.display(), file.content.trim_end()));
	}
	for skill in &request.skills {
		parts.push(format!("# Skill: {}\n\n{}", skill.name, skill.body.trim()));
	}
	(!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// Previous session turns as `pi-agent` history messages.
pub fn history_messages(history: &[crate::session::TurnRecord]) -> Result<Vec<pi_agent::Message>, Error> {
	let mut messages = Vec::new();
	for turn in history {
		if !turn.parts.is_empty() {
			let mut pi_parts = Vec::new();
			for (index, p) in turn.parts.iter().enumerate() {
				if p.is_image() {
					let Some(data) = p.data.as_deref() else {
						return Err(Error::new(format!(
							"session history contains image part missing data at index {index}"
						)));
					};
					let Some(mime) = p.mime_type.as_deref() else {
						return Err(Error::new(format!(
							"session history contains image part missing mimeType at index {index}"
						)));
					};
					let (clean_b64, norm_mime) = pi_agent::state::normalize_image_payload(data, mime)
						.map_err(|err| Error::new(format!("session history contains invalid image part at index {index}: {err}")))?;
					pi_parts.push(pi_agent::ContentPart::Image {
						data: clean_b64,
						mime_type: norm_mime,
					});
				} else if p.kind == "text" {
					let Some(text) = p.text.clone() else {
						return Err(Error::new(format!(
							"session history contains malformed text part missing text at index {index}"
						)));
					};
					pi_parts.push(pi_agent::ContentPart::Text { text });
				} else {
					return Err(Error::new(format!(
						"session history contains unsupported content part kind at index {index}"
					)));
				}
			}
			messages.push(pi_agent::Message::user_with_parts(pi_parts));
		} else {
			messages.push(pi_agent::Message::user(turn.prompt.clone()));
		}
		let stop_reason = crate::session::canonical_stop_reason(&turn.stop_reason);
		if stop_reason == "aborted" {
			if !turn.tool_calls.is_empty() {
				let calls: Vec<pi_agent::ToolCall> = turn
					.tool_calls
					.iter()
					.map(|call| pi_agent::ToolCall {
						id:        call.id.clone(),
						name:      call.name.clone(),
						arguments: call.arguments.clone(),
					})
					.collect();
				messages.push(pi_agent::Message::assistant_with_tools("", calls));
				for call in &turn.tool_calls {
					let name = if call.name.is_empty() {
						"tool"
					} else {
						&call.name
					};
					messages.push(pi_agent::Message::tool_with_status(
						call.id.clone(),
						name,
						call.result.clone(),
						call.is_error,
					));
				}
			}
			continue;
		}
		if turn.tool_calls.is_empty() {
			if !turn.assistant.is_empty() {
				messages.push(pi_agent::Message::assistant(turn.assistant.clone()));
			}
			continue;
		}
		let calls: Vec<pi_agent::ToolCall> = turn
			.tool_calls
			.iter()
			.map(|call| pi_agent::ToolCall {
				id:        call.id.clone(),
				name:      call.name.clone(),
				arguments: call.arguments.clone(),
			})
			.collect();
		messages.push(pi_agent::Message::assistant_with_tools(turn.assistant.clone(), calls));
		for call in &turn.tool_calls {
			let name = if call.name.is_empty() {
				"tool"
			} else {
				&call.name
			};
			messages.push(pi_agent::Message::tool_with_status(
				call.id.clone(),
				name,
				call.result.clone(),
				call.is_error,
			));
		}
	}
	messages
}

#[cfg(test)]
mod tests {
	use std::{collections::BTreeMap, path::PathBuf};

	use super::*;
	use crate::{
		approve::{ApprovalMode, Approvals},
		catalog::{Catalog, ModelInfo},
		config::{Config, ProviderConfig},
		output::OutputMode,
		prompts::ContextFile,
		skills::Skill,
	};

	fn catalog() -> Catalog {
		let mut providers = BTreeMap::new();
		let mut models = BTreeMap::new();
		models.insert("llama3".to_string(), ModelInfo {
			provider:       "local".to_string(),
			context_window: Some(8_192),
			input:          None,
		});
		providers.insert("local".to_string(), models);
		Catalog { providers, connections: BTreeMap::new(), source: PathBuf::from("test-models.json") }
	}

	fn catalog_with_connection(provider: &str, base_url: &str, api_key: Option<&str>) -> Catalog {
		let mut catalog = catalog();
		catalog
			.connections
			.insert(provider.to_string(), ProviderConfig {
				base_url: Some(base_url.to_string()),
				api_key: api_key.map(str::to_string),
				..ProviderConfig::default()
			});
		catalog
	}

	fn request(approvals: Approvals) -> RunRequest {
		let mut config = Config::default();
		config
			.providers
			.insert("local".to_string(), ProviderConfig {
				base_url: Some("http://127.0.0.1:8080/v1".to_string()),
				api_key: Some("sk-test".to_string()),
				..ProviderConfig::default()
			});
		RunRequest {
			prompt: "fix the build".to_string(),
			cwd: PathBuf::from("/work"),
			output_mode: OutputMode::Text,
			model: Some("local/llama3".to_string()),
			provider: None,
			api_key: None,
			system_prompt: Some("base prompt".to_string()),
			append_system_prompt: Some("appended".to_string()),
			context_files: vec![ContextFile {
				path:    PathBuf::from("/work/.omp/AGENTS.md"),
				content: "repo rules".to_string(),
			}],
			skills: vec![Skill {
				name:        "review".to_string(),
				description: "review code".to_string(),
				path:        PathBuf::from("/work/.omp/skills/review/SKILL.md"),
				body:        "skill body".to_string(),
			}],
			config,
			catalog: Arc::new(catalog()),
			approvals,
			max_time: None,
			tools_enabled: true,
			max_turns: 5,
			history: Vec::new(),
			cancellation_token: None,
		}
	}

	fn yolo() -> Approvals {
		Approvals::new(ApprovalMode::Yolo, Vec::new())
	}

	#[test]
	fn plan_maps_request_onto_provider_config() {
		let plan = RunPlan::resolve(&request(yolo())).expect("plan");
		assert_eq!(plan.base_url, "http://127.0.0.1:8080/v1");
		assert_eq!(plan.api_key, "sk-test");
		assert_eq!(plan.model, "llama3");
		assert_eq!(plan.max_turns, 5);
		assert_eq!(plan.context_limit, 8_192);
		assert_eq!(plan.tools, ["bash", "edit", "glob", "grep", "read", "todo", "write"]);
		assert_eq!(plan.effort, None);
		let system = plan.system_prompt.expect("system prompt");
		for part in ["base prompt", "appended", "repo rules", "skill body"] {
			assert!(system.contains(part), "system prompt is missing {part}");
		}
	}

	#[test]
	fn plan_requires_a_connection_and_allows_keyless_providers() {
		let mut missing_url = request(yolo());
		missing_url.config.providers.clear();
		let err = RunPlan::resolve(&missing_url).err().expect("no baseUrl");
		assert!(err.to_string().contains("has no baseUrl"));

		// Keyless local gateways are legal: the client sends an empty bearer.
		let mut missing_key = request(yolo());
		if let Some(provider) = missing_key.config.providers.get_mut("local") {
			provider.api_key = None;
		}
		let plan = RunPlan::resolve(&missing_key).expect("keyless provider");
		assert_eq!(plan.api_key, "");

		let mut no_model = request(yolo());
		no_model.model = None;
		assert!(RunPlan::resolve(&no_model).is_err());
	}

	#[test]
	fn effort_suffix_is_split_from_the_model_id() {
		assert_eq!(split_effort("llama3"), ("llama3", None));
		assert_eq!(split_effort("llama3:high"), ("llama3", Some("high")));
		assert_eq!(split_effort("llama3:hi"), ("llama3", Some("high")));
		assert_eq!(split_effort("llama3:xhi"), ("llama3", Some("xhigh")));
		assert_eq!(split_effort("llama3:max"), ("llama3", Some("max")));
		// Not an effort selector: the suffix stays part of the id.
		assert_eq!(split_effort("qwen:7b"), ("qwen:7b", None));
		assert_eq!(split_effort("llama3:m"), ("llama3:m", None));
		assert_eq!(split_effort(":high"), (":high", None));
	}

	#[test]
	fn explicit_provider_still_parses_the_effort_suffix() {
		let mut request = request(yolo());
		request.model = Some("llama3:high".to_string());
		request.provider = Some("local".to_string());
		let plan = RunPlan::resolve(&request).expect("plan");
		assert_eq!(plan.model, "llama3");
		assert_eq!(plan.effort.as_deref(), Some("high"));

		request.model = Some("llama3".to_string());
		let plan = RunPlan::resolve(&request).expect("plan");
		assert_eq!(plan.effort, None);
	}

	#[test]
	fn registry_implements_every_builtin_tool_name() {
		let names: Vec<&'static str> = BUILTIN_TOOLS.iter().map(|(name, _)| *name).collect();
		build_registry(&names).expect("every BUILTIN_TOOLS entry is implemented");
		let err = build_registry(&["nope"]).err().expect("unknown tool");
		assert!(err.to_string().contains("unknown builtin tool `nope`"));
	}

	#[test]
	fn provider_prefixed_model_resolves_through_models_yml_connection() {
		let mut nested = request(yolo());
		nested.model = Some("9router/ocg/mimo-v2.6-pro:high".to_string());
		nested.provider = None;
		nested.config.providers.clear();
		nested.catalog =
			Arc::new(catalog_with_connection("9router", "http://127.0.0.1:20128/v1", None));
		let plan = RunPlan::resolve(&nested).expect("plan");
		assert_eq!(plan.model, "ocg/mimo-v2.6-pro");
		assert_eq!(plan.base_url, "http://127.0.0.1:20128/v1");
		assert_eq!(plan.api_key, "");
		// `:high` is a reasoning-effort selector, not part of the model id; it is
		// normalized to the full selector and carried to the provider builder.
		assert_eq!(plan.effort.as_deref(), Some("high"));

		let mut explicit = request(yolo());
		explicit.model = Some("ocg/mimo-v2.6-pro".to_string());
		explicit.provider = Some("9router".to_string());
		explicit.config.providers.clear();
		explicit.catalog = Arc::new(catalog_with_connection(
			"9router",
			"http://127.0.0.1:20128/v1",
			Some("sk-models-yml"),
		));
		let plan = RunPlan::resolve(&explicit).expect("plan");
		assert_eq!(plan.model, "ocg/mimo-v2.6-pro");
		assert_eq!(plan.api_key, "sk-models-yml");

		// Explicit provider plus the fully prefixed form strips the prefix once.
		let mut prefixed = request(yolo());
		prefixed.model = Some("9router/ocg/mimo-v2.6-pro".to_string());
		prefixed.provider = Some("9router".to_string());
		prefixed.config.providers.clear();
		prefixed.catalog =
			Arc::new(catalog_with_connection("9router", "http://127.0.0.1:20128/v1", None));
		let plan = RunPlan::resolve(&prefixed).expect("plan");
		assert_eq!(plan.model, "ocg/mimo-v2.6-pro");
	}

	#[test]
	fn config_connection_wins_over_models_yml() {
		let mut request = request(yolo());
		request.catalog = Arc::new(catalog_with_connection(
			"local",
			"http://models-yml.example/v1",
			Some("sk-models-yml"),
		));
		let plan = RunPlan::resolve(&request).expect("plan");
		assert_eq!(plan.base_url, "http://127.0.0.1:8080/v1");
		assert_eq!(plan.api_key, "sk-test");
	}

	#[test]
	fn write_mode_offers_all_tools() {
		let plan =
			RunPlan::resolve(&request(Approvals::new(ApprovalMode::Write, Vec::new()))).expect("plan");
		assert_eq!(plan.tools, ["bash", "edit", "glob", "grep", "read", "todo", "write"]);
	}

	#[test]
	fn always_ask_offers_all_tools() {
		let plan = RunPlan::resolve(&request(Approvals::new(ApprovalMode::AlwaysAsk, Vec::new())))
			.expect("plan");
		assert_eq!(plan.tools, ["bash", "edit", "glob", "grep", "read", "todo", "write"]);

		let mut req = request(Approvals::new(ApprovalMode::AlwaysAsk, Vec::new()));
		req.config.tools = Some(vec!["bash".to_string()]);
		let plan = RunPlan::resolve(&req).expect("plan");
		assert_eq!(plan.tools, ["bash"]);
	}

	#[test]
	fn no_tools_disables_the_registry() {
		let mut request = request(yolo());
		request.tools_enabled = false;
		let plan = RunPlan::resolve(&request).expect("plan");
		assert!(plan.tools.is_empty());
	}

	#[test]
	fn history_turns_become_pi_agent_messages() {
		let history = vec![crate::session::TurnRecord {
			prompt:      "fix".to_string(),
			assistant:   "done".to_string(),
			stop_reason: "end_turn".to_string(),
			tool_calls:  vec![crate::session::ToolCallRecord {
				id:        "call-1".to_string(),
				name:      "read".to_string(),
				arguments: serde_json::json!({"path": "a.txt"}),
				result:    "contents".to_string(),
				is_error:  false,
			}],
			parts:       vec![],
		}];
		let messages = history_messages(&history).expect("history messages");
		assert_eq!(messages.len(), 3);
		assert_eq!(messages[0].role, pi_agent::Role::User);
		assert_eq!(messages[0].content, "fix");
		assert_eq!(messages[1].role, pi_agent::Role::Assistant);
		assert_eq!(messages[1].tool_calls.len(), 1);
		assert_eq!(messages[1].tool_calls[0].id, "call-1");
		assert_eq!(messages[2].role, pi_agent::Role::Tool);
		assert_eq!(messages[2].tool_call_id.as_deref(), Some("call-1"));
		assert_eq!(messages[2].content, "contents");
	}

	#[test]
	fn history_messages_excludes_aborted_assistant_without_tools() {
		let history = vec![crate::session::TurnRecord {
			prompt:      "fix".to_string(),
			assistant:   "partial output".to_string(),
			stop_reason: "aborted".to_string(),
			tool_calls:  vec![],
			parts:       vec![],
		}];
		let messages = history_messages(&history).expect("history messages");
		assert_eq!(messages.len(), 1);
		assert_eq!(messages[0].role, pi_agent::Role::User);
		assert_eq!(messages[0].content, "fix");
	}

	#[test]
	fn history_messages_keeps_aborted_assistant_with_tool_batch_results() {
		let history = vec![crate::session::TurnRecord {
			prompt:      "fix".to_string(),
			assistant:   "partial answer that should be excluded".to_string(),
			stop_reason: "aborted".to_string(),
			tool_calls:  vec![crate::session::ToolCallRecord {
				id:        "call-1".to_string(),
				name:      "read".to_string(),
				arguments: serde_json::json!({"path": "a.txt"}),
				result:    "[Tool execution interrupted by user]".to_string(),
				is_error:  true,
			}],
			parts:       vec![],
		}];
		let messages = history_messages(&history).expect("history messages");
		assert_eq!(messages.len(), 3);
		assert_eq!(messages[0].role, pi_agent::Role::User);
		assert_eq!(messages[1].role, pi_agent::Role::Assistant);
		assert_eq!(messages[1].content, "");
		assert_eq!(messages[1].tool_calls.len(), 1);
		assert_eq!(messages[2].role, pi_agent::Role::Tool);
		assert_eq!(messages[2].content, "[Tool execution interrupted by user]");
	}

	#[test]
	fn history_messages_preserves_failed_tool_status_and_name() {
		let history = vec![crate::session::TurnRecord {
			prompt:      "run command".to_string(),
			assistant:   "running".to_string(),
			stop_reason: "stop".to_string(),
			tool_calls:  vec![crate::session::ToolCallRecord {
				id:        "call-err-1".to_string(),
				name:      "bash".to_string(),
				arguments: serde_json::json!({"command": "false"}),
				result:    "exit code 1".to_string(),
				is_error:  true,
			}],
			parts:       vec![],
		}];
		let messages = history_messages(&history).expect("history messages");
		assert_eq!(messages.len(), 3);
		assert_eq!(messages[2].role, pi_agent::Role::Tool);
		assert_eq!(messages[2].tool_call_id.as_deref(), Some("call-err-1"));
		assert_eq!(messages[2].name.as_deref(), Some("bash"));
		assert!(messages[2].is_error, "failed tool call must preserve is_error: true on replay");
		assert_eq!(messages[2].content, "exit code 1");
	}

	#[test]
	fn history_messages_preserves_aborted_failed_tool_status_and_name() {
		let history = vec![crate::session::TurnRecord {
			prompt:      "run command".to_string(),
			assistant:   "running".to_string(),
			stop_reason: "aborted".to_string(),
			tool_calls:  vec![crate::session::ToolCallRecord {
				id:        "call-err-2".to_string(),
				name:      "fetch".to_string(),
				arguments: serde_json::json!({"url": "http://invalid"}),
				result:    "[Tool execution interrupted by user]".to_string(),
				is_error:  true,
			}],
			parts:       vec![],
		}];
		let messages = history_messages(&history).expect("history messages");
		assert_eq!(messages.len(), 3);
		assert_eq!(messages[2].role, pi_agent::Role::Tool);
		assert_eq!(messages[2].tool_call_id.as_deref(), Some("call-err-2"));
		assert_eq!(messages[2].name.as_deref(), Some("fetch"));
		assert!(
			messages[2].is_error,
			"aborted failed tool call must preserve is_error: true on replay"
		);
		assert_eq!(messages[2].content, "[Tool execution interrupted by user]");
	}

	#[test]
	fn history_messages_preserves_user_image_parts() {
		let history = vec![crate::session::TurnRecord {
			prompt:      "inspect diagram".to_string(),
			assistant:   "looks correct".to_string(),
			stop_reason: "end_turn".to_string(),
			tool_calls:  vec![],
			parts:       vec![
				crate::session::ContentPart::text("inspect diagram"),
				crate::session::ContentPart::image("aGVsbG8=", "image/png"),
			],
		}];
		let messages = history_messages(&history).expect("history messages");
		assert_eq!(messages.len(), 2);
		assert_eq!(messages[0].role, pi_agent::Role::User);
		assert_eq!(messages[0].content, "inspect diagram");
		assert!(messages[0].has_image());
		assert_eq!(messages[0].image_parts().len(), 1);
		assert_eq!(messages[0].image_parts()[0].mime_type, "image/png");
		assert_eq!(messages[0].image_parts()[0].data_base64, "aGVsbG8=");
	}

	#[test]
	fn history_messages_rejects_unsupported_part_kind() {
		let history = vec![crate::session::TurnRecord {
			prompt: "look".to_string(),
			assistant: "ok".to_string(),
			stop_reason: "stop".to_string(),
			tool_calls: vec![],
			parts: vec![crate::session::ContentPart {
				kind: "video".to_string(),
				text: None,
				data: Some("video-stream".to_string()),
				mime_type: Some("video/mp4".to_string()),
				extra: serde_json::Map::new(),
			}],
		}];
		let err = history_messages(&history).unwrap_err();
		assert!(
			err.to_string().contains("unsupported content part kind at index 0"),
			"must reject unknown part kind safely, got: {err}"
		);
	}

	#[test]
	fn history_messages_rejects_malformed_image_part() {
		let history = vec![crate::session::TurnRecord {
			prompt: "look".to_string(),
			assistant: "ok".to_string(),
			stop_reason: "stop".to_string(),
			tool_calls: vec![],
			parts: vec![crate::session::ContentPart {
				kind: "image".to_string(),
				text: None,
				data: Some("http://evil.com/pic.png".to_string()),
				mime_type: Some("image/png".to_string()),
				extra: serde_json::Map::new(),
			}],
		}];
		let err = history_messages(&history).unwrap_err();
		assert!(
			err.to_string().contains("external URLs are not permitted"),
			"must reject external URL in history, got: {err}"
		);
	}

	#[test]
	fn resumed_text_only_session_preserves_unaltered_string_wire_format() {
		let history = vec![crate::session::TurnRecord {
			prompt: "hello purely textual".to_string(),
			assistant: "hello human".to_string(),
			stop_reason: "stop".to_string(),
			tool_calls: vec![],
			parts: vec![],
		}];
		let messages = history_messages(&history).expect("history messages");
		assert_eq!(messages.len(), 2);
		assert_eq!(messages[0].content, "hello purely textual");
		assert!(messages[0].parts.is_empty(), "text-only turn must have empty parts");
		assert!(!messages[0].has_image());

		let wire = pi_agent::to_openai_messages(&messages).expect("wire messages");
		let wire_val = serde_json::to_value(&wire[0]).expect("serialize");
		assert_eq!(
			wire_val["content"],
			"hello purely textual",
			"resumed text-only message must serialize as plain string on OpenAI wire, not array"
		);
	}
	#[test]
	fn interrupted_tool_batch_emits_turn_end_exactly_once() {
		use std::io::{Read, Write};

		let (addr_tx, addr_rx) = std::sync::mpsc::channel::<std::net::SocketAddr>();

		std::thread::spawn(move || {
			let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
			addr_tx.send(listener.local_addr().unwrap()).unwrap();
			let (mut socket, _) = listener.accept().unwrap();
			let mut buf = [0u8; 1024];
			let _ = socket.read(&mut buf).unwrap();
			socket
				.write_all(
					b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
				)
				.unwrap();
			socket
				.write_all(
					b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"command\\\":\\\"sleep 5\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
				)
				.unwrap();
			socket.write_all(b"data: [DONE]\n\n").unwrap();
			socket.flush().unwrap();
			std::thread::sleep(std::time::Duration::from_millis(500));
		});

		let addr = addr_rx.recv().unwrap();
		let mut req = request(yolo());
		req.cwd = std::env::temp_dir();
		req.config
			.providers
			.insert("local".to_string(), ProviderConfig {
				base_url: Some(format!("http://{addr}/v1")),
				api_key: Some("sk-test".to_string()),
				..ProviderConfig::default()
			});
		req.max_time = Some(std::time::Duration::from_millis(150));

		let runner = PiAgentRunner::new().expect("runner");
		let stream = runner.run(&req).expect("stream");
		let mut turn_ends = Vec::new();
		let mut tool_results = Vec::new();
		for ev in stream {
			match ev {
				Ok(Event::TurnEnd { stop_reason, .. }) => turn_ends.push(stop_reason),
				Ok(Event::ToolEnd { result, error, .. }) => tool_results.push((result, error)),
				_ => {},
			}
		}
		assert_eq!(turn_ends, vec!["aborted".to_string()]);
		assert_eq!(tool_results.len(), 1);
		assert!(tool_results[0].1.is_some(), "interrupted tool must report error status");
	}
	#[test]
	fn live_events_stream_before_sse_done() {
		use std::io::{Read, Write};

		let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
		let (addr_tx, addr_rx) = std::sync::mpsc::channel::<std::net::SocketAddr>();

		std::thread::spawn(move || {
			let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
			addr_tx.send(listener.local_addr().unwrap()).unwrap();
			let (mut socket, _) = listener.accept().unwrap();
			let mut buf = [0u8; 1024];
			let _ = socket.read(&mut buf).unwrap();
			socket
				.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n")
				.unwrap();
			socket
				.write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"first live delta\"},\"finish_reason\":null}]}\n\n")
				.unwrap();
			socket.flush().unwrap();

			let _ = done_rx.recv();
			socket.write_all(b"data: [DONE]\n\n").unwrap();
			socket.flush().unwrap();
		});

		let addr = addr_rx.recv().unwrap();
		let mut req = request(yolo());
		req.config
			.providers
			.insert("local".to_string(), ProviderConfig {
				base_url: Some(format!("http://{addr}/v1")),
				api_key: Some("sk-test".to_string()),
				..ProviderConfig::default()
			});

		let runner = PiAgentRunner::new().expect("runner");
		let mut stream = runner.run(&req).expect("stream");

		let ev1 = stream.next().expect("first event").expect("ok");
		assert_eq!(ev1, Event::MessageStart { role: "assistant".to_string() });

		let ev2 = stream.next().expect("second event").expect("ok");
		assert_eq!(ev2, Event::MessageUpdate { delta: "first live delta".to_string() });

		let _ = done_tx.send(());

		let ev3 = stream.next().expect("third event").expect("ok");
		assert!(
			matches!(ev3, Event::MessageEnd { ref message, .. } if message.content == "first live delta")
		);

		let ev4 = stream.next().expect("fourth event").expect("ok");
		assert!(matches!(ev4, Event::TurnEnd { ref stop_reason, .. } if stop_reason == "stop"));

		let ev5 = stream.next().expect("fifth event").expect("ok");
		assert!(
			matches!(ev5, Event::AgentEnd { ref messages, .. } if messages.iter().any(|m| m.content == "first live delta"))
		);

		assert!(stream.next().is_none());
	}

	#[test]
	fn live_runner_timeout_steers_interruption() {
		use std::io::{Read, Write};

		let (addr_tx, addr_rx) = std::sync::mpsc::channel::<std::net::SocketAddr>();

		std::thread::spawn(move || {
			let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
			addr_tx.send(listener.local_addr().unwrap()).unwrap();
			let (mut socket, _) = listener.accept().unwrap();
			let mut buf = [0u8; 1024];
			let _ = socket.read(&mut buf).unwrap();
			socket
				.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n")
				.unwrap();
			socket
				.write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"start\"},\"finish_reason\":null}]}\n\n")
				.unwrap();
			socket.flush().unwrap();
			std::thread::sleep(std::time::Duration::from_secs(5));
		});

		let addr = addr_rx.recv().unwrap();
		let mut req = request(yolo());
		req.config
			.providers
			.insert("local".to_string(), ProviderConfig {
				base_url: Some(format!("http://{addr}/v1")),
				api_key: Some("sk-test".to_string()),
				..ProviderConfig::default()
			});
		req.max_time = Some(std::time::Duration::from_millis(150));

		let runner = PiAgentRunner::new().expect("runner");
		let mut stream = runner.run(&req).expect("stream");

		let ev1 = stream.next().expect("first event").expect("ok");
		assert_eq!(ev1, Event::MessageStart { role: "assistant".to_string() });
		let ev2 = stream.next().expect("second event").expect("ok");
		assert_eq!(ev2, Event::MessageUpdate { delta: "start".to_string() });

		let ev3 = stream.next().expect("third event").expect("ok");
		assert!(matches!(ev3, Event::MessageEnd { ref message, .. } if message.content == "start"));
		let ev4 = stream.next().expect("fourth event").expect("ok");
		assert!(matches!(ev4, Event::TurnEnd { ref stop_reason, .. } if stop_reason == "aborted"));

		let err = stream.next().expect("error event").err().expect("is error");
		assert!(err.to_string().contains(crate::deadline::TIMEOUT_MESSAGE));
	}

	#[test]
	fn stream_cancels_and_aborts_on_consumer_drop() {
		use std::io::{Read, Write};

		let (addr_tx, addr_rx) = std::sync::mpsc::channel::<std::net::SocketAddr>();

		let server = std::thread::spawn(move || {
			let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
			addr_tx.send(listener.local_addr().unwrap()).unwrap();
			let (mut socket, _) = listener.accept().unwrap();
			let mut buf = [0u8; 1024];
			let _ = socket.read(&mut buf).unwrap();
			socket
				.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n")
				.unwrap();
			socket
				.write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"},\"finish_reason\":null}]}\n\n")
				.unwrap();
			socket.flush().unwrap();
			std::thread::sleep(std::time::Duration::from_secs(5));
		});

		let addr = addr_rx.recv().unwrap();
		let mut req = request(yolo());
		req.config
			.providers
			.insert("local".to_string(), ProviderConfig {
				base_url: Some(format!("http://{addr}/v1")),
				api_key: Some("sk-test".to_string()),
				..ProviderConfig::default()
			});

		let runner = PiAgentRunner::new().expect("runner");
		let mut stream = runner.run(&req).expect("stream");
		let ev1 = stream.next().expect("first event").expect("ok");
		assert_eq!(ev1, Event::MessageStart { role: "assistant".to_string() });

		let start = std::time::Instant::now();
		drop(stream);
		assert!(start.elapsed() < std::time::Duration::from_millis(500));
		drop(server);
	}

	#[test]
	fn consumer_cancellation_token_interrupts_running_stream() {
		use std::io::{Read, Write};

		let (addr_tx, addr_rx) = std::sync::mpsc::channel::<std::net::SocketAddr>();

		std::thread::spawn(move || {
			let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
			addr_tx.send(listener.local_addr().unwrap()).unwrap();
			let (mut socket, _) = listener.accept().unwrap();
			let mut buf = [0u8; 1024];
			let _ = socket.read(&mut buf).unwrap();
			socket
				.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n")
				.unwrap();
			socket
				.write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"},\"finish_reason\":null}]}\n\n")
				.unwrap();
			socket.flush().unwrap();
			std::thread::sleep(std::time::Duration::from_secs(5));
		});

		let addr = addr_rx.recv().unwrap();
		let mut req = request(yolo());
		req.config
			.providers
			.insert("local".to_string(), ProviderConfig {
				base_url: Some(format!("http://{addr}/v1")),
				api_key: Some("sk-test".to_string()),
				..ProviderConfig::default()
			});
		let cancel_token = pi_agent::CancellationToken::new();
		req.cancellation_token = Some(cancel_token.clone());

		let runner = PiAgentRunner::new().expect("runner");
		let mut stream = runner.run(&req).expect("stream");
		let ev1 = stream.next().expect("first event").expect("ok");
		assert_eq!(ev1, Event::MessageStart { role: "assistant".to_string() });
		let ev2 = stream.next().expect("second event").expect("ok");
		assert_eq!(ev2, Event::MessageUpdate { delta: "hello".to_string() });

		cancel_token.cancel();

		let ev3 = stream.next().expect("third event").expect("ok");
		assert!(matches!(ev3, Event::MessageEnd { ref message, .. } if message.content == "hello"));
		let ev4 = stream.next().expect("fourth event").expect("ok");
		assert!(matches!(ev4, Event::TurnEnd { ref stop_reason, .. } if stop_reason == "aborted"));

		let err = stream.next().expect("error event").err().expect("is error");
		assert!(err.to_string().contains("interrupted"));
	}
	#[test]
	fn timer_guard_aborts_on_early_discovery_error() {
		let mut req = request(yolo());
		req.max_time = Some(std::time::Duration::from_millis(50));
		let cancel_token = pi_agent::CancellationToken::new();
		req.cancellation_token = Some(cancel_token.clone());
		req.config.tools = Some(vec!["unknown_tool_for_test".to_string()]);

		let runner = PiAgentRunner::new().expect("runner");
		let res = runner.run(&req);
		assert!(res.is_err());

		std::thread::sleep(std::time::Duration::from_millis(100));
		assert!(!cancel_token.is_cancelled(), "orphaned timer must not cancel caller token");
	}

	#[test]
	fn timer_guard_aborts_on_provider_setup_error() {
		let mut req = request(yolo());
		req.max_time = Some(std::time::Duration::from_millis(50));
		let cancel_token = pi_agent::CancellationToken::new();
		req.cancellation_token = Some(cancel_token.clone());
		req.config.providers.insert(
			"local".to_string(),
			ProviderConfig {
				base_url: Some("invalid url with space and :::".to_string()),
				api_key: Some("sk-test".to_string()),
				..ProviderConfig::default()
			},
		);

		let runner = PiAgentRunner::new().expect("runner");
		let res = runner.run(&req);
		assert!(res.is_err());

		std::thread::sleep(std::time::Duration::from_millis(100));
		assert!(!cancel_token.is_cancelled(), "orphaned timer must not cancel caller token");
	}

	#[tokio::test]
	async fn run_plan_fails_closed_for_explicit_text_only_catalog_model() {
		let mut cat = catalog_with_connection("local", "http://127.0.0.1:8080/v1", Some("sk-test"));
		let mut models = BTreeMap::new();
		models.insert("text-only".to_string(), ModelInfo {
			provider:       "local".to_string(),
			context_window: Some(4096),
			input:          Some(vec!["text".to_string()]),
		});
		cat.providers.insert("local".to_string(), models);

		let mut req = request(Approvals::default());
		req.catalog = Arc::new(cat);
		req.model = Some("local/text-only".to_string());

		let plan = RunPlan::resolve(&req).expect("resolve plan");
		assert!(!plan.supports_images, "explicit text-only model must fail closed (supports_images: false)");

		let provider = OpenAiCompatibleProvider::new(plan.base_url, plan.api_key, plan.model)
			.expect("provider build")
			.with_images(plan.supports_images);

		let msg = pi_agent::Message::user_with_images(
			"look at this",
			vec![pi_agent::ImagePart::new("image/png", "aGVsbG8=")],
		);
		let err = provider.complete(&[msg], &[]).await.unwrap_err();
		assert!(
			matches!(err, pi_agent::ProviderError::UnsupportedImage(_)),
			"provider must reject image message when model is text-only, got: {:?}",
			err
		);
	}

	#[test]
	fn run_plan_enables_images_for_multimodal_catalog_model() {
		let mut cat = catalog_with_connection("local", "http://127.0.0.1:8080/v1", Some("sk-test"));
		let mut models = BTreeMap::new();
		models.insert("vision-model".to_string(), ModelInfo {
			provider:       "local".to_string(),
			context_window: Some(4096),
			input:          Some(vec!["text".to_string(), "image".to_string()]),
		});
		cat.providers.insert("local".to_string(), models);

		let mut req = request(Approvals::default());
		req.catalog = Arc::new(cat);
		req.model = Some("local/vision-model".to_string());

		let plan = RunPlan::resolve(&req).expect("resolve plan");
		assert!(plan.supports_images, "multimodal model declaring 'image' must have supports_images: true");
	}

	#[test]
	fn run_plan_permits_images_for_unknown_catalog_model_capabilities() {
		let mut cat = catalog_with_connection("local", "http://127.0.0.1:8080/v1", Some("sk-test"));
		let mut models = BTreeMap::new();
		models.insert("unspecified".to_string(), ModelInfo {
			provider:       "local".to_string(),
			context_window: Some(4096),
			input:          None,
		});
		cat.providers.insert("local".to_string(), models);

		let mut req = request(Approvals::default());
		req.catalog = Arc::new(cat);
		req.model = Some("local/unspecified".to_string());

		let plan = RunPlan::resolve(&req).expect("resolve plan");
		assert!(
			plan.supports_images,
			"unspecified model input capabilities default to permissive true for gateway compatibility"
		);
	}
}
