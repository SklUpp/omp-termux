//! `pi-agent` backed runner: OpenAI-compatible provider plus builtin tools.
//!
//! The CLI resolves data (config, catalog, prompts, skills) into
//! [`RunRequest`]; this module maps it onto `pi-agent`'s `AgentConfig` and
//! converts the loop's turn results back into the CLI [`Event`] stream.

use std::{
	collections::{HashMap, HashSet},
	path::{Path, PathBuf},
	sync::{
		Arc, Mutex, Weak,
		atomic::{AtomicBool, AtomicU64, Ordering},
	},
};

use async_trait::async_trait;
use pi_agent::{
	AgentConfig, AgentError, AgentEvent, AgentLoop, AgentState, AskTool, AstGrepTool, BashTool,
	CheckpointTool, DelegationLimits, RewindTool,
	ChildToolSettings, EditTool, GlobTool, GrepTool, ModelProvider,
	OpenAiCompatibleProvider, PendingStore, ReadTool,
	SteeringEvent, Supervisor, TodoTool, Tool, ToolApprover, ToolRegistry, WriteTool,
	dap::{DapSessionManager, DapTool},
	process_broker::BrokerProcessBackend,
	tools::process::{ProcessBackend, ProcessManager},
	lsp::{LspSession, LspTool},
	mcp::{
		MCPContent, MCPServerConfig, MCPStdioServerConfig, McpConnection, McpError,
		McpToolOriginSource, connect_to_server, create_mcp_tool_name, get_mcp_tool_origin_key,
		mcp_tool_match_score, sanitize_mcp_tool_name_part,
	},
	tools::hashline::{NativeSnapshotSink, SnapshotStore, edit::EditToolOptions},
	tools::{
		edit_variants::{
			EditMode, ResolvedSessionTools, SessionDisplayResolver, default_session_tools,
			normalize_edit_mode, resolve_edit_mode,
		},
		read::FileDisplayInputs,
		registry::excluded_tool_reason,
		task_isolation::TaskIsolationSettings,
	},
	tools::task_catalog::{
		AgentCatalog, ChildModel, ChildModelFactory, EFFORT_SELECTORS, parse_effort_prefix,
		split_effort_selector,
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
/// The registry lists every selected tool; approval is not a visibility filter.
/// `ToolApprover` gates each call in the scheduler immediately before
/// execution, so denied tools still reach the model and report explicit denial.
///
/// `lsp` and `debug` are mixed-tier: approval classifies their per-action
/// readonly sets, so both are listed as not read-only here. Their availability
/// still follows `lsp.enabled`/`debug.enabled` (see [`allowed_tools`]).
pub const BUILTIN_TOOLS: [(&str, bool); 13] = [
	("read", true),
	("glob", true),
	("grep", true),
	("ast_grep", true),
	("ast_edit", false),
	("write", false),
	("edit", false),
	("bash", false),
	("eval", false),
	("todo", false),
	("ask", true),
	("lsp", false),
	("debug", false),
];

/// Delegation tools assembled around a [`Supervisor`] rather than built from
/// static registry entries. They are offered by default and can be selected or
/// excluded with `--tools`/`config.tools`.
pub const DELEGATION_TOOLS: [&str; 2] = ["task", "hub"];

/// Synchronous `AgentRunner` that drives `pi-agent` on its own tokio runtime.
pub struct PiAgentRunner {
	runtime:    tokio::runtime::Runtime,
	dispatcher: Arc<SignalDispatcher>,
	/// In-memory supervisor reused across turns of this process. Child records
	/// survive REPL turns so `hub jobs` can inspect earlier delegations; they
	/// are never written to the session log.
	supervisor: Mutex<Option<Arc<Supervisor>>>,
	/// Root `read`/`edit` provenance shared by every turn this runner serves.
	session:    Mutex<Option<Arc<RunnerSession>>>,
	/// One broker-backed process supervisor per project scope, reused across
	/// turns. A scope that cannot reach its broker falls back to the
	/// supervisor's local manager (no entry cached).
	processes:  Mutex<HashMap<PathBuf, Arc<dyn ProcessBackend>>>,
	/// Per-runner nonce folded into the local fallback log root, so two CLIs on
	/// the same project never prune each other's logs when one manager drops.
	process_scope_ns: String,
	/// Exit teardown ownership: one caller runs it, the rest report its real
	/// completion instead of returning on empty slots.
	shutdown_latch: Arc<ShutdownLatch>,
}

/// One snapshot store plus one `EditTool` per runner: tags, seen-line ranges,
/// named `CUT` registers, and the no-op guard must survive REPL turns, while
/// child agents still get fresh instances through `Supervisor::child_registry`.
/// `pending` sits beside them for the same reason: a staged preview registered
/// in one prompt must stay resolvable in the next, even though the tool
/// registry is rebuilt for every prompt.
struct RunnerSession {
	snapshots: Arc<SnapshotStore>,
	edit:      Arc<EditTool>,
	pending:   Arc<PendingStore>,
	/// Session-owned soft requirement policy: a staged preview must keep
	/// reminding and forcing across prompt turns whose registry is rebuilt.
	policy:    Arc<dyn pi_agent::state::ToolPolicy>,
	/// Process-stable eval identity for in-memory sessions; replaced by the
	/// persisted session id when the host has one.
	eval_session_id: Mutex<String>,
	session_file:    Mutex<Option<PathBuf>>,
	/// Started from the runner's Tokio runtime on the first eval-enabled turn.
	/// Settings changes restart the bridge and dispose retained kernels.
	eval:            Mutex<Option<(pi_agent::eval::EvalSettings, Arc<pi_agent::eval::EvalRuntime>)>>,
	/// One debug session tree for every root turn: the registry (and its
	/// `debug` tool) is rebuilt per prompt, but launched debuggees must survive
	/// until `terminate` or runner teardown. Children get their own manager.
	dap:             Arc<DapSessionManager>,
	/// LSP ownership scope shared by every root turn. Process-global clients are
	/// claimed here so teardown closes only sessions nothing else uses.
	lsp:             Mutex<LspSlot>,
	/// `tools.maxTimeout` handed to the root `debug` tool each turn.
	debug_timeout:   Mutex<Option<u64>>,
	/// Session internal URL router for this run: the identical handle is handed
	/// to the supervisor, so root and child `read`/`write` see one scheme set.
	internal_urls:   Mutex<Option<Arc<pi_agent::tools::internal_urls::InternalUrlRouter>>>,
	/// Immutable URL scope (session file + `local://` root) for this run.
	url_scope:       Mutex<Arc<pi_agent::tools::internal_urls::UrlScope>>,
}

/// Shared LSP ownership scope plus the transport preference it was built with.
struct LspSlot {
	shared:  bool,
	session: Arc<LspSession>,
}

impl LspSlot {
	fn new(shared: bool) -> Self {
		Self { shared, session: LspSession::new(shared) }
	}
}

impl RunnerSession {
	fn new(options: EditToolOptions) -> Self {
		let snapshots = Arc::new(SnapshotStore::new());
		let edit = Arc::new(EditTool::with_options(snapshots.clone(), options));
		let pending = Arc::new(PendingStore::new());
		let policy = Arc::new(pi_agent::state::StagedPreviewPolicy::new(Arc::clone(&pending)));
		Self {
			snapshots,
			edit,
			pending,
			policy,
			eval_session_id: Mutex::new(crate::output::session_id()),
			session_file: Mutex::new(None),
			eval: Mutex::new(None),
			dap: Arc::new(DapSessionManager::new()),
			lsp: Mutex::new(LspSlot::new(true)),
			debug_timeout: Mutex::new(None),
			internal_urls: Mutex::new(None),
			url_scope: Mutex::new(Arc::new(pi_agent::tools::internal_urls::UrlScope::default())),
		}
	}

	/// Root DAP session tree shared by every turn of this runner.
	fn dap_manager(&self) -> Arc<DapSessionManager> {
		Arc::clone(&self.dap)
	}

	/// Root LSP ownership scope shared by every turn of this runner.
	fn lsp_session(&self) -> Arc<LspSession> {
		Arc::clone(&self.lsp.lock().unwrap_or_else(|poison| poison.into_inner()).session)
	}

	/// Follow `lsp.shared`, replacing (and releasing) the previous scope when it
	/// changes so clients created under the old transport are not left open.
	fn set_lsp_shared(&self, shared: bool, runtime: &tokio::runtime::Runtime) {
		let previous = {
			let mut slot = self.lsp.lock().unwrap_or_else(|poison| poison.into_inner());
			if slot.shared == shared {
				return;
			}
			std::mem::replace(&mut *slot, LspSlot::new(shared))
		};
		// Close the superseded scope off the caller's thread; release is
		// ownership-scoped, so clients another session still uses survive.
		let session = previous.session;
		runtime.spawn(async move {
			session.release().await;
		});
	}

	fn set_debug_timeout(&self, seconds: Option<u64>) {
		*self.debug_timeout.lock().unwrap_or_else(|poison| poison.into_inner()) =
			seconds.filter(|seconds| *seconds > 0);
	}

	fn debug_timeout(&self) -> Option<u64> {
		*self.debug_timeout.lock().unwrap_or_else(|poison| poison.into_inner())
	}

	fn set_session_identity(&self, id: Option<&str>, file: Option<&Path>) {
		if let Some(id) = id.filter(|id| !id.is_empty()) {
			*self.eval_session_id.lock().unwrap_or_else(|poison| poison.into_inner()) =
				id.to_string();
		}
		*self.session_file.lock().unwrap_or_else(|poison| poison.into_inner()) =
			file.map(Path::to_path_buf);
	}

	/// Install this run's URL router; `None` leaves every `scheme://` source
	/// unavailable to the root `read`/`write` tools.
	fn set_internal_urls(
		&self,
		router: Option<Arc<pi_agent::tools::internal_urls::InternalUrlRouter>>,
	) {
		*self
			.internal_urls
			.lock()
			.unwrap_or_else(|poison| poison.into_inner()) = router;
	}

	/// Install this run's immutable URL scope; without one the empty scope
	/// applies (no session file, no `local://` root).
	fn set_url_scope(&self, scope: Option<Arc<pi_agent::tools::internal_urls::UrlScope>>) {
		*self
			.url_scope
			.lock()
			.unwrap_or_else(|poison| poison.into_inner()) =
			scope.unwrap_or_else(|| Arc::new(pi_agent::tools::internal_urls::UrlScope::default()));
	}

	fn internal_urls(&self) -> Option<Arc<pi_agent::tools::internal_urls::InternalUrlRouter>> {
		self
			.internal_urls
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
			.clone()
	}

	fn url_scope(&self) -> Arc<pi_agent::tools::internal_urls::UrlScope> {
		Arc::clone(
			&self
				.url_scope
				.lock()
				.unwrap_or_else(|poison| poison.into_inner()),
		)
	}

	fn eval_session_id(&self) -> String {
		self
			.eval_session_id
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
			.clone()
	}

	fn eval_runtime(
		&self,
		runtime: &tokio::runtime::Runtime,
		settings: pi_agent::eval::EvalSettings,
	) -> crate::Result<Arc<pi_agent::eval::EvalRuntime>> {
		let mut slot = self.eval.lock().unwrap_or_else(|poison| poison.into_inner());
		if let Some((current, eval)) = slot.as_ref()
			&& current == &settings
		{
			return Ok(Arc::clone(eval));
		}
		if let Some((_, previous)) = slot.take() {
			runtime.block_on(previous.shutdown());
		}
		let started = runtime
			.block_on(pi_agent::eval::EvalRuntime::start(settings.clone()))
			.map_err(|error| Error::new(format!("eval bridge start failed: {error}")))?;
		let eval = Arc::new(started);
		*slot = Some((settings, Arc::clone(&eval)));
		Ok(eval)
	}

	fn take_eval(&self) -> Option<Arc<pi_agent::eval::EvalRuntime>> {
		self
			.eval
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
			.take()
			.map(|(_, eval)| eval)
	}
}

/// Exact upstream `parseFlag` truthy set (`packages/utils/src/env.ts`).
const ENV_FLAG_TRUTHY: [&str; 9] = ["1", "Y", "y", "TRUE", "true", "YES", "yes", "ON", "on"];

fn parse_env_flag(name: &str) -> bool {
	std::env::var(name).is_ok_and(|value| ENV_FLAG_TRUTHY.contains(&value.as_str()))
}

/// CLI settings + environment as the injected `edit.mode`/display resolver.
///
/// `PI_EDIT_VARIANT` pins the `edit` wire contract (invalid values are a hard
/// error, `auto`/empty means dynamic), while display resolution follows
/// upstream precedence: model variant → env → `edit.mode` → hashline/sloppy.
struct ConfigDisplayResolver {
	config:       crate::config::Config,
	env_variant:  Option<EditMode>,
	strict:       bool,
}

impl ConfigDisplayResolver {
	fn from_config(config: &crate::config::Config) -> Result<Self> {
		let env_variant = match std::env::var("PI_EDIT_VARIANT")
			.ok()
			.filter(|raw| !raw.is_empty() && raw != "auto")
		{
			Some(raw) => Some(normalize_edit_mode(&raw).ok_or_else(|| {
				Error::new(format!("Invalid PI_EDIT_VARIANT: {raw}"))
			})?),
			None => None,
		};
		Ok(Self {
			config: config.clone(),
			env_variant,
			strict: parse_env_flag("PI_STRICT_EDIT_MODE"),
		})
	}
}

impl SessionDisplayResolver for ConfigDisplayResolver {
	fn resolve(&self, active_model: Option<&str>, has_edit_tool: bool) -> ResolvedSessionTools {
		let display_mode = resolve_edit_mode(
			&self.config,
			active_model,
			self.env_variant.map(EditMode::as_str),
			self.strict,
		);
		ResolvedSessionTools {
			display: FileDisplayInputs::new(
				display_mode,
				has_edit_tool,
				self.config.read_line_numbers(),
			),
			edit_tool: self.env_variant.unwrap_or(display_mode),
		}
	}
}

impl PiAgentRunner {
	pub fn new() -> Result<Self> {
		let runtime = tokio::runtime::Builder::new_multi_thread()
			.enable_all()
			.build()
			.map_err(|err| Error::with_source("cannot start tokio runtime for pi-agent", err))?;
		let dispatcher = Arc::new(SignalDispatcher::start(&runtime));
		Ok(Self {
			runtime,
			dispatcher,
			supervisor: Mutex::new(None),
			session: Mutex::new(None),
			processes: Mutex::new(HashMap::new()),
			process_scope_ns: crate::output::session_id(),
			shutdown_latch: Arc::new(ShutdownLatch::new()),
		})
	}

	/// Inject a steering message into the live turn, preserving its exact parts
	/// (text and images). `false` when no turn is running or the loop closed its
	/// steering channel — the caller reports that, never claims success.
	pub fn steer(&self, parts: Vec<pi_agent::ContentPart>) -> bool {
		self.dispatcher.inject(parts)
	}

	/// Process supervisor for exactly `project`, connected on first use and
	/// reused by later turns of this runner. Always returns a scope-bound
	/// backend: when the broker is unreachable, an in-process manager bound to
	/// that project's log root is used, so a turn never inherits another
	/// project's backend.
	fn process_backend(&self, project: &Path) -> Arc<dyn ProcessBackend> {
		let key = project.to_path_buf();
		if let Ok(cache) = self.processes.lock()
			&& let Some(backend) = cache.get(&key)
		{
			return Arc::clone(backend);
		}
		// Construction must not touch the broker: a turn that never starts a
		// process (most agent/read turns) must not spend the turn budget on a
		// worker handshake. The first process op connects, then caches.
		let backend: Arc<dyn ProcessBackend> = Arc::new(LazyProcessBackend::new(
			project.to_path_buf(),
			process_scope_root(project, &self.process_scope_ns),
		));
		if let Ok(mut cache) = self.processes.lock() {
			cache.insert(key, Arc::clone(&backend));
		}
		backend
	}


	/// Reuse this runner's root store/`EditTool`, refreshing only the knobs.
	fn session_tools(&self, options: EditToolOptions, mode: EditMode) -> Arc<RunnerSession> {
		let mut slot = self.session.lock().unwrap_or_else(|poison| poison.into_inner());
		let session = Arc::clone(slot.get_or_insert_with(|| Arc::new(RunnerSession::new(options))));
		session.edit.set_options(options);
		// Cross-turn registers/guard survive; only the wire contract follows the
		// active model/env, as upstream's lazy `get mode()`.
		session.edit.set_mode(mode);
		session
	}
}

impl PiAgentRunner {
	/// Release everything this runner keeps alive across turns: the in-memory
	/// supervisor, eval kernels and loopback bridge, the LSP ownership scope and
	/// the root debug tree.
	///
	/// Idempotent and coalesced: the first caller owns teardown, later callers
	/// wait for its real completion (`ShutdownLatch`) instead of returning on
	/// already-empty slots while adapters are still draining. Call it only once
	/// the last turn has returned — it cancels live child tasks and detaches
	/// adapters.
	///
	/// Never blocks a Tokio thread: teardown needs that same runtime to drive
	/// the LSP/DAP futures, so a worker waiting here would deadlock a
	/// single-worker runtime. Off a Tokio thread this returns only after every
	/// owned adapter is closed; on one, teardown completes detached and the
	/// caller returns — the idle-Ctrl-C path therefore tears down and exits
	/// from a plain thread.
	///
	/// Per-operation bounds are unchanged (LSP `shutdown`/`exit` per client, the
	/// DAP terminate deadline, the kernel grace in `EvalRuntime::shutdown`).
	/// There is deliberately no outer cap: cancelling `LspSession::release`
	/// once it had drained its keys would leak every client still queued.
	///
	/// The host needs this because an installed process-wide `AgentRunner`
	/// keeps the runner alive past `main`, so `Drop` alone never fires on exit.
	pub fn shutdown(&self) {
		if !self.shutdown_latch.begin() {
			// Another caller owns teardown. Off a runtime thread, report only
			// once it really finished; on one, waiting would stall the workers
			// it depends on.
			if tokio::runtime::Handle::try_current().is_err() {
				self.shutdown_latch.wait();
			}
			return;
		}
		// Background jobs hold `Arc<Supervisor>`, so the supervisor's own Drop
		// cannot fire while they run. Shut it down here, before the `runtime`
		// field is dropped, so queued reservations are cancelled and live child
		// tasks get their cancellation token plus abort handle. The slot lock is
		// released before any callback runs.
		if let Some(supervisor) = self.take_supervisor() {
			supervisor.shutdown();
		}
		// Kernels, the loopback bridge, LSP connections and the root debug tree
		// live as long as the runner session. Shut them down explicitly while the
		// Tokio runtime is still alive: LSP release is ownership-scoped, so a
		// server another session still uses stays up, and DAP terminate sends
		// `disconnect` before the adapter is killed.
		let Some(session) = self.take_session() else {
			self.shutdown_latch.finish();
			return;
		};
		// Teardown always runs on a plain thread: `Runtime::block_on` panics in
		// a runtime context, and `LatchFinish` closes the latch even if an
		// adapter panics, so a waiting caller sees completion either way.
		let handle = self.runtime.handle().clone();
		let latch = Arc::clone(&self.shutdown_latch);
		let teardown = std::thread::spawn(move || {
			let _finish = LatchFinish(latch);
			handle.block_on(shutdown_session(session));
		});
		if tokio::runtime::Handle::try_current().is_err() {
			let _ = teardown.join();
		}
	}

	fn take_supervisor(&self) -> Option<Arc<Supervisor>> {
		self
			.supervisor
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
			.take()
	}

	fn take_session(&self) -> Option<Arc<RunnerSession>> {
		self
			.session
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
			.take()
	}
}

/// Deadline handed to the DAP terminate at exit: `terminate` disconnects and
/// kills each adapter before it, so the debug tree never outlives the process.
const DAP_SHUTDOWN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

/// Await-needing half of [`PiAgentRunner::shutdown`]: eval kernel/bridge
/// disposal, LSP release (`shutdown` + `exit`, else kill) and the bounded DAP
/// terminate that disconnects the root tree before killing adapters.
///
/// The three run concurrently and each carries its own bound, so a slow LSP
/// client can never starve the DAP disconnect (or kernels) of teardown, and no
/// outer cap can cancel `LspSession::release` midway — it marks itself released
/// and drains its keys before the first await, so a cancelled release leaks
/// every client it had not reached yet.
async fn shutdown_session(session: Arc<RunnerSession>) {
	let eval = session.take_eval();
	let lsp = session.lsp_session();
	let dap = session.dap_manager();
	let dispose_eval = async move {
		if let Some(eval) = eval {
			eval.shutdown().await;
		}
	};
	let release_lsp = async move {
		lsp.release().await;
	};
	let terminate_dap = async move {
		let deadline = std::time::Instant::now() + DAP_SHUTDOWN_DEADLINE;
		let cancel = pi_agent::CancellationToken::new();
		let _ = dap.terminate(&cancel, deadline).await;
	};
	tokio::join!(dispose_eval, release_lsp, terminate_dap);
}

/// One-shot gate over the runner's exit teardown: `begin` elects the single
/// caller that runs it, `wait` reports its real completion to the others, so a
/// concurrent `shutdown` cannot believe the teardown is done just because the
/// slots it looked at were already empty.
///
/// Never waited on from a Tokio thread: those workers drive the futures
/// teardown awaits.
struct ShutdownLatch {
	state:    Mutex<ShutdownState>,
	finished: std::sync::Condvar,
}

struct ShutdownState {
	started: bool,
	done:    bool,
}

impl ShutdownLatch {
	fn new() -> Self {
		Self {
			state:    Mutex::new(ShutdownState { started: false, done: false }),
			finished: std::sync::Condvar::new(),
		}
	}

	/// `true` for the one caller that must run teardown.
	fn begin(&self) -> bool {
		let mut state = self.state.lock().unwrap_or_else(|poison| poison.into_inner());
		if state.started {
			return false;
		}
		state.started = true;
		true
	}

	fn finish(&self) {
		let mut state = self.state.lock().unwrap_or_else(|poison| poison.into_inner());
		state.done = true;
		self.finished.notify_all();
	}

	/// Block until the elected caller finished. Off a Tokio thread only.
	fn wait(&self) {
		let mut state = self.state.lock().unwrap_or_else(|poison| poison.into_inner());
		while !state.done {
			state = self.finished.wait(state).unwrap_or_else(|poison| poison.into_inner());
		}
	}
}

/// Closes the latch on every exit path, including a panicking adapter, so a
/// waiting caller sees completion instead of hanging on a dead owner.
struct LatchFinish(Arc<ShutdownLatch>);

impl Drop for LatchFinish {
	fn drop(&mut self) {
		self.0.finish();
	}
}

impl Drop for PiAgentRunner {
	fn drop(&mut self) {
		self.shutdown();
	}
}

/// Runs [`PiAgentRunner::shutdown`] when it drops.
///
/// A host that installs the runner as the process-wide `AgentRunner` keeps an
/// `Arc` alive in a static, so the runner's own `Drop` never runs at exit. One
/// guard next to the runner's creation closes kernels, LSP clients and debug
/// adapters on every return path, without late-created help/verb paths paying
/// for a runner they never use.
pub struct RunnerShutdownGuard(Arc<PiAgentRunner>);

impl RunnerShutdownGuard {
	#[must_use]
	pub fn new(runner: Arc<PiAgentRunner>) -> Self {
		// The idle Ctrl-C path (`process::exit`) must run the same teardown, so
		// the dispatcher keeps a `Weak` back to this runner.
		runner.dispatcher.set_owner(&runner);
		Self(runner)
	}
}

impl Drop for RunnerShutdownGuard {
	fn drop(&mut self) {
		self.0.shutdown();
	}
}

/// Every tool name the runner can mount itself, derived from the real
/// constructors/constants so a rename cannot drift. Used to reject host tool
/// definitions that would take a builtin, ask/checkpoint/rewind, or a
/// bridge-owned (`eval`/`task`/`hub`) name. MCP names are per-turn (they exist
/// only after discovery), so the mount-time conflict check covers those.
#[must_use]
pub fn reserved_tool_names() -> Vec<String> {
	let mut names: Vec<String> = pi_agent::tools::builtin_tools()
		.iter()
		.map(|tool| tool.name().to_string())
		.collect();
	names.push(Tool::name(&AskTool::new()).to_string());
	names.push(pi_agent::tools::checkpoint::CHECKPOINT_TOOL_NAME.to_string());
	names.push(pi_agent::tools::checkpoint::REWIND_TOOL_NAME.to_string());
	names.extend(crate::eval_host::OWNED_TOOL_NAMES.iter().map(|name| (*name).to_string()));
	names
}

/// An `Arc<PiAgentRunner>` is itself a runner, so a caller that also needs the
/// concrete type (live `steer`, `RpcLaunch`) can register one shared instance
/// instead of building a second runtime.
impl AgentRunner for Arc<PiAgentRunner> {
	fn run(&self, request: &RunRequest) -> Result<EventStream> {
		<PiAgentRunner as AgentRunner>::run(self, request)
	}
}

struct SignalDispatcher {
	active_turn: Arc<std::sync::Mutex<Option<tokio::sync::mpsc::Sender<SteeringEvent>>>>,
	/// The dialog's own steering channel. Registered only by a real turn (never
	/// by the MCP-discovery phase), so `steer` cannot report success for a
	/// message the discovery watcher would drop.
	steer_turn:  Arc<std::sync::Mutex<Option<tokio::sync::mpsc::Sender<SteeringEvent>>>>,
	/// Runner the idle Ctrl-C path must tear down before `exit(130)`; a `Weak`
	/// so the installed static `AgentRunner` stays the only owner.
	owner:       Arc<Mutex<Weak<PiAgentRunner>>>,
	_handle:     tokio::task::JoinHandle<()>,
}

struct TurnSignalRegistration {
	active_turn: Arc<std::sync::Mutex<Option<tokio::sync::mpsc::Sender<SteeringEvent>>>>,
	/// The channel this registration installed; a later registration for another
	/// turn must not be wiped when this one drops.
	own:         tokio::sync::mpsc::Sender<SteeringEvent>,
	/// Present only for the dialog registration; cleared on drop.
	steer_turn:  Option<Arc<std::sync::Mutex<Option<tokio::sync::mpsc::Sender<SteeringEvent>>>>>,
}

impl Drop for TurnSignalRegistration {
	fn drop(&mut self) {
		clear_if_owning(&self.active_turn, &self.own);
		if let Some(steer_turn) = &self.steer_turn {
			clear_if_owning(steer_turn, &self.own);
		}
	}
}

/// Clear a slot only when it still holds `own`. RPC can start a second turn
/// while the first is finishing, so an unconditional clear would remove the
/// live turn's interrupt and steering channels (and make Ctrl-C fall through to
/// `exit(130)` mid-turn).
fn clear_if_owning(
	slot: &Arc<std::sync::Mutex<Option<tokio::sync::mpsc::Sender<SteeringEvent>>>>,
	own: &tokio::sync::mpsc::Sender<SteeringEvent>,
) {
	if let Ok(mut guard) = slot.lock()
		&& guard.as_ref().is_some_and(|current| current.same_channel(own))
	{
		*guard = None;
	}
}

impl SignalDispatcher {
	fn start(runtime: &tokio::runtime::Runtime) -> Self {
		let active_turn: Arc<std::sync::Mutex<Option<tokio::sync::mpsc::Sender<SteeringEvent>>>> =
			Arc::new(std::sync::Mutex::new(None));
		let active_turn_clone = Arc::clone(&active_turn);
		let owner: Arc<Mutex<Weak<PiAgentRunner>>> = Arc::new(Mutex::new(Weak::new()));
		let owner_clone = Arc::clone(&owner);
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
					// No turn owns the interrupt: close kernels, LSP clients and
					// debug adapters before exiting instead of orphaning them.
					// The `Weak` is upgraded without holding its lock across the
					// teardown.
					let runner = owner_clone
						.lock()
						.unwrap_or_else(|poison| poison.into_inner())
						.upgrade();
					// Teardown is driven by this runtime's own workers, so it must
					// not run here: a plain thread performs it (blocking until every
					// owned adapter is closed) and exits 130 afterwards, while this
					// task ends and frees the worker it occupied.
					std::thread::spawn(move || {
						if let Some(runner) = runner {
							runner.shutdown();
						}
						std::process::exit(130);
					});
					break;
				}
			}
		});
		Self {
			active_turn,
			steer_turn: Arc::new(std::sync::Mutex::new(None)),
			owner,
			_handle,
		}
	}

	/// Record the runner the idle Ctrl-C path tears down.
	fn set_owner(&self, runner: &Arc<PiAgentRunner>) {
		*self.owner.lock().unwrap_or_else(|poison| poison.into_inner()) = Arc::downgrade(runner);
	}

	/// Register the interrupt destination for a phase that cannot be steered
	/// (MCP discovery): Ctrl-C still cancels, injected messages are refused.
	fn register(&self, tx: tokio::sync::mpsc::Sender<SteeringEvent>) -> TurnSignalRegistration {
		if let Ok(mut guard) = self.active_turn.lock() {
			*guard = Some(tx.clone());
		}
		TurnSignalRegistration {
			active_turn: Arc::clone(&self.active_turn),
			own: tx,
			steer_turn: None,
		}
	}

	/// Register the running dialog: Ctrl-C interrupts it and `steer` may inject
	/// into it until the registration drops.
	fn register_dialog(
		&self,
		tx: tokio::sync::mpsc::Sender<SteeringEvent>,
	) -> TurnSignalRegistration {
		if let Ok(mut guard) = self.active_turn.lock() {
			*guard = Some(tx.clone());
		}
		if let Ok(mut guard) = self.steer_turn.lock() {
			*guard = Some(tx.clone());
		}
		TurnSignalRegistration {
			active_turn: Arc::clone(&self.active_turn),
			own: tx,
			steer_turn: Some(Arc::clone(&self.steer_turn)),
		}
	}

	/// Hand one steering message to the live dialog. `false` when no dialog is
	/// running or the channel is full: the caller reports "steering is
	/// unavailable" instead of dropping the message silently.
	fn inject(&self, parts: Vec<pi_agent::ContentPart>) -> bool {
		match self.steer_turn.lock() {
			Ok(guard) => guard.as_ref().is_some_and(|tx| {
				tx.try_send(SteeringEvent::InjectMessage(parts)).is_ok()
			}),
			Err(_) => false,
		}
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

		let mcp_spawn = if request.tools_enabled {
			match self.runtime.block_on(load_and_filter_mcp_tools_detailed(
				&request.cwd,
				&request.config,
				&request.approvals,
				&cancellation_token,
			)) {
				Ok(spawn) => spawn,
				Err(err) => {
					drop(discovery_sig_reg);
					sig_watcher.abort();
					return Err(err);
				}
			}
		} else {
			McpSpawnResult {
				tools: Vec::new(),
				server_errors: Vec::new(),
				all_server_names: Vec::new(),
			}
		};

		drop(discovery_sig_reg);
		sig_watcher.abort();

		if cancellation_token.is_cancelled() {
			if timed_out.load(Ordering::SeqCst) {
				return Err(Error::new("execution timed out during MCP discovery"));
			}
			return Err(Error::new("MCP discovery interrupted"));
		}
		let mut approvals = request.approvals.clone();
		for tool in &mcp_spawn.tools {
			approvals.allow_mcp_tool_origin(tool.server_name(), tool.tool_name(), tool.name());
		}


		let plan = RunPlan::resolve_with_mcp(request, !mcp_spawn.tools.is_empty())?;
		let edit_options = EditToolOptions {
			enforce_seen_lines: request.config.edit.enforce_seen_lines(),
		};
		let display_resolver: Arc<dyn SessionDisplayResolver> =
			Arc::new(ConfigDisplayResolver::from_config(&request.config)?);
		let active_model = format!("{}/{}", plan.provider, plan.model);
		let resolved_display =
			display_resolver.resolve(Some(&active_model), plan.tools.contains(&"edit"));
		let session = self.session_tools(edit_options, resolved_display.edit_tool);
		session.set_session_identity(
			request.session_id.as_deref(),
			request.session_file.as_deref(),
		);
		// Router and scope are installed before the registry is built: the root
		// `read`/`write` instances below copy the scope at construction, and the
		// supervisor shares the same router with every child registry.
		session.set_internal_urls(request.internal_urls.clone());
		session.set_url_scope(request.url_scope.clone());
		// `lsp.shared`/`tools.maxTimeout` are applied before the registry is built
		// so the root tool instances carry this turn's transport and cap.
		session.set_lsp_shared(request.config.lsp.shared(), &self.runtime);
		session.set_debug_timeout(request.config.max_tool_timeout_secs);
		// `ask` needs the root context's UI: register it after the supervisor has
		// cloned its child template, so children never receive the tool.
		let build_tools: Vec<&'static str> = plan
			.tools
			.iter()
			.copied()
			.filter(|tool| *tool != "ask")
			.collect();
		let base_registry = build_registry_with_display(
			&build_tools,
			mcp_spawn.tools,
			mcp_spawn.server_errors,
			mcp_spawn.all_server_names,
			request.todo_phases.clone(),
			request.session_persisted,
			edit_options,
			Some(&session),
			resolved_display,
		)?;
		let (allow_task, allow_hub) = delegation_selection(request);
		// Needed for completion tiers and agent spawning; the `task` tool remains
		// gated separately by `allow_task`.
		let model_factory: Arc<dyn ChildModelFactory> = Arc::new(CliChildModelFactory {
			config: request.config.clone(),
			catalog: Arc::clone(&request.catalog),
			explicit_provider: request.provider.clone(),
			api_key: Some(plan.api_key.clone()),
			default_model: request.model.clone(),
			root_provider: Some(plan.provider.clone()),
			root_base_url: Some(plan.base_url.clone()),
			credential_resolver: request.credential_resolver.clone(),
			session_id: request.session_id.clone(),
			cancel: Some(cancellation_token.clone()),
		});
		let child_factory: Option<Arc<dyn ChildModelFactory>> =
			allow_task.then(|| Arc::clone(&model_factory));
		let limits = DelegationLimits {
			max_depth: request
				.config
				.max_recursion_depth
				.unwrap_or_else(|| DelegationLimits::default().max_depth),
			max_concurrency: request
				.config
				.max_concurrency
				.unwrap_or_else(|| DelegationLimits::default().max_concurrency),
		};
		let cwd = request.cwd.clone();
		let prompt = request.prompt.clone();
		let mut messages = match &request.history_messages {
			Some(override_messages) => override_messages.clone(),
			None => history_messages(&request.history)?,
		};
		if let Some(system) = plan.system_prompt {
			messages.insert(0, pi_agent::Message::system(system));
		}

		let user_message = user_message_from_parts(&prompt, &request.parts)?;

		if !plan.supports_images
			&& (user_message.has_image() || messages.iter().any(pi_agent::Message::has_image))
		{
			return Err(Error::new(format!(
				"model '{}/{}' does not support image input",
				plan.provider, plan.model
			)));
		}
		let plan_api = plan.api.clone();
		let plan_provider = plan.provider.clone();
		let plan_model = plan.model.clone();
		let provider = OpenAiCompatibleProvider::new(plan.base_url, plan.api_key, plan.model)
			.map_err(|err| Error::with_source("pi-agent provider setup failed", err))?
			.with_effort(plan.effort)
			.with_images(plan.supports_images)
			.with_prompt_cache_enabled(plan.provider == "kimi-code")
			// Per-model `compat.supportsUsageInStreaming` override, else the TS
			// provider default (`!isCerebras`, `compat/openai.ts`): the Cerebras
			// dialect rejects the `stream_options` field.
			.with_usage_in_streaming(plan.supports_usage_in_streaming)
			.with_session_id(request.provider_session_id.clone())
			.with_prompt_cache_key(request.prompt_cache_key.clone());
		let config = AgentConfig {
			max_turns: plan.max_turns,
			context_limit: plan.context_limit,
			system_prompt: None,
			cwd,
		};
		let provider: Arc<dyn ModelProvider> = Arc::new(provider);
		// Children never prompt: keep the interactive `ask` surface on the root
		// approver only.
		let child_approvals: Arc<dyn ToolApprover> = Arc::new(approvals.without_ask());
		let approvals: Arc<dyn ToolApprover> = Arc::new(approvals);
		let mut root_supervisor: Option<Arc<Supervisor>> = None;
		let mut root_sink = None;
		let process_backend = (allow_task
			|| allow_hub
			|| plan.tools.iter().any(|tool| *tool == "bash"))
			.then(|| self.process_backend(&request.cwd));
		let mut registry = if allow_task || allow_hub {
			let mut slot = self.supervisor.lock().map_err(|_| {
				Error::new("delegation supervisor lock poisoned")
			})?;
			let supervisor = match slot.as_ref() {
				Some(supervisor) => {
					supervisor.rebind(
						base_registry,
						provider.clone(),
						Some(child_approvals.clone()),
						limits,
						allow_task,
						allow_hub,
					);
					Arc::clone(supervisor)
				},
				None => {
					let supervisor = Supervisor::with_selection(
						base_registry,
						provider.clone(),
						Some(child_approvals.clone()),
						limits,
						allow_task,
						allow_hub,
					);
					*slot = Some(Arc::clone(&supervisor));
					supervisor
				},
			};
			supervisor.set_edit_options(edit_options);
			supervisor.set_display_resolver(Arc::clone(&display_resolver));
			supervisor.set_child_tool_settings(ChildToolSettings {
				lsp_shared: request.config.lsp.shared(),
				debug_max_timeout: request.config.max_tool_timeout_secs,
			});
			supervisor.set_active_model(active_model.clone());
			if let Some(observer) = &request.subagent_observer {
				// Idempotent: a supervisor reused across turns keeps one
				// subscription, and a replaced one is subscribed again.
				supervisor.subscribe_once(&observer.0);
			}
			supervisor.set_internal_urls(request.internal_urls.clone());
			if let Some(scope) = &request.url_scope {
				supervisor.set_url_scope(Arc::clone(scope));
			}
			supervisor.set_child_session_root(request.child_session_root.clone());
			let isolation_base =
				crate::worktree_cmd::resolve_base(request.config.worktree_base.as_deref())
					.map_err(|message| Error::new(message))?;
			supervisor.set_task_isolation(TaskIsolationSettings {
				mode: request.config.task_isolation_mode.unwrap_or_default(),
				apply: request.config.task_isolation_apply.unwrap_or(true),
				merge: request.config.task_isolation_merge.unwrap_or_default(),
				commits: request.config.task_isolation_commits.unwrap_or_default(),
				worktree_base: Some(isolation_base),
			});
			if let Some(processes) = &process_backend {
				supervisor.set_processes(Arc::clone(processes));
			}
			if let Some(factory) = &child_factory {
				match supervisor.agent_catalog() {
					// Definitions and policy are discovered once per runner; only
					// the resolver follows the root model. A running child keeps
					// the snapshot it captured at spawn.
					Some(catalog) => catalog.set_model_factory(Arc::clone(factory)),
					None => {
						let extension_roots = request
							.config
							.extensions
							.as_deref()
							.unwrap_or(&[])
							.iter()
							.map(|path| crate::dirs::absolute(&request.cwd, path))
							.collect::<Vec<_>>();
						let discovery =
							crate::agents::discover_agents(&request.cwd, &extension_roots)?;
						let catalog = AgentCatalog::new(
							discovery.agents,
							discovery.project_agents_dir,
						)
						.with_policy(
							request.config.disabled_agents.clone().unwrap_or_default(),
							request.config.agent_model_overrides.clone(),
							request.config.max_recursion_depth,
							request.config.max_effort.clone(),
						)
						.with_model_factory(Arc::clone(factory));
						supervisor.set_agent_catalog(Arc::new(catalog));
					},
				}
			}
			supervisor.set_async_enabled(request.config.async_config.enabled());
			root_sink = Some(supervisor.owner_results_sink(None));
			root_supervisor = Some(Arc::clone(&supervisor));
			supervisor.root_registry()
		} else {
			base_registry
		};
		// Bash-only sessions still need the real process backend: `bash async`
		// must run on the same scoped supervisor as hub process ops.
		if root_supervisor.is_none()
			&& let Some(processes) = &process_backend
			&& registry.get("bash").is_some()
		{
			processes.set_async_enabled(request.config.async_config.enabled());
			registry.remove("bash");
			registry.register(Arc::new(BashTool::with_processes(Arc::clone(processes), None)));
		}
		if plan.tools.contains(&"ask") {
			registry.register(Arc::new(AskTool::new()));
		}
		// Root-only checkpoint pair: children inherit the supervisor template,
		// which never receives these, so they cannot capture the root store.
		let checkpoint_allowed = request.tools_enabled
			&& request.config.tools.as_ref().is_none_or(|allowed| {
				allowed.iter().any(|tool| tool == "checkpoint" || tool == "rewind")
			});
		if checkpoint_allowed
			&& let Some(controller) = &request.checkpoint_controller
		{
			registry.register(Arc::new(CheckpointTool::new(Arc::clone(controller))));
			registry.register(Arc::new(RewindTool::new(Arc::clone(controller))));
		}
		// Eval is mounted after the final registry exists: the host's bridged
		// `tool.*` view must see ask/checkpoint/bash, and registering before the
		// supervisor clones its template would leak a root-bound host to children.
		let eval_allowed = request.tools_enabled && plan.tools.contains(&"eval");
		let eval_binder = if eval_allowed {
			let settings = crate::eval_host::eval_settings(&request.config);
			let eval_runtime = session.eval_runtime(&self.runtime, settings)?;
			let artifacts_dir = request
				.session_file
				.as_deref()
				.and_then(crate::session::artifacts_directory_for);
			if let Some(directory) = &artifacts_dir {
				std::fs::create_dir_all(directory).map_err(|error| {
					Error::new(format!(
						"cannot create session artifacts directory {}: {error}",
						directory.display()
					))
				})?;
			}
			let context = crate::eval_host::EvalMountContext {
				runtime: eval_runtime,
				supervisor: root_supervisor.as_ref().map(Arc::downgrade),
				provider: Arc::clone(&provider),
				model_factory: Some(Arc::clone(&model_factory)),
				session_id: session.eval_session_id(),
				session_file: request.session_file.clone(),
				artifacts_dir,
				limits,
			};
			Some(Arc::new(crate::eval_host::CliEvalBinder::new(context))
				as Arc<dyn pi_agent::eval::EvalToolBinder>)
		} else {
			None
		};
		if let Some(supervisor) = &root_supervisor {
			supervisor.set_eval_binder(eval_binder.clone());
		}
		if let Some(binder) = &eval_binder {
			binder.bind(&mut registry, None);
		}
		// Host tools mount last, after every root-owned tool exists, so a host
		// definition can never shadow `read`, `bash`, a checkpoint pair or a
		// bridged eval tool. Registered one at a time: the second occurrence of
		// a name fails the same way whether it repeats the batch or an existing
		// tool. Children never see these — the supervisor cloned its template
		// before this point — and a tool-disabled request mounts none: the
		// compaction request sets `tools_enabled = false`, and advertising host
		// adapters there would let the summarizer call out to the host mid-turn.
		if request.tools_enabled {
			for tool in &request.extra_tools {
				let name = tool.0.name();
				if registry.get(name).is_some() {
					return Err(Error::new(format!("Duplicate tool name: {name}")));
				}
				registry.register(Arc::clone(&tool.0));
			}
		}
		let mut agent = AgentLoop::with_approver(config, provider, registry, approvals);
		if let Some(ack) = &request.turn_ack {
			agent = agent.with_turn_ack(Arc::clone(ack));
		}
		let agent = match (root_supervisor, root_sink) {
			(Some(supervisor), Some(sink)) => agent
				.with_background_results(sink)
				.with_peer_inbox(supervisor.mailbox(), pi_agent::mailbox::MAIN_AGENT_ID),
			_ => agent,
		};
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

		let signal_reg = self.dispatcher.register_dialog(steering_tx.clone());

		let end_user_message = user_message.clone();
		let dialog_message = user_message;
		let dialog_handle = self.runtime.spawn(async move {
			agent
				.run_dialog_message(dialog_message, &mut state, Some(steering_rx), Some(pi_events_tx))
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
					AgentEvent::ToolUpdate { id, name, args, partial } => {
						if event_tx_clone
							.send(Ok(Event::ToolUpdate { id, tool: name, args, partial }))
							.await
							.is_err()
						{
							cancel_on_err.cancel();
							let _ = steering_tx_err.try_send(SteeringEvent::Interrupt);
							abort_after_grace(dialog_abort_clone);
							return;
						}
					},
					AgentEvent::ToolEnd { id, name, result, is_error, images, details } => {
						let error = is_error.then(|| result.clone());
						if event_tx_clone
							.send(Ok(Event::ToolEnd { id, tool: name, result, error, images, details }))
							.await
							.is_err()
						{
							cancel_on_err.cancel();
							let _ = steering_tx_err.try_send(SteeringEvent::Interrupt);
							abort_after_grace(dialog_abort_clone);
							return;
						}
					},
					AgentEvent::UserSteer { parts } => {
						if event_tx_clone.send(Ok(Event::UserSteer { parts })).await.is_err() {
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
					let mut new_messages = vec![end_user_message];
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
	/// Whether the streaming request asks for a usage trailer. Per-model
	/// `compat.supportsUsageInStreaming` wins; otherwise the TS provider default
	/// (`!isCerebras`) applies.
	supports_usage_in_streaming: bool,
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
		//
		// Precedence is TS `getApiKey` (`model-registry.ts:2297-2315`): an explicit
		// command/config key returns before the store is consulted at all, then the
		// credential store (login, env, static), then the connection block. The
		// store lookup is driven by the turn's own token, so abort/EOF interrupts a
		// slow refresh instead of stranding it.
		let api_key = match request.api_key.clone() {
			Some(explicit) => explicit,
			None => {
				let cancel = match &request.cancellation_token {
					Some(token) => {
						let token = token.clone();
						crate::auth::http::Cancel::with_predicate(std::sync::Arc::new(move || {
							token.is_cancelled()
						}))
					},
					None => crate::auth::http::Cancel::new(),
				};
				let stored_key = match request.credential_resolver.as_ref() {
					Some(resolver) => resolver
						.0
						.api_key(&provider, request.session_id.as_deref(), &cancel)
						.map_err(|error| Error::with_source("credential lookup failed", error))?,
					None => None,
				};
				stored_key
					.or_else(|| {
						connection
							.as_ref()
							.and_then(|connection| connection.resolve_api_key())
					})
					.unwrap_or_default()
			},
		};
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
		let supports_usage_in_streaming = request
			.catalog
			.supports_usage_in_streaming(&provider, &model)
			.unwrap_or(provider != "cerebras");
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
			supports_usage_in_streaming,
		})
	}
}
/// Resolves an agent model pattern through config roles, the model catalog, and
/// `config.yml` connections, then builds a provider for that child only.
/// The root provider is untouched, so `/model` changes refresh only the
/// factory snapshot installed on the catalog.
struct CliChildModelFactory {
	config:              crate::config::Config,
	catalog:             Arc<crate::catalog::Catalog>,
	explicit_provider:   Option<String>,
	/// Key the root turn actually used (`RunPlan::api_key`). Reused only for
	/// the root provider **and** base URL; never forwarded to another target.
	api_key:             Option<String>,
	default_model:       Option<String>,
	/// Provider the root run actually selected. The root run key may only be
	/// reused for this provider; every other child resolves its own credential.
	root_provider:       Option<String>,
	/// Base URL the root run actually used, paired with `root_provider`.
	root_base_url:       Option<String>,
	/// Shared credential store handle. Consulted for a child that overrides to
	/// another provider; `None` keeps the connection-block fallback only.
	credential_resolver: Option<crate::agent::ResolverHandle>,
	/// Root session id for `getApiKey` sticky selection.
	session_id:          Option<String>,
	/// The turn's own cancellation token, so a cancelled child credential
	/// lookup fails closed instead of hitting the store.
	cancel:              Option<pi_agent::CancellationToken>,
}

impl CliChildModelFactory {
	/// Resolve a CLI/agent model pattern to an ordered candidate list using the
	/// same role rules as `model-resolver.ts`: `@role`, `pi/role`, `*` aliases,
	/// comma lists, thinking suffixes, configured values, default inheritance for
	/// smol/slow/designer, and the bundled `priority.json` chains.
	fn patterns_for(&self, raw: &str) -> std::result::Result<Vec<String>, String> {
		let raw = raw.trim();
		if raw.is_empty() {
			return Err("empty model pattern".to_string());
		}
		let Some((role, suffix)) = role_alias(raw, &self.config.model_roles) else {
			return Ok(vec![raw.to_string()]);
		};
		let mut visited = HashSet::new();
		let mut patterns = self.role_patterns(&role, &mut visited)?;
		if patterns.is_empty() {
			patterns = self.default_patterns();
		} else if let Some(level) = suffix {
			patterns = patterns
				.into_iter()
				.map(|pattern| format!("{pattern}:{level}"))
				.collect();
		}
		if patterns.is_empty() {
			return Err(format!(
				"model role '@{role}' has no resolvable model and no default model is set"
			));
		}
		Ok(patterns)
	}

	fn default_patterns(&self) -> Vec<String> {
		for candidate in [
			self.default_model.as_deref(),
			self.config.default_model.as_deref(),
		] {
			let Some(candidate) = candidate.map(str::trim).filter(|value| !value.is_empty()) else {
				continue;
			};
			if let Some((role, suffix)) = role_alias(candidate, &self.config.model_roles) {
				let mut visited = HashSet::new();
				if let Ok(mut patterns) = self.role_patterns(&role, &mut visited) {
					if let Some(level) = suffix {
						patterns = patterns
							.into_iter()
							.map(|pattern| format!("{pattern}:{level}"))
							.collect();
					}
					if !patterns.is_empty() {
						return patterns;
					}
				}
				// An alias that expands to nothing must not be returned as a
				// literal model id; try the next fallback candidate.
				continue;
			}
			let patterns = split_pattern_list(candidate);
			if !patterns.is_empty() {
				return patterns;
			}
		}
		Vec::new()
	}

	fn role_patterns(
		&self,
		role: &str,
		visited: &mut HashSet<String>,
	) -> std::result::Result<Vec<String>, String> {
		if !visited.insert(role.to_string()) {
			return Err(format!("model role cycle while resolving '@{role}'"));
		}
		let configured = self
			.config
			.model_roles
			.get(role)
			.map(|value| value.trim())
			.filter(|value| !value.is_empty())
			.map(str::to_string);
		let mut patterns = if let Some(configured) = configured {
			split_pattern_list(&configured)
		} else if should_inherit_default(role) {
			match self
				.config
				.model_roles
				.get("default")
				.map(|value| value.trim())
				.filter(|value| !value.is_empty())
			{
				Some(default) => self.inherited_default(role, default, visited)?,
				None => self.priority(role),
			}
		} else {
			self.priority(role)
		};
		if patterns.is_empty() {
			patterns = self.priority(role);
		}

		let mut resolved = Vec::new();
		for pattern in patterns {
			if let Some((alias, suffix)) = role_alias(&pattern, &self.config.model_roles) {
				let mut branch = visited.clone();
				if let Ok(mut nested) = self.role_patterns(&alias, &mut branch) {
					if !nested.is_empty() {
						if let Some(level) = suffix {
							nested = nested
								.into_iter()
								.map(|pattern| format!("{pattern}:{level}"))
								.collect();
						}
						resolved.extend(nested);
						continue;
					}
				}
			}
			resolved.push(pattern);
		}
		Ok(resolved)
	}

	fn inherited_default(
		&self,
		role: &str,
		default_value: &str,
		visited: &HashSet<String>,
	) -> std::result::Result<Vec<String>, String> {
		let mut resolved = Vec::new();
		for pattern in split_pattern_list(default_value) {
			let (base, level) = split_thinking(&pattern, 0);
			if let Some((alias, _)) = role_alias(base, &self.config.model_roles) {
				if alias == role {
					// `modelRoles.default = "@smol"` must not loop back.
					let defaults = self.priority(role);
					resolved.extend(defaults.into_iter().map(|default| match level {
						Some(level) => format!("{default}:{level}"),
						None => default,
					}));
					continue;
				}
				if !visited.contains(&alias) {
					let mut branch = visited.clone();
					if let Ok(nested) = self.role_patterns(&alias, &mut branch) {
						if !nested.is_empty() {
							resolved.extend(nested);
							continue;
						}
					}
				}
			}
			resolved.push(pattern);
		}
		Ok(resolved)
	}

	fn priority(&self, role: &str) -> Vec<String> {
		let key = match role {
			"advisor" => "slow",
			"tiny" => "smol",
			other => other,
		};
		priority_json()
			.get(key)
			.and_then(serde_json::Value::as_array)
			.map(|patterns| {
				patterns
					.iter()
					.filter_map(|pattern| pattern.as_str().map(str::to_string))
					.collect()
			})
			.unwrap_or_default()
	}

	fn locate(
		&self,
		pattern: &str,
	) -> std::result::Result<
		(String, String, String, bool, Option<String>, crate::config::ProviderConfig),
		String,
	> {
		let (base, selector) = split_thinking(pattern, -1);
		let effort = selector.and_then(|selector| match selector {
			"inherit" | "auto" | "off" => None,
			other => Some(other.to_string()),
		});
		let (provider, model) = match base.split_once('/') {
			Some((provider, id)) if !provider.is_empty() && !id.is_empty() => {
				(provider.to_string(), id.to_string())
			},
			_ => {
				let provider = self
					.explicit_provider
					.clone()
					.or_else(|| self.catalog.provider_of(base).map(str::to_string))
					.ok_or_else(|| {
						format!(
							"no provider for model `{base}`: pass --provider or use `provider/{base}`"
						)
					})?;
				(provider, base.to_string())
			},
		};
		let connection = self
			.config
			.providers
			.get(&provider)
			.cloned()
			.or_else(|| self.catalog.connections.get(&provider).cloned())
			.ok_or_else(|| format!("provider `{provider}` has no connection block"))?;
		let base_url = connection
			.base_url
			.clone()
			.ok_or_else(|| format!("provider `{provider}` has no baseUrl"))?;
		let supports_images = self.catalog.supports_images(&provider, &model).unwrap_or(true);
		Ok((provider, model, base_url, supports_images, effort, connection))
	}

	/// Key for a resolved child target. The root turn's key is reused only for
	/// the identical provider+base URL (the existing same-provider guard). Any
	/// other target resolves its own credential from the shared store first,
	/// then its connection block — the root secret is never copied across.
	fn api_key_for(
		&self,
		provider: &str,
		base_url: &str,
		connection: &crate::config::ProviderConfig,
	) -> std::result::Result<String, String> {
		let reuses_root = self.root_provider.as_deref() == Some(provider)
			&& self.root_base_url.as_deref() == Some(base_url);
		if reuses_root {
			return Ok(self.api_key.clone().unwrap_or_default());
		}
		let cancel = match &self.cancel {
			Some(token) => {
				let token = token.clone();
				crate::auth::http::Cancel::with_predicate(std::sync::Arc::new(move || {
					token.is_cancelled()
				}))
			},
			None => crate::auth::http::Cancel::new(),
		};
		let stored = match &self.credential_resolver {
			Some(resolver) => resolver
				.0
				.api_key(provider, self.session_id.as_deref(), &cancel)
				.map_err(|error| {
					format!("credential lookup failed for provider `{provider}`: {error}")
				})?,
			None => None,
		};
		Ok(stored
			.or_else(|| connection.resolve_api_key())
			.unwrap_or_default())
	}

	fn target(
		&self,
		pattern: &str,
	) -> std::result::Result<(String, String, String, String, bool, Option<String>), String> {
		let (provider, model, base_url, supports_images, effort, connection) = self.locate(pattern)?;
		let api_key = self.api_key_for(&provider, &base_url, &connection)?;
		Ok((provider, model, base_url, api_key, supports_images, effort))
	}
}

impl ChildModelFactory for CliChildModelFactory {
	fn can_resolve(&self, model: &str) -> bool {
		// Structural probe only: never touches the credential store.
		self.patterns_for(model)
			.map(|patterns| patterns.iter().any(|pattern| self.locate(pattern).is_ok()))
			.unwrap_or(false)
	}

	fn resolve(
		&self,
		model: &str,
		effort: Option<&str>,
		fallback_effort: Option<&str>,
	) -> std::result::Result<ChildModel, String> {
		let patterns = self.patterns_for(model)?;
		let mut last_error = None;
		for pattern in patterns {
			let (provider_id, id, base_url, api_key, supports_images, pattern_effort) =
				match self.target(&pattern) {
					Ok(target) => target,
					Err(error) => {
						last_error = Some(error);
						continue;
					},
				};
			let chosen_effort = effort
				.map(str::to_string)
				.or(pattern_effort)
				.or_else(|| fallback_effort.map(str::to_string));
			let provider = match OpenAiCompatibleProvider::new(base_url, api_key, id.clone()) {
				Ok(provider) => provider
					.with_effort(chosen_effort)
					.with_images(supports_images)
					.with_prompt_cache_enabled(provider_id == "kimi-code")
					// Per-model `compat.supportsUsageInStreaming`, else TS
					// provider default `!isCerebras`.
					.with_usage_in_streaming(
						self
							.catalog
							.supports_usage_in_streaming(&provider_id, &id)
							.unwrap_or(provider_id != "cerebras"),
					),
				Err(error) => {
					last_error = Some(error.to_string());
					continue;
				},
			};
			return Ok(ChildModel {
				provider: Arc::new(provider),
				model: format!("{provider_id}/{id}"),
				role: None,
			});
		}
		Err(last_error.unwrap_or_else(|| format!("no model candidate for '{model}'")))
	}
}

/// Role ids from `model-roles.ts`; configured custom roles are also accepted.
const MODEL_ROLE_IDS: [&str; 10] = [
	"default", "smol", "slow", "vision", "plan", "designer", "commit", "tiny", "task", "advisor",
];

/// Thinking selectors accepted after `:` in a model pattern (TS `thinking.ts`).
const THINKING_SELECTORS: [&str; 9] =
	["inherit", "auto", "off", "minimal", "low", "medium", "high", "xhigh", "max"];

static PRIORITY_JSON: std::sync::LazyLock<serde_json::Value> =
	std::sync::LazyLock::new(|| {
		serde_json::from_str(include_str!(
			"../../../packages/coding-agent/src/priority.json"
		))
		.unwrap_or(serde_json::Value::Null)
	});

fn priority_json() -> &'static serde_json::Value {
	&PRIORITY_JSON
}

fn should_inherit_default(role: &str) -> bool {
	matches!(role, "smol" | "slow" | "designer")
}

fn split_pattern_list(value: &str) -> Vec<String> {
	value
		.split(',')
		.map(str::trim)
		.filter(|pattern| !pattern.is_empty())
		.map(str::to_string)
		.collect()
}

fn is_known_role(role: &str, configured: &std::collections::BTreeMap<String, String>) -> bool {
	MODEL_ROLE_IDS.contains(&role) || configured.contains_key(role)
}

/// Recognize `@role`, legacy `pi/role`, and the `*` default alias, returning the
/// role and an optional `:thinking` suffix.
fn role_alias(
	value: &str,
	configured: &std::collections::BTreeMap<String, String>,
) -> Option<(String, Option<&'static str>)> {
	let value = value.trim();
	let prefix_len = if value == "*" || value.starts_with("*:") {
		0
	} else if value.starts_with('@') {
		1
	} else if value.starts_with("pi/") {
		3
	} else {
		return None;
	};
	let (base, suffix) = split_thinking(value, prefix_len as isize);
	let candidate = if base == "*" {
		"default"
	} else {
		base.get(prefix_len..)?
	};
	if candidate.is_empty() || !is_known_role(candidate, configured) {
		return None;
	}
	Some((candidate.to_string(), suffix))
}

/// Unique-prefix thinking selector split, mirroring `splitThinkingSuffix`.
fn split_thinking(model: &str, min_colon_index: isize) -> (&str, Option<&'static str>) {
	let Some(colon) = model.rfind(':') else {
		return (model, None);
	};
	if (colon as isize) <= min_colon_index {
		return (model, None);
	}
	let suffix = &model[colon + 1..];
	if suffix.len() < 2 {
		return (model, None);
	}
	let mut matches = THINKING_SELECTORS
		.iter()
		.filter(|selector| selector.starts_with(suffix))
		.copied();
	let Some(selector) = matches.next() else {
		return (model, None);
	};
	if matches.next().is_some() {
		return (model, None);
	}
	(&model[..colon], Some(selector))
}


/// Root model-pattern suffix split, shared with the child catalog parser.
///
/// `off` and `auto` selectors strip from the model id but never reach the
/// provider as an effort: `off` disables reasoning and `auto` defers to the
/// endpoint (TS `toReasoningEffort`/`concreteThinkingLevel`).
pub fn split_effort(model: &str) -> (&str, Option<&str>) {
	let (base, effort) = split_effort_selector(model);
	if let Some(effort) = effort {
		return (base, (effort != "off").then_some(effort));
	}
	match model.rsplit_once(':') {
		Some((base, "auto")) if !base.is_empty() => (base, None),
		_ => (model, None),
	}
}

/// A validated `--thinking` selector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CliThinking {
	Off,
	Auto,
	Effort(&'static str),
}

/// Parses a `--thinking` value exactly like TS `parseCliThinkingLevel`:
/// case-sensitive unique prefixes of `off`..`max`, plus the exact `auto`
/// sentinel. `inherit` is not a CLI selector.
#[must_use]
pub fn parse_cli_thinking(value: &str) -> Option<CliThinking> {
	if value.is_empty() || value.trim() != value || value != value.to_ascii_lowercase() {
		return None;
	}
	if value == "auto" {
		return Some(CliThinking::Auto);
	}
	match parse_effort_prefix(value) {
		Some("off") => Some(CliThinking::Off),
		Some(effort) => Some(CliThinking::Effort(effort)),
		None => None,
	}
}

/// Rewrites a model selector so the explicit `--thinking` level wins over any
/// `:level` suffix already on it (TS `main.ts` precedence).
#[must_use]
pub fn apply_thinking(model: &str, level: CliThinking) -> String {
	let (base, _) = split_effort(model);
	match level {
		CliThinking::Effort(effort) => format!("{base}:{effort}"),
		CliThinking::Off | CliThinking::Auto => base.to_string(),
	}
}

/// Warning emitted for an unparseable `--thinking` value; the flag is ignored
/// and the model suffix or endpoint default still applies (TS flag table).
#[must_use]
pub fn cli_thinking_warning(value: &str) -> String {
	format!(
		"warning: Invalid thinking level passed to --thinking: {value} (valid: {}, auto)",
		EFFORT_SELECTORS.join(", ")
	)
}

/// Which supervisor-backed tools this run may offer. `--tools` is a whitelist:
/// without it both delegation tools are on, and naming one (but not the other)
/// enables exactly that subset. `--no-tools` disables both.
pub fn delegation_selection(request: &RunRequest) -> (bool, bool) {
	if !request.tools_enabled {
		return (false, false);
	}
	match &request.config.tools {
		None => (true, true),
		Some(allowed) => (
			allowed.iter().any(|tool| tool == "task"),
			allowed.iter().any(|tool| tool == "hub"),
		),
	}
}

/// Tools the model may call, preserving explicit configuration restrictions.
fn allowed_tools(request: &RunRequest, _has_mcp_tools: bool) -> Result<Vec<&'static str>> {
	if !request.tools_enabled {
		return Ok(Vec::new());
	}
	let ask_available = request.config.ask.enabled() && request.approvals.ask_channel().is_some();
	let lsp_enabled = request.config.lsp.enabled();
	let debug_enabled = request.config.debug.enabled();
	let mut tools: Vec<&'static str> = BUILTIN_TOOLS
		.iter()
		.map(|(name, _)| *name)
		.filter(|name| match *name {
			"lsp" => lsp_enabled,
			"debug" => debug_enabled,
			_ => true,
		})
		.collect();
	if let Some(allowed) = &request.config.tools {
		// An excluded upstream tool must state the boundary instead of vanishing
		// from the selected set; the table is the dispatcher's, not a copy.
		for name in allowed {
			if let Some(reason) = excluded_tool_reason(name) {
				return Err(Error::new(format!(
					"tool `{name}` is excluded by the platform boundary: {reason}"
				)));
			}
		}
		// A setting that turns a selected tool off is a configuration conflict,
		// not a silent empty selection.
		if allowed.iter().any(|tool| tool == "lsp") && !lsp_enabled {
			return Err(Error::new(
				"tool `lsp` is disabled by `lsp.enabled`; enable it or drop it from --tools",
			));
		}
		if allowed.iter().any(|tool| tool == "debug") && !debug_enabled {
			return Err(Error::new(
				"tool `debug` is disabled by `debug.enabled`; enable it or drop it from --tools",
			));
		}
		tools.retain(|name| allowed.iter().any(|a| a == *name));
		if allowed.iter().any(|tool| tool == "ask") && !ask_available {
			return Err(Error::new(
				"tool `ask` requires interactive stdin/stdout and ask.enabled",
			));
		}
	}
	if !ask_available {
		tools.retain(|name| *name != "ask");
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
	build_registry_with_mcp_and_diagnostics(tools, mcp_tools, Vec::new(), Vec::new())
}

pub fn build_registry_with_mcp_and_diagnostics(
	tools: &[&'static str],
	mcp_tools: Vec<McpTool>,
	server_errors: Vec<(String, String)>,
	all_server_names: Vec<String>,
) -> Result<ToolRegistry> {
	build_registry_full(
		tools,
		mcp_tools,
		server_errors,
		all_server_names,
		Vec::new(),
		false,
		EditToolOptions::default(),
		None,
	)
}

/// Build the registry with session-restored `todo` phases.
#[allow(clippy::too_many_arguments)]
fn build_registry_full(
	tools: &[&'static str],
	mcp_tools: Vec<McpTool>,
	server_errors: Vec<(String, String)>,
	all_server_names: Vec<String>,
	todo_phases: Vec<pi_agent::tools::todo::TodoPhase>,
	session_persisted: bool,
	edit_options: EditToolOptions,
	session: Option<&Arc<RunnerSession>>,
) -> Result<ToolRegistry> {
	build_registry_with_display(
		tools,
		mcp_tools,
		server_errors,
		all_server_names,
		todo_phases,
		session_persisted,
		edit_options,
		session,
		default_session_tools(tools.contains(&"edit")),
	)
}

/// Build the registry for one resolved session (CLI settings + active model).
#[allow(clippy::too_many_arguments)]
fn build_registry_with_display(
	tools: &[&'static str],
	mcp_tools: Vec<McpTool>,
	server_errors: Vec<(String, String)>,
	all_server_names: Vec<String>,
	todo_phases: Vec<pi_agent::tools::todo::TodoPhase>,
	session_persisted: bool,
	edit_options: EditToolOptions,
	session: Option<&Arc<RunnerSession>>,
	resolved: ResolvedSessionTools,
) -> Result<ToolRegistry> {
	let mut registry = ToolRegistry::new();
	// Root turns share one snapshot/`EditTool` pair and one pending queue (see
	// `RunnerSession`); a standalone registry (tests, MCP-less helpers) gets
	// fresh instances. Child agents always get their own through
	// `Supervisor::child_registry`.
	let snapshots = session
		.map_or_else(|| Arc::new(SnapshotStore::new()), |session| Arc::clone(&session.snapshots));
	let pending = session
		.map_or_else(|| Arc::new(PendingStore::new()), |session| Arc::clone(&session.pending));
	let read_sink = Arc::new(NativeSnapshotSink::new(snapshots.clone()));
	// Same router and scope the supervisor uses, so a child and the root resolve
	// identical `scheme://` inputs. Without a router the tools stay filesystem-only.
	let internal_urls = session.and_then(|session| session.internal_urls());
	let url_scope = session.map_or_else(
		|| Arc::new(pi_agent::tools::internal_urls::UrlScope::default()),
		|session| session.url_scope(),
	);
	for name in tools {
		let tool: Arc<dyn Tool> = match *name {
			"read" => {
				let read = ReadTool::with_snapshot_sink(read_sink.clone())
					.with_display_inputs(resolved.display);
				Arc::new(match &internal_urls {
					Some(router) => read.with_internal_urls(Arc::clone(router), Arc::clone(&url_scope)),
					None => read,
				})
			},
			"glob" => Arc::new(GlobTool),
			"grep" => Arc::new(GrepTool),
			"ast_grep" => Arc::new(
				AstGrepTool::with_snapshot_sink(read_sink.clone())
					.with_display_inputs(resolved.display),
			),
			"ast_edit" => Arc::new(pi_agent::tools::AstEditTool::with_display_inputs(
				Arc::clone(&snapshots),
				Arc::clone(&pending),
				resolved.display,
			)),
			"write" => {
				let write = WriteTool::with_pending(Arc::clone(&pending));
				Arc::new(match &internal_urls {
					Some(router) => write.with_internal_urls(Arc::clone(router), Arc::clone(&url_scope)),
					None => write,
				})
			},
			"edit" => match session {
				Some(session) => session.edit.clone(),
				None => Arc::new(EditTool::with_mode(
					snapshots.clone(),
					edit_options,
					resolved.edit_tool,
				)),
			},
			"bash" => Arc::new(BashTool::new()),
			"todo" => Arc::new(TodoTool::restored(todo_phases.clone(), session_persisted)),
			"ask" => Arc::new(AskTool::new()),
			// Session-scoped: the LSP owner scope and the debug session tree live
			// on `RunnerSession`, because the registry (and these tool instances)
			// is rebuilt every prompt while servers/debuggees must survive turns.
			"lsp" => match session {
				Some(session) => Arc::new(LspTool::with_session(session.lsp_session())),
				None => Arc::new(LspTool::new()),
			},
			"debug" => match session {
				Some(session) => Arc::new(
					DapTool::shared(session.dap_manager()).with_max_timeout(session.debug_timeout()),
				),
				None => Arc::new(DapTool::new()),
			},
			// Eval needs a session-scoped runtime/host; `run_turn` mounts it into
			// the final root/child registries instead.
			"eval" => continue,
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
	for (server, err) in server_errors {
		registry.register_server_error(server, err);
	}
	registry.set_configured_servers(all_server_names);
	// The soft requirement must follow the same queue: a root turn keeps the
	// session policy, while a standalone registry (tests, helpers) gets one
	// bound to the fresh queue it just created.
	match session {
		Some(session) => registry.set_policy(Arc::clone(&session.policy)),
		None => registry.set_policy(Arc::new(pi_agent::state::StagedPreviewPolicy::new(pending))),
	}
	Ok(registry)
}

fn format_mcp_infrastructure_error(server_name: &str, stage: &str, err: &McpError) -> String {
	let category = match err {
		McpError::Http { status, .. } => {
			if *status == 401 || *status == 403 {
				format!("HTTP error {status} (authentication/authorization failed)")
			} else {
				format!("HTTP error {status}")
			}
		},
		McpError::Timeout(_) => {
			if stage == "call" {
				"request timed out (execution uncertain; do not replay)".to_string()
			} else {
				"request timed out".to_string()
			}
		},
		McpError::AcceptedStreamError(_) => {
			"stream disconnected after call accepted (non-replayable, execution uncertain)".to_string()
		},
		McpError::Protocol(_) => "protocol error".to_string(),
		McpError::Transport(_) => "connection failed".to_string(),
		McpError::Closed => "connection closed".to_string(),
		McpError::JsonRpc { code, .. } => format!("JSON-RPC error {code}"),
		McpError::Tool(_) => "tool execution error".to_string(),
		McpError::Config(_) => "configuration error".to_string(),
		McpError::OAuthUnsupported(_) => "OAuth unsupported".to_string(),
		_ => "execution failed".to_string(),
	};

	match stage {
		"connect" => format!("Failed to connect to MCP server \"{server_name}\": {category}"),
		"list" => format!("Failed to list tools from MCP server \"{server_name}\": {category}"),
		"call" => format!("MCP tool error: server \"{server_name}\" {category}"),
		_ => format!("MCP error on server \"{server_name}\" during {stage}: {category}"),
	}
}

#[cfg(test)]
mod tests_mcp_infrastructure_error {
	use super::*;

	#[test]
	fn test_json_rpc_error_redacts_message_secrets() {
		let err = McpError::JsonRpc {
			code: -32603,
			message: "secret_db_password_12345".to_string(),
			data: None,
		};
		let res = format_mcp_infrastructure_error("my_server", "call", &err);
		assert!(!res.contains("secret_db_password_12345"));
		assert_eq!(res, "MCP tool error: server \"my_server\" JSON-RPC error -32603");
	}

	#[test]
	fn test_timeout_marks_uncertain_only_on_call() {
		let err = McpError::Timeout("timed out waiting for response".to_string());
		let call_res = format_mcp_infrastructure_error("my_server", "call", &err);
		assert_eq!(
			call_res,
			"MCP tool error: server \"my_server\" request timed out (execution uncertain; do not replay)"
		);

		let connect_res = format_mcp_infrastructure_error("my_server", "connect", &err);
		assert_eq!(
			connect_res,
			"Failed to connect to MCP server \"my_server\": request timed out"
		);

		let list_res = format_mcp_infrastructure_error("my_server", "list", &err);
		assert_eq!(
			list_res,
			"Failed to list tools from MCP server \"my_server\": request timed out"
		);
	}
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
				terminate:    false,
				details:      None,
			});
		}

		tokio::select! {
			_ = ctx.cancellation_token.cancelled() => {
				Ok(pi_agent::ToolResult {
					tool_call_id: String::new(),
					content: format!("MCP tool {} cancelled", self.name),
					is_error: true,
					terminate: false,
					details: None,
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
						Ok(pi_agent::ToolResult {
							tool_call_id: String::new(),
							content,
							is_error,
							terminate: false,
							details: None,
						})
					},
					Err(err) => Ok(pi_agent::ToolResult {
						tool_call_id: String::new(),
						content: format_mcp_infrastructure_error(&self.server_name, "call", &err),
						is_error: true,
						terminate: false,
						details: None,
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
	agent_dir: Option<&Path>,
	discovered: &[crate::config::DiscoveredMcpServer],
) -> Vec<String> {
	let mut names = std::collections::HashSet::new();
	for s in discovered {
		names.insert(s.name.clone());
	}
	let default_agent_dir = crate::dirs::agent_dir();
	let ad = agent_dir.unwrap_or(&default_agent_dir);
	let candidate_paths = [
		cwd.join(".omp").join("mcp.json"),
		cwd.join(".omp").join(".mcp.json"),
		ad.join("mcp.json"),
		ad.join(".mcp.json"),
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
	let mut res: Vec<String> = names.into_iter().collect();
	res.sort();
	res
}
// mcp_tool_match_score re-exported from pi_agent::mcp

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
				approvals.allow.contains(&format!("mcp:{s}"))
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

	// Explicit scoped server-level allow is always definitive and unambiguous.
	let prompt_name = format!("mcp-server:{server_name}");
	if approvals.allow.contains(&format!("mcp:{server_name}"))
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
pub(crate) fn poll_stdin_readable(timeout_ms: i32) -> bool {
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
pub(crate) fn read_stdin_byte() -> Option<u8> {
	unsafe extern "C" {
		fn read(fd: std::os::raw::c_int, buf: *mut std::os::raw::c_void, count: usize) -> isize;
	}
	let mut b = 0u8;
	let ret = unsafe { read(0, &mut b as *mut u8 as *mut std::os::raw::c_void, 1) };
	if ret == 1 { Some(b) } else { None }
}

/// Discard unread terminal input, including a partial canonical line that the
/// user typed but never submitted. Only a TTY is flushed: headless/piped stdin
/// may belong to the next consumer and must never be drained here.
#[cfg(unix)]
pub(crate) fn flush_tty_stdin() {
	use std::io::IsTerminal;
	if !std::io::stdin().is_terminal() {
		return;
	}
	const TCIFLUSH: std::os::raw::c_int = 0;
	unsafe extern "C" {
		fn tcflush(
			fd: std::os::raw::c_int,
			queue_selector: std::os::raw::c_int,
		) -> std::os::raw::c_int;
	}
	unsafe {
		tcflush(0, TCIFLUSH);
	}
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

#[derive(Debug, Clone)]
pub struct McpSpawnResult {
	pub tools: Vec<McpTool>,
	pub server_errors: Vec<(String, String)>,
	pub all_server_names: Vec<String>,
}

pub async fn filter_and_spawn_mcp_servers_with_paths_detailed(
	discovered: Vec<crate::config::DiscoveredMcpServer>,
	cwd: &Path,
	agent_dir: &Path,
	config: &crate::config::Config,
	approvals: &crate::approve::Approvals,
	cancel_token: &pi_agent::CancellationToken,
) -> Result<McpSpawnResult> {
	let mut all_tools = Vec::new();
	let mut server_errors: Vec<(String, String)> = Vec::new();
	let all_server_names = collect_all_configured_server_names(cwd, Some(agent_dir), &discovered);
	if let Some(requested_tools) = &config.tools {
		// Boundary and typo validation must happen before any ambiguity check or
		// MCP server launch: a selected excluded tool is a platform decision, not
		// a retryable unknown name, and nothing should spawn on a bad selection.
		for name in requested_tools {
			if let Some(reason) = excluded_tool_reason(name) {
				return Err(Error::new(format!(
					"tool `{name}` is excluded by the platform boundary: {reason}"
				)));
			}
		}
		let mut valid: Vec<String> = BUILTIN_TOOLS.iter().map(|(n, _)| (*n).to_string()).collect();
		valid.extend(DELEGATION_TOOLS.iter().map(|name| (*name).to_string()));
		// `checkpoint`/`rewind` mount only when `checkpoint.enabled` supplies a
		// controller, but `--tools` may always name them.
		valid.extend(["checkpoint", "rewind"].map(str::to_string));
		valid.extend(all_server_names.iter().cloned());
		// MCP tool ids are only enumerable after a server responds; a name that
		// matches no builtin, no delegation tool, and no configured server cannot
		// become valid, so reject it before spending a launch handshake.
		let unknown: Vec<&String> = requested_tools
			.iter()
			.filter(|name| {
				!valid.iter().any(|known| known == *name)
					&& !name.starts_with("mcp__")
					&& !all_server_names
						.iter()
						.any(|server| mcp_tool_match_score(server, name) > 0)
			})
			.collect();
		if !unknown.is_empty() {
			let plural = if unknown.len() == 1 { "" } else { "s" };
			return Err(Error::new(format!(
				"Unknown tool{} in --tools: {}. Valid tools: {}.",
				plural,
				unknown.iter().map(|name| name.as_str()).collect::<Vec<_>>().join(", "),
				valid.join(", ")
			)));
		}
	}

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
						approvals.allow.contains(&format!("mcp:{s}"))
							|| approvals.allow.contains(&format!("mcp-server:{s}"))
					})
					.collect();
				if explicitly_allowed.len() != 1 {
					return Err(Error::new(format!(
						"Ambiguous tool \"{}\" in --tools: matches multiple servers ({}). Disambiguate by explicitly allowing the intended server with --allow mcp:<server>.",
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
					let safe_err = format_mcp_infrastructure_error(&server.name, "connect", &err);
					eprintln!("{safe_err}");
					server_errors.push((server.name.clone(), safe_err));
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
					let safe_err = format_mcp_infrastructure_error(&server.name, "list", &err);
					eprintln!("{safe_err}");
					server_errors.push((server.name.clone(), safe_err));
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
		return Ok(McpSpawnResult {
			tools: unique_tools,
			server_errors,
			all_server_names,
		});
	}

	if let Some(requested_tools) = &config.tools {
		let mut known_tools: Vec<String> = BUILTIN_TOOLS.iter().map(|(n, _)| n.to_string()).collect();
		known_tools.extend(DELEGATION_TOOLS.iter().map(|name| (*name).to_string()));
		// `checkpoint`/`rewind` are not default builtins: they mount only when
		// `checkpoint.enabled` supplies a controller, but `--tools` may name them.
		known_tools.extend(["checkpoint", "rewind"].map(str::to_string));
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

	Ok(McpSpawnResult {
		tools: unique_tools,
		server_errors,
		all_server_names,
	})
}

pub async fn filter_and_spawn_mcp_servers_detailed(
	discovered: Vec<crate::config::DiscoveredMcpServer>,
	cwd: &Path,
	config: &crate::config::Config,
	approvals: &crate::approve::Approvals,
	cancel_token: &pi_agent::CancellationToken,
) -> Result<McpSpawnResult> {
	let agent_dir = crate::dirs::agent_dir();
	filter_and_spawn_mcp_servers_with_paths_detailed(discovered, cwd, &agent_dir, config, approvals, cancel_token).await
}

pub async fn filter_and_spawn_mcp_servers(
	discovered: Vec<crate::config::DiscoveredMcpServer>,
	cwd: &Path,
	config: &crate::config::Config,
	approvals: &crate::approve::Approvals,
	cancel_token: &pi_agent::CancellationToken,
) -> Result<Vec<McpTool>> {
	let res = filter_and_spawn_mcp_servers_detailed(discovered, cwd, config, approvals, cancel_token).await?;
	Ok(res.tools)
}

pub async fn load_and_filter_mcp_tools_detailed(
	cwd: &Path,
	config: &crate::config::Config,
	approvals: &crate::approve::Approvals,
	cancel_token: &pi_agent::CancellationToken,
) -> Result<McpSpawnResult> {
	let agent_dir = crate::dirs::agent_dir();
	load_and_filter_mcp_tools_with_paths_detailed(cwd, &agent_dir, config, approvals, cancel_token).await
}

pub async fn load_and_filter_mcp_tools(
	cwd: &Path,
	config: &crate::config::Config,
	approvals: &crate::approve::Approvals,
	cancel_token: &pi_agent::CancellationToken,
) -> Result<Vec<McpTool>> {
	let res = load_and_filter_mcp_tools_detailed(cwd, config, approvals, cancel_token).await?;
	Ok(res.tools)
}

pub async fn load_and_filter_mcp_tools_with_paths_detailed(
	cwd: &Path,
	agent_dir: &Path,
	config: &crate::config::Config,
	approvals: &crate::approve::Approvals,
	cancel_token: &pi_agent::CancellationToken,
) -> Result<McpSpawnResult> {
	let report = crate::config::discover_mcp_servers_with_paths(cwd, agent_dir);
	for warning in &report.warnings {
		eprintln!("{warning}");
	}
	filter_and_spawn_mcp_servers_with_paths_detailed(report.servers, cwd, agent_dir, config, approvals, cancel_token).await
}

pub async fn load_and_filter_mcp_tools_with_paths(
	cwd: &Path,
	agent_dir: &Path,
	config: &crate::config::Config,
	approvals: &crate::approve::Approvals,
	cancel_token: &pi_agent::CancellationToken,
) -> Result<Vec<McpTool>> {
	let res = load_and_filter_mcp_tools_with_paths_detailed(cwd, agent_dir, config, approvals, cancel_token).await?;
	Ok(res.tools)
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

fn convert_content_parts(
	parts: &[crate::session::ContentPart],
	context_label: &str,
) -> Result<Vec<pi_agent::ContentPart>, Error> {
	let mut pi_parts = Vec::new();
	for (index, p) in parts.iter().enumerate() {
		if p.is_image() {
			let Some(data) = p.data.as_deref() else {
				return Err(Error::new(format!(
					"{context_label} contains image part missing data at index {index}"
				)));
			};
			let Some(mime) = p.mime_type.as_deref() else {
				return Err(Error::new(format!(
					"{context_label} contains image part missing mimeType at index {index}"
				)));
			};
			let (clean_b64, norm_mime) = pi_agent::state::normalize_image_payload(data, mime)
				.map_err(|err| Error::new(format!("{context_label} contains invalid image part at index {index}: {err}")))?;
			pi_parts.push(pi_agent::ContentPart::Image {
				data: clean_b64,
				mime_type: norm_mime,
			});
		} else if p.kind == "text" {
			let Some(text) = p.text.clone() else {
				return Err(Error::new(format!(
					"{context_label} contains malformed text part missing text at index {index}"
				)));
			};
			pi_parts.push(pi_agent::ContentPart::Text { text });
		} else {
			return Err(Error::new(format!(
				"{context_label} contains unsupported content part kind at index {index}"
			)));
		}
	}
	Ok(pi_parts)
}

/// Convert the CLI prompt/parts into the same user message the runner appends,
/// so the no-session mirror stays byte-identical to the loop.
pub fn user_message_from_parts(
	prompt: &str,
	parts: &[crate::session::ContentPart],
) -> Result<pi_agent::Message, Error> {
	if parts.is_empty() {
		return Ok(pi_agent::Message::user(prompt));
	}
	Ok(pi_agent::Message::user_with_parts(convert_content_parts(
		parts, "request",
	)?))
}

/// Previous session turns as `pi-agent` history messages.
pub fn history_messages(history: &[crate::session::TurnRecord]) -> Result<Vec<pi_agent::Message>, Error> {
	let mut messages = Vec::new();
	for turn in history {
		if !turn.parts.is_empty() {
			let pi_parts = convert_content_parts(&turn.parts, "session history")?;
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
					let tool_msg = if !call.result_parts.is_empty() {
						let pi_parts = convert_content_parts(&call.result_parts, "tool result history")?;
						pi_agent::Message::tool_with_status_and_parts(
							call.id.clone(),
							name,
							call.result.clone(),
							pi_parts,
							call.is_error,
						)
					} else {
						pi_agent::Message::tool_with_status(
							call.id.clone(),
							name,
							call.result.clone(),
							call.is_error,
						)
					};
					messages.push(tool_msg);
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
			let tool_msg = if !call.result_parts.is_empty() {
				let pi_parts = convert_content_parts(&call.result_parts, "tool result history")?;
				pi_agent::Message::tool_with_status_and_parts(
					call.id.clone(),
					name,
					call.result.clone(),
					pi_parts,
					call.is_error,
				)
			} else {
				pi_agent::Message::tool_with_status(
					call.id.clone(),
					name,
					call.result.clone(),
					call.is_error,
				)
			};
			messages.push(tool_msg);
		}
	}
	Ok(messages)
}

/// Latest todo phases recorded in in-memory turns (`--no-session`), newest turn
/// and call first. Only successful top-level `todo` calls contribute
/// `details.phases`; `{"phases": []}` clears, malformed payloads and error
/// results are skipped. Mirrors [`crate::session::SessionStore::latest_todo_phases`];
/// nested child task results never appear as top-level calls, so they cannot
/// leak into root state.
#[must_use]
pub fn latest_todo_phases_from_turns(
	history: &[crate::session::TurnRecord],
) -> Vec<pi_agent::tools::todo::TodoPhase> {
	for turn in history.iter().rev() {
		for call in turn.tool_calls.iter().rev() {
			if call.name != "todo" || call.is_error {
				continue;
			}
			let Some(phases) = call.details.as_ref().and_then(|details| details.get("phases")) else {
				continue;
			};
			if !phases.is_array() {
				continue;
			}
			if let Ok(phases) =
				serde_json::from_value::<Vec<pi_agent::tools::todo::TodoPhase>>(phases.clone())
			{
				return phases;
			}
		}
	}
	Vec::new()
}

/// Process backend that connects to the daemon broker on first use.
///
/// Broker startup can take seconds (or fail entirely under a unit-test
/// harness); doing it during `run()` setup would eat model-turn budgets even
/// when no process operation is requested. Only trait methods that need a
/// process resolve the backend.
struct LazyProcessBackend {
	project: PathBuf,
	fallback_log_root: PathBuf,
	backend: tokio::sync::OnceCell<Arc<dyn ProcessBackend>>,
	async_enabled: AtomicBool,
	#[cfg(test)]
	test_broker_unavailable: bool,
}

impl LazyProcessBackend {
	fn new(project: PathBuf, fallback_log_root: PathBuf) -> Self {
		Self {
			project,
			fallback_log_root,
			backend: tokio::sync::OnceCell::new(),
			async_enabled: AtomicBool::new(true),
			#[cfg(test)]
			test_broker_unavailable: false,
		}
	}

	#[cfg(test)]
	fn for_test_unavailable_broker(fallback_log_root: PathBuf) -> Self {
		Self {
			test_broker_unavailable: true,
			..Self::new(PathBuf::from("/nonexistent-project"), fallback_log_root)
		}
	}

	async fn resolved(&self) -> std::result::Result<Arc<dyn ProcessBackend>, pi_agent::tools::process::ProcessError> {
		// One async initializer for the whole cell: concurrent first process ops
		// must not each connect/fall back, because the losers' managers would
		// share the fallback log root and delete the winner's logs on drop.
		let backend = self
			.backend
			.get_or_init(|| async {
				#[cfg(test)]
				if self.test_broker_unavailable {
					eprintln!(
						"omp: process broker unavailable (test); using the in-process supervisor for {}",
						self.project.display()
					);
					return Arc::new(ProcessManager::new(self.fallback_log_root.clone()))
						as Arc<dyn ProcessBackend>;
				}
				let backend: Arc<dyn ProcessBackend> =
					match BrokerProcessBackend::connect(&self.project).await {
						Ok(backend) => backend,
						Err(error) => {
							eprintln!(
								"omp: process broker unavailable ({error}); using the in-process supervisor for {}",
								self.project.display()
							);
							Arc::new(ProcessManager::new(self.fallback_log_root.clone()))
						},
					};
				backend
			})
			.await;
		// A concurrent `set_async_enabled` may have changed the flag while the
		// connection was in flight; mirror the latest value once.
		backend.set_async_enabled(self.async_enabled.load(Ordering::SeqCst));
		Ok(Arc::clone(backend))
	}
}

#[async_trait]
impl ProcessBackend for LazyProcessBackend {
	async fn start(
		self: Arc<Self>,
		spec: pi_agent::tools::process::ProcessSpec,
		owner: Option<String>,
	) -> std::result::Result<pi_agent::tools::process::StartOutcome, pi_agent::tools::process::ProcessError> {
		self.resolved().await?.start(spec, owner).await
	}

	async fn list(
		self: Arc<Self>,
		owner: Option<String>,
	) -> std::result::Result<Vec<pi_agent::tools::process::ProcessSnapshot>, pi_agent::tools::process::ProcessError> {
		self.resolved().await?.list(owner).await
	}

	async fn logs(
		self: Arc<Self>,
		name: String,
		options: pi_agent::tools::process::LogOptions,
		owner: Option<String>,
	) -> std::result::Result<pi_agent::tools::process::LogsOutcome, pi_agent::tools::process::ProcessError> {
		self.resolved().await?.logs(name, options, owner).await
	}

	async fn wait(
		self: Arc<Self>,
		name: String,
		request: pi_agent::tools::process::WaitRequest,
		timeout_ms: u64,
		owner: Option<String>,
	) -> std::result::Result<pi_agent::tools::process::WaitOutcome, pi_agent::tools::process::ProcessError> {
		self.resolved().await?.wait(name, request, timeout_ms, owner).await
	}

	async fn send(
		self: Arc<Self>,
		name: String,
		data: Option<String>,
		signal: Option<pi_agent::tools::process::Signal>,
		owner: Option<String>,
	) -> std::result::Result<pi_agent::tools::process::ProcessSnapshot, pi_agent::tools::process::ProcessError> {
		self.resolved().await?.send(name, data, signal, owner).await
	}

	async fn stop(
		self: Arc<Self>,
		name: String,
		timeout_ms: u64,
		owner: Option<String>,
	) -> std::result::Result<pi_agent::tools::process::ProcessSnapshot, pi_agent::tools::process::ProcessError> {
		self.resolved().await?.stop(name, timeout_ms, owner).await
	}

	async fn restart(
		self: Arc<Self>,
		name: String,
		owner: Option<String>,
	) -> std::result::Result<pi_agent::tools::process::ProcessSnapshot, pi_agent::tools::process::ProcessError> {
		self.resolved().await?.restart(name, owner).await
	}

	async fn describe(
		self: Arc<Self>,
		name: String,
		owner: Option<String>,
	) -> std::result::Result<(pi_agent::tools::process::ProcessSnapshot, pi_agent::tools::process::ProcessSpec), pi_agent::tools::process::ProcessError> {
		self.resolved().await?.describe(name, owner).await
	}

	fn async_enabled(&self) -> bool {
		self.async_enabled.load(Ordering::SeqCst)
	}

	fn set_async_enabled(&self, enabled: bool) {
		self.async_enabled.store(enabled, Ordering::SeqCst);
		if let Some(backend) = self.backend.get() {
			backend.set_async_enabled(enabled);
		}
	}

	fn next_background_name(&self) -> String {
		// Process-global sequence: two lazy backends for aliased project paths
		// (same broker) must never mint the same job name.
		static NAME_SEQ: AtomicU64 = AtomicU64::new(0);
		format!(
			"bash-{}-{}",
			std::process::id(),
			NAME_SEQ.fetch_add(1, Ordering::SeqCst)
		)
	}

	fn arm_timeout(self: Arc<Self>, name: String, owner: Option<String>, timeout_ms: u64) {
		tokio::spawn(async move {
			if let Ok(backend) = self.resolved().await {
				backend.arm_timeout(name, owner, timeout_ms);
			}
		});
	}
}

/// Project-scoped log root for the in-process fallback supervisor.
fn process_scope_root(project: &Path, runner_ns: &str) -> PathBuf {
	use std::hash::{Hash, Hasher};
	let mut hasher = std::collections::hash_map::DefaultHasher::new();
	project.hash(&mut hasher);
	runner_ns.hash(&mut hasher);
	crate::dirs::agent_dir()
		.join("run")
		.join("processes")
		.join(format!("{:016x}", hasher.finish()))
}

#[cfg(test)]
mod tests {
	use std::{
		collections::BTreeMap,
		path::PathBuf,
		sync::atomic::{AtomicUsize, Ordering},
	};

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
			..ModelInfo::default()
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
			parts: Vec::new(),
			cwd: PathBuf::from("/work"),
			output_mode: OutputMode::Text,
			model: Some("local/llama3".to_string()),
			provider: None,
			api_key: None,
			provider_session_id: None,
			prompt_cache_key: None,
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
			history_messages: None,
			checkpoint_controller: None,
			turn_ack: None,
			todo_phases: Vec::new(),
			session_persisted: false,
			session_file: None,
			session_id: None,
			cancellation_token: None,
			extra_tools: Vec::new(),
			internal_urls: None,
			url_scope: None,
			subagent_observer: None,
			child_session_root: None,
			credential_resolver: None,
		}
	}

	fn yolo() -> Approvals {
		Approvals::new(ApprovalMode::Yolo, Vec::new())
	}

	#[test]
	fn excluded_tool_in_tools_selection_reports_the_boundary() {
		let mut request = request(yolo());
		request.config.tools = Some(vec!["read".to_string(), "computer".to_string()]);
		let error = allowed_tools(&request, false).expect_err("computer is excluded");
		let message = error.to_string();
		assert!(message.contains("`computer`"), "{message}");
		assert!(message.contains("excluded by the platform boundary"), "{message}");
		assert!(message.contains("no desktop backend"), "{message}");

		// A supported selection still resolves, sorted and ask-filtered.
		request.config.tools = Some(vec!["read".to_string(), "grep".to_string()]);
		let tools = allowed_tools(&request, false).expect("selection");
		assert_eq!(tools, ["grep", "read"]);
	}

	#[test]
	fn lsp_and_debug_follow_their_enable_flags() {
		let mut request = request(yolo());
		let tools = allowed_tools(&request, false).expect("defaults");
		assert!(tools.contains(&"lsp"), "lsp.enabled defaults to true");
		assert!(tools.contains(&"debug"), "debug.enabled defaults to true");

		request.config.lsp.enabled = Some(false);
		request.config.debug.enabled = Some(false);
		let tools = allowed_tools(&request, false).expect("disabled");
		assert!(!tools.contains(&"lsp"));
		assert!(!tools.contains(&"debug"));

		// A selected tool that the settings disable is an explicit conflict, not
		// a silently empty selection.
		request.config.tools = Some(vec!["lsp".to_string()]);
		let error = allowed_tools(&request, false).expect_err("lsp is disabled");
		assert!(error.to_string().contains("lsp.enabled"), "{error}");
		request.config.tools = Some(vec!["debug".to_string()]);
		let error = allowed_tools(&request, false).expect_err("debug is disabled");
		assert!(error.to_string().contains("debug.enabled"), "{error}");
	}

	#[test]
	fn plan_maps_request_onto_provider_config() {
		let plan = RunPlan::resolve(&request(yolo())).expect("plan");
		assert_eq!(plan.base_url, "http://127.0.0.1:8080/v1");
		assert_eq!(plan.api_key, "sk-test");
		assert_eq!(plan.model, "llama3");
		assert_eq!(plan.max_turns, 5);
		assert_eq!(plan.context_limit, 8_192);
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
	fn child_model_roles_follow_ts_priority_inheritance_and_suffixes() {
		let factory = |roles: &[(&str, &str)], default: Option<&str>| {
			let mut config = Config::default();
			for (role, value) in roles {
				config
					.model_roles
					.insert((*role).to_string(), (*value).to_string());
			}
			CliChildModelFactory {
				config,
				catalog: Arc::new(catalog()),
				explicit_provider: None,
				api_key: None,
				default_model: default.map(str::to_string),
				root_provider: None,
				root_base_url: None,
				credential_resolver: None,
				session_id: None,
				cancel: None,
			}
		};

		let priorities = factory(&[], None);
		let smol = priorities.patterns_for("@smol").expect("smol priority");
		assert_eq!(smol.first().map(String::as_str), Some("cerebras/zai-glm-4.7"));
		assert_eq!(smol.last().map(String::as_str), Some("mini"));
		assert_eq!(
			priorities.patterns_for("@advisor").expect("advisor alias")[0],
			"openai-codex/gpt-5.5"
		);

		let suffixed = priorities.patterns_for("@smol:hi").expect("suffix");
		assert!(suffixed.iter().all(|pattern| pattern.ends_with(":high")));
		let default_suffix = factory(&[("default", "local/llama3")], None);
		assert_eq!(
			default_suffix.patterns_for("*:auto").expect("auto suffix"),
			vec!["local/llama3:auto".to_string()]
		);

		let self_alias = factory(&[("default", "@smol")], None);
		assert_eq!(
			self_alias.patterns_for("@smol").expect("self alias")[0],
			"cerebras/zai-glm-4.7"
		);

		let inherited = factory(&[("default", "local/llama3")], None);
		assert_eq!(
			inherited.patterns_for("@slow").expect("inherit default"),
			vec!["local/llama3".to_string()]
		);

		// Roles with no configured/priority chain fall back to the active model.
		let active = factory(&[], Some("local/llama3"));
		assert_eq!(
			active.patterns_for("@task").expect("active fallback"),
			vec!["local/llama3".to_string()]
		);

		let configured = factory(
			&[("slow", "local/other, local/llama3"), ("task", "@slow")],
			Some("local/llama3"),
		);
		assert_eq!(
			configured.patterns_for("@task").expect("cross alias"),
			vec!["local/other".to_string(), "local/llama3".to_string()]
		);
	}

	#[test]
	fn child_targets_never_reuse_root_key_for_other_providers() {
		let mut config = Config::default();
		config.providers.insert(
			"rootprov".to_string(),
			ProviderConfig {
				base_url: Some("http://127.0.0.1:1".to_string()),
				api_key: Some("root-conn-key".to_string()),
				..ProviderConfig::default()
			},
		);
		config.providers.insert(
			"otherprov".to_string(),
			ProviderConfig {
				base_url: Some("http://127.0.0.1:2".to_string()),
				api_key: Some("other-key".to_string()),
				..ProviderConfig::default()
			},
		);
		config.providers.insert(
			"nokey".to_string(),
			ProviderConfig {
				base_url: Some("http://127.0.0.1:3".to_string()),
				api_key: None,
				..ProviderConfig::default()
			},
		);
		let factory = CliChildModelFactory {
			config,
			catalog: Arc::new(catalog()),
			explicit_provider: None,
			api_key: Some("root-session-key".to_string()),
			default_model: None,
			root_provider: Some("rootprov".to_string()),
			root_base_url: Some("http://127.0.0.1:1".to_string()),
			credential_resolver: None,
			session_id: None,
			cancel: None,
		};

		let (_, _, _, key, _, _) = factory.target("otherprov/model").expect("other provider");
		assert_eq!(key, "other-key", "root key must not leak to a foreign provider");
		let (_, _, _, key, _, _) = factory.target("nokey/model").expect("keyless provider");
		assert_eq!(key, "", "root key must not be sent to a keyless foreign provider");
		let (_, _, _, key, _, _) = factory.target("rootprov/model").expect("root provider");
		assert_eq!(key, "root-session-key");
	}

	#[test]
	fn child_override_uses_child_provider_store_key_and_fails_closed_on_cancel() {
		use crate::{
			agent::ResolverHandle,
			auth::{error::AuthError, http::Cancel},
			rpc::auth::CredentialResolver,
		};

		/// Records every provider the factory asks the shared store about.
		struct RecordingResolver {
			keys:    std::collections::HashMap<&'static str, &'static str>,
			queries: Mutex<Vec<String>>,
		}

		impl CredentialResolver for RecordingResolver {
			fn api_key(
				&self,
				provider: &str,
				_session_id: Option<&str>,
				cancel: &Cancel,
			) -> std::result::Result<Option<String>, AuthError> {
				self.queries.lock().expect("lock").push(provider.to_string());
				cancel.check(provider)?;
				Ok(self.keys.get(provider).map(|key| (*key).to_string()))
			}

			fn peek_api_key(&self, _provider: &str) -> std::result::Result<Option<String>, AuthError> {
				Ok(None)
			}
		}

		let mut config = Config::default();
		config.providers.insert(
			"rootprov".to_string(),
			ProviderConfig {
				base_url: Some("http://127.0.0.1:1".to_string()),
				api_key: None,
				..ProviderConfig::default()
			},
		);
		config.providers.insert(
			"childprov".to_string(),
			ProviderConfig {
				base_url: Some("http://127.0.0.1:2".to_string()),
				// No connection key: the store is the only source for this child.
				api_key: None,
				..ProviderConfig::default()
			},
		);
		let resolver = Arc::new(RecordingResolver {
			keys: std::collections::HashMap::from([
				("rootprov", "root-store-key"),
				("childprov", "child-store-key"),
			]),
			queries: Mutex::new(Vec::new()),
		});
		let factory = CliChildModelFactory {
			config,
			catalog: Arc::new(catalog()),
			explicit_provider: None,
			api_key: Some("root-store-key".to_string()),
			default_model: None,
			root_provider: Some("rootprov".to_string()),
			root_base_url: Some("http://127.0.0.1:1".to_string()),
			credential_resolver: Some(ResolverHandle(
				resolver.clone() as Arc<dyn CredentialResolver>,
			)),
			session_id: Some("session-1".to_string()),
			cancel: None,
		};

		// Same provider+base URL: reuse the root key, never touch the store.
		let (_, _, _, key, _, _) = factory.target("rootprov/model").expect("root target");
		assert_eq!(key, "root-store-key");
		assert!(resolver.queries.lock().expect("lock").is_empty());

		// Override to another provider: that provider's own store credential,
		// and the root secret is not forwarded.
		let (_, _, _, key, _, _) = factory.target("childprov/model").expect("child target");
		assert_eq!(key, "child-store-key");
		assert_eq!(*resolver.queries.lock().expect("lock"), vec!["childprov".to_string()]);

		// A cancelled turn fails closed before returning any key.
		let token = pi_agent::CancellationToken::new();
		token.cancel();
		let cancelled = CliChildModelFactory {
			config: factory.config.clone(),
			catalog: Arc::clone(&factory.catalog),
			explicit_provider: None,
			api_key: Some("root-store-key".to_string()),
			default_model: None,
			root_provider: Some("rootprov".to_string()),
			root_base_url: Some("http://127.0.0.1:1".to_string()),
			credential_resolver: Some(ResolverHandle(
				resolver.clone() as Arc<dyn CredentialResolver>,
			)),
			session_id: Some("session-1".to_string()),
			cancel: Some(token),
		};
		let error = cancelled.target("childprov/model").expect_err("cancelled lookup");
		assert!(error.contains("credential lookup failed for provider `childprov`"), "{error}");
	}

	#[test]
	fn always_ask_respects_explicit_tool_selection() {
		let mut req = request(Approvals::new(ApprovalMode::AlwaysAsk, Vec::new()));
		req.config.tools = Some(vec!["bash".to_string()]);
		let plan = RunPlan::resolve(&req).expect("plan");
		assert_eq!(plan.tools, ["bash"]);
	}

	#[test]
	fn ask_tool_is_offered_only_with_an_interactive_channel() {
		let mut request = request(yolo());
		assert!(!RunPlan::resolve(&request).expect("headless").tools.contains(&"ask"));

		request
			.approvals
			.set_ask(Some(crate::ask::terminal_channel(None)));
		let plan = RunPlan::resolve(&request).expect("interactive");
		assert!(plan.tools.contains(&"ask"), "{:?}", plan.tools);

		request.config.ask.enabled = Some(false);
		let plan = RunPlan::resolve(&request).expect("disabled");
		assert!(!plan.tools.contains(&"ask"));

		request.config.tools = Some(vec!["ask".to_string()]);
		let err = match RunPlan::resolve(&request) {
			Ok(_) => panic!("explicit headless ask must fail"),
			Err(err) => err,
		};
		assert!(err.to_string().contains("requires interactive"));
	}

	#[test]
	fn no_tools_disables_the_registry() {
		let mut request = request(yolo());
		request.tools_enabled = false;
		let plan = RunPlan::resolve(&request).expect("plan");
		assert!(plan.tools.is_empty());
	}

	#[test]
	fn delegation_defaults_on_and_follows_the_tools_whitelist() {
		let mut request = request(yolo());
		assert_eq!(delegation_selection(&request), (true, true));

		request.config.tools = Some(vec!["read".to_string()]);
		assert_eq!(delegation_selection(&request), (false, false));

		request.config.tools = Some(vec!["task".to_string()]);
		assert_eq!(delegation_selection(&request), (true, false));

		request.config.tools = Some(vec!["hub".to_string()]);
		assert_eq!(delegation_selection(&request), (false, true));

		request.tools_enabled = false;
		assert_eq!(delegation_selection(&request), (false, false));
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
				result_parts: vec![],
				details: None,
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
				result_parts: vec![],
				details: None,
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
				result_parts: vec![],
				details: None,
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
				result_parts: vec![],
				details: None,
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
	fn history_messages_preserves_tool_result_image_parts() {
		let history = vec![crate::session::TurnRecord {
			prompt:      "inspect camera".to_string(),
			assistant:   "taking photo".to_string(),
			stop_reason: "stop".to_string(),
			tool_calls:  vec![crate::session::ToolCallRecord {
				id:           "call-cam-1".to_string(),
				name:         "camera".to_string(),
				arguments:    serde_json::json!({}),
				result:       "snapshot ready".to_string(),
				is_error:     false,
				result_parts: vec![
					crate::session::ContentPart::text("snapshot ready"),
					crate::session::ContentPart::image("aW1hZ2VkYXRh", "image/png"),
				],
				details:      None,
			}],
			parts:       vec![],
		}];
		let messages = history_messages(&history).expect("history messages");
		assert_eq!(messages.len(), 3);
		assert_eq!(messages[2].role, pi_agent::Role::Tool);
		assert_eq!(messages[2].content, "snapshot ready");
		assert_eq!(messages[2].parts.len(), 2);
		assert!(matches!(messages[2].parts[0], pi_agent::ContentPart::Text { .. }));
		assert!(matches!(messages[2].parts[1], pi_agent::ContentPart::Image { .. }));
	}

	#[test]
	fn history_messages_rejects_malformed_tool_result_image_part() {
		let history = vec![crate::session::TurnRecord {
			prompt:      "inspect camera".to_string(),
			assistant:   "taking photo".to_string(),
			stop_reason: "stop".to_string(),
			tool_calls:  vec![crate::session::ToolCallRecord {
				id:           "call-cam-1".to_string(),
				name:         "camera".to_string(),
				arguments:    serde_json::json!({}),
				result:       "snapshot ready".to_string(),
				is_error:     false,
				result_parts: vec![crate::session::ContentPart {
					kind:      "image".to_string(),
					text:      None,
					data:      Some("https://remote.server/bad.png".to_string()),
					mime_type: Some("image/png".to_string()),
					extra:     serde_json::Map::new(),
				}],
				details:      None,
			}],
			parts:       vec![],
		}];
		let err = history_messages(&history).unwrap_err();
		assert!(
			err.to_string().contains("external URLs are not permitted"),
			"must reject external URL in tool result history, got: {err}"
		);
	}

	#[test]
	fn history_messages_rejects_unsupported_tool_result_part_kind() {
		let history = vec![crate::session::TurnRecord {
			prompt:      "inspect camera".to_string(),
			assistant:   "taking photo".to_string(),
			stop_reason: "stop".to_string(),
			tool_calls:  vec![crate::session::ToolCallRecord {
				id:           "call-cam-1".to_string(),
				name:         "camera".to_string(),
				arguments:    serde_json::json!({}),
				result:       "snapshot ready".to_string(),
				is_error:     false,
				result_parts: vec![crate::session::ContentPart {
					kind:      "audio".to_string(),
					text:      None,
					data:      None,
					mime_type: None,
					extra:     serde_json::Map::new(),
				}],
				details:      None,
			}],
			parts:       vec![],
		}];
		let err = history_messages(&history).unwrap_err();
		assert!(
			err.to_string().contains("unsupported content part kind at index 0"),
			"must reject unsupported part kind in tool result history, got: {err}"
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
			..ModelInfo::default()
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
		let err = provider
			.complete(&[msg], &[], &pi_agent::ToolChoice::Auto)
			.await
			.unwrap_err();
		assert!(
			matches!(err, pi_agent::ProviderError::UnsupportedImage(_)),
			"provider must reject image message when model is text-only, got: {:?}",
			err
		);
	}

	#[test]
	fn run_plan_honors_per_model_usage_in_streaming_compat() {
		let mut cat = catalog_with_connection("cerebras", "http://127.0.0.1:8080/v1", Some("sk-test"));
		cat.providers.insert(
			"cerebras".to_string(),
			BTreeMap::from([("llama".to_string(), ModelInfo {
				provider: "cerebras".to_string(),
				supports_usage_in_streaming: Some(true),
				..ModelInfo::default()
			})]),
		);
		let mut req = request(Approvals::default());
		req.model = Some("cerebras/llama".to_string());
		req.catalog = Arc::new(cat);
		let plan = RunPlan::resolve(&req).expect("plan");
		// Per-model override beats the Cerebras provider default (`!isCerebras`).
		assert!(plan.supports_usage_in_streaming);

		let mut cat = catalog_with_connection("venice", "http://127.0.0.1:8080/v1", Some("sk-test"));
		cat.providers.insert(
			"venice".to_string(),
			BTreeMap::from([("v1".to_string(), ModelInfo {
				provider: "venice".to_string(),
				supports_usage_in_streaming: Some(false),
				..ModelInfo::default()
			})]),
		);
		req.model = Some("venice/v1".to_string());
		req.catalog = Arc::new(cat);
		let plan = RunPlan::resolve(&req).expect("plan");
		assert!(!plan.supports_usage_in_streaming);
	}

	#[test]
	fn run_plan_enables_images_for_multimodal_catalog_model() {
		let mut cat = catalog_with_connection("local", "http://127.0.0.1:8080/v1", Some("sk-test"));
		let mut models = BTreeMap::new();
		models.insert("vision-model".to_string(), ModelInfo {
			provider:       "local".to_string(),
			context_window: Some(4096),
			input:          Some(vec!["text".to_string(), "image".to_string()]),
			..ModelInfo::default()
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
			..ModelInfo::default()
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

	#[test]
	fn runner_rejects_image_request_on_text_only_model() {
		let mut cat = catalog_with_connection("local", "http://127.0.0.1:8080/v1", Some("sk-test"));
		let mut models = BTreeMap::new();
		models.insert("text-only".to_string(), ModelInfo {
			provider: "local".to_string(),
			context_window: Some(4096),
			input: Some(vec!["text".to_string()]),
			..ModelInfo::default()
		});
		cat.providers.insert("local".to_string(), models);

		let mut req = request(Approvals::default());
		req.catalog = Arc::new(cat);
		req.model = Some("local/text-only".to_string());
		req.parts = vec![
			crate::session::ContentPart::text("describe"),
			crate::session::ContentPart::image("aW1hZ2VkYXRh", "image/png"),
		];

		let runner = PiAgentRunner::new().expect("runner");
		let err = match runner.run(&req) {
			Err(e) => e,
			Ok(_) => panic!("expected runner to reject image on text-only model"),
		};
		assert!(
			err.to_string().contains("does not support image input"),
			"must fail explicitly for text-only model with images: {err}"
		);
	}

	#[test]
	fn runner_transfers_ordered_image_and_text_to_wire() {
		// Every fixture read is bounded: headers stop at 64 KiB, the request body
		// at 1 MiB, and each socket read has a timeout, so a truncated or stalled
		// request fails the test with a diagnostic instead of silently dropping it.
		const WIRE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

		let (addr_tx, addr_rx) = std::sync::mpsc::channel::<std::net::SocketAddr>();
		let (wire_tx, wire_rx) =
			std::sync::mpsc::channel::<std::result::Result<serde_json::Value, String>>();

		std::thread::spawn(move || {
			fn serve(
				addr_tx: &std::sync::mpsc::Sender<std::net::SocketAddr>,
			) -> std::result::Result<serde_json::Value, String> {
				use std::io::{Read, Write};

				const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
				const MAX_HEADER_BYTES: usize = 64 * 1024;
				const MAX_BODY_BYTES: usize = 1024 * 1024;

				let listener = std::net::TcpListener::bind("127.0.0.1:0")
					.map_err(|err| format!("fixture bind failed: {err}"))?;
				let addr = listener
					.local_addr()
					.map_err(|err| format!("fixture local_addr failed: {err}"))?;
				addr_tx
					.send(addr)
					.map_err(|_| "fixture address receiver dropped".to_string())?;
				let (mut stream, _) = listener
					.accept()
					.map_err(|err| format!("fixture accept failed: {err}"))?;
				stream
					.set_read_timeout(Some(READ_TIMEOUT))
					.map_err(|err| format!("fixture read timeout setup failed: {err}"))?;

				let mut received: Vec<u8> = Vec::new();
				let mut chunk = [0u8; 8192];
				let header_end = loop {
					if let Some(pos) = received.windows(4).position(|w| w == b"\r\n\r\n") {
						break pos;
					}
					if received.len() > MAX_HEADER_BYTES {
						return Err(format!(
							"fixture request headers exceeded {MAX_HEADER_BYTES} bytes"
						));
					}
					let read = stream.read(&mut chunk).map_err(|err| {
						format!("fixture reading request headers failed: {err}")
					})?;
					if read == 0 {
						return Err("fixture connection closed before request headers completed"
							.to_string());
					}
					received.extend_from_slice(&chunk[..read]);
				};

				let header = String::from_utf8_lossy(&received[..header_end]).into_owned();
				let mut content_len: Option<usize> = None;
				for line in header.lines() {
					let Some((name, value)) = line.split_once(':') else {
						continue;
					};
					if name.trim().eq_ignore_ascii_case("content-length") {
						content_len = Some(value.trim().parse().map_err(|err| {
							format!("fixture invalid Content-Length {value:?}: {err}")
						})?);
					}
				}
				let content_len = content_len.ok_or_else(|| {
					format!("fixture request missing Content-Length header: {header:?}")
				})?;
				if content_len > MAX_BODY_BYTES {
					return Err(format!(
						"fixture request body of {content_len} bytes exceeds the {MAX_BODY_BYTES}-byte fixture cap"
					));
				}

				let body_start = header_end + 4;
				while received.len() < body_start + content_len {
					let read = stream
						.read(&mut chunk)
						.map_err(|err| format!("fixture reading request body failed: {err}"))?;
					if read == 0 {
						return Err(format!(
							"fixture connection closed after {} of {content_len} request body bytes",
							received.len().saturating_sub(body_start)
						));
					}
					received.extend_from_slice(&chunk[..read]);
				}

				let body = &received[body_start..body_start + content_len];
				let wire_json = serde_json::from_slice(body)
					.map_err(|err| format!("fixture request body is not valid JSON: {err}"))?;

				let resp = serde_json::json!({
					"id": "cmpl-img",
					"object": "chat.completion.chunk",
					"created": 1,
					"model": "vision-model",
					"choices": [{
						"index": 0,
						"delta": { "role": "assistant", "content": "I see the chart" },
						"finish_reason": null
					}]
				}).to_string();
				let http_resp = format!(
					"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {resp}\n\ndata: [DONE]\n\n"
				);
				stream
					.write_all(http_resp.as_bytes())
					.map_err(|err| format!("fixture writing response failed: {err}"))?;
				stream
					.flush()
					.map_err(|err| format!("fixture flushing response failed: {err}"))?;
				Ok(wire_json)
			}

			let _ = wire_tx.send(serve(&addr_tx));
		});

		let addr = addr_rx.recv().expect("recv addr");
		let url = format!("http://{addr}/v1");

		let mut cat = catalog_with_connection("local", &url, Some("sk-test"));
		let mut models = BTreeMap::new();
		models.insert("vision-model".to_string(), ModelInfo {
			provider: "local".to_string(),
			context_window: Some(4096),
			input: Some(vec!["text".to_string(), "image".to_string()]),
			..ModelInfo::default()
		});
		cat.providers.insert("local".to_string(), models);

		let mut req = request(Approvals::default());
		let mut config = Config::default();
		config.providers.insert("local".to_string(), ProviderConfig {
			base_url: Some(url.clone()),
			api_key: Some("sk-test".to_string()),
			..ProviderConfig::default()
		});
		req.config = config;
		req.catalog = Arc::new(cat);
		req.model = Some("local/vision-model".to_string());
		req.parts = vec![
			crate::session::ContentPart::text("describe this"),
			crate::session::ContentPart::image("aW1hZ2VkYXRh", "image/png"),
		];

		let runner = PiAgentRunner::new().expect("runner");
		let stream = runner.run(&req).expect("run runner");
		let events: Vec<std::result::Result<Event, Error>> = stream.collect();
		let stream_errors: Vec<String> = events
			.iter()
			.filter_map(|event| event.as_ref().err().map(ToString::to_string))
			.collect();
		let fixture_result = match wire_rx.recv_timeout(WIRE_TIMEOUT) {
			Ok(result) => result,
			Err(err) => panic!("wire fixture received no request: {err}"),
		};
		assert!(
			stream_errors.is_empty(),
			"runner stream reported errors before the wire payload was checked: {stream_errors:?}; fixture: {fixture_result:?}"
		);
		let wire_json = fixture_result.unwrap_or_else(|err| panic!("wire fixture failed: {err}"));
		let messages = wire_json["messages"].as_array().expect("messages");
		let user_msg = messages
			.iter()
			.find(|m| m["role"] == "user")
			.expect("user message in wire payload");
		let content = user_msg["content"].as_array().expect("content parts");
		assert_eq!(content.len(), 2);
		assert_eq!(content[0]["type"], "text");
		assert_eq!(content[0]["text"], "describe this");
		assert_eq!(content[1]["type"], "image_url");
		assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,aW1hZ2VkYXRh");
	}

	#[test]
	fn repl_turns_share_read_edit_provenance_but_keep_children_fresh() {
		let dir = std::env::temp_dir().join(format!(
			"omp-cli-runtime-session-tools-{}",
			crate::output::session_id()
		));
		std::fs::create_dir_all(&dir).expect("mkdir");
		let ctx = pi_agent::AgentContext::new(dir.clone());
		let runner = PiAgentRunner::new().expect("runner");
		let plain = EditToolOptions { enforce_seen_lines: false };
		let build = |session: &Arc<RunnerSession>, options: EditToolOptions| {
			build_registry_full(
				&["read", "edit"],
				Vec::new(),
				Vec::new(),
				Vec::new(),
				Vec::new(),
				false,
				options,
				Some(session),
			)
			.expect("registry")
		};
		async fn execute(
			registry: &ToolRegistry,
			tool: &str,
			args: serde_json::Value,
			ctx: &pi_agent::AgentContext,
		) -> pi_agent::ToolResult {
			let tool = registry.get(tool).expect("tool");
			tool.execute(&args, ctx).await.expect("tool executes")
		}

		// Drive real tool calls on the runner's runtime; the runner (and its
		// tokio runtime) must drop outside async context.
		runner.runtime.block_on(async {
		// Turn 1: read mints the tag, `CUT` parks line 2 in a named register.
		std::fs::write(dir.join("note.txt"), "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n").expect("write");
		let session = runner.session_tools(plain, EditMode::Hashline);
		let turn1 = build(&session, plain);
		let read = execute(&turn1, "read", serde_json::json!({ "path": "note.txt" }), &ctx).await;
		assert!(!read.is_error, "{}", read.content);
		let header = read.content.lines().next().expect("header").to_string();
		let cut = execute(&turn1, "edit", serde_json::json!({
			"input": format!("{header}\nCUT 2.=2 @saved\n"),
		}), &ctx)
		.await;
		assert!(!cut.is_error, "{}", cut.content);
		assert_eq!(
			std::fs::read_to_string(dir.join("note.txt")).expect("read"),
			"a\nc\nd\ne\nf\ng\nh\ni\nj\n"
		);

		// Turn 2: the register from turn 1 still pastes without a new `CUT`.
		let turn2 = build(&session, plain);
		let read = execute(&turn2, "read", serde_json::json!({ "path": "note.txt" }), &ctx).await;
		let header = read.content.lines().next().expect("header").to_string();
		let paste = execute(&turn2, "edit", serde_json::json!({
			"input": format!("{header}\nPUT >$ @saved\n"),
		}), &ctx)
		.await;
		assert!(!paste.is_error, "{}", paste.content);
		assert_eq!(
			std::fs::read_to_string(dir.join("note.txt")).expect("read"),
			"a\nc\nd\ne\nf\ng\nh\ni\nj\nb\n"
		);

		// Turn 3: the tag minted before an external prepend still recovers.
		let turn3 = build(&session, plain);
		let read = execute(&turn3, "read", serde_json::json!({ "path": "note.txt" }), &ctx).await;
		let drift_header = read.content.lines().next().expect("header").to_string();
		let before_drift = std::fs::read_to_string(dir.join("note.txt")).expect("read");
		std::fs::write(dir.join("note.txt"), format!("zero\n{before_drift}")).expect("drift");
		let recovered = execute(&turn3, "edit", serde_json::json!({
			"input": format!("{drift_header}\nPUT 2.=2:\n+A\n"),
		}), &ctx)
		.await;
		assert!(!recovered.is_error, "{}", recovered.content);
		assert_eq!(
			std::fs::read_to_string(dir.join("note.txt")).expect("read"),
			"zero\na\nA\nd\ne\nf\ng\nh\ni\nj\nb\n"
		);

		// Turn 4: opt-in seen lines reject an anchor this turn never displayed.
		std::fs::write(dir.join("other.txt"), "o1\no2\no3\no4\no5\no6\no7\no8\no9\no10\n")
			.expect("write");
		let guarded = EditToolOptions { enforce_seen_lines: true };
		let session = runner.session_tools(guarded, EditMode::Hashline);
		let turn4 = build(&session, guarded);
		let read = execute(&turn4, "read", serde_json::json!({ "path": "other.txt:1-4" }), &ctx).await;
		let header = read.content.lines().next().expect("header").to_string();
		let rejected = execute(&turn4, "edit", serde_json::json!({
			"input": format!("{header}\nPUT 9.=9:\n+X\n"),
		}), &ctx)
		.await;
		assert!(rejected.is_error, "{}", rejected.content);
		assert_eq!(
			std::fs::read_to_string(dir.join("other.txt")).expect("read"),
			"o1\no2\no3\no4\no5\no6\no7\no8\no9\no10\n"
		);
		let read = execute(&turn4, "read", serde_json::json!({ "path": "other.txt:9" }), &ctx).await;
		let header = read.content.lines().next().expect("header").to_string();
		let landed = execute(&turn4, "edit", serde_json::json!({
			"input": format!("{header}\nPUT 9.=9:\n+X\n"),
		}), &ctx)
		.await;
		assert!(!landed.is_error, "{}", landed.content);
		assert_eq!(
			std::fs::read_to_string(dir.join("other.txt")).expect("read"),
			"o1\no2\no3\no4\no5\no6\no7\no8\nX\no10\n"
		);

		// A child registry never inherits this provenance.
		let provider = OpenAiCompatibleProvider::new(
			"http://127.0.0.1:9/v1",
			"sk-test",
			"test-model",
		)
		.expect("provider");
		let supervisor =
			Supervisor::new(turn4, Arc::new(provider), None, DelegationLimits::default());
		let child = supervisor.child_registry(1, Some("peer"));
		let child_edit = child.get("edit").expect("child edit");
		let stolen = child_edit
			.execute(&serde_json::json!({
				"input": format!("{header}\nPUT 9.=9:\n+Y\n"),
			}), &ctx)
			.await
			.expect("child edit executes");
		assert!(stolen.is_error, "{}", stolen.content);
		assert_eq!(
			std::fs::read_to_string(dir.join("other.txt")).expect("read"),
			"o1\no2\no3\no4\no5\no6\no7\no8\nX\no10\n"
		);
		});
	}

	struct Stage {
		path:  PathBuf,
		calls: Arc<AtomicUsize>,
	}

	#[async_trait::async_trait]
	impl pi_agent::PendingAction for Stage {
		fn label(&self) -> &str {
			"repl pending edit"
		}

		fn source_tool(&self) -> &str {
			"ast_edit"
		}

		async fn apply(
			&self,
			_ctx: &pi_agent::AgentContext,
			reason: &str,
		) -> Result<pi_agent::ToolResult, pi_agent::ToolError> {
			self.calls.fetch_add(1, Ordering::SeqCst);
			std::fs::write(&self.path, reason)
				.map_err(|err| pi_agent::ToolError::ExecutionFailed(err.to_string()))?;
			Ok(pi_agent::ToolResult {
				tool_call_id: String::new(),
				content:      "applied".to_string(),
				is_error:     false,
				terminate:    false,
				details:      None,
			})
		}
	}

	#[test]
	fn repl_turns_share_the_pending_store_across_registry_rebuilds() {
		let dir = std::env::temp_dir().join(format!(
			"omp-cli-runtime-pending-{}",
			crate::output::session_id()
		));
		std::fs::create_dir_all(&dir).expect("mkdir");
		let target = dir.join("out.txt");
		let calls = Arc::new(AtomicUsize::new(0));
		let runner = PiAgentRunner::new().expect("runner");
		let options = EditToolOptions::default();
		let build = |session: Option<&Arc<RunnerSession>>| {
			build_registry_full(
				&["write"],
				Vec::new(),
				Vec::new(),
				Vec::new(),
				Vec::new(),
				false,
				options,
				session,
			)
			.expect("registry")
		};
		let ctx = pi_agent::AgentContext::new(dir.clone());

		runner.runtime.block_on(async {
			// Prompt 1 stages the action under the session store; prompt 2 runs
			// against a rebuilt registry and must still see it.
			let session = runner.session_tools(options, EditMode::Hashline);
			let _turn1 = build(Some(&session));
			session.pending.register(Arc::new(Stage { path: target.clone(), calls: calls.clone() }));
			let turn2 = build(Some(&session));

			// The session policy follows the rebuilt registry: same queue head,
			// so the loop can remind/force without recreating state per prompt.
			let head = session.pending.peek_head().expect("staged head");
			let policy = turn2.policy().expect("session policy mounted");
			let requirement = pi_agent::state::ToolPolicy::requirement(policy.as_ref())
				.expect("requirement");
			assert_eq!(
				(requirement.id, requirement.generation),
				(head.id, head.generation)
			);
			assert_eq!(requirement.tool_name, "write");

			let applied = turn2
				.get("write")
				.expect("write")
				.execute(
					&serde_json::json!({"path": "xd://resolve", "content": "apply it"}),
					&ctx,
				)
				.await
				.expect("apply");
			assert!(!applied.is_error, "{}", applied.content);
			assert_eq!(std::fs::read_to_string(&target).expect("read"), "apply it");
			assert_eq!(calls.load(Ordering::SeqCst), 1);
			assert!(session.pending.is_empty());

			// A registry built without the session owns a separate queue.
			let standalone = build(None);
			let rejected = standalone
				.get("write")
				.expect("write")
				.execute(&serde_json::json!({"path": "xd://reject", "content": "none"}), &ctx)
				.await
				.expect("reject");
			assert!(!rejected.is_error);
			assert!(rejected.content.contains("Nothing to reject"));
		});

		std::fs::remove_dir_all(&dir).ok();
	}

	#[tokio::test]
	async fn restored_todo_tool_reads_phases_from_the_session_log() {
		let dir = std::env::temp_dir().join(format!(
			"omp-cli-runtime-todo-{}",
			crate::output::session_id()
		));
		let options = crate::session::SessionOptions {
			enabled: true,
			cwd: dir.clone(),
			model: None,
			provider: None,
			dir: Some(dir.clone()),
			id: Some("todo-runtime".to_string()),
			continue_latest: false,
			parent_session: None,
			no_title: false,
		};
		let mut store = crate::session::SessionStore::open(&options);
		assert!(store.enabled());

		// Invoke the real tool, persist its details exactly as the live turn
		// recorder does, then recover through a fresh store/registry.
		let ctx = pi_agent::AgentContext::new(dir.clone());
		let registry = build_registry_full(
			&["todo"],
			Vec::new(),
			Vec::new(),
			Vec::new(),
			Vec::new(),
			true,
			EditToolOptions::default(),
			None,
		)
		.expect("registry");
		let todo = registry.get("todo").expect("todo tool");
		let args = serde_json::json!({
			"op": "init",
			"list": [{"phase": "Plan", "items": ["resume me"]}],
		});
		let init = todo.execute(&args, &ctx).await.expect("todo init executes");
		assert!(!init.is_error, "init failed: {}", init.content);
		let details = init.details.clone().expect("details");
		assert_eq!(details["phases"][0]["tasks"][0]["content"], "resume me");
		assert_eq!(details["storage"], "session");

		store
			.append_turn(&crate::session::TurnRecord {
				prompt: "seed".to_string(),
				assistant: "planned".to_string(),
				stop_reason: "toolUse".to_string(),
				tool_calls: vec![crate::session::ToolCallRecord {
					id: "call-todo".to_string(),
					name: "todo".to_string(),
					arguments: args,
					result: init.content,
					is_error: init.is_error,
					result_parts: Vec::new(),
					details: Some(details),
				}],
				parts: Vec::new(),
			})
			.expect("append seed turn");
		drop(store);

		let reopened = crate::session::SessionStore::open(&options);
		let phases = reopened.latest_todo_phases();
		assert_eq!(phases[0].tasks[0].content, "resume me");

		let registry = build_registry_full(
			&["todo"],
			Vec::new(),
			Vec::new(),
			Vec::new(),
			phases,
			true,
			EditToolOptions::default(),
			None,
		)
		.expect("registry");
		let todo = registry.get("todo").expect("todo tool");
		let viewed = todo
			.execute(&serde_json::json!({"op": "view"}), &ctx)
			.await
			.expect("todo executes");
		assert!(!viewed.is_error);
		let details = viewed.details.as_ref().expect("details");
		assert_eq!(details["phases"][0]["tasks"][0]["content"], "resume me");
		assert_eq!(details["storage"], "session");

		std::fs::remove_dir_all(dir).ok();
	}

	#[test]
	fn latest_todo_phases_from_turns_prefers_newest_valid_todo_call() {
		use crate::session::{ToolCallRecord, TurnRecord};

		fn call(name: &str, is_error: bool, details: Option<serde_json::Value>) -> ToolCallRecord {
			ToolCallRecord {
				id: "call".to_string(),
				name: name.to_string(),
				arguments: serde_json::json!({}),
				result: String::new(),
				is_error,
				result_parts: Vec::new(),
				details,
			}
		}
		fn turn(calls: Vec<ToolCallRecord>) -> TurnRecord {
			TurnRecord {
				prompt: String::new(),
				assistant: String::new(),
				stop_reason: "toolUse".to_string(),
				tool_calls: calls,
				parts: Vec::new(),
			}
		}
		fn phases(content: &str) -> serde_json::Value {
			serde_json::json!([{
				"name": "Plan",
				"tasks": [{"content": content, "status": "pending"}],
			}])
		}
		fn details(phases: serde_json::Value) -> Option<serde_json::Value> {
			Some(serde_json::json!({"op": "init", "phases": phases, "storage": "memory"}))
		}

		assert!(latest_todo_phases_from_turns(&[]).is_empty(), "empty history has no phases");

		let seed = turn(vec![call("todo", false, details(phases("seed")))]);
		let found = latest_todo_phases_from_turns(std::slice::from_ref(&seed));
		assert_eq!(found[0].tasks[0].content, "seed");

		// Newest turn wins over older turns.
		let newer = turn(vec![call("todo", false, details(phases("newer")))]);
		let found = latest_todo_phases_from_turns(&[seed.clone(), newer]);
		assert_eq!(found[0].tasks[0].content, "newer");

		// A failed todo echoes the pre-call state; it never overrides.
		let failed = turn(vec![call("todo", true, details(phases("stale echo")))]);
		let found = latest_todo_phases_from_turns(&[seed.clone(), failed]);
		assert_eq!(found[0].tasks[0].content, "seed");

		// A child `task` result carrying nested phases is not a top-level call.
		let child = turn(vec![call(
			"task",
			false,
			Some(serde_json::json!({"results": [{"details": {"phases": phases("child")}}]})),
		)]);
		assert!(
			latest_todo_phases_from_turns(&[child]).is_empty(),
			"nested child task details must never become root todo state"
		);

		// Malformed payloads are skipped in favour of the older valid call.
		for malformed in [
			serde_json::json!({"phases": "oops"}),
			serde_json::json!({"phases": [{"name": 1}]}),
			serde_json::json!({}),
		] {
			let broken = turn(vec![call("todo", false, Some(malformed))]);
			let found = latest_todo_phases_from_turns(&[seed.clone(), broken]);
			assert_eq!(found[0].tasks[0].content, "seed", "malformed payload must not clear");
		}

		// `{"phases": []}` is a real clear and wins by turn/call order.
		let cleared = turn(vec![
			call("todo", false, details(phases("init"))),
			call("todo", false, details(serde_json::json!([]))),
		]);
		assert!(latest_todo_phases_from_turns(&[seed, cleared]).is_empty());
	}

	#[tokio::test]
	async fn no_session_todo_tool_rehydrates_memory_phases_from_turns() {
		let dir = std::env::temp_dir().join(format!(
			"omp-cli-runtime-todo-memory-{}",
			crate::output::session_id()
		));
		let history = vec![crate::session::TurnRecord {
			prompt: "plan".to_string(),
			assistant: "planned".to_string(),
			stop_reason: "toolUse".to_string(),
			tool_calls: vec![crate::session::ToolCallRecord {
				id: "call-todo".to_string(),
				name: "todo".to_string(),
				arguments: serde_json::json!({"op": "init"}),
				result: "todos updated".to_string(),
				is_error: false,
				result_parts: Vec::new(),
				details: Some(serde_json::json!({
					"op": "init",
					"phases": [{"name": "Plan", "tasks": [{"content": "resume me", "status": "pending"}]}],
					"storage": "memory",
				})),
			}],
			parts: Vec::new(),
		}];

		let phases = latest_todo_phases_from_turns(&history);
		assert_eq!(phases[0].tasks[0].content, "resume me");

		let registry = build_registry_full(
			&["todo"],
			Vec::new(),
			Vec::new(),
			Vec::new(),
			phases,
			false,
			EditToolOptions::default(),
			None,
		)
		.expect("registry");
		let viewed = registry
			.get("todo")
			.expect("todo tool")
			.execute(
				&serde_json::json!({"op": "view"}),
				&pi_agent::AgentContext::new(dir.clone()),
			)
			.await
			.expect("todo view executes");
		assert!(!viewed.is_error);
		let details = viewed.details.as_ref().expect("details");
		assert_eq!(details["phases"][0]["tasks"][0]["content"], "resume me");
		assert_eq!(
			details["storage"], "memory",
			"no-session todo details must report memory storage"
		);
	}

	#[test]
	fn cli_task_and_hub_share_supervisor_across_turns() {
		use std::io::{Read, Write};
		use std::sync::atomic::{AtomicU64, Ordering};

		fn last_message_text(body: &str) -> String {
			let Ok(request) = serde_json::from_str::<serde_json::Value>(body) else {
				return String::new();
			};
			let Some(messages) = request.get("messages").and_then(|v| v.as_array()) else {
				return String::new();
			};
			let Some(last) = messages.last() else {
				return String::new();
			};
			match last.get("content") {
				Some(serde_json::Value::String(text)) => text.clone(),
				Some(serde_json::Value::Array(parts)) => parts
					.iter()
					.filter_map(|part| part.get("text").and_then(|v| v.as_str()))
					.collect::<Vec<_>>()
					.join(""),
				_ => String::new(),
			}
		}

		let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fixture");
		let addr = listener.local_addr().expect("fixture addr");
		let call_seq = AtomicU64::new(0);
		let seen_tool_schemas = Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
		let seen_tool_schemas_for_server = Arc::clone(&seen_tool_schemas);
		let server = std::thread::spawn(move || {
			for _ in 0..5 {
				let (mut socket, _) = listener.accept().expect("accept fixture");
				let mut buf = Vec::new();
				let mut chunk = [0u8; 4096];
				let mut header_end = None;
				let mut content_len = 0usize;
				loop {
					let n = socket.read(&mut chunk).expect("read request");
					if n == 0 {
						break;
					}
					buf.extend_from_slice(&chunk[..n]);
					if header_end.is_none() {
						if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
							header_end = Some(pos + 4);
							let headers = String::from_utf8_lossy(&buf[..pos]);
							for line in headers.lines() {
								if line.to_ascii_lowercase().starts_with("content-length:") {
									content_len = line
										.split(':')
										.nth(1)
										.and_then(|v| v.trim().parse().ok())
										.unwrap_or(0);
								}
							}
						}
					}
					if let Some(end) = header_end {
						if buf.len() >= end + content_len {
							break;
						}
					}
				}
				let end = header_end.expect("headers");
				let body = String::from_utf8_lossy(&buf[end..end + content_len]);
				let latest = last_message_text(&body);
				let parsed = serde_json::from_str::<serde_json::Value>(&body).ok();
				let seen: Vec<String> = parsed
					.as_ref()
					.and_then(|request| request.get("tools"))
					.and_then(|tools| tools.as_array())
					.map(|tools| {
						tools
							.iter()
							.filter_map(|tool| {
								tool.pointer("/function/name").and_then(|name| name.as_str()).map(str::to_string)
							})
							.collect()
					})
					.unwrap_or_default();
				seen_tool_schemas_for_server.lock().expect("schema log").push(seen);
				let streaming = parsed
					.as_ref()
					.and_then(|request| request.get("stream").and_then(|v| v.as_bool()))
					.unwrap_or(false);

				let (tool, text) = if latest == "CLI_TASK" {
					(
						Some((
							"task",
							serde_json::json!({
								"name": "cli-child",
								"agent": "cli-child",
								"task": "CLI_CHILD_WORK"
							}),
						)),
						None,
					)
				} else if latest.starts_with("Complete assignment thoroughly:") && latest.contains("CLI_CHILD_WORK") {
					(
						Some(("yield", serde_json::json!({ "result": { "data": "child-ok" } }))),
						None,
					)
				} else if latest == "CLI_JOBS" {
					(Some(("hub", serde_json::json!({ "op": "jobs" }))), None)
				} else if latest.contains("\"op\":\"jobs\"") {
					(None, Some("jobs done"))
				} else if latest.contains("\"name\":\"cli-child\"") {
					(None, Some("root done"))
				} else {
					(None, Some("unexpected route"))
				};

				let id = call_seq.fetch_add(1, Ordering::SeqCst) + 1;
				let payload = if streaming {
					if let Some((name, args)) = tool {
						let delta = serde_json::json!({
							"choices": [{
								"delta": {
									"tool_calls": [{
										"index": 0,
										"id": format!("call_{id}"),
										"type": "function",
										"function": {
											"name": name,
											"arguments": serde_json::to_string(&args).unwrap()
										}
									}]
								},
								"finish_reason": null
							}]
						});
						format!("data: {delta}\n\ndata: [DONE]\n\n")
					} else {
						let delta = serde_json::json!({
							"choices": [{
								"delta": { "content": text.unwrap_or("unexpected route") },
								"finish_reason": null
							}]
						});
						format!("data: {delta}\n\ndata: [DONE]\n\n")
					}
				} else if let Some((name, args)) = tool {
					serde_json::json!({
						"choices": [{
							"message": {
								"role": "assistant",
								"content": null,
								"tool_calls": [{
									"id": format!("call_{id}"),
									"type": "function",
									"function": {
										"name": name,
										"arguments": serde_json::to_string(&args).unwrap()
									}
								}]
							}
						}]
					})
					.to_string()
				} else {
					serde_json::json!({
						"choices": [{
							"message": {
								"role": "assistant",
								"content": text.unwrap_or("unexpected route"),
								"tool_calls": null
							}
						}]
					})
					.to_string()
				};

				let content_type = if streaming {
					"text/event-stream"
				} else {
					"application/json"
				};
				write!(
					socket,
					"HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
					payload.len(),
					payload
				)
				.expect("write fixture");
				socket.flush().expect("flush fixture");
			}
		});

		// Explicit project agent: the bundled `task` agent's model/effort policy
		// may change, but this test pins synchronous delegation and yield
		// extraction, so the child must carry its own model and tool allowlist.
		let project = std::env::temp_dir().join(format!("omp-cli-task-hub-{}", std::process::id()));
		let agents_dir = project.join(".omp/agents");
		std::fs::create_dir_all(&agents_dir).expect("project agents dir");
		std::fs::write(
			agents_dir.join("cli-child.md"),
			"---\nname: cli-child\ndescription: CLI delegation fixture child\ntools:\n  - yield\nmodel: local/llama3\n---\nYield the requested data.\n",
		)
		.expect("write cli-child agent");
		let mut req = request(yolo());
		req.cwd = project.clone();
		// This test pins the synchronous delegation contract (inline `task`
		// results); background delivery is covered by pi-agent's
		// `async_coordination` suite.
		req.config.async_config.enabled = Some(false);
		req.config.providers.insert("local".to_string(), ProviderConfig {
			base_url: Some(format!("http://{addr}/v1")),
			api_key: Some("sk-test".to_string()),
			..ProviderConfig::default()
		});
		req.model = Some("local/llama3".to_string());
		req.prompt = "CLI_TASK".to_string();

		let runner = PiAgentRunner::new().expect("runner");
		let mut task_result = None;
		let mut child_yield_at_root = false;
		for event in runner.run(&req).expect("run task turn") {
			if let Ok(Event::ToolEnd { tool, result, .. }) = event {
				if tool == "yield" {
					child_yield_at_root = true;
				}
				if tool == "task" {
					task_result = Some(result);
				}
			}
		}
		let task_result = task_result.expect("task tool ran");
		let task_details: serde_json::Value =
			serde_json::from_str(&task_result).expect("task result json");
		assert_eq!(task_details["results"][0]["name"], "cli-child");
		let child_id = task_details["results"][0]["id"]
			.as_str()
			.expect("child id")
			.to_string();
		assert!(
			child_id.starts_with("task-"),
			"child id must be supervisor-generated, got {child_id}"
		);
		assert_eq!(task_details["results"][0]["exitCode"].as_i64(), Some(0));
		assert_eq!(
			task_details["results"][0]["extractedToolData"]["yield"][0]["data"],
			"child-ok",
			"task result: {task_details}"
		);
		assert!(!child_yield_at_root, "yield must stay child-only");
		let schemas = seen_tool_schemas.lock().expect("schema log");
		assert!(
			schemas
				.iter()
				.any(|tools| tools.iter().any(|tool| tool == "task") && !tools.iter().any(|tool| tool == "yield")),
			"root schema must expose task without yield: {schemas:?}"
		);
		assert!(
			schemas
				.iter()
				.any(|tools| tools.iter().any(|tool| tool == "yield") && !tools.iter().any(|tool| tool == "task")),
			"child schema must expose yield: {schemas:?}"
		);
		for tools in schemas.iter() {
			if tools.iter().any(|tool| tool == "task") {
				assert!(
					!tools.iter().any(|tool| tool == "yield"),
					"root tool schema leaked child yield: {tools:?}"
				);
			}
		}
		drop(schemas);

		// A later REPL turn must still see the completed child in the same
		// in-memory supervisor.
		let mut jobs_req = request(yolo());
		jobs_req.cwd = project.clone();
		jobs_req.config.providers.insert("local".to_string(), ProviderConfig {
			base_url: Some(format!("http://{addr}/v1")),
			api_key: Some("sk-test".to_string()),
			..ProviderConfig::default()
		});
		jobs_req.model = Some("local/llama3".to_string());
		jobs_req.prompt = "CLI_JOBS".to_string();

		let mut hub_result = None;
		for event in runner.run(&jobs_req).expect("run hub turn") {
			if let Ok(Event::ToolEnd { tool, result, .. }) = event {
				if tool == "hub" {
					hub_result = Some(result);
				}
			}
		}
		let hub_result = hub_result.expect("hub tool ran");
		let hub_details: serde_json::Value =
			serde_json::from_str(&hub_result).expect("hub result json");
		assert_eq!(hub_details["op"], "jobs");
		assert_eq!(hub_details["jobs"][0]["id"].as_str(), Some(child_id.as_str()));
		assert_eq!(hub_details["jobs"][0]["name"], "cli-child");
		assert_eq!(hub_details["jobs"][0]["status"], "completed");
		assert_eq!(hub_details["jobs"][0]["type"], "task");

		server.join().expect("fixture joined");
	}
}

#[cfg(test)]
mod process_scope_tests {
	use super::*;

	#[test]
	fn fallback_scope_root_is_stable_and_per_project() {
		// Scope roots derive from `agent_dir()`, which other tests relocate via
		// `PI_CODING_AGENT_DIR`/`PI_CONFIG_DIR` while deleting their temp roots.
		// Holding the env lock keeps this fixture's root out of their cleanup.
		let _env = crate::TEST_ENV_LOCK.lock().expect("env lock");
		let one = PathBuf::from("/tmp/project-one");
		let two = PathBuf::from("/tmp/project-two");
		let (a, b) = ("runner-a", "runner-b");
		assert_eq!(process_scope_root(&one, a), process_scope_root(&one, a));
		assert_ne!(process_scope_root(&one, a), process_scope_root(&two, a));
		// Same project, two runners: distinct roots so one manager's drop
		// cannot prune the other's logs.
		let root_a = process_scope_root(&one, a);
		let root_b = process_scope_root(&one, b);
		assert_ne!(root_a, root_b);
		std::fs::create_dir_all(&root_a).unwrap();
		std::fs::create_dir_all(&root_b).unwrap();
		std::fs::write(root_a.join("run.log"), b"a").unwrap();
		std::fs::write(root_b.join("run.log"), b"b").unwrap();
		let manager_a = pi_agent::tools::process::ProcessManager::new(root_a.clone());
		let manager_b = pi_agent::tools::process::ProcessManager::new(root_b.clone());
		drop(manager_a);
		assert!(!root_a.exists());
		assert!(root_b.join("run.log").exists());
		drop(manager_b);
		assert!(!root_b.exists());
	}
	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn lazy_backend_initializes_once_under_concurrency() {
		let root = std::env::temp_dir().join(format!(
			"omp-lazy-singleflight-{}-{}",
			std::process::id(),
			crate::output::session_id()
		));
		std::fs::create_dir_all(&root).unwrap();
		std::fs::write(root.join("marker"), b"keep").unwrap();
		let lazy = Arc::new(LazyProcessBackend::for_test_unavailable_broker(root.clone()));
		let mut tasks = Vec::new();
		for _ in 0..8 {
			let lazy = Arc::clone(&lazy);
			tasks.push(tokio::spawn(async move { lazy.list(None).await.is_ok() }));
		}
		for task in tasks {
			assert!(task.await.expect("join"), "concurrent first op failed");
		}
		// A losing initializer would have dropped its own fallback manager,
		// deleting this shared root and the marker with it.
		assert!(root.join("marker").exists(), "fallback root was deleted by a losing initializer");
		let _ = std::fs::remove_dir_all(&root);
	}

	#[test]
	fn background_names_are_unique_across_backend_instances() {
		let root = std::env::temp_dir().join(format!("omp-lazy-names-{}", std::process::id()));
		let a = LazyProcessBackend::for_test_unavailable_broker(root.join("a"));
		let b = LazyProcessBackend::for_test_unavailable_broker(root.join("b"));
		let names: std::collections::HashSet<String> = (0..32)
			.flat_map(|_| [a.next_background_name(), b.next_background_name()])
			.collect();
		assert_eq!(names.len(), 64, "background job names collided across backends");
	}

	/// A second `shutdown` caller must not read the already-empty slots as
	/// "teardown done": `begin` elects exactly one owner and `wait` returns only
	/// after that owner reported real completion.
	#[test]
	fn shutdown_latch_waits_for_the_real_teardown() {
		let latch = Arc::new(ShutdownLatch::new());
		assert!(latch.begin(), "first caller owns teardown");
		assert!(!latch.begin(), "second caller must not repeat teardown");
		let finished = Arc::new(AtomicBool::new(false));
		let waiter = {
			let latch = Arc::clone(&latch);
			let finished = Arc::clone(&finished);
			std::thread::spawn(move || {
				latch.wait();
				assert!(
					finished.load(Ordering::SeqCst),
					"wait returned before teardown finished"
				);
			})
		};
		std::thread::sleep(std::time::Duration::from_millis(50));
		finished.store(true, Ordering::SeqCst);
		latch.finish();
		waiter.join().expect("waiter thread");
		// Closed latch: later callers return at once and elect nobody.
		latch.wait();
		assert!(!latch.begin());
	}

}
