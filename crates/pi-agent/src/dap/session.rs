//! DAP session manager: single-root session tree, breakpoint synchronization,
//! handshake, stop outcome waits, output ring, idle cleanup and termination.
//!
//! Mirrors `packages/coding-agent/src/dap/session.ts`, including the
//! configurationDone handshake, reverse `runInTerminal`/`startDebugging`
//! handling and per-request cancellation.

use std::{
	collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
	future::Future,
	path::{Path, PathBuf},
	process::Stdio,
	sync::{
		Arc, Mutex as StdMutex, MutexGuard, Weak,
		atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
	},
	time::{Duration, Instant},
};

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use tokio::{
	io::AsyncReadExt,
	process::Command,
	sync::{Mutex as AsyncMutex, broadcast},
	time::timeout,
};
use tokio_util::sync::CancellationToken;

use super::client::{
	DapClient, DapError, EventPredicate, ReverseRequestHandler, apply_detached, non_interactive_env,
};
use super::config::ResolvedAdapter;
use super::types::{
	Breakpoint, BreakpointRecord, BreakpointsResponse, Capabilities, ContinueOutcome, ContinueState,
	ContinuedEventBody, DataBreakpointInfoResponse, DataBreakpointRecord, DisassembleResponse,
	DisassembledInstruction, EvaluateResponse, ExitedEventBody, FunctionBreakpointRecord,
	InstructionBreakpointRecord, Module, ModulesResponse, OutputEventBody, ReadMemoryResponse,
	RunInTerminalArguments, Scope, ScopesResponse, SessionStatus, SessionSummary, Source, SourcesResponse,
	StackFrame, StackTraceResponse, StartDebuggingArguments, StopLocation, StoppedEventBody, Thread,
	ThreadsResponse, Variable, VariablesResponse, WriteMemoryResponse,
};

pub const IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
pub const CLEANUP_INTERVAL: Duration = Duration::from_secs(30);
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
pub const MAX_OUTPUT_BYTES: usize = 128 * 1024;
pub const STOP_CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);

fn lock<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
	mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn now_millis() -> i64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|duration| duration.as_millis() as i64)
		.unwrap_or(0)
}

fn iso8601(millis: i64) -> String {
	let seconds = millis.div_euclid(1000);
	let millis_part = millis.rem_euclid(1000);
	let days = seconds.div_euclid(86_400);
	let seconds_of_day = seconds.rem_euclid(86_400);
	let (year, month, day) = civil_from_days(days);
	format!(
		"{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis_part:03}Z",
		seconds_of_day / 3600,
		(seconds_of_day % 3600) / 60,
		seconds_of_day % 60
	)
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
	let z = days + 719_468;
	let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
	let day_of_era = z - era * 146_097;
	let year_of_era =
		(day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
	let year = year_of_era + era * 400;
	let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
	let month_prime = (5 * day_of_year + 2) / 153;
	let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
	let month = month_prime + if month_prime < 10 { 3 } else { -9 };
	(year + i64::from(month <= 2), month as u32, day as u32)
}

fn basename(path: &str) -> String {
	Path::new(path)
		.file_name()
		.map(|name| name.to_string_lossy().into_owned())
		.unwrap_or_else(|| path.to_string())
}

fn normalize_path(path: &str) -> String {
	Path::new(path).to_string_lossy().into_owned()
}

#[derive(Default)]
struct OutputRing {
	chunks: VecDeque<Vec<u8>>,
	buffered: usize,
	total: u64,
	truncated: bool,
}

impl OutputRing {
	fn push(&mut self, text: &str) {
		if text.is_empty() {
			return;
		}
		let bytes = text.as_bytes().to_vec();
		let length = bytes.len();
		self.total += length as u64;
		self.chunks.push_back(bytes);
		self.buffered += length;
		while self.chunks.len() > 1 {
			let front = self.chunks.front().map(Vec::len).unwrap_or(0);
			if self.buffered - front < MAX_OUTPUT_BYTES {
				break;
			}
			self.chunks.pop_front();
			self.buffered -= front;
			self.truncated = true;
		}
		if self.buffered > MAX_OUTPUT_BYTES {
			let excess = self.buffered - MAX_OUTPUT_BYTES;
			if let Some(front) = self.chunks.front_mut() {
				front.drain(..excess.min(front.len()));
				self.buffered -= excess;
				self.truncated = true;
			}
		}
	}

	fn text(&self) -> String {
		let mut bytes = Vec::with_capacity(self.buffered);
		for chunk in &self.chunks {
			bytes.extend_from_slice(chunk);
		}
		String::from_utf8_lossy(&bytes).into_owned()
	}
}

/// One live debug session. Fields mutated by event callbacks use short-lived
/// mutexes; no lock is held across an await.
pub struct DapSession {
	pub id: String,
	pub adapter: ResolvedAdapter,
	pub cwd: PathBuf,
	pub program: Option<String>,
	pub client: DapClient,
	pub env: HashMap<String, String>,
	status: StdMutex<SessionStatus>,
	launched_at: i64,
	last_used_at: AtomicI64,
	breakpoints: StdMutex<BTreeMap<String, Vec<BreakpointRecord>>>,
	function_breakpoints: StdMutex<Vec<FunctionBreakpointRecord>>,
	instruction_breakpoints: StdMutex<Vec<Value>>,
	data_breakpoints: StdMutex<Vec<Value>>,
	breakpoint_queue: AsyncMutex<()>,
	output: StdMutex<OutputRing>,
	stop: StdMutex<StopLocation>,
	threads: StdMutex<Vec<Thread>>,
	last_stack_frames: StdMutex<Vec<StackFrame>>,
	exit_code: StdMutex<Option<i64>>,
	capabilities: StdMutex<Option<Capabilities>>,
	initialized_seen: AtomicBool,
	needs_configuration_done: AtomicBool,
	configuration_done_sent: AtomicBool,
	pub parent_session_id: Option<String>,
	child_session_ids: StdMutex<BTreeSet<String>>,
	pub port: Option<u16>,
}

impl DapSession {
	pub fn status(&self) -> SessionStatus {
		*lock(&self.status)
	}

	fn set_status(&self, status: SessionStatus) {
		*lock(&self.status) = status;
	}

	fn touch(&self) {
		self.last_used_at.store(now_millis(), Ordering::SeqCst);
	}

	fn capability(&self, name: &str) -> bool {
		lock(&self.capabilities).as_ref().is_some_and(|capabilities| capabilities.supports(name))
	}

	fn push_output(&self, text: &str) {
		lock(&self.output).push(text);
	}

	fn stop_location(&self) -> StopLocation {
		lock(&self.stop).clone()
	}

	fn summary(&self) -> SessionSummary {
		let stop = self.stop_location();
		let breakpoints = lock(&self.breakpoints);
		let breakpoint_count = breakpoints.values().map(Vec::len).sum();
		let output = lock(&self.output);
		SessionSummary {
			id: self.id.clone(),
			adapter: self.adapter.name.clone(),
			cwd: self.cwd.to_string_lossy().into_owned(),
			program: self.program.clone(),
			status: self.status(),
			launched_at: iso8601(self.launched_at),
			last_used_at: iso8601(self.last_used_at.load(Ordering::SeqCst)),
			thread_id: stop.thread_id,
			frame_id: stop.frame_id,
			stop_reason: stop.reason.clone(),
			stop_description: stop.description.clone().or(stop.text.clone()),
			frame_name: stop.frame_name.clone(),
			instruction_pointer_reference: stop.instruction_pointer_reference.clone(),
			source: stop.source.clone(),
			line: stop.line,
			column: stop.column,
			breakpoint_files: breakpoints.len(),
			breakpoint_count,
			function_breakpoint_count: lock(&self.function_breakpoints).len(),
			output_bytes: output.total,
			output_truncated: output.truncated,
			exit_code: *lock(&self.exit_code),
			needs_configuration_done: self.needs_configuration_done.load(Ordering::SeqCst)
				&& !self.configuration_done_sent.load(Ordering::SeqCst),
			parent_session_id: self.parent_session_id.clone(),
			child_session_ids: {
				let children = lock(&self.child_session_ids);
				(!children.is_empty()).then(|| children.iter().cloned().collect())
			},
		}
	}
}

#[derive(Debug, Clone)]
pub struct LaunchOptions {
	pub adapter: ResolvedAdapter,
	pub program: String,
	pub args: Option<Vec<String>>,
	pub cwd: PathBuf,
	pub extra_launch_arguments: Value,
	pub env: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct AttachOptions {
	pub adapter: ResolvedAdapter,
	pub cwd: PathBuf,
	pub pid: Option<i64>,
	pub port: Option<u16>,
	pub host: Option<String>,
	pub env: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct SetBreakpointsResult {
	pub snapshot: SessionSummary,
	pub breakpoints: Vec<BreakpointRecord>,
	pub source_path: String,
}

#[derive(Debug, Clone)]
pub struct FunctionBreakpointsResult {
	pub snapshot: SessionSummary,
	pub breakpoints: Vec<FunctionBreakpointRecord>,
}

#[derive(Debug, Clone)]
pub struct InstructionBreakpointsResult {
	pub snapshot: SessionSummary,
	pub breakpoints: Vec<InstructionBreakpointRecord>,
}

#[derive(Debug, Clone)]
pub struct DataBreakpointsResult {
	pub snapshot: SessionSummary,
	pub breakpoints: Vec<DataBreakpointRecord>,
}

#[derive(Debug, Clone)]
pub struct DataBreakpointInfoResult {
	pub snapshot: SessionSummary,
	pub info: DataBreakpointInfoResponse,
}

#[derive(Debug, Clone)]
pub struct StackTraceResult {
	pub snapshot: SessionSummary,
	pub stack_frames: Vec<StackFrame>,
}

#[derive(Debug, Clone)]
pub struct ThreadsResult {
	pub snapshot: SessionSummary,
	pub threads: Vec<Thread>,
}

#[derive(Debug, Clone)]
pub struct ScopesResult {
	pub snapshot: SessionSummary,
	pub scopes: Vec<Scope>,
}

#[derive(Debug, Clone)]
pub struct VariablesResult {
	pub snapshot: SessionSummary,
	pub variables: Vec<Variable>,
}

#[derive(Debug, Clone)]
pub struct EvaluateResult {
	pub snapshot: SessionSummary,
	pub evaluation: EvaluateResponse,
}

#[derive(Debug, Clone)]
pub struct DisassembleResult {
	pub snapshot: SessionSummary,
	pub instructions: Vec<DisassembledInstruction>,
}

#[derive(Debug, Clone)]
pub struct ReadMemoryResult {
	pub snapshot: SessionSummary,
	pub address: String,
	pub data: Option<String>,
	pub unreadable_bytes: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct WriteMemoryResult {
	pub snapshot: SessionSummary,
	pub offset: Option<i64>,
	pub bytes_written: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct ModulesResult {
	pub snapshot: SessionSummary,
	pub modules: Vec<Module>,
}

#[derive(Debug, Clone)]
pub struct SourcesResult {
	pub snapshot: SessionSummary,
	pub sources: Vec<Source>,
}

#[derive(Debug, Clone)]
pub struct CustomRequestResult {
	pub snapshot: SessionSummary,
	pub body: Value,
}

#[derive(Debug, Clone)]
pub struct OutputSnapshot {
	pub snapshot: SessionSummary,
	pub output: String,
}

struct ManagerInner {
	sessions: StdMutex<BTreeMap<String, Arc<DapSession>>>,
	active: StdMutex<Option<String>>,
	next_id: AtomicU64,
	cleanup_started: AtomicBool,
	outcomes: broadcast::Sender<String>,
}

#[derive(Clone)]
pub struct DapSessionManager {
	inner: Arc<ManagerInner>,
}

impl Default for DapSessionManager {
	fn default() -> Self {
		Self::new()
	}
}

impl DapSessionManager {
	pub fn new() -> Self {
		let (outcomes, _) = broadcast::channel(64);
		let manager = Self {
			inner: Arc::new(ManagerInner {
				sessions: StdMutex::new(BTreeMap::new()),
				active: StdMutex::new(None),
				next_id: AtomicU64::new(0),
				cleanup_started: AtomicBool::new(false),
				outcomes,
			}),
		};
		manager.start_cleanup_timer();
		manager
	}

	fn from_inner(inner: Arc<ManagerInner>) -> Self {
		Self { inner }
	}

	fn start_cleanup_timer(&self) {
		if self.inner.cleanup_started.swap(true, Ordering::SeqCst) {
			return;
		}
		if tokio::runtime::Handle::try_current().is_err() {
			return;
		}
		let weak = Arc::downgrade(&self.inner);
		tokio::spawn(async move {
			let mut ticker = tokio::time::interval(CLEANUP_INTERVAL);
			ticker.tick().await;
			loop {
				ticker.tick().await;
				let Some(inner) = weak.upgrade() else {
					break;
				};
				Self::from_inner(inner).cleanup_idle_sessions().await;
			}
		});
	}

	pub fn get_active_session(&self) -> Option<SessionSummary> {
		self.active_session().map(|session| session.summary())
	}

	pub fn list_sessions(&self) -> Vec<SessionSummary> {
		lock(&self.inner.sessions).values().map(|session| session.summary()).collect()
	}

	pub fn get_capabilities(&self) -> Option<Capabilities> {
		self.active_session()
			.and_then(|session| lock(&session.capabilities).clone())
	}

	fn active_session(&self) -> Option<Arc<DapSession>> {
		let active_id = lock(&self.inner.active).clone()?;
		let session = lock(&self.inner.sessions).get(&active_id).cloned();
		if session.is_none() {
			*lock(&self.inner.active) = None;
		}
		session
	}

	fn active_session_or_err(&self) -> Result<Arc<DapSession>, DapError> {
		self
			.active_session()
			.ok_or_else(|| DapError::message("No active debug session. Launch or attach first."))
	}

	fn touch_active(&self) -> Result<Arc<DapSession>, DapError> {
		let session = self.active_session_or_err()?;
		self.touch_session_and_ancestors(&session);
		if session.status() != SessionStatus::Terminated && !session.client.is_alive() {
			session.set_status(SessionStatus::Terminated);
		}
		Ok(session)
	}

	pub async fn launch(
		&self,
		options: LaunchOptions,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<SessionSummary, DapError> {
		self.ensure_launch_slot().await?;
		let client = DapClient::spawn(&options.adapter, &options.cwd, &options.env).await?;
		let session = self.register(
			client,
			options.adapter.clone(),
			options.cwd.clone(),
			Some(options.program.clone()),
			None,
			options.env.clone(),
		);
		match self.launch_registered(&session, &options, cancel, deadline).await {
			Ok(summary) => Ok(summary),
			Err(error) => {
				self.dispose_session(&session).await;
				Err(map_debugpy_missing_module(&options.adapter.name, error))
			},
		}
	}

	pub async fn attach(
		&self,
		options: AttachOptions,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<SessionSummary, DapError> {
		self.ensure_launch_slot().await?;
		let client = DapClient::spawn(&options.adapter, &options.cwd, &options.env).await?;
		let session = self.register(
			client,
			options.adapter.clone(),
			options.cwd.clone(),
			None,
			None,
			options.env.clone(),
		);
		match self.attach_registered(&session, &options, cancel, deadline).await {
			Ok(summary) => Ok(summary),
			Err(error) => {
				self.dispose_session(&session).await;
				Err(map_debugpy_missing_module(&options.adapter.name, error))
			},
		}
	}

	async fn launch_registered(
		&self,
		session: &Arc<DapSession>,
		options: &LaunchOptions,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<SessionSummary, DapError> {
		self.initialize_session(session, cancel, deadline).await?;
		let mut arguments = merge_objects(&session.adapter.launch_defaults, &options.extra_launch_arguments);
		set_object_field(&mut arguments, "program", Value::String(options.program.clone()));
		set_object_field(&mut arguments, "cwd", Value::String(options.cwd.to_string_lossy().into_owned()));
		if let Some(args) = &options.args {
			set_object_field(&mut arguments, "args", json!(args));
		}
		self.start_root(session, "launch", arguments, cancel, deadline).await
	}

	async fn attach_registered(
		&self,
		session: &Arc<DapSession>,
		options: &AttachOptions,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<SessionSummary, DapError> {
		self.initialize_session(session, cancel, deadline).await?;
		if session.adapter.skip_attach_request() {
			self.complete_configuration_handshake(session, true, cancel, deadline).await?;
			return Ok(session.summary());
		}
		let mut arguments = session.adapter.attach_defaults.clone();
		if !arguments.is_object() {
			arguments = Value::Object(Map::new());
		}
		set_object_field(&mut arguments, "cwd", Value::String(options.cwd.to_string_lossy().into_owned()));
		if let Some(pid) = options.pid {
			set_object_field(&mut arguments, "pid", json!(pid));
			set_object_field(&mut arguments, "processId", json!(pid));
		}
		if let Some(port) = options.port {
			set_object_field(&mut arguments, "port", json!(port));
		}
		if let Some(host) = &options.host {
			set_object_field(&mut arguments, "host", Value::String(host.clone()));
		}
		self.start_root(session, "attach", arguments, cancel, deadline).await
	}

	async fn initialize_session(
		&self,
		session: &Arc<DapSession>,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<(), DapError> {
		let body = session
			.client
			.initialize(initialize_arguments(&session.adapter), cancel, deadline)
			.await?;
		let capabilities = Capabilities(if body.is_object() { body } else { json!({}) });
		session
			.needs_configuration_done
			.store(capabilities.supports("supportsConfigurationDoneRequest"), Ordering::SeqCst);
		*lock(&session.capabilities) = Some(capabilities);
		Ok(())
	}

	/// Fire launch/attach, complete the `initialized` → `configurationDone`
	/// handshake concurrently, then capture an initial stop (e.g. stopOnEntry).
	async fn start_root(
		&self,
		session: &Arc<DapSession>,
		command: &str,
		arguments: Value,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<SessionSummary, DapError> {
		let (mut outcome, root_id) = self.subscribe_outcome(session);
		drive_start_request(
			session.client.send_request(command, Some(arguments), cancel, deadline),
			self.complete_configuration_handshake(session, false, cancel, deadline),
			command,
		)
		.await?;
		let capture_deadline = deadline.min(Instant::now() + STOP_CAPTURE_TIMEOUT);
		match self.await_outcome(&mut outcome, &root_id, cancel, capture_deadline).await {
			Ok(()) => {
				if let Some(active) = self.active_in_tree(session) {
					if active.status() == SessionStatus::Stopped {
						let frame_deadline = deadline.min(Instant::now() + STOP_CAPTURE_TIMEOUT);
						self.fetch_top_frame(&active, cancel, frame_deadline).await;
					}
				}
			},
			Err(DapError::Aborted) => return Err(DapError::Aborted),
			Err(_) => {
				if session.initialized_seen.load(Ordering::SeqCst) && session.status() == SessionStatus::Launching {
					let next = if session.configuration_done_sent.load(Ordering::SeqCst) {
						SessionStatus::Running
					} else {
						SessionStatus::Configuring
					};
					session.set_status(next);
				}
			},
		}
		let result = self.active_in_tree(session).unwrap_or_else(|| Arc::clone(session));
		Ok(result.summary())
	}

	async fn complete_configuration_handshake(
		&self,
		session: &Arc<DapSession>,
		tolerate_missing_initialized: bool,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<(), DapError> {
		if session.configuration_done_sent.load(Ordering::SeqCst) {
			return Ok(());
		}
		if !session.needs_configuration_done.load(Ordering::SeqCst) {
			if session.parent_session_id.is_some() {
				self.apply_root_breakpoints_to_session(session, cancel, deadline).await;
			}
			let status = session.status();
			if status == SessionStatus::Launching || status == SessionStatus::Configuring {
				session.set_status(SessionStatus::Running);
			}
			return Ok(());
		}
		if !session.initialized_seen.load(Ordering::SeqCst) {
			match session.client.wait_for_event("initialized", None, cancel, deadline).await {
				Ok(_) => {},
				Err(DapError::Aborted) => return Err(DapError::Aborted),
				// Upstream's explicit `skipAttachRequest` path waits, then treats
				// a missing `initialized` as done and returns success WITHOUT
				// sending `configurationDone` (session.ts:1501-1509). Only that
				// configured path tolerates the missing event; root and child
				// launches keep the strict error.
				Err(_) if tolerate_missing_initialized => return Ok(()),
				Err(error) => return Err(error),
			}
		}
		if session.parent_session_id.is_some() {
			self.apply_root_breakpoints_to_session(session, cancel, deadline).await;
		}
		session
			.client
			.send_request("configurationDone", Some(json!({})), cancel, deadline)
			.await?;
		session.configuration_done_sent.store(true, Ordering::SeqCst);
		if session.status() == SessionStatus::Configuring {
			session.set_status(SessionStatus::Running);
		}
		Ok(())
	}

	async fn ensure_configuration_done(
		&self,
		session: &Arc<DapSession>,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<(), DapError> {
		if !session.needs_configuration_done.load(Ordering::SeqCst)
			|| session.configuration_done_sent.load(Ordering::SeqCst)
		{
			return Ok(());
		}
		session
			.client
			.send_request("configurationDone", Some(json!({})), cancel, deadline)
			.await?;
		session.configuration_done_sent.store(true, Ordering::SeqCst);
		if session.status() == SessionStatus::Configuring {
			session.set_status(SessionStatus::Running);
		}
		Ok(())
	}

	async fn send_request_with_config(
		&self,
		session: &Arc<DapSession>,
		command: &str,
		arguments: Option<Value>,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<Value, DapError> {
		self.ensure_configuration_done(session, cancel, deadline).await?;
		let body = session.client.send_request(command, arguments, cancel, deadline).await?;
		self.touch_session_and_ancestors(session);
		Ok(body)
	}

	// ── breakpoints ──────────────────────────────────────────────────────────

	pub async fn set_breakpoint(
		&self,
		file: &str,
		line: i64,
		condition: Option<String>,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<SetBreakpointsResult, DapError> {
		let session = self.touch_active()?;
		let source_path = normalize_path(file);
		let root = self.root_session(&session);
		let mut current = lock(&root.breakpoints).get(&source_path).cloned().unwrap_or_default();
		current.retain(|entry| entry.line != line);
		current.push(BreakpointRecord { verified: false, line, condition, ..Default::default() });
		current.sort_by_key(|entry| entry.line);
		let arguments = source_breakpoints_arguments(&source_path, &current);
		let tree = self.live_tree_sessions(&session);
		for target in &tree {
			lock(&target.breakpoints).insert(
				source_path.clone(),
				current.iter().cloned().map(|entry| BreakpointRecord { verified: false, ..entry }).collect(),
			);
		}
		self.sync_breakpoint_tree(&session, &tree, "setBreakpoints", arguments, cancel, deadline, |target, response| {
			lock(&target.breakpoints).insert(source_path.clone(), map_source_breakpoints(&current, response));
		})
		.await?;
		Ok(SetBreakpointsResult {
			snapshot: session.summary(),
			breakpoints: lock(&session.breakpoints).get(&source_path).cloned().unwrap_or_default(),
			source_path,
		})
	}

	pub async fn remove_breakpoint(
		&self,
		file: &str,
		line: i64,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<SetBreakpointsResult, DapError> {
		let session = self.touch_active()?;
		let source_path = normalize_path(file);
		let root = self.root_session(&session);
		let mut current = lock(&root.breakpoints).get(&source_path).cloned().unwrap_or_default();
		current.retain(|entry| entry.line != line);
		let arguments = source_breakpoints_arguments(&source_path, &current);
		let tree = self.live_tree_sessions(&session);
		for target in &tree {
			if current.is_empty() {
				lock(&target.breakpoints).remove(&source_path);
			} else {
				lock(&target.breakpoints).insert(
					source_path.clone(),
					current.iter().cloned().map(|entry| BreakpointRecord { verified: false, ..entry }).collect(),
				);
			}
		}
		self.sync_breakpoint_tree(&session, &tree, "setBreakpoints", arguments, cancel, deadline, |target, response| {
			if current.is_empty() {
				lock(&target.breakpoints).remove(&source_path);
			} else {
				lock(&target.breakpoints).insert(source_path.clone(), map_source_breakpoints(&current, response));
			}
		})
		.await?;
		Ok(SetBreakpointsResult {
			snapshot: session.summary(),
			breakpoints: lock(&session.breakpoints).get(&source_path).cloned().unwrap_or_default(),
			source_path,
		})
	}

	pub async fn set_function_breakpoint(
		&self,
		name: &str,
		condition: Option<String>,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<FunctionBreakpointsResult, DapError> {
		let session = self.touch_active()?;
		let root = self.root_session(&session);
		let mut current = lock(&root.function_breakpoints).clone();
		current.retain(|entry| entry.name != name);
		current.push(FunctionBreakpointRecord { verified: false, name: name.to_string(), condition, ..Default::default() });
		current.sort_by(|left, right| left.name.cmp(&right.name));
		let arguments = json!({
			"breakpoints": current.iter().map(|entry| {
				let mut value = json!({ "name": entry.name });
				if let Some(condition) = &entry.condition {
					value["condition"] = Value::String(condition.clone());
				}
				value
			}).collect::<Vec<_>>()
		});
		let tree = self.live_tree_sessions(&session);
		for target in &tree {
			*lock(&target.function_breakpoints) =
				current.iter().cloned().map(|entry| FunctionBreakpointRecord { verified: false, ..entry }).collect();
		}
		self.sync_breakpoint_tree(
			&session,
			&tree,
			"setFunctionBreakpoints",
			arguments.clone(),
			cancel,
			deadline,
			|target, response| {
				*lock(&target.function_breakpoints) = map_function_breakpoints(&current, response);
			},
		)
		.await?;
		Ok(FunctionBreakpointsResult {
			snapshot: session.summary(),
			breakpoints: lock(&session.function_breakpoints).clone(),
		})
	}

	pub async fn remove_function_breakpoint(
		&self,
		name: &str,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<FunctionBreakpointsResult, DapError> {
		let session = self.touch_active()?;
		let root = self.root_session(&session);
		let mut current = lock(&root.function_breakpoints).clone();
		current.retain(|entry| entry.name != name);
		let arguments = json!({
			"breakpoints": current.iter().map(|entry| {
				let mut value = json!({ "name": entry.name });
				if let Some(condition) = &entry.condition {
					value["condition"] = Value::String(condition.clone());
				}
				value
			}).collect::<Vec<_>>()
		});
		let tree = self.live_tree_sessions(&session);
		for target in &tree {
			*lock(&target.function_breakpoints) =
				current.iter().cloned().map(|entry| FunctionBreakpointRecord { verified: false, ..entry }).collect();
		}
		self.sync_breakpoint_tree(
			&session,
			&tree,
			"setFunctionBreakpoints",
			arguments,
			cancel,
			deadline,
			|target, response| {
				*lock(&target.function_breakpoints) = map_function_breakpoints(&current, response);
			},
		)
		.await?;
		Ok(FunctionBreakpointsResult {
			snapshot: session.summary(),
			breakpoints: lock(&session.function_breakpoints).clone(),
		})
	}

	pub async fn set_instruction_breakpoint(
		&self,
		instruction_reference: &str,
		offset: Option<i64>,
		condition: Option<String>,
		hit_condition: Option<String>,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<InstructionBreakpointsResult, DapError> {
		let session = self.touch_active()?;
		let root = self.root_session(&session);
		let mut current = lock(&root.instruction_breakpoints).clone();
		current.retain(|entry| {
			entry.get("instructionReference").and_then(Value::as_str) != Some(instruction_reference)
				|| entry.get("offset").and_then(Value::as_i64) != offset
		});
		current.push(instruction_breakpoint_value(instruction_reference, offset, condition, hit_condition));
		current.sort_by(|left, right| {
			let left_reference = left.get("instructionReference").and_then(Value::as_str).unwrap_or("");
			let right_reference = right.get("instructionReference").and_then(Value::as_str).unwrap_or("");
			left_reference.cmp(right_reference).then_with(|| {
				left.get("offset").and_then(Value::as_i64).unwrap_or(0)
					.cmp(&right.get("offset").and_then(Value::as_i64).unwrap_or(0))
			})
		});
		let arguments = json!({ "breakpoints": current.clone() });
		let tree = self.live_tree_sessions(&session);
		for target in &tree {
			*lock(&target.instruction_breakpoints) = current.clone();
		}
		let mut origin_response: Option<Vec<Breakpoint>> = None;
		self.sync_breakpoint_tree(
			&session,
			&tree,
			"setInstructionBreakpoints",
			arguments,
			cancel,
			deadline,
			|target, response| {
				if Arc::ptr_eq(target, &session) {
					origin_response = response.cloned();
				} else {
					*lock(&target.instruction_breakpoints) = current.clone();
				}
			},
		)
		.await?;
		Ok(InstructionBreakpointsResult {
			snapshot: session.summary(),
			breakpoints: instruction_breakpoint_records(&current, origin_response.as_deref()),
		})
	}

	pub async fn remove_instruction_breakpoint(
		&self,
		instruction_reference: &str,
		offset: Option<i64>,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<InstructionBreakpointsResult, DapError> {
		let session = self.touch_active()?;
		let root = self.root_session(&session);
		let mut current = lock(&root.instruction_breakpoints).clone();
		current.retain(|entry| {
			if entry.get("instructionReference").and_then(Value::as_str) != Some(instruction_reference) {
				return true;
			}
			offset.is_some() && entry.get("offset").and_then(Value::as_i64) != offset
		});
		let arguments = json!({ "breakpoints": current.clone() });
		let tree = self.live_tree_sessions(&session);
		for target in &tree {
			*lock(&target.instruction_breakpoints) = current.clone();
		}
		let mut origin_response: Option<Vec<Breakpoint>> = None;
		self.sync_breakpoint_tree(
			&session,
			&tree,
			"setInstructionBreakpoints",
			arguments,
			cancel,
			deadline,
			|target, response| {
				if Arc::ptr_eq(target, &session) {
					origin_response = response.cloned();
				}
			},
		)
		.await?;
		Ok(InstructionBreakpointsResult {
			snapshot: session.summary(),
			breakpoints: instruction_breakpoint_records(&current, origin_response.as_deref()),
		})
	}

	pub async fn data_breakpoint_info(
		&self,
		name: &str,
		variables_reference: Option<i64>,
		frame_id: Option<i64>,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<DataBreakpointInfoResult, DapError> {
		let session = self.touch_active()?;
		let mut arguments = Map::new();
		arguments.insert("name".to_string(), Value::String(name.to_string()));
		if let Some(variables_reference) = variables_reference {
			arguments.insert("variablesReference".to_string(), json!(variables_reference));
		}
		if let Some(frame_id) = frame_id {
			arguments.insert("frameId".to_string(), json!(frame_id));
		}
		let body = self
			.send_request_with_config(&session, "dataBreakpointInfo", Some(Value::Object(arguments)), cancel, deadline)
			.await?;
		let info = serde_json::from_value(body).unwrap_or_default();
		Ok(DataBreakpointInfoResult { snapshot: session.summary(), info })
	}

	pub async fn set_data_breakpoint(
		&self,
		data_id: &str,
		access_type: Option<String>,
		condition: Option<String>,
		hit_condition: Option<String>,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<DataBreakpointsResult, DapError> {
		let session = self.touch_active()?;
		let root = self.root_session(&session);
		let mut current = lock(&root.data_breakpoints).clone();
		current.retain(|entry| entry.get("dataId").and_then(Value::as_str) != Some(data_id));
		current.push(data_breakpoint_value(data_id, access_type, condition, hit_condition));
		current.sort_by(|left, right| {
			left.get("dataId").and_then(Value::as_str).unwrap_or("")
				.cmp(right.get("dataId").and_then(Value::as_str).unwrap_or(""))
		});
		let arguments = json!({ "breakpoints": current.clone() });
		let tree = self.live_tree_sessions(&session);
		for target in &tree {
			*lock(&target.data_breakpoints) = current.clone();
		}
		let mut origin_response: Option<Vec<Breakpoint>> = None;
		self.sync_breakpoint_tree(
			&session,
			&tree,
			"setDataBreakpoints",
			arguments,
			cancel,
			deadline,
			|target, response| {
				if Arc::ptr_eq(target, &session) {
					origin_response = response.cloned();
				}
			},
		)
		.await?;
		Ok(DataBreakpointsResult {
			snapshot: session.summary(),
			breakpoints: data_breakpoint_records(&current, origin_response.as_deref()),
		})
	}

	pub async fn remove_data_breakpoint(
		&self,
		data_id: &str,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<DataBreakpointsResult, DapError> {
		let session = self.touch_active()?;
		let root = self.root_session(&session);
		let mut current = lock(&root.data_breakpoints).clone();
		current.retain(|entry| entry.get("dataId").and_then(Value::as_str) != Some(data_id));
		let arguments = json!({ "breakpoints": current.clone() });
		let tree = self.live_tree_sessions(&session);
		for target in &tree {
			*lock(&target.data_breakpoints) = current.clone();
		}
		let mut origin_response: Option<Vec<Breakpoint>> = None;
		self.sync_breakpoint_tree(
			&session,
			&tree,
			"setDataBreakpoints",
			arguments,
			cancel,
			deadline,
			|target, response| {
				if Arc::ptr_eq(target, &session) {
					origin_response = response.cloned();
				}
			},
		)
		.await?;
		Ok(DataBreakpointsResult {
			snapshot: session.summary(),
			breakpoints: data_breakpoint_records(&current, origin_response.as_deref()),
		})
	}

	#[allow(clippy::too_many_arguments)]
	async fn sync_breakpoint_tree<F>(
		&self,
		origin: &Arc<DapSession>,
		tree: &[Arc<DapSession>],
		command: &str,
		arguments: Value,
		cancel: &CancellationToken,
		deadline: Instant,
		mut apply: F,
	) -> Result<(), DapError>
	where
		F: FnMut(&Arc<DapSession>, Option<&Vec<Breakpoint>>),
	{
		let mut ordered: Vec<Arc<DapSession>> = Vec::with_capacity(tree.len());
		ordered.push(Arc::clone(origin));
		ordered.extend(tree.iter().filter(|target| !Arc::ptr_eq(target, origin)).cloned());
		for target in &ordered {
			if cancel.is_cancelled() {
				return Err(DapError::Aborted);
			}
			let _queue = target.breakpoint_queue.lock().await;
			if cancel.is_cancelled() {
				return Err(DapError::Aborted);
			}
			let response = self
				.send_request_with_config(target, command, Some(arguments.clone()), cancel, deadline)
				.await;
			match response {
				Ok(body) => {
					let breakpoints = serde_json::from_value::<BreakpointsResponse>(body)
						.map(|response| response.breakpoints)
						.ok();
					apply(target, breakpoints.as_ref());
				},
				Err(error) if Arc::ptr_eq(target, origin) => return Err(error),
				Err(error) => {
					eprintln!(
						"Failed to synchronize {command} with child debug session {}: {error}",
						target.id
					);
				},
			}
		}
		Ok(())
	}

	async fn apply_root_breakpoints_to_session(
		&self,
		session: &Arc<DapSession>,
		cancel: &CancellationToken,
		deadline: Instant,
	) {
		let root = self.root_session(session);
		let sources: Vec<(String, Vec<BreakpointRecord>)> =
			lock(&root.breakpoints).iter().map(|(path, entries)| (path.clone(), entries.clone())).collect();
		for (source_path, entries) in sources {
			let result = session
				.client
				.send_request(
					"setBreakpoints",
					Some(source_breakpoints_arguments(&source_path, &entries)),
					cancel,
					deadline,
				)
				.await;
			match result {
				Ok(body) => {
					let response = serde_json::from_value::<BreakpointsResponse>(body).ok();
					lock(&session.breakpoints)
						.insert(source_path, map_source_breakpoints(&entries, response.as_ref().map(|r| &r.breakpoints)));
				},
				Err(error) => eprintln!("Failed to bind source breakpoints in child {}: {error}", session.id),
			}
		}
		let functions = lock(&root.function_breakpoints).clone();
		if !functions.is_empty() {
			let arguments = json!({
				"breakpoints": functions.iter().map(|entry| {
					let mut value = json!({ "name": entry.name });
					if let Some(condition) = &entry.condition {
						value["condition"] = Value::String(condition.clone());
					}
					value
				}).collect::<Vec<_>>()
			});
			match session.client.send_request("setFunctionBreakpoints", Some(arguments), cancel, deadline).await {
				Ok(body) => {
					let response = serde_json::from_value::<BreakpointsResponse>(body).ok();
					*lock(&session.function_breakpoints) =
						map_function_breakpoints(&functions, response.as_ref().map(|r| &r.breakpoints));
				},
				Err(error) => eprintln!("Failed to bind function breakpoints in child {}: {error}", session.id),
			}
		}
		let instructions = lock(&root.instruction_breakpoints).clone();
		if !instructions.is_empty() {
			if let Err(error) = session
				.client
				.send_request("setInstructionBreakpoints", Some(json!({ "breakpoints": instructions })), cancel, deadline)
				.await
			{
				eprintln!("Failed to bind instruction breakpoints in child {}: {error}", session.id);
			}
		}
		let data = lock(&root.data_breakpoints).clone();
		if !data.is_empty() {
			if let Err(error) = session
				.client
				.send_request("setDataBreakpoints", Some(json!({ "breakpoints": data })), cancel, deadline)
				.await
			{
				eprintln!("Failed to bind data breakpoints in child {}: {error}", session.id);
			}
		}
	}

	// ── execution ────────────────────────────────────────────────────────────

	pub async fn continue_execution(
		&self,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<ContinueOutcome, DapError> {
		let session = self.touch_active()?;
		let thread_id = self.resolve_thread_id(&session, cancel, deadline).await?;
		*lock(&session.stop) = StopLocation::default();
		lock(&session.last_stack_frames).clear();
		session.set_status(SessionStatus::Running);
		let (outcome, root_id) = self.subscribe_outcome(&session);
		self.send_request_with_config(&session, "continue", Some(json!({ "threadId": thread_id })), cancel, deadline)
			.await?;
		self.await_stop_outcome(session, outcome, &root_id, cancel, deadline).await
	}

	pub async fn step_in(
		&self,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<ContinueOutcome, DapError> {
		self.step("stepIn", cancel, deadline).await
	}

	pub async fn step_out(
		&self,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<ContinueOutcome, DapError> {
		self.step("stepOut", cancel, deadline).await
	}

	pub async fn step_over(
		&self,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<ContinueOutcome, DapError> {
		self.step("next", cancel, deadline).await
	}

	async fn step(
		&self,
		command: &str,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<ContinueOutcome, DapError> {
		let session = self.touch_active()?;
		let thread_id = self.resolve_thread_id(&session, cancel, deadline).await?;
		*lock(&session.stop) = StopLocation::default();
		lock(&session.last_stack_frames).clear();
		session.set_status(SessionStatus::Running);
		let (outcome, root_id) = self.subscribe_outcome(&session);
		self.send_request_with_config(&session, command, Some(json!({ "threadId": thread_id })), cancel, deadline)
			.await?;
		self.await_stop_outcome(session, outcome, &root_id, cancel, deadline).await
	}

	pub async fn pause(
		&self,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<SessionSummary, DapError> {
		let session = self.touch_active()?;
		if session.status() == SessionStatus::Stopped {
			return Ok(session.summary());
		}
		let thread_id = self.resolve_thread_id(&session, cancel, deadline).await?;
		let waiter = session
			.client
			.event_waiter("stopped", None::<EventPredicate>);
		self.send_request_with_config(&session, "pause", Some(json!({ "threadId": thread_id })), cancel, deadline)
			.await?;
		if session.status() != SessionStatus::Stopped {
			match waiter.wait(cancel, deadline).await {
				Ok(_) => {},
				Err(DapError::Aborted) => return Err(DapError::Aborted),
				Err(_) => {},
			}
		}
		Ok(session.summary())
	}

	async fn await_stop_outcome(
		&self,
		session: Arc<DapSession>,
		mut outcome: broadcast::Receiver<String>,
		root_id: &str,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<ContinueOutcome, DapError> {
		match self.await_outcome(&mut outcome, root_id, cancel, deadline).await {
			Ok(()) => {
				let result = match self.active_in_tree(&session) {
					Some(active) => active,
					None => session,
				};
				if result.status() == SessionStatus::Stopped {
					let frame_deadline = deadline.min(Instant::now() + STOP_CAPTURE_TIMEOUT);
					self.fetch_top_frame(&result, cancel, frame_deadline).await;
				}
				let state = match result.status() {
					SessionStatus::Stopped => ContinueState::Stopped,
					SessionStatus::Terminated => ContinueState::Terminated,
					_ => ContinueState::Running,
				};
				Ok(ContinueOutcome { snapshot: result.summary(), state, timed_out: false })
			},
			Err(DapError::Aborted) => Err(DapError::Aborted),
			Err(_) => {
				let result = match self.active_in_tree(&session) {
					Some(active) => active,
					None => session,
				};
				let state = if result.status() == SessionStatus::Running {
					ContinueState::Running
				} else {
					match result.status() {
						SessionStatus::Stopped => ContinueState::Stopped,
						SessionStatus::Terminated => ContinueState::Terminated,
						_ => ContinueState::Running,
					}
				};
				let timed_out = result.status() == SessionStatus::Running;
				Ok(ContinueOutcome { snapshot: result.summary(), state, timed_out })
			},
		}
	}

	pub async fn threads(
		&self,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<ThreadsResult, DapError> {
		let session = self.touch_active()?;
		let mut merged = Vec::new();
		let mut seen = HashSet::new();
		for target in self.live_tree_sessions(&session) {
			let response = self
				.send_request_with_config(&target, "threads", None, cancel, deadline)
				.await;
			let threads = match response {
				Ok(body) => serde_json::from_value::<ThreadsResponse>(body)
					.map(|response| response.threads)
					.unwrap_or_default(),
				Err(error) if cancel.is_cancelled() => return Err(error),
				Err(error) => {
					eprintln!("Failed to list threads for debug session {}: {error}", target.id);
					continue;
				},
			};
			*lock(&target.threads) = threads.clone();
			for thread in threads {
				if seen.insert(format!("{}\u{0}{}", target.id, thread.id)) {
					merged.push(thread);
				}
			}
		}
		Ok(ThreadsResult { snapshot: session.summary(), threads: merged })
	}

	pub async fn stack_trace(
		&self,
		levels: Option<i64>,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<StackTraceResult, DapError> {
		let session = self.touch_active()?;
		let thread_id = self.resolve_thread_id(&session, cancel, deadline).await?;
		let mut arguments = json!({ "threadId": thread_id });
		if let Some(levels) = levels {
			arguments["levels"] = json!(levels);
		}
		let body = self
			.send_request_with_config(&session, "stackTrace", Some(arguments), cancel, deadline)
			.await?;
		let response: StackTraceResponse = serde_json::from_value(body).unwrap_or_default();
		*lock(&session.last_stack_frames) = response.stack_frames.clone();
		apply_top_frame(&session, response.stack_frames.first());
		Ok(StackTraceResult { snapshot: session.summary(), stack_frames: response.stack_frames })
	}

	pub async fn scopes(
		&self,
		frame_id: Option<i64>,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<ScopesResult, DapError> {
		let session = self.touch_active()?;
		let resolved = frame_id.or(session.stop_location().frame_id).ok_or_else(|| {
			DapError::message("No active stack frame. Run stack_trace first or supply frame_id.")
		})?;
		let body = self
			.send_request_with_config(
				&session,
				"scopes",
				Some(json!({ "frameId": resolved })),
				cancel,
				deadline,
			)
			.await?;
		let response: ScopesResponse = serde_json::from_value(body).unwrap_or_default();
		Ok(ScopesResult { snapshot: session.summary(), scopes: response.scopes })
	}

	pub async fn variables(
		&self,
		variables_reference: i64,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<VariablesResult, DapError> {
		let session = self.touch_active()?;
		let body = self
			.send_request_with_config(
				&session,
				"variables",
				Some(json!({ "variablesReference": variables_reference })),
				cancel,
				deadline,
			)
			.await?;
		let response: VariablesResponse = serde_json::from_value(body).unwrap_or_default();
		Ok(VariablesResult { snapshot: session.summary(), variables: response.variables })
	}

	pub async fn evaluate(
		&self,
		expression: &str,
		context: &str,
		frame_id: Option<i64>,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<EvaluateResult, DapError> {
		let session = self.touch_active()?;
		let effective_frame = frame_id.or(session.stop_location().frame_id);
		let mut arguments = json!({ "expression": expression, "context": context });
		if let Some(frame_id) = effective_frame {
			arguments["frameId"] = json!(frame_id);
		}
		let body = self
			.send_request_with_config(&session, "evaluate", Some(arguments), cancel, deadline)
			.await?;
		let evaluation: EvaluateResponse = serde_json::from_value(body).unwrap_or_default();
		Ok(EvaluateResult { snapshot: session.summary(), evaluation })
	}

	#[allow(clippy::too_many_arguments)]
	pub async fn disassemble(
		&self,
		memory_reference: &str,
		instruction_count: i64,
		offset: Option<i64>,
		instruction_offset: Option<i64>,
		resolve_symbols: Option<bool>,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<DisassembleResult, DapError> {
		let session = self.touch_active()?;
		let mut arguments = json!({ "memoryReference": memory_reference, "instructionCount": instruction_count });
		if let Some(offset) = offset {
			arguments["offset"] = json!(offset);
		}
		if let Some(instruction_offset) = instruction_offset {
			arguments["instructionOffset"] = json!(instruction_offset);
		}
		if let Some(resolve_symbols) = resolve_symbols {
			arguments["resolveSymbols"] = json!(resolve_symbols);
		}
		let body = self
			.send_request_with_config(&session, "disassemble", Some(arguments), cancel, deadline)
			.await?;
		let response: DisassembleResponse = serde_json::from_value(body).unwrap_or_default();
		Ok(DisassembleResult { snapshot: session.summary(), instructions: response.instructions })
	}

	pub async fn read_memory(
		&self,
		memory_reference: &str,
		count: i64,
		offset: Option<i64>,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<ReadMemoryResult, DapError> {
		let session = self.touch_active()?;
		let mut arguments = json!({ "memoryReference": memory_reference, "count": count });
		if let Some(offset) = offset {
			arguments["offset"] = json!(offset);
		}
		let body = self
			.send_request_with_config(&session, "readMemory", Some(arguments), cancel, deadline)
			.await?;
		let response: ReadMemoryResponse = serde_json::from_value(body).unwrap_or_default();
		Ok(ReadMemoryResult {
			snapshot: session.summary(),
			address: if response.address.is_empty() {
				memory_reference.to_string()
			} else {
				response.address
			},
			data: response.data,
			unreadable_bytes: response.unreadable_bytes,
		})
	}

	pub async fn write_memory(
		&self,
		memory_reference: &str,
		data: &str,
		offset: Option<i64>,
		allow_partial: Option<bool>,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<WriteMemoryResult, DapError> {
		let session = self.touch_active()?;
		let mut arguments = json!({ "memoryReference": memory_reference, "data": data });
		if let Some(offset) = offset {
			arguments["offset"] = json!(offset);
		}
		if let Some(allow_partial) = allow_partial {
			arguments["allowPartial"] = json!(allow_partial);
		}
		let body = self
			.send_request_with_config(&session, "writeMemory", Some(arguments), cancel, deadline)
			.await?;
		let response: WriteMemoryResponse = serde_json::from_value(body).unwrap_or_default();
		Ok(WriteMemoryResult {
			snapshot: session.summary(),
			offset: response.offset,
			bytes_written: response.bytes_written,
		})
	}

	pub async fn modules(
		&self,
		start_module: Option<i64>,
		module_count: Option<i64>,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<ModulesResult, DapError> {
		let session = self.touch_active()?;
		let mut arguments = Map::new();
		if let Some(start_module) = start_module {
			arguments.insert("startModule".to_string(), json!(start_module));
		}
		if let Some(module_count) = module_count {
			arguments.insert("moduleCount".to_string(), json!(module_count));
		}
		let body = self
			.send_request_with_config(&session, "modules", Some(Value::Object(arguments)), cancel, deadline)
			.await?;
		let response: ModulesResponse = serde_json::from_value(body).unwrap_or_default();
		Ok(ModulesResult { snapshot: session.summary(), modules: response.modules })
	}

	pub async fn loaded_sources(
		&self,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<SourcesResult, DapError> {
		let session = self.touch_active()?;
		let body = self
			.send_request_with_config(&session, "loadedSources", Some(json!({})), cancel, deadline)
			.await?;
		let response: SourcesResponse = serde_json::from_value(body).unwrap_or_default();
		Ok(SourcesResult { snapshot: session.summary(), sources: response.sources })
	}

	pub async fn custom_request(
		&self,
		command: &str,
		arguments: Option<Value>,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<CustomRequestResult, DapError> {
		let session = self.touch_active()?;
		let body = self
			.send_request_with_config(&session, command, arguments, cancel, deadline)
			.await?;
		Ok(CustomRequestResult { snapshot: session.summary(), body })
	}

	pub fn get_output(&self) -> Result<OutputSnapshot, DapError> {
		let session = self.touch_active()?;
		let output = lock(&session.output).text();
		Ok(OutputSnapshot { snapshot: session.summary(), output })
	}

	pub async fn terminate(
		&self,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<Option<SessionSummary>, DapError> {
		let Some(session) = self.active_session() else {
			return Ok(None);
		};
		self.touch_session_and_ancestors(&session);
		let root = self.root_session(&session);
		let summary = session.summary();
		let tree = self.tree_sessions(&root);
		for node in tree.iter().rev() {
			node.set_status(SessionStatus::Terminated);
			if node.capability("supportsTerminateRequest") {
				let _ = node.client.send_request("terminate", None, cancel, deadline).await;
			}
			let _ = node
				.client
				.send_request("disconnect", Some(json!({ "terminateDebuggee": true })), cancel, deadline)
				.await;
		}
		self.dispose_session(&root).await;
		Ok(Some(summary))
	}

	// ── child sessions / reverse requests ────────────────────────────────────

	async fn start_child_session(
		&self,
		parent: &Arc<DapSession>,
		request: &str,
		configuration: Value,
	) -> Result<(), DapError> {
		if parent.adapter.connect_mode != super::config::ConnectMode::Tcp {
			return Err(DapError::message(format!(
				"DAP adapter {} cannot accept child session connections",
				parent.adapter.name
			)));
		}
		let Some(port) = parent.port else {
			return Err(DapError::message(format!(
				"DAP adapter {} cannot accept child session connections",
				parent.adapter.name
			)));
		};
		let cwd = {
			let relative = configuration.get("cwd").and_then(Value::as_str).unwrap_or(".");
			let candidate = PathBuf::from(relative);
			if candidate.is_absolute() {
				candidate
			} else {
				parent.cwd.join(candidate)
			}
		};
		let client = DapClient::connect(&parent.adapter, port).await?;
		let program = configuration
			.get("program")
			.and_then(Value::as_str)
			.map(str::to_string);
		let child = self.register(
			client,
			parent.adapter.clone(),
			cwd.clone(),
			program,
			Some(parent.id.clone()),
			parent.env.clone(),
		);
		let cancel = CancellationToken::new();
		let deadline = Instant::now() + DEFAULT_OPERATION_TIMEOUT;
		let result = async {
			self.initialize_session(&child, &cancel, deadline).await?;
			let mut arguments = configuration.clone();
			set_object_field(&mut arguments, "cwd", Value::String(cwd.to_string_lossy().into_owned()));
			drive_start_request(
				child.client.send_request(request, Some(arguments), &cancel, deadline),
				self.complete_configuration_handshake(&child, false, &cancel, deadline),
				request,
			)
			.await?;
			Ok(())
		}
		.await;
		if result.is_err() {
			self.dispose_session(&child).await;
		}
		result
	}

	#[cfg(test)]
	pub(crate) async fn register_for_test(
		&self,
		client: DapClient,
		adapter: ResolvedAdapter,
		cwd: PathBuf,
		program: Option<String>,
		env: HashMap<String, String>,
	) -> Arc<DapSession> {
		self.register(client, adapter, cwd, program, None, env)
	}

	/// Drive the real launch handshake against an in-process fake adapter.
	#[cfg(test)]
	pub(crate) async fn launch_with_client_for_test(
		&self,
		client: DapClient,
		adapter: ResolvedAdapter,
		cwd: PathBuf,
		program: String,
		env: HashMap<String, String>,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<SessionSummary, DapError> {
		let session = self.register(client, adapter, cwd.clone(), Some(program.clone()), None, env);
		self.initialize_session(&session, cancel, deadline).await?;
		let arguments = json!({
			"program": program,
			"cwd": cwd.to_string_lossy(),
		});
		self.start_root(&session, "launch", arguments, cancel, deadline).await
	}

	#[cfg(test)]
	pub(crate) fn active_session_for_test(&self) -> Option<Arc<DapSession>> {
		self.active_session()
	}

	#[cfg(test)]
	pub(crate) fn register_child_for_test(
		&self,
		parent: &Arc<DapSession>,
		client: DapClient,
		adapter: ResolvedAdapter,
		cwd: PathBuf,
		program: Option<String>,
		env: HashMap<String, String>,
	) -> Arc<DapSession> {
		self.register(client, adapter, cwd, program, Some(parent.id.clone()), env)
	}

	#[cfg(test)]
	pub(crate) async fn dispose_for_test(&self, session: &Arc<DapSession>) {
		self.dispose_session(session).await;
	}

	#[cfg(test)]
	pub(crate) async fn initialize_for_test(
		&self,
		session: &Arc<DapSession>,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<(), DapError> {
		self.initialize_session(session, cancel, deadline).await
	}

	#[cfg(test)]
	pub(crate) async fn handshake_for_test(
		&self,
		session: &Arc<DapSession>,
		tolerate_missing_initialized: bool,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<(), DapError> {
		self.complete_configuration_handshake(session, tolerate_missing_initialized, cancel, deadline)
			.await
	}

	/// Drive the real attach handshake against an in-process fake adapter.
	#[cfg(test)]
	pub(crate) async fn attach_with_client_for_test(
		&self,
		client: DapClient,
		adapter: ResolvedAdapter,
		cwd: PathBuf,
		env: HashMap<String, String>,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<SessionSummary, DapError> {
		let session = self.register(client, adapter.clone(), cwd.clone(), None, None, env.clone());
		let options = AttachOptions { adapter, cwd, pid: None, port: None, host: None, env };
		self.attach_registered(&session, &options, cancel, deadline).await
	}

	fn register(
		&self,
		client: DapClient,
		adapter: ResolvedAdapter,
		cwd: PathBuf,
		program: Option<String>,
		parent_session_id: Option<String>,
		env: HashMap<String, String>,
	) -> Arc<DapSession> {
		let id = format!("debug-{}", self.inner.next_id.fetch_add(1, Ordering::SeqCst) + 1);
		let session = Arc::new(DapSession {
			id: id.clone(),
			adapter,
			cwd,
			program,
			port: client.port(),
			client: client.clone(),
			env,
			status: StdMutex::new(SessionStatus::Launching),
			launched_at: now_millis(),
			last_used_at: AtomicI64::new(now_millis()),
			breakpoints: StdMutex::new(BTreeMap::new()),
			function_breakpoints: StdMutex::new(Vec::new()),
			instruction_breakpoints: StdMutex::new(Vec::new()),
			data_breakpoints: StdMutex::new(Vec::new()),
			breakpoint_queue: AsyncMutex::new(()),
			output: StdMutex::new(OutputRing::default()),
			stop: StdMutex::new(StopLocation::default()),
			threads: StdMutex::new(Vec::new()),
			last_stack_frames: StdMutex::new(Vec::new()),
			exit_code: StdMutex::new(None),
			capabilities: StdMutex::new(None),
			initialized_seen: AtomicBool::new(false),
			needs_configuration_done: AtomicBool::new(false),
			configuration_done_sent: AtomicBool::new(false),
			parent_session_id: parent_session_id.clone(),
			child_session_ids: StdMutex::new(BTreeSet::new()),
		});
		lock(&self.inner.sessions).insert(id.clone(), Arc::clone(&session));
		if let Some(parent_id) = &parent_session_id {
			if let Some(parent) = lock(&self.inner.sessions).get(parent_id).cloned() {
				lock(&parent.child_session_ids).insert(id.clone());
			}
		}
		self.install_handlers(&session);
		if !self.has_live_stopped_active_session() {
			*lock(&self.inner.active) = Some(id.clone());
		}
		self.start_heartbeat(&session);
		session
	}

	fn install_handlers(&self, session: &Arc<DapSession>) {
		let weak_session = Arc::downgrade(session);
		let weak_manager = Arc::downgrade(&self.inner);
		session.client.on_event(
			"output",
			Arc::new(move |body: Value| {
				let Some(session) = weak_session.upgrade() else {
					return;
				};
				let parsed: OutputEventBody = serde_json::from_value(body).unwrap_or_default();
				session.push_output(&parsed.output);
			}),
		);
		let weak_session = Arc::downgrade(session);
		session.client.on_event(
			"initialized",
			Arc::new(move |_body: Value| {
				let Some(session) = weak_session.upgrade() else {
					return;
				};
				session.initialized_seen.store(true, Ordering::SeqCst);
				if !session.configuration_done_sent.load(Ordering::SeqCst)
					&& session.status() == SessionStatus::Launching
				{
					session.set_status(SessionStatus::Configuring);
				}
			}),
		);
		let weak_session = Arc::downgrade(session);
		let weak_manager_stopped = Arc::downgrade(&self.inner);
		session.client.on_event(
			"stopped",
			Arc::new(move |body: Value| {
				let (Some(session), Some(inner)) = (weak_session.upgrade(), weak_manager_stopped.upgrade()) else {
					return;
				};
				let stopped: StoppedEventBody = serde_json::from_value(body).unwrap_or_default();
				session.set_status(SessionStatus::Stopped);
				*lock(&session.stop) = StopLocation {
					thread_id: stopped.thread_id,
					reason: (!stopped.reason.is_empty()).then_some(stopped.reason),
					description: stopped.description,
					text: stopped.text,
					..Default::default()
				};
				lock(&session.last_stack_frames).clear();
				*lock(&inner.active) = Some(session.id.clone());
				ManagerResolve::new(inner).resolve(&session);
			}),
		);
		let weak_session = Arc::downgrade(session);
		session.client.on_event(
			"continued",
			Arc::new(move |body: Value| {
				let Some(session) = weak_session.upgrade() else {
					return;
				};
				let continued: ContinuedEventBody = serde_json::from_value(body).unwrap_or_default();
				session.set_status(SessionStatus::Running);
				*lock(&session.stop) = StopLocation { thread_id: continued.thread_id, ..Default::default() };
				lock(&session.last_stack_frames).clear();
			}),
		);
		let weak_session = Arc::downgrade(session);
		let weak_manager = Arc::downgrade(&self.inner);
		session.client.on_event(
			"exited",
			Arc::new(move |body: Value| {
				let (Some(session), Some(inner)) = (weak_session.upgrade(), weak_manager.upgrade()) else {
					return;
				};
				let exited: ExitedEventBody = serde_json::from_value(body).unwrap_or_default();
				*lock(&session.exit_code) = exited.exit_code;
				session.set_status(SessionStatus::Terminated);
				ManagerResolve::new(inner).reactivate_and_resolve(&session);
			}),
		);
		let weak_session = Arc::downgrade(session);
		let weak_manager = Arc::downgrade(&self.inner);
		session.client.on_event(
			"terminated",
			Arc::new(move |_body: Value| {
				let (Some(session), Some(inner)) = (weak_session.upgrade(), weak_manager.upgrade()) else {
					return;
				};
				session.set_status(SessionStatus::Terminated);
				ManagerResolve::new(inner).reactivate_and_resolve(&session);
			}),
		);

		let weak_session = Arc::downgrade(session);
		let weak_manager = Arc::downgrade(&self.inner);
		session.client.on_reverse_request(
			"runInTerminal",
			Arc::new(RunInTerminalHandler { session: weak_session, manager: weak_manager }),
		);
		let weak_session = Arc::downgrade(session);
		let weak_manager = Arc::downgrade(&self.inner);
		session.client.on_reverse_request(
			"startDebugging",
			Arc::new(StartDebuggingHandler { session: weak_session, manager: weak_manager }),
		);
	}

	fn start_heartbeat(&self, session: &Arc<DapSession>) {
		let weak_session = Arc::downgrade(session);
		let weak_manager = Arc::downgrade(&self.inner);
		tokio::spawn(async move {
			let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
			ticker.tick().await;
			loop {
				ticker.tick().await;
				let (Some(session), Some(inner)) = (weak_session.upgrade(), weak_manager.upgrade()) else {
					break;
				};
				if !session.client.is_alive() {
					session.set_status(SessionStatus::Terminated);
					ManagerResolve::new(inner).reactivate_and_resolve(&session);
					break;
				}
			}
		});
	}

	// ── outcome waiters / tree helpers ───────────────────────────────────────

	fn subscribe_outcome(&self, session: &Arc<DapSession>) -> (broadcast::Receiver<String>, String) {
		let receiver = self.inner.outcomes.subscribe();
		(receiver, self.root_session(session).id.clone())
	}

	async fn await_outcome(
		&self,
		receiver: &mut broadcast::Receiver<String>,
		root_id: &str,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<(), DapError> {
		let now = Instant::now();
		if now >= deadline {
			return Err(DapError::message("DAP session tree outcome timed out after 0ms"));
		}
		let wait = deadline - now;
		let sleep = tokio::time::sleep(wait);
		tokio::pin!(sleep);
		loop {
			tokio::select! {
				biased;
				_ = cancel.cancelled() => return Err(DapError::Aborted),
				_ = &mut sleep => {
					return Err(DapError::message(format!(
						"DAP session tree outcome timed out after {}ms",
						wait.as_millis()
					)));
				},
				received = receiver.recv() => match received {
					Ok(id) if id == root_id => return Ok(()),
					Ok(_) => continue,
					Err(broadcast::error::RecvError::Closed) => {
						return Err(DapError::message("DAP session tree outcome channel closed"));
					},
					Err(broadcast::error::RecvError::Lagged(_)) => return Ok(()),
				},
			}
		}
	}

	fn resolve_tree_outcome(&self, session: &Arc<DapSession>) {
		let root_id = self.root_session(session).id.clone();
		let _ = self.inner.outcomes.send(root_id);
	}

	fn active_in_tree(&self, session: &Arc<DapSession>) -> Option<Arc<DapSession>> {
		let active = self.active_session()?;
		(self.root_session(&active).id == self.root_session(session).id).then_some(active)
	}

	fn has_live_stopped_active_session(&self) -> bool {
		self.active_session().is_some_and(|session| {
			session.status() == SessionStatus::Stopped && session.client.is_alive()
		})
	}

	fn reactivate_after_termination(&self, session: &Arc<DapSession>) {
		if lock(&self.inner.active).as_deref() != Some(session.id.as_str()) {
			return;
		}
		let live: Vec<Arc<DapSession>> = self
			.tree_sessions(session)
			.into_iter()
			.filter(|candidate| candidate.status() != SessionStatus::Terminated && candidate.client.is_alive())
			.collect();
		if live.is_empty() {
			return;
		}
		let replacement = live
			.iter()
			.find(|candidate| candidate.status() == SessionStatus::Stopped)
			.or_else(|| live.iter().find(|candidate| candidate.parent_session_id.is_some()))
			.or_else(|| live.first())
			.cloned();
		if let Some(replacement) = replacement {
			*lock(&self.inner.active) = Some(replacement.id.clone());
		}
	}

	fn root_session(&self, session: &Arc<DapSession>) -> Arc<DapSession> {
		let mut root = Arc::clone(session);
		loop {
			let Some(parent_id) = root.parent_session_id.clone() else {
				break;
			};
			let Some(parent) = lock(&self.inner.sessions).get(&parent_id).cloned() else {
				break;
			};
			root = parent;
		}
		root
	}

	fn tree_sessions(&self, session: &Arc<DapSession>) -> Vec<Arc<DapSession>> {
		self.subtree_sessions(&self.root_session(session))
	}

	/// Descendants of `session` only (parent first). Disposal from a child must
	/// never climb to a healthy root; explicit root callers still cover the tree.
	fn subtree_sessions(&self, session: &Arc<DapSession>) -> Vec<Arc<DapSession>> {
		let mut result = Vec::new();
		let mut stack = vec![Arc::clone(session)];
		while let Some(current) = stack.pop() {
			let children: Vec<String> = lock(&current.child_session_ids).iter().cloned().collect();
			result.push(Arc::clone(&current));
			for child_id in children {
				if let Some(child) = lock(&self.inner.sessions).get(&child_id).cloned() {
					stack.push(child);
				}
			}
		}
		result
	}

	fn live_tree_sessions(&self, session: &Arc<DapSession>) -> Vec<Arc<DapSession>> {
		let live: Vec<Arc<DapSession>> = self
			.tree_sessions(session)
			.into_iter()
			.filter(|candidate| candidate.status() != SessionStatus::Terminated && candidate.client.is_alive())
			.collect();
		if live.is_empty() {
			vec![Arc::clone(session)]
		} else {
			live
		}
	}

	fn touch_session_and_ancestors(&self, session: &Arc<DapSession>) {
		let now = now_millis();
		let mut current = Some(Arc::clone(session));
		while let Some(node) = current {
			node.last_used_at.store(now, Ordering::SeqCst);
			current = node
				.parent_session_id
				.as_ref()
				.and_then(|parent_id| lock(&self.inner.sessions).get(parent_id).cloned());
		}
	}

	async fn resolve_thread_id(
		&self,
		session: &Arc<DapSession>,
		cancel: &CancellationToken,
		deadline: Instant,
	) -> Result<i64, DapError> {
		if let Some(thread_id) = session.stop_location().thread_id {
			return Ok(thread_id);
		}
		{
			let threads = lock(&session.threads);
			if let Some(thread) = threads.first() {
				return Ok(thread.id);
			}
		}
		let body = session.client.send_request("threads", None, cancel, deadline).await?;
		let response: ThreadsResponse = serde_json::from_value(body).unwrap_or_default();
		*lock(&session.threads) = response.threads.clone();
		response.threads.first().map(|thread| thread.id).ok_or_else(|| {
			DapError::message("Debugger reported no threads.")
		})
	}

	async fn fetch_top_frame(&self, session: &Arc<DapSession>, cancel: &CancellationToken, deadline: Instant) {
		let Some(thread_id) = session.stop_location().thread_id else {
			return;
		};
		match session
			.client
			.send_request(
				"stackTrace",
				Some(json!({ "threadId": thread_id, "levels": 1 })),
				cancel,
				deadline,
			)
			.await
		{
			Ok(body) => {
				let response: StackTraceResponse = serde_json::from_value(body).unwrap_or_default();
				*lock(&session.last_stack_frames) = response.stack_frames.clone();
				apply_top_frame(session, response.stack_frames.first());
			},
			Err(error) => eprintln!("Failed to capture stopped frame for {}: {error}", session.id),
		}
	}

	// ── lifecycle ────────────────────────────────────────────────────────────

	async fn ensure_launch_slot(&self) -> Result<(), DapError> {
		let dead: Vec<Arc<DapSession>> = lock(&self.inner.sessions)
			.values()
			.filter(|session| session.status() == SessionStatus::Terminated || !session.client.is_alive())
			.cloned()
			.collect();
		for session in dead {
			self.dispose_session(&session).await;
		}
		let root = lock(&self.inner.sessions)
			.values()
			.find(|session| session.parent_session_id.is_none())
			.cloned();
		if let Some(root) = root {
			return Err(DapError::message(format!(
				"Debug session {} is still active. Terminate it before launching another.",
				root.id
			)));
		}
		Ok(())
	}

	async fn cleanup_idle_sessions(&self) {
		let now = now_millis();
		let expired: Vec<Arc<DapSession>> = lock(&self.inner.sessions)
			.values()
			.filter(|session| {
				session.status() == SessionStatus::Terminated
					|| now - session.last_used_at.load(Ordering::SeqCst) > IDLE_TIMEOUT.as_millis() as i64
					|| !session.client.is_alive()
			})
			.cloned()
			.collect();
		for session in expired {
			self.dispose_session(&session).await;
		}
	}

	async fn dispose_session(&self, session: &Arc<DapSession>) {
		// Walk down from the passed session only: child failure, idle cleanup
		// and dead-slot cleanup must not tear down a healthy root tree.
		let tree = self.subtree_sessions(session);
		for node in tree.iter().rev() {
			lock(&self.inner.sessions).remove(&node.id);
			if let Some(parent_id) = &node.parent_session_id {
				if let Some(parent) = lock(&self.inner.sessions).get(parent_id).cloned() {
					lock(&parent.child_session_ids).remove(&node.id);
				}
			}
			let replacement = node
				.parent_session_id
				.clone()
				.or_else(|| lock(&self.inner.sessions).keys().next().cloned());
			let mut active = lock(&self.inner.active);
			if active.as_deref() == Some(node.id.as_str()) {
				*active = replacement;
			}
		}
		for node in tree {
			node.client.dispose().await;
		}
	}
}

fn map_debugpy_missing_module(adapter_name: &str, error: DapError) -> DapError {
	if adapter_name != "debugpy" {
		return error;
	}
	let message = error.to_string();
	if message.contains("No module named") && message.contains("debugpy") {
		return DapError::message("adapter 'debugpy' is not available: install with 'pip install debugpy'");
	}
	error
}

/// Poll the launch/attach request and the `initialized` → `configurationDone`
/// handshake together. The request must reach the adapter before the adapter
/// emits `initialized`, and Rust futures are lazy, so neither future may be
/// left unpolled. Whichever side fails first returns after a bounded 50 ms
/// drain of the other, so a simultaneous failure is combined instead of
/// waiting for the full deadline; `Aborted` never drains. A genuine handshake
/// timeout propagates instead of being treated as success.
async fn drive_start_request<Start, Handshake>(
	start: Start,
	handshake: Handshake,
	command: &str,
) -> Result<(), DapError>
where
	Start: Future<Output = Result<Value, DapError>>,
	Handshake: Future<Output = Result<(), DapError>>,
{
	enum First {
		Start(Result<Value, DapError>),
		Handshake(Result<(), DapError>),
	}

	tokio::pin!(start);
	tokio::pin!(handshake);
	let first = tokio::select! {
		result = &mut start => First::Start(result),
		result = &mut handshake => First::Handshake(result),
	};
	match first {
		First::Start(Ok(_)) => handshake.await,
		First::Handshake(Ok(())) => start.await.map(|_| ()),
		First::Start(Err(start_error)) => {
			let configuration_error = if matches!(start_error, DapError::Aborted) {
				None
			} else {
				timeout(Duration::from_millis(50), &mut handshake).await.ok()
			};
			match configuration_error {
				Some(Err(configuration_error)) => {
					Err(combine_start_errors(command, &start_error, &configuration_error))
				},
				_ => Err(start_error),
			}
		},
		First::Handshake(Err(configuration_error)) => {
			// A handshake failure can arrive before the launch/attach response;
			// drain the request briefly so its error can be combined, then
			// surface the handshake error without consuming the full deadline.
			let start_error = if matches!(configuration_error, DapError::Aborted) {
				None
			} else {
				timeout(Duration::from_millis(50), &mut start).await.ok()
			};
			match start_error {
				Some(Err(start_error)) => Err(combine_start_errors(command, &start_error, &configuration_error)),
				_ => Err(configuration_error),
			}
		},
	}
}

fn combine_start_errors(command: &str, start: &DapError, configuration: &DapError) -> DapError {
	let start_message = start.to_string();
	let configuration_message = configuration.to_string();
	if start_message == configuration_message {
		return start.clone();
	}
	DapError::message(format!(
		"DAP {command} failed: {start_message}\nDAP configurationDone also failed: {configuration_message}"
	))
}

fn initialize_arguments(adapter: &ResolvedAdapter) -> Value {
	json!({
		"clientID": "omp",
		"clientName": "Oh My Pi",
		"adapterID": adapter.name,
		"locale": "en-US",
		"linesStartAt1": true,
		"columnsStartAt1": true,
		"pathFormat": "path",
		"supportsRunInTerminalRequest": true,
		"supportsStartDebuggingRequest": true,
		"supportsMemoryReferences": true,
		"supportsVariableType": true,
		"supportsInvalidatedEvent": true,
	})
}

fn merge_objects(base: &Value, overlay: &Value) -> Value {
	let mut merged = base.as_object().cloned().unwrap_or_default();
	if let Some(overlay) = overlay.as_object() {
		for (key, value) in overlay {
			merged.insert(key.clone(), value.clone());
		}
	}
	Value::Object(merged)
}

fn set_object_field(target: &mut Value, key: &str, value: Value) {
	if !target.is_object() {
		*target = Value::Object(Map::new());
	}
	target[key] = value;
}

fn source_breakpoints_arguments(source_path: &str, entries: &[BreakpointRecord]) -> Value {
	json!({
		"source": { "path": source_path, "name": basename(source_path) },
		"breakpoints": entries.iter().map(|entry| {
			let mut value = json!({ "line": entry.line });
			if let Some(condition) = &entry.condition {
				value["condition"] = Value::String(condition.clone());
			}
			value
		}).collect::<Vec<_>>()
	})
}

fn map_source_breakpoints(input: &[BreakpointRecord], response: Option<&Vec<Breakpoint>>) -> Vec<BreakpointRecord> {
	input
		.iter()
		.enumerate()
		.map(|(index, entry)| {
			let answered = response.and_then(|breakpoints| breakpoints.get(index));
			BreakpointRecord {
				line: entry.line,
				condition: entry.condition.clone(),
				id: answered.and_then(|breakpoint| breakpoint.id),
				verified: answered.map(|breakpoint| breakpoint.verified).unwrap_or(false),
				message: answered.and_then(|breakpoint| breakpoint.message.clone()),
			}
		})
		.collect()
}

fn map_function_breakpoints(
	input: &[FunctionBreakpointRecord],
	response: Option<&Vec<Breakpoint>>,
) -> Vec<FunctionBreakpointRecord> {
	input
		.iter()
		.enumerate()
		.map(|(index, entry)| {
			let answered = response.and_then(|breakpoints| breakpoints.get(index));
			FunctionBreakpointRecord {
				name: entry.name.clone(),
				condition: entry.condition.clone(),
				id: answered.and_then(|breakpoint| breakpoint.id),
				verified: answered.map(|breakpoint| breakpoint.verified).unwrap_or(false),
				message: answered.and_then(|breakpoint| breakpoint.message.clone()),
			}
		})
		.collect()
}

fn instruction_breakpoint_value(
	instruction_reference: &str,
	offset: Option<i64>,
	condition: Option<String>,
	hit_condition: Option<String>,
) -> Value {
	let mut value = json!({ "instructionReference": instruction_reference });
	if let Some(offset) = offset {
		value["offset"] = json!(offset);
	}
	if let Some(condition) = condition {
		value["condition"] = Value::String(condition);
	}
	if let Some(hit_condition) = hit_condition {
		value["hitCondition"] = Value::String(hit_condition);
	}
	value
}

fn instruction_breakpoint_records(
	input: &[Value],
	response: Option<&[Breakpoint]>,
) -> Vec<InstructionBreakpointRecord> {
	input
		.iter()
		.enumerate()
		.map(|(index, entry)| {
			let answered = response.and_then(|breakpoints| breakpoints.get(index));
			InstructionBreakpointRecord {
				id: answered.and_then(|breakpoint| breakpoint.id),
				verified: answered.map(|breakpoint| breakpoint.verified).unwrap_or(false),
				instruction_reference: answered
					.and_then(|breakpoint| breakpoint.instruction_reference.clone())
					.or_else(|| entry.get("instructionReference").and_then(Value::as_str).map(str::to_string))
					.unwrap_or_default(),
				offset: answered
					.and_then(|breakpoint| breakpoint.offset)
					.or_else(|| entry.get("offset").and_then(Value::as_i64)),
				condition: entry.get("condition").and_then(Value::as_str).map(str::to_string),
				hit_condition: entry.get("hitCondition").and_then(Value::as_str).map(str::to_string),
				message: answered.and_then(|breakpoint| breakpoint.message.clone()),
			}
		})
		.collect()
}

fn data_breakpoint_value(
	data_id: &str,
	access_type: Option<String>,
	condition: Option<String>,
	hit_condition: Option<String>,
) -> Value {
	let mut value = json!({ "dataId": data_id });
	if let Some(access_type) = access_type {
		value["accessType"] = Value::String(access_type);
	}
	if let Some(condition) = condition {
		value["condition"] = Value::String(condition);
	}
	if let Some(hit_condition) = hit_condition {
		value["hitCondition"] = Value::String(hit_condition);
	}
	value
}

fn data_breakpoint_records(input: &[Value], response: Option<&[Breakpoint]>) -> Vec<DataBreakpointRecord> {
	input
		.iter()
		.enumerate()
		.map(|(index, entry)| {
			let answered = response.and_then(|breakpoints| breakpoints.get(index));
			DataBreakpointRecord {
				id: answered.and_then(|breakpoint| breakpoint.id),
				verified: answered.map(|breakpoint| breakpoint.verified).unwrap_or(false),
				data_id: entry.get("dataId").and_then(Value::as_str).unwrap_or_default().to_string(),
				access_type: entry.get("accessType").and_then(Value::as_str).map(str::to_string),
				condition: entry.get("condition").and_then(Value::as_str).map(str::to_string),
				hit_condition: entry.get("hitCondition").and_then(Value::as_str).map(str::to_string),
				message: answered.and_then(|breakpoint| breakpoint.message.clone()),
			}
		})
		.collect()
}

fn apply_top_frame(session: &Arc<DapSession>, frame: Option<&StackFrame>) {
	let Some(frame) = frame else {
		return;
	};
	let mut stop = lock(&session.stop);
	stop.frame_id = Some(frame.id);
	stop.frame_name = Some(frame.name.clone());
	stop.instruction_pointer_reference = frame.instruction_pointer_reference.clone();
	stop.source = frame.source.clone();
	stop.line = Some(frame.line);
	stop.column = Some(frame.column);
}

/// Small helper so event closures can resolve/re-activate without borrowing
/// a `DapSessionManager` (which they do not own).
struct ManagerResolve {
	inner: Arc<ManagerInner>,
}

impl ManagerResolve {
	fn new(inner: Arc<ManagerInner>) -> Self {
		Self { inner }
	}

	fn resolve(&self, session: &Arc<DapSession>) {
		DapSessionManager::from_inner(Arc::clone(&self.inner)).resolve_tree_outcome(session);
	}

	fn reactivate_and_resolve(&self, session: &Arc<DapSession>) {
		let manager = DapSessionManager::from_inner(Arc::clone(&self.inner));
		manager.reactivate_after_termination(session);
		manager.resolve_tree_outcome(session);
	}
}

struct RunInTerminalHandler {
	session: Weak<DapSession>,
	manager: Weak<ManagerInner>,
}

#[async_trait]
impl ReverseRequestHandler for RunInTerminalHandler {
	async fn handle(&self, arguments: Value) -> Result<Value, DapError> {
		let Some(session) = self.session.upgrade() else {
			return Err(DapError::message("DAP session is gone"));
		};
		let _ = self.manager.upgrade();
		let args: RunInTerminalArguments =
			serde_json::from_value(arguments).map_err(|error| DapError::message(error.to_string()))?;
		let Some(program) = args.args.first().cloned() else {
			return Err(DapError::message("runInTerminal request did not include a command"));
		};
		let cwd = {
			let candidate = PathBuf::from(args.cwd.as_deref().unwrap_or("."));
			if candidate.is_absolute() {
				candidate
			} else {
				session.cwd.join(candidate)
			}
		};
		let mut command = Command::new(program);
		command.args(args.args.iter().skip(1));
		command.current_dir(cwd);
		command.stdin(Stdio::null());
		command.stdout(Stdio::piped());
		command.stderr(Stdio::null());
		command.envs(non_interactive_env());
		command.envs(session.env.iter().filter(|(_, value)| !value.is_empty()));
		for (key, value) in &args.env {
			if let Some(value) = value {
				command.env(key, value);
			}
		}
		apply_detached(&mut command);
		let mut child = command
			.spawn()
			.map_err(|error| DapError::message(format!("runInTerminal failed to start debuggee: {error}")))?;
		let process_id = child.id();
		let mut stdout = child.stdout.take();
		let output_session = Arc::clone(&session);
		tokio::spawn(async move {
			if let Some(stdout) = stdout.as_mut() {
				let mut chunk = [0u8; 4096];
				while let Ok(read) = stdout.read(&mut chunk).await {
					if read == 0 {
						break;
					}
					output_session.push_output(&String::from_utf8_lossy(&chunk[..read]));
				}
			}
			let _ = child.wait().await;
		});
		Ok(match process_id {
			Some(pid) => json!({ "processId": pid }),
			None => json!({}),
		})
	}
}

struct StartDebuggingHandler {
	session: Weak<DapSession>,
	manager: Weak<ManagerInner>,
}

#[async_trait]
impl ReverseRequestHandler for StartDebuggingHandler {
	async fn handle(&self, arguments: Value) -> Result<Value, DapError> {
		let (Some(session), Some(inner)) = (self.session.upgrade(), self.manager.upgrade()) else {
			return Err(DapError::message("DAP session is gone"));
		};
		let args: StartDebuggingArguments =
			serde_json::from_value(arguments).unwrap_or_default();
		let request = if args.request.as_deref() == Some("attach") { "attach" } else { "launch" };
		let configuration = if args.configuration.is_object() {
			args.configuration
		} else {
			Value::Object(Map::new())
		};
		DapSessionManager::from_inner(inner)
			.start_child_session(&session, request, configuration)
			.await?;
		Ok(json!({}))
	}
}
