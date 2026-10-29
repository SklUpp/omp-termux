//! Supervisor observability: lifecycle/progress emission, observer detach and
//! the child transcript log the task executor writes.

use std::{
	path::PathBuf,
	sync::{Arc, Mutex},
};

use async_trait::async_trait;
use pi_agent::{
	provider::{ChunkStream, ModelProvider, ProviderError, ToolChoice},
	state::Message,
	supervisor::{
		ChildSpawn, DelegationLimits, SubagentEvent, SubagentLifecycleStatus, SubagentObserver,
		Supervisor,
	},
	tools::registry::ToolRegistry,
};

struct UnusedProvider;

#[async_trait]
impl ModelProvider for UnusedProvider {
	async fn complete(
		&self,
		_messages: &[Message],
		_tools: &[serde_json::Value],
		_tool_choice: &ToolChoice,
	) -> Result<Message, ProviderError> {
		Err(ProviderError::Network("unused".to_string()))
	}

	async fn stream(
		&self,
		_messages: &[Message],
		_tools: &[serde_json::Value],
		_tool_choice: &ToolChoice,
	) -> Result<ChunkStream, ProviderError> {
		Err(ProviderError::Network("unused".to_string()))
	}
}

fn supervisor() -> Arc<Supervisor> {
	Supervisor::new(
		ToolRegistry::with_builtins(),
		Arc::new(UnusedProvider),
		None,
		DelegationLimits::default(),
	)
}

#[derive(Default)]
struct Collect {
	events: Mutex<Vec<SubagentEvent>>,
}

impl Collect {
	/// Drain: callers snapshot the frames observed since their previous take.
	fn take(&self) -> Vec<SubagentEvent> {
		std::mem::take(&mut *self.events.lock().expect("events"))
	}
}

impl SubagentObserver for Collect {
	fn observe(&self, event: &SubagentEvent) {
		self.events.lock().expect("events").push(event.clone());
	}
}

fn spawn(name: &str, index: usize, detached: bool) -> ChildSpawn<'_> {
	ChildSpawn {
		name,
		index,
		agent: "scout",
		agent_source: "builtin",
		task: "look around",
		parent_id: None,
		parent_tool_call_id: Some("call-7"),
		detached,
	}
}

fn session_root() -> PathBuf {
	let dir = std::env::temp_dir().join(format!("omp-subagent-obs-{}", std::process::id()));
	std::fs::create_dir_all(&dir).expect("dir");
	dir
}

#[test]
fn lifecycle_progress_and_terminal_are_emitted_once() {
	let dir = session_root();
	let supervisor = supervisor();
	supervisor.set_child_session_root(Some(dir.clone()));
	let observer = Arc::new(Collect::default());
	let guard = supervisor.subscribe(observer.clone());

	let (id, _token) =
		supervisor.reserve_child_full(spawn("alpha", 1, true)).expect("reserve");

	let events = observer.take();
	assert_eq!(events.len(), 1, "reserve emits exactly one started event");
	let SubagentEvent::Lifecycle(started) = &events[0] else {
		panic!("first event must be a lifecycle event");
	};
	assert_eq!(started.id, id);
	assert_eq!(started.status, SubagentLifecycleStatus::Started);
	assert!(started.detached);
	assert_eq!(started.agent_source, "builtin");
	assert_eq!(started.parent_tool_call_id.as_deref(), Some("call-7"));

	let expected = dir.join(format!("{id}.jsonl"));
	let session_file = started.session_file.as_deref().expect("real transcript path");
	assert_eq!(session_file, expected.display().to_string());
	assert!(expected.exists(), "the announced transcript must exist");
	assert_eq!(supervisor.child_session_file(&id), Some(expected.clone()));

	assert!(supervisor.report_progress(&id, "running", Some("look around")));
	supervisor.mark_completed(&id, 12, "done");
	// Repeating a terminal transition must not emit a second frame.
	supervisor.mark_completed(&id, 12, "done");

	let events = observer.take();
	assert_eq!(events.len(), 2, "progress + one terminal");
	let SubagentEvent::Progress(progress) = &events[0] else {
		panic!("first event must be progress");
	};
	assert_eq!(progress.id, id);
	assert!(progress.detached);
	assert_eq!(progress.status, "running");
	assert_eq!(progress.session_file, Some(expected.display().to_string()));
	let SubagentEvent::Lifecycle(terminal) = &events[1] else {
		panic!("second event must be the terminal lifecycle");
	};
	assert_eq!(terminal.status, SubagentLifecycleStatus::Completed);
	assert_eq!(terminal.duration_ms, Some(12));

	// Progress for a settled child is not invented.
	assert!(!supervisor.report_progress(&id, "running", None));
	assert!(observer.take().is_empty(), "a settled child invents nothing");

	drop(guard);
	let (aborted_id, _) = supervisor.reserve_child_full(spawn("beta", 2, false)).expect("reserve");
	supervisor.mark_aborted(&aborted_id, "cancelled by hub");
	assert!(observer.take().is_empty(), "a dropped guard receives nothing");
}

#[test]
fn transcript_appends_real_messages_for_paging() {
	let dir = session_root();
	let supervisor = supervisor();
	supervisor.set_child_session_root(Some(dir));
	let (id, _token) = supervisor.reserve_child_full(spawn("gamma", 1, false)).expect("reserve");
	let path = supervisor.child_session_file(&id).expect("path");

	supervisor.append_child_transcript(&id, &[&Message::user("do the thing")]);
	supervisor.append_child_transcript(&id, &[&Message::assistant("done")]);

	let contents = std::fs::read_to_string(&path).expect("read transcript");
	let lines: Vec<&str> = contents.lines().collect();
	assert_eq!(lines.len(), 2);
	let first: serde_json::Value = serde_json::from_str(lines[0]).expect("first line");
	assert_eq!(first["type"], serde_json::json!("message"));
	assert_eq!(first["message"]["role"], serde_json::json!("user"));
	assert_eq!(first["message"]["content"], serde_json::json!("do the thing"));
	let second: serde_json::Value = serde_json::from_str(lines[1]).expect("second line");
	assert_eq!(second["message"]["role"], serde_json::json!("assistant"));
	assert_eq!(second["message"]["content"], serde_json::json!("done"));
	assert!(contents.ends_with('\n'), "every append must terminate a complete line");
}

#[test]
fn without_a_session_root_no_transcript_is_announced() {
	let supervisor = supervisor();
	let observer = Arc::new(Collect::default());
	let _guard = supervisor.subscribe(observer.clone());
	let (id, _token) = supervisor.reserve_child_full(spawn("delta", 1, false)).expect("reserve");
	assert!(supervisor.child_session_file(&id).is_none());
	let events = observer.take();
	let SubagentEvent::Lifecycle(started) = &events[0] else {
		panic!("first event must be a lifecycle event");
	};
	assert!(started.session_file.is_none());
	assert!(!started.detached);
}

#[test]
fn real_child_events_flow_only_while_the_child_is_running() {
	use pi_agent::{AgentEvent, ContentPart};

	let supervisor = supervisor();
	let observer = Arc::new(Collect::default());
	let _guard = supervisor.subscribe(observer.clone());
	let (id, _token) = supervisor.reserve_child_full(spawn("beta", 2, false)).expect("reserve");

	assert!(supervisor.report_event(
		&id,
		AgentEvent::ToolStart {
			id:        "call-1".to_string(),
			name:      "grep".to_string(),
			arguments: serde_json::json!({ "pattern": "x" }),
		},
	));
	// Opaque observer payload: these bytes are not a decodable PNG and are
	// never decoded here, only carried end to end.
	let tool_end = AgentEvent::ToolEnd {
		id:       "call-1".to_string(),
		name:     "grep".to_string(),
		result:   "hit".to_string(),
		is_error: false,
		images:   vec![ContentPart::Image {
			data:      "aGk=".to_string(),
			mime_type: "image/png".to_string(),
		}],
		details:  Some(serde_json::json!({ "matches": 1 })),
	};
	assert!(supervisor.report_event(&id, tool_end.clone()));

	let events = observer.take();
	let seen: Vec<&AgentEvent> = events
		.iter()
		.filter_map(|event| match event {
			SubagentEvent::Event { id: seen_id, event } => {
				assert_eq!(seen_id, &id);
				Some(event)
			},
			_ => None,
		})
		.collect();
	assert_eq!(seen.len(), 2, "both real loop events reach observers");
	assert!(matches!(seen[0], AgentEvent::ToolStart { .. }));
	assert!(matches!(seen[1], AgentEvent::ToolEnd { .. }));
	assert_eq!(
		seen[1], &tool_end,
		"the observer carries the exact image part and details, not just a ToolEnd"
	);

	supervisor.mark_completed(&id, 3, "done");
	assert!(
		!supervisor.report_event(&id, AgentEvent::MessageUpdate { delta: "late".to_string() }),
		"a settled child emits nothing"
	);
	let _ = observer.take();
	supervisor.mark_completed(&id, 3, "done");
	assert!(
		observer
			.take()
			.iter()
			.all(|event| !matches!(event, SubagentEvent::Event { .. })),
		"no event frame after the terminal one"
	);
}

#[test]
fn terminal_frame_is_never_followed_by_an_event_across_threads() {
	use std::sync::Barrier;

	use pi_agent::AgentEvent;

	// Barrier-only coordination: no sleeps, no timing assumptions. Without the
	// emission gate a settle between `report_event`'s status check and its emit
	// lets an `Event` frame follow the terminal `Lifecycle`.
	for _ in 0..256 {
		let supervisor = supervisor();
		let observer = Arc::new(Collect::default());
		let _guard = supervisor.subscribe(observer.clone());
		let (id, _token) = supervisor
			.reserve_child_full(spawn("omega", 1, false))
			.expect("reserve");
		let _ = observer.take();

		let barrier = Arc::new(Barrier::new(3));
		let emitter = {
			let supervisor = Arc::clone(&supervisor);
			let barrier = Arc::clone(&barrier);
			let id = id.clone();
			std::thread::spawn(move || {
				barrier.wait();
				supervisor.report_event(&id, AgentEvent::MessageUpdate { delta: "race".to_string() });
			})
		};
		let settler = {
			let supervisor = Arc::clone(&supervisor);
			let barrier = Arc::clone(&barrier);
			let id = id.clone();
			std::thread::spawn(move || {
				barrier.wait();
				supervisor.mark_completed(&id, 1, "done");
			})
		};
		barrier.wait();
		emitter.join().expect("emitter thread");
		settler.join().expect("settler thread");

		let events = observer.take();
		let mut terminal = None;
		for (index, event) in events.iter().enumerate() {
			if let SubagentEvent::Lifecycle(lifecycle) = event {
				if lifecycle.status != SubagentLifecycleStatus::Started {
					terminal = Some(index);
				}
			}
		}
		let terminal = terminal.expect("the terminal lifecycle must be emitted");
		assert!(
			events[terminal + 1..]
				.iter()
				.all(|event| !matches!(event, SubagentEvent::Event { .. })),
			"an event frame followed the terminal one: {events:?}"
		);
	}
}
