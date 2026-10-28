//! Consumer-visible process-lifecycle contracts for the security blockers:
//! duplicate starts, cancelled PTY launch, root-exit descendant sweep,
//! stop-vs-restart race, pinned identity on adoption, detached physical
//! lifetime, and Spawned-before-readiness metadata. Tests use real barriers
//! (Barrier, sleep, `/proc`) and assert observable API behavior only.

use std::{
	collections::HashMap,
	path::{Path, PathBuf},
	sync::{
		Arc,
		atomic::{AtomicUsize, Ordering},
	},
	time::Duration,
};

use pi_agent::tools::process::{
	ProcessManager, ProcessSpec, ProcessState, ReadySpec, RestartPolicy, StartIdentity, WaitRequest,
};

struct TempDir {
	path: PathBuf,
}

impl TempDir {
	fn new(label: &str) -> Self {
		static NEXT: AtomicUsize = AtomicUsize::new(0);
		let unique =
			format!("pi-process-{label}-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::SeqCst));
		let path = std::env::temp_dir().join(unique);
		std::fs::create_dir_all(&path).expect("create temp dir");
		Self { path }
	}
}

impl Drop for TempDir {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.path);
	}
}

fn alive(pid: u32) -> bool {
	Path::new("/proc").join(pid.to_string()).exists()
}

fn spec(dir: &TempDir, name: &str, application: &str, args: &[&str]) -> ProcessSpec {
	ProcessSpec {
		name: name.to_string(),
		application: application.to_string(),
		args: args.iter().map(|arg| (*arg).to_string()).collect(),
		env: HashMap::new(),
		cwd: dir.path.display().to_string(),
		pty: false,
		ready: None,
		restart: RestartPolicy::No,
		persist: false,
		detached: false,
	}
}

fn state_of(manager: &ProcessManager, name: &str) -> Option<ProcessState> {
	manager.list(None).into_iter().find(|snapshot| snapshot.name == name).map(|snapshot| snapshot.state)
}

fn terminal(state: ProcessState) -> bool {
	matches!(state, ProcessState::Exited | ProcessState::Failed)
}

/// Duplicate `start` calls race on one name: exactly one wins; no second kernel
/// process is ever spawned.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_starts_for_one_name_spawn_once() {
	let dir = TempDir::new("duplicate");
	let manager = Arc::new(ProcessManager::new(dir.path.clone()));
	let launch = spec(&dir, "dup", "sh", &["-c", "sleep 30"]);
	let barrier = Arc::new(tokio::sync::Barrier::new(4));
	let mut tasks = Vec::new();
	for _ in 0..4 {
		let manager = Arc::clone(&manager);
		let launch = launch.clone();
		let barrier = Arc::clone(&barrier);
		tasks.push(tokio::spawn(async move {
			barrier.wait().await;
			manager.start(launch, None).await
		}));
	}
	let mut winners = 0;
	for task in tasks {
		if task.await.expect("join").is_ok() {
			winners += 1;
		}
	}
	assert_eq!(winners, 1, "only one start may own a live name");
	assert_eq!(manager.list(None).len(), 1, "a losing start must not register a record");
	manager.stop("dup", None, 2_000).await.expect("stop winner");
}

/// Dropping a `start` future while readiness is pending must kill the spawned
/// PTY child, not leave an untracked process behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_launch_kills_pty_child() {
	let dir = TempDir::new("cancel");
	let manager = Arc::new(ProcessManager::new(dir.path.clone()));
	let mut launch = spec(&dir, "cancel", "sh", &["-c", "sleep 30"]);
	launch.pty = true;
	launch.ready = Some(ReadySpec {
		log: Some("NEVER_MATCH_THIS".to_string()),
		port: None,
		host: None,
		timeout_ms: 30_000,
	});
	let running = Arc::clone(&manager);
	let task = tokio::spawn(async move { running.start(launch, None).await });
	tokio::time::sleep(Duration::from_millis(300)).await;
	task.abort();
	let _ = task.await;
	for _ in 0..60 {
		if terminal(state_of(&manager, "cancel").unwrap_or(ProcessState::Exited)) {
			break;
		}
		tokio::time::sleep(Duration::from_millis(50)).await;
	}
	let snapshot = manager
		.list(None)
		.into_iter()
		.find(|snapshot| snapshot.name == "cancel")
		.expect("cancelled launch must settle its record");
	assert!(terminal(snapshot.state), "cancelled launch left state {:?}", snapshot.state);
	assert!(snapshot.pid.is_none(), "cancelled launch kept a live pid");
}

/// Root exit must sweep background descendants that inherited the pipes and
/// would otherwise keep the generation (and its readers) alive forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn root_exit_sweeps_background_descendant() {
	let dir = TempDir::new("descendant");
	let manager = Arc::new(ProcessManager::new(dir.path.clone()));
	let launch = spec(
		&dir,
		"tree",
		"sh",
		&["-c", "sleep 30 & echo $! > child.pid; exit 0"],
	);
	manager.start(launch, None).await.expect("start");
	let exit = manager
		.wait("tree", None, WaitRequest { for_exit: true, pattern: None }, 3_000)
		.await
		.expect("wait");
	assert!(terminal(exit.snapshot.state));
	let child = std::fs::read_to_string(dir.path.join("child.pid"))
		.expect("child pid file")
		.trim()
		.parse::<u32>()
		.expect("child pid");
	for _ in 0..40 {
		if !alive(child) {
			break;
		}
		tokio::time::sleep(Duration::from_millis(50)).await;
	}
	assert!(!alive(child), "descendant {child} survived root exit");
}

/// A stop that lands while a restart is armed must win: after the restart delay
/// elapses no replacement generation may spawn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_during_armed_restart_never_relaunches() {
	let dir = TempDir::new("restart-stop");
	let manager = Arc::new(ProcessManager::new(dir.path.clone()));
	let mut launch = spec(&dir, "crashy", "sh", &["-c", "exit 1"]);
	launch.restart = RestartPolicy::Always;
	manager.start(launch, None).await.expect("start");
	tokio::time::sleep(Duration::from_millis(200)).await;
	let stopped = manager.stop("crashy", None, 2_000).await.expect("stop");
	assert!(terminal(stopped.state));
	let restarts = stopped.restart_count;
	tokio::time::sleep(Duration::from_millis(2_500)).await;
	let final_state = state_of(&manager, "crashy").expect("record retained");
	assert!(terminal(final_state), "restart sed after stop: {final_state:?}");
	let later = manager.stop("crashy", None, 1_000).await.expect("idempotent stop");
	assert_eq!(later.restart_count, restarts, "a stopped record gained a restart");
}

/// Adoption trusts only the recorded `(pid, start-time token)`: a stale
/// snapshot must not pin (or later signal) whatever process now owns that pid.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_identity_is_not_pinned_on_adoption() {
	let dir = TempDir::new("identity");
	let manager = Arc::new(ProcessManager::new_keepalive(dir.path.clone()));
	let mut decoy = std::process::Command::new("sleep")
		.arg("30")
		.spawn()
		.expect("decoy spawn");
	let pid = decoy.id();
	let mut launch = spec(&dir, "adopt", "sleep", &["30"]);
	launch.detached = true;
	launch.persist = true;
	let snapshot = pi_agent::tools::process::ProcessSnapshot {
		name: "adopt".to_string(),
		id: "stale".to_string(),
		state: ProcessState::Running,
		pid: Some(pid),
		created_at: 1,
		started_at: 1,
		ready_at: None,
		exited_at: None,
		exit_code: None,
		exit_reason: None,
		restart_count: 0,
		output_bytes: 0,
		owner: None,
		ready_match: None,
		ready_pending: None,
		start_identity: Some(StartIdentity { pid, token: "not-the-real-token".to_string() }),
		persist: true,
		detached: true,
	};
	let adopted = manager.adopt_detached(launch, snapshot).expect("adopt");
	assert!(terminal(adopted.state), "stale identity was treated as alive");
	assert!(adopted.pid.is_none(), "stale pid was pinned");
	assert!(alive(pid), "adoption signal the unrelated pid owner");
	let _ = decoy.kill();
	let _ = decoy.wait();
}

/// Local runner mode must reject persistence instead of faking it; broker mode
/// keeps a detached child physically alive across manager drop, bounded by the
/// real process (no pipe/stdin surrogate that could impersonate it).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detached_physical_lifetime_is_backend_gated() {
	let dir = TempDir::new("detached");
	let local = Arc::new(ProcessManager::new(dir.path.clone()));
	let mut launch = spec(&dir, "detached", "sleep", &["30"]);
	launch.detached = true;
	launch.persist = true;
	let rejected = local.start(launch.clone(), None).await;
	assert!(rejected.is_err(), "local manager claimed persistence");

	let broker = Arc::new(ProcessManager::new_keepalive(dir.path.clone()));
	let started = broker.start(launch, None).await.expect("broker-owned start");
	let pid = started.snapshot.pid.expect("detached pid");
	assert!(alive(pid));
	drop(broker);
	assert!(alive(pid), "detached child died with the broker-owned manager");
	let _ = std::process::Command::new("kill").arg("-9").arg(pid.to_string()).status();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_while_restart_armed_never_double_spawns() {
	let dir = TempDir::new("start-restart");
	let manager = Arc::new(ProcessManager::new(dir.path.clone()));
	let mut launch = spec(&dir, "race", "sh", &["-c", "exit 1"]);
	launch.restart = RestartPolicy::Always;
	manager.start(launch.clone(), None).await.expect("start");
	for _ in 0..100 {
		if state_of(&manager, "race") == Some(ProcessState::Restarting) {
			break;
		}
		tokio::time::sleep(Duration::from_millis(25)).await;
	}
	assert_eq!(state_of(&manager, "race"), Some(ProcessState::Restarting));
	let replacement = manager.start(launch, None).await;
	assert!(replacement.is_err(), "start must not rearm a restarting record");
	tokio::time::sleep(Duration::from_millis(2_500)).await;
	let snapshots = manager.list(None);
	assert_eq!(snapshots.len(), 1, "rearm created a second record");
	assert_eq!(snapshots[0].restart_count, 2, "timer and start both launched");
	manager.stop("race", None, 2_000).await.expect("stop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_manager_drop_kills_quiet_pty_child() {
	let dir = TempDir::new("external-drop");
	let manager = Arc::new(ProcessManager::new(dir.path.clone()));
	let mut launch = spec(&dir, "quiet", "sh", &["-c", "sleep 30"]);
	launch.pty = true;
	// PTY spawns use a cleared environment plus this map; without it `sh`
	// cannot be exec'd and the launch would settle as failed.
	launch.env = std::env::vars().collect();
	let started = manager.start(launch, None).await.expect("start");
	assert_eq!(
		started.snapshot.state,
		ProcessState::Running,
		"pty launch did not reach Running: {:?} {:?}",
		started.snapshot.state,
		started.snapshot.exit_reason
	);
	let pid = started.snapshot.pid.expect("running pty launch must expose pid");
	assert!(alive(pid));
	drop(manager);
	for _ in 0..40 {
		if !alive(pid) {
			break;
		}
		tokio::time::sleep(Duration::from_millis(50)).await;
	}
	assert!(!alive(pid), "last external manager drop left quiet PTY child {pid}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detached_output_exceeds_display_cap_with_monotonic_cursor() {
	let dir = TempDir::new("detached-cursor");
	let manager = Arc::new(ProcessManager::new_keepalive(dir.path.clone()));
	let mut launch = spec(
		&dir,
		"mono",
		"sh",
		&["-c", "head -c 400000 /dev/zero | tr '\\0' 'a'; sleep 5"],
	);
	launch.detached = true;
	launch.persist = true;
	let started = manager.start(launch, None).await.expect("start");
	let pid = started.snapshot.pid.expect("pid");
	tokio::time::sleep(Duration::from_millis(800)).await;
	let options = pi_agent::tools::process::LogOptions {
		lines: 10,
		head: false,
		grep: None,
		follow: false,
		cursor: None,
		timeout_ms: 1_000,
	};
	let first = manager.logs("mono", None, options.clone()).await.expect("logs");
	assert!(first.text.len() <= 256 * 1024, "display cap exceeded: {}", first.text.len());
	assert!(first.cursor >= 300_000, "detached file was truncated below its output: {}", first.cursor);
	let second = manager.logs("mono", None, options).await.expect("logs again");
	assert!(second.cursor >= first.cursor, "detached cursor rewind: {} -> {}", first.cursor, second.cursor);
	let _ = std::process::Command::new("kill").arg("-9").arg(pid.to_string()).status();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawned_event_precedes_readiness_with_private_spec() {
	let dir = TempDir::new("events");
	let manager = Arc::new(ProcessManager::new(dir.path.clone()));
	let mut events = manager.events();
	let mut launch = spec(&dir, "pending", "sh", &["-c", "sleep 30"]);
	launch.ready = Some(ReadySpec {
		log: Some("NEVER_MATCH_THIS".to_string()),
		port: None,
		host: None,
		timeout_ms: 300,
	});
	launch.env.insert("PROCESS_CONTRACT".to_string(), "secret".to_string());
	let outcome = manager.start(launch, None).await.expect("start");
	assert!(outcome.ready_timed_out, "readiness unexpectedly settled");
	assert_eq!(outcome.snapshot.state, ProcessState::Starting);
	let event =
		tokio::time::timeout(Duration::from_secs(1), events.recv()).await.expect("event").expect("recv");
	assert_eq!(event.kind, pi_agent::tools::process::ProcessEventKind::Spawned);
	assert!(event.snapshot.pid.is_some(), "Spawned arrived without a pid");
	assert_eq!(event.spec.env.get("PROCESS_CONTRACT").map(String::as_str), Some("secret"));
	let (described_snapshot, described) = manager.describe("pending", None).expect("describe");
	assert_eq!(described_snapshot.name, "pending");
	// describe is the private control path: it must keep the full launch spec
	// (including env) so restart/describe callers can round-trip it.
	assert_eq!(
		described.env.get("PROCESS_CONTRACT").map(String::as_str),
		Some("secret"),
		"describe dropped the private launch env"
	);
	// The public wire snapshot has no env field and must not leak it.
	let public = serde_json::to_value(&described_snapshot).expect("serialize snapshot");
	assert!(public.get("env").is_none(), "public snapshot gained an env field");
	let wire = public.to_string();
	assert!(
		!wire.contains("PROCESS_CONTRACT") && !wire.contains("secret"),
		"public snapshot leaked launch env: {wire}"
	);
	manager.stop("pending", None, 2_000).await.expect("stop");
}
