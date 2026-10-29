//! Long-lived RPC session: model/config state, transcript, and turn pump.
//!
//! The session mirrors `AgentSession` as far as the native backend allows and
//! owns everything the RPC transport needs between commands. It is deliberately
//! independent of the wire transport (frames go through [`FrameSink`]) and of
//! process-wide state, so another transport (ACP) can drive the same session.
//!
//! Native gaps are never papered over. A feature whose backend is missing
//! returns a precise error naming the seam (see `[RpcError]`); nothing reports
//! success for work that did not happen, and no token usage is invented.

use std::{
	collections::VecDeque,
	path::{Path, PathBuf},
	sync::{Arc, Mutex, MutexGuard},
	time::{Duration, Instant, SystemTime},
};

use pi_agent::{
	CancellationToken, TurnAck,
	state::{ContentPart as AgentContentPart, Message},
	supervisor::SubagentObserver,
	tools::{
		internal_urls::{InternalUrlRouter, UrlScope},
		task_catalog::EFFORT_SELECTORS,
		todo::TodoPhase,
	},
};
use serde_json::{Value, json};

use crate::{
	Error, Result,
	agent::{AgentRunner, Event as AgentEvent, RunRequest},
	approve::{ApprovalMode, Approvals},
	catalog::Catalog,
	config::Config,
	output::{self, OutputMode},
	prompts::Prompts,
	runtime,
	session::{
		self, ContentPart, MessageContent, MessageEntry, SessionMessage, SessionOptions,
		SessionStore, TurnBuilder, TurnRecord,
	},
	session_checkpoint::CheckpointHost,
	skills::Skill,
};

use super::{
	host_tools::HostToolBridge,
	host_uris::HostUriBridge,
	types::{
		InterruptMode, QueueMode, RpcImage, RpcResponse, RpcSessionState,
		SubagentSubscriptionLevel,
	},
};

/// A precise prerequisite error: the native backend for this operation is absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prerequisite {
	pub code: &'static str,
	pub message: String,
}

impl Prerequisite {
	pub(super) fn new(code: &'static str, message: impl Into<String>) -> Self {
		Self { code, message: message.into() }
	}
}

/// Frame destination. Implemented by the JSONL writer and by other transports.
pub trait FrameSink: Send + Sync {
	fn send(&self, frame: Value);

	/// Send a response error that could not be produced synchronously.
	fn send_response(&self, response: RpcResponse) {
		self.send(response.into_value());
	}
}

/// One steering message carrying the exact user parts the client sent.
///
/// The native steering channel injects a message into the live conversation, so
/// images travel with it instead of being rejected. The wiring is
/// `RpcLaunch.steer` -> `PiAgentRunner::steer(Vec<pi_agent::ContentPart>)` ->
/// `SignalDispatcher` -> `SteeringEvent::InjectMessage(Vec<ContentPart>)`; the
/// parts are rebuilt by the CLI entry point, which is the only place that may
/// know both types.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SteeringMessage {
	pub text:   String,
	pub images: Vec<RpcImage>,
}

/// Steer hook supplied by the runner owner once `PiAgentRunner::steer` exists.
pub type SteerFn = Arc<dyn Fn(&SteeringMessage) -> bool + Send + Sync>;

/// Everything one RPC process needs that the CLI entry point has already resolved.
///
/// The approval surface is a *recipe*, not a built [`Approvals`]: [`RpcMode`]
/// constructs the [`ExtensionUiBridge`] first and installs it as both the
/// approval input and the `ask` channel before the session opens, so a headless
/// host answers permission prompts over `extension_ui_request` and never reads
/// the process terminal.
///
/// [`RpcMode`]: super::mode::RpcMode
/// [`ExtensionUiBridge`]: super::input::ExtensionUiBridge
pub struct RpcLaunch {
	pub runner:   Arc<dyn AgentRunner>,
	pub config:   Config,
	pub cwd:      PathBuf,
	pub prompts:  Prompts,
	pub skills:   Vec<Skill>,
	pub catalog:  Arc<Catalog>,
	/// Resolved credential-store path (`<agent dir>/agent.db`), resolved once by
	/// the CLI so no hidden default can pick a different store here.
	pub agent_db: PathBuf,
	pub approval_mode: ApprovalMode,
	pub allow:    Vec<String>,
	pub model:    Option<String>,
	pub provider: Option<String>,
	pub api_key:  Option<String>,
	pub thinking: Option<runtime::CliThinking>,
	pub max_time: Option<Duration>,
	pub max_turns: usize,
	pub tools_enabled: bool,
	pub prompt_cache_key: Option<String>,
	pub provider_session_id: Option<String>,
	pub store_options: SessionOptions,
	/// Present when the host installed a live steering channel.
	pub steer: Option<SteerFn>,
}

/// Cancellation and steering surface for the turn currently running.
#[derive(Default)]
pub struct ActiveTurn {
	inner: Mutex<ActiveState>,
}

#[derive(Default)]
struct ActiveState {
	/// Run that currently owns this surface. A worker from an older run must not
	/// clear the steering hook of the run that replaced it.
	generation:  u64,
	cancel:      Option<CancellationToken>,
	steer:       Option<SteerFn>,
	bash:        std::collections::BTreeMap<u64, CancellationToken>,
	next_bash_id: u64,
}

impl ActiveTurn {
	#[must_use]
	pub fn steer_hook(&self) -> Option<SteerFn> {
		self.lock().steer.clone()
	}

	/// Inject a steering message (text and images) into the live turn.
	pub fn steer(&self, message: &SteeringMessage) -> std::result::Result<(), Prerequisite> {
		let guard = self.lock();
		if guard.cancel.is_none() {
			return Err(Prerequisite::new(
				"not_streaming",
				"cannot steer: no turn is streaming",
			));
		}
		let Some(hook) = guard.steer.as_ref() else {
			return Err(Prerequisite::new(
				"steer_unavailable",
				"steering is unavailable: this host installed no steering channel for the session",
			));
		};
		if hook(message) {
			Ok(())
		} else {
			Err(Prerequisite::new(
				"steer_unavailable",
				"steering is unavailable: the live turn closed its steering channel",
			))
		}
	}

	pub fn register_bash(&self) -> (u64, CancellationToken) {
		let token = CancellationToken::new();
		let mut guard = self.lock();
		guard.next_bash_id += 1;
		let id = guard.next_bash_id;
		guard.bash.insert(id, token.clone());
		(id, token)
	}

	pub fn clear_bash(&self, id: u64) {
		self.lock().bash.remove(&id);
	}

	/// Cancel every live bash run; a second concurrent run must not make the
	/// first uncancellable.
	#[must_use]
	pub fn abort_bash(&self) -> bool {
		let guard = self.lock();
		let mut cancelled = false;
		for token in guard.bash.values() {
			token.cancel();
			cancelled = true;
		}
		cancelled
	}

	fn lock(&self) -> MutexGuard<'_, ActiveState> {
		self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
	}

	pub(super) fn begin(
		&self,
		generation: u64,
		token: CancellationToken,
		steer: Option<SteerFn>,
	) {
		let mut guard = self.lock();
		guard.generation = generation;
		guard.cancel = Some(token);
		guard.steer = steer;
	}

	/// Release the surface only for the run that owns it; a superseded worker's
	/// `end` is a no-op so it cannot wipe the new run's steering hook.
	#[must_use]
	pub(super) fn end_if(&self, generation: u64) -> bool {
		let mut guard = self.lock();
		if guard.generation != generation {
			return false;
		}
		guard.cancel = None;
		guard.steer = None;
		true
	}
}

/// One user message entering a turn.
#[derive(Debug, Clone)]
pub struct TurnInput {
	pub text:    String,
	pub images:  Vec<RpcImage>,
}

/// Long-lived session state owned by the RPC transport.
pub struct RpcSession {
	pub(crate) runner:   Arc<dyn AgentRunner>,
	/// Credential store shared with the RPC auth bridge; every request this
	/// session builds resolves keys through it.
	pub(crate) resolver: Arc<dyn crate::rpc::auth::CredentialResolver>,
	pub(crate) config:   Config,
	pub(crate) cwd:      PathBuf,
	pub(crate) prompts:  Prompts,
	pub(crate) skills:   Vec<Skill>,
	pub(crate) catalog:  Arc<Catalog>,
	pub(crate) approvals: Approvals,
	pub(crate) store:    SessionStore,
	pub(crate) store_options: SessionOptions,
	pub(crate) history:  Vec<TurnRecord>,
	/// Exact provider-ready context override (after compaction or rewind).
	pub(crate) history_messages: Option<Vec<Message>>,
	/// Live conversation in wire form (`AgentMessage` JSON), oldest first.
	pub(crate) messages: Vec<Value>,
	pub(crate) model:    Option<String>,
	pub(crate) provider: Option<String>,
	pub(crate) api_key:  Option<String>,
	pub(crate) thinking: Option<runtime::CliThinking>,
	pub(crate) thinking_level: Option<String>,
	pub(crate) prompt_cache_key: Option<String>,
	pub(crate) provider_session_id: Option<String>,
	pub(crate) max_time: Option<Duration>,
	pub(crate) max_turns: usize,
	pub(crate) tools_enabled: bool,
	pub(crate) checkpoint_host: CheckpointHost,
	/// `--no-session` mirror of the live branch (no leading system message).
	pub(crate) in_memory_messages: Option<Vec<Message>>,
	pub(crate) is_streaming: bool,
	pub(crate) is_compacting: bool,
	pub(crate) auto_compaction: bool,
	pub(crate) auto_retry: bool,
	pub(crate) retry_abort: Option<CancellationToken>,
	pub(crate) steering_mode: QueueMode,
	pub(crate) follow_up_mode: QueueMode,
	pub(crate) interrupt_mode: InterruptMode,
	pub(crate) queued: VecDeque<TurnInput>,
	pub(crate) todo_phases: Vec<TodoPhase>,
	pub(crate) session_name: Option<String>,
	/// Per-provider-family priority selection (TS `ServiceTierByFamily`).
	pub(crate) priority_families: std::collections::BTreeMap<String, bool>,
	pub(crate) last_assistant_text: Option<String>,
	pub(crate) tokens_per_second: Option<f64>,
	pub(crate) steer: Option<SteerFn>,
	pub(crate) host_tools: Arc<HostToolBridge>,
	pub(crate) host_uris: Arc<HostUriBridge>,
	pub(crate) subagent_level: SubagentSubscriptionLevel,
	pub(crate) protocol_version: u8,
	/// One router for every turn: the host URI bridge mounts its validated
	/// schemes here, and the runner hands the same handle to root and child
	/// `read`/`write` tools.
	pub(crate) internal_urls: Arc<InternalUrlRouter>,
	/// Observer fed to `RunRequest.subagent_observer` so the runner subscribes
	/// the registry to whichever supervisor it builds.
	pub(crate) observer: Arc<dyn SubagentObserver>,
	/// Session goal/budget controller (Goal Mode accounting + `+Nk` window).
	pub(crate) goal:     Arc<crate::goal::GoalRuntime>,
}

impl RpcSession {
	/// Open the session described by the launch inputs.
	pub fn open(
		launch: RpcLaunch,
		approvals: Approvals,
		host_tools: Arc<HostToolBridge>,
		host_uris: Arc<HostUriBridge>,
		internal_urls: Arc<InternalUrlRouter>,
		observer: Arc<dyn SubagentObserver>,
		resolver: Arc<dyn crate::rpc::auth::CredentialResolver>,
	) -> Result<Self> {
		let RpcLaunch {
			runner,
			config,
			cwd,
			prompts,
			skills,
			catalog,
			agent_db: _,
			approval_mode: _,
			allow: _,
			model,
			provider,
			api_key,
			thinking,
			max_time,
			max_turns,
			tools_enabled,
			prompt_cache_key,
			provider_session_id,
			store_options,
			steer,
		} = launch;

		let mut store = SessionStore::open(&store_options);
		for warning in store.take_warnings() {
			eprintln!("warning: {warning}");
		}
		if store.is_v1() {
			return Err(Error::new(
				"cannot run RPC on a legacy v1 session log: read-only; use import_legacy_v1",
			));
		}
		let thinking_level = thinking.and_then(|level| match level {
			runtime::CliThinking::Off => Some("off".to_string()),
			runtime::CliThinking::Auto => None,
			runtime::CliThinking::Effort(effort) => Some(effort.to_string()),
		});
		let history = if store.has_branch_context() {
			store.branch_history().map_err(Error::new)?
		} else {
			store.history().to_vec()
		};
		let messages = if store.enabled() {
			branch_wire_messages(&store)
		} else {
			Vec::new()
		};
		let session_name = store
			.title_slot()
			.map(|slot| slot.title.clone())
			.or_else(|| store.header().and_then(|header| header.title.clone()));
		let last_assistant_text = history
			.last()
			.map(|turn| turn.assistant.clone())
			.filter(|text| !text.is_empty());
		let mut checkpoint_host = CheckpointHost::new();
		checkpoint_host.rehydrate(&store);
		let todo_phases = store.latest_todo_phases();
		let mut session = Self {
			runner,
			resolver,
			config,
			cwd,
			prompts,
			skills,
			catalog,
			approvals,
			store,
			store_options,
			history,
			history_messages: None,
			messages,
			model,
			provider,
			api_key,
			thinking,
			thinking_level,
			prompt_cache_key,
			provider_session_id,
			max_time,
			max_turns: max_turns.max(1),
			tools_enabled,
			checkpoint_host,
			in_memory_messages: None,
			is_streaming: false,
			is_compacting: false,
			auto_compaction: false,
			auto_retry: false,
			retry_abort: None,
			steering_mode: QueueMode::All,
			follow_up_mode: QueueMode::OneAtATime,
			interrupt_mode: InterruptMode::Immediate,
			queued: VecDeque::new(),
			todo_phases,
			session_name,
			priority_families: std::collections::BTreeMap::new(),
			last_assistant_text,
			tokens_per_second: None,
			steer,
			host_tools,
			host_uris,
			subagent_level: SubagentSubscriptionLevel::Off,
			protocol_version: 1,
			internal_urls,
			observer,
			goal: Arc::new(crate::goal::GoalRuntime::new()),
		};
		session.reconcile_goal_mode();
		Ok(session)
	}

	// ---------------------------------------------------------------- state --

	/// Model selector currently in effect (`provider/model[:effort]` form).
	#[must_use]
	pub fn model_selector(&self) -> Option<String> {
		let model = self.model.as_deref()?;
		match self.thinking {
			Some(level) => Some(runtime::apply_thinking(model, level)),
			None => Some(model.to_string()),
		}
	}

	/// Wire `Model` object for the active model, built from catalog data only.
	#[must_use]
	pub fn model_object(&self) -> Option<Value> {
		let raw = self.model.as_deref()?;
		let (base, _) = runtime::split_effort(raw);
		let provider = self
			.provider
			.clone()
			.or_else(|| self.catalog.provider_of(base).map(str::to_string))?;
		let info = self.catalog.providers.get(&provider).and_then(|models| models.get(base));
		let connection = self.catalog.connections.get(&provider);
		let mut model = json!({
			"id": base,
			"name": info.and_then(|i| i.name.clone()).unwrap_or_else(|| base.to_string()),
			"api": connection.and_then(|c| c.api.clone()).unwrap_or_default(),
			"provider": provider,
			"baseUrl": connection
				.and_then(|c| c.base_url.clone())
				.unwrap_or_default(),
			"reasoning": info.is_some_and(|i| i.reasoning),
			"input": info
				.and_then(|i| i.input.clone())
				.unwrap_or_else(|| vec!["text".to_string()]),
			"cost": info
				.and_then(|i| i.cost.clone())
				.unwrap_or_else(|| json!({})),
			"contextWindow": info.and_then(|i| i.context_window).unwrap_or(0),
			"maxTokens": info.and_then(|i| i.max_tokens).unwrap_or(0),
		});
		if let Some(efforts) = info.and_then(|i| i.efforts.clone()) {
			model["thinking"] = json!({ "efforts": efforts });
		}
		Some(model)
	}

	/// All catalog models in wire form, sorted by provider then id.
	#[must_use]
	pub fn available_models(&self) -> Vec<Value> {
		let mut models = Vec::new();
		for (provider, entries) in &self.catalog.providers {
			for id in entries.keys() {
				let connection = self.catalog.connections.get(provider);
				let info = entries.get(id).expect("entry exists");
				let mut model = json!({
					"id": id,
					"name": info.name.clone().unwrap_or_else(|| id.clone()),
					"api": connection.and_then(|c| c.api.clone()).unwrap_or_default(),
					"provider": provider,
					"baseUrl": connection.and_then(|c| c.base_url.clone()).unwrap_or_default(),
					"reasoning": info.reasoning,
					"input": info.input.clone().unwrap_or_else(|| vec!["text".to_string()]),
					"cost": info.cost.clone().unwrap_or_else(|| json!({})),
					"contextWindow": info.context_window.unwrap_or(0),
					"maxTokens": info.max_tokens.unwrap_or(0),
				});
				if let Some(efforts) = &info.efforts {
					model["thinking"] = json!({ "efforts": efforts });
				}
				models.push(model);
			}
		}
		models
	}

	/// Efforts the active model advertises, or `None` when it has no thinking
	/// surface at all.
	#[must_use]
	pub fn available_efforts(&self) -> Option<Vec<String>> {
		let raw = self.model.as_deref()?;
		let (base, _) = runtime::split_effort(raw);
		let provider = self
			.provider
			.clone()
			.or_else(|| self.catalog.provider_of(base).map(str::to_string))?;
		self.catalog
			.providers
			.get(&provider)
			.and_then(|models| models.get(base))
			.and_then(|info| info.efforts.clone())
	}

	/// `get_state` payload.
	pub fn state(&self, system_prompt: Option<Vec<String>>) -> RpcSessionState {
		RpcSessionState {
			model: self.model_object(),
			thinking_level: self.thinking_level.clone(),
			is_streaming: self.is_streaming,
			is_compacting: self.is_compacting,
			steering_mode: self.steering_mode.as_str(),
			follow_up_mode: self.follow_up_mode.as_str(),
			interrupt_mode: self.interrupt_mode.as_str(),
			session_file: self.store_file_string(),
			session_id: self.session_id(),
			session_name: self.session_name.clone(),
			auto_compaction_enabled: self.auto_compaction,
			queued_message_count: self.queued.len(),
			todo_phases: serde_json::to_value(&self.todo_phases).unwrap_or_else(|_| json!([])),
			fast_mode_enabled: self.fast_mode_enabled(),
			tokens_per_second: self.tokens_per_second,
			fast_mode_active: self.fast_mode_active(),
			message_count: self.messages.len(),
			system_prompt,
			dump_tools: self.dump_tools(),
			context_usage: self.context_usage(),
		}
	}

	/// `dumpTools`: registry schemas in the shape clients display.
	///
	/// The native registry exposes OpenAI `tools` entries, so each one is
	/// unwrapped to `{name, description, parameters}`. `examples` is absent
	/// because native tools carry none.
	fn dump_tools(&self) -> Option<Vec<Value>> {
		if !self.tools_enabled {
			return Some(Vec::new());
		}
		let registry = runtime::build_registry(&[]).ok()?;
		Some(
			registry
				.schemas()
				.into_iter()
				.filter_map(|schema| {
					let function = schema.get("function")?;
					Some(json!({
						"name": function.get("name")?.clone(),
						"description": function.get("description")?.clone(),
						"parameters": function.get("parameters")?.clone(),
					}))
				})
				.collect(),
		)
	}

	/// Context occupancy of the newest provider request on the active branch.
	///
	/// Reads the persisted prompt-side counters (`input + cacheRead +
	/// cacheWrite`, `totalTokens` fallback) through
	/// [`SessionStore::latest_context_tokens`]; it never sums events, so a turn
	/// with several provider requests cannot double-count. `None` means no
	/// request on the branch reported usage.
	#[must_use]
	pub fn context_usage(&self) -> Option<Value> {
		let tokens = match self.store.latest_context_tokens() {
			Some(tokens) => tokens,
			// `--no-session` never appends to the store, so read the last
			// assistant usage mirrored from the loop: the reported occupancy,
			// never a zero stand-in.
			None => self.in_memory_context_tokens()?,
		};
		let limit = self.context_limit()?;
		Some(json!({
			"tokens": tokens,
			"contextWindow": limit,
			"percent": if limit == 0 {
				0.0
			} else {
				(tokens as f64 / limit as f64) * 100.0
			},
		}))
	}

	/// Occupancy of the newest reported request in the in-memory mirror.
	///
	/// Same formula as [`SessionStore::latest_context_tokens`] (`contextTokens`,
	/// then the prompt counters, then `totalTokens`), minus the provider-side
	/// orchestration subtraction that only TS-written logs carry. `None` when no
	/// mirrored assistant message reported usage.
	fn in_memory_context_tokens(&self) -> Option<u64> {
		self
			.in_memory_messages
			.as_ref()?
			.iter()
			.rev()
			.filter(|message| message.role == pi_agent::Role::Assistant)
			.find_map(|message| message.usage.as_ref())
			.map(|usage| {
				let usage = session::Usage::from(*usage);
				if let Some(context) = usage.context_tokens {
					return context;
				}
				let prompt = usage.input + usage.cache_read + usage.cache_write;
				if prompt > 0 {
					return prompt;
				}
				let total = usage.total_tokens;
				if total > 0 {
					total
				} else {
					usage.input + usage.output + usage.cache_read + usage.cache_write
				}
			})
	}

	fn context_limit(&self) -> Option<usize> {
		let raw = self.model.as_deref()?;
		let (base, _) = runtime::split_effort(raw);
		let provider = self
			.provider
			.clone()
			.or_else(|| self.catalog.provider_of(base).map(str::to_string))?;
		self.catalog.context_window(&provider, base)
	}

	#[must_use]
	pub fn session_id(&self) -> String {
		if self.store.enabled() {
			self.store.id().to_string()
		} else {
			self.store_options
				.id
				.clone()
				.unwrap_or_else(|| "in-memory".to_string())
		}
	}

	#[must_use]
	pub fn store_file_string(&self) -> Option<String> {
		self.store.file().map(|path| path.display().to_string())
	}

	/// `get_session_stats` payload. Token totals come from the store's
	/// cumulative usage index (one contribution per persisted assistant entry);
	/// reasoning/cost/premium figures are read from the same persisted usage
	/// objects without touching the token totals.
	#[must_use]
	pub fn stats(&self) -> Value {
		let usage = self.store.session_usage();
		let mut reasoning = 0u64;
		let mut cost = 0.0f64;
		let mut premium_requests = 0u64;
		for entry in self.store.messages() {
			if entry.message.role != "assistant" {
				continue;
			}
			let Some(usage) = &entry.message.usage else {
				continue;
			};
			reasoning = reasoning.saturating_add(usage.reasoning_tokens.unwrap_or(0));
			if let Some(entry_cost) = &usage.cost {
				cost += entry_cost.total;
			}
			premium_requests = premium_requests.saturating_add(
				usage
					.extra
					.get("premiumRequests")
					.and_then(Value::as_u64)
					.unwrap_or(0),
			);
		}
		let mut stats = json!({
			"sessionId": self.session_id(),
			"userMessages": self
				.messages
				.iter()
				.filter(|message| message.get("role").and_then(Value::as_str) == Some("user"))
				.count(),
			"assistantMessages": self
				.messages
				.iter()
				.filter(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
				.count(),
			"toolCalls": self
				.messages
				.iter()
				.filter_map(|message| message.get("content").and_then(Value::as_array))
				.flatten()
				.filter(|part| part.get("type").and_then(Value::as_str) == Some("toolCall"))
				.count(),
			"toolResults": self
				.messages
				.iter()
				.filter(|message| message.get("role").and_then(Value::as_str) == Some("toolResult"))
				.count(),
			"totalMessages": self.messages.len(),
			"tokens": {
				"input": usage.input,
				"output": usage.output,
				"reasoning": reasoning,
				"cacheRead": usage.cache_read,
				"cacheWrite": usage.cache_write,
				"total": usage.total_tokens,
			},
			"premiumRequests": premium_requests,
			"cost": cost,
		});
		if let Some(file) = self.store_file_string() {
			stats["sessionFile"] = json!(file);
		}
		if let Some(context) = self.context_usage() {
			stats["contextUsage"] = context;
		}
		stats
	}

	// ------------------------------------------------------------ fast mode --

	/// Provider family whose service-tier knob governs the active model, or
	/// `None` when the model exposes no serving-priority control.
	///
	/// Port of TS `serviceTierFamily` (`packages/ai/src/types.ts`): OpenRouter
	/// ids are classified by namespace, `anthropic-messages` models belong to
	/// the Anthropic family on any provider, and custom OpenAI-compatible relays
	/// serving OpenAI ids are OpenAI family unless the provider owns a separate
	/// control (Fireworks) or rejects the field (GitHub Copilot).
	fn service_tier_family(&self) -> Option<&'static str> {
		let (provider, api, id) = self.active_model_parts()?;
		service_tier_family(&provider, api.as_deref(), &id)
	}

	/// Whether the user selected priority for the active model's family
	/// (TS `isFastModeEnabled`).
	#[must_use]
	pub fn fast_mode_enabled(&self) -> bool {
		self
			.service_tier_family()
			.is_some_and(|family| self.priority_families.get(family).copied().unwrap_or(false))
	}

	/// Whether priority will actually be realized on the wire for the active
	/// model (TS `isFastModeActive`). Until the runner forwards
	/// `RunRequest.service_tier`, the selection is stored but not realized, so
	/// this stays `false` instead of claiming a wire effect that did not happen.
	#[must_use]
	pub fn fast_mode_active(&self) -> bool {
		let Some((provider, api, id)) = self.active_model_parts() else {
			return false;
		};
		self.fast_mode_enabled() && realizes_priority(&provider, api.as_deref(), &id)
	}

	/// Apply `set_fast_mode`; returns whether the model has a service-tier knob
	/// (TS `setFastMode` return value).
	pub fn set_fast_mode(&mut self, enabled: bool) -> bool {
		let Some(family) = self.service_tier_family() else {
			return false;
		};
		if enabled {
			self.priority_families.insert(family.to_string(), true);
		} else {
			self.priority_families.remove(family);
		}
		true
	}

	/// Provider, wire api and bare model id of the active selection.
	fn active_model_parts(&self) -> Option<(String, Option<String>, String)> {
		let raw = self.model.as_deref()?;
		let (base, _) = runtime::split_effort(raw);
		let provider = self
			.provider
			.clone()
			.or_else(|| self.catalog.provider_of(base).map(str::to_string))?;
		let api = self
			.catalog
			.connections
			.get(&provider)
			.and_then(|connection| connection.api.clone())
			.or_else(|| {
				self.config
					.providers
					.get(&provider)
					.and_then(|connection| connection.api.clone())
			});
		Some((provider, api, base.to_string()))
	}

	// --------------------------------------------------------------- models --

	/// Set the active model by provider id and model id.
	pub fn set_model(&mut self, provider: &str, model_id: &str) -> Result<()> {
		let Some(models) = self.catalog.providers.get(provider) else {
			return Err(Error::new(format!("Unknown provider: {provider}")));
		};
		if !models.contains_key(model_id) {
			return Err(Error::new(format!("Unknown model for provider {provider}: {model_id}")));
		}
		// An inherited prompt-cache key dies on a model switch, exactly like the
		// one-shot CLI path.
		let provider_changed = self.provider.as_deref() != Some(provider)
			|| self.model.as_deref() != Some(model_id);
		if provider_changed {
			self.prompt_cache_key = None;
		}
		self.model = Some(model_id.to_string());
		self.provider = Some(provider.to_string());
		self.api_key = self.config.resolve_api_key(provider);
		self.thinking_level = match self.thinking {
			Some(runtime::CliThinking::Effort(effort)) => Some(effort.to_string()),
			Some(runtime::CliThinking::Off) => Some("off".to_string()),
			Some(runtime::CliThinking::Auto) | None => None,
		};
		if self.available_efforts().is_none() {
			self.thinking = None;
			self.thinking_level = None;
		}
		Ok(())
	}

	/// Next model in catalog order (`cycle_model`).
	pub fn cycle_model(&mut self) -> Option<Value> {
		let models = self.available_models();
		if models.is_empty() {
			return None;
		}
		let current_id = self.model.clone();
		let current_provider = self.provider.clone();
		let position = models.iter().position(|model| {
			model.get("id").and_then(Value::as_str) == current_id.as_deref()
				&& model.get("provider").and_then(Value::as_str) == current_provider.as_deref()
		});
		let next = models
			.get(position.map_or(0, |index| (index + 1) % models.len()))
			.cloned()?;
		let provider = next.get("provider").and_then(Value::as_str)?.to_string();
		let id = next.get("id").and_then(Value::as_str)?.to_string();
		self.set_model(&provider, &id).ok()?;
		Some(json!({ "model": next, "thinkingLevel": self.thinking_level, "isScoped": false }))
	}

	/// Set an explicit thinking level. The level must be a valid selector and,
	/// when the model advertises efforts, one of them.
	pub fn set_thinking_level(&mut self, level: &str) -> Result<()> {
		let Some(parsed) = runtime::parse_cli_thinking(level) else {
			return Err(Error::new(format!(
				"Unknown thinking level: {level} (valid: {}, auto)",
				EFFORT_SELECTORS.join(", ")
			)));
		};
		if let runtime::CliThinking::Effort(effort) = parsed {
			if let Some(efforts) = self.available_efforts() {
				if !efforts.iter().any(|candidate| candidate == effort) {
					return Err(Error::new(format!(
						"Model does not support thinking level \"{effort}\" (valid: {})",
						efforts.join(", ")
					)));
				}
			}
		}
		self.thinking = Some(parsed);
		self.thinking_level = match parsed {
			runtime::CliThinking::Off => Some("off".to_string()),
			runtime::CliThinking::Auto => None,
			runtime::CliThinking::Effort(effort) => Some(effort.to_string()),
		};
		Ok(())
	}

	/// Cycle `off` -> advertised efforts -> `off`. `Value::Null` means the
	/// model advertises no levels (upstream answers `null`, not an error).
	pub fn cycle_thinking_level(&mut self) -> Result<Value> {
		let efforts = self.available_efforts().unwrap_or_default();
		if efforts.is_empty() {
			return Ok(Value::Null);
		}
		let mut levels = vec!["off".to_string()];
		levels.extend(efforts);
		let current = self.thinking_level.clone().unwrap_or_else(|| "off".to_string());
		let next = levels
			.iter()
			.position(|level| *level == current)
			.map_or(0, |index| (index + 1) % levels.len());
		let level = levels[next].clone();
		self.set_thinking_level(&level)?;
		Ok(json!({ "level": self.thinking_level }))
	}

	// ------------------------------------------------------------- sessions --

	/// Start a fresh session log, keeping the prompt inputs and route.
	pub fn new_session(&mut self, parent_session: Option<&str>) -> Result<()> {
		let mut options = self.store_options.clone();
		options.id = None;
		options.continue_latest = false;
		options.parent_session = parent_session.map(str::to_string);
		options.dir = self
			.store
			.file()
			.and_then(|path| path.parent().map(Path::to_path_buf))
			.or(options.dir);
		let mut store = SessionStore::open(&options);
		for warning in store.take_warnings() {
			eprintln!("warning: {warning}");
		}
		if store.is_v1() {
			return Err(Error::new("cannot start a new session from a legacy v1 log"));
		}
		// `parent_session` is persisted in `SessionHeader` at creation, so the
		// lineage survives a reload of the new log.
		self.store = store;
		self.history.clear();
		self.history_messages = None;
		self.messages.clear();
		self.in_memory_messages = None;
		self.todo_phases.clear();
		self.session_name = None;
		self.last_assistant_text = None;
		self.tokens_per_second = None;
		self.checkpoint_host = CheckpointHost::new();
		self.checkpoint_host.rehydrate(&self.store);
		Ok(())
	}

	/// Switch to an existing session log.
	pub fn switch_session(&mut self, path: &Path) -> Result<()> {
		if !path.is_file() {
			return Err(Error::new(format!("Session not found: {}", path.display())));
		}
		let mut options = self.store_options.clone();
		options.dir = path.parent().map(Path::to_path_buf);
		options.id = path
			.file_stem()
			.and_then(|stem| stem.to_str())
			.map(str::to_string);
		options.continue_latest = false;
		let mut store = SessionStore::open(&options);
		for warning in store.take_warnings() {
			eprintln!("warning: {warning}");
		}
		if store.is_v1() {
			return Err(Error::new("cannot switch to a legacy v1 session log: read-only"));
		}
		// Switching targets an existing log, so persistence was requested: a
		// store that failed to open must not silently run in memory where its
		// entries would be lost on exit.
		if !store.enabled() {
			return Err(Error::new(format!(
				"cannot switch to {}: the session log cannot be persisted",
				path.display()
			)));
		}
		self.history = if store.has_branch_context() {
			store.branch_history().map_err(Error::new)?
		} else {
			store.history().to_vec()
		};
		self.messages = branch_wire_messages(&store);
		self.session_name = store
			.title_slot()
			.map(|slot| slot.title.clone())
			.or_else(|| store.header().and_then(|header| header.title.clone()));
		self.todo_phases = store.latest_todo_phases();
		self.history_messages = None;
		self.in_memory_messages = None;
		self.last_assistant_text = self
			.history
			.last()
			.map(|turn| turn.assistant.clone())
			.filter(|text| !text.is_empty());
		self.store = store;
		self.checkpoint_host = CheckpointHost::new();
		self.checkpoint_host.rehydrate(&self.store);
		Ok(())
	}

	/// Rename the session: append the `title_change` entry and rewrite the
	/// 256-byte title slot so list views and resumes see the same name.
	pub fn set_session_name(&mut self, name: &str) -> Result<()> {
		let title = clean_title(name);
		if title.is_empty() {
			return Err(Error::new("Session name cannot be empty"));
		}
		let previous = self.session_name.clone();
		let now = SystemTime::now();
		let iso = output::iso8601(now);
		if self.store.enabled() {
			let mut extra = serde_json::Map::new();
			extra.insert("title".to_string(), json!(title));
			extra.insert("source".to_string(), json!("user"));
			if let Some(previous) = &previous {
				extra.insert("previousTitle".to_string(), json!(previous));
			}
			let entry = session::SessionEntry {
				kind: "title_change".to_string(),
				id: format!("entry-{}", output::session_id()),
				parent_id: self.store.leaf_id().map(str::to_string),
				timestamp: iso.clone(),
				extra: Value::Object(extra),
			};
			self.store.append_entry(entry).map_err(Error::new)?;
			if let Some(file) = self.store.file().map(Path::to_path_buf) {
				let slot = session::serialize_title_slot(&title, Some("user"), &iso);
				if let Ok(content) = std::fs::read_to_string(&file) {
					let overlay = session::overlay_title_slot(&content, &slot);
					if let Err(err) = session::overwrite_title_slot_rolled_back(&file, &overlay) {
						eprintln!("warning: cannot update session title slot: {err}");
					}
				}
			}
		}
		self.session_name = Some(title);
		Ok(())
	}

	/// Messages of the current branch, in wire form.
	#[must_use]
	pub fn wire_messages(&self) -> &[Value] {
		&self.messages
	}

	/// `get_branch_messages`: user messages with their entry ids.
	#[must_use]
	pub fn branch_messages(&self) -> Vec<Value> {
		let entries: Vec<&MessageEntry> = if self.store.has_branch_context() {
			self.store.branch_messages()
		} else {
			self.store.messages().iter().collect()
		};
		entries
			.iter()
			.filter(|entry| entry.message.role == "user")
			.map(|entry| {
				json!({
					"entryId": entry.id,
					"text": entry.message.content.text(),
				})
			})
			.collect()
	}

	/// `branch` selection result. The store owns the leaf mutation: a durable
	/// metadata marker moves the active branch to `entry_id` without editing or
	/// deleting entries, so the selection survives a reload.
	pub fn branch(&mut self, entry_id: &str) -> std::result::Result<Value, Prerequisite> {
		if !self.store.has_entry(entry_id) {
			return Err(Prerequisite::new(
				"unknown_entry",
				format!("Unknown session entry: {entry_id}"),
			));
		}
		let text = self
			.store
			.messages()
			.iter()
			.find(|entry| entry.id == entry_id)
			.map(|entry| entry.message.content.text())
			.unwrap_or_default();
		self.store.branch_to(entry_id).map_err(|error| {
			Prerequisite::new("branch_unavailable", format!("cannot branch to {entry_id}: {error}"))
		})?;
		// The branch changed under the same log: rebuild the context the next
		// request and the frame surface read from.
		self.history = if self.store.has_branch_context() {
			self.store.branch_history().map_err(|error| {
				Prerequisite::new("branch_unavailable", error)
			})?
		} else {
			self.store.history().to_vec()
		};
		self.history_messages = None;
		self.todo_phases = self.store.latest_todo_phases();
		self.last_assistant_text = self
			.history
			.last()
			.map(|turn| turn.assistant.clone())
			.filter(|text| !text.is_empty());
		self.messages = self
			.store
			.branch_messages()
			.into_iter()
			.filter_map(|entry| serde_json::to_value(&entry.message).ok())
			.collect();
		Ok(json!({ "text": text, "cancelled": false }))
	}

	// ---------------------------------------------------------------- todos --

	pub fn set_todos(&mut self, phases: Vec<TodoPhase>) {
		self.todo_phases = phases;
	}

	// --------------------------------------------------------------- prompt --

	/// Latest assistant text for `get_last_assistant_text`.
	#[must_use]
	pub fn last_assistant_text(&self) -> Option<&str> {
		self.last_assistant_text.as_deref()
	}

	/// Queue a follow-up message for after the current turn.
	pub fn queue_follow_up(&mut self, input: TurnInput) -> usize {
		self.queued.push_back(input);
		self.queued.len()
	}

	/// Take the next queued messages according to a queue mode.
	pub fn take_queued(&mut self, mode: QueueMode) -> Vec<TurnInput> {
		match mode {
			QueueMode::All => self.queued.drain(..).collect(),
			QueueMode::OneAtATime => self.queued.pop_front().into_iter().collect(),
		}
	}

	/// Convert request images to agent parts, rejecting malformed data and
	/// models that cannot take image input.
	fn image_parts(&self, images: &[RpcImage]) -> Result<Vec<ContentPart>> {
		let mut parts: Vec<ContentPart> = Vec::new();
		for image in images {
			match AgentContentPart::image(&image.data, &image.mime_type) {
				Ok(AgentContentPart::Image { data, mime_type }) => {
					parts.push(ContentPart::image(&data, &mime_type));
				},
				Ok(AgentContentPart::Text { .. }) => {
					return Err(Error::new("image content is required for prompt images"));
				},
				Err(err) => return Err(Error::new(format!("invalid image: {err}"))),
			}
		}
		if parts.is_empty() {
			return Ok(parts);
		}
		let Some(raw) = self.model.as_deref() else {
			return Err(Error::new("cannot send images without a selected model"));
		};
		let (base, _) = runtime::split_effort(raw);
		let provider = self
			.provider
			.clone()
			.or_else(|| self.catalog.provider_of(base).map(str::to_string))
			.unwrap_or_default();
		if self.catalog.supports_images(&provider, base) == Some(false) {
			return Err(Error::new(format!(
				"model '{provider}/{base}' does not support image input"
			)));
		}
		Ok(parts)
	}

	/// Validate steering images before they reach the live turn, so the host
	/// gets a protocol error instead of a silently dropped message.
	pub fn validate_steering_images(
		&self,
		images: &[RpcImage],
	) -> std::result::Result<(), Prerequisite> {
		self.image_parts(images).map(|_| ()).map_err(|err| {
			Prerequisite::new("invalid_steering_image", format!("cannot steer: {err}"))
		})
	}

	/// Refuse a prompt whose host tools cannot mount, before the user entry is
	/// journaled: a reserved or duplicated name fails the turn with the log
	/// untouched, instead of leaving an unanswered prompt behind.
	fn check_host_tool_names(&self) -> Result<()> {
		if !self.tools_enabled {
			return Ok(());
		}
		let reserved = runtime::reserved_tool_names();
		let mut seen = std::collections::HashSet::new();
		for tool in self.host_tools.adapters() {
			let name = tool.name().to_string();
			if reserved.iter().any(|known| known == &name) || !seen.insert(name.clone()) {
				return Err(Error::new(format!("Duplicate tool name: {name}")));
			}
		}
		Ok(())
	}

	/// Session usage at turn start: the goal accounting baseline and budget seed.
	fn goal_seed(&self) -> crate::goal::TokenCounters {
		let usage = self.store.session_usage();
		crate::goal::TokenCounters {
			input:       usage.input,
			output:      usage.output,
			cache_read:  usage.cache_read,
			cache_write: usage.cache_write,
		}
	}

	fn tool_selected(&self, name: &str) -> bool {
		self.tools_enabled
			&& self
				.config
				.tools
				.as_ref()
				.is_none_or(|allowed| allowed.iter().any(|tool| tool == name))
	}

	/// Headless registration rule: eager under `goal.enabled` unless the
	/// selection excludes it, and always while a goal exists (#9444).
	fn goal_tool_mounted(&self) -> bool {
		self.config.goal.enabled()
			&& self.tools_enabled
			&& (self.goal.has_goal() || self.tool_selected(crate::goal_tool::GOAL_TOOL_NAME))
	}

	/// Hidden per-turn goal context (TS `goal-mode-context`).
	fn goal_mode_context(&self) -> Option<String> {
		let active = self.goal.build_active_prompt()?;
		let todo = crate::goal::build_goal_todo_context(
			&self.todo_phases,
			self.tool_selected("todo"),
		);
		Some(crate::goal::render_goal_mode_context(&active, todo.as_deref()))
	}

	/// Drain queued goal writes through this session's single writer and return
	/// the live `goal_updated` frames in order.
	pub fn goal_frames(&mut self) -> Vec<Value> {
		for intent in self.goal.take_pending() {
			let result = match intent {
				crate::goal::GoalPersist::ModeChange { mode, data } => {
					self.store.append_mode_change(mode, data)
				},
				crate::goal::GoalPersist::Completed(data) => {
					self.store.append_goal_completed(data)
				},
			};
			if let Err(err) = result {
				eprintln!("warning: {err}");
			}
		}
		self
			.goal
			.take_events()
			.iter()
			.map(crate::goal::goal_updated_frame)
			.collect()
	}

	/// Complete-exit once the turn drained: clear the mode and write `none` plus
	/// the terminal metric exactly once.
	pub fn finish_goal_turn(&mut self) -> Vec<Value> {
		let mut frames = self.goal_frames();
		if let Some(goal) = self.goal.take_exiting() {
			if let Err(err) = self.store.append_mode_change(crate::goal::MODE_NONE, None) {
				eprintln!("warning: {err}");
			}
			if let Err(err) =
				self.store.append_goal_completed(crate::goal::encode_completed_data(&goal))
			{
				eprintln!("warning: {err}");
			}
			frames.extend(self.goal_frames());
		}
		frames
	}

	/// TS `#reconcileModeFromSession` over the active branch.
	pub fn reconcile_goal_mode(&mut self) -> Vec<Value> {
		match crate::goal::plan_restore(&self.store, self.config.goal.enabled()) {
			crate::goal::RestorePlan::Idle => self.goal.clear_accounting(),
			crate::goal::RestorePlan::ClearGoalMode => {
				self.goal.set_restored_state(None);
				self.goal.clear_accounting();
				if let Err(err) = self.store.append_mode_change(crate::goal::MODE_NONE, None) {
					eprintln!("warning: {err}");
				}
			},
			crate::goal::RestorePlan::Restore { enabled, goal } => {
				self.goal.set_restored_state(Some(crate::goal::GoalModeState {
					enabled,
					mode: crate::goal::GoalMode::Active,
					goal,
				}));
				// Replayed without an explicit preserve: an active goal pauses.
				let _ = self.goal.on_thread_resumed(false);
			},
		}
		self.goal_frames()
	}

	/// Build the provider request for a prompt, recording the user entry.
	pub fn prepare_turn(
		&mut self,
		input: &TurnInput,
	) -> Result<(RunRequest, PreparedTurn)> {
		if self.is_streaming {
			return Err(Error::new("a turn is already streaming"));
		}
		if self.store.is_v1() {
			return Err(Error::new(
				"cannot append to legacy v1 session log: read-only; use import_legacy_v1",
			));
		}

		let parts = self.image_parts(&input.images)?;
		self.check_host_tool_names()?;
		let started = SystemTime::now();
		let iso = output::iso8601(started);
		let started_ms = started
			.duration_since(SystemTime::UNIX_EPOCH)
			.unwrap_or_default()
			.as_millis() as u64;

		let (turn_parts, user_content) = if parts.is_empty() {
			(Vec::new(), MessageContent::Text(input.text.clone()))
		} else {
			let mut assembled = Vec::new();
			if !input.text.is_empty() {
				assembled.push(ContentPart::text(&input.text));
			}
			assembled.extend(parts.iter().cloned());
			(parts.clone(), MessageContent::Parts(assembled))
		};

		let user_id = format!("msg-{}", output::session_id());
		let parent_id = self.store.leaf_id().map(str::to_string);
		let message = SessionMessage::user(user_content, started_ms);
		let entry = MessageEntry::new(user_id.clone(), parent_id, &iso, message);

		let in_memory_before = self.in_memory_messages.as_ref().map_or(0, Vec::len);
		// TS `#createMagicKeywordNotices`: the user prompt opens the `+Nk` budget
		// window and the accounting baseline before the request is built.
		let seed = self.goal_seed();
		self.goal.begin_turn(Some(&input.text), seed);
		// Build the request BEFORE the user entry is appended: with a durable
		// `branch_summary` the context is rebuilt from the store, so an entry
		// written first would come back as history and the runner would append
		// the same prompt (text and images) a second time.
		let mut request = self.request(input.text.clone(), turn_parts)?;
		if self.store.enabled() {
			if let Err(err) = entry.validate() {
				eprintln!("warning: {err}");
			} else if let Err(err) = self.store.append_message(&entry) {
				eprintln!("warning: {err}");
			}
		}
		if let Ok(value) = serde_json::to_value(&entry.message) {
			self.messages.push(value);
		}
		if !self.store.enabled() {
			let user_message = runtime::user_message_from_parts(&input.text, &request.parts)
				.map_err(|err| Error::new(err.to_string()))?;
			self.in_memory_messages.get_or_insert_with(Vec::new).push(user_message);
		}
		// One token per turn: `StreamHandles::drop` cancels the request token when
		// the provider stream ends, so sharing it across turns would abort the
		// next queued turn. Run-wide stickiness lives in the RPC command loop's
		// abort flag, which owns the queue, not in this token.
		let token = CancellationToken::new();
		request.cancellation_token = Some(token.clone());
		let turn_ack = TurnAck::new();
		request.turn_ack = Some(Arc::clone(&turn_ack));

		let prepared = PreparedTurn {
			token,
			turn_ack,
			builder: if request.parts.is_empty() {
				TurnBuilder::new(input.text.clone())
			} else {
				TurnBuilder::new_with_parts(input.text.clone(), request.parts.clone())
			},
			parent_id: Some(user_id),
			in_memory_before,
			started: Instant::now(),
			model_fallback: self.model.clone(),
			provider_fallback: self.provider.clone(),
			tool_names: std::collections::HashMap::new(),
			tool_images: std::collections::HashMap::new(),
			wire_user_message: self.messages.last().cloned(),
			aborted: false,
		};
		self.is_streaming = true;
		Ok((request, prepared))
	}

	pub(crate) fn request(&self, prompt: String, parts: Vec<ContentPart>) -> Result<RunRequest> {
		let history_messages = if !self.store.enabled() {
			Some(self.in_memory_messages.clone().unwrap_or_default())
		} else if let Some(messages) = &self.history_messages {
			Some(messages.clone())
		} else if self.store.has_branch_context() {
			if let Some(warning) = self.store.branch_chain_warning() {
				eprintln!("warning: {warning}");
			}
			match CheckpointHost::context_messages(&self.store) {
				Ok(messages) => Some(messages),
				Err(err) => {
					eprintln!("warning: cannot rebuild branch context: {err}");
					None
				},
			}
		} else {
			None
		};
		let checkpoint_controller = self
			.config
			.checkpoint
			.enabled()
			.then(|| Arc::clone(self.checkpoint_host.controller()));
		let session_persisted = self.store.enabled() && !self.store.is_v1();
		let todo_phases = if session_persisted {
			self.store.latest_todo_phases()
		} else {
			runtime::latest_todo_phases_from_turns(&self.history)
		};
		let mut extra_tools: Vec<crate::agent::ExtraTool> = Vec::new();
		if self.goal_tool_mounted() {
			let tool: Arc<dyn pi_agent::tools::Tool> =
				Arc::new(crate::goal_tool::GoalTool::new(Arc::clone(&self.goal)));
			extra_tools.push(crate::agent::ExtraTool::from(tool));
		}
		// The live host tool set, mounted by the runner after every
		// builtin/ask/checkpoint/eval tool; empty when the host registered none.
		for tool in self.host_tools.adapters() {
			extra_tools.push(crate::agent::ExtraTool::from(tool));
		}
		let goal_context = self.goal_mode_context();
		Ok(RunRequest {
			prompt,
			parts,
			cwd: self.cwd.clone(),
			output_mode: OutputMode::Json,
			model: self.model_selector(),
			provider: self.provider.clone(),
			api_key: self.api_key.clone(),
			provider_session_id: self.provider_session_id.clone(),
			prompt_cache_key: self.prompt_cache_key.clone(),
			system_prompt: self.prompts.system.clone(),
			append_system_prompt: self.prompts.append.clone(),
			context_files: self.prompts.files.clone(),
			skills: self.skills.clone(),
			history: self.history.clone(),
			history_messages,
			checkpoint_controller,
			turn_ack: None,
			todo_phases,
			session_persisted,
			session_file: session_persisted
				.then(|| self.store.file().map(Path::to_path_buf))
				.flatten(),
			session_id: session_persisted.then(|| self.store.id().to_string()),
			config: self.config.clone(),
			catalog: Arc::clone(&self.catalog),
			approvals: self.approvals.clone(),
			max_time: self.max_time,
			max_turns: self.max_turns,
			tools_enabled: self.tools_enabled,
			cancellation_token: None,
			// Goal tool first, then the live host tool set; both mount after every
			// builtin/ask/checkpoint/eval tool and a duplicate name is a hard error.
			extra_tools,
			internal_urls: Some(Arc::clone(&self.internal_urls)),
			url_scope: Some(self.url_scope()),
			subagent_observer: Some(crate::agent::ObserverHandle(Arc::clone(&self.observer))),
			credential_resolver: Some(crate::agent::ResolverHandle(Arc::clone(&self.resolver))),
			child_session_root: self.child_session_root(),
			goal: Some(Arc::clone(&self.goal)),
			goal_context,
		})
	}

	/// Immutable URL scope for this session's turns: the session file plus its
	/// session-scoped artifacts directory as the `local://` root.
	fn url_scope(&self) -> Arc<UrlScope> {
		let session_file = self
			.store
			.file()
			.filter(|_| self.store.enabled())
			.map(Path::to_path_buf);
		let local_root = session_file
			.as_deref()
			.and_then(session::artifacts_directory_for);
		Arc::new(UrlScope {
			session_file,
			local_roots: local_root
				.map(|dir| std::collections::BTreeMap::from([("local".to_string(), dir)]))
				.unwrap_or_default(),
		})
	}

	/// Directory child transcripts live in. Only a persisted session has a
	/// stable one; in-memory sessions keep `get_subagent_messages` unavailable
	/// instead of inventing a temp path.
	fn child_session_root(&self) -> Option<PathBuf> {
		self
			.store
			.file()
			.filter(|_| self.store.enabled())
			.and_then(session::artifacts_directory_for)
			.map(|dir| dir.join("subagents"))
	}

	/// Persist and account for one finished turn.
	pub fn finish_turn(&mut self, prepared: &mut PreparedTurn, outcome: &TurnOutcome) {
		self.is_streaming = false;
		let mut record = std::mem::take(&mut prepared.builder).finish();
		if outcome.error.is_some() {
			let canon = session::canonical_stop_reason(&record.stop_reason);
			if record.stop_reason.is_empty() || canon == "toolUse" {
				record.stop_reason = "aborted".to_string();
			}
			self.checkpoint_host.abandon_in_memory_rewind();
			if let Some(messages) = self.in_memory_messages.as_mut() {
				messages.truncate(prepared.in_memory_before);
			}
		}
		if !record.assistant.is_empty() {
			self.last_assistant_text = Some(record.assistant.clone());
		}
		// Token rate needs provider-reported usage, which this build's provider
		// stream does not surface; the state field stays absent rather than zero.
		self.tokens_per_second = None;
		let _ = prepared.started.elapsed();
		if self.store.enabled() {
			// The live path already journaled the prompt (prepare_turn) and every
			// assistant/tool tail entry (persist_turn_end, record_user_steer), so
			// appending the turn record here would write the whole turn twice.
			// Rebuild the view from the log instead.
			self.refresh_history_from_store();
		} else {
			self.history.push(record);
		}
	}

	/// Persist a steering message the loop accepted, at its real position, and
	/// mirror it on the wire.
	///
	/// Called from the event pump after acceptance, never at queue time: a
	/// message that was queued but not injected must not appear in the log.
	fn record_user_steer(&mut self, prepared: &mut PreparedTurn, parts: &[AgentContentPart]) {
		let mut text = Vec::new();
		let mut images: Vec<ContentPart> = Vec::new();
		for part in parts {
			match part {
				AgentContentPart::Text { text: value } => text.push(value.clone()),
				AgentContentPart::Image { data, mime_type } => {
					images.push(ContentPart::image(data, mime_type));
				},
			}
		}
		let joined = text.join("\n");
		let content = if images.is_empty() {
			MessageContent::Text(joined.clone())
		} else {
			let mut assembled = Vec::new();
			if !joined.is_empty() {
				assembled.push(ContentPart::text(&joined));
			}
			assembled.extend(images.iter().cloned());
			MessageContent::Parts(assembled)
		};
		let now = SystemTime::now();
		let timestamp_ms = now
			.duration_since(SystemTime::UNIX_EPOCH)
			.unwrap_or_default()
			.as_millis() as u64;
		// Parent is the current physical leaf, so the injected user lands at the
		// position the loop accepted it instead of being replayed at the tail of
		// the turn record.
		let entry = MessageEntry::new(
			format!("msg-{}", output::session_id()),
			self.store.leaf_id().map(str::to_string),
			output::iso8601(now),
			SessionMessage::user(content, timestamp_ms),
		);
		let message = runtime::user_message_from_parts(&joined, &images).ok();
		if self.store.enabled() {
			if let Err(err) = entry.validate() {
				eprintln!("warning: {err}");
			} else if let Err(err) = self.store.append_message(&entry) {
				eprintln!("warning: {err}");
			} else {
				// The accepted entry becomes the parent for everything this turn
				// appends next (assistant text, tool results), so the active branch
				// includes the steering message instead of orphaning it.
				prepared.parent_id = Some(entry.id.clone());
				// The next request rebuilds its context from the store, so follow
				// it now: otherwise the injected message would be missing from
				// the very next prompt even though the log holds it.
				if let (Some(messages), Some(message)) =
					(self.history_messages.as_mut(), message.as_ref())
				{
					messages.push(message.clone());
				}
				self.refresh_history_from_store();
			}
		} else if let Some(message) = message {
			self.in_memory_messages.get_or_insert_with(Vec::new).push(message);
		}
		if let Ok(value) = serde_json::to_value(&entry.message) {
			self.messages.push(value);
		}
	}

	/// Re-derive the turn view from the log after an entry landed outside a
	/// turn record (steering); the active branch decides which entries count.
	fn refresh_history_from_store(&mut self) {
		let rebuilt = if self.store.has_branch_context() {
			self.store.branch_history()
		} else {
			Ok(self.store.history().to_vec())
		};
		match rebuilt {
			Ok(history) => self.history = history,
			Err(err) => eprintln!("warning: cannot rebuild session history: {err}"),
		}
	}

	/// Apply one streamed turn event to session state. Returns `true` when the
	/// caller should keep streaming.
	pub fn observe_event(&mut self, prepared: &mut PreparedTurn, event: &AgentEvent) {
		match event {
			AgentEvent::ToolStart { id, tool, .. } => {
				prepared.tool_names.insert(id.clone(), tool.clone());
			},
			AgentEvent::ToolEnd { id, images, .. } => {
				if !images.is_empty() {
					let converted: Vec<ContentPart> = images
						.iter()
						.filter_map(|part| match part {
							AgentContentPart::Image { data, mime_type } => {
								Some(ContentPart::image(data, mime_type))
							},
							AgentContentPart::Text { .. } => None,
						})
						.collect();
					prepared.tool_images.insert(id.clone(), converted);
				}
			},
			AgentEvent::UserSteer { parts } => self.record_user_steer(prepared, parts),
			AgentEvent::TurnEnd { message, tool_results, stop_reason, api, provider, model } => {
				self.persist_turn_end(
					prepared,
					message,
					tool_results,
					stop_reason,
					api.as_deref(),
					provider.as_deref(),
					model.as_deref(),
				);
			},
			AgentEvent::AgentEnd { messages, .. } => {
				if !self.store.enabled() {
					// In-memory mirror of the loop's final conversation.
					let mut mirror = Vec::with_capacity(messages.len());
					for message in messages {
						mirror.push(message.clone());
					}
					self.in_memory_messages = Some(mirror);
				}
			},
			_ => {},
		}
	}

	#[allow(clippy::too_many_arguments)]
	fn persist_turn_end(
		&mut self,
		prepared: &mut PreparedTurn,
		message: &Message,
		tool_results: &[pi_agent::ToolResult],
		stop_reason: &str,
		api: Option<&str>,
		provider: Option<&str>,
		model: Option<&str>,
	) {
		let now = SystemTime::now();
		let iso = output::iso8601(now);
		let now_ms = now
			.duration_since(SystemTime::UNIX_EPOCH)
			.unwrap_or_default()
			.as_millis() as u64;

		let mut parts = Vec::new();
		if !message.content.is_empty() {
			parts.push(ContentPart::text(message.content.clone()));
		}
		for call in &message.tool_calls {
			let mut part = ContentPart {
				kind: "toolCall".to_string(),
				text: None,
				data: None,
				mime_type: None,
				extra: serde_json::Map::new(),
			};
			part.extra.insert("id".to_string(), json!(call.id));
			part.extra.insert("name".to_string(), json!(call.name));
			part.extra.insert("arguments".to_string(), call.arguments.clone());
			parts.push(part);
		}
		let content = if parts.is_empty() {
			MessageContent::Text(String::new())
		} else {
			MessageContent::Parts(parts)
		};

		let asst_api = api
			.map(str::to_string)
			.or_else(|| {
				provider
					.or(prepared.provider_fallback.as_deref())
					.and_then(|p| self.config.providers.get(p).and_then(|c| c.api.clone()))
			})
			.unwrap_or_default();
		let asst_provider = provider
			.map(str::to_string)
			.or_else(|| prepared.provider_fallback.clone())
			.unwrap_or_default();
		let asst_model = model
			.map(str::to_string)
			.or_else(|| prepared.model_fallback.clone())
			.unwrap_or_default();

		let canon_stop = session::canonical_stop_reason(stop_reason);
		// One persisted usage per assistant message: `TurnEnd` repeats the same
		// message snapshot as `MessageEnd`, so the usage is read here only and
		// never added on the event stream.
		let mut asst_msg = SessionMessage::assistant(
			content,
			asst_api,
			asst_provider,
			asst_model,
			session::reported_usage(message),
			canon_stop,
			now_ms,
		);
		if canon_stop == "aborted" {
			asst_msg.error_message = Some("Interrupted by user".to_string());
		}
		let asst_id = format!("msg-{}", output::session_id());
		let asst_entry = MessageEntry::new(
			asst_id.clone(),
			prepared.parent_id.take(),
			&iso,
			asst_msg,
		);
		let mut entries = vec![asst_entry];
		let mut previous = asst_id;

		for result in tool_results {
			let id = format!("msg-{}", output::session_id());
			let name = prepared
				.tool_names
				.get(&result.tool_call_id)
				.cloned()
				.unwrap_or_default();
			let images = prepared
				.tool_images
				.get(&result.tool_call_id)
				.cloned()
				.unwrap_or_default();
			let tool_content = if images.is_empty() {
				MessageContent::Text(result.content.clone())
			} else {
				// Same parts the provider request carries (validated once in
				// pi-agent from `details.images`), so the journal replays them.
				let mut assembled = Vec::new();
				if !result.content.is_empty() {
					assembled.push(ContentPart::text(&result.content));
				}
				assembled.extend(images);
				MessageContent::Parts(assembled)
			};
			let mut tool_msg = SessionMessage::tool_result(
				result.tool_call_id.clone(),
				name,
				tool_content,
				result.is_error,
				now_ms,
			);
			if let Some(details) = &result.details {
				tool_msg.extra.insert("details".to_string(), details.clone());
			}
			entries.push(MessageEntry::new(id.clone(), Some(previous.clone()), &iso, tool_msg));
			previous = id;
		}

		let mut valid = Vec::new();
		for entry in entries {
			match entry.validate() {
				Ok(()) => valid.push(entry),
				Err(err) => eprintln!("warning: {err}"),
			}
		}
		let pending = prepared.turn_ack.take();
		let turn_start = pending.as_ref().map_or(0, |(request, _)| request.turn_start);
		if self.store.enabled() && !valid.is_empty() {
			if let Err(err) = self.store.append_messages(&valid) {
				eprintln!("warning: {err}");
			}
			let _ = self.checkpoint_host.note_persisted_tool_results(&self.store, &valid);
			if let Err(err) = self.checkpoint_host.persist_active_reminder(&mut self.store) {
				eprintln!("warning: {err}");
			}
		} else if !self.store.enabled() {
			let _ = self
				.checkpoint_host
				.note_unpersisted_tool_results(tool_results, &prepared.tool_names, turn_start);
		}
		for entry in &valid {
			if let Ok(value) = serde_json::to_value(&entry.message) {
				self.messages.push(value);
			}
		}
		prepared.parent_id = Some(previous);

		if let Some((_, reply)) = pending {
			let _ = reply.send(pi_agent::TurnAckDecision::default());
		}
	}
}

fn is_openai_service_tier_api(api: Option<&str>) -> bool {
	matches!(
		api,
		Some("openai-completions" | "openai-responses" | "openai-codex-responses")
	)
}

/// Port of TS `isOpenAIModelId` (`packages/catalog/src/identity/family.ts`).
///
/// Boundary-delimited family prefixes only; a substring match such as
/// `my-gpt-tool` is not an OpenAI model. This is a prefix test rather than the
/// upstream regex, which is equivalent for catalog ids.
fn is_openai_model_id(id: &str) -> bool {
	let lower = id.to_ascii_lowercase();
	if lower.contains("openai/") {
		return true;
	}
	for segment in lower.split('/') {
		for prefix in ["gpt-", "gpt.", "chatgpt-", "chatgpt.", "codex-", "codex."] {
			if segment.starts_with(prefix) {
				return true;
			}
		}
		for sku in ["o1", "o3", "o4"] {
			if segment == sku
				|| segment.starts_with(&format!("{sku}-"))
				|| segment.starts_with(&format!("{sku}."))
			{
				return true;
			}
		}
	}
	false
}

fn excludes_inferred_openai_service_tier(provider: &str) -> bool {
	matches!(provider, "fireworks" | "github-copilot")
}

fn is_openai_service_tier_model(provider: &str, api: Option<&str>, id: &str) -> bool {
	!excludes_inferred_openai_service_tier(provider)
		&& is_openai_service_tier_api(api)
		&& is_openai_model_id(id)
}

/// Port of TS `serviceTierFamily` (`packages/ai/src/types.ts`).
fn service_tier_family(provider: &str, api: Option<&str>, id: &str) -> Option<&'static str> {
	if provider == "openrouter" {
		let lower = id.to_ascii_lowercase();
		if lower.starts_with("anthropic/") {
			return Some("anthropic");
		}
		if lower.starts_with("google/") {
			return Some("google");
		}
		if lower.starts_with("openai/") {
			return Some("openai");
		}
		return None;
	}
	if matches!(provider, "openai" | "openai-codex") {
		return Some("openai");
	}
	if api == Some("anthropic-messages") {
		return Some("anthropic");
	}
	if matches!(provider, "google" | "google-vertex") {
		return Some("google");
	}
	if is_openai_service_tier_model(provider, api, id) {
		return Some("openai");
	}
	None
}

fn should_send_service_tier(provider: &str, api: Option<&str>, id: &str) -> bool {
	if matches!(provider, "openai" | "openai-codex") {
		return true;
	}
	if provider == "openrouter" {
		return true;
	}
	if is_openai_service_tier_model(provider, api, id) {
		return true;
	}
	matches!(provider, "google" | "google-vertex" | "fireworks")
}

/// Port of TS `realizesPriorityServiceTier`: whether `priority` reaches the
/// provider wire for this model. Bedrock/Vertex Claude and OpenRouter Anthropic
/// models silently drop it, so they report inactive.
fn realizes_priority(provider: &str, api: Option<&str>, id: &str) -> bool {
	if provider == "anthropic" {
		return true;
	}
	if provider == "openrouter" {
		let family = service_tier_family(provider, api, id);
		return matches!(family, Some("openai" | "google"));
	}
	if api == Some("anthropic-messages") {
		return false;
	}
	should_send_service_tier(provider, api, id)
}

fn clean_title(name: &str) -> String {
	let cleaned = name.trim().replace(['\r', '\n'], " ");
	let mut chars = cleaned.chars();
	let truncated: String = chars.by_ref().take(200).collect();
	truncated.trim().to_string()
}

/// Live turn bookkeeping owned by the pump.
pub struct PreparedTurn {
	pub token:      CancellationToken,
	pub turn_ack:   Arc<TurnAck>,
	pub builder:    TurnBuilder,
	pub parent_id:  Option<String>,
	pub in_memory_before: usize,
	pub started:    Instant,
	pub model_fallback: Option<String>,
	pub provider_fallback: Option<String>,
	pub tool_names: std::collections::HashMap<String, String>,
	/// Validated result images per tool call, as the provider request carries
	/// them; persisted as tool-result parts and replayed on resume.
	pub tool_images: std::collections::HashMap<String, Vec<ContentPart>>,
	pub wire_user_message: Option<Value>,
	pub aborted:    bool,
}

impl PreparedTurn {
	/// Discard everything a failed attempt streamed so the retry starts a clean
	/// assistant turn. The user entry (`parent_id`, `wire_user_message`) and the
	/// in-memory boundary stay, so the retry cannot duplicate the prompt.
	pub fn reset_for_retry(&mut self, input: &TurnInput, parts: &[ContentPart]) {
		self.builder = if parts.is_empty() {
			TurnBuilder::new(input.text.clone())
		} else {
			TurnBuilder::new_with_parts(input.text.clone(), parts.to_vec())
		};
		self.tool_names.clear();
		self.started = Instant::now();
		self.aborted = false;
	}
}

/// Result of one pumped turn.
#[derive(Debug, Clone, Default)]
pub struct TurnOutcome {
	pub error:   Option<String>,
	pub aborted: bool,
}

/// Which turn the caller wants executed.
pub enum TurnAdvance {
	/// Begin the queued message now.
	Run(TurnInput),
	/// Nothing queued.
	Idle,
}

impl RpcSession {
	/// Pop the next queued turn according to the follow-up mode.
	///
	/// `all` batches every queued message into one native turn (the runner takes
	/// a single prompt + parts, so texts are joined and images concatenated);
	/// `one-at-a-time` returns exactly one message.
	pub fn next_queued(&mut self) -> TurnAdvance {
		let mode = self.follow_up_mode;
		let batch = self.take_queued(mode);
		if batch.is_empty() {
			return TurnAdvance::Idle;
		}
		if batch.len() == 1 {
			return TurnAdvance::Run(batch.into_iter().next().expect("one message"));
		}
		let mut text = String::new();
		let mut images = Vec::new();
		for input in batch {
			if !text.is_empty() {
				text.push_str("\n\n");
			}
			text.push_str(&input.text);
			images.extend(input.images);
		}
		TurnAdvance::Run(TurnInput { text, images })
	}

	/// Whether messages are waiting for the current turn to finish.
	#[must_use]
	pub fn has_queued(&self) -> bool {
		!self.queued.is_empty()
	}

	/// Put a message at the head of the queue (`abort_and_prompt`).
	pub fn queue_front(&mut self, input: TurnInput) {
		self.queued.push_front(input);
	}

	/// The assistant message and tool results just persisted for a finished
	/// turn. Sliced by role so a missing tool result never mislabels the frame.
	#[must_use]
	pub fn last_turn_wire(&self, tool_count: usize) -> (Option<Value>, Vec<Value>) {
		let mut index = self.messages.len();
		let mut results = Vec::new();
		for _ in 0..tool_count {
			let Some(candidate) = index.checked_sub(1).and_then(|i| self.messages.get(i)) else {
				break;
			};
			if candidate.get("role").and_then(Value::as_str) != Some("toolResult") {
				break;
			}
			results.insert(0, candidate.clone());
			index -= 1;
		}
		let message = index
			.checked_sub(1)
			.and_then(|i| self.messages.get(i))
			.filter(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
			.cloned();
		(message, results)
	}

	/// Run one isolated summarizer request (no tools, one turn, not persisted)
	/// and return the streamed assistant text.
	fn summarize(&mut self, prompt: String) -> Result<String> {
		let mut request = self.request(prompt, Vec::new())?;
		request.tools_enabled = false;
		request.max_turns = 1;
		request.session_persisted = false;
		request.session_file = None;
		request.session_id = None;
		request.turn_ack = None;
		request.cancellation_token = None;
		request.history = Vec::new();
		let runner = Arc::clone(&self.runner);
		let events = runner.run(&request)?;
		let mut text = String::new();
		for event in events {
			if let AgentEvent::MessageUpdate { delta } = event? {
				text.push_str(&delta);
			}
		}
		let text = text.trim().to_string();
		if text.is_empty() {
			return Err(Error::new("the summarizer returned no text"));
		}
		Ok(text)
	}

	/// `compact`: replace the live context with a model-written summary.
	pub fn compact(&mut self, custom: Option<String>) -> Result<Value> {
		if self.is_streaming {
			return Err(Error::new("Cannot compact while a response is streaming"));
		}
		if self.is_compacting {
			return Err(Error::new("Compaction already in progress"));
		}
		self.is_compacting = true;
		let result = self.summarize(compaction_prompt(custom.as_deref()));
		self.is_compacting = false;
		let summary = result?;
		// Upstream `prepareCompaction`: prompt-side occupancy of the newest
		// reported request; 0 only when the branch never reported usage.
		let tokens_before = self
			.store
			.latest_context_tokens()
			.or_else(|| self.in_memory_context_tokens())
			.unwrap_or(0);
		let messages_removed = self.messages.len();
		let summary_message = runtime::user_message_from_parts(&summary, &[])
			.map_err(|err| Error::new(err.to_string()))?;
		if self.store.enabled() {
			self.history_messages = Some(vec![summary_message]);
		} else {
			// No-session mode sends `in_memory_messages`, not `history_messages`;
			// the compacted summary must replace the pre-compaction history or
			// the next turn still sends it.
			self.history_messages = None;
			self.in_memory_messages = Some(vec![summary_message]);
		}
		self.messages = vec![json!({
			"role": "user",
			"content": [{ "type": "text", "text": summary }],
			"timestamp": now_ms(),
		})];
		self.last_assistant_text = None;
		// The rewrite retains no prior entry: the rebuilt context starts at this
		// compaction entry, so its id is the real `firstKeptEntryId` (same
		// convention as the importer, `foreign_session.rs` compaction entries).
		// An empty id only when the log is disabled or the append failed.
		let mut first_kept = String::new();
		if self.store.enabled() {
			match self.store.append_custom_message(
				"compaction",
				&summary,
				false,
				json!({
					"tokensBefore": tokens_before,
					"messagesRemoved": messages_removed,
				}),
			) {
				Ok(id) => first_kept = id,
				Err(err) => eprintln!("warning: {err}"),
			}
		}
		Ok(json!({
			"summary": summary,
			"firstKeptEntryId": first_kept,
			"tokensBefore": tokens_before,
		}))
	}

	/// `handoff`: summarize, then continue in a fresh session that starts from
	/// the summary. Returns the new session path when one was persisted.
	pub fn handoff(&mut self, custom: Option<String>) -> Result<Option<String>> {
		if self.is_streaming {
			return Err(Error::new("Cannot hand off while a response is in progress"));
		}
		if self.is_compacting {
			return Err(Error::new("Compaction already in progress"));
		}
		self.is_compacting = true;
		let result = self.summarize(handoff_prompt(custom.as_deref()));
		self.is_compacting = false;
		let summary = result?;
		self.new_session(None)?;
		let summary_message = runtime::user_message_from_parts(&summary, &[])
			.map_err(|err| Error::new(err.to_string()))?;
		if self.store.enabled() {
			self.history_messages = Some(vec![summary_message]);
		} else {
			// No-session mode sends `in_memory_messages`, not `history_messages`;
			// the compacted summary must replace the pre-compaction history or
			// the next turn still sends it.
			self.history_messages = None;
			self.in_memory_messages = Some(vec![summary_message]);
		}
		self.messages = vec![json!({
			"role": "user",
			"content": [{ "type": "text", "text": summary }],
			"timestamp": now_ms(),
		})];
		self.last_assistant_text = None;
		Ok(self.store.file().map(|path| path.display().to_string()))
	}

	/// `export_html`: write a standalone HTML transcript.
	pub fn export_html(&self, output_path: Option<&str>) -> Result<String> {
		let path = match output_path {
			Some(path) => PathBuf::from(path),
			None => match self.store.file() {
				Some(file) => file.with_extension("html"),
				None => self.cwd.join(format!("omp-session-{}.html", self.session_id())),
			},
		};
		let html = render_html(self);
		if let Some(parent) = path.parent() {
			if !parent.as_os_str().is_empty() {
				std::fs::create_dir_all(parent).map_err(|err| {
					Error::new(format!("cannot create {}: {err}", parent.display()))
				})?;
			}
		}
		std::fs::write(&path, html)
			.map_err(|err| Error::new(format!("cannot write {}: {err}", path.display())))?;
		Ok(path.display().to_string())
	}
}

fn now_ms() -> u64 {
	SystemTime::now()
		.duration_since(SystemTime::UNIX_EPOCH)
		.unwrap_or_default()
		.as_millis() as u64
}

/// Wire mirror of the messages on the store's active branch.
///
/// `messages()` returns every entry on the file; when the log carries a
/// `branch_summary` the visible conversation is only the selected path, so the
/// mirror has to follow the same entries `history()` was rebuilt from.
fn branch_wire_messages(store: &SessionStore) -> Vec<Value> {
	if !store.has_branch_context() {
		return store
			.messages()
			.iter()
			.filter_map(|entry| serde_json::to_value(&entry.message).ok())
			.collect();
	}
	store
		.branch_messages()
		.into_iter()
		.filter_map(|entry| serde_json::to_value(&entry.message).ok())
		.collect()
}

fn compaction_prompt(custom: Option<&str>) -> String {
	let mut prompt = String::from(
		"Provide a thorough summary of the conversation above for continuing work in a fresh \
		 context. Focus on goals, decisions, files changed, and remaining tasks. Use plain \
		 prose, no preamble.",
	);
	if let Some(custom) = custom.map(str::trim).filter(|custom| !custom.is_empty()) {
		prompt.push_str("\n\nAdditional focus: ");
		prompt.push_str(custom);
	}
	prompt
}

fn handoff_prompt(custom: Option<&str>) -> String {
	let mut prompt = String::from(
		"Write a handoff document for a fresh session. Cover the current goal, what has been \
		 done, what is left, key constraints and open questions. Use plain prose, no preamble.",
	);
	if let Some(custom) = custom.map(str::trim).filter(|custom| !custom.is_empty()) {
		prompt.push_str("\n\nAdditional focus: ");
		prompt.push_str(custom);
	}
	prompt
}

fn html_escape(text: &str) -> String {
	text.replace('&', "&amp;")
		.replace('<', "&lt;")
		.replace('>', "&gt;")
		.replace('"', "&quot;")
}

fn render_html(session: &RpcSession) -> String {
	let mut body = String::new();
	for message in session.wire_messages() {
		let role = message.get("role").and_then(Value::as_str).unwrap_or("unknown");
		let mut text = String::new();
		match message.get("content") {
			Some(Value::String(value)) => text.push_str(value),
			Some(Value::Array(parts)) => {
				for part in parts {
					if let Some(chunk) = part.get("text").and_then(Value::as_str) {
						text.push_str(chunk);
					}
					if let Some(call) = part.get("input") {
						text.push_str(&format!("\n[tool call] {call}"));
					}
				}
			},
			_ => {},
		}
		body.push_str(&format!(
			"<article class=\"{role}\"><h2>{role}</h2><pre>{}</pre></article>\n",
			html_escape(&text)
		));
	}
	let title = html_escape(session.session_name.as_deref().unwrap_or("OMP session"));
	format!(
		"<!doctype html>\n<html><head><meta charset=\"utf-8\"><title>{title}</title>\
		 <style>body{{font-family:system-ui,sans-serif;max-width:52rem;margin:2rem auto;padding:0 1rem;}}\
		 article{{border-top:1px solid #ddd;padding:1rem 0}}pre{{white-space:pre-wrap}}\
		 .user h2{{color:#0b6}}.assistant h2{{color:#06c}}</style></head><body>\
		 <h1>{title}</h1>\n{body}</body></html>\n"
	)
}

