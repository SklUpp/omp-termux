//! Real async coordination behavior: early return, exactly-once delivery,
//! owner-scoped cancellation, mailbox wakeups, and loop-boundary injection.

use std::{
	sync::{
		Arc, Mutex,
		atomic::{AtomicBool, AtomicUsize, Ordering},
	},
	time::Duration,
};

use async_trait::async_trait;
use pi_agent::{
	BackgroundResultSink,
	loop_module::{AgentLoop, AgentError},
	mailbox::{IrcMessage, MAIN_AGENT_ID, Mailbox},
	provider::{ChunkStream, ModelProvider, ProviderError, StreamChunk, ToolChoice},
	state::{AgentConfig, AgentContext, Message},
	supervisor::{
		ChildStatus, DelegationLimits, MAX_OUTSTANDING_RESULTS, Supervisor,
	},
	tools::{
		Tool,
		hub::{HubTool, HubToolDetails},
		registry::ToolRegistry,
		task::{TaskTool, TaskToolDetails},
	},
};
use serde_json::json;
use tokio::sync::{Notify, Semaphore};

struct TestProvider {
	reply:    String,
	gate:     Option<Arc<Semaphore>>,
	entered:  Option<Arc<Notify>>,
	captured: Option<Arc<Mutex<Vec<Message>>>>,
}

#[async_trait]
impl ModelProvider for TestProvider {
	async fn complete(
		&self,
		messages: &[Message],
		_tools: &[serde_json::Value],
		_tool_choice: &ToolChoice,
	) -> Result<Message, ProviderError> {
		if let Some(captured) = &self.captured {
			captured.lock().unwrap().extend_from_slice(messages);
		}
		if let Some(entered) = &self.entered {
			entered.notify_one();
		}
		if let Some(gate) = &self.gate {
			let _permit = gate.acquire().await.map_err(|_| {
				ProviderError::Protocol("gate closed".to_string())
			})?;
		}
		Ok(Message::assistant(self.reply.clone()))
	}

	/// Background children forward child events, so the loop streams. Mirror
	/// `complete` exactly: capture, announce entry, then hold the gate.
	async fn stream(
		&self,
		messages: &[Message],
		_tools: &[serde_json::Value],
		_tool_choice: &ToolChoice,
	) -> Result<ChunkStream, ProviderError> {
		if let Some(captured) = &self.captured {
			captured.lock().unwrap().extend_from_slice(messages);
		}
		if let Some(entered) = &self.entered {
			entered.notify_one();
		}
		if let Some(gate) = &self.gate {
			let _permit = gate.acquire().await.map_err(|_| {
				ProviderError::Protocol("gate closed".to_string())
			})?;
		}
		Ok(text_stream(&self.reply))
	}
}

/// One assistant turn as a two-chunk SSE: the reply text, then the done frame.
fn text_stream(reply: &str) -> ChunkStream {
	let items: Vec<Result<StreamChunk, ProviderError>> = vec![
		Ok(StreamChunk {
			content: Some(reply.to_string()),
			tool_call_deltas: Vec::new(),
			usage: None,
			is_done: false,
		}),
		Ok(StreamChunk {
			content: None,
			tool_call_deltas: Vec::new(),
			usage: None,
			is_done: true,
		}),
	];
	Box::pin(futures::stream::iter(items))
}

fn provider(reply: &str) -> Arc<TestProvider> {
	Arc::new(TestProvider {
		reply:    reply.to_string(),
		gate:     None,
		entered:  None,
		captured: None,
	})
}

fn hanging_provider(entered: Arc<Notify>, gate: Arc<Semaphore>) -> Arc<TestProvider> {
	Arc::new(TestProvider {
		reply:    "never".to_string(),
		gate:     Some(gate),
		entered:  Some(entered),
		captured: None,
	})
}

async fn spawn_background(
	supervisor: &Arc<Supervisor>,
	name: &str,
	task: &str,
) -> String {
	let tool = TaskTool::new(supervisor.clone(), 0);
	let result = tool
		.execute(&json!({ "name": name, "task": task }), &AgentContext::new(std::env::temp_dir()))
		.await
		.expect("task start");
	let details: TaskToolDetails = serde_json::from_str(&result.content).expect("details");
	let state = details.async_state.expect("async details");
	assert_eq!(state.state, "running");
	state.job_id
}

async fn take_delivery(supervisor: &Arc<Supervisor>) -> Vec<String> {
	tokio::time::timeout(Duration::from_secs(5), async {
		loop {
			let delivered = supervisor.take_results_for(None);
			if !delivered.is_empty() {
				return delivered;
			}
			tokio::time::sleep(Duration::from_millis(10)).await;
		}
	})
	.await
	.expect("background delivery")
}

#[tokio::test]
async fn background_task_returns_real_job_id_then_delivers_exactly_once() {
	let supervisor = Supervisor::new(
		ToolRegistry::new(),
		provider("child-output"),
		None,
		DelegationLimits::default(),
	);
	let _sink = supervisor.owner_results_sink(None);
	let id = spawn_background(&supervisor, "bg", "do work").await;
	assert!(id.starts_with("task-"));

	let delivered = take_delivery(&supervisor).await;
	assert_eq!(delivered.len(), 1);
	assert!(delivered[0].contains("<task-result"));
	assert!(delivered[0].contains("status=\"completed\""));
	assert!(delivered[0].contains("child-output"));
	assert!(
		supervisor.take_results_for(None).is_empty(),
		"exactly-once: a second turn must not see the same result"
	);
}

#[tokio::test]
async fn settled_job_snapshot_consumes_and_hides_the_auto_delivery() {
	let supervisor = Supervisor::new(
		ToolRegistry::new(),
		provider("snapshot-output"),
		None,
		DelegationLimits::default(),
	);
	let _sink = supervisor.owner_results_sink(None);
	let id = spawn_background(&supervisor, "bg", "do work").await;
	let hub = HubTool::root(supervisor.clone());
	let wait = hub
		.execute(&json!({ "op": "wait", "ids": [id], "timeoutMs": 5000 }), &AgentContext::new(std::env::temp_dir()))
		.await
		.expect("wait");
	let details: HubToolDetails = serde_json::from_str(&wait.content).expect("details");
	assert_eq!(details.jobs.len(), 1);
	assert_eq!(details.jobs[0].status, "completed");
	assert!(details.jobs[0].result_text.as_deref().unwrap().contains("snapshot-output"));
	assert!(
		supervisor.take_results_for(None).is_empty(),
		"snapshot is the only delivery"
	);

	// A second snapshot has no body to replay.
	let again = hub
		.execute(&json!({ "op": "jobs" }), &AgentContext::new(std::env::temp_dir()))
		.await
		.expect("jobs");
	let details: HubToolDetails = serde_json::from_str(&again.content).expect("details");
	assert_eq!(details.jobs[0].status, "completed");
	assert!(details.jobs[0].result_text.is_none());
}

#[tokio::test]
async fn queued_background_job_is_cancelled_before_it_runs() {
	let gate = Arc::new(Semaphore::new(0));
	let entered = Arc::new(Notify::new());
	let supervisor = Supervisor::new(
		ToolRegistry::new(),
		hanging_provider(entered.clone(), gate.clone()),
		None,
		DelegationLimits { max_depth: 2, max_concurrency: 1 },
	);
	let _sink = supervisor.owner_results_sink(None);
	let _first = spawn_background(&supervisor, "first", "hold permit").await;
	tokio::time::timeout(Duration::from_secs(5), entered.notified())
		.await
		.expect("first job entered provider");
	let second = spawn_background(&supervisor, "second", "queued").await;

	let hub = HubTool::root(supervisor.clone());
	let cancel = hub
		.execute(&json!({ "op": "cancel", "ids": [second] }), &AgentContext::new(std::env::temp_dir()))
		.await
		.expect("cancel");
	let details: HubToolDetails = serde_json::from_str(&cancel.content).expect("details");
	assert_eq!(details.cancelled.as_ref().unwrap()[0].status, "cancelled");
	let record = supervisor
		.children()
		.into_iter()
		.find(|record| record.id == details.cancelled.as_ref().unwrap()[0].id)
		.unwrap();
	assert_eq!(record.status, ChildStatus::Aborted);
	assert!(supervisor.take_results_for(None).is_empty(), "cancelled job never delivers");

	// Release the hanging first job so the test runtime can stop it.
	gate.add_permits(8);
}

#[tokio::test]
async fn peer_message_wakens_a_registered_wait_without_missing_the_send() {
	let gate = Arc::new(Semaphore::new(0));
	let entered = Arc::new(Notify::new());
	let supervisor = Supervisor::new(
		ToolRegistry::new(),
		hanging_provider(entered.clone(), gate.clone()),
		None,
		DelegationLimits::default(),
	);
	let _sink = supervisor.owner_results_sink(None);
	let peer = spawn_background(&supervisor, "peer", "stay running").await;
	tokio::time::timeout(Duration::from_secs(5), entered.notified())
		.await
		.expect("peer entered provider");

	let root = HubTool::root(supervisor.clone());
	let waiting = {
		let root = root.clone();
		let peer = peer.clone();
		tokio::spawn(async move {
			root
				.execute(
					&json!({ "op": "wait", "from": peer, "timeoutMs": 5000 }),
					&AgentContext::new(std::env::temp_dir()),
				)
				.await
				.expect("root wait")
		})
	};
	tokio::time::sleep(Duration::from_millis(50)).await;

	let child = HubTool::with_owner(supervisor.clone(), Some(peer.clone()));
	let sent = child
		.execute(
			&json!({ "op": "send", "to": MAIN_AGENT_ID, "message": "ping" }),
			&AgentContext::new(std::env::temp_dir()),
		)
		.await
		.expect("child send");
	let mut send_details: HubToolDetails = serde_json::from_value(
		sent.details.clone().expect("send must expose structured details"),
	)
	.expect("send details");
	let receipt = send_details.receipts.take().unwrap().pop().unwrap();
	assert_eq!(receipt.outcome, "injected");
	assert!(receipt.error.is_none());

	let waited = waiting.await.expect("wait task");
	let details: HubToolDetails = serde_json::from_str(&waited.content).expect("wait details");
	assert_eq!(details.waited.as_ref().unwrap().body, "ping");
	assert_eq!(details.waited.as_ref().unwrap().from, peer);
	gate.add_permits(8);
}

#[tokio::test]
async fn job_and_cancel_owner_scope_excludes_siblings_and_grandchildren() {
	let supervisor = Supervisor::new(
		ToolRegistry::new(),
		provider("done"),
		None,
		DelegationLimits::default(),
	);
	let _sink = supervisor.owner_results_sink(None);
	let root_job = spawn_background(&supervisor, "root-job", "work").await;
	// A sibling job is a real, known identity that owns no other job (including
	// root_job), unlike fabricating a child id from root_job itself.
	let child_job = spawn_background(&supervisor, "child-job", "work").await;

	let child_hub = HubTool::with_owner(supervisor.clone(), Some(child_job.clone()));
	let jobs = child_hub
		.execute(&json!({ "op": "jobs" }), &AgentContext::new(std::env::temp_dir()))
		.await
		.expect("child jobs");
	let details: HubToolDetails = serde_json::from_str(&jobs.content).expect("details");
	assert!(details.jobs.is_empty(), "child must not see the root's job");

	let cancel = child_hub
		.execute(
			&json!({ "op": "cancel", "ids": [root_job] }),
			&AgentContext::new(std::env::temp_dir()),
		)
		.await
		.expect("child cancel");
	let details: HubToolDetails = serde_json::from_str(&cancel.content).expect("details");
	assert_eq!(details.cancelled.as_ref().unwrap()[0].status, "not_found");
	assert!(
		details.cancelled.as_ref().unwrap()[0].message.contains("not spawned by you"),
		"foreign job rejection must be explicit"
	);

	let self_cancel = child_hub
		.execute(
			&json!({ "op": "cancel", "ids": [child_job] }),
			&AgentContext::new(std::env::temp_dir()),
		)
		.await
		.expect("self cancel");
	let details: HubToolDetails = serde_json::from_str(&self_cancel.content).expect("details");
	assert_eq!(
		details.cancelled.as_ref().unwrap()[0].message,
		"Cannot cancel yourself."
	);
}

#[tokio::test]
async fn owner_scoped_sink_does_not_take_root_results() {
	let supervisor = Supervisor::new(
		ToolRegistry::new(),
		provider("root-result"),
		None,
		DelegationLimits::default(),
	);
	let _root_sink = supervisor.owner_results_sink(None);
	let child_sink = supervisor.owner_results_sink(Some("task-999"));
	let _ = spawn_background(&supervisor, "bg", "work").await;
	let delivered = take_delivery(&supervisor).await;
	assert_eq!(delivered.len(), 1);
	assert!(child_sink.take_background_results().is_empty());
}

struct OnceSink {
	delivered: AtomicBool,
}

impl BackgroundResultSink for OnceSink {
	fn take_background_results(&self) -> Vec<String> {
		if self.delivered.swap(true, Ordering::SeqCst) {
			Vec::new()
		} else {
			vec!["<task-result id=\"task-1\" status=\"completed\">\n<output>\nloop-output\n</output>\n</task-result>".to_string()]
		}
	}
}

#[tokio::test]
async fn agent_loop_injects_delivery_and_peer_mail_at_turn_boundary() -> Result<(), AgentError> {
	let captured = Arc::new(Mutex::new(Vec::<Message>::new()));
	let provider = Arc::new(TestProvider {
		reply:    "done".to_string(),
		gate:     None,
		entered:  None,
		captured: Some(captured.clone()),
	});
	let mailbox = Arc::new(Mailbox::new());
	mailbox.buffer(IrcMessage::new("task-7", MAIN_AGENT_ID, "hello root"));
	let agent = AgentLoop::new(
		AgentConfig {
			max_turns: 2,
			context_limit: 100_000,
			system_prompt: None,
			cwd: std::env::temp_dir(),
		},
		provider,
		ToolRegistry::new(),
	)
.with_background_results(Arc::new(OnceSink {
		delivered: AtomicBool::new(false),
	}))
	.with_peer_inbox(mailbox, MAIN_AGENT_ID);
	let mut state = pi_agent::state::AgentState::default();
	let turns = agent
		.run_dialog_message_with_cancel(
			Message::user("hello"),
			&mut state,
			None,
			None,
			tokio_util::sync::CancellationToken::new(),
		)
		.await?;
	assert_eq!(turns.len(), 1);
	let seen = captured.lock().unwrap().clone();
	let texts: Vec<&str> = seen.iter().map(|message| message.content.as_str()).collect();
	assert!(texts.iter().any(|text| text.contains("loop-output")));
	assert!(texts.iter().any(|text| text.contains("hello root")));
	Ok(())
}

/// First completion hangs (holding the parent job's permit), later ones answer
/// immediately, so a nested spawn can only finish if it uses fresh permits.
struct ScriptedProvider {
	calls:   AtomicUsize,
	entered: Notify,
	release: Semaphore,
}

#[async_trait]
impl ModelProvider for ScriptedProvider {
	async fn complete(
		&self,
		_messages: &[Message],
		_tools: &[serde_json::Value],
		_tool_choice: &ToolChoice,
	) -> Result<Message, ProviderError> {
		if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
			self.entered.notify_one();
			let _permit = self
				.release
				.acquire()
				.await
				.map_err(|_| ProviderError::Protocol("release closed".to_string()))?;
			return Ok(Message::assistant("parent-output".to_string()));
		}
		Ok(Message::assistant("nested-output".to_string()))
	}

	/// Same script as `complete`: the first call hangs holding the parent job's
	/// permit; later calls (the nested child) answer immediately.
	async fn stream(
		&self,
		_messages: &[Message],
		_tools: &[serde_json::Value],
		_tool_choice: &ToolChoice,
	) -> Result<ChunkStream, ProviderError> {
		if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
			self.entered.notify_one();
			let _permit = self
				.release
				.acquire()
				.await
				.map_err(|_| ProviderError::Protocol("release closed".to_string()))?;
			return Ok(text_stream("parent-output"));
		}
		Ok(text_stream("nested-output"))
	}
}

#[tokio::test]
async fn nested_session_semaphore_does_not_wait_on_exhausted_parent_permits() {
	let scripted = Arc::new(ScriptedProvider {
		calls:   AtomicUsize::new(0),
		entered: Notify::new(),
		release: Semaphore::new(0),
	});
	let supervisor = Supervisor::new(
		ToolRegistry::new(),
		scripted.clone(),
		None,
		DelegationLimits { max_depth: 3, max_concurrency: 1 },
	);
	let _sink = supervisor.owner_results_sink(None);
	let root_registry = supervisor.root_registry();
	let parent = root_registry.get("task").expect("root task tool");
	let ctx = AgentContext::new(std::env::temp_dir());
	let parent_result = parent
		.execute(&json!({ "name": "parent", "task": "parent" }), &ctx)
		.await
		.expect("parent spawn");
	let parent_details: TaskToolDetails =
		serde_json::from_str(&parent_result.content).expect("details");
	assert!(parent_details.async_state.is_some(), "parent must run in background");
	tokio::time::timeout(Duration::from_secs(5), scripted.entered.notified())
		.await
		.expect("parent child reached the hanging provider");
	// The parent session's only permit is now held for the whole child run.
	let nested = TaskTool::with_parent(supervisor.clone(), 1, None);
	let result = nested
		.execute(
			&json!({ "name": "nested", "task": "work" }),
			&AgentContext::new(std::env::temp_dir()),
		)
		.await
		.expect("nested spawn");
	let details: TaskToolDetails = serde_json::from_str(&result.content).expect("details");
	assert!(details.async_state.is_some(), "nested spawn must run in background");
	let delivered = tokio::time::timeout(Duration::from_secs(5), async {
		loop {
			let delivered = supervisor.take_results_for(None);
			if !delivered.is_empty() {
				return delivered;
			}
			tokio::time::sleep(Duration::from_millis(10)).await;
		}
	})
	.await
	.expect("nested job must use its own session permits, not the parent's");
	assert!(delivered[0].contains("nested-output"), "{delivered:?}");
	scripted.release.add_permits(1);
	supervisor.shutdown();
}

#[tokio::test]
async fn settled_job_records_stay_bounded_and_pending_deliveries_survive() {
	let supervisor = Supervisor::new(
		ToolRegistry::new(),
		provider("done"),
		None,
		DelegationLimits::default(),
	);
	for index in 0..120 {
		let (id, _token) = supervisor
			.reserve_child(&format!("job{index}"), index, "task", "work", None)
			.expect("capacity");
		supervisor.unregister_active_child(&id);
		supervisor.mark_completed(&id, 1, "settled");
		supervisor.consume_results(std::slice::from_ref(&id));
	}
	let retained = supervisor.children();
	assert!(
		retained.len() <= 100,
		"settled records must be evicted; retained {}",
		retained.len()
	);

	let (pending, _token) = supervisor
		.reserve_child("pending", 0, "task", "work", None)
		.expect("capacity");
	supervisor.unregister_active_child(&pending);
	supervisor.mark_completed(&pending, 1, "kept");
	supervisor.enqueue_delivery(&pending, "kept".to_string());
	for index in 0..120 {
		let (id, _token) = supervisor
			.reserve_child(&format!("more{index}"), index, "task", "w", None)
			.expect("capacity");
		supervisor.unregister_active_child(&id);
		supervisor.mark_completed(&id, 1, "settled");
		supervisor.consume_results(std::slice::from_ref(&id));
	}
	let retained = supervisor.children();
	assert!(
		retained.iter().any(|record| record.id == pending),
		"an undelivered result must never be evicted"
	);
	assert_eq!(supervisor.take_results_for(None), vec!["kept".to_string()]);
}

#[tokio::test]
async fn hub_wait_sees_root_presence_excludes_self_and_wakes_on_peer_exit() {
	let supervisor = Supervisor::new(
		ToolRegistry::new(),
		provider("done"),
		None,
		DelegationLimits::default(),
	);
	let mailbox = supervisor.mailbox();
	mailbox.set_agent_active(MAIN_AGENT_ID, true);
	let hub = HubTool::with_owner(supervisor.clone(), Some("child".to_string()));
	let ctx = AgentContext::new(std::env::temp_dir());

	// Root is running: waiting for `Main` is a real wait, not "not running".
	let result = hub
		.execute(&json!({ "op": "wait", "from": "Main", "timeoutMs": 50 }), &ctx)
		.await
		.expect("wait for Main");
	assert!(!result.is_error, "{}", result.content);
	assert!(result.content.contains("No message"), "{}", result.content);

	// A caller must not count itself as the peer it is waiting for.
	mailbox.set_agent_active(MAIN_AGENT_ID, false);
	let result = hub
		.execute(&json!({ "op": "wait", "timeoutMs": 0 }), &ctx)
		.await
		.expect("self-only wait");
	assert!(result.content.contains("No running peers"), "{}", result.content);

	// Peer exit wakes an otherwise unbounded wait.
	mailbox.set_agent_active(MAIN_AGENT_ID, true);
	let waiting = {
		let hub = hub.clone();
		let ctx = AgentContext::new(std::env::temp_dir());
		tokio::spawn(async move {
			hub.execute(&json!({ "op": "wait", "timeoutMs": 10_000 }), &ctx).await
		})
	};
	tokio::time::sleep(Duration::from_millis(50)).await;
	mailbox.set_agent_active(MAIN_AGENT_ID, false);
	let result = tokio::time::timeout(Duration::from_secs(2), waiting)
		.await
		.expect("peer wait must wake when the last peer exits")
		.expect("join")
		.expect("wait result");
	assert!(result.content.contains("No running peers"), "{}", result.content);
}


#[tokio::test]
async fn result_admission_is_explicitly_bounded_before_spawning() {
	let supervisor = Supervisor::new(
		ToolRegistry::new(),
		provider("done"),
		None,
		DelegationLimits::default(),
	);
	let mut reserved = Vec::new();
	for index in 0..MAX_OUTSTANDING_RESULTS {
		let (id, _token) = supervisor
			.reserve_child(&format!("running{index}"), index, "task", "work", None)
			.expect("capacity within the admission bound");
		reserved.push(id);
	}
	assert_eq!(supervisor.outstanding_capacity(), 0);
	assert!(
		supervisor
			.reserve_child("overflow", 0, "task", "work", None)
			.is_none(),
		"admission must fail explicitly at the cap"
	);
	assert_eq!(supervisor.children().len(), MAX_OUTSTANDING_RESULTS);

	let too_many: Vec<serde_json::Value> = (0..=MAX_OUTSTANDING_RESULTS)
		.map(|index| json!({ "name": format!("batch{index}"), "task": "work" }))
		.collect();
	let tool = TaskTool::with_semaphore(supervisor.clone(), 0, Arc::new(Semaphore::new(8)));
	let error = tool
		.execute(
			&json!({ "tasks": too_many }),
			&AgentContext::new(std::env::temp_dir()),
		)
		.await
		.expect_err("oversized batch must be rejected before allocating records");
	assert!(error.to_string().contains("capacity"), "{error}");
	assert_eq!(
		supervisor.children().len(),
		MAX_OUTSTANDING_RESULTS,
		"a rejected batch must not consume records or slots"
	);

	supervisor.mark_completed(&reserved[0], 1, "done");
	supervisor.consume_results(&[reserved[0].clone()]);
	assert!(
		supervisor
			.reserve_child("after-consumption", 0, "task", "work", None)
			.is_some(),
		"consuming a delivered result frees exactly one slot"
	);
}


#[tokio::test]
async fn settled_pending_results_keep_slots_until_consumed() {
	let supervisor = Supervisor::new(
		ToolRegistry::new(),
		provider("done"),
		None,
		DelegationLimits::default(),
	);
	let mut ids = Vec::new();
	for index in 0..MAX_OUTSTANDING_RESULTS {
		let (id, _token) = supervisor
			.reserve_child(&format!("pending{index}"), index, "task", "work", None)
			.expect("capacity");
		supervisor.mark_completed(&id, 1, "body");
		supervisor.enqueue_delivery(&id, "body".to_string());
		ids.push(id);
	}
	assert_eq!(
		supervisor.outstanding_capacity(),
		0,
		"settled but undelivered results must keep their admission slots"
	);
	assert!(
		supervisor
			.reserve_child("overflow", 0, "task", "work", None)
			.is_none(),
		"the 65th result is refused after settlement, not only while running"
	);
	assert!(supervisor.consume_results(std::slice::from_ref(&ids[0])).is_empty());
	assert_eq!(
		supervisor.outstanding_capacity(),
		1,
		"consuming one pending result frees exactly one slot"
	);
	assert!(
		supervisor
			.reserve_child("after-consume", 0, "task", "work", None)
			.is_some()
	);
	assert_eq!(
		supervisor.take_results_for(None).len(),
		MAX_OUTSTANDING_RESULTS - 1,
		"remaining bodies stay deliverable"
	);
}

/// A sink that only exposes the rich view: the loop must use it, so a text-only
/// regressions fails loudly instead of silently dropping identity/usage.
struct UsageSink {
	calls: AtomicUsize,
}

impl BackgroundResultSink for UsageSink {
	fn take_background_results(&self) -> Vec<String> {
		Vec::new()
	}

	fn take_background_deliveries(&self) -> Vec<pi_agent::supervisor::BackgroundDelivery> {
		if self.calls.fetch_add(1, Ordering::SeqCst) > 0 {
			return Vec::new();
		}
		vec![pi_agent::supervisor::BackgroundDelivery {
			id: "task-usage".to_string(),
			result_key: Some("ns:task-usage".to_string()),
			text: "<task-result id=\"task-usage\" status=\"completed\">\n<usage input=\"5\" output=\"2\" />\n\
			       <output>\nbody\n</output>\n</task-result>"
				.to_string(),
			usage: Some(pi_agent::state::TokenUsage {
				input: 5,
				output: 2,
				..pi_agent::state::TokenUsage::default()
			}),
		}]
	}
}

/// The host sees every injected background result once, with its job identity
/// and reported usage, at the same boundary that injects the text.
#[tokio::test]
async fn background_delivery_reaches_the_host_once_with_identity_and_usage(
) -> Result<(), AgentError> {
	let captured = Arc::new(Mutex::new(Vec::<Message>::new()));
	let provider = Arc::new(TestProvider {
		reply:    "done".to_string(),
		gate:     None,
		entered:  None,
		captured: Some(captured.clone()),
	});
	let agent = AgentLoop::new(
		AgentConfig {
			max_turns: 2,
			context_limit: 100_000,
			system_prompt: None,
			cwd: std::env::temp_dir(),
		},
		provider,
		ToolRegistry::new(),
	)
	.with_background_results(Arc::new(UsageSink { calls: AtomicUsize::new(0) }));
	let mut state = pi_agent::state::AgentState::default();
	let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(16);
	let turns = agent
		.run_dialog_message_with_cancel(
			Message::user("go"),
			&mut state,
			None,
			Some(events_tx),
			tokio_util::sync::CancellationToken::new(),
		)
		.await?;
	assert_eq!(turns.len(), 1);

	let mut delivered = Vec::new();
	while let Ok(event) = events_rx.try_recv() {
		if let pi_agent::loop_module::AgentEvent::BackgroundResult {
			id,
			result_key,
			text,
			usage,
		} = event
		{
			delivered.push((id, result_key, text, usage));
		}
	}
	assert_eq!(delivered.len(), 1, "one event per delivered result");
	assert_eq!(delivered[0].0, "task-usage", "identity travels with the event");
	assert_eq!(
		delivered[0].1.as_deref(),
		Some("ns:task-usage"),
		"the accounting key travels with the event"
	);
	assert!(delivered[0].2.contains("body"));
	assert_eq!(
		delivered[0].3,
		Some(pi_agent::state::TokenUsage {
			input: 5,
			output: 2,
			..pi_agent::state::TokenUsage::default()
		}),
		"reported usage travels once, unmodified"
	);
	let seen = captured.lock().unwrap().clone();
	assert_eq!(
		seen
			.iter()
			.filter(|message| message.content.contains("task-usage"))
			.count(),
		1,
		"the delivered text is injected exactly once"
	);
	Ok(())
}
