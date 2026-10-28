use std::{
	collections::HashMap,
	sync::{
		Arc, Mutex,
		atomic::{AtomicBool, AtomicU64, Ordering},
	},
	time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

use crate::{
	loop_module::SteeringEvent,
	provider::ModelProvider,
	state::ToolApprover,
	tools::{
		HubTool, TaskTool, YieldTool,
		registry::ToolRegistry,
	},
};

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
	state: Mutex<SupervisorState>,
	seq: AtomicU64,
	/// Wakes `cancel_child` waiters whenever a child reaches a terminal status.
	settle: Notify,
}

/// Result of a scoped `hub` cancellation attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelAttempt {
	Cancelled,
	NotFound,
	AlreadyCompleted,
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
		Arc::new(Self {
			template: Mutex::new(template),
			provider: Mutex::new(provider),
			child_approver: Mutex::new(child_approver),
			limits: Mutex::new(limits),
			allow_task: AtomicBool::new(allow_task),
			allow_hub: AtomicBool::new(allow_hub),
			state: Mutex::new(SupervisorState {
				records: HashMap::new(),
				order: Vec::new(),
				active: HashMap::new(),
				reservations: HashMap::new(),
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

	pub fn limits(&self) -> DelegationLimits {
		*self.limits.lock().unwrap()
	}

	pub fn provider(&self) -> Arc<dyn ModelProvider> {
		self.provider.lock().unwrap().clone()
	}

	pub fn child_approver(&self) -> Option<Arc<dyn ToolApprover>> {
		self.child_approver.lock().unwrap().clone()
	}

	pub fn task_enabled(&self) -> bool {
		self.allow_task.load(Ordering::SeqCst)
	}

	pub fn hub_enabled(&self) -> bool {
		self.allow_hub.load(Ordering::SeqCst)
	}

	pub fn root_registry(self: &Arc<Self>) -> ToolRegistry {
		let mut reg = self.template.lock().unwrap().clone();
		let limits = self.limits();
		if self.task_enabled() && (limits.max_depth < 0 || 0 < limits.max_depth) {
			reg.register(Arc::new(TaskTool::new(Arc::clone(self), 0)));
		}
		if self.hub_enabled() {
			reg.register(Arc::new(HubTool::root(Arc::clone(self))));
		}
		reg
	}

	pub fn child_registry(self: &Arc<Self>, depth: i32, owner: Option<&str>) -> ToolRegistry {
		let mut reg = self.template.lock().unwrap().clone();
		// `todo` is parent-owned: the template shares one live `Arc<TodoTool>`
		// instance, and child events/session details are not persisted. Dropping
		// it here prevents descendants from mutating the parent list.
		reg.remove("todo");
		reg.register(Arc::new(YieldTool::new()));
		let limits = self.limits();
		if self.task_enabled() && (limits.max_depth < 0 || depth < limits.max_depth) {
			reg.register(Arc::new(TaskTool::with_parent(
				Arc::clone(self),
				depth,
				owner.map(str::to_string),
			)));
		}
		if self.hub_enabled() {
			reg.register(Arc::new(HubTool::with_owner(
				Arc::clone(self),
				owner.map(str::to_string),
			)));
		}
		reg
	}

	pub fn next_seq(&self) -> u64 {
		self.seq.fetch_add(1, Ordering::SeqCst) + 1
	}

	pub fn children(&self) -> Vec<ChildRecord> {
		let state = self.state.lock().unwrap();
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
	) -> (String, CancellationToken) {
		let id = format!("task-{}", self.next_seq());
		let cancellation_token = CancellationToken::new();
		let mut state = self.state.lock().unwrap();
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
		(id, cancellation_token)
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
		let mut state = self.state.lock().unwrap();
		state.active.remove(id);
		state.reservations.remove(id);
	}

	pub fn mark_completed(&self, id: &str, duration_ms: u64, output: &str) {
		let changed = {
			let mut state = self.state.lock().unwrap();
			match state.records.get_mut(id) {
				Some(record) if record.status == ChildStatus::Running => {
					record.status = ChildStatus::Completed;
					record.duration_ms = Some(duration_ms);
					record.output = Some(output.to_string());
					true
				},
				_ => false,
			}
		};
		if changed {
			self.settle.notify_waiters();
		}
	}

	pub fn mark_failed(&self, id: &str, duration_ms: u64, error: &str) {
		let changed = {
			let mut state = self.state.lock().unwrap();
			match state.records.get_mut(id) {
				Some(record) if record.status == ChildStatus::Running => {
					record.status = ChildStatus::Failed;
					record.duration_ms = Some(duration_ms);
					record.error = Some(error.to_string());
					true
				},
				_ => false,
			}
		};
		if changed {
			self.settle.notify_waiters();
		}
	}

	pub fn mark_aborted(&self, id: &str, reason: &str) {
		let changed = {
			let mut state = self.state.lock().unwrap();
			match state.records.get_mut(id) {
				Some(record) if record.status == ChildStatus::Running => {
					record.status = ChildStatus::Aborted;
					record.aborted = Some(true);
					record.abort_reason = Some(reason.to_string());
					true
				},
				_ => false,
			}
		};
		if changed {
			self.settle.notify_waiters();
		}
	}
}

/// Cancellation may take up to the shell's own graceful-kill window; after this
/// bound the task is aborted as a last resort.
const CANCEL_SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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
