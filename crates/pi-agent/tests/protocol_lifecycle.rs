//! Consumer-level LSP and DAP protocol lifecycle regressions.
//!
//! Every test drives the public client/manager surface against a real fixture
//! subprocess over Content-Length framing. The fixtures log the actual wire
//! conversation, so assertions cover spawn/initialize/document sync, request
//! correlation, cancellation, process exit, restart, the DAP handshake order,
//! reverse requests, and representative inspection actions.

use std::{
	collections::HashMap,
	future::Future,
	path::{Path, PathBuf},
	sync::{
		Arc,
		atomic::{AtomicU64, Ordering},
	},
	time::{Duration, Instant},
};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use pi_agent::{
	dap::{
		AttachOptions, ConnectMode, DapSessionManager, LaunchOptions, ResolvedAdapter, SessionStatus,
	},
	lsp::{
		LspError,
		client::{
			ClientStatus, ensure_file_open, get_active_clients, get_or_create_client, notify_saved,
			shutdown_client_instance, sync_content,
		},
		mux::{endpoint_for_project, server::LspMuxServer},
		types::ServerConfig,
		utils::file_to_uri,
	},
};

// =============================================================================
// Fixtures and helpers
// =============================================================================

struct TestDir(PathBuf);

impl TestDir {
	fn new(label: &str) -> Self {
		static COUNTER: AtomicU64 = AtomicU64::new(0);
		let path = std::env::temp_dir().join(format!(
			"omp-protocol-{label}-{}-{}",
			std::process::id(),
			COUNTER.fetch_add(1, Ordering::Relaxed)
		));
		std::fs::create_dir_all(&path).expect("create test directory");
		Self(path)
	}

	fn path(&self) -> &Path {
		&self.0
	}
}

impl Drop for TestDir {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

fn fixture(name: &str) -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR"))
		.join("tests")
		.join("fixtures")
		.join("protocol_lifecycle")
		.join(name)
}

fn python_path() -> Option<PathBuf> {
	let env: HashMap<String, String> = std::env::vars().collect();
	pi_agent::eval::env::which("python3", &env).or_else(|| pi_agent::eval::env::which("python", &env))
}

fn require_python() -> Option<PathBuf> {
	let python = python_path();
	if python.is_none() {
		eprintln!("skipping protocol lifecycle test: no python interpreter on PATH");
	}
	python
}

async fn bounded<F: Future<Output = ()>>(label: &str, seconds: u64, future: F) {
	if tokio::time::timeout(Duration::from_secs(seconds), future).await.is_err() {
		panic!("{label} timed out after {seconds}s");
	}
}

fn read_log(path: &Path) -> Vec<Value> {
	std::fs::read_to_string(path)
		.unwrap_or_default()
		.lines()
		.filter_map(|line| serde_json::from_str(line).ok())
		.collect()
}

async fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
	let deadline = Instant::now() + timeout;
	loop {
		if predicate() {
			return true;
		}
		if Instant::now() >= deadline {
			return false;
		}
		tokio::time::sleep(Duration::from_millis(20)).await;
	}
}

fn incoming_index(entries: &[Value], method: &str) -> Option<usize> {
	entries
		.iter()
		.position(|entry| entry["dir"] == "in" && entry["msg"]["method"] == method)
}

fn incoming_request_index(entries: &[Value], command: &str) -> Option<usize> {
	entries.iter().position(|entry| {
		entry["dir"] == "in" && entry["msg"]["type"] == "request" && entry["msg"]["command"] == command
	})
}

fn outgoing_event_index(entries: &[Value], event: &str) -> Option<usize> {
	entries.iter().position(|entry| {
		entry["dir"] == "out" && entry["msg"]["type"] == "event" && entry["msg"]["event"] == event
	})
}

fn lsp_config(python: &Path, script: &Path, log: &Path) -> ServerConfig {
	ServerConfig {
		command: python.to_string_lossy().into_owned(),
		args: vec![script.to_string_lossy().into_owned(), log.to_string_lossy().into_owned()],
		file_types: vec![".test".to_string()],
		language_id: Some("testlang".to_string()),
		root_markers: Vec::new(),
		init_options: None,
		settings: Some(json!({ "test": { "flag": true } })),
		disabled: None,
		warmup_timeout_ms: None,
		workspace_ready_timings: None,
		capabilities: None,
		is_linter: None,
		resolved_command: Some(python.to_path_buf()),
		custom_linter: None,
	}
}

fn dap_adapter(python: &Path, script: &Path, log: &Path, name: &str) -> ResolvedAdapter {
	ResolvedAdapter {
		name: name.to_string(),
		command: python.to_string_lossy().into_owned(),
		args: vec![
			script.to_string_lossy().into_owned(),
			log.to_string_lossy().into_owned(),
			"protocol-fixture-sentinel".to_string(),
		],
		resolved_command: python.to_string_lossy().into_owned(),
		languages: Vec::new(),
		file_types: Vec::new(),
		root_markers: Vec::new(),
		launch_defaults: json!({}),
		attach_defaults: json!({}),
		connect_mode: ConnectMode::Stdio,
		accepts_directory_program: false,
	}
}

fn launch_options(adapter: ResolvedAdapter, cwd: &Path, program: &str) -> LaunchOptions {
	LaunchOptions {
		adapter,
		program: program.to_string(),
		args: None,
		cwd: cwd.to_path_buf(),
		extra_launch_arguments: json!({}),
		env: HashMap::new(),
	}
}

// =============================================================================
// LSP
// =============================================================================

#[tokio::test]
async fn lsp_lifecycle_initializes_configures_and_syncs_documents() {
	let Some(python) = require_python() else { return };
	bounded("lsp lifecycle", 30, async move {
		let dir = TestDir::new("lsp-lifecycle");
		let log = dir.path().join("lsp.log");
		let script = fixture("fake_lsp.py");
		let config = lsp_config(&python, &script, &log);
		let expected_argv = json!(&config.args);
		let cwd = dir.path().to_path_buf();
		let cancel = CancellationToken::new();

		let client = get_or_create_client(&config, &cwd, Some(Duration::from_secs(10)), &cancel, false)
			.await
			.expect("initialize fixture language server");
		assert_eq!(client.status(), ClientStatus::Ready);
		let capabilities = client.server_capabilities().expect("initialize capabilities");
		assert_eq!(capabilities.get("hoverProvider").and_then(Value::as_bool), Some(true));

		let file = cwd.join("sample.test");
		let uri = file_to_uri(&file);
		std::fs::write(&file, "fn main() {}\n").expect("write source file");

		ensure_file_open(&client, &file, &cancel).await.expect("didOpen");
		assert!(client.has_open_file(&uri));
		assert_eq!(client.open_file_version(&uri), Some(1));

		sync_content(&client, &file, "fn main() { println!(); }\n", &cancel)
			.await
			.expect("didChange");
		assert_eq!(client.open_file_version(&uri), Some(2));

		notify_saved(&client, &file, &cancel).await.expect("didSave");
		assert!(
			wait_until(Duration::from_secs(2), || {
				client.diagnostics_for(&uri).is_some_and(|published| {
					published
						.diagnostics
						.iter()
						.any(|entry| entry.message == "seen-v2")
				})
			})
			.await,
			"fixture did not publish the version-2 diagnostics"
		);
		let diagnostics = client.diagnostics_for(&uri).expect("diagnostics");
		assert!(diagnostics.diagnostics.iter().any(|entry| entry.message == "seen-v2"));

		client.close_file(&uri);
		assert!(!client.has_open_file(&uri));
		assert!(shutdown_client_instance(&client).await);

		let entries = read_log(&log);
		let initialize = incoming_index(&entries, "initialize").expect("initialize request");
		let initialized = incoming_index(&entries, "initialized").expect("initialized notification");
		let configuration =
			incoming_index(&entries, "workspace/didChangeConfiguration").expect("configuration notification");
		let open = incoming_index(&entries, "textDocument/didOpen").expect("didOpen");
		let change = incoming_index(&entries, "textDocument/didChange").expect("didChange");
		let save = incoming_index(&entries, "textDocument/didSave").expect("didSave");
		let startup = entries.iter().find(|entry| entry["dir"] == "argv").expect("fixture startup argv");
		assert_eq!(
			startup["msg"]["argv"], expected_argv,
			"spawned language server argv must equal the configured args"
		);
		assert!(
			initialize < initialized
				&& initialized < configuration
				&& configuration < open
				&& open < change
				&& change < save,
			"unexpected LSP lifecycle order: {entries:?}"
		);

		let init = &entries[initialize]["msg"]["params"];
		assert!(init["rootUri"].as_str().is_some_and(|value| value.starts_with("file://")));
		assert_eq!(init["workspaceFolders"].as_array().map(Vec::len), Some(1));
		assert!(init["capabilities"]["textDocument"].is_object());
		assert_eq!(init["processId"].as_u64(), Some(u64::from(std::process::id())));

		let open_params = &entries[open]["msg"]["params"]["textDocument"];
		assert_eq!(open_params["languageId"], "testlang");
		assert_eq!(open_params["text"], "fn main() {}\n");
		let change_params = &entries[change]["msg"]["params"];
		assert_eq!(change_params["textDocument"]["version"], 2);
		assert_eq!(change_params["contentChanges"][0]["text"], "fn main() { println!(); }\n");
		assert_eq!(entries[save]["msg"]["params"]["textDocument"]["uri"], uri);
		assert_eq!(
			entries[configuration]["msg"]["params"]["settings"]["test"]["flag"],
			true
		);
	})
	.await;
}

#[tokio::test]
async fn lsp_request_correlation_keeps_out_of_order_responses_paired() {
	let Some(python) = require_python() else { return };
	bounded("lsp correlation", 30, async move {
		let dir = TestDir::new("lsp-correlation");
		let log = dir.path().join("lsp.log");
		let script = fixture("fake_lsp.py");
		let config = lsp_config(&python, &script, &log);
		let cancel = CancellationToken::new();
		let client = get_or_create_client(&config, dir.path(), Some(Duration::from_secs(10)), &cancel, false)
			.await
			.expect("initialize fixture language server");

		let slow = client.send_request(
			"test/delayed",
			json!({ "n": 1, "delayMs": 150 }),
			&cancel,
			Some(Duration::from_secs(5)),
		);
		let fast = client.send_request(
			"test/delayed",
			json!({ "n": 2, "delayMs": 10 }),
			&cancel,
			Some(Duration::from_secs(5)),
		);
		let (slow, fast) = tokio::join!(slow, fast);
		assert_eq!(slow.expect("slow response")["echo"], 1);
		assert_eq!(fast.expect("fast response")["echo"], 2);
		assert_eq!(client.pending_count(), 0);

		let (first_id, second_id) = {
			let entries = read_log(&log);
			let delayed: Vec<&Value> = entries
				.iter()
				.filter(|entry| entry["dir"] == "in" && entry["msg"]["method"] == "test/delayed")
				.collect();
			assert_eq!(delayed.len(), 2, "both requests must reach the server");
			let id_for = |n: i64| {
				delayed
					.iter()
					.find(|entry| entry["msg"]["params"]["n"] == n)
					.expect("request id")
					["msg"]["id"]
					.as_i64()
					.expect("numeric request id")
			};
			let ids = (id_for(1), id_for(2));
			assert_ne!(ids.0, ids.1);
			ids
		};
		// The fixture flushes the frame before appending the log line, so the
		// response entries may land just after the caller's `join!` returns.
		assert!(
			wait_until(Duration::from_secs(2), || {
				let entries = read_log(&log);
				[first_id, second_id].iter().all(|id| {
					entries.iter().any(|entry| {
						entry["dir"] == "out" && entry["msg"]["id"].as_i64() == Some(*id)
					})
				})
			})
			.await,
			"outbound responses were not logged for both request ids"
		);
		let entries = read_log(&log);
		let response = |id: i64| {
			entries
				.iter()
				.find(|entry| entry["dir"] == "out" && entry["msg"]["id"] == id)
				.expect("response for request")
		};
		assert_eq!(response(first_id)["msg"]["result"]["echo"], 1);
		assert_eq!(response(second_id)["msg"]["result"]["echo"], 2);

		assert!(shutdown_client_instance(&client).await);
	})
	.await;
}

#[tokio::test]
async fn lsp_cancellation_and_deadline_emit_cancel_request_and_release_pending() {
	let Some(python) = require_python() else { return };
	bounded("lsp cancellation", 30, async move {
		let dir = TestDir::new("lsp-cancel");
		let log = dir.path().join("lsp.log");
		let script = fixture("fake_lsp.py");
		let config = lsp_config(&python, &script, &log);
		let cancel = CancellationToken::new();
		let client = get_or_create_client(&config, dir.path(), Some(Duration::from_secs(10)), &cancel, false)
			.await
			.expect("initialize fixture language server");

		let task_client = Arc::clone(&client);
		let request_token = cancel.child_token();
		let task = tokio::spawn(async move {
			task_client
				.send_request("test/hang", json!({}), &request_token, None)
				.await
		});
		assert!(
			wait_until(Duration::from_secs(2), || incoming_index(&read_log(&log), "test/hang").is_some()).await,
			"hang request never reached the fixture"
		);
		let hang_id = read_log(&log)
			.iter()
			.find(|entry| entry["dir"] == "in" && entry["msg"]["method"] == "test/hang")
			.expect("hang request")["msg"]["id"]
			.as_i64()
			.expect("numeric hang id");

		cancel.cancel();
		let error = task.await.expect("request task").expect_err("hang must be cancelled");
		assert!(matches!(error, LspError::Cancelled), "unexpected cancellation error: {error:?}");
		assert_eq!(client.pending_count(), 0);
		assert!(
			wait_until(Duration::from_secs(2), || {
				read_log(&log).iter().any(|entry| {
					entry["dir"] == "in"
						&& entry["msg"]["method"] == "$/cancelRequest"
						&& entry["msg"]["params"]["id"].as_i64() == Some(hang_id)
				})
			})
			.await,
			"cancellation did not emit $/cancelRequest for {hang_id}"
		);

		let timeout_token = CancellationToken::new();
		let timeout_error = client
			.send_request("test/hang", json!({}), &timeout_token, Some(Duration::from_millis(120)))
			.await
			.expect_err("request must time out");
		assert!(
			matches!(timeout_error, LspError::Timeout { .. }),
			"unexpected timeout error: {timeout_error:?}"
		);
		assert!(
			wait_until(Duration::from_secs(2), || {
				read_log(&log)
					.iter()
					.filter(|entry| entry["dir"] == "in" && entry["msg"]["method"] == "test/hang")
					.count()
					>= 2
			})
			.await,
			"timed-out request never reached the fixture"
		);
		let hang_requests: Vec<i64> = read_log(&log)
			.iter()
			.filter(|entry| entry["dir"] == "in" && entry["msg"]["method"] == "test/hang")
			.filter_map(|entry| entry["msg"]["id"].as_i64())
			.collect();
		assert_eq!(hang_requests.len(), 2);
		let timeout_id = hang_requests[1];
		assert!(
			wait_until(Duration::from_secs(2), || {
				read_log(&log).iter().any(|entry| {
					entry["dir"] == "in"
						&& entry["msg"]["method"] == "$/cancelRequest"
						&& entry["msg"]["params"]["id"].as_i64() == Some(timeout_id)
				})
			})
			.await,
			"deadline did not emit $/cancelRequest for {timeout_id}"
		);
		assert_eq!(client.pending_count(), 0);

		assert!(shutdown_client_instance(&client).await);
	})
	.await;
}

#[tokio::test]
async fn lsp_process_exit_rejects_pending_request_and_unregisters_client() {
	let Some(python) = require_python() else { return };
	bounded("lsp process exit", 30, async move {
		let dir = TestDir::new("lsp-exit");
		let log = dir.path().join("lsp.log");
		let script = fixture("fake_lsp.py");
		let mut config = lsp_config(&python, &script, &log);
		// `get_active_clients` reports the configured command, so make this
		// test's registration name unique instead of matching the shared
		// interpreter path that concurrent tests also register.
		let registry_name = format!("fake-lsp-exit-{}", std::process::id());
		config.command = registry_name.clone();
		let cancel = CancellationToken::new();
		let client = get_or_create_client(&config, dir.path(), Some(Duration::from_secs(10)), &cancel, false)
			.await
			.expect("initialize fixture language server");

		let error = client
			.send_request("test/crash", json!({}), &cancel, Some(Duration::from_secs(5)))
			.await
			.expect_err("server exit must reject the pending request");
		let message = error.message().to_lowercase();
		assert!(
			message.contains("exited") || message.contains("closed"),
			"unexpected exit error: {message}"
		);
		assert_eq!(client.pending_count(), 0);
		assert!(wait_until(Duration::from_secs(2), || client.status() == ClientStatus::Error).await);
		assert!(
			wait_until(Duration::from_secs(2), || {
				!get_active_clients().iter().any(|status| status.name == registry_name)
			})
			.await,
			"exited client stayed registered"
		);
	})
	.await;
}

#[tokio::test]
async fn lsp_private_clients_are_isolated_per_workspace_and_restart_replaces_process() {
	let Some(python) = require_python() else { return };
	bounded("lsp isolation and restart", 40, async move {
		let dir_a = TestDir::new("lsp-a");
		let dir_b = TestDir::new("lsp-b");
		let log = dir_a.path().join("lsp.log");
		let script = fixture("fake_lsp.py");
		let config = lsp_config(&python, &script, &log);
		let cancel = CancellationToken::new();

		let client_a = get_or_create_client(&config, dir_a.path(), Some(Duration::from_secs(10)), &cancel, false)
			.await
			.expect("client A");
		let client_b = get_or_create_client(&config, dir_b.path(), Some(Duration::from_secs(10)), &cancel, false)
			.await
			.expect("client B");
		assert!(!Arc::ptr_eq(&client_a, &client_b));
		assert_ne!(client_a.name, client_b.name);
		assert_ne!(client_a.pid, client_b.pid);

		let file = dir_a.path().join("only-a.test");
		let uri = file_to_uri(&file);
		std::fs::write(&file, "alpha\n").expect("write source file");
		ensure_file_open(&client_a, &file, &cancel).await.expect("didOpen on A");
		assert!(client_a.has_open_file(&uri));
		assert!(!client_b.has_open_file(&uri));

		let old_pid = client_a.pid;
		assert!(shutdown_client_instance(&client_a).await);
		let restarted = get_or_create_client(&config, dir_a.path(), Some(Duration::from_secs(10)), &cancel, false)
			.await
			.expect("restarted client A");
		assert_ne!(restarted.pid, old_pid, "restart must spawn a fresh process");
		assert_eq!(restarted.status(), ClientStatus::Ready);
		assert!(!restarted.has_open_file(&uri), "restart must start from empty document state");

		let root_a = file_to_uri(dir_a.path());
		let initializes_for_a = read_log(&log)
			.iter()
			.filter(|entry| {
				entry["dir"] == "in"
					&& entry["msg"]["method"] == "initialize"
					&& entry["msg"]["params"]["rootUri"] == root_a
			})
			.count();
		assert_eq!(initializes_for_a, 2, "workspace A must have initialized twice");

		assert!(shutdown_client_instance(&client_b).await);
		assert!(shutdown_client_instance(&restarted).await);
	})
	.await;
}

#[tokio::test]
async fn lsp_shared_mux_keeps_a_separate_transport_from_private() {
	let Some(python) = require_python() else { return };
	bounded("lsp shared mux", 45, async move {
		let dir = TestDir::new("lsp-shared");
		let log = dir.path().join("lsp.log");
		let script = fixture("fake_lsp.py");
		let config = lsp_config(&python, &script, &log);
		let cancel = CancellationToken::new();

		let private = get_or_create_client(&config, dir.path(), Some(Duration::from_secs(10)), &cancel, false)
			.await
			.expect("private client");
		let private_again = get_or_create_client(&config, dir.path(), Some(Duration::from_secs(10)), &cancel, false)
			.await
			.expect("private client reuse");
		assert!(
			Arc::ptr_eq(&private, &private_again),
			"private preference must reuse the same process for one workspace"
		);

		// Bind the real mux server in-process so the shared path is exercised
		// end to end without a daemon broker (which resolves to the libtest
		// harness under `current_exe` and would silently fall back to private).
		let endpoint = endpoint_for_project(dir.path()).expect("mux endpoint");
		let server = Arc::new(LspMuxServer::new());
		server.listen(&endpoint).await.expect("bind in-process mux");
		let shared = get_or_create_client(&config, dir.path(), Some(Duration::from_secs(10)), &cancel, true)
			.await
			.expect("shared client");
		assert!(shared.shared_mux, "shared preference must connect through the mux transport");
		assert!(!Arc::ptr_eq(&private, &shared));
		assert_ne!(private.name, shared.name, "shared and private preferences must not alias");
		assert_ne!(private.pid, shared.pid, "the mux link must own its own language server child");

		for (label, client) in [("private", &private), ("shared", &shared)] {
			let echo = client
				.send_request(
					"test/delayed",
					json!({ "n": 9, "delayMs": 0 }),
					&cancel,
					Some(Duration::from_secs(5)),
				)
				.await
				.unwrap_or_else(|error| panic!("{label} client request failed: {error}"));
			assert_eq!(echo["echo"], 9, "{label} client response mismatch");
		}

		let _ = shutdown_client_instance(&shared).await;
		server.shutdown().await;
		assert!(shutdown_client_instance(&private).await);
	})
	.await;
}

// =============================================================================
// DAP
// =============================================================================

#[tokio::test]
async fn dap_real_subprocess_launch_orders_handshake_and_handles_reverse_requests() {
	let Some(python) = require_python() else { return };
	bounded("dap launch", 45, async move {
		let dir = TestDir::new("dap-launch");
		let log = dir.path().join("dap.log");
		let script = fixture("fake_dap.py");
		let adapter = dap_adapter(&python, &script, &log, "fake-dap");
		let expected_argv = json!(&adapter.args);
		let manager = DapSessionManager::new();
		let cancel = CancellationToken::new();
		let deadline = Instant::now() + Duration::from_secs(30);

		let summary = manager
			.launch(launch_options(adapter, dir.path(), "reverse"), &cancel, deadline)
			.await
			.expect("launch fixture adapter");
		assert_eq!(summary.adapter, "fake-dap");
		assert_eq!(summary.program.as_deref(), Some("reverse"));
		assert_eq!(summary.status, SessionStatus::Stopped);
		assert_eq!(summary.thread_id, Some(7));

		let entries = read_log(&log);
		let initialize = incoming_request_index(&entries, "initialize").expect("initialize request");
		let launch = incoming_request_index(&entries, "launch").expect("launch request");
		let initialized = outgoing_event_index(&entries, "initialized").expect("initialized event");
		let configuration = incoming_request_index(&entries, "configurationDone").expect("configurationDone");
		let startup = entries.iter().find(|entry| entry["dir"] == "argv").expect("fixture startup argv");
		assert_eq!(
			startup["msg"]["argv"], expected_argv,
			"spawned adapter argv must equal the configured args exactly"
		);
		assert_eq!(
			entries[initialize]["msg"]["arguments"]["adapterID"], "fake-dap",
			"initialize payload must carry the configured adapter name"
		);
		assert!(
			initialize < launch && launch < initialized && initialized < configuration,
			"launch handshake order is wrong: {entries:?}"
		);
		assert_eq!(
			entries
				.iter()
				.filter(|entry| entry["dir"] == "in" && entry["msg"]["command"] == "configurationDone")
				.count(),
			1,
			"configurationDone must be sent once"
		);
		assert_eq!(entries[launch]["msg"]["arguments"]["program"], "reverse");
		assert_eq!(entries[launch]["msg"]["arguments"]["cwd"], dir.path().to_string_lossy().as_ref());

		assert!(
			wait_until(Duration::from_secs(5), || {
				let entries = read_log(&log);
				["runInTerminal", "startDebugging"].into_iter().all(|command| {
					entries.iter().any(|entry| {
						entry["dir"] == "in"
							&& entry["msg"]["type"] == "response"
							&& entry["msg"]["command"] == command
					})
				})
			})
			.await,
			"reverse request responses missing from the wire log"
		);
		let entries = read_log(&log);
		let run_terminal = entries
			.iter()
			.find(|entry| {
				entry["dir"] == "in"
					&& entry["msg"]["type"] == "response"
					&& entry["msg"]["command"] == "runInTerminal"
			})
			.expect("runInTerminal response");
		assert_eq!(run_terminal["msg"]["success"], true);
		assert!(run_terminal["msg"]["body"]["processId"].as_u64().is_some());
		let start_debugging = entries
			.iter()
			.find(|entry| {
				entry["dir"] == "in"
					&& entry["msg"]["type"] == "response"
					&& entry["msg"]["command"] == "startDebugging"
			})
			.expect("startDebugging response");
		assert_eq!(start_debugging["msg"]["success"], false);
		assert!(
			start_debugging["msg"]["message"]
				.as_str()
				.unwrap_or_default()
				.contains("cannot accept child session connections"),
			"stdio startDebugging must fail explicitly: {start_debugging:?}"
		);

		let mut output = String::new();
		let output_deadline = Instant::now() + Duration::from_secs(3);
		while Instant::now() < output_deadline {
			output = manager.get_output().map(|snapshot| snapshot.output).unwrap_or_default();
			if output.contains("dumb|true") {
				break;
			}
			tokio::time::sleep(Duration::from_millis(20)).await;
		}
		assert!(output.contains("dumb|true"), "runInTerminal output/env missing: {output:?}");

		let threads = manager.threads(&cancel, deadline).await.expect("threads");
		assert_eq!(threads.threads.len(), 1);
		assert_eq!(threads.threads[0].id, 7);
		assert_eq!(threads.threads[0].name, "main");

		let stack = manager.stack_trace(Some(5), &cancel, deadline).await.expect("stack trace");
		assert_eq!(stack.stack_frames.len(), 1);
		assert_eq!(stack.stack_frames[0].id, 11);
		assert_eq!(stack.stack_frames[0].line, 3);
		assert_eq!(stack.stack_frames[0].source.as_ref().and_then(|source| source.path.as_deref()), Some("/tmp/main.py"));

		let scopes = manager.scopes(Some(11), &cancel, deadline).await.expect("scopes");
		assert_eq!(scopes.scopes.len(), 1);
		assert_eq!(scopes.scopes[0].variables_reference, 21);

		let variables = manager.variables(21, &cancel, deadline).await.expect("variables");
		assert_eq!(variables.variables.len(), 1);
		assert_eq!(variables.variables[0].name, "answer");
		assert_eq!(variables.variables[0].value, "42");

		let evaluation = manager
			.evaluate("answer + 1", "repl", Some(11), &cancel, deadline)
			.await
			.expect("evaluate");
		assert_eq!(evaluation.evaluation.result, "43");
		assert_eq!(evaluation.evaluation.r#type.as_deref(), Some("int"));

		let memory = manager
			.read_memory("0x1000", 4, None, &cancel, deadline)
			.await
			.expect("read memory");
		assert_eq!(memory.address, "0x1000");
		assert_eq!(memory.data.as_deref(), Some("AQIDBA=="));
		assert_eq!(memory.unreadable_bytes, Some(0));

		let custom = manager
			.custom_request("custom/echo", Some(json!({ "x": 1 })), &cancel, deadline)
			.await
			.expect("custom request");
		assert_eq!(custom.body["marker"], "custom");
		assert_eq!(custom.body["echoed"]["x"], 1);

		let entries = read_log(&log);
		let stack_request = entries
			.iter()
			.rev()
			.find(|entry| {
				entry["dir"] == "in"
					&& entry["msg"]["command"] == "stackTrace"
					&& entry["msg"]["arguments"]["levels"] == 5
			})
			.expect("stackTrace with levels");
		assert_eq!(stack_request["msg"]["arguments"]["threadId"], 7);
		let evaluate_request = entries
			.iter()
			.rev()
			.find(|entry| entry["dir"] == "in" && entry["msg"]["command"] == "evaluate")
			.expect("evaluate request");
		assert_eq!(evaluate_request["msg"]["arguments"]["expression"], "answer + 1");
		assert_eq!(evaluate_request["msg"]["arguments"]["context"], "repl");
		assert_eq!(evaluate_request["msg"]["arguments"]["frameId"], 11);
		let memory_request = entries
			.iter()
			.rev()
			.find(|entry| entry["dir"] == "in" && entry["msg"]["command"] == "readMemory")
			.expect("readMemory request");
		assert_eq!(memory_request["msg"]["arguments"]["memoryReference"], "0x1000");
		assert_eq!(memory_request["msg"]["arguments"]["count"], 4);
		let custom_request = entries
			.iter()
			.rev()
			.find(|entry| entry["dir"] == "in" && entry["msg"]["command"] == "custom/echo")
			.expect("custom request");
		assert_eq!(custom_request["msg"]["arguments"]["x"], 1);

		let terminated = manager
			.terminate(&cancel, Instant::now() + Duration::from_secs(5))
			.await
			.expect("terminate");
		assert!(terminated.is_some());
		assert!(manager.list_sessions().is_empty());
		let entries = read_log(&log);
		assert!(incoming_request_index(&entries, "terminate").is_some());
		assert!(incoming_request_index(&entries, "disconnect").is_some());
	})
	.await;
}

#[tokio::test]
async fn dap_skip_attach_waits_for_missing_initialized_without_sending_configuration_done() {
	let Some(python) = require_python() else { return };
	bounded("dap skip attach", 30, async move {
		let dir = TestDir::new("dap-skip");
		let log = dir.path().join("dap.log");
		let script = fixture("fake_dap.py");
		let mut adapter = dap_adapter(&python, &script, &log, "fake-skip");
		adapter.attach_defaults = json!({ "skipAttachRequest": true });
		let manager = DapSessionManager::new();
		let cancel = CancellationToken::new();

		let started = Instant::now();
		let summary = manager
			.attach(
				AttachOptions {
					adapter,
					cwd: dir.path().to_path_buf(),
					pid: Some(1234),
					port: None,
					host: None,
					env: HashMap::new(),
				},
				&cancel,
				Instant::now() + Duration::from_secs(2),
			)
			.await
			.expect("skip attach");
		let elapsed = started.elapsed();
		assert!(
			elapsed >= Duration::from_millis(1500) && elapsed < Duration::from_secs(6),
			"skip attach must wait the bounded operation deadline, took {elapsed:?}"
		);
		assert_eq!(summary.status, SessionStatus::Launching);

		let entries = read_log(&log);
		assert!(incoming_request_index(&entries, "initialize").is_some());
		assert!(incoming_request_index(&entries, "attach").is_none(), "skip attach must not send attach");
		assert!(
			incoming_request_index(&entries, "configurationDone").is_none(),
			"skip attach must not send configurationDone without initialized"
		);

		manager
			.terminate(&cancel, Instant::now() + Duration::from_secs(5))
			.await
			.expect("terminate skip session");
	})
	.await;
}

#[tokio::test]
async fn dap_process_exit_rejects_pending_request() {
	let Some(python) = require_python() else { return };
	bounded("dap process exit", 30, async move {
		let dir = TestDir::new("dap-exit");
		let log = dir.path().join("dap.log");
		let script = fixture("fake_dap.py");
		let adapter = dap_adapter(&python, &script, &log, "fake-dap");
		let manager = DapSessionManager::new();
		let cancel = CancellationToken::new();
		manager
			.launch(
				launch_options(adapter, dir.path(), "main.py"),
				&cancel,
				Instant::now() + Duration::from_secs(10),
			)
			.await
			.expect("launch fixture adapter");

		let error = manager
			.custom_request("die", None, &cancel, Instant::now() + Duration::from_secs(5))
			.await
			.expect_err("adapter exit must reject the pending request");
		let message = error.to_string().to_lowercase();
		assert!(
			message.contains("exited") || message.contains("closed") || message.contains("not running"),
			"unexpected exit error: {message}"
		);
		assert!(incoming_request_index(&read_log(&log), "die").is_some());
	})
	.await;
}

#[tokio::test]
async fn dap_fresh_manager_cannot_see_or_terminate_another_managers_session() {
	let Some(python) = require_python() else { return };
	bounded("dap ownership", 30, async move {
		let dir = TestDir::new("dap-owner");
		let log = dir.path().join("dap.log");
		let script = fixture("fake_dap.py");
		let adapter = dap_adapter(&python, &script, &log, "fake-dap");
		let parent = DapSessionManager::new();
		let child = DapSessionManager::new();
		let cancel = CancellationToken::new();
		parent
			.launch(
				launch_options(adapter, dir.path(), "main.py"),
				&cancel,
				Instant::now() + Duration::from_secs(10),
			)
			.await
			.expect("parent launch");
		assert_eq!(parent.list_sessions().len(), 1);

		assert!(child.list_sessions().is_empty(), "child manager must not see the parent debuggee");
		assert!(child.get_active_session().is_none());
		assert!(
			child
				.terminate(&cancel, Instant::now() + Duration::from_secs(5))
				.await
				.expect("child terminate")
				.is_none(),
			"child terminate must not target the parent debuggee"
		);
		let active = parent.get_active_session().expect("parent session survives");
		assert_eq!(active.status, SessionStatus::Stopped);

		parent
			.terminate(&cancel, Instant::now() + Duration::from_secs(5))
			.await
			.expect("parent terminate");
	})
	.await;
}
