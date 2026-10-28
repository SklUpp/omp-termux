use std::{
	collections::{HashMap, HashSet, VecDeque},
	path::PathBuf,
	sync::{
		Arc, Mutex,
		atomic::{AtomicBool, AtomicU64, Ordering},
	},
	time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

use crate::{
	loop_module::SteeringEvent,
	mailbox::Mailbox,
	provider::ModelProvider,
	state::ToolApprover,
	tools::{
		BashTool, EditTool, HubTool, TaskTool, ThinkTool, YieldTool,
		hashline::{self, NativeSnapshotSink, SnapshotStore},
		process::{ProcessBackend, ProcessManager},
		read::ReadTool,
		registry::ToolRegistry,
		task_catalog::AgentCatalog,
	},
};

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
}

/// One undelivered background result, scoped to the agent that spawned it.
#[derive(Clone)]
struct PendingDelivery {
	id: String,
	owner: Option<String>,
	text: String,
}

struct OwnerResultSink {
	supervisor: Arc<Supervisor>,
	owner: Option<String>,
}

impl BackgroundResultSink for OwnerResultSink {
	fn take_background_results(&self) -> Vec<String> {
		self.supervisor.take_results_for(self.owner.as_deref())
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
	/// Agent catalog installed by the host (CLI) runtime. Definitions and policy
	/// are stable; the catalog's model resolver is refreshed per root turn.
	agent_catalog: Mutex<Option<Arc<AgentCatalog>>>,
	/// Process supervisor shared by root/child `bash` and `hub` process ops.
	/// Injected by the host (broker client); otherwise a lazily created local
	/// manager rooted at `process_root`.
	processes: Mutex<Option<Arc<dyn ProcessBackend>>>,
	process_root: PathBuf,
	/// Runner-unique prefix for backend-visible process owners. UI ids such as
	/// `task-1` repeat across concurrent CLIs; process scoping must not.
	process_owner_ns: String,
	state: Mutex<SupervisorState>,
	seq: AtomicU64,
	/// Wakes `cancel_child`, `hub jobs`, and `hub wait` waiters whenever a child
	/// reaches a terminal status or a delivery is consumed.
	settle: Notify,
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
			agent_catalog: Mutex::new(None),
			processes: Mutex::new(None),
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
		let mut state = self.state.lock().unwrap();
		let mut taken = Vec::new();
		let mut index = 0;
		while index < state.pending.len() {
			if state.pending[index].owner.as_deref() == owner {
				if let Some(delivery) = state.pending.remove(index) {
					state.consumed.insert(delivery.id.clone());
					state.outstanding.remove(&delivery.id);
					state.pending_bytes = state.pending_bytes.saturating_sub(delivery.text.len());
					taken.push(delivery.text);
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
		state.pending_bytes += text.len();
		state.pending.push_back(PendingDelivery { id: id.to_string(), owner, text });
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
				.try_send(SteeringEvent::InjectMessage(text))
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
		let mut reg = self.template.lock().unwrap().clone();
		// `read`/`edit` are session-scoped: a child gets its own snapshot store,
		// `CUT` registers, and no-op guard. Rebinding keeps the parent's tool
		// whitelist and edit options, but never its read provenance.
		let snapshots = Arc::new(SnapshotStore::new());
		if reg.get("read").is_some() {
			let sink = Arc::new(NativeSnapshotSink::new(snapshots.clone()));
			reg.remove("read");
			reg.register(Arc::new(ReadTool::with_snapshot_sink(sink)));
		}
		if reg.get("edit").is_some() {
			let options = *self.edit_options.lock().unwrap();
			reg.remove("edit");
			reg.register(Arc::new(EditTool::with_options(snapshots, options)));
		}
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
			reg.register(Arc::new(BashTool::with_processes(
				Arc::clone(&processes),
				backend_owner,
			)));
		}
		reg.register(Arc::new(YieldTool::new()));
		reg.register(Arc::new(ThinkTool::new()));
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
		let record = ChildRecord {
			id: id.clone(),
			name: name.to_string(),
			index,
			agent: agent.to_string(),
			task: task.to_string(),
			status: ChildStatus::Running,
			parent_id: parent_id.map(str::to_string),
			started_at,
			duration_ms: None,
			output: None,
			error: None,
			aborted: None,
			abort_reason: None,
		};
		state.order.push(id.clone());
		state.records.insert(id.clone(), record);
		state
			.reservations
			.insert(id.clone(), cancellation_token.clone());
		state.outstanding.insert(id.clone());
		Some((id, cancellation_token))
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
		self.settle_child(id, duration_ms, Some(output), None);
	}

	pub fn mark_failed(&self, id: &str, duration_ms: u64, error: &str) {
		self.settle_child(id, duration_ms, None, Some(error));
	}

	pub fn mark_aborted(&self, id: &str, reason: &str) -> bool {
		let changed = {
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
			changed
		};
		if changed {
			self.sweep_owner_deliveries(id);
			self.settle.notify_waiters();
		}
		changed
	}

	fn settle_child(&self, id: &str, duration_ms: u64, output: Option<&str>, error: Option<&str>) {
		let changed = {
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
					true
				},
				_ => false,
			};
			if changed {
				state.settled_at.insert(id.to_string(), now_ms());
			}
			changed
		};
		if changed {
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
		state::Message,
		tools::{process::ProcessManager, registry::ToolRegistry, Tool},
	};

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
}
