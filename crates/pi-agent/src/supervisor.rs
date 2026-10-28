use std::{
	collections::HashMap,
	sync::{
		Arc, Mutex,
		atomic::{AtomicU64, Ordering},
	},
	time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::{
	loop_module::SteeringEvent,
	provider::ModelProvider,
	state::ToolApprover,
	tools::{
		TaskTool, YieldTool,
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
	pub id: String,
	pub index: usize,
	pub agent: String,
	pub task: String,
	pub status: ChildStatus,
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

struct ActiveChild {
	steering_tx: mpsc::Sender<SteeringEvent>,
	abort_handle: tokio::task::AbortHandle,
}

struct SupervisorState {
	records: HashMap<String, ChildRecord>,
	order: Vec<String>,
	active: HashMap<String, ActiveChild>,
}

pub struct Supervisor {
	template: ToolRegistry,
	pub provider: Arc<dyn ModelProvider>,
	pub child_approver: Option<Arc<dyn ToolApprover>>,
	pub limits: DelegationLimits,
	state: Mutex<SupervisorState>,
	seq: AtomicU64,
}

impl Supervisor {
	pub fn new(
		template: ToolRegistry,
		provider: Arc<dyn ModelProvider>,
		child_approver: Option<Arc<dyn ToolApprover>>,
		limits: DelegationLimits,
	) -> Arc<Self> {
		Arc::new(Self {
			template,
			provider,
			child_approver,
			limits,
			state: Mutex::new(SupervisorState {
				records: HashMap::new(),
				order: Vec::new(),
				active: HashMap::new(),
			}),
			seq: AtomicU64::new(0),
		})
	}

	pub fn root_registry(self: &Arc<Self>) -> ToolRegistry {
		let mut reg = self.template.clone();
		if self.limits.max_depth < 0 || 0 < self.limits.max_depth {
			reg.register(Arc::new(TaskTool::new(Arc::clone(self), 0)));
		}
		reg
	}

	pub fn child_registry(self: &Arc<Self>, depth: i32) -> ToolRegistry {
		let mut reg = self.template.clone();
		reg.register(Arc::new(YieldTool::new()));
		if self.limits.max_depth < 0 || depth < self.limits.max_depth {
			reg.register(Arc::new(TaskTool::new(Arc::clone(self), depth)));
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

	pub fn cancel(&self, id: &str) -> bool {
		let mut state = self.state.lock().unwrap();
		if let Some(active) = state.active.remove(id) {
			let _ = active.steering_tx.try_send(SteeringEvent::Interrupt);
			active.abort_handle.abort();
			if let Some(record) = state.records.get_mut(id) {
				if record.status == ChildStatus::Running {
					record.status = ChildStatus::Aborted;
					record.aborted = Some(true);
					record.abort_reason = Some("aborted by supervisor".to_string());
				}
			}
			true
		} else {
			false
		}
	}

	pub fn register_child(&self, id: &str, index: usize, agent: &str, task: &str) {
		let mut state = self.state.lock().unwrap();
		let started_at = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.map(|d| d.as_millis() as u64)
			.ok();
		let record = ChildRecord {
			id: id.to_string(),
			index,
			agent: agent.to_string(),
			task: task.to_string(),
			status: ChildStatus::Running,
			started_at,
			duration_ms: None,
			output: None,
			error: None,
			aborted: None,
			abort_reason: None,
		};
		state.order.push(id.to_string());
		state.records.insert(id.to_string(), record);
	}

	pub fn register_active_child(
		&self,
		id: &str,
		steering_tx: mpsc::Sender<SteeringEvent>,
		abort_handle: tokio::task::AbortHandle,
	) {
		let mut state = self.state.lock().unwrap();
		state
			.active
			.insert(id.to_string(), ActiveChild { steering_tx, abort_handle });
	}

	pub fn unregister_active_child(&self, id: &str) {
		let mut state = self.state.lock().unwrap();
		state.active.remove(id);
	}

	pub fn mark_completed(&self, id: &str, duration_ms: u64, output: &str) {
		let mut state = self.state.lock().unwrap();
		if let Some(record) = state.records.get_mut(id) {
			if record.status == ChildStatus::Running {
				record.status = ChildStatus::Completed;
				record.duration_ms = Some(duration_ms);
				record.output = Some(output.to_string());
			}
		}
	}

	pub fn mark_failed(&self, id: &str, duration_ms: u64, error: &str) {
		let mut state = self.state.lock().unwrap();
		if let Some(record) = state.records.get_mut(id) {
			if record.status == ChildStatus::Running {
				record.status = ChildStatus::Failed;
				record.duration_ms = Some(duration_ms);
				record.error = Some(error.to_string());
			}
		}
	}

	pub fn mark_aborted(&self, id: &str, reason: &str) {
		let mut state = self.state.lock().unwrap();
		if let Some(record) = state.records.get_mut(id) {
			if record.status == ChildStatus::Running {
				record.status = ChildStatus::Aborted;
				record.aborted = Some(true);
				record.abort_reason = Some(reason.to_string());
			}
		}
	}
}

pub struct ChildGuard {
	pub handle: tokio::task::AbortHandle,
	pub steering_tx: mpsc::Sender<SteeringEvent>,
	supervisor: Arc<Supervisor>,
	id: String,
	defused: bool,
}

impl ChildGuard {
	pub fn new(
		handle: tokio::task::AbortHandle,
		steering_tx: mpsc::Sender<SteeringEvent>,
		supervisor: Arc<Supervisor>,
		id: String,
	) -> Self {
		Self {
			handle,
			steering_tx,
			supervisor,
			id,
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
			let _ = self.steering_tx.try_send(SteeringEvent::Interrupt);
			self.handle.abort();
			self.supervisor.mark_aborted(&self.id, "aborted by parent");
		}
	}
}
