use std::{
	collections::{BTreeMap, HashMap, HashSet, VecDeque},
	io::Write,
	path::PathBuf,
	sync::{
		Arc, Mutex, MutexGuard, Weak,
		atomic::{AtomicBool, AtomicU64, Ordering},
	},
	time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

use crate::{
	dap::DapTool,
	eval::EvalToolBinder,
	loop_module::{AgentEvent, SteeringEvent},
	lsp::LspTool,
	mailbox::Mailbox,
	provider::ModelProvider,
	state::{AcpClientBridge, Message, StagedPreviewPolicy, TokenUsage, ToolApprover},
	tools::{
		AstGrepTool, BashTool, EditTool, HubTool, PendingStore, TaskTool, ThinkTool, WriteTool,
		YieldTool,
		edit_variants::{ResolvedSessionTools, SessionDisplayResolver, default_session_tools},
		hashline::{self, NativeSnapshotSink, SnapshotStore},
		internal_urls::{InternalUrlRouter, UrlScope},
		process::{ProcessBackend, ProcessManager},
		read::ReadTool,
		registry::ToolRegistry,
		task_catalog::AgentCatalog,
		task_isolation::TaskIsolationSettings,
	},
};

/// Settings a child's isolated `lsp`/`debug` instances inherit from the root.
///
/// Root instances live in the supervisor template and hold the runner's shared
/// LSP ownership scope and DAP session tree. A child must not see, drive, or
/// terminate the parent's debuggee and must not keep the parent's owned LSP
/// processes alive, so [`Supervisor::child_registry_unbound`] swaps in fresh
/// instances built from these values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildToolSettings {
	/// `lsp.shared`: children use the same broker transport preference.
	pub lsp_shared:       bool,
	/// `tools.maxTimeout` ceiling handed to the child debug tool.
	pub debug_max_timeout: Option<u64>,
}

impl Default for ChildToolSettings {
	fn default() -> Self {
		Self { lsp_shared: true, debug_max_timeout: None }
	}
}

/// Settled job records stay inspectable for five minutes (TypeScript
/// `DEFAULT_RETENTION_MS`), with a hard count cap so a long-lived REPL session
/// cannot grow memory without bound.
const RECORD_RETENTION_MS: u64 = 5 * 60 * 1000;
const MAX_SETTLED_RECORDS: usize = 100;

/// Outstanding results (running + undelivered) admitted by `reserve_child`.
/// Admission fails explicitly at the cap instead of growing an unbounded
/// backlog of promised bodies while the root provider call is blocked.
pub const MAX_OUTSTANDING_RESULTS: usize = 64;
/// Per-result body cap; mirrors the existing bash output budget.
pub const MAX_RESULT_BYTES: usize = 64 * 1024;
/// Aggregate cap for queued (undelivered) result text. Slot and per-body caps
/// keep the realistic maximum below this; the check is a defensive bound.
pub const MAX_PENDING_RESULT_BYTES: usize = 8 * 1024 * 1024;

/// Cap a result body on a UTF-8 boundary and mark it honestly.
pub fn cap_result_text(text: &str) -> (String, bool) {
	if text.len() <= MAX_RESULT_BYTES {
		return (text.to_string(), false);
	}
	let marker = format!("\n[truncated: {} bytes total]", text.len());
	let mut end = MAX_RESULT_BYTES.saturating_sub(marker.len());
	while end > 0 && !text.is_char_boundary(end) {
		end -= 1;
	}
	(format!("{}{}", &text[..end], marker), true)
}

fn truncate_to_bytes(text: &str, max_bytes: usize) -> String {
	if text.len() <= max_bytes {
		return text.to_string();
	}
	let marker = format!("\n[truncated: {} bytes total]", text.len());
	// Never grow past the caller's budget: if even the marker cannot fit, the
	// only honest representation is an empty body (the aggregate bound is
	// defensive; slot and per-body caps keep it unreachable in practice).
	if marker.len() >= max_bytes {
		// Signal truncation without exceeding the remaining budget.
		return marker[..max_bytes].to_string();
	}
	let mut end = max_bytes - marker.len();
	while end > 0 && !text.is_char_boundary(end) {
		end -= 1;
	}
	format!("{}{}", &text[..end], marker)
}

fn now_ms() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|duration| duration.as_millis() as u64)
		.unwrap_or(0)
}

/// Pull-based delivery of finished background results into an agent loop.
///
/// The loop calls [`take_background_results`] at a turn boundary; removal and
/// return happen under the supervisor lock, so a result is delivered exactly
/// once even when several callers race.
pub trait BackgroundResultSink: Send + Sync {
	fn take_background_results(&self) -> Vec<String>;

	/// Rich view for hosts that account delivered child usage at the boundary.
	///
	/// Shares one consumption with [`take_background_results`]: whichever view
	/// drains an id first removes it, so a host wrapper may call this while the
	/// loop calls the text view and still see every delivery once. The default
	/// carries no identity or usage (a sink with no supervisor behind it);
	/// supervisor-backed sinks override it.
	fn take_background_deliveries(&self) -> Vec<BackgroundDelivery> {
		self
			.take_background_results()
			.into_iter()
			.map(|text| BackgroundDelivery { id: String::new(), text, usage: None })
			.collect()
	}
}

/// One undelivered background result, scoped to the agent that spawned it.
#[derive(Clone)]
struct PendingDelivery {
	id: String,
	owner: Option<String>,
	text: String,
	/// Summed reported usage retained from the settled job record at enqueue
	/// time. `None` when the child (or the host-owned eval job) reported none.
	usage: Option<TokenUsage>,
}

/// A delivered background result with the identity and reported usage that
/// make root accounting exact.
///
/// `id` is the exactly-once key: the supervisor marks a result consumed under
/// the same lock that removes it, so an id reaches a consumer through the
/// auto-delivery path or the hub snapshot path, never both. `usage` is absent
/// when nothing was reported — never a zero-filled stand-in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackgroundDelivery {
	pub id: String,
	pub text: String,
	pub usage: Option<TokenUsage>,
}

struct OwnerResultSink {
	supervisor: Arc<Supervisor>,
	owner: Option<String>,
}

impl BackgroundResultSink for OwnerResultSink {
	fn take_background_results(&self) -> Vec<String> {
		self.supervisor.take_results_for(self.owner.as_deref())
	}

	fn take_background_deliveries(&self) -> Vec<BackgroundDelivery> {
		self.supervisor.take_deliveries_for(self.owner.as_deref())
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DelegationLimits {
	pub max_depth: i32,
	pub max_concurrency: usize,
}

impl Default for DelegationLimits {
	fn default() -> Self {
		Self {
			max_depth: 2,
			max_concurrency: 32,
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChildStatus {
	Running,
	Completed,
	Failed,
	Aborted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildRecord {
	/// Immutable, supervisor-generated identity. This is the only value accepted
	/// by `hub cancel`; the model-supplied name is a non-unique label.
	pub id: String,
	/// Model-supplied display name (`task.name`), never used for identity.
	pub name: String,
	pub index: usize,
	pub agent: String,
	pub task: String,
	pub status: ChildStatus,
	/// Agent that spawned this child; `None` for a root-owned spawn. Used to
	/// scope child `hub` snapshots and cancellation to direct descendants.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub parent_id: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub started_at: Option<u64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub duration_ms: Option<u64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub output: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub error: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub aborted: Option<bool>,
	#[serde(skip_serializing_if = "Option::is_none", alias = "abort_reason")]
	pub abort_reason: Option<String>,
	/// Real child transcript log (`<session artifacts>/<child_id>.jsonl`) the
	/// task executor writes. `None` when no session root was installed.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub session_file: Option<PathBuf>,
	/// Source the agent definition came from (`project`, `builtin`, ...).
	pub agent_source: String,
	/// Model tool-call id that spawned this child, when the caller has one.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub parent_tool_call_id: Option<String>,
	/// True for background spawns (`task` with async), false for synchronous
	/// ones. Mirrors the upstream `detached` flag on lifecycle/progress frames.
	pub detached: bool,
	/// Summed provider-reported usage over this child's own turns, taken from
	/// each turn's assistant [`Message::usage`]. `None` when the provider
	/// reported nothing at all (unknown, never a zero-filled stand-in). The
	/// delivered `<task-result>` envelope carries the same counters once, so a
	/// root consumer never has to guess which of the record and the text is
	/// authoritative.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub usage: Option<TokenUsage>,
}

/// One reserve request with every field observability needs. `reserve_child`
/// keeps the historical five-argument shape for callers that only need
/// identity.
#[derive(Debug, Clone, Copy)]
pub struct ChildSpawn<'a> {
	pub name:                &'a str,
	pub index:               usize,
	pub agent:               &'a str,
	pub agent_source:        &'a str,
	pub task:                &'a str,
	pub parent_id:           Option<&'a str>,
	pub parent_tool_call_id: Option<&'a str>,
	pub detached:            bool,
}

impl<'a> ChildSpawn<'a> {
	#[must_use]
	pub fn new(name: &'a str, index: usize, agent: &'a str, task: &'a str) -> Self {
		Self {
			name,
			index,
			agent,
			agent_source: agent,
			task,
			parent_id: None,
			parent_tool_call_id: None,
			detached: false,
		}
	}
}

/// Terminal state carried by a lifecycle event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentLifecycleStatus {
	Started,
	Completed,
	Failed,
	Aborted,
}

impl SubagentLifecycleStatus {
	#[must_use]
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Started => "started",
			Self::Completed => "completed",
			Self::Failed => "failed",
			Self::Aborted => "aborted",
		}
	}
}

/// Spawn/terminal notification for one child (`subagent_lifecycle` upstream).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentLifecycle {
	pub id:                  String,
	pub index:               usize,
	pub agent:               String,
	pub agent_source:        String,
	pub task:                String,
	pub status:              SubagentLifecycleStatus,
	pub session_file:        Option<String>,
	pub parent_tool_call_id: Option<String>,
	pub detached:            bool,
	pub error:               Option<String>,
	pub abort_reason:        Option<String>,
	pub duration_ms:         Option<u64>,
}

/// Real progress for one running child (`subagent_progress` upstream). Emitted
/// only at genuine transitions (a reserved child actually starting to run), not
/// on a timer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentProgress {
	pub id:                  String,
	pub index:               usize,
	pub agent:               String,
	pub agent_source:        String,
	pub task:                String,
	pub parent_tool_call_id: Option<String>,
	pub session_file:        Option<String>,
	pub detached:            bool,
	pub status:              String,
	pub description:         Option<String>,
}

/// One observability notification. `Event` carries a real event emitted by the
/// child agent's own loop (never synthesized): `task` forwards the loop's
/// `AgentEvent` channel for every child it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentEvent {
	Lifecycle(SubagentLifecycle),
	Progress(SubagentProgress),
	Event { id: String, event: AgentEvent },
}

/// Consumer of [`SubagentEvent`]s. The RPC mode installs one that feeds the
/// `SubagentRegistry`; embedders may install any other.
pub trait SubagentObserver: Send + Sync {
	fn observe(&self, event: &SubagentEvent);
}

/// Removes its observer when dropped. Holding it for the session lifetime is
/// what keeps a registry from leaking across sessions: drop the guard and no
/// further event reaches the observer.
pub struct ObserverGuard {
	supervisor: Weak<Supervisor>,
	id:         u64,
}

impl Drop for ObserverGuard {
	fn drop(&mut self) {
		if let Some(supervisor) = self.supervisor.upgrade() {
			supervisor
				.observers
				.lock()
				.unwrap_or_else(|poison| poison.into_inner())
				.retain(|(id, _)| *id != self.id);
		}
	}
}

/// Lifecycle payload for one record at one status.
fn lifecycle_of(record: &ChildRecord, status: SubagentLifecycleStatus) -> SubagentLifecycle {
	SubagentLifecycle {
		id:                  record.id.clone(),
		index:               record.index,
		agent:               record.agent.clone(),
		agent_source:        record.agent_source.clone(),
		task:                record.task.clone(),
		status,
		session_file:        record
			.session_file
			.as_ref()
			.map(|path| path.display().to_string()),
		parent_tool_call_id: record.parent_tool_call_id.clone(),
		detached:            record.detached,
		error:               record.error.clone(),
		abort_reason:        record.abort_reason.clone(),
		duration_ms:         record.duration_ms,
	}
}

#[derive(Clone)]
struct ActiveChild {
	steering_tx: mpsc::Sender<SteeringEvent>,
	abort_handle: tokio::task::AbortHandle,
	/// The child loop's own cancellation token. Cancelling it lets in-flight
	/// tools (notably `bash`) terminate their subprocesses cooperatively.
	cancellation_token: CancellationToken,
}

struct SupervisorState {
	records: HashMap<String, ChildRecord>,
	order: Vec<String>,
	active: HashMap<String, ActiveChild>,
	/// Cancellation tokens for children that are reserved but still waiting for
	/// a concurrency permit. Cancelling one prevents the child from starting.
	reservations: HashMap<String, CancellationToken>,
	/// Background results not yet handed to their owner or a hub snapshot.
	pending: VecDeque<PendingDelivery>,
	/// Jobs whose result was already delivered or recovered by a snapshot.
	consumed: HashSet<String>,
	/// Wall-clock settlement time, used by bounded record retention.
	settled_at: HashMap<String, u64>,
	/// Results admitted but not yet consumed (running or pending delivery).
	outstanding: HashSet<String>,
	/// Bytes held by queued undelivered result text.
	pending_bytes: usize,
}

pub struct Supervisor {
	/// Parent tool whitelist (builtin + allowed MCP). Child registries clone it
	/// so delegation can never grant a tool the caller was not offered.
	template: Mutex<ToolRegistry>,
	provider: Mutex<Arc<dyn ModelProvider>>,
	child_approver: Mutex<Option<Arc<dyn ToolApprover>>>,
	limits: Mutex<DelegationLimits>,
	allow_task: AtomicBool,
	allow_hub: AtomicBool,
	/// `async.enabled` equivalent: background spawns are allowed by default.
	async_enabled: AtomicBool,
	/// Set once a result sink is registered. Without a sink a background
	/// result has nowhere to go, so the task tool falls back to synchronous
	/// execution (same fallback the TypeScript manager logs when it is absent).
	result_sink_attached: AtomicBool,
	/// Root session spawn permits, shared by every root `TaskTool` instance so
	/// the `task.maxConcurrency` bound survives REPL turns. Each child registry
	/// creates its own semaphore, matching the TypeScript one-per-session
	/// `TaskTool`; a supervisor-wide semaphore would deadlock nested spawns
	/// while a parent holds a permit across its descendants.
	root_permits: Arc<Semaphore>,
	/// Peer mailboxes; supervisor-owned so they survive REPL turns.
	mailbox: Arc<Mailbox>,
	/// Options copied into each child's fresh `EditTool` (same knobs as the
	/// parent run, but a fresh store/registers/guard).
	edit_options: Mutex<hashline::edit::EditToolOptions>,
	/// Host-injected `edit.mode`/display resolution. Children resolve again with
	/// their own selected model; unset keeps the config-free default.
	display_resolver: Mutex<Option<Arc<dyn SessionDisplayResolver>>>,
	/// Root session's actual `provider/id`, so a child that inherits the parent
	/// provider still resolves model-specific variants instead of dropping them.
	active_model: Mutex<Option<String>>,
	/// Agent catalog installed by the host (CLI) runtime. Definitions and policy
	/// are stable; the catalog's model resolver is refreshed per root turn.
	agent_catalog: Mutex<Option<Arc<AgentCatalog>>>,
	/// Process supervisor shared by root/child `bash` and `hub` process ops.
	/// Injected by the host (broker client); otherwise a lazily created local
	/// manager rooted at `process_root`.
	processes: Mutex<Option<Arc<dyn ProcessBackend>>>,
	/// Host-mounted eval tool factory. Root binds after the final registry is
	/// assembled; children bind per owner so each gets a fresh eval session.
	eval_binder: Mutex<Option<Arc<dyn EvalToolBinder>>>,
	/// ACP reverse-call bridge for this connection. Children inherit it so
	/// their `read`/`write`/`bash` route through the same client capabilities
	/// as the root; `None` keeps every descendant local.
	acp_bridge: Mutex<Option<Arc<dyn AcpClientBridge>>>,
	/// Settings for child-scoped `lsp`/`debug` instances (see
	/// [`ChildToolSettings`]). Set by the host from `lsp.shared` and
	/// `tools.maxTimeout` on every root turn.
	child_tool_settings: Mutex<ChildToolSettings>,
	/// `task.isolation.*` + resolved `worktree.base`. Installed by the CLI on
	/// every root turn; each `task` spawn snapshots it through `ChildContext`.
	task_isolation: Mutex<TaskIsolationSettings>,
	process_root: PathBuf,
	/// Runner-unique prefix for backend-visible process owners. UI ids such as
	/// `task-1` repeat across concurrent CLIs; process scoping must not.
	process_owner_ns: String,
	state: Mutex<SupervisorState>,
	seq: AtomicU64,
	/// Wakes `cancel_child`, `hub jobs`, and `hub wait` waiters whenever a child
	/// reaches a terminal status or a delivery is consumed.
	settle: Notify,
	/// Installed observability consumers, keyed by subscription id so a guard
	/// can remove exactly its own entry.
	observers: Mutex<Vec<(u64, Arc<dyn SubagentObserver>)>>,
	observer_seq: AtomicU64,
	/// Serializes a child's state decision with the frames it produces, so a
	/// settled child can never emit after its terminal lifecycle frame. Always
	/// taken before `state`; observer callbacks run under this gate but never
	/// under `state`, and must not re-enter an emitting method.
	emission: Mutex<()>,
	/// Directory child transcripts are written to (`<artifacts>/<id>.jsonl`).
	child_session_root: Mutex<Option<PathBuf>>,
	/// Session internal URL router. Children rebuild `read`/`write` with their
	/// own snapshot/pending stores, so the router must be re-applied there.
	internal_urls: Mutex<Option<Arc<InternalUrlRouter>>>,
	/// Root session scope paired with the router. Each tool call builds its own
	/// context from it, so concurrent turns never share mutable scope.
	url_scope: Mutex<Arc<UrlScope>>,
}

/// Result of a scoped `hub` cancellation attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelAttempt {
	Cancelled,
	NotFound,
	AlreadyCompleted,
}

/// Backend owner namespace: random per supervisor, never a bare PID/scope
/// counter. Without `/dev/urandom`, fall back to this process's kernel start
/// time (a recycled PID gets a new start time; execve keeps both), and
/// only then to pid+nanos as an explicitly unsupported proc+entropy fallback.
fn owner_namespace(scope: u64) -> String {
	let pid = std::process::id();
	let nonce = match crate::process_broker::protocol::random_hex(8) {
		Ok(hex) => hex,
		Err(_) => match pi_shell::process::Process::from_pid(pid as i32) {
			Some(process) => format!("start-{}", process.identity_token()),
			None => format!(
				"{:x}",
				std::time::SystemTime::now()
					.duration_since(std::time::UNIX_EPOCH)
					.map_or(0, |elapsed| elapsed.as_nanos())
			),
		},
	};
	format!("omp-{pid}-{nonce}-{scope}")
}

impl Supervisor {
	pub fn new(
		template: ToolRegistry,
		provider: Arc<dyn ModelProvider>,
		child_approver: Option<Arc<dyn ToolApprover>>,
		limits: DelegationLimits,
	) -> Arc<Self> {
		Self::with_selection(template, provider, child_approver, limits, true, true)
	}

	pub fn with_selection(
		template: ToolRegistry,
		provider: Arc<dyn ModelProvider>,
		child_approver: Option<Arc<dyn ToolApprover>>,
		limits: DelegationLimits,
		allow_task: bool,
		allow_hub: bool,
	) -> Arc<Self> {
		let permits = if limits.max_concurrency == 0 {
			Semaphore::MAX_PERMITS
		} else {
			limits.max_concurrency
		};
		static SCOPE: AtomicU64 = AtomicU64::new(0);
		let scope = SCOPE.fetch_add(1, Ordering::SeqCst) + 1;
		Arc::new(Self {
			template: Mutex::new(template),
			provider: Mutex::new(provider),
			child_approver: Mutex::new(child_approver),
			limits: Mutex::new(limits),
			allow_task: AtomicBool::new(allow_task),
			allow_hub: AtomicBool::new(allow_hub),
			async_enabled: AtomicBool::new(true),
			result_sink_attached: AtomicBool::new(false),
			root_permits: Arc::new(Semaphore::new(permits)),
			mailbox: Arc::new(Mailbox::default()),
			edit_options: Mutex::new(hashline::edit::EditToolOptions::default()),
			display_resolver: Mutex::new(None),
			active_model: Mutex::new(None),
			agent_catalog: Mutex::new(None),
			processes: Mutex::new(None),
			eval_binder: Mutex::new(None),
			acp_bridge: Mutex::new(None),
			child_tool_settings: Mutex::new(ChildToolSettings::default()),
			task_isolation: Mutex::new(TaskIsolationSettings::default()),
			process_root: {
				std::env::temp_dir()
					.join(format!("omp-agent-processes-{}-{scope}", std::process::id()))
			},
			process_owner_ns: owner_namespace(scope),
			state: Mutex::new(SupervisorState {
				records: HashMap::new(),
				order: Vec::new(),
				active: HashMap::new(),
				reservations: HashMap::new(),
				pending: VecDeque::new(),
				consumed: HashSet::new(),
				settled_at: HashMap::new(),
				outstanding: HashSet::new(),
				pending_bytes: 0,
			}),
			seq: AtomicU64::new(0),
			settle: Notify::new(),
			observers: Mutex::new(Vec::new()),
			observer_seq: AtomicU64::new(0),
			emission: Mutex::new(()),
			child_session_root: Mutex::new(None),
			internal_urls: Mutex::new(None),
			url_scope: Mutex::new(Arc::new(UrlScope::default())),
		})
	}

	/// Re-point a long-lived supervisor at the current run's registry, provider,
	/// approval gate, limits, and tool selection. Child records are retained so a
	/// later turn in the same process can inspect them via `hub jobs`.
	pub fn rebind(
		&self,
		template: ToolRegistry,
		provider: Arc<dyn ModelProvider>,
		child_approver: Option<Arc<dyn ToolApprover>>,
		limits: DelegationLimits,
		allow_task: bool,
		allow_hub: bool,
	) {
		*self.template.lock().unwrap() = template;
		*self.provider.lock().unwrap() = provider;
		*self.child_approver.lock().unwrap() = child_approver;
		*self.limits.lock().unwrap() = limits;
		self.allow_task.store(allow_task, Ordering::SeqCst);
		self.allow_hub.store(allow_hub, Ordering::SeqCst);
		// Child records *and* active abort handles are retained: a child still
		// running from an earlier turn keeps its handle so `hub cancel` can
		// reach it. Normal completion, cancellation, and parent-future drop all
		// unregister their own handle, so this map cannot grow stale.
	}

	pub fn set_agent_catalog(&self, catalog: Arc<AgentCatalog>) {
		*self
			.agent_catalog
			.lock()
			.unwrap_or_else(|poison| poison.into_inner()) = Some(catalog);
	}

	/// Install the session's internal URL router so child `read`/`write`
	/// instances resolve the same schemes as the root registry.
	pub fn set_internal_urls(&self, router: Option<Arc<InternalUrlRouter>>) {
		*self
			.internal_urls
			.lock()
			.unwrap_or_else(|poison| poison.into_inner()) = router;
	}

	/// Install the root session scope (session file + artifacts dir) that every
	/// tool call copies into its own context.
	pub fn set_url_scope(&self, scope: Arc<UrlScope>) {
		*self.url_scope.lock().unwrap_or_else(|poison| poison.into_inner()) = scope;
	}

	/// Session internal URL router, when one is installed.
	#[must_use]
	pub fn internal_urls(&self) -> Option<Arc<InternalUrlRouter>> {
		self
			.internal_urls
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
			.clone()
	}

	/// Scope for one registry rebuild. A child gets its own transcript file and
	/// artifacts directory; roots get the session scope installed above.
	fn url_scope_for(&self, owner: Option<&str>) -> Arc<UrlScope> {
		if let Some(id) = owner
			&& let Some(path) = self.child_session_file(id)
		{
			let local_root = path
				.parent()
				.map(std::path::Path::to_path_buf)
				.unwrap_or_else(|| PathBuf::from("."));
			return Arc::new(UrlScope {
				session_file: Some(path),
				local_roots:  BTreeMap::from([("local".to_string(), local_root)]),
			});
		}
		Arc::clone(&self.url_scope.lock().unwrap_or_else(|poison| poison.into_inner()))
	}

	/// Record the settings isolated child `lsp`/`debug` instances inherit.
	pub fn set_child_tool_settings(&self, settings: ChildToolSettings) {
		*self
			.child_tool_settings
			.lock()
			.unwrap_or_else(|poison| poison.into_inner()) = settings;
	}

	#[must_use]
	pub fn child_tool_settings(&self) -> ChildToolSettings {
		*self
			.child_tool_settings
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
	}

	#[must_use]
	pub fn agent_catalog(&self) -> Option<Arc<AgentCatalog>> {
		self
			.agent_catalog
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
			.clone()
	}

	/// Evict settled records that are older than [`RECORD_RETENTION_MS`] or
	/// beyond [`MAX_SETTLED_RECORDS`], keeping running work, queued
	/// reservations, and undelivered results. Mirrors the TypeScript job
	/// manager's five-minute retention while bounding the long-lived runner.
	fn prune_state(state: &mut SupervisorState) {
		let now = now_ms();
		let pending: HashSet<&str> =
			state.pending.iter().map(|delivery| delivery.id.as_str()).collect();
		let mut candidates: Vec<(u64, String)> = Vec::new();
		for id in &state.order {
			if state.active.contains_key(id) || state.reservations.contains_key(id) {
				continue;
			}
			let Some(record) = state.records.get(id) else { continue };
			if record.status == ChildStatus::Running || pending.contains(id.as_str()) {
				continue;
			}
			candidates.push((state.settled_at.get(id).copied().unwrap_or(now), id.clone()));
		}
		let mut evict: HashSet<String> = candidates
			.iter()
			.filter(|(settled, _)| now.saturating_sub(*settled) > RECORD_RETENTION_MS)
			.map(|(_, id)| id.clone())
			.collect();
		let retained: Vec<&(u64, String)> = candidates
			.iter()
			.filter(|(_, id)| !evict.contains(id))
			.collect();
		if retained.len() > MAX_SETTLED_RECORDS {
			let mut fresh = retained;
			fresh.sort_by_key(|pair| pair.0);
			let excess = fresh.len() - MAX_SETTLED_RECORDS;
			for (_, id) in fresh.into_iter().take(excess) {
				evict.insert(id.clone());
			}
		}
		if evict.is_empty() {
			return;
		}
		state.order.retain(|id| !evict.contains(id));
		for id in &evict {
			state.records.remove(id);
			state.settled_at.remove(id);
			state.consumed.remove(id);
		}
	}

	pub fn limits(&self) -> DelegationLimits {
		*self.limits.lock().unwrap()
	}

	pub fn provider(&self) -> Arc<dyn ModelProvider> {
		self.provider.lock().unwrap().clone()
	}

	pub fn child_approver(&self) -> Option<Arc<dyn ToolApprover>> {
		self.child_approver.lock().unwrap().clone()
	}

	/// Mirror the parent run's `edit` options onto child `EditTool`s.
	pub fn set_edit_options(&self, options: hashline::edit::EditToolOptions) {
		*self.edit_options.lock().unwrap() = options;
	}

	/// Install the host's display/`edit.mode` resolver (CLI settings + env).
	pub fn set_display_resolver(&self, resolver: Arc<dyn SessionDisplayResolver>) {
		*self.display_resolver.lock().unwrap() = Some(resolver);
	}

	/// Root session model (`provider/id`), used when a child inherits the parent
	/// provider so model-variant resolution never falls back to defaults.
	pub fn set_active_model(&self, active_model: impl Into<String>) {
		*self.active_model.lock().unwrap() = Some(active_model.into());
	}

	fn resolve_session_tools(
		&self,
		active_model: Option<&str>,
		has_edit_tool: bool,
	) -> ResolvedSessionTools {
		let model = active_model
			.map(str::to_string)
			.or_else(|| self.active_model.lock().unwrap().clone());
		let resolver = self.display_resolver.lock().unwrap().clone();
		match resolver {
			Some(resolver) => resolver.resolve(model.as_deref(), has_edit_tool),
			None => default_session_tools(has_edit_tool),
		}
	}

	/// Rebuild the display-bearing tools of `registry` for one session model.
	///
	/// Idempotent and allocation-free for absent tools: one snapshot store and
	/// one pending store are created, then `read`, `ast_grep`, `edit`,
	/// `ast_edit`, and `write` are replaced only when mounted. Callers must do
	/// this after any allowlist filtering so `has_edit_tool` reflects the final
	/// set (no anchors for agents without `edit`). `owner` identifies the child
	/// eval session so the eval host re-binds with the fresh child tools; root
	/// calls use `None`.
	pub fn rebind_session_tools(
		&self,
		registry: &mut ToolRegistry,
		active_model: Option<&str>,
		owner: Option<&str>,
	) {
		let has_edit_tool = registry.get("edit").is_some();
		let resolved = self.resolve_session_tools(active_model, has_edit_tool);
		let snapshots = Arc::new(SnapshotStore::new());
		let pending = Arc::new(PendingStore::new());
		let sink = Arc::new(NativeSnapshotSink::new(snapshots.clone()));
		let router = self.internal_urls();
		let url_scope = self.url_scope_for(owner);
		let acp_bridge = self.acp_bridge();

		if registry.get("read").is_some() {
			registry.remove("read");
			let read = ReadTool::with_snapshot_sink(sink.clone())
				.with_display_inputs(resolved.display)
				.with_acp_bridge(acp_bridge.clone());
			registry.register(Arc::new(match &router {
				Some(router) => read.with_internal_urls(Arc::clone(router), Arc::clone(&url_scope)),
				None => read,
			}));
		}
		if registry.get("ast_grep").is_some() {
			registry.remove("ast_grep");
			registry.register(Arc::new(
				AstGrepTool::with_snapshot_sink(sink).with_display_inputs(resolved.display),
			));
		}
		if has_edit_tool {
			let options = *self.edit_options.lock().unwrap();
			registry.remove("edit");
			registry.register(Arc::new(EditTool::with_mode(
				Arc::clone(&snapshots),
				options,
				resolved.edit_tool,
			)));
		}
		if registry.get("ast_edit").is_some() {
			registry.remove("ast_edit");
			registry.register(Arc::new(crate::tools::AstEditTool::with_display_inputs(
				snapshots,
				Arc::clone(&pending),
				resolved.display,
			)));
		}
		if registry.get("write").is_some() {
			registry.remove("write");
			let write = WriteTool::with_pending(Arc::clone(&pending)).with_acp_bridge(acp_bridge);
			registry.register(Arc::new(match &router {
				Some(router) => write.with_internal_urls(Arc::clone(router), Arc::clone(&url_scope)),
				None => write,
			}));
		}
		registry.set_policy(Arc::new(StagedPreviewPolicy::new(pending)));
		if registry.get("eval").is_some()
			&& let Some(binder) = self.eval_binder()
		{
			// Rebind after display/session tools so the eval host's tool-call
			// view contains the child's fresh read/edit/write instances.
			binder.bind(registry, owner);
		}
	}

	pub fn task_enabled(&self) -> bool {
		self.allow_task.load(Ordering::SeqCst)
	}

	pub fn hub_enabled(&self) -> bool {
		self.allow_hub.load(Ordering::SeqCst)
	}

	/// `async.enabled`: default `true`, matching the upstream setting.
	pub fn async_enabled(&self) -> bool {
		self.async_enabled.load(Ordering::SeqCst)
	}

	/// Install/replace the per-turn eval mount. `None` removes eval from child
	/// registries (tool not selected or tools disabled).
	pub fn set_eval_binder(&self, binder: Option<Arc<dyn EvalToolBinder>>) {
		*self.eval_binder.lock().unwrap_or_else(|poison| poison.into_inner()) = binder;
	}

	pub fn eval_binder(&self) -> Option<Arc<dyn EvalToolBinder>> {
		self
			.eval_binder
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
			.clone()
	}

	/// Install/replace the ACP reverse-call bridge for this connection.
	pub fn set_acp_bridge(&self, bridge: Option<Arc<dyn AcpClientBridge>>) {
		*self.acp_bridge.lock().unwrap_or_else(|poison| poison.into_inner()) = bridge;
	}

	pub fn acp_bridge(&self) -> Option<Arc<dyn AcpClientBridge>> {
		self
			.acp_bridge
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
			.clone()
	}

	/// Release eval kernels retained for a settled child owner.
	pub async fn dispose_eval_owner(&self, owner: &str) {
		if let Some(binder) = self.eval_binder() {
			binder.dispose_owner(owner).await;
		}
	}

	/// Rebind a child's eval host with the exact provider its loop resolved.
	/// Without this, `completion(model = "default")` inside a child eval session
	/// would answer with the root session model.
	pub fn rebind_eval_provider(
		&self,
		registry: &mut ToolRegistry,
		owner: &str,
		provider: Arc<dyn ModelProvider>,
	) {
		if registry.get("eval").is_some()
			&& let Some(binder) = self.eval_binder()
		{
			binder.bind_with_provider(registry, Some(owner), Some(provider));
		}
	}

	/// Build the `task` tool used by eval `agent()`. Root usage shares the root
	/// concurrency permits. A child owner gets its real depth, agent definition
	/// (self-spawn/spawns policy), parent tool ceiling, and its own permits, so
	/// eval cannot bypass recursion or delegation rules.
	pub fn eval_task_tool(
		self: &Arc<Self>,
		owner: Option<&str>,
		parent_tools: Option<std::collections::BTreeSet<String>>,
	) -> TaskTool {
		let Some(owner) = owner else {
			return TaskTool::with_semaphore(Arc::clone(self), 0, Arc::clone(&self.root_permits));
		};
		let (depth, agent_name) = {
			let state = self.state.lock().unwrap_or_else(|poison| poison.into_inner());
			let mut depth = 0i32;
			let mut current = Some(owner.to_string());
			let mut agent = None;
			while let Some(id) = current {
				let Some(record) = state.records.get(&id) else {
					break;
				};
				depth = depth.saturating_add(1);
				if agent.is_none() {
					agent = Some(record.agent.clone());
				}
				current = record.parent_id.clone();
			}
			(depth.max(1), agent.unwrap_or_else(|| "task".to_string()))
		};
		let mut tool = TaskTool::with_parent(Arc::clone(self), depth, Some(owner.to_string()));
		tool.agent_name = Some(agent_name.clone());
		if let Some(catalog) = self.agent_catalog()
			&& let Some(definition) = catalog.get(&agent_name)
		{
			tool.spawns = definition.spawns.clone();
		}
		tool.parent_tools = parent_tools;
		tool
	}

	pub fn set_async_enabled(&self, enabled: bool) {
		self.async_enabled.store(enabled, Ordering::SeqCst);
		if let Some(processes) = self.processes.lock().unwrap().as_ref() {
			processes.set_async_enabled(enabled);
		}
	}

	/// Install the run's process backend (local manager or broker client).
	/// Mirrors the current `async.enabled` policy onto it.
	pub fn set_processes(&self, processes: Arc<dyn ProcessBackend>) {
		processes.set_async_enabled(self.async_enabled());
		*self.processes.lock().unwrap() = Some(processes);
	}

	/// Shared backend for `bash async` and hub process ops. Without an injected
	/// backend, a local in-process manager is created on first use and reused
	/// for this supervisor's lifetime.
	/// Backend-visible owner label for a child id, namespaced per runner so
	/// concurrent CLIs never share process ownership for `task-1`.
	fn process_owner(&self, owner: Option<&str>) -> Option<String> {
		owner.map(|id| format!("{}:{id}", self.process_owner_ns))
	}

	pub fn processes(&self) -> Arc<dyn ProcessBackend> {
		let mut slot = self.processes.lock().unwrap();
		if let Some(processes) = slot.as_ref() {
			return Arc::clone(processes);
		}
		let manager: Arc<dyn ProcessBackend> =
			Arc::new(ProcessManager::new(self.process_root.clone()));
		manager.set_async_enabled(self.async_enabled());
		*slot = Some(Arc::clone(&manager));
		manager
	}

	/// True only when background execution has both a policy and a delivery
	/// target. A caller that never registers a sink keeps the synchronous
	/// behavior, which is the upstream "no AsyncJobManager" fallback.
	pub fn background_ready(&self) -> bool {
		self.async_enabled() && self.result_sink_attached.load(Ordering::SeqCst)
	}

	pub fn mailbox(&self) -> Arc<Mailbox> {
		self.mailbox.clone()
	}

	fn mark_result_sink_attached(&self) {
		self.result_sink_attached.store(true, Ordering::SeqCst);
	}

	/// Build a pull-based sink scoped to one owner. Root is `None`; a child
	/// passes its supervisor id so it never consumes a sibling's results.
	pub fn owner_results_sink(self: &Arc<Self>, owner: Option<&str>) -> Arc<dyn BackgroundResultSink> {
		self.mark_result_sink_attached();
		Arc::new(OwnerResultSink {
			supervisor: Arc::clone(self),
			owner: owner.map(str::to_string),
		})
	}

	/// Remove and return every undelivered background result owned by `owner`.
	/// A result is enqueued only after the child settles, so this never yields
	/// a still-running job.
	pub fn take_results_for(&self, owner: Option<&str>) -> Vec<String> {
		self
			.take_deliveries_for(owner)
			.into_iter()
			.map(|delivery| delivery.text)
			.collect()
	}

	/// Rich view of [`Self::take_results_for`]: the same single consumption
	/// (one lock, one `consumed` mark per id) with the identity and reported
	/// usage a root consumer needs. Calling either view drains the other.
	pub fn take_deliveries_for(&self, owner: Option<&str>) -> Vec<BackgroundDelivery> {
		let mut state = self.state.lock().unwrap();
		let mut taken = Vec::new();
		let mut index = 0;
		while index < state.pending.len() {
			if state.pending[index].owner.as_deref() == owner {
				if let Some(delivery) = state.pending.remove(index) {
					state.consumed.insert(delivery.id.clone());
					state.outstanding.remove(&delivery.id);
					state.pending_bytes = state.pending_bytes.saturating_sub(delivery.text.len());
					taken.push(BackgroundDelivery {
						id: delivery.id,
						text: delivery.text,
						usage: delivery.usage,
					});
				}
			} else {
				index += 1;
			}
		}
		taken
	}

	/// Enqueue a settled background result for exactly-once delivery. If the
	/// spawning agent already settled before its nested job finished, route to
	/// the nearest live ancestor (root = `None`) instead of orphaning the result.
	pub fn enqueue_delivery(&self, id: &str, text: String) {
		let mut state = self.state.lock().unwrap();
		// Every queued body is backed by an admission slot; a settled job whose
		// slot was already released (aborted or consumed) must not add mail.
		if state.consumed.contains(id) || !state.outstanding.contains(id) {
			return;
		}
		let mut owner = state.records.get(id).and_then(|record| record.parent_id.clone());
		while let Some(current) = owner.clone() {
			let live = state
				.records
				.get(&current)
				.is_some_and(|record| record.status == ChildStatus::Running);
			if live {
				break;
			}
			owner = state
				.records
				.get(&current)
				.and_then(|record| record.parent_id.clone());
		}
		let (mut text, _) = cap_result_text(&text);
		let remaining = MAX_PENDING_RESULT_BYTES.saturating_sub(state.pending_bytes);
		if text.len() > remaining {
			text = truncate_to_bytes(&text, remaining);
		}
		let usage = state.records.get(id).and_then(|record| record.usage);
		state.pending_bytes += text.len();
		state
			.pending
			.push_back(PendingDelivery { id: id.to_string(), owner, text, usage });
	}

	/// Enqueue a non-task background result (eval auto-background) scoped to an
	/// owner. Unlike [`Self::enqueue_delivery`] the id has no supervisor
	/// admission slot, so this is the host's exactly-once entry point: a
	/// consumed or already-queued id is rejected. A settled owner routes to its
	/// nearest live ancestor (root = `None`).
	pub fn enqueue_owner_result(&self, owner: Option<&str>, id: &str, text: String) -> bool {
		let mut state = self.state.lock().unwrap_or_else(|poison| poison.into_inner());
		if state.consumed.contains(id)
			|| state.pending.iter().any(|delivery| delivery.id == id)
		{
			return false;
		}
		let mut owner = owner.map(str::to_string);
		while let Some(current) = owner.clone() {
			let live = state
				.records
				.get(&current)
				.is_some_and(|record| record.status == ChildStatus::Running);
			if live {
				break;
			}
			owner = state
				.records
				.get(&current)
				.and_then(|record| record.parent_id.clone());
		}
		let (mut text, _) = cap_result_text(&text);
		let remaining = MAX_PENDING_RESULT_BYTES.saturating_sub(state.pending_bytes);
		if remaining == 0 {
			return false;
		}
		if text.len() > remaining {
			text = truncate_to_bytes(&text, remaining);
		}
		if text.is_empty() {
			return false;
		}
		state.pending_bytes += text.len();
		state.pending.push_back(PendingDelivery {
			id: id.to_string(),
			owner,
			text,
			usage: None,
		});
		true
	}

	/// Atomically consume a set of settled results for a snapshot. Returns the
	/// ids that were *already* delivered before this call, so the snapshot can
	/// hide their bodies; every id is marked consumed and its queued copy is
	/// removed under one lock. This closes the race where the loop drains a
	/// result between a hub status check and a per-id consume.
	pub fn consume_results(&self, ids: &[String]) -> HashSet<String> {
		let mut state = self.state.lock().unwrap();
		let mut already = HashSet::new();
		let mut removed_any = false;
		for id in ids {
			if state.consumed.contains(id) {
				already.insert(id.clone());
			}
			let removed_bytes: usize = state
				.pending
				.iter()
				.filter(|delivery| &delivery.id == id)
				.map(|delivery| delivery.text.len())
				.sum();
			let before = state.pending.len();
			state.pending.retain(|delivery| &delivery.id != id);
			removed_any |= state.pending.len() != before;
			state.pending_bytes = state.pending_bytes.saturating_sub(removed_bytes);
			state.outstanding.remove(id);
			state.consumed.insert(id.clone());
		}
		if removed_any {
			drop(state);
			self.settle.notify_waiters();
		}
		already
	}

	/// Cancel queued reservations and abort every live child. Runtime owners
	/// call this when the runner is dropped: background tasks hold their own
	/// `Arc<Supervisor>`, so relying on `Drop` alone would let them outlive the
	/// runner. Safe to call more than once.
	pub fn shutdown(&self) {
		let (reservations, active) = {
			let mut state = match self.state.lock() {
				Ok(state) => state,
				Err(poisoned) => poisoned.into_inner(),
			};
			state.pending.clear();
			(state.reservations.drain().map(|(_, token)| token).collect::<Vec<_>>(),
			 state.active.drain().map(|(_, child)| child).collect::<Vec<_>>())
		};
		for reservation in reservations {
			reservation.cancel();
		}
		for active in active {
			// Cancel first so shell tools can unwind their process trees, then
			// abort the task as the bounded fallback.
			active.cancellation_token.cancel();
			let _ = active.steering_tx.try_send(SteeringEvent::Interrupt);
			active.abort_handle.abort();
		}
		self.settle.notify_waiters();
	}

	pub fn settle_signal(&self) -> tokio::sync::futures::Notified<'_> {
		self.settle.notified()
	}

	/// True when `id` names a supervisor child or the root agent id.
	pub fn is_known_agent(&self, id: &str) -> bool {
		id == crate::mailbox::MAIN_AGENT_ID || self.state.lock().unwrap().records.contains_key(id)
	}

	pub fn running_agent_ids(&self) -> Vec<String> {
		self.state.lock().unwrap().active.keys().cloned().collect()
	}

	/// Hand a peer message to a live child. Only an actual `try_send` success
	/// is reported as injected; anything else is an error the caller turns into
	/// a failed receipt plus a buffered mailbox copy.
	pub fn inject_peer_message(&self, id: &str, text: String) -> Result<(), String> {
		let state = self.state.lock().unwrap();
		let Some(record) = state.records.get(id) else {
			return Err(format!(
				"Unknown agent \"{id}\" — check `hub` list for live peers."
			));
		};
		if record.status == ChildStatus::Aborted {
			return Err(format!(
				"Agent \"{id}\" was hard-aborted and cannot be messaged or revived."
			));
		}
		match state.active.get(id) {
			Some(active) => active
				.steering_tx
				.try_send(SteeringEvent::InjectMessage(vec![
					crate::state::ContentPart::text(text),
				]))
				.map_err(|_| format!("Agent \"{id}\" has no live session.")),
			None => Err(format!("Agent \"{id}\" has no live session.")),
		}
	}

	pub fn root_registry(self: &Arc<Self>) -> ToolRegistry {
		let mut reg = self.template.lock().unwrap().clone();
		let processes = self.processes();
		if reg.get("bash").is_some() {
			reg.remove("bash");
			reg.register(Arc::new(BashTool::with_processes(
				Arc::clone(&processes),
				None,
			)));
		}
		reg.register(Arc::new(ThinkTool::new()));
		let limits = self.limits();
		if self.task_enabled() && (limits.max_depth < 0 || 0 < limits.max_depth) {
			reg.register(Arc::new(TaskTool::with_semaphore(
				Arc::clone(self),
				0,
				Arc::clone(&self.root_permits),
			)));
		}
		if self.hub_enabled() {
			reg.register(Arc::new(
				HubTool::root(Arc::clone(self)).with_processes(processes, None),
			));
		}
		reg
	}

	pub fn child_registry(self: &Arc<Self>, depth: i32, owner: Option<&str>) -> ToolRegistry {
		let mut reg = self.child_registry_unbound(depth, owner);
		// Direct callers (tests, host convenience) get a fully bound child.
		// `task` instead filters its allowlist first and rebinds exactly once
		// with the child's effective model, so no store is allocated twice.
		self.rebind_session_tools(&mut reg, None, owner);
		reg
	}

	/// Child registry with parent display-bearing tools still attached: callers
	/// MUST finish with [`Supervisor::rebind_session_tools`] after allowlist
	/// filtering.
	pub(crate) fn child_registry_unbound(self: &Arc<Self>, depth: i32, owner: Option<&str>) -> ToolRegistry {
		let mut reg = self.template.lock().unwrap().clone();
		// `read`/`edit`/`ast_grep`/`ast_edit`/`write` are session-scoped: a
		// child gets its own snapshot store, `CUT` registers, no-op guard,
		// pending queue, and read provenance. `rebind_session_tools` replaces
		// them after the whitelist so the child model and final tool set are
		// known; until then the template's copies are placeholders.
		// `todo` is parent-owned: the template shares one live `Arc<TodoTool>`
		// instance, and child events/session details are not persisted. Dropping
		// it here prevents descendants from mutating the parent list.
		reg.remove("todo");
		// Children are headless: interactive ask has no channel.
		reg.remove("ask");
		// One backend snapshot for every child tool: a concurrent root swap
		// must not give `bash` and `hub` different process scopes.
		let processes = self.processes();
		if reg.get("bash").is_some() {
			reg.remove("bash");
			let backend_owner = self.process_owner(owner);
			reg.register(Arc::new(
				BashTool::with_processes(Arc::clone(&processes), backend_owner)
					.with_acp_bridge(self.acp_bridge()),
			));
		}
		reg.register(Arc::new(YieldTool::new()));
		reg.register(Arc::new(ThinkTool::new()));
		// The template's `lsp`/`debug` instances belong to the root: the LSP tool
		// holds the root's shared ownership scope and the debug tool the root's
		// session tree. A child must neither drive the parent's debuggee nor keep
		// the parent's owned LSP servers open, so it gets isolated instances with
		// the same settings (`lsp.shared`, `tools.maxTimeout`).
		let child_tools = self.child_tool_settings();
		if reg.get("lsp").is_some() {
			reg.remove("lsp");
			reg.register(Arc::new(LspTool::with_shared(child_tools.lsp_shared)));
		}
		if reg.get("debug").is_some() {
			reg.remove("debug");
			reg.register(Arc::new(DapTool::new().with_max_timeout(child_tools.debug_max_timeout)));
		}
		// Add eval before the task tool filters the child allowlist, so `eval`
		// is matched by internal registry key. `rebind_session_tools` replaces
		// it once the child's fresh session tools exist.
		if let Some(binder) = self.eval_binder() {
			binder.bind(&mut reg, owner);
		}
		let limits = self.limits();
		if self.task_enabled() && (limits.max_depth < 0 || depth < limits.max_depth) {
			reg.register(Arc::new(TaskTool::with_parent(
				Arc::clone(self),
				depth,
				owner.map(str::to_string),
			)));
		}
		if self.hub_enabled() {
			let backend_owner = self.process_owner(owner);
			reg.register(Arc::new(
				HubTool::with_owner(Arc::clone(self), owner.map(str::to_string))
					.with_processes(processes, backend_owner),
			));
		}
		reg
	}

	pub fn next_seq(&self) -> u64 {
		self.seq.fetch_add(1, Ordering::SeqCst) + 1
	}

	pub fn children(&self) -> Vec<ChildRecord> {
		let mut state = self.state.lock().unwrap();
		Self::prune_state(&mut state);
		state
			.order
			.iter()
			.filter_map(|id| state.records.get(id).cloned())
			.collect()
	}

	/// Abort a running child. With `owner`, only a direct child of that agent is
	/// reachable, so a subagent cannot cancel its siblings or the root's work.
	///
	/// Cancellation is cooperative first: the child's own token is cancelled and
	/// we wait for the loop to settle so in-flight tools can terminate their
	/// subprocesses. Aborting the task is only the bounded fallback.
	pub async fn cancel_child(&self, id: &str, owner: Option<&str>) -> CancelAttempt {
		let (active, reservation) = {
			let mut state = self.state.lock().unwrap();
			let (visible, status) = match state.records.get(id) {
				Some(record) => (
					owner.is_none_or(|owner| record.parent_id.as_deref() == Some(owner)),
					record.status,
				),
				None => return CancelAttempt::NotFound,
			};
			if !visible {
				return CancelAttempt::NotFound;
			}
			if status != ChildStatus::Running {
				return CancelAttempt::AlreadyCompleted;
			}
			let active = state.active.get(id).cloned();
			let reservation = state.reservations.get(id).cloned();
			if active.is_none() && reservation.is_none() {
				return CancelAttempt::AlreadyCompleted;
			}
			if let Some(record) = state.records.get_mut(id) {
				record.abort_reason = Some("cancelled by hub".to_string());
			}
			(active, reservation)
		};

		if let Some(active) = active {
			active.cancellation_token.cancel();
			let _ = active.steering_tx.try_send(SteeringEvent::Interrupt);
			if tokio::time::timeout(CANCEL_SETTLE_TIMEOUT, self.wait_settled(id))
				.await
				.is_err()
			{
				active.abort_handle.abort();
				self.mark_aborted(id, "cancelled by hub");
			}
		} else if let Some(token) = reservation {
			// Still queued for a permit: cancel the reservation so the child never
			// starts, and wait for its queued future to acknowledge and settle.
			token.cancel();
			if tokio::time::timeout(CANCEL_SETTLE_TIMEOUT, self.wait_settled(id))
				.await
				.is_err()
			{
				// The reservation may have been promoted to an active handle while
				// we waited (the task only checks the token after acquiring the
				// permit). Abort that handle instead of marking an Aborted record
				// while the child keeps running.
				let late_active = {
					let state = self.state.lock().unwrap();
					state.active.get(id).cloned()
				};
				if let Some(active) = late_active {
					active.cancellation_token.cancel();
					let _ = active.steering_tx.try_send(SteeringEvent::Interrupt);
					active.abort_handle.abort();
					let _ = tokio::time::timeout(CANCEL_SETTLE_TIMEOUT, self.wait_settled(id)).await;
				}
				self.mark_aborted(id, "cancelled by hub");
			}
			self.unregister_active_child(id);
		}

		// The loop records its own reason on interrupt; hub cancellation is the
		// authoritative cause once we observed settlement.
		let mut state = self.state.lock().unwrap();
		if let Some(record) = state.records.get_mut(id)
			&& record.status == ChildStatus::Aborted
		{
			record.abort_reason = Some("cancelled by hub".to_string());
		}
		CancelAttempt::Cancelled
	}

	async fn wait_settled(&self, id: &str) {
		loop {
			// Enable the waiter before inspecting status, so a terminal event
			// between the check and the await cannot be missed.
			let mut notified = Box::pin(self.settle.notified());
			notified.as_mut().enable();
			let running = {
				let state = self.state.lock().unwrap();
				state
					.records
					.get(id)
					.is_some_and(|record| record.status == ChildStatus::Running)
			};
			if !running {
				return;
			}
			notified.await;
		}
	}

	/// Reserve an immutable identity before spawning. The supplied name is kept
	/// only as a label, so duplicate names never overwrite each other's records
	/// or abort handles.
	pub fn reserve_child(
		&self,
		name: &str,
		index: usize,
		agent: &str,
		task: &str,
		parent_id: Option<&str>,
	) -> Option<(String, CancellationToken)> {
		let mut spawn = ChildSpawn::new(name, index, agent, task);
		spawn.parent_id = parent_id;
		self.reserve_child_full(spawn)
	}

	/// Reserve with the fields observability needs beyond identity. The child
	/// transcript is created here so `sessionFile` on the `started` event is a
	/// real path the moment it is announced.
	pub fn reserve_child_full(&self, spawn: ChildSpawn<'_>) -> Option<(String, CancellationToken)> {
		let _emission = self.emission_gate();
		let (id, cancellation_token, record) = {
			let mut state = self.state.lock().unwrap();
			Self::prune_state(&mut state);
			if state.outstanding.len() >= MAX_OUTSTANDING_RESULTS {
				return None;
			}
			let id = format!("task-{}", self.next_seq());
			let cancellation_token = CancellationToken::new();
			let started_at = SystemTime::now()
				.duration_since(UNIX_EPOCH)
				.map(|d| d.as_millis() as u64)
				.ok();
			let session_file = self.allocate_child_session(&id);
			let record = ChildRecord {
				id: id.clone(),
				name: spawn.name.to_string(),
				index: spawn.index,
				agent: spawn.agent.to_string(),
				task: spawn.task.to_string(),
				status: ChildStatus::Running,
				parent_id: spawn.parent_id.map(str::to_string),
				started_at,
				duration_ms: None,
				output: None,
				error: None,
				aborted: None,
				abort_reason: None,
				session_file,
				agent_source: spawn.agent_source.to_string(),
				parent_tool_call_id: spawn.parent_tool_call_id.map(str::to_string),
				detached: spawn.detached,
				usage: None,
			};
			state.order.push(id.clone());
			state.records.insert(id.clone(), record.clone());
			state
				.reservations
				.insert(id.clone(), cancellation_token.clone());
			state.outstanding.insert(id.clone());
			(id, cancellation_token, record)
		};
		self.emit(SubagentEvent::Lifecycle(lifecycle_of(
			&record,
			SubagentLifecycleStatus::Started,
		)));
		Some((id, cancellation_token))
	}

	/// Install the run's `task.isolation.*` policy. Called by the CLI before
	/// building the root registry, so every new `TaskTool` snapshots it.
	pub fn set_task_isolation(&self, settings: TaskIsolationSettings) {
		*self
			.task_isolation
			.lock()
			.unwrap_or_else(|poison| poison.into_inner()) = settings;
	}

	pub fn task_isolation(&self) -> TaskIsolationSettings {
		self
			.task_isolation
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
			.clone()
	}

	/// Directory child transcripts are created in; `None` disables them.
	pub fn set_child_session_root(&self, root: Option<PathBuf>) {
		*self
			.child_session_root
			.lock()
			.unwrap_or_else(|poison| poison.into_inner()) = root;
	}

	/// Real transcript log for one child, when a session root was installed.
	#[must_use]
	pub fn child_session_file(&self, id: &str) -> Option<PathBuf> {
		let state = self.state.lock().unwrap();
		state.records.get(id).and_then(|record| record.session_file.clone())
	}

	/// Append real child messages to that child's transcript log.
	///
	/// The path comes from the child's record — never a guess — and a write
	/// failure is reported on stderr rather than replacing the child result.
	pub fn append_child_transcript(&self, id: &str, messages: &[&Message]) {
		let Some(path) = self.child_session_file(id) else {
			return;
		};
		let mut file = match std::fs::OpenOptions::new()
			.create(true)
			.append(true)
			.open(&path)
		{
			Ok(file) => file,
			Err(error) => {
				eprintln!(
					"warning: cannot open child transcript {}: {error}",
					path.display()
				);
				return;
			},
		};
		for message in messages {
			let Ok(message) = serde_json::to_value(message) else {
				continue;
			};
			let line = serde_json::json!({ "type": "message", "message": message });
			if let Err(error) = writeln!(file, "{line}") {
				eprintln!(
					"warning: cannot append child transcript {}: {error}",
					path.display()
				);
				return;
			}
		}
	}

	fn allocate_child_session(&self, id: &str) -> Option<PathBuf> {
		let root = self
			.child_session_root
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
			.clone()?;
		let path = root.join(format!("{id}.jsonl"));
		if let Some(parent) = path.parent() {
			let _ = std::fs::create_dir_all(parent);
		}
		// An empty log is a real, readable transcript: paging returns no
		// entries until the executor appends the child's actual messages.
		let _ = std::fs::OpenOptions::new()
			.create(true)
			.append(true)
			.open(&path);
		Some(path)
	}

	/// Subscribe an observability consumer for the supervisor's lifetime (or
	/// until the returned guard drops).
	pub fn subscribe(self: &Arc<Self>, observer: Arc<dyn SubagentObserver>) -> ObserverGuard {
		let id = self.observer_seq.fetch_add(1, Ordering::SeqCst) + 1;
		self
			.observers
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
			.push((id, observer));
		ObserverGuard { supervisor: Arc::downgrade(self), id }
	}

	/// Idempotent subscription for callers that see the supervisor once per
	/// turn: a second call with the same observer is a no-op. Returns `true`
	/// when this call installed it.
	pub fn subscribe_once(self: &Arc<Self>, observer: &Arc<dyn SubagentObserver>) -> bool {
		{
			let observers = self
				.observers
				.lock()
				.unwrap_or_else(|poison| poison.into_inner());
			if observers
				.iter()
				.any(|(_, installed)| Arc::ptr_eq(installed, observer))
			{
				return false;
			}
		}
		let id = self.observer_seq.fetch_add(1, Ordering::SeqCst) + 1;
		self
			.observers
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
			.push((id, Arc::clone(observer)));
		true
	}

	/// Report real progress for one running child. Callers pass an observed
	/// transition (the child actually started running); settled or unknown ids
	/// are ignored so no event outlives its record.
	pub fn report_progress(&self, id: &str, status: &str, description: Option<&str>) -> bool {
		let _emission = self.emission_gate();
		let record = {
			let state = self.state.lock().unwrap();
			state.records.get(id).cloned()
		};
		let Some(record) = record else {
			return false;
		};
		if record.status != ChildStatus::Running {
			return false;
		}
		self.emit(SubagentEvent::Progress(SubagentProgress {
			id:                  record.id.clone(),
			index:               record.index,
			agent:               record.agent.clone(),
			agent_source:        record.agent_source.clone(),
			task:                record.task.clone(),
			parent_tool_call_id: record.parent_tool_call_id.clone(),
			session_file:        record
				.session_file
				.as_ref()
				.map(|path| path.display().to_string()),
			detached:            record.detached,
			status:              status.to_string(),
			description:         description.map(str::to_string),
		}));
		true
	}

	/// Forward one real event from a running child's loop to observers. Events
	/// for settled or unknown ids are dropped, so no frame outlives its record.
	pub fn report_event(&self, id: &str, event: AgentEvent) -> bool {
		let _emission = self.emission_gate();
		let live = {
			let state = self.state.lock().unwrap();
			state
				.records
				.get(id)
				.is_some_and(|record| record.status == ChildStatus::Running)
		};
		if !live {
			return false;
		}
		self.emit(SubagentEvent::Event { id: id.to_string(), event });
		true
	}

	/// Emission gate. Taken before `state` and held until the frames are handed
	/// to observers, so a state decision and its frames cannot interleave with
	/// another child transition. Observers run under this gate (never under
	/// `state`), so they must not call back into an emitting method.
	fn emission_gate(&self) -> MutexGuard<'_, ()> {
		self.emission.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
	}

	fn emit(&self, event: SubagentEvent) {
		let observers: Vec<Arc<dyn SubagentObserver>> = self
			.observers
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
			.iter()
			.map(|(_, observer)| Arc::clone(observer))
			.collect();
		for observer in observers {
			observer.observe(&event);
		}
	}

	/// Free result slots for jobs whose inline/sync result was returned or
	/// whose aborted run never queued a body. Idempotent.
	pub fn release_outstanding(&self, id: &str) {
		let mut state = self.state.lock().unwrap();
		state.outstanding.remove(id);
	}

	/// Release a slot only when the settled job has no queued, undelivered
	/// body. Used by the background lifecycle guard when a panic unwinds
	/// between settlement and enqueue.
	pub fn release_outstanding_if_undelivered(&self, id: &str) {
		let mut state = self.state.lock().unwrap();
		if !state.pending.iter().any(|delivery| delivery.id == id) {
			state.outstanding.remove(id);
		}
	}

	/// Remaining admission capacity for new results.
	pub fn outstanding_capacity(&self) -> usize {
		let state = self.state.lock().unwrap();
		MAX_OUTSTANDING_RESULTS.saturating_sub(state.outstanding.len())
	}

	pub fn register_active_child(
		&self,
		id: &str,
		steering_tx: mpsc::Sender<SteeringEvent>,
		abort_handle: tokio::task::AbortHandle,
		cancellation_token: CancellationToken,
	) -> bool {
		let mut state = self.state.lock().unwrap();
		// Cancellation may have won between acquiring the permit and spawning.
		if cancellation_token.is_cancelled()
			|| !state
				.records
				.get(id)
				.is_some_and(|record| record.status == ChildStatus::Running)
		{
			return false;
		}
		state.active.insert(
			id.to_string(),
			ActiveChild { steering_tx, abort_handle, cancellation_token },
		);
		state.reservations.remove(id);
		true
	}

	pub fn unregister_active_child(&self, id: &str) {
		let changed = {
			let mut state = self.state.lock().unwrap();
			let active = state.active.remove(id).is_some();
			let reserved = state.reservations.remove(id).is_some();
			active || reserved
		};
		// Peer waits treat the active map as liveness; missing this wake leaves
		// an unbounded `hub wait` asleep after the last peer disappeared.
		if changed {
			self.settle.notify_waiters();
		}
	}

	pub fn mark_completed(&self, id: &str, duration_ms: u64, output: &str) {
		self.settle_child(id, duration_ms, Some(output), None, None);
	}

	/// Complete a job while retaining the child's reported usage. The caller
	/// passes the same counters it renders into the delivered envelope, so the
	/// record and the text never disagree.
	pub fn mark_completed_with_usage(
		&self,
		id: &str,
		duration_ms: u64,
		output: &str,
		usage: Option<TokenUsage>,
	) {
		self.settle_child(id, duration_ms, Some(output), None, usage);
	}

	pub fn mark_failed(&self, id: &str, duration_ms: u64, error: &str) {
		self.settle_child(id, duration_ms, None, Some(error), None);
	}

	/// Fail a job after it already reported usage. Turns that ran before the
	/// failure keep contributing, exactly like the blocking `task` result.
	pub fn mark_failed_with_usage(
		&self,
		id: &str,
		duration_ms: u64,
		error: &str,
		usage: Option<TokenUsage>,
	) {
		self.settle_child(id, duration_ms, None, Some(error), usage);
	}

	pub fn mark_aborted(&self, id: &str, reason: &str) -> bool {
		let _emission = self.emission_gate();
		let (changed, record) = {
			let mut state = self.state.lock().unwrap();
			let changed = match state.records.get_mut(id) {
				Some(record) if record.status == ChildStatus::Running => {
					record.status = ChildStatus::Aborted;
					record.aborted = Some(true);
					record.abort_reason = Some(reason.to_string());
					true
				},
				_ => false,
			};
			if changed {
				state.settled_at.insert(id.to_string(), now_ms());
				state.outstanding.remove(id);
			}
			let record = state.records.get(id).cloned();
			(changed, record)
		};
		if changed {
			if let Some(record) = record {
				self.emit(SubagentEvent::Lifecycle(lifecycle_of(
					&record,
					SubagentLifecycleStatus::Aborted,
				)));
			}
			self.sweep_owner_deliveries(id);
			self.settle.notify_waiters();
		}
		changed
	}

	fn settle_child(
		&self,
		id: &str,
		duration_ms: u64,
		output: Option<&str>,
		error: Option<&str>,
		usage: Option<TokenUsage>,
	) {
		let _emission = self.emission_gate();
		let (changed, record) = {
			let mut state = self.state.lock().unwrap();
			let changed = match state.records.get_mut(id) {
				Some(record) if record.status == ChildStatus::Running => {
					record.duration_ms = Some(duration_ms);
					match (output, error) {
						(Some(output), None) => {
							record.status = ChildStatus::Completed;
							record.output = Some(cap_result_text(output).0);
						},
						(None, Some(error)) => {
							record.status = ChildStatus::Failed;
							record.error = Some(cap_result_text(error).0);
						},
						_ => return,
					}
					record.usage = usage;
					true
				},
				_ => false,
			};
			if changed {
				state.settled_at.insert(id.to_string(), now_ms());
			}
			let record = state.records.get(id).cloned();
			(changed, record)
		};
		if changed {
			if let Some(record) = record {
				let status = match record.status {
					ChildStatus::Completed => SubagentLifecycleStatus::Completed,
					ChildStatus::Failed => SubagentLifecycleStatus::Failed,
					ChildStatus::Aborted => SubagentLifecycleStatus::Aborted,
					ChildStatus::Running => SubagentLifecycleStatus::Started,
				};
				self.emit(SubagentEvent::Lifecycle(lifecycle_of(&record, status)));
			}
			self.sweep_owner_deliveries(id);
			self.settle.notify_waiters();
		}
	}

	/// A child that settles before draining its own nested background results
	/// hands them to its parent so no finished work disappears. Exactly-once is
	/// preserved: entries are moved, not copied.
	fn sweep_owner_deliveries(&self, owner: &str) {
		let mut state = self.state.lock().unwrap();
		let parent = state
			.records
			.get(owner)
			.and_then(|record| record.parent_id.clone());
		for delivery in &mut state.pending {
			if delivery.owner.as_deref() == Some(owner) {
				delivery.owner = parent.clone();
			}
		}
	}
}

/// Cancellation may take up to the shell's own graceful-kill window; after this
/// bound the task is aborted as a last resort.
const CANCEL_SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

impl Drop for Supervisor {
	fn drop(&mut self) {
		self.shutdown();
	}
}

pub struct ChildGuard {
	pub handle: tokio::task::AbortHandle,
	pub steering_tx: mpsc::Sender<SteeringEvent>,
	supervisor: Arc<Supervisor>,
	id: String,
	cancellation_token: CancellationToken,
	defused: bool,
}

impl ChildGuard {
	pub fn new(
		handle: tokio::task::AbortHandle,
		steering_tx: mpsc::Sender<SteeringEvent>,
		supervisor: Arc<Supervisor>,
		id: String,
		cancellation_token: CancellationToken,
	) -> Self {
		Self {
			handle,
			steering_tx,
			supervisor,
			id,
			cancellation_token,
			defused: false,
		}
	}

	pub fn defuse(&mut self) {
		self.defused = true;
	}
}

impl Drop for ChildGuard {
	fn drop(&mut self) {
		if !self.defused {
			self.cancellation_token.cancel();
			let _ = self.steering_tx.try_send(SteeringEvent::Interrupt);
			self.handle.abort();
			self.supervisor.mark_aborted(&self.id, "aborted by parent");
			self.supervisor.unregister_active_child(&self.id);
		}
	}
}

#[cfg(test)]
mod process_wiring_tests {
	use std::sync::Arc;

	use async_trait::async_trait;

	use super::*;
	use crate::{
		provider::{ChunkStream, ModelProvider, ProviderError, ToolChoice},
		state::{AgentContext, Message, ToolResult},
		tools::{PendingAction, ToolError, process::ProcessManager, registry::ToolRegistry, Tool},
	};

	struct Stage(std::path::PathBuf);

	#[async_trait]
	impl PendingAction for Stage {
		fn label(&self) -> &str {
			"staged file"
		}

		fn source_tool(&self) -> &str {
			"ast_edit"
		}

		async fn apply(&self, _ctx: &AgentContext, reason: &str) -> Result<ToolResult, ToolError> {
			std::fs::write(&self.0, reason).map_err(|err| ToolError::ExecutionFailed(err.to_string()))?;
			Ok(ToolResult {
				tool_call_id: String::new(),
				content:       "applied".to_string(),
				is_error:      false,
				terminate:     false,
				details:       None,
			})
		}
	}

	struct QuietProvider;

	#[async_trait]
	impl ModelProvider for QuietProvider {
		async fn complete(
			&self,
			_messages: &[Message],
			_tools: &[serde_json::Value],
			_tool_choice: &ToolChoice,
		) -> Result<Message, ProviderError> {
			Err(ProviderError::Protocol("unused".to_string()))
		}

		async fn stream(
			&self,
			_messages: &[Message],
			_tools: &[serde_json::Value],
			_tool_choice: &ToolChoice,
		) -> Result<ChunkStream, ProviderError> {
			Err(ProviderError::Protocol("unused".to_string()))
		}
	}

	fn supervisor() -> Arc<Supervisor> {
		Supervisor::with_selection(
			ToolRegistry::with_builtins(),
			Arc::new(QuietProvider),
			None,
			DelegationLimits::default(),
			true,
			true,
		)
	}

	#[test]
	fn child_registry_owner_namespace_is_unique() {
		let one = supervisor();
		let two = supervisor();
		let backend: Arc<dyn ProcessBackend> =
			Arc::new(ProcessManager::new(std::env::temp_dir().join("omp-sup-test")));
		one.set_processes(Arc::clone(&backend));

		assert!(one.root_registry().get("bash").is_some());
		let child = one.child_registry(1, Some("task-1"));
		assert!(child.get("bash").is_some());
		assert!(child.get("hub").is_some());

		let first = one.process_owner(Some("task-1"));
		assert_eq!(first, Some(format!("{}:task-1", one.process_owner_ns)));
		assert_ne!(first, two.process_owner(Some("task-1")));
	}

	#[test]
	fn child_registry_gets_isolated_lsp_scope_and_debug_tree() {
		let mut template = ToolRegistry::new();
		template.register(Arc::new(LspTool::with_shared(false)));
		template.register(Arc::new(DapTool::new()));
		let supervisor = Supervisor::with_selection(
			template.clone(),
			Arc::new(QuietProvider),
			None,
			DelegationLimits::default(),
			true,
			true,
		);
		supervisor.set_child_tool_settings(ChildToolSettings {
			lsp_shared:     true,
			debug_max_timeout: Some(45),
		});

		let root_lsp = template.get("lsp").expect("template lsp");
		let root_debug = template.get("debug").expect("template debug");
		let first = supervisor.child_registry(1, Some("task-1"));
		let first_lsp = first.get("lsp").expect("child lsp");
		let first_debug = first.get("debug").expect("child debug");
		assert!(
			!Arc::ptr_eq(&root_lsp, &first_lsp),
			"a child must not join the root LSP ownership scope"
		);
		assert!(
			!Arc::ptr_eq(&root_debug, &first_debug),
			"a child must not see or terminate the root debug session tree"
		);

		let second = supervisor.child_registry(1, Some("task-2"));
		let second_debug = second.get("debug").expect("second child debug");
		assert!(
			!Arc::ptr_eq(&first_debug, &second_debug),
			"sibling children must not share one debug session tree"
		);
	}

	#[test]
	fn root_registry_keeps_the_template_lsp_scope_and_debug_tree() {
		let mut template = ToolRegistry::new();
		template.register(Arc::new(LspTool::with_shared(true)));
		template.register(Arc::new(DapTool::new()));
		let supervisor = Supervisor::with_selection(
			template.clone(),
			Arc::new(QuietProvider),
			None,
			DelegationLimits::default(),
			true,
			true,
		);
		let root = supervisor.root_registry();
		assert!(
			Arc::ptr_eq(
				&template.get("debug").expect("template debug"),
				&root.get("debug").expect("root debug"),
			),
			"root turns must keep one debug session tree"
		);
		assert!(
			Arc::ptr_eq(
				&template.get("lsp").expect("template lsp"),
				&root.get("lsp").expect("root lsp"),
			),
			"root turns must keep one LSP ownership scope"
		);
	}

	#[tokio::test]
	async fn child_registry_snapshots_backend_across_supervisor_swap() {
		let one = supervisor();
		let root_a = std::env::temp_dir().join(format!("omp-sup-consumer-a-{}", std::process::id()));
		let root_b = std::env::temp_dir().join(format!("omp-sup-consumer-b-{}", std::process::id()));
		one.set_processes(Arc::new(ProcessManager::new(root_a)));
		let child = one.child_registry(1, Some("task-1"));
		let bash = child.get("bash").expect("bash");
		let hub = child.get("hub").expect("hub");

		// Root swaps to a different backend after the child snapshot exists.
		one.set_processes(Arc::new(ProcessManager::new(root_b)));

		let ctx = crate::state::AgentContext::new(std::env::temp_dir());
		let started = bash
			.execute(
				&serde_json::json!({"command": "printf CHILD-MARKER", "async": true}),
				&ctx,
			)
			.await
			.expect("async bash start");
		assert!(!started.is_error, "{}", started.content);
		let job = started
			.details
			.as_ref()
			.and_then(|details| details["async"]["jobId"].as_str())
			.expect("job id")
			.to_string();

		// The captured backend serves the job: wait + logs observe the marker.
		let waited = hub
			.execute(
				&serde_json::json!({"op": "wait", "name": job, "timeoutMs": 5000}),
				&ctx,
			)
			.await
			.expect("hub wait");
		assert!(!waited.is_error, "wait: {}", waited.content);
		let logs = hub
			.execute(
				&serde_json::json!({"op": "logs", "name": job, "grep": "CHILD-MARKER"}),
				&ctx,
			)
			.await
			.expect("hub logs");
		assert!(logs.content.contains("CHILD-MARKER"), "logs: {}", logs.content);

		// A registry captured after the swap sees the new backend, which has no
		// such job: job tables are scoped to one backend snapshot.
		let swapped = one.child_registry(1, Some("task-1"));
		let missing = match swapped
			.get("hub")
			.expect("hub")
			.execute(&serde_json::json!({"op": "logs", "name": job}), &ctx)
			.await
		{
			Ok(result) => result.is_error,
			Err(_) => true,
		};
		assert!(missing, "swapped backend must not resolve the old job");
	}

	#[tokio::test]
	async fn child_registry_gets_a_fresh_pending_store() {
		let dir = std::env::temp_dir().join(format!("omp-sup-pending-{}", std::process::id()));
		std::fs::create_dir_all(&dir).expect("mkdir");
		let target = dir.join("out.txt");
		let root_store = Arc::new(PendingStore::new());
		let mut registry = ToolRegistry::new();
		registry.register(Arc::new(WriteTool::with_pending(root_store.clone())));
		let supervisor = Supervisor::with_selection(
			registry,
			Arc::new(QuietProvider),
			None,
			DelegationLimits::default(),
			true,
			true,
		);
		root_store.register(Arc::new(Stage(target.clone())));

		let ctx = AgentContext::new(dir.clone());
		let write = |registry: ToolRegistry, dir: std::path::PathBuf| {
			let tool = registry.get("write").expect("write");
			let ctx = AgentContext::new(dir);
			async move {
				tool.execute(&serde_json::json!({"path": "xd://resolve", "content": "go"}), &ctx)
					.await
			}
		};

		// The child's fresh queue cannot resolve the root's staged action, and
		// its soft-requirement policy points at that same child queue.
		let child_registry = supervisor.child_registry(1, Some("child"));
		let child_policy = child_registry.policy().expect("child policy");
		assert!(
			crate::state::ToolPolicy::requirement(child_policy.as_ref()).is_none(),
			"child policy must peek the child queue, never the root preview"
		);
		let child = write(child_registry, dir.clone()).await;
		let error = child.expect_err("child queue is empty");
		assert!(error.to_string().contains("No pending action to apply"));
		assert!(!target.exists());

		// The root queue still holds it and applies exactly once.
		let applied = write(supervisor.root_registry(), dir.clone())
			.await
			.expect("root apply");
		assert!(!applied.is_error);
		assert_eq!(std::fs::read_to_string(&target).expect("read"), "go");
		assert!(root_store.is_empty());
		let again = write(supervisor.root_registry(), dir.clone())
			.await
			.expect_err("consumed");
		assert!(again.to_string().contains("No pending action to apply"));

		std::fs::remove_dir_all(dir).ok();
	}

	#[test]
	fn eval_owner_results_dedupe_and_route_to_live_ancestor() {
		let supervisor = supervisor();
		assert!(supervisor.enqueue_owner_result(None, "eval-a", "one".to_string()));
		// Same id is exactly-once even before consumption.
		assert!(!supervisor.enqueue_owner_result(None, "eval-a", "duplicate".to_string()));
		// Settled/missing child owner routes to root.
		assert!(supervisor.enqueue_owner_result(
			Some("child-that-settled"),
			"eval-b",
			"two".to_string(),
		));
		assert!(supervisor.take_results_for(Some("child-that-settled")).is_empty());
		assert_eq!(
			supervisor.take_results_for(None),
			vec!["one".to_string(), "two".to_string()]
		);
		// Consumed ids never resurrect.
		assert!(!supervisor.enqueue_owner_result(None, "eval-a", "late".to_string()));
		// No sink attached means background delivery is unavailable.
		assert!(!supervisor.background_ready());
	}

	/// A completed background child keeps the summed usage on its retained
	/// record, delivers the body exactly once, and a later snapshot consumption
	/// hides only the body — never the counters.
	#[test]
	fn settled_child_usage_is_retained_and_delivered_exactly_once() {
		let supervisor = supervisor();
		let (id, _token) = supervisor
			.reserve_child("bg", 0, "task", "work", None)
			.expect("reserve");
		let usage = Some(crate::state::TokenUsage {
			input: 9,
			output: 4,
			..crate::state::TokenUsage::default()
		});
		supervisor.mark_completed_with_usage(&id, 5, "<task-result/>", usage);
		supervisor.enqueue_delivery(&id, "<task-result/>".to_string());
		let record = |supervisor: &Arc<Supervisor>| {
			supervisor
				.children()
				.into_iter()
				.find(|record| record.id == id)
				.expect("retained record")
		};
		assert_eq!(record(&supervisor).usage, usage);
		let delivered = supervisor.take_deliveries_for(None);
		assert_eq!(delivered.len(), 1);
		assert_eq!(delivered[0].id, id, "identity is the exactly-once key");
		assert_eq!(delivered[0].text, "<task-result/>");
		assert_eq!(delivered[0].usage, usage);
		assert!(
			supervisor.take_results_for(None).is_empty(),
			"the text and rich views share one consumption"
		);
		assert_eq!(
			record(&supervisor).usage,
			usage,
			"delivery must not clear the retained counters"
		);
	}

	/// A child that reports usage and then fails validation still contributes
	/// those counters: the failure is about the output shape, not the spend.
	#[test]
	fn failed_after_reported_usage_keeps_the_counters() {
		let supervisor = supervisor();
		let (id, _token) = supervisor
			.reserve_child("bg", 0, "task", "work", None)
			.expect("reserve");
		let usage = Some(crate::state::TokenUsage {
			input: 3,
			output: 2,
			cache_write: 1,
			..crate::state::TokenUsage::default()
		});
		let envelope = "<task-result status=\"failed\"><usage input=\"3\" /></task-result>";
		supervisor.mark_failed_with_usage(&id, 7, envelope, usage);
		supervisor.enqueue_delivery(&id, envelope.to_string());
		let record = supervisor
			.children()
			.into_iter()
			.find(|record| record.id == id)
			.expect("retained record");
		assert_eq!(record.status, ChildStatus::Failed);
		assert_eq!(record.usage, usage);

		// A snapshot consumption is the single delivery: the queued copy is
		// gone, yet the record still reports the counters once.
		assert!(supervisor.consume_results(std::slice::from_ref(&id)).is_empty());
		assert!(supervisor.take_results_for(None).is_empty());
		let record = supervisor
			.children()
			.into_iter()
			.find(|record| record.id == id)
			.expect("retained record");
		assert_eq!(record.usage, usage);
	}
}
