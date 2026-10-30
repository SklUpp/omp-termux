//! ACP session driver: one `RpcSession` per ACP session, ordered streaming,
//! single-flight prompts with implicit cancel, the 5 s cancel barrier, and the
//! reverse calls the client owns.
//!
//! Ground truth is `modes/acp/acp-agent.ts`: `prompt`/`cancel` 820-1123, the
//! settle/event pump 1360-1660, prompt-block conversion 1685-1721, replay
//! 2290-2400, and `session-tools.ts` permission gating (via
//! `acp-permission-gate.ts`). Turn primitives are the actual `RpcSession`
//! (`prepare_turn`/`observe_event`/`finish_turn`) and `AgentRunner::run`; there
//! is no second session store and no Goal write.
//!
//! Ownership: one command handler thread runs one prompt turn at a time. A
//! prompt worker takes the session mutex for the whole turn; cancel never does.
//! `notify_control` signals the turn token through [`AcpSession::signal_cancel`]
//! so a blocked prompt worker exits without touching the session lock.
//!
//! Reverse-call invocation from native tools (`fs/*`, `terminal/*`) belongs to
//! the pi-agent composition slice; [`AcpReverseClient`] is the exact sender it
//! calls. Permission and elicitation are fully wired here through
//! `Approvals::set_approver` and the `ask` channel.

use std::{
	collections::{BTreeMap, BTreeSet},
	path::{Path, PathBuf},
	sync::{
		Arc, Condvar, Mutex, MutexGuard, PoisonError,
		atomic::{AtomicU64, Ordering},
		mpsc::{self, Receiver, RecvTimeoutError, Sender},
	},
	thread,
	time::{Duration, Instant, SystemTime},
};

use async_trait::async_trait;
use pi_agent::{CancellationToken, state::ContentPart as AgentContentPart};
use serde_json::{Value, json};

use crate::{
	agent::{AgentRunner, Event as AgentEvent},
	approve::{ApprovalMode, Approvals, Decision, ToolTier, classify_tool},
	catalog::Catalog,
	config::Config,
	output,
	prompts::Prompts,
	rpc::{
		auth::CredentialResolver,
		host_tools::HostToolBridge,
		host_uris::HostUriBridge,
		session::{FrameSink, RpcLaunch, RpcSession, TurnInput, TurnOutcome},
		subagents::SubagentRegistry,
		types::RpcImage,
	},
	runtime,
	session::{self as session_store, SessionHeader, SessionOptions, compute_default_session_dir},
	skills::Skill,
};

use super::{
	capabilities,
	plan::{self, PlanGate, PlanToolCall},
	protocol::{ClientCapabilities, INTERNAL_ERROR, RequestError},
	reverse::{ReverseCaller, ReverseError},
	session_list,
	transport::Transport,
};

/// TS `ACP_CANCEL_CLEANUP_TIMEOUT_MS`.
pub const CANCEL_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
/// TS `ACP_BOOTSTRAP_RACE_GUARD_MS`.
pub const BOOTSTRAP_RACE_GUARD: Duration = Duration::from_millis(50);
/// TS `ACP_TEXT_LIMIT`.
pub const ACP_TEXT_LIMIT: usize = 4_000;
/// Reverse-call observation interval.
const CANCEL_POLL: Duration = Duration::from_millis(50);

/// Immutable per-connection recipe resolved by the CLI hook.
pub struct AcpRecipe {
	pub runner: Arc<dyn AgentRunner>,
	pub config: Config,
	pub prompts: Prompts,
	pub skills: Vec<Skill>,
	pub catalog: Arc<Catalog>,
	pub resolver: Arc<dyn CredentialResolver>,
	pub approval_mode: ApprovalMode,
	pub allow: Vec<String>,
	pub model: Option<String>,
	pub provider: Option<String>,
	pub api_key: Option<String>,
	pub thinking: Option<runtime::CliThinking>,
	pub max_time: Option<Duration>,
	pub max_turns: usize,
	pub tools_enabled: bool,
	pub sessions_root: PathBuf,
	/// `plan.enabled`; TS default is `true` (`settings-schema.ts:4743`).
	pub plan_enabled: bool,
}

impl AcpRecipe {
	#[must_use]
	pub fn new(
		runner: Arc<dyn AgentRunner>,
		config: Config,
		prompts: Prompts,
		skills: Vec<Skill>,
		catalog: Arc<Catalog>,
		resolver: Arc<dyn CredentialResolver>,
	) -> Self {
		Self {
			runner,
			config,
			prompts,
			skills,
			catalog,
			resolver,
			approval_mode: ApprovalMode::AlwaysAsk,
			allow: Vec::new(),
			model: None,
			provider: None,
			api_key: None,
			thinking: None,
			max_time: None,
			max_turns: 0,
			tools_enabled: true,
			sessions_root: SessionOptions::default_sessions_root(),
			plan_enabled: true,
		}
	}
}

/// Connection-scoped driver shared by every ACP session.
#[derive(Clone)]
pub struct AcpRuntime {
	recipe: Arc<AcpRecipe>,
	transport: Transport,
	reverse: ReverseCaller,
}

impl AcpRuntime {
	#[must_use]
	pub fn new(recipe: AcpRecipe, transport: Transport) -> Self {
		Self { recipe: Arc::new(recipe), reverse: ReverseCaller::new(transport.clone()), transport }
	}

	#[must_use]
	pub fn transport(&self) -> &Transport {
		&self.transport
	}

	#[must_use]
	pub fn sessions_root(&self) -> PathBuf {
		self.recipe.sessions_root.clone()
	}

	/// Open a brand-new session at an absolute cwd.
	pub fn new_session(
		&self,
		cwd: PathBuf,
		caps: &ClientCapabilities,
		mcp_servers: Vec<crate::config::DiscoveredMcpServer>,
	) -> Result<Arc<AcpSession>, RequestError> {
		let dir = compute_default_session_dir(&cwd, Some(&self.recipe.sessions_root));
		self.open(cwd, caps, None, dir, true, mcp_servers)
	}

	/// Open an existing stored session by id. `cwd` must match the requested
	/// workspace; the id lookup is the native store's.
	pub fn load_session(
		&self,
		id: &str,
		cwd: PathBuf,
		caps: &ClientCapabilities,
		mcp_servers: Vec<crate::config::DiscoveredMcpServer>,
	) -> Result<Arc<AcpSession>, RequestError> {
		let stored = session_list::find_session(id, &cwd, Some(&self.recipe.sessions_root))
			.ok_or_else(|| {
				RequestError::new(INTERNAL_ERROR, format!("ACP session not found: {id}"))
			})?;
		let dir = stored
			.path
			.parent()
			.map(Path::to_path_buf)
			.unwrap_or_else(|| compute_default_session_dir(&cwd, Some(&self.recipe.sessions_root)));
		let session = self.open(cwd, caps, Some(stored.id.clone()), dir, false, mcp_servers)?;
		Ok(session)
	}

	/// Fork a stored/loaded session into `cwd`.
	pub fn fork_session(
		&self,
		source: &AcpSession,
		cwd: PathBuf,
		caps: &ClientCapabilities,
		mcp_servers: Vec<crate::config::DiscoveredMcpServer>,
	) -> Result<Arc<AcpSession>, RequestError> {
		let source_path = source.persisted_path().ok_or_else(|| {
			RequestError::new(
				INTERNAL_ERROR,
				format!("ACP session cannot be forked before it is persisted: {}", source.id()),
			)
		})?;
		let dir = compute_default_session_dir(&cwd, Some(&self.recipe.sessions_root));
		let forked = session_store::fork_session(&source_path, Some(&cwd), Some(&dir))
			.map_err(|error| RequestError::new(INTERNAL_ERROR, error))?;
		let header = read_session_header(&forked)
			.ok_or_else(|| RequestError::new(INTERNAL_ERROR, "cannot read forked session header"))?;
		let opened = self.open(cwd, caps, Some(header.id), dir, false, mcp_servers)?;
		Ok(opened)
	}

	fn open(
		&self,
		cwd: PathBuf,
		caps: &ClientCapabilities,
		id: Option<String>,
		dir: PathBuf,
		fresh: bool,
		mcp_servers: Vec<crate::config::DiscoveredMcpServer>,
	) -> Result<Arc<AcpSession>, RequestError> {
		let store_options = SessionOptions {
			enabled: true,
			cwd: cwd.clone(),
			model: self.recipe.model.clone(),
			provider: self.recipe.provider.clone(),
			dir: Some(dir),
			id: if fresh {
				Some(output::session_id())
			} else {
				id.clone()
			},
			continue_latest: false,
			no_title: false,
			parent_session: None,
		};
		let launch = RpcLaunch {
			runner: Arc::clone(&self.recipe.runner),
			config: self.recipe.config.clone(),
			cwd: cwd.clone(),
			prompts: self.recipe.prompts.clone(),
			skills: self.recipe.skills.clone(),
			catalog: Arc::clone(&self.recipe.catalog),
			agent_db: PathBuf::new(),
			approval_mode: self.recipe.approval_mode,
			allow: self.recipe.allow.clone(),
			model: self.recipe.model.clone(),
			provider: self.recipe.provider.clone(),
			api_key: self.recipe.api_key.clone(),
			thinking: self.recipe.thinking,
			max_time: self.recipe.max_time,
			max_turns: self.recipe.max_turns,
			tools_enabled: self.recipe.tools_enabled,
			prompt_cache_key: None,
			provider_session_id: None,
			store_options,
			steer: None,
		};
		let sink: Arc<dyn FrameSink> = Arc::new(self.transport.clone());
		let host_tools = Arc::new(HostToolBridge::new(Arc::clone(&sink)));
		let host_uris = Arc::new(HostUriBridge::new(Arc::clone(&sink)));
		let internal_urls = Arc::new(pi_agent::tools::internal_urls::InternalUrlRouter::new());
		host_uris.bind(Arc::clone(&internal_urls));
		let registry = Arc::new(SubagentRegistry::new());
		let observer = registry.observer(Arc::clone(&sink));

		let token_slot = Arc::new(Mutex::new(None));
		let session_id_slot = Arc::new(Mutex::new(id.clone().unwrap_or_default()));
		let mut approvals =
			Approvals::new(self.recipe.approval_mode, self.recipe.allow.iter().cloned());
		let permission = Arc::new(AcpPermissionBroker {
			session_id: Arc::clone(&session_id_slot),
			reverse: self.reverse.clone(),
			mode: self.recipe.approval_mode,
			allow: Arc::new(self.recipe.allow.iter().cloned().collect()),
			cwd: cwd.clone(),
			token: Arc::clone(&token_slot),
			decisions: Arc::new(Mutex::new(BTreeMap::new())),
			next_id: Arc::new(AtomicU64::new(1)),
		});
		approvals.set_approver(Some(permission.clone()));
		approvals.set_ask(Some(Arc::new(AcpAskChannel {
			session_id: Arc::clone(&session_id_slot),
			reverse: self.reverse.clone(),
			caps: caps.clone(),
			next_id: Arc::new(AtomicU64::new(1)),
		})));

		let mut session = RpcSession::open(
			launch,
			approvals,
			host_tools,
			host_uris,
			internal_urls,
			observer,
			Arc::clone(&self.recipe.resolver),
		)
		.map_err(|error| RequestError::new(INTERNAL_ERROR, error.to_string()))?;
		let id = session.session_id();
		// Native `read`/`write`/`bash` calls route through the client only for
		// the capabilities this session advertised; the bridge is copied into
		// every `RunRequest` and onto the root/child tool instances.
		session.set_acp_bridge(Some(Arc::new(super::bridge::AcpToolBridge::new(
			AcpReverseClient::new(
				id.clone(),
				caps.clone(),
				self.reverse.clone(),
				self.transport.clone(),
			),
			caps.clone(),
		))));
		session.set_mcp_servers(mcp_servers);
		let record = Arc::new(AcpSession {
			id: id.clone(),
			cwd,
			runtime: self.clone(),
			caps: caps.clone(),
			session: Mutex::new(session),
			gate: PromptGate::default(),
			token: token_slot,
			plan: Mutex::new(PlanGate::new(self.recipe.plan_enabled)),
			closed: Mutex::new(None),
			next_message_id: AtomicU64::new(1),
			permission,
			on_close: Mutex::new(None),
		});
		// The permission broker and ask channel share this slot.
		record.permission.bind_session(&id);
		// Restore plan mode from the shared `mode_change` slot. Only `"plan"`
		// restores (goal/vibe/none leave plan off), and only while the feature
		// is enabled.
		if record.plan_enabled() {
			let restored = {
				let session = record.lock();
				match session.latest_mode_change() {
					Some((mode, data)) => plan::restore_from_mode_change(&mode, data.as_ref()),
					None => None,
				}
			};
			if let Some(state) = restored {
				let mut gate = record.lock_plan();
				gate.state = Some(state);
				gate.generation = gate.generation.wrapping_add(1);
			}
		}
		Ok(record)
	}
}

fn read_session_header(path: &Path) -> Option<SessionHeader> {
	let content = std::fs::read_to_string(path).ok()?;
	let line = content.lines().nth(1)?;
	serde_json::from_str::<SessionHeader>(line).ok()
}

/// One managed ACP session.
pub struct AcpSession {
	id: String,
	cwd: PathBuf,
	runtime: AcpRuntime,
	caps: ClientCapabilities,
	session: Mutex<RpcSession>,
	gate: PromptGate,
	/// Turn token, shared with the permission/ask surfaces so a cancel reaches
	/// their reverse waits. Never held while waiting for a client answer.
	token: Arc<Mutex<Option<CancellationToken>>>,
	plan: Mutex<PlanGate>,
	closed: Mutex<Option<String>>,
	next_message_id: AtomicU64,
	permission: Arc<AcpPermissionBroker>,
	on_close: Mutex<Option<CloseHook>>,
}

pub(crate) type CloseHook = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Debug)]
struct TurnDone {
	stop_reason: String,
	usage: Option<Value>,
	error: Option<RequestError>,
}

impl AcpSession {
	#[must_use]
	pub fn id(&self) -> &str {
		&self.id
	}

	#[must_use]
	pub fn cwd(&self) -> &Path {
		&self.cwd
	}

	#[must_use]
	pub fn persisted_path(&self) -> Option<PathBuf> {
		let session = self.lock();
		session.store_file_string().map(PathBuf::from)
	}

	#[must_use]
	pub fn is_closed(&self) -> bool {
		self.lock_closed().is_some()
	}

	/// Fork refuses while a prompt turn is still settling or cleaning up.
	#[must_use]
	pub fn is_prompt_in_flight(&self) -> bool {
		self.gate.is_active()
	}

	fn ensure_open(&self) -> Result<(), RequestError> {
		match &*self.lock_closed() {
			Some(reason) => Err(RequestError::new(
				INTERNAL_ERROR,
				format!("ACP session closed before queued prompt could run: {reason}"),
			)),
			None => Ok(()),
		}
	}

	/// Cancel signal from the transport reader. Signal only: never takes the
	/// session lock, never waits for a turn, never writes.
	pub fn signal_cancel(&self) {
		self.gate.request_cancel();
		if let Some(token) = self
			.token
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.clone()
		{
			token.cancel();
		}
	}

	/// Close for transport shutdown. Signals every live turn; the transport is
	/// already closed so no frame can leave.
	pub fn shutdown(&self, reason: &str) {
		*self.lock_closed() = Some(reason.to_string());
		self.signal_cancel();
	}

	fn lock(&self) -> MutexGuard<'_, RpcSession> {
		self.session.lock().unwrap_or_else(PoisonError::into_inner)
	}

	fn lock_closed(&self) -> MutexGuard<'_, Option<String>> {
		self.closed.lock().unwrap_or_else(PoisonError::into_inner)
	}

	fn lock_plan(&self) -> MutexGuard<'_, PlanGate> {
		self.plan.lock().unwrap_or_else(PoisonError::into_inner)
	}

	/// Apply a mode id: the exclusive TS `mode_change` slot accepts `plan` only
	/// while no Goal holds it, and every accepted change is persisted.
	fn apply_mode(&self, mode_id: &str) -> Result<(), RequestError> {
		if mode_id == plan::ACP_PLAN_MODE_ID && self.lock().goal.has_goal() {
			return Err(RequestError::new(
				INTERNAL_ERROR,
				"Unsupported ACP mode: plan (a Goal session holds the exclusive mode slot)",
			));
		}
		let persisted = {
			let mut gate = self.lock_plan();
			gate
				.set_mode(mode_id)
				.map_err(|message| RequestError::new(INTERNAL_ERROR, message))?;
			gate.persisted_mode_change()
		};
		self
			.lock()
			.append_mode_change(persisted.0, persisted.1)
			.map_err(|error| RequestError::new(INTERNAL_ERROR, error.to_string()))?;
		self.emit_update(json!({
			"sessionUpdate": "current_mode_update",
			"currentModeId": self.lock_plan().current_mode_id(),
		}));
		Ok(())
	}

	/// `session/set_mode` and the `mode` config option.
	pub fn set_mode(&self, mode_id: &str) -> Result<Value, RequestError> {
		self.apply_mode(mode_id)?;
		let config = self.config_options();
		self.emit_update(json!({
			"sessionUpdate": "config_option_update",
			"configOptions": config.clone(),
		}));
		Ok(json!({ "configOptions": config }))
	}

	/// `session/set_config_option`.
	pub fn set_config_option(&self, config_id: &str, value: &Value) -> Result<Value, RequestError> {
		if value.is_boolean() {
			return Err(RequestError::new(
				INTERNAL_ERROR,
				format!("Unsupported boolean ACP config option: {config_id}"),
			));
		}
		let text = value
			.as_str()
			.ok_or_else(|| RequestError::invalid_params())?;
		match config_id {
			"mode" => self.apply_mode(text)?,
			"model" => {
				let (provider, model_id) = text.split_once('/').ok_or_else(|| {
					RequestError::new(INTERNAL_ERROR, format!("Unknown ACP model: {text}"))
				})?;
				let mut session = self.lock();
				session
					.set_model(provider, model_id)
					.map_err(|error| RequestError::new(INTERNAL_ERROR, error.to_string()))?;
			},
			"thinking" => {
				let mut session = self.lock();
				session
					.set_thinking_level(text)
					.map_err(|error| RequestError::new(INTERNAL_ERROR, error.to_string()))?;
			},
			other => {
				return Err(RequestError::new(
					INTERNAL_ERROR,
					format!("Unknown ACP config option: {other}"),
				));
			},
		}
		let config = self.config_options();
		self.emit_update(json!({
			"sessionUpdate": "config_option_update",
			"configOptions": config.clone(),
		}));
		Ok(json!({ "configOptions": config }))
	}

	#[must_use]
	pub fn config_options(&self) -> Value {
		let session = self.lock();
		let plan = self.lock_plan();
		let mut options = vec![json!({
			"id": "mode",
			"name": "Mode",
			"category": "mode",
			"type": "select",
			"currentValue": plan.current_mode_id(),
			"options": plan.modes().as_array().cloned().unwrap_or_default().into_iter().map(|mode| json!({
				"value": mode["id"],
				"name": mode["name"],
				"description": mode["description"],
			})).collect::<Vec<_>>(),
		})];
		let models = session.available_models();
		if !models.is_empty() {
			let current = session
				.model_object()
				.and_then(|model| model_id_of(&model))
				.or_else(|| models.first().and_then(model_id_of));
			options.push(json!({
				"id": "model",
				"name": "Model",
				"category": "model",
				"type": "select",
				"currentValue": current,
				"options": models.iter().filter_map(|model| {
					let id = model_id_of(model)?;
					Some(json!({
						"value": id,
						"name": model.get("name").and_then(Value::as_str).unwrap_or(&id),
						"description": id,
					}))
				}).collect::<Vec<_>>(),
			}));
		}
		let efforts = session.available_efforts().unwrap_or_default();
		let state = session.state(None);
		let reasoning = session
			.model_object()
			.and_then(|model| model.get("reasoning").and_then(Value::as_bool))
			.unwrap_or(false);
		let current_thinking = if !reasoning {
			"off".to_string()
		} else {
			match state.thinking_level.as_deref() {
				Some("off") => "off".to_string(),
				Some(level) => level.to_string(),
				None => "auto".to_string(),
			}
		};
		let mut thinking_options = vec![
			json!({ "value": "off", "name": "Off" }),
			json!({ "value": "auto", "name": "Auto", "description": "Auto-detect per prompt" }),
		];
		for level in efforts {
			thinking_options.push(json!({ "value": level, "name": level }));
		}
		options.push(json!({
			"id": "thinking",
			"name": "Thinking",
			"category": "thought_level",
			"type": "select",
			"currentValue": current_thinking,
			"options": thinking_options,
		}));
		Value::Array(options)
	}

	#[must_use]
	pub fn mode_state(&self) -> Value {
		self.lock_plan().mode_state()
	}

	#[must_use]
	pub fn session_name(&self) -> Option<String> {
		self.lock().state(None).session_name
	}

	/// Replace the client-supplied MCP servers for later turns of an
	/// already-loaded session (`session/load|resume` on a managed id).
	pub fn set_mcp_servers(&self, servers: Vec<crate::config::DiscoveredMcpServer>) {
		self.lock().set_mcp_servers(servers);
	}

	/// `session/prompt`.
	pub fn prompt(self: &Arc<Self>, prompt: &Value) -> Result<Value, RequestError> {
		self.ensure_open()?;
		// Implicit cancel: a new prompt while a turn is still in flight aborts
		// it exactly like `session/cancel`, then queues behind cleanup.
		if self.gate.request_cancel() {
			if let Some(token) = self
				.token
				.lock()
				.unwrap_or_else(PoisonError::into_inner)
				.clone()
			{
				token.cancel();
			}
		}
		let (text, images) = convert_prompt_blocks(prompt)?;
		let mut text = text;
		if self.runtime.recipe.tools_enabled {
			if let Some(reminder) = self.lock_plan().take_reminder() {
				text = format!("{reminder}\n\n{text}");
			}
			if let Some(prefix) =
				self
					.lock_plan()
					.prompt_prefix(self.caps.elicitation_form(), true, true)
			{
				text = format!("{prefix}\n\n{text}");
			}
		}
		self.gate.begin();
		if let Err(error) = self.ensure_open() {
			self.gate.end();
			return Err(error);
		}
		let (tx, rx) = mpsc::channel();
		self.spawn_turn(text, images, tx)?;
		loop {
			match rx.recv_timeout(CANCEL_POLL) {
				Ok(done) => {
					if let Some(error) = done.error {
						return Err(error);
					}
					let mut response = json!({ "stopReason": done.stop_reason });
					if let Some(usage) = done.usage {
						response["usage"] = usage;
					}
					return Ok(response);
				},
				Err(RecvTimeoutError::Timeout) => {
					if self.gate.cancel_requested() {
						// The prompt response resolves cancelled immediately (TS
						// `#beginCancelCleanup`); cleanup stays bounded and may
						// close the session.
						self.spawn_cleanup_monitor(rx);
						return Ok(json!({ "stopReason": "cancelled" }));
					}
				},
				Err(RecvTimeoutError::Disconnected) => {
					self.gate.end();
					return Err(RequestError::internal_error().with_detail("ACP turn worker stopped"));
				},
			}
		}
	}

	fn spawn_turn(
		self: &Arc<Self>,
		text: String,
		images: Vec<RpcImage>,
		tx: Sender<TurnDone>,
	) -> Result<(), RequestError> {
		let session = Arc::clone(self);
		thread::Builder::new()
			.name(format!("omp-acp-turn-{}", self.id))
			.spawn(move || {
				let done = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
					session.execute_turn(text, images)
				})) {
					Ok(done) => done,
					Err(_) => TurnDone {
						stop_reason: "end_turn".to_string(),
						usage: None,
						error: Some(
							RequestError::internal_error().with_detail("ACP turn worker panicked"),
						),
					},
				};
				session.gate.end();
				*session.token.lock().unwrap_or_else(PoisonError::into_inner) = None;
				let _ = tx.send(done);
			})
			.map(|_| ())
			.map_err(|error| {
				self.gate.end();
				RequestError::internal_error()
					.with_detail(format!("cannot start ACP turn worker: {error}"))
			})
	}

	fn spawn_cleanup_monitor(self: &Arc<Self>, rx: Receiver<TurnDone>) {
		let session = Arc::clone(self);
		let _ = thread::Builder::new()
			.name(format!("omp-acp-cancel-{}", self.id))
			.spawn(move || {
				if rx.recv_timeout(CANCEL_CLEANUP_TIMEOUT).is_err() {
					session.force_close("ACP cancel cleanup timed out");
				}
			});
	}

	fn execute_turn(&self, text: String, images: Vec<RpcImage>) -> TurnDone {
		let mut session = self.lock();
		let input = TurnInput { text, images };
		let (request, mut prepared) = match session.prepare_turn(&input) {
			Ok(pair) => pair,
			Err(error) => {
				return TurnDone {
					stop_reason: "end_turn".to_string(),
					usage: None,
					error: Some(RequestError::new(INTERNAL_ERROR, error.to_string())),
				};
			},
		};
		let baseline = stats_tokens(&session.stats());
		let token = prepared.token.clone();
		*token_slot(&self.token) = Some(token.clone());
		if self.gate.cancel_requested() {
			token.cancel();
		}
		let mut map = TurnMap::default();
		let mut last_stop = String::new();
		let mut last_calls: Vec<PlanToolCall> = Vec::new();
		let mut stream_error: Option<String> = None;
		let runner = Arc::clone(&self.runtime.recipe.runner);
		match runner.run(&request) {
			Ok(stream) => {
				for event in stream {
					match event {
						Ok(event) => {
							session.observe_event(&mut prepared, &event);
							match &event {
								AgentEvent::MessageEnd { message, stop_reason, .. } => {
									if stop_reason.is_some() {
										last_stop = stop_reason.clone().unwrap_or_default();
									}
									if message.tool_calls.is_empty() {
										last_calls.clear();
									} else {
										last_calls = message
											.tool_calls
											.iter()
											.map(|call| PlanToolCall {
												name: call.name.clone(),
												args: call.arguments.clone(),
											})
											.collect();
									}
								},
								AgentEvent::TurnEnd { stop_reason, .. } => {
									last_stop = stop_reason.clone();
								},
								AgentEvent::ToolEnd { id, tool, .. } => {
									let args = map.tool_args.get(id);
									let mut plan = self.lock_plan();
									plan::observe_tool_end(&mut plan.settle, tool, args);
								},
								_ => {},
							}
							if !self.gate.cancel_requested() {
								for update in map_event(&event, &mut map, &self.cwd, &self.next_message_id)
								{
									self.emit_update(update);
								}
							}
						},
						Err(error) => {
							stream_error = Some(error.to_string());
							break;
						},
					}
				}
			},
			Err(error) => {
				stream_error = Some(error.to_string());
			},
		}
		let cancelled = self.gate.cancel_requested();
		let outcome =
			TurnOutcome { error: stream_error.clone().filter(|_| !cancelled), aborted: cancelled };
		session.finish_turn(&mut prepared, &outcome);
		let current = stats_tokens(&session.stats());
		let usage = usage_delta(&baseline, &current);
		let stop_reason = if cancelled {
			"cancelled".to_string()
		} else {
			stop_reason(&last_stop)
		};
		if let Some(error) = stream_error.as_deref().filter(|_| !cancelled) {
			self.emit_update(json!({
				"sessionUpdate": "agent_message_chunk",
				"content": { "type": "text", "text": limit_text(error) },
				"messageId": self.message_id(),
			}));
		}
		let has_ask = self.runtime.recipe.tools_enabled;
		let has_write = self.runtime.recipe.tools_enabled;
		let _ = self
			.lock_plan()
			.settle_turn(&stop_reason, &last_calls, has_ask, has_write);
		let title = session.state(None).session_name;
		if let Some(context) = session.context_usage() {
			let mut update = json!({
				"sessionUpdate": "usage_update",
				"size": context.get("contextWindow").cloned().unwrap_or(Value::Null),
				"used": context.get("tokens").cloned().unwrap_or(json!(0)),
			});
			let cost = session
				.stats()
				.get("cost")
				.and_then(Value::as_f64)
				.unwrap_or(0.0);
			if cost > 0.0 {
				update["cost"] = json!({ "amount": cost, "currency": "USD" });
			}
			self.emit_update(update);
		}
		self.emit_update(json!({
			"sessionUpdate": "session_info_update",
			"title": title,
			"updatedAt": output::iso8601(SystemTime::now()),
		}));
		TurnDone {
			stop_reason,
			usage,
			error: outcome
				.error
				.map(|message| RequestError::new(INTERNAL_ERROR, message)),
		}
	}

	/// Exact reverse-call surface for the pi-agent tool composition slice.
	#[must_use]
	pub fn reverse_client(&self) -> AcpReverseClient {
		AcpReverseClient::new(
			self.id.clone(),
			self.caps.clone(),
			self.runtime.reverse.clone(),
			self.runtime.transport.clone(),
		)
	}

	/// Plan state for the runner-side guard/composition.
	#[must_use]
	pub fn plan_enabled(&self) -> bool {
		self.lock_plan().enabled
	}

	#[must_use]
	pub fn plan_state(&self) -> Option<plan::AcpPlanState> {
		self.lock_plan().state.clone()
	}

	#[must_use]
	pub fn plan_generation(&self) -> u64 {
		self.lock_plan().generation
	}

	#[must_use]
	pub fn plan_reference(&self) -> Option<String> {
		self.lock_plan().plan_reference().map(str::to_string)
	}

	#[must_use]
	pub fn reminder_pending(&self) -> bool {
		self.lock_plan().reminder_pending()
	}

	/// Replay persisted wire messages as `session/update` notifications.
	///
	/// `branch_messages()` is the user-only `{entryId,text}` view; the mapper
	/// needs the full role-bearing wire shape, which `wire_messages()` keeps.
	pub fn replay_history(&self) {
		let session = self.lock();
		for message in session.wire_messages() {
			for update in replay_message(message, &self.cwd, &self.next_message_id) {
				self.emit_update(update);
			}
		}
	}

	pub fn protocol_response(&self) -> Value {
		json!({
			"sessionId": self.id,
			"configOptions": self.config_options(),
			"modes": self.mode_state(),
		})
	}

	/// Manager hook: called once after the close barrier, so the map drops the
	/// record even when the close came from the cancel-cleanup timeout.
	pub(crate) fn set_close_hook(&self, hook: CloseHook) {
		*self.on_close.lock().unwrap_or_else(PoisonError::into_inner) = Some(hook);
	}

	/// Close after a timeout without waiting for the wedged turn, matching TS
	/// `cancel()` closing the session once its cleanup barrier rejects.
	pub fn force_close(&self, reason: &str) {
		*self.lock_closed() = Some(reason.to_string());
		self.signal_cancel();
		self.gate.abort();
		if let Some(hook) = self
			.on_close
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.take()
		{
			hook(reason);
		}
	}

	/// Close: cancel any prompt, wait the bounded cleanup barrier, mark closed.
	pub fn close(&self, reason: &str) -> Result<(), RequestError> {
		*self.lock_closed() = Some(reason.to_string());
		self.signal_cancel();
		if !self.gate.wait_idle(CANCEL_CLEANUP_TIMEOUT) {
			// The worker is still unwinding; release queued prompts so they
			// fail closed instead of waiting for a turn that may never end.
			self.gate.abort();
		}
		let hook = self
			.on_close
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.take();
		if let Some(hook) = hook {
			hook(reason);
		}
		Ok(())
	}

	/// Bootstrap updates after the response reaches the client. Runs on a short
	/// timer so a client that creates the record on response handling does not
	/// see `unknown session` first (Zed race, `acp-agent.ts:2068-2098`).
	pub fn schedule_bootstrap(self: &Arc<Self>) {
		let session = Arc::clone(self);
		let _ = thread::Builder::new()
			.name(format!("omp-acp-bootstrap-{}", self.id))
			.spawn(move || {
				thread::sleep(BOOTSTRAP_RACE_GUARD);
				if session.is_closed() || session.runtime.transport.is_closed() {
					return;
				}
				session.emit_update(json!({
					"sessionUpdate": "available_commands_update",
					"availableCommands": [],
				}));
				session.emit_update(json!({
					"sessionUpdate": "session_info_update",
					"title": session.session_name(),
					"updatedAt": output::iso8601(SystemTime::now()),
				}));
			});
	}

	fn emit_update(&self, update: Value) {
		if self.is_closed() || self.runtime.transport.is_closed() {
			return;
		}
		let params = json!({ "sessionId": self.id, "update": update });
		let _ = self
			.runtime
			.transport
			.notify("session/update", Some(&params));
	}

	fn message_id(&self) -> String {
		format!("msg-{}-{}", self.id, self.next_message_id.fetch_add(1, Ordering::Relaxed))
	}
}

fn token_slot(
	slot: &Mutex<Option<CancellationToken>>,
) -> MutexGuard<'_, Option<CancellationToken>> {
	slot.lock().unwrap_or_else(PoisonError::into_inner)
}

fn model_id_of(model: &Value) -> Option<String> {
	let provider = model.get("provider").and_then(Value::as_str)?;
	let id = model.get("id").and_then(Value::as_str)?;
	Some(format!("{provider}/{id}"))
}

fn stats_tokens(stats: &Value) -> Tokens {
	let tokens = stats.get("tokens");
	Tokens {
		input: tokens
			.and_then(|t| t.get("input"))
			.and_then(Value::as_u64)
			.unwrap_or(0),
		output: tokens
			.and_then(|t| t.get("output"))
			.and_then(Value::as_u64)
			.unwrap_or(0),
		cache_read: tokens
			.and_then(|t| t.get("cacheRead"))
			.and_then(Value::as_u64)
			.unwrap_or(0),
		cache_write: tokens
			.and_then(|t| t.get("cacheWrite"))
			.and_then(Value::as_u64)
			.unwrap_or(0),
		total: tokens
			.and_then(|t| t.get("total"))
			.and_then(Value::as_u64)
			.unwrap_or(0),
	}
}

#[derive(Clone, Copy, Default)]
struct Tokens {
	input: u64,
	output: u64,
	cache_read: u64,
	cache_write: u64,
	total: u64,
}

fn usage_delta(previous: &Tokens, current: &Tokens) -> Option<Value> {
	let total = current.total.saturating_sub(previous.total);
	if total == 0 {
		return None;
	}
	let mut usage = json!({
		"inputTokens": current.input.saturating_sub(previous.input),
		"outputTokens": current.output.saturating_sub(previous.output),
		"totalTokens": total,
	});
	let cached_read = current.cache_read.saturating_sub(previous.cache_read);
	if cached_read > 0 {
		usage["cachedReadTokens"] = json!(cached_read);
	}
	let cached_write = current.cache_write.saturating_sub(previous.cache_write);
	if cached_write > 0 {
		usage["cachedWriteTokens"] = json!(cached_write);
	}
	Some(usage)
}

fn stop_reason(last: &str) -> String {
	match last {
		"length" | "max_tokens" => "max_tokens".to_string(),
		"aborted" => "cancelled".to_string(),
		"refusal" => "refusal".to_string(),
		"max_turn_requests" => "max_turn_requests".to_string(),
		_ => "end_turn".to_string(),
	}
}

/// Prompt block conversion. Every well-formed `ContentBlock` variant has a
/// fallback; only malformed blocks are `-32602`.
pub fn convert_prompt_blocks(prompt: &Value) -> Result<(String, Vec<RpcImage>), RequestError> {
	let blocks = prompt.as_array().ok_or_else(RequestError::invalid_params)?;
	let mut text = Vec::new();
	let mut images = Vec::new();
	for block in blocks {
		let kind = block
			.get("type")
			.and_then(Value::as_str)
			.ok_or_else(RequestError::invalid_params)?;
		match kind {
			"text" => {
				let value = block
					.get("text")
					.and_then(Value::as_str)
					.ok_or_else(RequestError::invalid_params)?;
				text.push(value.to_string());
			},
			"image" => {
				images.push(image_from(block)?);
			},
			"resource" => {
				let resource = block
					.get("resource")
					.and_then(Value::as_object)
					.ok_or_else(RequestError::invalid_params)?;
				if let Some(resource_text) = resource.get("text").and_then(Value::as_str) {
					text.push(resource_text.to_string());
				} else {
					let mime = resource
						.get("mimeType")
						.and_then(Value::as_str)
						.unwrap_or("");
					let blob = resource.get("blob").and_then(Value::as_str);
					if mime.starts_with("image/") {
						if let Some(blob) = blob {
							images.push(RpcImage { data: blob.to_string(), mime_type: mime.to_string() });
							continue;
						}
					}
					let uri = resource
						.get("uri")
						.and_then(Value::as_str)
						.ok_or_else(RequestError::invalid_params)?;
					text.push(format!("[embedded resource: {uri}]"));
				}
			},
			"resource_link" => {
				let value = block
					.get("title")
					.or_else(|| block.get("name"))
					.or_else(|| block.get("uri"))
					.and_then(Value::as_str)
					.ok_or_else(RequestError::invalid_params)?;
				text.push(value.to_string());
			},
			"audio" => text.push("[audio omitted]".to_string()),
			_ => {},
		}
	}
	Ok((text.join("\n\n").trim().to_string(), images))
}

fn image_from(block: &Value) -> Result<RpcImage, RequestError> {
	let data = block
		.get("data")
		.and_then(Value::as_str)
		.ok_or_else(RequestError::invalid_params)?;
	let mime = block
		.get("mimeType")
		.and_then(Value::as_str)
		.ok_or_else(RequestError::invalid_params)?;
	Ok(RpcImage { data: data.to_string(), mime_type: mime.to_string() })
}

/// Turn prompt gate: single-flight plus implicit cancel.
#[derive(Default)]
struct PromptGate {
	state: Mutex<GateState>,
	wake: Condvar,
}

#[derive(Default)]
struct GateState {
	active: bool,
	cancel_requested: bool,
}

impl PromptGate {
	/// Mark cancellation intent. Returns true when a turn was actually active
	/// and had not already been cancelled.
	fn request_cancel(&self) -> bool {
		let mut state = self.lock();
		if !state.active || state.cancel_requested {
			return false;
		}
		state.cancel_requested = true;
		self.wake.notify_all();
		true
	}

	fn cancel_requested(&self) -> bool {
		self.lock().cancel_requested
	}

	fn is_active(&self) -> bool {
		self.lock().active
	}

	fn begin(&self) {
		let mut state = self.lock();
		while state.active {
			state = self
				.wake
				.wait(state)
				.unwrap_or_else(PoisonError::into_inner);
		}
		state.active = true;
		state.cancel_requested = false;
	}

	fn end(&self) {
		let mut state = self.lock();
		state.active = false;
		state.cancel_requested = false;
		self.wake.notify_all();
	}

	fn abort(&self) {
		let mut state = self.lock();
		state.active = false;
		state.cancel_requested = true;
		self.wake.notify_all();
	}

	fn wait_idle(&self, timeout: Duration) -> bool {
		let deadline = Instant::now() + timeout;
		let mut state = self.lock();
		while state.active {
			let remaining = deadline.saturating_duration_since(Instant::now());
			if remaining.is_zero() {
				return false;
			}
			let (next, result) = self
				.wake
				.wait_timeout(state, remaining)
				.unwrap_or_else(PoisonError::into_inner);
			state = next;
			if result.timed_out() && state.active {
				return false;
			}
		}
		true
	}

	fn lock(&self) -> MutexGuard<'_, GateState> {
		self.state.lock().unwrap_or_else(PoisonError::into_inner)
	}
}

/// Reverse call sender for the client capabilities this build can invoke.
///
/// The pi-agent composition slice calls these from the native `read`/`write`/
/// `bash` tools only when the matching capability was advertised; every method
/// refuses with a typed `Send` error otherwise, so a missing capability can
/// never silently fabricate a client result.
#[derive(Clone)]
pub struct AcpReverseClient {
	session_id: String,
	caps: ClientCapabilities,
	reverse: ReverseCaller,
	transport: Transport,
}

impl AcpReverseClient {
	#[must_use]
	pub fn new(
		session_id: String,
		caps: ClientCapabilities,
		reverse: ReverseCaller,
		transport: Transport,
	) -> Self {
		Self { session_id, caps, reverse, transport }
	}

	pub fn read_text_file(
		&self,
		path: &str,
		line: Option<u64>,
		limit: Option<u64>,
		cancel: &CancellationToken,
	) -> Result<String, ReverseError> {
		if !self.caps.fs.read_text_file {
			return Err(unsupported("fs/read_text_file", "readTextFile"));
		}
		let mut params = json!({ "sessionId": self.session_id, "path": path });
		if let Some(line) = line {
			params["line"] = json!(line);
		}
		if let Some(limit) = limit {
			params["limit"] = json!(limit);
		}
		let reply = self.call("fs/read_text_file", params, cancel)?;
		reply
			.get("content")
			.and_then(Value::as_str)
			.map(str::to_string)
			.ok_or_else(|| ReverseError::Send {
				method: "fs/read_text_file".to_string(),
				reason: "response has no content".to_string(),
			})
	}

	pub fn write_text_file(
		&self,
		path: &str,
		content: &str,
		cancel: &CancellationToken,
	) -> Result<(), ReverseError> {
		if !self.caps.fs.write_text_file {
			return Err(unsupported("fs/write_text_file", "writeTextFile"));
		}
		self.call(
			"fs/write_text_file",
			json!({ "sessionId": self.session_id, "path": path, "content": content }),
			cancel,
		)?;
		Ok(())
	}

	pub fn terminal_create(
		&self,
		command: &str,
		args: Option<Vec<String>>,
		env: Option<Vec<(String, String)>>,
		cwd: Option<&str>,
		output_byte_limit: Option<u64>,
		cancel: &CancellationToken,
	) -> Result<String, ReverseError> {
		if !self.caps.terminal {
			return Err(unsupported("terminal/create", "terminal"));
		}
		let mut params = json!({ "sessionId": self.session_id, "command": command });
		if let Some(args) = args {
			params["args"] = json!(args);
		}
		if let Some(env) = env {
			params["env"] = json!(
				env.into_iter()
					.map(|(name, value)| json!({ "name": name, "value": value }))
					.collect::<Vec<_>>()
			);
		}
		if let Some(cwd) = cwd {
			params["cwd"] = json!(cwd);
		}
		if let Some(limit) = output_byte_limit {
			params["outputByteLimit"] = json!(limit);
		}
		let reply = self.call("terminal/create", params, cancel)?;
		reply
			.get("terminalId")
			.and_then(Value::as_str)
			.map(str::to_string)
			.ok_or_else(|| ReverseError::Send {
				method: "terminal/create".to_string(),
				reason: "response has no terminalId".to_string(),
			})
	}

	pub fn terminal_output(
		&self,
		terminal_id: &str,
		cancel: &CancellationToken,
	) -> Result<Value, ReverseError> {
		self.terminal_call("terminal/output", terminal_id, cancel)
	}

	pub fn terminal_wait_for_exit(
		&self,
		terminal_id: &str,
		cancel: &CancellationToken,
	) -> Result<Value, ReverseError> {
		self.terminal_call("terminal/wait_for_exit", terminal_id, cancel)
	}

	pub fn terminal_kill(
		&self,
		terminal_id: &str,
		cancel: &CancellationToken,
	) -> Result<Value, ReverseError> {
		self.terminal_call("terminal/kill", terminal_id, cancel)
	}

	pub fn terminal_release(
		&self,
		terminal_id: &str,
		cancel: &CancellationToken,
	) -> Result<Value, ReverseError> {
		self.terminal_call("terminal/release", terminal_id, cancel)
	}

	/// `unstable_createElicitation` (form mode).
	pub fn create_elicitation(
		&self,
		message: &str,
		properties: Value,
		required: Option<Vec<String>>,
		cancel: &CancellationToken,
	) -> Result<Value, ReverseError> {
		if !self.caps.elicitation_form() {
			return Err(unsupported("elicitation/create", "elicitation.form"));
		}
		let mut requested = json!({ "type": "object", "properties": properties });
		if let Some(required) = required {
			requested["required"] = json!(required);
		}
		self.call(
			"elicitation/create",
			json!({
				"mode": "form",
				"sessionId": self.session_id,
				"message": message,
				"requestedSchema": requested,
			}),
			cancel,
		)
	}

	/// `unstable_completeElicitation` is URL-mode only; form requests are
	/// settled by their response and never send this notification.
	pub fn complete_elicitation(&self, elicitation_id: &str) -> bool {
		self.transport.notify(
			"elicitation/complete",
			Some(&json!({ "elicitationId": elicitation_id, "sessionId": self.session_id })),
		)
	}

	fn terminal_call(
		&self,
		method: &str,
		terminal_id: &str,
		cancel: &CancellationToken,
	) -> Result<Value, ReverseError> {
		if !self.caps.terminal {
			return Err(unsupported(method, "terminal"));
		}
		self.call(method, json!({ "sessionId": self.session_id, "terminalId": terminal_id }), cancel)
	}

	fn call(
		&self,
		method: &str,
		params: Value,
		cancel: &CancellationToken,
	) -> Result<Value, ReverseError> {
		let cancelled = || cancel.is_cancelled();
		self
			.reverse
			.request_cancellable(method, Some(params), None, &cancelled)
	}
}

fn unsupported(method: &str, capability: &str) -> ReverseError {
	ReverseError::Send {
		method: method.to_string(),
		reason: format!("client did not advertise {capability}"),
	}
}

/// `session/request_permission` sender with the exact TS gate.
#[derive(Clone)]
struct AcpPermissionBroker {
	session_id: Arc<Mutex<String>>,
	reverse: ReverseCaller,
	mode: ApprovalMode,
	allow: Arc<BTreeSet<String>>,
	cwd: PathBuf,
	token: Arc<Mutex<Option<CancellationToken>>>,
	decisions: Arc<Mutex<BTreeMap<String, PersistedDecision>>>,
	next_id: Arc<AtomicU64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PersistedDecision {
	AllowAlways,
	RejectAlways,
}

impl AcpPermissionBroker {
	fn bind_session(&self, session_id: &str) {
		*self
			.session_id
			.lock()
			.unwrap_or_else(PoisonError::into_inner) = session_id.to_string();
	}

	fn ask(&self, name: &str, args: &Value, token: &CancellationToken) -> Result<(), String> {
		let Some(intent) = PermissionIntent::for_call(name, args) else {
			return Ok(());
		};
		match self
			.decisions
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.get(&intent.cache_key)
		{
			Some(PersistedDecision::AllowAlways) => return Ok(()),
			Some(PersistedDecision::RejectAlways) => {
				return Err("Tool call rejected by user (preference)".to_string());
			},
			None => {},
		}
		if token.is_cancelled() {
			return Err("Permission request cancelled".to_string());
		}
		let tool_call_id = format!("perm-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
		let session_id = self
			.session_id
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.clone();
		let params = intent.request(&session_id, &tool_call_id, args, &self.cwd);
		let cancelled = || token.is_cancelled();
		let outcome = self
			.reverse
			.request_cancellable("session/request_permission", Some(params), None, &cancelled)
			.map_err(|_| "Permission request cancelled".to_string())?;
		let outcome = outcome.get("outcome").cloned().unwrap_or(Value::Null);
		if outcome.get("outcome").and_then(Value::as_str) == Some("cancelled") {
			return Err("Permission request cancelled".to_string());
		}
		let option = outcome
			.get("optionId")
			.and_then(Value::as_str)
			.ok_or_else(|| "Tool permission response used unknown option ID: <missing>".to_string())?;
		let option = PermissionIntent::option(option)?;
		if option.allow_always {
			self
				.decisions
				.lock()
				.unwrap_or_else(PoisonError::into_inner)
				.insert(intent.cache_key.clone(), PersistedDecision::AllowAlways);
		} else if option.reject_always {
			self
				.decisions
				.lock()
				.unwrap_or_else(PoisonError::into_inner)
				.insert(intent.cache_key.clone(), PersistedDecision::RejectAlways);
		}
		if option.allow {
			Ok(())
		} else {
			Err(format!("Tool call rejected by user ({name})"))
		}
	}
}

#[async_trait]
impl pi_agent::ToolApprover for AcpPermissionBroker {
	fn ask_channel(&self) -> Option<Arc<dyn pi_agent::tools::ask::AskChannel>> {
		None
	}

	async fn approve(&self, name: &str, args: &Value) -> std::result::Result<(), String> {
		let tier = classify_tool(name, args);
		if self.decide_tier(name, tier) == Decision::Auto {
			return Ok(());
		}
		// TS `PERMISSION_REQUIRED_TOOLS`: bash/edit/delete/move only, and edit
		// only for a destructive intent. A matrix `Ask` outside that set has no
		// ACP prompt to answer it, so it fails closed instead of bypassing the
		// user's approval policy.
		let gated = match name {
			"bash" | "delete" | "move" => true,
			"edit" => PermissionIntent::for_edit(args).is_some(),
			_ => false,
		};
		if !gated {
			return Err(format!(
				"approval required for {name}; ACP permission requests cover bash/edit/delete/move only"
			));
		}
		let token = self
			.token
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.clone()
			.unwrap_or_default();
		let broker = self.clone();
		let name = name.to_string();
		let args = args.clone();
		// The reverse call can wait a whole user decision; never block the
		// runtime worker that called `approve`.
		tokio::task::spawn_blocking(move || broker.ask(&name, &args, &token))
			.await
			.map_err(|error| format!("permission task failed: {error}"))?
	}
}

impl AcpPermissionBroker {
	fn decide_tier(&self, tool: &str, tier: ToolTier) -> Decision {
		let allowed =
			self.allow.contains(tool) || (tool == "apply_patch" && self.allow.contains("edit"));
		if allowed || self.mode == ApprovalMode::Yolo {
			return Decision::Auto;
		}
		if tier == ToolTier::Read {
			return Decision::Auto;
		}
		Decision::Ask
	}
}

struct PermissionIntent {
	cache_key: String,
	title: String,
	kind: Option<&'static str>,
	paths: Vec<String>,
	content: Option<Value>,
}

impl PermissionIntent {
	fn for_call(name: &str, args: &Value) -> Option<Self> {
		match name {
			"bash" => Some(Self {
				cache_key: "bash".to_string(),
				title: args
					.get("command")
					.and_then(Value::as_str)
					.map(|command| command.chars().take(80).collect())
					.filter(|command: &String| !command.is_empty())
					.unwrap_or_else(|| "bash".to_string()),
				kind: Some("execute"),
				paths: Vec::new(),
				content: args
					.get("command")
					.and_then(Value::as_str)
					.map(|command| json!([{ "type": "content", "content": { "type": "text", "text": format!("$ {command}") } }])),
			}),
			"delete" => {
				let path = args.get("path").and_then(Value::as_str);
				Some(Self {
					cache_key: "delete".to_string(),
					title: path.map(|path| format!("Delete {path}")).unwrap_or_else(|| "delete".to_string()),
					kind: None,
					paths: path.map(|path| vec![path.to_string()]).unwrap_or_default(),
					content: None,
				})
			},
			"move" => {
				let from = args
					.get("oldPath")
					.or_else(|| args.get("path"))
					.or_else(|| args.get("from"))
					.and_then(Value::as_str);
				let to = args
					.get("newPath")
					.or_else(|| args.get("to"))
					.or_else(|| args.get("destination"))
					.and_then(Value::as_str);
				let title = match (from, to) {
					(Some(from), Some(to)) => format!("Move {from} to {to}"),
					(Some(from), None) => format!("Move {from}"),
					_ => "move".to_string(),
				};
				Some(Self {
					cache_key: "move".to_string(),
					title,
					kind: None,
					paths: from.into_iter().chain(to).map(str::to_string).collect(),
					content: None,
				})
			},
			"edit" => Self::for_edit(args),
			_ => None,
		}
	}

	fn for_edit(args: &Value) -> Option<Self> {
		let edits = args.get("edits").and_then(Value::as_array)?;
		let file_path = args.get("path").and_then(Value::as_str);
		if edits
			.iter()
			.any(|edit| edit.get("op").and_then(Value::as_str) == Some("delete"))
		{
			return Some(Self {
				cache_key: "edit:delete".to_string(),
				title: file_path
					.map(|path| format!("Delete {path}"))
					.unwrap_or_else(|| "Delete edit target".to_string()),
				kind: None,
				paths: file_path
					.map(|path| vec![path.to_string()])
					.unwrap_or_default(),
				content: None,
			});
		}
		let rename = edits.iter().find_map(|edit| {
			let op = edit.get("op").and_then(Value::as_str);
			let rename = edit.get("rename").and_then(Value::as_str)?;
			(op != Some("create")).then_some(rename)
		})?;
		let from = file_path.map(str::to_string);
		let title = match &from {
			Some(from) => format!("Move {from} to {rename}"),
			None => format!("Move {rename}"),
		};
		Some(Self {
			cache_key: "edit:move".to_string(),
			title,
			kind: None,
			paths: from.into_iter().chain([rename.to_string()]).collect(),
			content: None,
		})
	}

	fn request(&self, session_id: &str, tool_call_id: &str, args: &Value, cwd: &Path) -> Value {
		let mut tool_call = json!({
			"toolCallId": tool_call_id,
			"title": self.title.clone(),
			"status": "pending",
			"rawInput": args,
		});
		if let Some(kind) = self.kind {
			tool_call["kind"] = json!(kind);
		}
		if let Some(content) = &self.content {
			tool_call["content"] = content.clone();
		}
		let mut seen = BTreeSet::new();
		let locations: Vec<Value> = self
			.paths
			.iter()
			.filter(|path| !is_internal_url(path))
			.map(|path| {
				if Path::new(path).is_absolute() {
					path.clone()
				} else {
					cwd.join(path).to_string_lossy().to_string()
				}
			})
			.filter(|path| seen.insert(path.clone()))
			.map(|path| json!({ "path": path }))
			.collect();
		if !locations.is_empty() {
			tool_call["locations"] = Value::Array(locations);
		}
		json!({
			"sessionId": session_id,
			"toolCall": tool_call,
			"options": [
				{ "optionId": "allow_once", "name": "Allow once", "kind": "allow_once" },
				{ "optionId": "allow_always", "name": "Always allow", "kind": "allow_always" },
				{ "optionId": "reject_once", "name": "Reject", "kind": "reject_once" },
				{ "optionId": "reject_always", "name": "Always reject", "kind": "reject_always" }
			],
		})
	}

	fn option(id: &str) -> Result<PermissionOption, String> {
		match id {
			"allow_once" => {
				Ok(PermissionOption { allow: true, allow_always: false, reject_always: false })
			},
			"allow_always" => {
				Ok(PermissionOption { allow: true, allow_always: true, reject_always: false })
			},
			"reject_once" => {
				Ok(PermissionOption { allow: false, allow_always: false, reject_always: false })
			},
			"reject_always" => {
				Ok(PermissionOption { allow: false, allow_always: false, reject_always: true })
			},
			other => Err(format!("Tool permission response used unknown option ID: {other}")),
		}
	}
}

struct PermissionOption {
	allow: bool,
	allow_always: bool,
	reject_always: bool,
}

/// `ask` tool surface backed by ACP elicitation forms.
struct AcpAskChannel {
	session_id: Arc<Mutex<String>>,
	reverse: ReverseCaller,
	caps: ClientCapabilities,
	next_id: Arc<AtomicU64>,
}

#[async_trait]
impl pi_agent::tools::ask::AskChannel for AcpAskChannel {
	async fn ask(
		&self,
		questions: &[pi_agent::tools::ask::AskQuestion],
		cancel: CancellationToken,
	) -> pi_agent::tools::ask::AskOutcome {
		use pi_agent::tools::ask::{AskAnswer, AskOutcome};
		if cancel.is_cancelled() {
			return AskOutcome::Aborted;
		}
		if !self.caps.elicitation_form() {
			return AskOutcome::Cancelled;
		}
		let (properties, message) = ask_form(questions);
		let session_id = self
			.session_id
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.clone();
		let reverse = self.reverse.clone();
		let _ = self.next_id.fetch_add(1, Ordering::Relaxed);
		let answer = tokio::task::spawn_blocking(move || {
			let cancelled = || cancel.is_cancelled();
			reverse.request_cancellable(
				"elicitation/create",
				Some(json!({
					"mode": "form",
					"sessionId": session_id,
					"message": message,
					"requestedSchema": { "type": "object", "properties": properties },
				})),
				None,
				&cancelled,
			)
		})
		.await;
		let value = match answer {
			Ok(Ok(value)) => value,
			Ok(Err(ReverseError::Cancelled)) => return AskOutcome::Aborted,
			_ => return AskOutcome::Cancelled,
		};
		if value.get("action").and_then(Value::as_str) != Some("accept") {
			return AskOutcome::Cancelled;
		}
		let Some(content) = value.get("content").and_then(Value::as_object) else {
			return AskOutcome::Cancelled;
		};
		let mut answers = Vec::with_capacity(questions.len());
		for (index, question) in questions.iter().enumerate() {
			let key = format!("q{index}");
			let other = content
				.get(&format!("{key}__other"))
				.and_then(Value::as_str)
				.map(str::trim)
				.filter(|value| !value.is_empty())
				.map(str::to_string);
			let labels: Vec<&str> = question
				.options
				.iter()
				.map(|option| option.label.as_str())
				.collect();
			let selected = match content.get(&key) {
				Some(Value::Array(values)) if question.multi => values
					.iter()
					.filter_map(Value::as_str)
					.filter(|value| labels.contains(value))
					.map(str::to_string)
					.collect(),
				Some(Value::String(value)) if other.is_none() && labels.contains(&value.as_str()) => {
					vec![value.clone()]
				},
				_ => Vec::new(),
			};
			answers.push(AskAnswer {
				id: question.id.clone(),
				selected_options: selected,
				custom_input: other,
				note: None,
				timed_out: false,
			});
		}
		AskOutcome::Answered(answers)
	}
}

fn ask_form(questions: &[pi_agent::tools::ask::AskQuestion]) -> (Value, String) {
	let mut properties = serde_json::Map::new();
	for (index, question) in questions.iter().enumerate() {
		let key = format!("q{index}");
		let description = question
			.header
			.as_deref()
			.map(str::trim)
			.filter(|header| !header.is_empty());
		let entries: Vec<Value> = question
			.options
			.iter()
			.map(|option| {
				let mut entry = json!({ "const": option.label, "title": option.label });
				if let Some(description) = option
					.description
					.as_deref()
					.map(str::trim)
					.filter(|d| !d.is_empty())
				{
					entry["description"] = json!(description);
				}
				entry
			})
			.collect();
		if entries.is_empty() {
			// TS only emits the `q{i}` property when the question has options;
			// the free-text companion below is always present.
		} else if question.multi {
			let mut schema = json!({
				"type": "array",
				"title": question.question,
				"items": { "anyOf": entries },
			});
			if let Some(description) = description {
				schema["description"] = json!(description);
			}
			properties.insert(key.clone(), schema);
		} else {
			let mut schema = json!({
				"type": "string",
				"title": question.question,
				"oneOf": entries,
			});
			if let Some(description) = description {
				schema["description"] = json!(description);
			}
			if let Some(recommended) = question
				.recommended
				.and_then(|index| usize::try_from(index).ok())
				.filter(|index| *index < question.options.len())
			{
				schema["default"] = json!(question.options[recommended].label);
			}
			properties.insert(key.clone(), schema);
		}
		properties.insert(
			format!("{key}__other"),
			json!({ "type": "string", "title": pi_agent::tools::ask::OTHER_OPTION }),
		);
	}
	let message = if questions.len() == 1 {
		questions[0].question.clone()
	} else {
		format!("Answer {} questions", questions.len())
	};
	(Value::Object(properties), message)
}

// --------------------------------------------------------------- updates --

#[derive(Default)]
struct TurnMap {
	message_id: Option<String>,
	text_emitted: bool,
	tool_args: BTreeMap<String, Value>,
}

fn map_event(event: &AgentEvent, map: &mut TurnMap, cwd: &Path, next_id: &AtomicU64) -> Vec<Value> {
	match event {
		AgentEvent::MessageStart { role } => {
			if role == "assistant" {
				map.message_id = Some(format!("msg-{}", next_id.fetch_add(1, Ordering::Relaxed)));
				map.text_emitted = false;
			}
			Vec::new()
		},
		AgentEvent::MessageUpdate { delta } => {
			if delta.is_empty() {
				return Vec::new();
			}
			map.text_emitted = true;
			vec![json!({
				"sessionUpdate": "agent_message_chunk",
				"content": { "type": "text", "text": limit_text(delta) },
				"messageId": map.message_id.clone(),
			})]
		},
		AgentEvent::MessageEnd { message, stop_reason, .. } => {
			let mut updates = Vec::new();
			if message.role == pi_agent::Role::Assistant {
				if !map.text_emitted && !message.content.is_empty() {
					map.text_emitted = true;
					updates.push(json!({
						"sessionUpdate": "agent_message_chunk",
						"content": { "type": "text", "text": limit_text(&message.content) },
						"messageId": map.message_id.clone(),
					}));
				}
				for part in &message.parts {
					if let AgentContentPart::Image { data, mime_type } = part {
						updates.push(json!({
							"sessionUpdate": "agent_message_chunk",
							"content": { "type": "image", "data": data, "mimeType": mime_type },
							"messageId": map.message_id.clone(),
						}));
					}
				}
				if stop_reason.as_deref() == Some("error") && message.content.is_empty() {
					// The native Message carries no separate error text; the
					// worker reports the stream error after the turn.
				}
			}
			map.message_id = None;
			map.text_emitted = false;
			updates
		},
		AgentEvent::ToolStart { id, tool, args } => {
			map.tool_args.insert(id.clone(), args.clone());
			vec![tool_call_update(id, tool, args, cwd)]
		},
		AgentEvent::ToolUpdate { id, tool, args, partial } => {
			let mut content = build_tool_start_content(tool, args);
			merge_content(&mut content, structured_content(partial));
			let mut update = json!({
				"sessionUpdate": "tool_call_update",
				"toolCallId": id,
				"status": "in_progress",
				"rawOutput": partial,
			});
			if !content.is_empty() {
				update["content"] = Value::Array(content);
			}
			let locations = tool_locations(args, cwd);
			if !locations.is_empty() {
				update["locations"] = Value::Array(locations);
			}
			vec![update]
		},
		AgentEvent::ToolEnd { id, tool, result, error, images, details } => {
			let args = map.tool_args.remove(id).unwrap_or(Value::Null);
			let mut content = build_tool_start_content(tool, &args);
			if let Some(details) = details {
				for block in diff_content(details) {
					content.push(block);
				}
				for block in details_images(details) {
					content.push(block);
				}
			}
			for image in images {
				if let AgentContentPart::Image { data, mime_type } = image {
					content.push(json!({
						"type": "content",
						"content": { "type": "image", "data": data, "mimeType": mime_type },
					}));
				}
			}
			if let Some(text) = normalize_text(result) {
				if !has_text_content(&content, &text) {
					content.push(json!({
						"type": "content",
						"content": { "type": "text", "text": text },
					}));
				}
			}
			let mut update = json!({
				"sessionUpdate": "tool_call_update",
				"toolCallId": id,
				"status": if error.is_some() { "failed" } else { "completed" },
				"rawOutput": {
					"content": result,
					"details": details,
					"errorMessage": error,
				},
			});
			if !content.is_empty() {
				update["content"] = Value::Array(content);
			}
			let locations = details
				.as_ref()
				.map(|details| tool_locations(details, cwd))
				.unwrap_or_default();
			if !locations.is_empty() {
				update["locations"] = Value::Array(locations);
			}
			let mut updates = vec![update];
			if tool == "todo" && error.is_none() {
				if let Some(entries) = details.as_ref().and_then(todo_entries) {
					updates.push(json!({ "sessionUpdate": "plan", "entries": entries }));
				}
			}
			updates
		},
		AgentEvent::TurnEnd { .. } | AgentEvent::AgentEnd { .. } | AgentEvent::UserSteer { .. } => {
			Vec::new()
		},
	}
}

fn tool_call_update(id: &str, tool: &str, args: &Value, cwd: &Path) -> Value {
	let mut update = json!({
		"sessionUpdate": "tool_call",
		"toolCallId": id,
		"title": tool_title(tool, args),
		"kind": tool_kind(tool, args),
		"status": "pending",
		"rawInput": args,
	});
	let content = build_tool_start_content(tool, args);
	if !content.is_empty() {
		update["content"] = Value::Array(content);
	}
	let locations = tool_locations(args, cwd);
	if !locations.is_empty() {
		update["locations"] = Value::Array(locations);
	}
	update
}

fn tool_kind(tool: &str, args: &Value) -> &'static str {
	if tool == "write" && xd_device(args).is_some() {
		return "execute";
	}
	match tool {
		"read" => "read",
		"write" | "edit" => "edit",
		"delete" => "delete",
		"move" => "move",
		"bash" | "shell" | "exec" | "eval" => "execute",
		"grep" | "glob" | "ast_grep" | "ast_edit" => "search",
		"web_search" => "fetch",
		"todo" => "think",
		_ => "other",
	}
}

fn tool_title(tool: &str, args: &Value) -> String {
	if is_command_tool(tool) {
		if let Some(command) = args.get("command").and_then(Value::as_str) {
			return limit_text(&format!("$ {command}"));
		}
	}
	if tool == "eval" {
		if let Some(text) = eval_start_text(args) {
			return text;
		}
	}
	let subject = args
		.get("path")
		.or_else(|| args.get("command"))
		.or_else(|| args.get("pattern"))
		.or_else(|| args.get("query"))
		.and_then(Value::as_str);
	match subject {
		Some(subject) if is_internal_url(subject) => subject.to_string(),
		Some(subject) => format!("{tool}: {subject}"),
		None => tool.to_string(),
	}
}

fn build_tool_start_content(tool: &str, args: &Value) -> Vec<Value> {
	let text = if is_command_tool(tool) {
		args
			.get("command")
			.and_then(Value::as_str)
			.map(|command| limit_text(&format!("$ {command}")))
	} else if tool == "eval" {
		eval_start_text(args)
	} else {
		None
	};
	text
		.map(|text| vec![json!({ "type": "content", "content": { "type": "text", "text": text } })])
		.unwrap_or_default()
}

fn is_command_tool(tool: &str) -> bool {
	matches!(tool, "bash" | "shell" | "exec")
}

fn eval_start_text(args: &Value) -> Option<String> {
	let cells: Vec<&Value> = match args.get("cells").and_then(Value::as_array) {
		Some(cells) => cells.iter().collect(),
		None if args.get("code").and_then(Value::as_str).is_some() => vec![args],
		None => return None,
	};
	let mut lines = Vec::new();
	for cell in cells {
		let language = cell.get("language").and_then(Value::as_str).unwrap_or("?");
		let title = cell.get("title").and_then(Value::as_str);
		let code = cell.get("code").and_then(Value::as_str)?;
		lines.push(match title {
			Some(title) => format!("[{language}] {title}"),
			None => format!("[{language}]"),
		});
		lines.push(code.to_string());
	}
	(!lines.is_empty()).then(|| limit_text(&lines.join("\n")))
}

fn xd_device(args: &Value) -> Option<&str> {
	let path = args.get("path").and_then(Value::as_str)?;
	let rest = path.strip_prefix("xd://")?;
	let name = rest.split(['/', '?', '#']).next()?;
	(!name.is_empty()).then_some(name)
}

fn tool_locations(args: &Value, cwd: &Path) -> Vec<Value> {
	let mut seen = BTreeSet::new();
	let mut locations = Vec::new();
	if let Some(files) = args.get("perFileResults").and_then(Value::as_array) {
		for file in files {
			if let Some(raw) = file.get("path").and_then(Value::as_str) {
				if !is_internal_url(raw) {
					let resolved = if Path::new(raw).is_absolute() {
						PathBuf::from(raw)
					} else {
						cwd.join(raw)
					};
					let path = resolved.to_string_lossy().to_string();
					if seen.insert(path.clone()) {
						locations.push(json!({ "path": path }));
					}
				}
			}
		}
	}
	for key in ["path", "oldPath", "newPath"] {
		let Some(raw) = args.get(key).and_then(Value::as_str) else {
			continue;
		};
		if is_internal_url(raw) {
			continue;
		}
		let resolved = if Path::new(raw).is_absolute() {
			PathBuf::from(raw)
		} else {
			cwd.join(raw)
		};
		let path = resolved.to_string_lossy().to_string();
		if seen.insert(path.clone()) {
			locations.push(json!({ "path": path }));
		}
	}
	locations
}

fn is_internal_url(value: &str) -> bool {
	let Some((scheme, _rest)) = value.split_once("://") else {
		return false;
	};
	!scheme.is_empty()
		&& scheme
			.chars()
			.next()
			.is_some_and(|c| c.is_ascii_alphabetic())
		&& scheme
			.chars()
			.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
}

fn diff_content(details: &Value) -> Vec<Value> {
	let entries: Vec<&Value> = details
		.get("perFileResults")
		.and_then(Value::as_array)
		.map(|entries| entries.iter().collect())
		.unwrap_or_else(|| vec![details]);
	let mut blocks = Vec::new();
	for entry in entries {
		if entry.get("isError").and_then(Value::as_bool) == Some(true) {
			continue;
		}
		let Some(path) = entry.get("path").and_then(Value::as_str) else {
			continue;
		};
		let old = entry.get("oldText").and_then(Value::as_str);
		let new = entry.get("newText").and_then(Value::as_str);
		if old.is_none() && new.is_none() {
			continue;
		}
		blocks.push(json!({
			"type": "diff",
			"path": path,
			"oldText": old,
			"newText": new.unwrap_or(""),
		}));
	}
	blocks
}

fn details_images(details: &Value) -> Vec<Value> {
	let Some(images) = details.get("images").and_then(Value::as_array) else {
		return Vec::new();
	};
	images
		.iter()
		.filter_map(|image| {
			let data = image.get("data").and_then(Value::as_str)?;
			let mime = image
				.get("mimeType")
				.or_else(|| image.get("mime_type"))
				.and_then(Value::as_str)?;
			Some(json!({
				"type": "content",
				"content": { "type": "image", "data": data, "mimeType": mime },
			}))
		})
		.collect()
}

fn structured_content(value: &Value) -> Vec<Value> {
	let blocks = match value {
		Value::Array(blocks) => blocks,
		Value::Object(object) => match object.get("content").and_then(Value::as_array) {
			Some(blocks) => blocks,
			None => return Vec::new(),
		},
		_ => return Vec::new(),
	};
	blocks.iter().filter_map(structured_block).collect()
}

fn structured_block(block: &Value) -> Option<Value> {
	match block.get("type").and_then(Value::as_str)? {
		"text" => {
			let text = block.get("text").and_then(Value::as_str)?;
			(!text.is_empty()).then(
				|| json!({ "type": "content", "content": { "type": "text", "text": limit_text(text) } }),
			)
		},
		"image" | "audio" => {
			let data = block.get("data").and_then(Value::as_str)?;
			let mime = block.get("mimeType").and_then(Value::as_str)?;
			Some(json!({
				"type": "content",
				"content": { "type": block.get("type").cloned().unwrap_or(json!("image")), "data": data, "mimeType": mime },
			}))
		},
		"resource_link" => {
			let uri = block.get("uri").and_then(Value::as_str)?;
			let name = block.get("name").and_then(Value::as_str)?;
			let mut link = json!({ "type": "resource_link", "uri": uri, "name": name });
			for key in ["title", "description", "mimeType"] {
				if let Some(value) = block.get(key).and_then(Value::as_str) {
					link[key] = json!(value);
				}
			}
			if let Some(size) = block.get("size").and_then(Value::as_f64) {
				link["size"] = json!(size);
			}
			Some(json!({ "type": "content", "content": link }))
		},
		"resource" => {
			let resource = block.get("resource")?;
			let uri = resource.get("uri").and_then(Value::as_str)?;
			if let Some(text) = resource.get("text").and_then(Value::as_str) {
				let mut embedded =
					json!({ "type": "resource", "resource": { "uri": uri, "text": text } });
				if let Some(mime) = resource.get("mimeType").and_then(Value::as_str) {
					embedded["resource"]["mimeType"] = json!(mime);
				}
				return Some(json!({ "type": "content", "content": embedded }));
			}
			let blob = resource.get("blob").and_then(Value::as_str)?;
			let mut embedded = json!({ "type": "resource", "resource": { "uri": uri, "blob": blob } });
			if let Some(mime) = resource.get("mimeType").and_then(Value::as_str) {
				embedded["resource"]["mimeType"] = json!(mime);
			}
			Some(json!({ "type": "content", "content": embedded }))
		},
		_ => None,
	}
}

fn merge_content(target: &mut Vec<Value>, extra: Vec<Value>) {
	for block in extra {
		let duplicate = block.get("type").and_then(Value::as_str) == Some("content")
			&& block["content"].get("type").and_then(Value::as_str) == Some("text")
			&& block["content"]
				.get("text")
				.and_then(Value::as_str)
				.is_some_and(|text| has_text_content(target, text));
		if !duplicate {
			target.push(block);
		}
	}
}

fn has_text_content(content: &[Value], text: &str) -> bool {
	content.iter().any(|block| {
		block.get("type").and_then(Value::as_str) == Some("content")
			&& block["content"].get("type").and_then(Value::as_str) == Some("text")
			&& block["content"].get("text").and_then(Value::as_str) == Some(text)
	})
}

fn readable_text(value: &Value) -> Option<String> {
	match value {
		Value::String(text) => normalize_text(text),
		Value::Object(object) => {
			if let Some(text) = object.get("text").and_then(Value::as_str) {
				return normalize_text(text);
			}
			if let Some(text) = object.get("errorMessage").and_then(Value::as_str) {
				return normalize_text(text);
			}
			if let Some(text) = object.get("message").and_then(Value::as_str) {
				return normalize_text(text);
			}
			if let Some(blocks) = object.get("content").and_then(Value::as_array) {
				let text = blocks
					.iter()
					.filter_map(|block| block.get("text").and_then(Value::as_str))
					.collect::<Vec<_>>()
					.join("\n");
				return normalize_text(&text);
			}
			normalize_text(&value.to_string())
		},
		_ => None,
	}
}

fn normalize_text(text: &str) -> Option<String> {
	let trimmed = text.trim();
	(!trimmed.is_empty()).then(|| limit_text(trimmed))
}

fn limit_text(text: &str) -> String {
	if text.chars().count() > ACP_TEXT_LIMIT {
		let mut limited: String = text.chars().take(ACP_TEXT_LIMIT - 1).collect();
		limited.push('…');
		limited
	} else {
		text.to_string()
	}
}

fn todo_entries(details: &Value) -> Option<Vec<Value>> {
	let phases = details.get("phases")?.as_array()?;
	let mut entries = Vec::new();
	for phase in phases {
		let Some(tasks) = phase.get("tasks").and_then(Value::as_array) else {
			continue;
		};
		for task in tasks {
			let Some(content) = task
				.get("content")
				.and_then(Value::as_str)
				.filter(|c| !c.is_empty())
			else {
				continue;
			};
			let status = match task.get("status").and_then(Value::as_str) {
				Some("in_progress") => "in_progress",
				Some("completed" | "abandoned") => "completed",
				_ => "pending",
			};
			entries.push(json!({ "content": content, "priority": "medium", "status": status }));
		}
	}
	Some(entries)
}

fn replay_message(message: &Value, cwd: &Path, next_id: &AtomicU64) -> Vec<Value> {
	let role = message.get("role").and_then(Value::as_str).unwrap_or("");
	let message_id = format!("msg-{}", next_id.fetch_add(1, Ordering::Relaxed));
	match role {
		"assistant" => replay_assistant(message, cwd, &message_id),
		"toolResult" => replay_tool_result(message),
		"user" | "developer" | "custom" | "hookMessage" | "bashExecution" | "pythonExecution"
		| "compactionSummary" => replay_user_content(message, &message_id),
		_ => Vec::new(),
	}
}

fn replay_assistant(message: &Value, cwd: &Path, message_id: &str) -> Vec<Value> {
	let mut updates = Vec::new();
	if let Some(text) = message
		.get("content")
		.and_then(Value::as_str)
		.filter(|text| !text.is_empty())
	{
		updates.push(json!({
			"sessionUpdate": "agent_message_chunk",
			"content": { "type": "text", "text": limit_text(text) },
			"messageId": message_id,
		}));
	}
	let blocks: Vec<&Value> = match message.get("content") {
		Some(Value::Array(blocks)) => blocks.iter().collect(),
		_ => message
			.get("parts")
			.and_then(Value::as_array)
			.map(|parts| parts.iter().collect())
			.unwrap_or_default(),
	};
	for block in blocks {
		match block.get("type").and_then(Value::as_str) {
			Some("text") => {
				if let Some(text) = block
					.get("text")
					.and_then(Value::as_str)
					.filter(|text| !text.is_empty())
				{
					updates.push(json!({
						"sessionUpdate": "agent_message_chunk",
						"content": { "type": "text", "text": limit_text(text) },
						"messageId": message_id,
					}));
				}
			},
			Some("image") => {
				if let (Some(data), Some(mime)) = (
					block.get("data").and_then(Value::as_str),
					block.get("mimeType").and_then(Value::as_str),
				) {
					updates.push(json!({
						"sessionUpdate": "agent_message_chunk",
						"content": { "type": "image", "data": data, "mimeType": mime },
						"messageId": message_id,
					}));
				}
			},
			Some("thinking") => {
				if let Some(text) = block
					.get("thinking")
					.and_then(Value::as_str)
					.filter(|text| !text.trim().is_empty())
				{
					updates.push(json!({
						"sessionUpdate": "agent_thought_chunk",
						"content": { "type": "text", "text": limit_text(text.trim()) },
						"messageId": message_id,
					}));
				}
			},
			Some("toolCall" | "tool_use") => {
				let Some(id) = block.get("id").and_then(Value::as_str) else {
					continue;
				};
				let Some(name) = block.get("name").and_then(Value::as_str) else {
					continue;
				};
				let args = block
					.get("arguments")
					.or_else(|| block.get("input"))
					.cloned()
					.map(|value| match value {
						Value::String(raw) => {
							serde_json::from_str::<Value>(&raw).unwrap_or(Value::String(raw))
						},
						value => value,
					})
					.unwrap_or_else(|| json!({}));
				updates.push(tool_call_update(id, name, &args, cwd));
			},
			_ => {},
		}
	}
	// pi-agent `Message` mirror: tool calls live in a sibling field.
	for call in message
		.get("tool_calls")
		.and_then(Value::as_array)
		.into_iter()
		.flatten()
	{
		let Some(id) = call.get("id").and_then(Value::as_str) else {
			continue;
		};
		let Some(name) = call.get("name").and_then(Value::as_str) else {
			continue;
		};
		let args = call.get("arguments").cloned().unwrap_or_else(|| json!({}));
		updates.push(tool_call_update(id, name, &args, cwd));
	}
	if updates.is_empty() {
		if let Some(error) = message
			.get("errorMessage")
			.and_then(Value::as_str)
			.filter(|text| !text.is_empty())
		{
			updates.push(json!({
				"sessionUpdate": "agent_message_chunk",
				"content": { "type": "text", "text": limit_text(error) },
				"messageId": message_id,
			}));
		}
	}
	updates
}

fn replay_tool_result(message: &Value) -> Vec<Value> {
	let id = message
		.get("toolCallId")
		.or_else(|| message.get("tool_call_id"))
		.and_then(Value::as_str)
		.unwrap_or("tool");
	let result = message.get("content").cloned().unwrap_or(Value::Null);
	let failed = message
		.get("isError")
		.or_else(|| message.get("is_error"))
		.and_then(Value::as_bool)
		.unwrap_or(false);
	let mut content = Vec::new();
	if let Some(text) = readable_text(&result) {
		content.push(json!({ "type": "content", "content": { "type": "text", "text": text } }));
	}
	let mut update = json!({
		"sessionUpdate": "tool_call_update",
		"toolCallId": id,
		"status": if failed { "failed" } else { "completed" },
		"rawOutput": result,
	});
	if !content.is_empty() {
		update["content"] = Value::Array(content);
	}
	vec![update]
}

fn replay_user_content(message: &Value, message_id: &str) -> Vec<Value> {
	let mut blocks = Vec::new();
	match message.get("content") {
		Some(Value::String(text)) if !text.is_empty() => {
			blocks.push(json!({ "type": "text", "text": limit_text(text) }));
		},
		Some(Value::Array(items)) => {
			for item in items {
				match item.get("type").and_then(Value::as_str) {
					Some("text") => {
						if let Some(text) = item
							.get("text")
							.and_then(Value::as_str)
							.filter(|text| !text.is_empty())
						{
							blocks.push(json!({ "type": "text", "text": limit_text(text) }));
						}
					},
					Some("image") => {
						if let (Some(data), Some(mime)) = (
							item.get("data").and_then(Value::as_str),
							item.get("mimeType").and_then(Value::as_str),
						) {
							blocks.push(json!({ "type": "image", "data": data, "mimeType": mime }));
						}
					},
					_ => {},
				}
			}
		},
		_ => {},
	}
	blocks
		.into_iter()
		.map(|block| {
			json!({
				"sessionUpdate": "user_message_chunk",
				"content": block,
				"messageId": message_id,
			})
		})
		.collect()
}

// --------------------------------------------------------------- manager --

/// Session map for one ACP connection.
#[derive(Default)]
pub struct SessionManager {
	sessions: Mutex<BTreeMap<String, Arc<AcpSession>>>,
}

impl SessionManager {
	#[must_use]
	pub fn new() -> Self {
		Self::default()
	}

	pub fn insert(self: &Arc<Self>, session: Arc<AcpSession>) {
		let id = session.id().to_string();
		let manager = Arc::downgrade(self);
		session.set_close_hook(Arc::new(move |_reason| {
			if let Some(manager) = manager.upgrade() {
				manager.remove(&id);
			}
		}));
		self.lock().insert(session.id().to_string(), session);
	}

	#[must_use]
	pub fn get(&self, id: &str) -> Option<Arc<AcpSession>> {
		self.lock().get(id).cloned()
	}

	pub fn remove(&self, id: &str) -> Option<Arc<AcpSession>> {
		self.lock().remove(id)
	}

	#[must_use]
	pub fn first(&self) -> Option<Arc<AcpSession>> {
		self.lock().values().next().cloned()
	}

	/// Signal every live session; used by transport close.
	pub fn shutdown_all(&self, reason: &str) {
		let sessions: Vec<Arc<AcpSession>> = self.lock().values().cloned().collect();
		for session in sessions {
			session.shutdown(reason);
		}
	}

	/// Count of managed sessions (usage extension picks the first record).
	#[must_use]
	pub fn len(&self) -> usize {
		self.lock().len()
	}

	fn lock(&self) -> MutexGuard<'_, BTreeMap<String, Arc<AcpSession>>> {
		self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
	}
}

/// `session/list` payload.
pub fn list_payload(
	manager: &SessionManager,
	cwd: Option<&Path>,
	cursor: Option<&str>,
	root: &Path,
) -> Result<Value, RequestError> {
	// Loaded sessions are already on disk; the scan below is the source of
	// truth, matching TS `#listStoredSessions`.
	let sessions = session_list::list_sessions(cwd, Some(root));
	let (paged, next) = session_list::page(&sessions, cursor)
		.map_err(|message| RequestError::new(INTERNAL_ERROR, message))?;
	let _ = manager;
	let mut payload = json!({
		"sessions": paged.iter().map(session_list::session_info).collect::<Vec<_>>(),
	});
	if let Some(next) = next {
		payload["nextCursor"] = json!(next);
	}
	Ok(payload)
}

/// `_omp/usage`: no provider-quota backend exists, so no report is invented.
#[must_use]
pub fn usage_payload() -> Value {
	json!({ "reports": [] })
}

/// `_omp/extensions` boundary payload.
#[must_use]
pub fn extensions_payload() -> Value {
	capabilities::extensions_payload()
}

/// Resolve a requested workspace path. ACP cwd must be absolute.
pub fn absolute_cwd(raw: &str) -> Result<PathBuf, RequestError> {
	let path = PathBuf::from(raw);
	if !path.is_absolute() {
		return Err(
			RequestError::invalid_params().with_detail(format!("ACP cwd must be absolute: {raw}")),
		);
	}
	Ok(path)
}
