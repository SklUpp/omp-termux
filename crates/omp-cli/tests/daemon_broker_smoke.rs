#![cfg(unix)]

//! Real two-process broker smoke: worker binary + socket client(s).
//!
//! Exercises the acceptance path the TS daemon covers — start `persist`, drop
//! the client (a CLI exit), reconnect from a fresh client, read logs, stop —
//! and the detached path across a full broker restart (recovery/adoption).
//!
//! Requires the built `omp` binary (`CARGO_BIN_EXE_omp`); no model call, no
//! network.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use pi_agent::process_broker::{
	client::BrokerClient,
	protocol::{self, IDLE_GRACE_ENV, PROJECT_DIR_ENV, RUNTIME_DIR_ENV, WORKER_ARG, WaitTarget, WireSpec},
};
use pi_agent::tools::process::{LogOptions, ProcessState, RestartPolicy, is_terminal_state};

const MARKER: &str = "BROKER_SMOKE_ALIVE";
const SECRET: &str = "supersecret-value";
const DETACHED_MARKER: &str = "BROKER_DETACHED_ALIVE";
const IDLE_MS: u64 = 500;

struct Worker {
	child: Option<Child>,
}

impl Worker {
	fn spawn(project: &Path, runtime: &Path) -> Self {
		Self::spawn_with_logical_grace(project, runtime, None, IDLE_MS)
	}

	fn spawn_with_logical(project: &Path, runtime: &Path, logical: Option<&Path>) -> Self {
		Self::spawn_with_logical_grace(project, runtime, logical, IDLE_MS)
	}

	fn spawn_with_logical_grace(
		project: &Path,
		runtime: &Path,
		logical: Option<&Path>,
		idle_ms: u64,
	) -> Self {
		let mut command = Command::new(env!("CARGO_BIN_EXE_omp"));
		command
			.arg(WORKER_ARG)
			.env(PROJECT_DIR_ENV, project)
			.env(RUNTIME_DIR_ENV, runtime)
			.env(IDLE_GRACE_ENV, idle_ms.to_string());
		if let Some(logical) = logical {
			command.env(protocol::RUNTIME_ROOT_ENV, logical);
		}
		command
			.stdin(Stdio::null())
			.stdout(Stdio::null())
			.stderr(Stdio::null());
		#[cfg(unix)]
		{
			use std::os::unix::process::CommandExt;
			command.process_group(0);
		}
		let child = command.spawn().expect("spawn broker worker");
		Self { child: Some(child) }
	}

	/// Simulate a broker crash: SIGKILL the worker, leaving its lease, socket
	/// and metadata exactly as an OOM/`kill -9` would.
	fn crash(&mut self) {
		if let Some(mut child) = self.child.take() {
			let _ = child.kill();
			let _ = child.wait();
		}
	}

	async fn wait_exit(&mut self) -> bool {
		let deadline = Instant::now() + Duration::from_secs(10);
		loop {
			let status = match self.child.as_mut() {
				Some(child) => child.try_wait(),
				None => return true,
			};
			match status {
				Ok(Some(_)) => {
					self.child = None;
					return true;
				},
				Ok(None) => {},
				Err(_) => return false,
			}
			if Instant::now() >= deadline {
				return false;
			}
			tokio::time::sleep(Duration::from_millis(25)).await;
		}
	}
}

impl Drop for Worker {
	fn drop(&mut self) {
		if let Some(mut child) = self.child.take() {
			let _ = child.kill();
			let _ = child.wait();
		}
	}
}

struct Scratch {
	root: PathBuf,
	project: PathBuf,
	runtime: PathBuf,
}

impl Scratch {
	fn new(tag: &str) -> Self {
		let root = std::env::temp_dir().join(format!(
			"omp-broker-smoke-{tag}-{}-{}",
			std::process::id(),
			protocol::random_hex(6).expect("random")
		));
		let project = root.join("project");
		let runtime = root.join("runtime");
		std::fs::create_dir_all(&project).expect("project dir");
		Self { root, project, runtime }
	}

	fn endpoint(&self) -> PathBuf {
		self.runtime.join(protocol::SOCKET_FILE)
	}

	async fn wait_socket(&self) {
		let deadline = Instant::now() + Duration::from_secs(10);
		while !self.endpoint().exists() {
			assert!(Instant::now() < deadline, "broker socket never appeared");
			tokio::time::sleep(Duration::from_millis(25)).await;
		}
	}
}

impl Drop for Scratch {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.root);
	}
}

fn spec(name: &str, script: &str, detached: bool, cwd: &Path) -> WireSpec {
	// Complete child environment (full_env): the broker must use it verbatim.
	let mut env: BTreeMap<String, String> = std::env::vars_os()
		.filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
		.collect();
	env.insert("SECRET_TOKEN".to_string(), SECRET.to_string());
	WireSpec {
		name: name.to_string(),
		application: "sh".to_string(),
		args: vec!["-c".to_string(), script.to_string()],
		env,
		full_env: true,
		cwd: cwd.to_string_lossy().into_owned(),
		pty: false,
		ready: None,
		restart: RestartPolicy::No,
		persist: true,
		detached,
	}
}

fn logs_options() -> LogOptions {
	LogOptions {
		lines: 200,
		head: false,
		grep: None,
		follow: false,
		cursor: None,
		timeout_ms: 1_000,
	}
}

async fn wait_for_log(client: &BrokerClient, name: &str, needle: &str) -> String {
	let deadline = Instant::now() + Duration::from_secs(10);
	loop {
		let logs = client
			.logs(name, logs_options(), None)
			.await
			.expect("logs through broker");
		if logs.text.contains(needle) {
			return logs.text;
		}
		assert!(
			Instant::now() < deadline,
			"log marker {needle} never appeared; last text: {:?}",
			logs.text
		);
		tokio::time::sleep(Duration::from_millis(50)).await;
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persist_child_survives_client_exit_and_reconnects() {
	let scratch = Scratch::new("persist");
	let mut worker = Worker::spawn(&scratch.project, &scratch.runtime);
	scratch.wait_socket().await;

	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		let mode = std::fs::metadata(scratch.endpoint()).expect("socket metadata").permissions().mode();
		assert_eq!(mode & 0o777, 0o600, "broker socket must be 0600");
	}

	let first = BrokerClient::connect_existing(&scratch.project, &scratch.runtime)
		.await
		.expect("first client");
	let script = format!("printf '{MARKER}\\n'; sleep 30");
	let started = first
		.start(spec("sleeper", &script, false, &scratch.project), None)
		.await
		.expect("start persist daemon");
	let pid = started.daemon.pid.expect("persist daemon has a pid");
	assert!(!is_terminal_state(started.daemon.state));
	wait_for_log(&first, "sleeper", MARKER).await;

	// A CLI exit: the connection and presence marker go away, the child does not.
	drop(first);
	tokio::time::sleep(Duration::from_millis(IDLE_MS + 250)).await;

	let second = BrokerClient::connect_existing(&scratch.project, &scratch.runtime)
		.await
		.expect("second client reconnects");
	let listed = second.list(None).await.expect("list after reconnect");
	let sleeper = listed
		.iter()
		.find(|daemon| daemon.name == "sleeper")
		.expect("persist daemon is still registered after the client exited");
	assert_eq!(sleeper.pid, Some(pid), "same pinned child identity");
	assert!(!is_terminal_state(sleeper.state), "child survived the client exit");
	let described = second.describe("sleeper", None).await.expect("describe");
	assert!(described.spec.persist);
	assert_eq!(described.daemon.pid, Some(pid));
	let public = serde_json::to_string(&described.spec).expect("serialize public spec");
	assert!(!public.contains("SECRET"), "public spec leaks env keys: {public}");
	assert!(!public.contains(SECRET), "public spec leaks env values");

	let stopped = second.stop("sleeper", 2_000, None).await.expect("stop");
	assert!(is_terminal_state(stopped.state), "stop settles the daemon: {:?}", stopped.state);

	// Broker-side deadline arming (bash async) uses the manager's own timeout
	// vocabulary, not a client-side kill.
	let timed = second
		.start(spec("timed", "sleep 30", false, &scratch.project), None)
		.await
		.expect("start timed daemon");
	assert!(timed.daemon.pid.is_some());
	second.arm_timeout("timed", 250, None).await.expect("arm timeout");
	let waited = second
		.wait("timed", WaitTarget::Exit, None, 10_000, None)
		.await
		.expect("wait for timeout exit");
	assert!(is_terminal_state(waited.daemon.state), "timeout settles the daemon");

	second.shutdown().await.expect("stop the broker");
	assert!(worker.wait_exit().await, "broker worker exits after shutdown");
	assert!(!scratch.endpoint().exists(), "broker socket is removed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detached_child_survives_broker_restart() {
	let scratch = Scratch::new("detached");
	let mut first_worker = Worker::spawn(&scratch.project, &scratch.runtime);
	scratch.wait_socket().await;

	let first = BrokerClient::connect_existing(&scratch.project, &scratch.runtime)
		.await
		.expect("first client");
	let script = format!("printf '{DETACHED_MARKER}\\n'; sleep 60");
	let started = first
		.start(spec("detached", &script, true, &scratch.project), None)
		.await
		.expect("start detached daemon");
	let pid = started.daemon.pid.expect("detached daemon has a pid");
	assert!(started.daemon.detached);
	assert!(started.daemon.persist, "detached forces persist");
	wait_for_log(&first, "detached", DETACHED_MARKER).await;

	// Full broker exit; the detached child must keep running and be adopted by
	// the next broker generation.
	first.shutdown().await.expect("stop the first broker");
	drop(first);
	assert!(first_worker.wait_exit().await, "first broker worker exits");
	assert!(!scratch.endpoint().exists(), "first broker socket removed");

	let mut second_worker = Worker::spawn(&scratch.project, &scratch.runtime);
	scratch.wait_socket().await;
	let second = BrokerClient::connect_existing(&scratch.project, &scratch.runtime)
		.await
		.expect("second client");
	let described = second.describe("detached", None).await.expect("adopt detached record");
	assert_eq!(described.daemon.pid, Some(pid), "same pinned detached child identity");
	assert!(!is_terminal_state(described.daemon.state), "detached child was adopted alive");
	assert!(described.spec.detached);
	wait_for_log(&second, "detached", DETACHED_MARKER).await;

	let stopped = second.stop("detached", 2_000, None).await.expect("stop detached");
	assert!(is_terminal_state(stopped.state));
	second.shutdown().await.expect("stop the second broker");
	assert!(second_worker.wait_exit().await, "second broker worker exits");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mismatched_start_identity_is_never_adopted_or_killed() {
	let scratch = Scratch::new("identity");
	let mut first_worker = Worker::spawn(&scratch.project, &scratch.runtime);
	scratch.wait_socket().await;

	let first = BrokerClient::connect_existing(&scratch.project, &scratch.runtime)
		.await
		.expect("first client");
	let script = format!("printf '{DETACHED_MARKER}\\n'; exec sleep 120");
	let started = first
		.start(spec("detached", &script, true, &scratch.project), None)
		.await
		.expect("start detached daemon");
	let pid = started.daemon.pid.expect("pid");
	wait_for_log(&first, "detached", DETACHED_MARKER).await;
	first.shutdown().await.expect("stop the first broker");
	drop(first);
	assert!(first_worker.wait_exit().await, "first broker worker exits");

	// Simulate a recycled/foreign PID: metadata now claims a different start
	// identity for the same PID. Recovery must not adopt or signal it.
	let meta_path = scratch
		.runtime
		.join(protocol::DAEMONS_DIR)
		.join("detached")
		.join(protocol::META_FILE);
	let raw = std::fs::read_to_string(&meta_path).expect("meta exists");
	let mut meta: serde_json::Value = serde_json::from_str(&raw).expect("meta json");
	assert!(
		meta["daemon"]["startIdentity"]["token"].is_string(),
		"metadata must persist the pinned start identity: {meta}"
	);
	meta["daemon"]["startIdentity"]["token"] = serde_json::Value::from("forged-token");
	std::fs::write(&meta_path, serde_json::to_vec(&meta).expect("encode meta")).expect("rewrite meta");
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		std::fs::set_permissions(&meta_path, std::fs::Permissions::from_mode(0o600)).expect("chmod meta");
	}

	let mut second_worker = Worker::spawn(&scratch.project, &scratch.runtime);
	scratch.wait_socket().await;
	let second = BrokerClient::connect_existing(&scratch.project, &scratch.runtime)
		.await
		.expect("second client");
	assert!(
		second.describe("detached", None).await.is_err(),
		"mismatched identity must not be adopted"
	);
	assert!(
		Path::new(&format!("/proc/{pid}")).exists(),
		"the unrelated live process must not be signalled"
	);
	let _ = std::process::Command::new("kill").arg("-9").arg(pid.to_string()).status();
	second.shutdown().await.expect("stop the second broker");
	assert!(second_worker.wait_exit().await, "second broker worker exits");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broker_crash_recovery_adopts_matching_identity() {
	let scratch = Scratch::new("crash");
	let mut first_worker = Worker::spawn(&scratch.project, &scratch.runtime);
	scratch.wait_socket().await;

	let first = BrokerClient::connect_existing(&scratch.project, &scratch.runtime)
		.await
		.expect("first client");
	let script = format!("printf '{DETACHED_MARKER}\\n'; sleep 60");
	let started = first
		.start(spec("detached", &script, true, &scratch.project), None)
		.await
		.expect("start detached daemon");
	let pid = started.daemon.pid.expect("pid");
	wait_for_log(&first, "detached", DETACHED_MARKER).await;

	first_worker.crash();
	assert!(
		first.list(None).await.is_err(),
		"a dead broker must surface a transport error, never an empty daemon table"
	);
	drop(first);
	assert!(
		scratch.endpoint().exists(),
		"a crashed broker leaves its stale socket behind"
	);

	let mut second_worker = Worker::spawn(&scratch.project, &scratch.runtime);
	scratch.wait_socket().await;
	let second = BrokerClient::connect_existing(&scratch.project, &scratch.runtime)
		.await
		.expect("second client after crash");
	let described = second.describe("detached", None).await.expect("adopt after crash");
	assert_eq!(described.daemon.pid, Some(pid), "matching identity is adopted");
	assert!(!is_terminal_state(described.daemon.state));
	wait_for_log(&second, "detached", DETACHED_MARKER).await;

	let stopped = second.stop("detached", 2_000, None).await.expect("stop adopted");
	assert!(is_terminal_state(stopped.state));
	second.shutdown().await.expect("stop the second broker");
	assert!(second_worker.wait_exit().await, "second broker worker exits");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn long_logical_root_shortens_socket_and_persists_across_clients() {
	let scratch = Scratch::new("long-root");
	// Logical root long enough that <logical>/<scope>/broker.sock exceeds 104.
	let long = scratch
		.root
		.join("daemons-".to_string() + &"x".repeat(70) + "-profile");
	std::fs::create_dir_all(&long).expect("long logical root");
	let physical = protocol::physical_runtime_dir_with(&long, &scratch.project)
		.expect("physical runtime for long root");
	let endpoint = protocol::endpoint(&physical).expect("short endpoint fits");
	assert!(
		!physical.starts_with(&long),
		"long logical root was used for the socket: {}",
		physical.display()
	);
	assert!(endpoint.as_os_str().len() <= protocol::MAX_SOCKET_PATH_BYTES);
	protocol::ensure_scope(&physical, &long, &scratch.project).expect("scope checkpoint");

	let mut worker = Worker::spawn_with_logical(&scratch.project, &physical, Some(&long));
	let deadline = Instant::now() + Duration::from_secs(10);
	while !endpoint.exists() {
		assert!(Instant::now() < deadline, "short-root broker socket never appeared");
		tokio::time::sleep(Duration::from_millis(25)).await;
	}

	// First CLI-like client establishes a persist child, then exits.
	let first = BrokerClient::connect_existing(&scratch.project, &physical)
		.await
		.expect("first client on short root");
	let script = format!("printf '{MARKER}\\n'; sleep 30");
	let started = first
		.start(spec("sleeper", &script, false, &scratch.project), None)
		.await
		.expect("start persist daemon on short root");
	let pid = started.daemon.pid.expect("persist daemon pid");
	wait_for_log(&first, "sleeper", MARKER).await;
	drop(first);
	tokio::time::sleep(Duration::from_millis(IDLE_MS + 250)).await;

	// Second CLI-like client reconnects through the same physical short root.
	let second = BrokerClient::connect_existing(&scratch.project, &physical)
		.await
		.expect("second client on short root");
	let listed = second.list(None).await.expect("list after reconnect");
	let sleeper = listed
		.iter()
		.find(|daemon| daemon.name == "sleeper")
		.expect("persist daemon survived the first client");
	assert_eq!(sleeper.pid, Some(pid));
	let stopped = second.stop("sleeper", 2_000, None).await.expect("stop");
	assert!(is_terminal_state(stopped.state));

	let scope = protocol::read_scope_record(&physical.join(protocol::SCOPE_FILE)).expect("scope");
	assert_eq!(scope.version, protocol::SCOPE_VERSION);
	assert_eq!(scope.logical_root, long.display().to_string());
	assert_eq!(scope.root_token.len(), 64);
	assert_eq!(scope.project_dir, protocol::canonical_project_dir(&scratch.project).display().to_string());

	second.shutdown().await.expect("shutdown short-root broker");
	assert!(worker.wait_exit().await, "short-root worker exits");
	assert!(!endpoint.exists(), "short-root socket removed");
	let _ = std::fs::remove_dir_all(&physical);
}

/// Run the real `omp ps` binary in a profile/agent sandbox.
fn ps_cli(agent: &Path, base: &Path, args: &[&str]) -> std::process::Output {
	Command::new(env!("CARGO_BIN_EXE_omp"))
		.args(args)
		.env("PI_CONFIG_DIR", base)
		.env("PI_CODING_AGENT_DIR", agent)
		.env_remove("OMP_PROFILE")
		.env_remove("PI_PROFILE")
		.env_remove(protocol::RUNTIME_ROOT_ENV)
		.stdin(Stdio::null())
		.output()
		.expect("run omp ps")
}

fn ps_row(stdout: &[u8], name: &str) -> serde_json::Value {
	let scopes: serde_json::Value = serde_json::from_slice(stdout).expect("ps --json parses");
	for scope in scopes.as_array().expect("scope array") {
		for row in scope["daemons"].as_array().expect("daemon array") {
			if row["name"] == name {
				return row.clone();
			}
		}
	}
	panic!("no {name} row in {}", String::from_utf8_lossy(stdout));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ps_cli_lists_logs_and_stops_a_task_owned_daemon() {
	let scratch = Scratch::new("ps-cli");
	let agent = scratch.root.join("agent");
	let root = agent.join("run").join("daemons");
	let project = protocol::canonical_project_dir(&scratch.project);
	let runtime =
		protocol::physical_runtime_dir_with(&root, &project).expect("physical runtime");
	if !runtime.starts_with(&root) {
		// A socket-path-shortened scope needs its logical-root checkpoint before
		// the worker will bind it.
		protocol::ensure_scope(&runtime, &root, &project).expect("scope checkpoint");
	}
	let mut worker = Worker::spawn_with_logical_grace(&project, &runtime, Some(&root), 60_000);
	let endpoint = runtime.join(protocol::SOCKET_FILE);
	let deadline = Instant::now() + Duration::from_secs(10);
	while !endpoint.exists() {
		assert!(Instant::now() < deadline, "ps broker socket never appeared");
		tokio::time::sleep(Duration::from_millis(25)).await;
	}

	let client = BrokerClient::connect_existing(&project, &runtime)
		.await
		.expect("broker client");
	let script = format!("printf '{MARKER}\\n'; sleep 45");
	client
		.start(spec("ps-smoke", &script, false, &project), None)
		.await
		.expect("start task-owned daemon");
	wait_for_log(&client, "ps-smoke", MARKER).await;
	drop(client);

	let project_arg = project.to_str().expect("utf8 project");
	let list = ps_cli(&agent, &scratch.root, &["ps", "--dir", project_arg, "--json"]);
	assert!(
		list.status.success(),
		"omp ps --json failed: {}",
		String::from_utf8_lossy(&list.stderr)
	);
	let stdout = String::from_utf8_lossy(&list.stdout);
	assert!(!stdout.contains(SECRET), "ps list leaked a child secret: {stdout}");
	let row = ps_row(&list.stdout, "ps-smoke");
	assert_eq!(row["supervised"], true, "live broker rows are supervised");
	assert_eq!(row["state"], "running");
	assert!(
		row["command"].as_str().unwrap_or_default().contains("sh -c"),
		"persisted launch command missing: {row}"
	);

	let logs = ps_cli(
		&agent,
		&scratch.root,
		&["ps", "logs", "ps-smoke", "--dir", project_arg, "--lines", "40", "--grep", MARKER],
	);
	assert!(
		logs.status.success(),
		"omp ps logs failed: {}",
		String::from_utf8_lossy(&logs.stderr)
	);
	let logs_out = String::from_utf8_lossy(&logs.stdout);
	assert!(logs_out.contains(MARKER), "logs missing marker: {logs_out}");
	assert!(logs_out.contains("[ps-smoke: running]"), "logs missing state tail: {logs_out}");

	let info = ps_cli(&agent, &scratch.root, &["ps", "info", "ps-smoke", "--dir", project_arg, "--json"]);
	assert!(
		info.status.success(),
		"omp ps info failed: {}",
		String::from_utf8_lossy(&info.stderr)
	);
	let info_out = String::from_utf8_lossy(&info.stdout);
	assert!(info_out.contains("\"spec\""), "info json lacks spec: {info_out}");
	assert!(!info_out.contains(SECRET), "info json leaked the child environment: {info_out}");

	let stopped = ps_cli(&agent, &scratch.root, &["ps", "stop", "ps-smoke", "--dir", project_arg, "--json"]);
	assert!(
		stopped.status.success(),
		"omp ps stop failed: {}",
		String::from_utf8_lossy(&stopped.stderr)
	);
	let stopped_state: serde_json::Value =
		serde_json::from_slice(&stopped.stdout).expect("stop json snapshot");
	assert!(
		is_terminal_state(
			serde_json::from_value::<ProcessState>(stopped_state["state"].clone()).expect("state")
		),
		"stop did not settle the daemon: {stopped_state}"
	);

	let verify = BrokerClient::connect_existing(&project, &runtime)
		.await
		.expect("verification client");
	let listed = verify.list(None).await.expect("list after stop");
	let daemon = listed.iter().find(|daemon| daemon.name == "ps-smoke").expect("row");
	assert!(is_terminal_state(daemon.state), "daemon still live: {:?}", daemon.state);
	verify.shutdown().await.expect("shutdown broker");
	assert!(worker.wait_exit().await, "ps broker worker exits");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ps_cli_all_enumerates_a_shortened_long_root_scope() {
	let scratch = Scratch::new("ps-long");
	// Force the socket path past `sun_path`: the live scope only exists in the
	// private shortened base, not under the logical runtime root.
	let agent = scratch.root.join(format!("agent-{}", "x".repeat(90)));
	let root = agent.join("run").join("daemons");
	let project = protocol::canonical_project_dir(&scratch.project);
	let runtime =
		protocol::physical_runtime_dir_with(&root, &project).expect("physical runtime");
	assert!(
		!runtime.starts_with(&root),
		"expected a shortened runtime, got {}",
		runtime.display()
	);
	protocol::ensure_scope(&runtime, &root, &project).expect("scope checkpoint");
	let mut worker = Worker::spawn_with_logical_grace(&project, &runtime, Some(&root), 60_000);
	let endpoint = runtime.join(protocol::SOCKET_FILE);
	let deadline = Instant::now() + Duration::from_secs(10);
	while !endpoint.exists() {
		assert!(Instant::now() < deadline, "long-root broker socket never appeared");
		tokio::time::sleep(Duration::from_millis(25)).await;
	}

	let client = BrokerClient::connect_existing(&project, &runtime)
		.await
		.expect("long-root client");
	let script = format!("printf '{MARKER}\\n'; sleep 30");
	client
		.start(spec("ps-long-smoke", &script, false, &project), None)
		.await
		.expect("start long-root daemon");
	wait_for_log(&client, "ps-long-smoke", MARKER).await;
	drop(client);

	let list = ps_cli(&agent, &scratch.root, &["ps", "--all", "--json"]);
	assert!(
		list.status.success(),
		"omp ps --all failed: {}",
		String::from_utf8_lossy(&list.stderr)
	);
	let scopes: serde_json::Value = serde_json::from_slice(&list.stdout).expect("ps --all json");
	let matching: Vec<&serde_json::Value> = scopes
		.as_array()
		.expect("scope array")
		.iter()
		.filter(|scope| {
			scope["daemons"]
				.as_array()
				.is_some_and(|daemons| daemons.iter().any(|row| row["name"] == "ps-long-smoke"))
		})
		.collect();
	assert_eq!(matching.len(), 1, "shortened scope listed exactly once: {:?}", list.stdout);
	assert_eq!(matching[0]["kind"], "project");
	let project_arg = project.to_str().expect("utf8 project");
	assert_eq!(matching[0]["projectDir"], project_arg);
	let row = ps_row(&list.stdout, "ps-long-smoke");
	assert_eq!(row["supervised"], true);
	assert!(
		row["command"].as_str().unwrap_or_default().contains("sh -c"),
		"persisted launch command missing: {row}"
	);

	let verify = BrokerClient::connect_existing(&project, &runtime)
		.await
		.expect("verification client");
	verify.stop("ps-long-smoke", 2_000, None).await.expect("stop");
	verify.shutdown().await.expect("shutdown broker");
	assert!(worker.wait_exit().await, "long-root broker worker exits");
}
