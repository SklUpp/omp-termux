//! Session-scoped staging queue for preview→apply tool actions.
//!
//! A tool that stages work (e.g. a future `ast_edit` preview) registers a
//! [`PendingAction`] here and tells the model to `write` a reason to
//! `xd://resolve` (apply) or `xd://reject` (discard). The store owns the
//! lifecycle: one entry is handed to exactly one resolve/reject call, a failed
//! apply requeues the same entry, and a clear invalidates any resolve/reject
//! already in flight so it cannot resurrect a discarded preview.
//!
//! The owner is the runner session, not a [`ToolRegistry`]: registries are
//! rebuilt for every prompt, so a registry-local queue would forget a staged
//! preview between turns. Child agents get their own store.

use std::{
	collections::VecDeque,
	sync::{Arc, Mutex, MutexGuard},
};

use async_trait::async_trait;

use super::{ToolError, tool_result};
use crate::state::{AgentContext, ToolResult};

/// Plain-text resolution devices (`xd://resolve`, `xd://reject`).
pub const RESOLVE_DEVICE: &str = "resolve";
pub const REJECT_DEVICE: &str = "reject";
pub const RESOLVE_DEVICE_PATH: &str = "xd://resolve";
pub const REJECT_DEVICE_PATH: &str = "xd://reject";

/// Parsed `xd://<device>` target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XdTarget<'a> {
	/// `xd://<device>`; the name is empty for bare `xd://`.
	Device(&'a str),
	/// Starts with `xd://` but the name carries URL delimiters.
	Invalid,
}

/// Parse a trimmed, case-insensitive `xd://<name>` write/read target.
///
/// Returns `None` for anything that is not an `xd://` URL at all, so callers
/// can tell an ordinary path from a device dispatch or a malformed URL.
pub fn parse_xd_target(input: &str) -> Option<XdTarget<'_>> {
	let trimmed = input.trim();
	let prefix = trimmed.get(..5)?;
	if !prefix.eq_ignore_ascii_case("xd://") {
		return None;
	}
	let name = &trimmed[5..];
	if name.contains(['/', '?', '#']) {
		return Some(XdTarget::Invalid);
	}
	Some(XdTarget::Device(name))
}

/// One-line usage string returned by `read xd://<device>`.
pub fn resolution_device_usage(device: &str) -> Option<&'static str> {
	match device {
		RESOLVE_DEVICE => Some(
			"Write a one-sentence reason as plain text to xd://resolve to APPLY the pending staged action (e.g. a tool preview).",
		),
		REJECT_DEVICE => Some(
			"Write a one-sentence reason as plain text to xd://reject to DISCARD the pending staged action (e.g. a tool preview).",
		),
		_ => None,
	}
}

/// Whether `path` is `xd://resolve` or `xd://reject`.
pub fn is_resolution_device_path(path: &str) -> bool {
	matches!(parse_xd_target(path), Some(XdTarget::Device(device)) if resolution_device_usage(device).is_some())
}

/// One staged action: the mutation applied by `xd://resolve` and the optional
/// cleanup/undo run by `xd://reject`.
#[async_trait]
pub trait PendingAction: Send + Sync {
	/// Human label used in the default discard result and dispatch details.
	fn label(&self) -> &str;
	/// Tool that staged this action (`ast_edit`, …).
	fn source_tool(&self) -> &str;

	/// Apply the staged change. An `Err` leaves the entry pending; a cancelled
	/// run must surface `ToolError::Aborted` rather than dropping the future,
	/// so the same entry can be retried or rejected.
	async fn apply(&self, ctx: &AgentContext, reason: &str) -> Result<ToolResult, ToolError>;

	/// Discard the staged change. The default has no side effects beyond
	/// consuming the entry.
	async fn reject(&self, _ctx: &AgentContext, reason: &str) -> Result<ToolResult, ToolError> {
		Ok(tool_result(format!("Discarded: {}. Reason: {reason}", self.label()), false))
	}
}

/// Stable identity of the newest staged action, returned by
/// [`PendingStore::peek_head`].
///
/// `generation` is the queue generation the head was observed in. Neither
/// `id` nor `generation` is enforced by resolve/reject: those always act on
/// the current head. A caller that wants to act on this specific entry (e.g. a
/// reminder or force reset) must compare both `id` and `generation` against a
/// fresh [`PendingStore::peek_head`]; a `clear` bumps the generation and a new
/// [`PendingStore::register`] assigns a new id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingHead {
	pub id:          u64,
	pub source_tool: String,
	pub label:       String,
	pub generation:  u64,
}

/// One queued action with the id assigned when it was registered.
struct Entry {
	id:     u64,
	action: Arc<dyn PendingAction>,
}

/// Queue contents and the generation used to invalidate in-flight
/// resolve/reject futures; both live under a single lock so a peek and its
/// generation are always a consistent snapshot.
struct State {
	entries:    VecDeque<Entry>,
	generation: u64,
	next_id:    u64,
}

/// Stack of staged actions, newest last (upstream `peekPendingHead`).
///
/// The store is shared across prompts by the runner session; every mutating
/// method locks, and [`PendingStore::resolve`]/[`PendingStore::reject`] remove
/// the head *before* awaiting `apply`, so concurrent or repeated resolves can
/// never run the same action twice. A dropped or failed resolve/reject future
/// requeues its action through an RAII guard, but only while the generation it
/// captured with the take is still current: a [`PendingStore::clear`] during
/// the await discards the in-flight entry instead of resurrecting it.
pub struct PendingStore {
	state: Mutex<State>,
}

impl Default for PendingStore {
	fn default() -> Self {
		Self {
			state: Mutex::new(State { entries: VecDeque::new(), generation: 0, next_id: 1 }),
		}
	}
}

impl PendingStore {
	pub fn new() -> Self {
		Self::default()
	}

	fn state(&self) -> MutexGuard<'_, State> {
		self.state.lock().unwrap_or_else(|poison| poison.into_inner())
	}

	/// Stage an action as the newest head, returning its stable id. The id
	/// identifies the entry for [`PendingStore::peek_head`] callers; it does
	/// not order the queue.
	pub fn register(&self, action: Arc<dyn PendingAction>) -> u64 {
		let mut state = self.state();
		let id = state.next_id;
		state.next_id = state.next_id.wrapping_add(1);
		state.entries.push_back(Entry { id, action });
		id
	}

	/// Inspect the newest staged action without consuming it.
	pub fn peek(&self) -> Option<Arc<dyn PendingAction>> {
		self.state().entries.back().map(|entry| Arc::clone(&entry.action))
	}

	/// Snapshot the newest staged action's identity, atomically with the queue
	/// generation, without consuming it.
	pub fn peek_head(&self) -> Option<PendingHead> {
		let state = self.state();
		state.entries.back().map(|entry| PendingHead {
			id:          entry.id,
			source_tool: entry.action.source_tool().to_string(),
			label:       entry.action.label().to_string(),
			generation:  state.generation,
		})
	}

	pub fn generation(&self) -> u64 {
		self.state().generation
	}

	pub fn len(&self) -> usize {
		self.state().entries.len()
	}

	pub fn is_empty(&self) -> bool {
		self.state().entries.is_empty()
	}

	/// Drop every staged action (session end, phantom drain) and invalidate any
	/// resolve/reject already in flight, so they cannot requeue afterwards.
	pub fn clear(&self) {
		let mut state = self.state();
		state.entries.clear();
		state.generation = state.generation.wrapping_add(1);
	}

	/// Pop the newest action together with the generation it was taken in.
	/// An empty queue is a plain miss; draining nothing must not invalidate
	/// entries still held by concurrent resolve/reject futures.
	fn take(&self) -> Option<(Entry, u64)> {
		let mut state = self.state();
		let generation = state.generation;
		state.entries.pop_back().map(|entry| (entry, generation))
	}

	/// Restore a failed, cancelled, or dropped action to its head position
	/// without reordering the previews staged before it. A `clear` since the
	/// take bumped the generation, so the stale entry is dropped instead.
	fn requeue(&self, entry: Entry, generation: u64) {
		let mut state = self.state();
		if state.generation == generation {
			state.entries.push_back(entry);
		}
	}

	/// Apply the newest action. With nothing pending this is an error.
	///
	/// A failed or cancelled `apply`, a dropped future, and an `is_error`
	/// result all leave the same entry pending for a retry or a later reject,
	/// unless a `clear` happened meanwhile.
	pub async fn resolve(&self, ctx: &AgentContext, reason: &str) -> Result<ToolResult, ToolError> {
		let Some((entry, generation)) = self.take() else {
			return Err(ToolError::ExecutionFailed(format!(
				"No pending action to apply — {RESOLVE_DEVICE_PATH} is only valid while a staged preview is pending."
			)));
		};
		let mut guard = RequeueOnDrop::new(self, entry, generation);
		match guard.action().apply(ctx, reason).await {
			Ok(result) if !result.is_error => {
				let decorated = resolve_details(result, "apply", reason, guard.action());
				guard.consume();
				Ok(decorated)
			},
			// `is_error` is the tool-level failure channel: keep the entry.
			Ok(result) => Ok(resolve_details(result, "apply", reason, guard.action())),
			Err(err) => Err(err),
		}
	}

	/// Discard the newest action. With nothing pending this succeeds: rejecting
	/// asks to reach the "no staged change" end-state, which already holds.
	pub async fn reject(&self, ctx: &AgentContext, reason: &str) -> Result<ToolResult, ToolError> {
		let Some((entry, generation)) = self.take() else {
			let mut result = tool_result("Nothing to reject; no pending action remains.", false);
			result.details = Some(serde_json::json!({"action": "discard", "reason": reason}));
			return Ok(result);
		};
		let mut guard = RequeueOnDrop::new(self, entry, generation);
		match guard.action().reject(ctx, reason).await {
			Ok(result) if !result.is_error => {
				let decorated = resolve_details(result, "discard", reason, guard.action());
				guard.consume();
				Ok(decorated)
			},
			Ok(result) => Ok(resolve_details(result, "discard", reason, guard.action())),
			Err(err) => Err(err),
		}
	}
}

/// Requeue the entry unless the call proved it was consumed, so a dropped
/// resolve/reject future (interrupt mid-apply) cannot lose the staged action.
/// Entries staged before a `clear` are dropped rather than resurrected.
struct RequeueOnDrop<'a> {
	store:      &'a PendingStore,
	entry:      Option<Entry>,
	generation: u64,
}

impl<'a> RequeueOnDrop<'a> {
	fn new(store: &'a PendingStore, entry: Entry, generation: u64) -> Self {
		Self { store, entry: Some(entry), generation }
	}

	fn action(&self) -> &dyn PendingAction {
		self.entry.as_ref().expect("pending action armed").action.as_ref()
	}

	/// Mark the action consumed so `Drop` leaves the queue alone.
	fn consume(&mut self) {
		self.entry.take();
	}
}

impl Drop for RequeueOnDrop<'_> {
	fn drop(&mut self) {
		if let Some(entry) = self.entry.take() {
			self.store.requeue(entry, self.generation);
		}
	}
}

/// Wrap an action result with the upstream `ResolveDetails` shape.
fn resolve_details(
	mut result: ToolResult,
	action: &str,
	reason: &str,
	pending: &dyn PendingAction,
) -> ToolResult {
	let source = result.details.take();
	let mut details = serde_json::json!({
		"action": action,
		"reason": reason,
		"sourceToolName": pending.source_tool(),
		"label": pending.label(),
	});
	if let Some(source) = source {
		details["sourceResultDetails"] = source;
	}
	result.details = Some(details);
	result
}

#[cfg(test)]
mod tests {
	use std::{
		path::PathBuf,
		sync::{
			Arc,
			atomic::{AtomicBool, AtomicUsize, Ordering},
		},
		time::{SystemTime, UNIX_EPOCH},
	};

	use serde_json::json;
	use tokio::sync::Notify;

	use super::*;
	use crate::tools::{ReadTool, Tool, WriteTool};

	struct TempDir(PathBuf);

	impl TempDir {
		fn new(tag: &str) -> Self {
			let nanos = SystemTime::now()
				.duration_since(UNIX_EPOCH)
				.expect("clock")
				.as_nanos();
			let path = std::env::temp_dir().join(format!("omp-pending-{tag}-{nanos}"));
			std::fs::create_dir_all(&path).expect("mkdir");
			Self(path)
		}
	}

	impl Drop for TempDir {
		fn drop(&mut self) {
			std::fs::remove_dir_all(&self.0).ok();
		}
	}

	/// Real action: writes a file and counts apply attempts.
	struct FileAction {
		path:  PathBuf,
		calls: AtomicUsize,
		fail:  AtomicBool,
		abort: AtomicBool,
	}

	impl FileAction {
		fn new(path: PathBuf) -> Self {
			Self { path, calls: AtomicUsize::new(0), fail: AtomicBool::new(false), abort: AtomicBool::new(false) }
		}
	}

	#[async_trait]
	impl PendingAction for FileAction {
		fn label(&self) -> &str {
			"test edit"
		}

		fn source_tool(&self) -> &str {
			"ast_edit"
		}

		async fn apply(&self, _ctx: &AgentContext, reason: &str) -> Result<ToolResult, ToolError> {
			self.calls.fetch_add(1, Ordering::SeqCst);
			if self.abort.load(Ordering::SeqCst) {
				return Err(ToolError::Aborted("cancelled".to_string()));
			}
			if self.fail.load(Ordering::SeqCst) {
				return Err(ToolError::ExecutionFailed("preview is stale".to_string()));
			}
			std::fs::write(&self.path, format!("applied: {reason}\n"))
				.map_err(|err| ToolError::ExecutionFailed(err.to_string()))?;
			Ok(tool_result("applied", false))
		}
	}

	fn ctx(dir: &TempDir) -> AgentContext {
		AgentContext::new(dir.0.clone())
	}

	async fn dispatch(tool: &WriteTool, path: &str, reason: &str, ctx: &AgentContext) -> Result<ToolResult, ToolError> {
		tool.execute(&json!({"path": path, "content": reason}), ctx).await
	}

	#[tokio::test]
	async fn resolve_consumes_the_entry_exactly_once() {
		let dir = TempDir::new("consume");
		let store = PendingStore::new();
		let target = dir.0.join("out.txt");
		store.register(Arc::new(FileAction::new(target.clone())));
		let ctx = ctx(&dir);

		let applied = store.resolve(&ctx, "looks right").await.expect("apply");
		assert!(!applied.is_error);
		assert_eq!(std::fs::read_to_string(&target).expect("read"), "applied: looks right\n");
		assert!(store.is_empty());

		let error = store.resolve(&ctx, "again").await.expect_err("second resolve");
		assert!(error.to_string().contains("No pending action to apply"));
	}

	#[tokio::test]
	async fn failed_apply_requeues_the_same_entry() {
		let dir = TempDir::new("requeue");
		let store = PendingStore::new();
		let file = Arc::new(FileAction::new(dir.0.join("out.txt")));
		let action: Arc<dyn PendingAction> = file.clone();
		store.register(action.clone());
		file.fail.store(true, Ordering::SeqCst);
		let ctx = ctx(&dir);

		let error = store.resolve(&ctx, "first").await.expect_err("stale apply");
		assert!(error.to_string().contains("preview is stale"));
		assert_eq!(store.len(), 1);
		assert!(Arc::ptr_eq(&store.peek().expect("requeued"), &action));
		assert_eq!(file.calls.load(Ordering::SeqCst), 1);

		// The same entry applies on retry; no duplicate registration needed.
		file.fail.store(false, Ordering::SeqCst);
		let retried = store.resolve(&ctx, "second").await.expect("retry");
		assert!(!retried.is_error);
		assert_eq!(file.calls.load(Ordering::SeqCst), 2);
		assert_eq!(
			std::fs::read_to_string(dir.0.join("out.txt")).expect("read"),
			"applied: second\n"
		);
	}

	#[tokio::test]
	async fn cancelled_apply_requeues_the_same_entry() {
		let dir = TempDir::new("cancel");
		let store = PendingStore::new();
		let file = Arc::new(FileAction::new(dir.0.join("out.txt")));
		file.abort.store(true, Ordering::SeqCst);
		let action: Arc<dyn PendingAction> = file.clone();
		store.register(action.clone());
		let ctx = ctx(&dir);

		let error = store.resolve(&ctx, "go").await.expect_err("cancelled");
		assert!(matches!(error, ToolError::Aborted(_)));
		assert!(Arc::ptr_eq(&store.peek().expect("requeued"), &action));
		assert_eq!(file.calls.load(Ordering::SeqCst), 1);
	}

	/// Returns a tool-level error result on the first apply: an unsuccessful
	/// consume must leave the entry queued.
	struct ErrorResultAction {
		path:  PathBuf,
		calls: AtomicUsize,
	}

	#[async_trait]
	impl PendingAction for ErrorResultAction {
		fn label(&self) -> &str {
			"error result edit"
		}

		fn source_tool(&self) -> &str {
			"ast_edit"
		}

		async fn apply(&self, _ctx: &AgentContext, reason: &str) -> Result<ToolResult, ToolError> {
			if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
				return Ok(tool_result("preview is stale", true));
			}
			std::fs::write(&self.path, format!("applied: {reason}\n"))
				.map_err(|err| ToolError::ExecutionFailed(err.to_string()))?;
			Ok(tool_result("applied", false))
		}
	}

	#[tokio::test]
	async fn error_result_keeps_the_entry_pending() {
		let dir = TempDir::new("error-result");
		let store = PendingStore::new();
		let target = dir.0.join("out.txt");
		let file = Arc::new(ErrorResultAction { path: target.clone(), calls: AtomicUsize::new(0) });
		let action: Arc<dyn PendingAction> = file.clone();
		store.register(action.clone());
		let ctx = ctx(&dir);

		let failed = store.resolve(&ctx, "first").await.expect("error result");
		assert!(failed.is_error);
		assert_eq!(store.len(), 1);
		assert!(Arc::ptr_eq(&store.peek().expect("requeued"), &action));
		assert!(!target.exists());

		let retried = store.resolve(&ctx, "second").await.expect("retry");
		assert!(!retried.is_error);
		assert_eq!(std::fs::read_to_string(&target).expect("read"), "applied: second\n");
		assert!(store.is_empty());
	}

	#[tokio::test]
	async fn requeue_keeps_newest_first_order() {
		let dir = TempDir::new("order");
		let store = PendingStore::new();
		let older = Arc::new(FileAction::new(dir.0.join("older.txt")));
		let newest = Arc::new(FileAction::new(dir.0.join("newest.txt")));
		let older_ref: Arc<dyn PendingAction> = older.clone();
		let newest_ref: Arc<dyn PendingAction> = newest.clone();
		store.register(older_ref.clone());
		store.register(newest_ref.clone());
		assert!(Arc::ptr_eq(&store.peek().expect("head"), &newest_ref));

		// A failed apply of the head requeues it *above* the older preview.
		newest.fail.store(true, Ordering::SeqCst);
		let ctx = ctx(&dir);
		store.resolve(&ctx, "stale").await.expect_err("stale head");
		assert_eq!(store.len(), 2);
		assert!(Arc::ptr_eq(&store.peek().expect("head"), &newest_ref));

		newest.fail.store(false, Ordering::SeqCst);
		store.resolve(&ctx, "newest done").await.expect("newest");
		assert_eq!(
			std::fs::read_to_string(dir.0.join("newest.txt")).expect("read"),
			"applied: newest done\n"
		);
		assert!(Arc::ptr_eq(&store.peek().expect("next head"), &older_ref));

		store.resolve(&ctx, "older done").await.expect("older");
		assert!(store.is_empty());
		assert_eq!(
			std::fs::read_to_string(dir.0.join("older.txt")).expect("read"),
			"applied: older done\n"
		);
	}

	/// Holds `apply` until the test drops the resolve future.
	struct SlowAction {
		path:    PathBuf,
		entered: Arc<Notify>,
		gate:    Arc<Notify>,
	}

	#[async_trait]
	impl PendingAction for SlowAction {
		fn label(&self) -> &str {
			"slow edit"
		}

		fn source_tool(&self) -> &str {
			"ast_edit"
		}

		async fn apply(&self, _ctx: &AgentContext, reason: &str) -> Result<ToolResult, ToolError> {
			self.entered.notify_one();
			self.gate.notified().await;
			std::fs::write(&self.path, format!("applied: {reason}\n"))
				.map_err(|err| ToolError::ExecutionFailed(err.to_string()))?;
			Ok(tool_result("applied", false))
		}
	}

	/// Holds `apply` until the test releases it, then fails.
	struct SlowFailAction {
		entered: Arc<Notify>,
		gate:    Arc<Notify>,
	}

	#[async_trait]
	impl PendingAction for SlowFailAction {
		fn label(&self) -> &str {
			"slow stale edit"
		}

		fn source_tool(&self) -> &str {
			"ast_edit"
		}

		async fn apply(&self, _ctx: &AgentContext, _reason: &str) -> Result<ToolResult, ToolError> {
			self.entered.notify_one();
			self.gate.notified().await;
			Err(ToolError::ExecutionFailed("preview is stale".to_string()))
		}
	}

	#[tokio::test]
	async fn dropping_resolve_future_requeues_the_entry() {
		let dir = TempDir::new("dropped");
		let target = dir.0.join("out.txt");
		let store = Arc::new(PendingStore::new());
		let slow = Arc::new(SlowAction {
			path:    target.clone(),
			entered: Arc::new(Notify::new()),
			gate:    Arc::new(Notify::new()),
		});
		let action: Arc<dyn PendingAction> = slow.clone();
		store.register(action.clone());
		let ctx = ctx(&dir);

		let handle = {
			let store = store.clone();
			let ctx = ctx.clone();
			tokio::spawn(async move { store.resolve(&ctx, "aborted").await })
		};
		slow.entered.notified().await;
		handle.abort();
		assert!(handle.await.expect_err("cancelled task").is_cancelled());

		// The RAII guard returned the same entry; no partial write happened.
		assert_eq!(store.len(), 1);
		assert!(Arc::ptr_eq(&store.peek().expect("requeued"), &action));
		assert!(!target.exists());

		slow.gate.notify_one();
		let retried = store.resolve(&ctx, "retry").await.expect("retry");
		assert!(!retried.is_error);
		assert_eq!(std::fs::read_to_string(&target).expect("read"), "applied: retry\n");
		assert!(store.is_empty());
	}

	#[tokio::test]
	async fn head_identity_is_stable_across_retry_and_fresh_per_register() {
		let dir = TempDir::new("identity");
		let store = PendingStore::new();
		let file = Arc::new(FileAction::new(dir.0.join("out.txt")));
		file.fail.store(true, Ordering::SeqCst);
		let registered = store.register(file.clone());
		let ctx = ctx(&dir);

		let head = store.peek_head().expect("head");
		assert_eq!(head.id, registered);
		assert_eq!(head.source_tool, "ast_edit");
		assert_eq!(head.label, "test edit");
		assert_eq!(head.generation, store.generation());

		store.resolve(&ctx, "stale").await.expect_err("stale apply");
		let retried = store.peek_head().expect("requeued head");
		assert_eq!(retried.id, registered, "a retry keeps the staged id");
		assert_eq!(retried.generation, head.generation);

		file.fail.store(false, Ordering::SeqCst);
		store.resolve(&ctx, "ok").await.expect("apply");
		assert!(store.peek_head().is_none());

		let older = store.register(Arc::new(FileAction::new(dir.0.join("older.txt"))));
		let newer = store.register(Arc::new(FileAction::new(dir.0.join("newer.txt"))));
		assert!(newer > older, "every register gets a fresh monotonic id");
		assert_eq!(store.peek_head().expect("newest").id, newer);
		assert_eq!(store.peek().expect("peek agrees").label(), "test edit");

		let generation = store.generation();
		store.clear();
		assert_eq!(store.generation(), generation + 1, "clear invalidates in-flight entries");
		assert!(store.peek_head().is_none());

		let fresh = store.register(Arc::new(FileAction::new(dir.0.join("fresh.txt"))));
		assert!(fresh > newer, "ids stay unique across clear");
		assert_eq!(store.peek_head().expect("fresh").id, fresh);
		assert_eq!(store.peek_head().expect("fresh").generation, generation + 1);
	}

	#[tokio::test]
	async fn clear_while_apply_in_flight_drops_the_failed_entry() {
		let dir = TempDir::new("clear-fail");
		let store = Arc::new(PendingStore::new());
		let slow = Arc::new(SlowFailAction {
			entered: Arc::new(Notify::new()),
			gate:    Arc::new(Notify::new()),
		});
		store.register(slow.clone());
		let ctx = ctx(&dir);

		let handle = {
			let store = store.clone();
			let ctx = ctx.clone();
			tokio::spawn(async move { store.resolve(&ctx, "stale").await })
		};
		slow.entered.notified().await;
		store.clear();
		assert!(store.peek_head().is_none());

		slow.gate.notify_one();
		let error = handle.await.expect("join").expect_err("stale apply");
		assert!(error.to_string().contains("preview is stale"));
		assert!(store.is_empty(), "a cleared entry must not resurrect on failure");
		assert!(store.peek_head().is_none());
	}

	#[tokio::test]
	async fn clear_while_resolve_in_flight_drops_the_requeued_entry() {
		let dir = TempDir::new("clear-drop");
		let target = dir.0.join("out.txt");
		let store = Arc::new(PendingStore::new());
		let slow = Arc::new(SlowAction {
			path:    target.clone(),
			entered: Arc::new(Notify::new()),
			gate:    Arc::new(Notify::new()),
		});
		store.register(slow.clone());
		let ctx = ctx(&dir);

		let handle = {
			let store = store.clone();
			let ctx = ctx.clone();
			tokio::spawn(async move { store.resolve(&ctx, "aborted").await })
		};
		slow.entered.notified().await;
		store.clear();
		handle.abort();
		assert!(handle.await.expect_err("cancelled task").is_cancelled());

		assert!(store.is_empty(), "a cleared entry must not resurrect on drop");
		assert!(store.peek_head().is_none());
		assert!(!target.exists());
		slow.gate.notify_one();
	}

	#[tokio::test]
	async fn empty_concurrent_resolve_does_not_invalidate_the_inflight_entry() {
		let dir = TempDir::new("empty-race");
		let store = Arc::new(PendingStore::new());
		let slow = Arc::new(SlowFailAction {
			entered: Arc::new(Notify::new()),
			gate:    Arc::new(Notify::new()),
		});
		let registered = store.register(slow.clone());
		let ctx = ctx(&dir);

		let first = {
			let store = store.clone();
			let ctx = ctx.clone();
			tokio::spawn(async move { store.resolve(&ctx, "first").await })
		};
		slow.entered.notified().await;
		// Seeing the queue empty must not bump the generation: the first call
		// still owns the entry and requeues it when its apply fails.
		let second = store.resolve(&ctx, "second").await;
		assert!(second.expect_err("nothing pending").to_string().contains("No pending action"));

		slow.gate.notify_one();
		first.await.expect("join").expect_err("stale apply");
		let head = store.peek_head().expect("requeued head");
		assert_eq!(head.id, registered);
	}

	/// Applies exactly once while a concurrent resolve sees an empty queue.
	struct GatedAction {
		path:    PathBuf,
		entered: Arc<Notify>,
		gate:    Arc<Notify>,
		calls:   Arc<AtomicUsize>,
	}

	#[async_trait]
	impl PendingAction for GatedAction {
		fn label(&self) -> &str {
			"gated edit"
		}

		fn source_tool(&self) -> &str {
			"ast_edit"
		}

		async fn apply(&self, _ctx: &AgentContext, reason: &str) -> Result<ToolResult, ToolError> {
			self.calls.fetch_add(1, Ordering::SeqCst);
			self.entered.notify_one();
			self.gate.notified().await;
			std::fs::write(&self.path, format!("applied: {reason}\n"))
				.map_err(|err| ToolError::ExecutionFailed(err.to_string()))?;
			Ok(tool_result("applied", false))
		}
	}

	#[tokio::test]
	async fn concurrent_resolves_apply_once() {
		let dir = TempDir::new("concurrent");
		let target = dir.0.join("out.txt");
		let store = Arc::new(PendingStore::new());
		let entered = Arc::new(Notify::new());
		let gate = Arc::new(Notify::new());
		let calls = Arc::new(AtomicUsize::new(0));
		store.register(Arc::new(GatedAction {
			path: target.clone(),
			entered: entered.clone(),
			gate: gate.clone(),
			calls: calls.clone(),
		}));
		let ctx = ctx(&dir);

		let first = {
			let store = store.clone();
			let ctx = ctx.clone();
			tokio::spawn(async move { store.resolve(&ctx, "first").await })
		};
		// Hold the first apply in flight, then race a second resolve.
		entered.notified().await;
		let second = store.resolve(&ctx, "second").await;
		assert!(second.is_err(), "a consumed entry cannot be applied twice");
		gate.notify_one();

		assert!(first.await.expect("join").is_ok());
		assert_eq!(calls.load(Ordering::SeqCst), 1);
		assert_eq!(std::fs::read_to_string(&target).expect("read"), "applied: first\n");
	}

	#[tokio::test]
	async fn reject_discards_without_apply_and_empty_reject_succeeds() {
		let dir = TempDir::new("reject");
		let store = PendingStore::new();
		let action = Arc::new(FileAction::new(dir.0.join("out.txt")));
		store.register(action.clone());
		let ctx = ctx(&dir);

		let discarded = store.reject(&ctx, "wrong callsites").await.expect("reject");
		assert!(!discarded.is_error);
		assert_eq!(discarded.details.as_ref().expect("details")["action"], "discard");
		assert_eq!(action.calls.load(Ordering::SeqCst), 0);
		assert!(!dir.0.join("out.txt").exists());

		let empty = store.reject(&ctx, "nothing here").await.expect("empty reject");
		assert!(!empty.is_error);
		assert!(empty.content.contains("Nothing to reject"));
	}

	#[tokio::test]
	async fn write_dispatches_before_any_filesystem_target() {
		let dir = TempDir::new("dispatch");
		let store = Arc::new(PendingStore::new());
		let tool = WriteTool::with_pending(store.clone());
		let ctx = ctx(&dir);

		// Nothing staged: resolve errors, reject succeeds, no file is created.
		let error = dispatch(&tool, "xd://resolve", "go").await.expect_err("empty resolve");
		assert!(error.to_string().contains("No pending action to apply"));
		let rejected = dispatch(&tool, "xd://reject", "nothing").await.expect("empty reject");
		assert!(!rejected.is_error);
		assert!(rejected.content.contains("Nothing to reject"));

		// A staged action is applied through the device, once.
		let target = dir.0.join("out.txt");
		store.register(Arc::new(FileAction::new(target.clone())));
		let applied = dispatch(&tool, "xd://resolve", "looks right").await.expect("apply");
		assert!(!applied.is_error);
		assert_eq!(std::fs::read_to_string(&target).expect("read"), "applied: looks right\n");
		let details = applied.details.as_ref().expect("dispatch details");
		assert_eq!(details["xdev"]["tool"], "resolve");
		assert_eq!(details["xdev"]["inner"]["action"], "apply");
		assert_eq!(details["xdev"]["inner"]["sourceToolName"], "ast_edit");
		let again = dispatch(&tool, "xd://resolve", "again").await.expect_err("consumed");
		assert!(again.to_string().contains("No pending action to apply"));

		let entries: Vec<_> = std::fs::read_dir(&dir.0)
			.expect("read dir")
			.map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
			.collect();
		assert_eq!(entries, vec!["out.txt"]);
	}

	#[tokio::test]
	async fn unknown_or_invalid_xd_targets_error_without_writing() {
		let dir = TempDir::new("unknown");
		let tool = WriteTool::with_pending(Arc::new(PendingStore::new()));
		let ctx = ctx(&dir);

		for path in ["xd://propose", "xd://", "xd://a/b"] {
			let error = dispatch(&tool, path, "body").await.expect_err(path);
			assert!(error.to_string().contains("xd://"), "{path}: {error}");
		}
		assert!(std::fs::read_dir(&dir.0).expect("read dir").next().is_none());

		let usage = ReadTool::new()
			.execute(&json!({"path": "xd://resolve"}), &ctx)
			.await
			.expect("read usage");
		assert!(!usage.is_error);
		assert!(usage.content.contains("APPLY"));
	}
}
