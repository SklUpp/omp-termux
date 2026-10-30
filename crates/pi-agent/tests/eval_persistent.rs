//! Consumer-level eval regressions: real kernels exercised through the tool the
//! model sees. Covers persistence, env filtering, truncation, cancellation,
//! timeout classification, per-call disposal, dead-kernel recovery, local-root
//! mapping, bridge routing, backgrounding, and the JavaScript capture contract.
//!
//! Tests skip (with an eprintln) when the interpreter/runtime is absent; Julia
//! and Ruby semantics are external prerequisites. Julia readiness is the one
//! exception: its kernel handshake is exercised with a protocol wrapper.

use std::{
	collections::HashMap,
	path::PathBuf,
	sync::{Arc, Mutex},
	time::{Duration, Instant},
};

use async_trait::async_trait;
use serde_json::{Value, json};

use pi_agent::{
	eval::{
		ArtifactRef, AutoBackground, EvalAllowance, EvalHost, EvalLanguage, EvalRuntime, EvalSessionInfo,
		EvalSettings, EvalTool, Interpreters, KernelMode,
	},
	state::{AgentContext, ToolResult},
	tools::{Tool, ToolError},
};

#[derive(Default)]
struct TestHost {
	calls: Mutex<Vec<(String, Value)>>,
	deliveries: Mutex<Vec<(String, String)>>,
	agent_calls: Mutex<usize>,
	agent_delay: Mutex<Duration>,
	/// Simulated host-level publish refusal (Android `link(2)` EACCES and
	/// friends); `None` publishes one synthetic artifact.
	artifact_error: Mutex<Option<String>>,
	/// Simulated absent session artifacts directory.
	artifacts_disabled: Mutex<bool>,
}

impl TestHost {
	fn deny_artifacts(&self, error: Option<String>) {
		*self.artifact_error.lock().unwrap_or_else(|poison| poison.into_inner()) = error;
	}

	fn disable_artifacts(&self) {
		*self.artifacts_disabled.lock().unwrap_or_else(|poison| poison.into_inner()) = true;
	}
}

#[async_trait]
impl EvalHost for TestHost {
	async fn call_tool(&self, name: &str, args: Value, _ctx: &AgentContext) -> Result<ToolResult, ToolError> {
		self
			.calls
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
			.push((name.to_string(), args.clone()));
		let content = match name {
			"echo" => format!(
				"echo-ok:{}",
				args.get("path").and_then(Value::as_str).unwrap_or("missing")
			),
			"shot" => "shot-ok".to_string(),
			"bash" => "bash-ok".to_string(),
			_ => format!("{name}-ok"),
		};
		let details = (name == "shot")
			.then(|| json!({"images": [{"type": "image", "data": "aGk=", "mimeType": "image/png"}]}));
		Ok(ToolResult {
			tool_call_id: String::new(),
			content,
			is_error: false,
			terminate: false,
			details,
		})
	}

	async fn completion(&self, _args: Value, _ctx: &AgentContext) -> Result<Value, String> {
		Ok(json!({"text": "completion-ok"}))
	}

	async fn agent(&self, _args: Value, _ctx: &AgentContext) -> Result<Value, String> {
		*self.agent_calls.lock().unwrap_or_else(|poison| poison.into_inner()) += 1;
		let delay = *self.agent_delay.lock().unwrap_or_else(|poison| poison.into_inner());
		if !delay.is_zero() {
			tokio::time::sleep(delay).await;
		}
		Ok(json!({"text": "agent-ok"}))
	}

	async fn budget(&self, _args: Value, _ctx: &AgentContext) -> Result<Value, String> {
		Ok(json!({"total": null, "spent": 0, "hard": false}))
	}

	fn concurrency(&self, _args: &Value) -> Result<Value, String> {
		Ok(json!({"limit": 2}))
	}

	async fn write_artifact(&self, _kind: &str, _bytes: &[u8]) -> Result<Option<ArtifactRef>, String> {
		if *self
			.artifacts_disabled
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
		{
			return Ok(None);
		}
		let denied = self
			.artifact_error
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
			.clone();
		if let Some(error) = denied {
			return Err(error);
		}
		Ok(Some(ArtifactRef { id: "artifact-test".to_string(), path: None }))
	}

	fn enqueue_background_result(&self, job_id: &str, text: String) -> bool {
		self
			.deliveries
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
			.push((job_id.to_string(), text));
		true
	}
}

fn ambient_env() -> HashMap<String, String> {
	std::env::vars().collect()
}

fn binary(name: &str) -> Option<PathBuf> {
	pi_agent::eval::env::which(name, &ambient_env())
}

/// Absolute `sh` script: fixtures must not depend on the narrowed `PATH`.
#[cfg(unix)]
fn shell_script(body: &str) -> Option<String> {
	let shell = binary("sh")?;
	Some(format!("#!{}\n{body}", shell.display()))
}

/// Fake `julia` that records its child pid and hangs. Shell *and* sleep are
/// absolute: the narrowed test `PATH` hides them and Termux has no `/bin`.
#[cfg(unix)]
fn hang_script(pid_file: &std::path::Path) -> Option<String> {
	let sleep = binary("sleep")?;
	let body = format!(
		"'{}' 30 &\necho $! > '{}'\n'{}' 30",
		sleep.display(),
		pid_file.display(),
		sleep.display()
	);
	shell_script(&body)
}

fn python_path() -> Option<PathBuf> {
	binary("python3").or_else(|| binary("python"))
}

fn js_path() -> Option<PathBuf> {
	binary("bun")
}

fn ruby_path() -> Option<PathBuf> {
	binary("ruby")
}

fn julia_path() -> Option<PathBuf> {
	binary("julia")
}

fn py_only() -> EvalAllowance {
	EvalAllowance { python: true, js: false, ruby: false, julia: false }
}

fn one_language(language: EvalLanguage) -> EvalAllowance {
	let mut allowance = EvalAllowance { python: false, js: false, ruby: false, julia: false };
	allowance.set(language, true);
	allowance
}

fn test_session() -> EvalSessionInfo {
	EvalSessionInfo::new(format!("test-{}", pi_agent::eval::util::random_token()))
		.with_owner(Some("test-owner".to_string()))
}

async fn harness_with(
	allowance: EvalAllowance,
	interpreters: Interpreters,
	session: EvalSessionInfo,
	host: Arc<TestHost>,
	tune: impl FnOnce(EvalSettings) -> EvalSettings,
) -> Option<(Arc<EvalTool>, Arc<EvalRuntime>, AgentContext, PathBuf)> {
	let settings = tune(
		// Contract defaults: a 10 s probe budget for probed backends and a 30 s
		// cell budget. Julia readiness draws from the cell budget, so cold CI
		// boots are covered without a test-only override; the dedicated
		// short-timeout regressions set their own bounds.
		EvalSettings::new(allowance, interpreters).with_spill_bytes(4096),
	);
	let runtime = match EvalRuntime::start(settings).await {
		Ok(runtime) => Arc::new(runtime),
		Err(error) => {
			eprintln!("skipping eval regression: runtime start failed: {error}");
			return None;
		},
	};
	let tool = Arc::new(EvalTool::new(Arc::clone(&runtime), host, session));
	let cwd = std::env::temp_dir().join(format!("omp-eval-test-{}", pi_agent::eval::util::random_token()));
	std::fs::create_dir_all(&cwd).expect("temp cwd");
	let ctx = AgentContext::new(cwd.clone());
	Some((tool, runtime, ctx, cwd))
}

async fn py_harness(
	spill_bytes: usize,
) -> Option<(Arc<EvalTool>, Arc<EvalRuntime>, AgentContext, PathBuf, Arc<TestHost>)> {
	python_path()?;
	let host = Arc::new(TestHost::default());
	let (tool, runtime, ctx, cwd) =
		harness_with(py_only(), Interpreters::default(), test_session(), Arc::clone(&host), |settings| {
			settings.with_spill_bytes(spill_bytes)
		})
		.await?;
	Some((tool, runtime, ctx, cwd, host))
}

fn cell_details(result: &ToolResult) -> Value {
	result.details.clone().expect("eval details")
}

async fn teardown(runtime: &EvalRuntime, cwd: PathBuf) {
	runtime.dispose_owner(Some("test-owner")).await;
	runtime.shutdown().await;
	let _ = std::fs::remove_dir_all(cwd);
}

const TINY_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg==";

#[tokio::test]
async fn python_state_persists_across_calls_and_secrets_are_filtered() {
	let Some((tool, runtime, mut ctx, cwd, _host)) = py_harness(4096).await else { return };
	ctx.env.insert("PI_API_KEY".to_string(), "sk-test".to_string());
	ctx.env.insert("CUSTOM_TOKEN".to_string(), "tok".to_string());
	ctx.env.insert("PI_DEBUG".to_string(), "ok".to_string());

	let first = tool
		.execute(&json!({"language": "py", "code": "x = 40\nprint('hello')"}), &ctx)
		.await
		.expect("first cell");
	assert!(!first.is_error, "{}", first.content);
	assert!(first.content.contains("hello"), "{}", first.content);

	let second = tool
		.execute(&json!({"language": "py", "code": "x += 1\nprint(x)"}), &ctx)
		.await
		.expect("second cell");
	assert!(!second.is_error, "{}", second.content);
	assert!(second.content.contains("41"), "state persisted: {}", second.content);
	let details = cell_details(&second);
	assert_eq!(details["cells"][0]["status"], "complete");
	assert_eq!(details["cells"][0]["language"], "python");
	assert_eq!(details["language"], "python");
	assert_eq!(details["languages"][0], "python");

	let env_cell = tool
		.execute(
			&json!({
				"language": "py",
				"code": "import os\nprint(os.environ.get('PI_API_KEY'), os.environ.get('CUSTOM_TOKEN'), os.environ.get('PI_DEBUG'))"
			}),
			&ctx,
		)
		.await
		.expect("env cell");
	assert!(!env_cell.is_error, "{}", env_cell.content);
	assert!(env_cell.content.contains("None None ok"), "secrets leaked or debug missing: {}", env_cell.content);

	let after_reset = tool
		.execute(&json!({"language": "py", "code": "print('x' in globals())", "reset": true}), &ctx)
		.await
		.expect("reset cell");
	assert!(!after_reset.is_error, "{}", after_reset.content);
	assert!(after_reset.content.contains("False"), "reset did not wipe state: {}", after_reset.content);
	assert_eq!(runtime.retained_count(Some("test-owner")), 1, "session mode retains one kernel");

	teardown(&runtime, cwd).await;
}

#[tokio::test]
async fn output_truncation_emits_artifact_metadata() {
	let Some((tool, runtime, ctx, cwd, _host)) = py_harness(512).await else { return };
	let result = tool
		.execute(&json!({"language": "py", "code": "print('a' * 5000)"}), &ctx)
		.await
		.expect("big output cell");
	assert!(!result.is_error, "{}", result.content);
	assert!(result.content.contains("output truncated"), "{}", result.content);
	let details = cell_details(&result);
	assert_eq!(details["meta"]["truncated"], true);
	assert_eq!(details["meta"]["artifactId"], "artifact-test");
	teardown(&runtime, cwd).await;
}

/// A genuine filesystem `EACCES` for the artifacts directory, or `None` when
/// the process may still write into a mode-000 directory (root).
#[cfg(unix)]
fn artifact_denial(root: &std::path::Path) -> Option<String> {
	use std::os::unix::fs::PermissionsExt;

	let locked = root.join("artifacts");
	std::fs::create_dir_all(&locked).ok()?;
	std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).ok()?;
	let denied = std::fs::write(locked.join("1.eval.log"), b"x").err();
	let _ = std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700));
	denied
		.filter(|error| error.kind() == std::io::ErrorKind::PermissionDenied)
		.map(|error| error.to_string())
}

/// A host publish refusal (Android `link(2)` EACCES) must reach the consumer
/// as a tool error while the truncated cell text stays intact. `Ok(None)`
/// stays a non-error: no artifacts directory simply means no artifact.
#[cfg(unix)]
#[tokio::test]
async fn artifact_publish_denial_surfaces_as_tool_error() {
	let Some((tool, runtime, ctx, cwd, host)) = py_harness(512).await else { return };
	let Some(denial) = artifact_denial(&cwd) else {
		eprintln!("skipping artifact denial regression: filesystem does not deny mode-000 writes");
		teardown(&runtime, cwd).await;
		return;
	};
	host.deny_artifacts(Some(denial));
	let denied = tool
		.execute(&json!({"language": "py", "code": "print('a' * 5000)"}), &ctx)
		.await
		.expect("denied artifact cell");
	assert!(denied.is_error, "artifact denial stayed invisible: {}", denied.content);
	assert!(
		denied.content.contains("artifact unavailable"),
		"denial not visible to the model: {}",
		denied.content
	);
	assert!(
		denied.content.contains("output truncated"),
		"cell text lost: {}",
		denied.content
	);
	let details = cell_details(&denied);
	assert_eq!(details["isError"], true);
	assert_eq!(details["cells"][0]["status"], "error");
	assert_eq!(details["meta"]["truncated"], true);
	assert!(details["meta"].get("artifactId").is_none(), "{}", details["meta"]);
	let notice = details["notice"].as_str().unwrap_or_default();
	assert!(notice.contains("artifact unavailable"), "{notice}");
	assert!(notice.contains("Permission denied"), "{notice}");

	host.deny_artifacts(None);
	host.disable_artifacts();
	let disabled = tool
		.execute(&json!({"language": "py", "code": "print('a' * 5000)"}), &ctx)
		.await
		.expect("disabled artifact cell");
	assert!(!disabled.is_error, "disabled artifacts must not fail: {}", disabled.content);
	let details = cell_details(&disabled);
	assert!(details["meta"].get("artifactId").is_none(), "{}", details["meta"]);
	assert!(details.get("notice").is_none(), "{details}");
	teardown(&runtime, cwd).await;
}

#[tokio::test]
async fn cell_timeout_aborts_without_killing_the_kernel_or_replaying() {
	let Some((tool, runtime, ctx, cwd, _host)) = py_harness(4096).await else { return };
	let marker = cwd.join("timeout-side-effect.txt");
	let code = format!(
		"import time\nopen({:?}, 'a').write('x')\ntime.sleep(30)",
		marker.display().to_string()
	);
	let started = Instant::now();
	let result = tool
		.execute(&json!({"language": "py", "code": code, "timeout": 2}), &ctx)
		.await
		.expect("timeout cell returns a result");
	assert!(started.elapsed() < Duration::from_secs(10), "timeout escalated slowly");
	assert!(result.is_error, "timeout must be an error: {}", result.content);
	assert!(result.content.contains("Command aborted (timeout after 2s)"), "{}", result.content);
	let details = cell_details(&result);
	assert_eq!(details["cells"][0]["status"], "error");
	assert_eq!(
		std::fs::read_to_string(&marker).expect("side effect").len(),
		1,
		"a timed-out cell must not be replayed"
	);

	let alive = tool
		.execute(&json!({"language": "py", "code": "print('still-alive')"}), &ctx)
		.await
		.expect("post-timeout cell");
	assert!(!alive.is_error, "timeout must leave the kernel usable: {}", alive.content);
	assert!(alive.content.contains("still-alive"), "{}", alive.content);
	teardown(&runtime, cwd).await;
}

#[tokio::test]
async fn external_cancellation_stops_the_cell_without_replay() {
	let Some((tool, runtime, ctx, cwd, _host)) = py_harness(4096).await else { return };
	let marker = cwd.join("cancel-side-effect.txt");
	let code = format!(
		"import time\nopen({:?}, 'a').write('x')\ntime.sleep(30)",
		marker.display().to_string()
	);
	let cancellation = ctx.cancellation_token.clone();
	let runner_tool = Arc::clone(&tool);
	let runner_ctx = ctx.clone();
	let task = tokio::spawn(async move {
		runner_tool
			.execute(&json!({"language": "py", "code": code, "timeout": 60}), &runner_ctx)
			.await
	});
	let deadline = Instant::now() + Duration::from_secs(15);
	while std::fs::metadata(&marker).map_or(0, |meta| meta.len()) != 1 {
		assert!(Instant::now() < deadline, "cell never wrote its side effect");
		tokio::time::sleep(Duration::from_millis(20)).await;
	}
	cancellation.cancel();
	let started = Instant::now();
	let result = tokio::time::timeout(Duration::from_secs(8), task)
		.await
		.expect("cancellation must settle promptly")
		.expect("join");
	assert!(started.elapsed() < Duration::from_secs(8));
	match result {
		Ok(result) => assert!(result.is_error, "cancelled cell must be an error: {}", result.content),
		Err(ToolError::Aborted(_)) => {},
		Err(other) => panic!("unexpected cancellation error: {other}"),
	}
	assert_eq!(
		std::fs::read_to_string(&marker).expect("side effect").len(),
		1,
		"a cancelled cell must not be replayed"
	);
	teardown(&runtime, cwd).await;
}

#[tokio::test]
async fn per_call_mode_shuts_each_kernel_down() {
	let Some((tool, runtime, ctx, cwd)) = harness_with(
		py_only(),
		Interpreters::default(),
		test_session(),
		Arc::new(TestHost::default()),
		|settings| settings.with_kernel_mode(KernelMode::PerCall),
	)
	.await
	else {
		return;
	};
	let first = tool
		.execute(&json!({"language": "py", "code": "x = 41"}), &ctx)
		.await
		.expect("first call");
	assert!(!first.is_error, "{}", first.content);
	assert_eq!(runtime.retained_count(Some("test-owner")), 0, "per-call must not retain the kernel");
	let second = tool
		.execute(&json!({"language": "py", "code": "print(globals().get('x'))"}), &ctx)
		.await
		.expect("second call");
	assert!(!second.is_error, "{}", second.content);
	assert!(second.content.contains("None"), "per-call state leaked: {}", second.content);
	assert_eq!(runtime.retained_count(Some("test-owner")), 0);
	teardown(&runtime, cwd).await;
}

#[tokio::test]
async fn concurrent_per_call_cells_do_not_reset_each_other() {
	let Some((tool, runtime, ctx, cwd)) = harness_with(
		py_only(),
		Interpreters::default(),
		test_session(),
		Arc::new(TestHost::default()),
		|settings| settings.with_kernel_mode(KernelMode::PerCall),
	)
	.await
	else {
		return;
	};
	let left_args = json!({"language": "py", "code": "import time\ntime.sleep(0.4)\nprint('cell-a')"});
	let right_args = json!({"language": "py", "code": "import time\ntime.sleep(0.2)\nprint('cell-b')"});
	let left = tool.execute(&left_args, &ctx);
	let right = tool.execute(&right_args, &ctx);
	let (left, right) = tokio::join!(left, right);
	let left = left.expect("left per-call cell");
	let right = right.expect("right per-call cell");
	assert!(!left.is_error, "{}", left.content);
	assert!(!right.is_error, "{}", right.content);
	assert!(left.content.contains("cell-a"), "{}", left.content);
	assert!(right.content.contains("cell-b"), "{}", right.content);
	assert_eq!(runtime.retained_count(Some("test-owner")), 0);
	teardown(&runtime, cwd).await;
}

#[tokio::test]
async fn dead_kernel_is_replaced_once_and_the_cell_recovers() {
	let Some((tool, runtime, ctx, cwd, _host)) = py_harness(4096).await else { return };
	let marker = cwd.join("died-once.txt");
	let code = format!(
		"import os, pathlib\nm = pathlib.Path({:?})\nif m.exists():\n    print('recovered')\nelse:\n    m.write_text('x')\n    os._exit(7)",
		marker.display().to_string()
	);
	let result = tool
		.execute(&json!({"language": "py", "code": code}), &ctx)
		.await
		.expect("dead kernel recovery");
	assert!(!result.is_error, "retry must recover the cell: {}", result.content);
	assert!(result.content.contains("recovered"), "{}", result.content);
	assert!(marker.exists(), "first attempt ran exactly once");
	teardown(&runtime, cwd).await;
}

#[cfg(unix)]
#[tokio::test]
async fn probe_timeout_is_a_prerequisite_error_not_a_cell_abort() {
	use std::os::unix::fs::PermissionsExt as _;

	let Some(python) = python_path() else { return };
	let Some((old_tool, old_runtime, ctx, cwd, _host)) = py_harness(4096).await else { return };
	drop(old_tool);
	old_runtime.shutdown().await;
	let wrapper = cwd.join("slow-probe-python");
	std::fs::write(&wrapper, format!("#!/bin/sh\nsleep 2\nexec '{}' \"$@\"\n", python.display()))
		.expect("write wrapper");
	std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).expect("chmod");

	let interpreters = Interpreters::new(Some(wrapper.display().to_string()), None, None);
	let settings = EvalSettings::new(py_only(), interpreters).with_probe_timeout_ms(5_000);
	// The wrapper is an explicit interpreter, so build a dedicated runtime: the
	// shared runtime above already cached the real interpreter for this cwd.
	let runtime = Arc::new(EvalRuntime::start(settings).await.expect("runtime"));
	let tool = Arc::new(EvalTool::new(
		Arc::clone(&runtime),
		Arc::new(TestHost::default()),
		test_session(),
	));
	let started = Instant::now();
	let error = tool
		.execute(&json!({"language": "py", "code": "print(1)", "timeout": 1}), &ctx)
		.await
		.expect_err("probe must time out");
	assert!(started.elapsed() < Duration::from_secs(5), "probe bound was not enforced");
	let message = error.to_string();
	assert!(message.contains("probe timed out"), "{message}");
	assert!(!message.contains("availability probe was cancelled"), "{message}");
	drop(tool);
	teardown(&runtime, cwd).await;
}

#[cfg(unix)]
#[tokio::test]
async fn probe_and_startup_time_are_not_charged_to_the_cell_budget() {
	use std::os::unix::fs::PermissionsExt as _;

	let Some(python) = python_path() else { return };
	let cwd = std::env::temp_dir().join(format!("omp-eval-test-{}", pi_agent::eval::util::random_token()));
	std::fs::create_dir_all(&cwd).expect("temp cwd");
	let wrapper = cwd.join("slow-start-python");
	std::fs::write(&wrapper, format!("#!/bin/sh\nsleep 0.5\nexec '{}' \"$@\"\n", python.display()))
		.expect("write wrapper");
	std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).expect("chmod");

	let interpreters = Interpreters::new(Some(wrapper.display().to_string()), None, None);
	let Some((tool, runtime, ctx, cwd)) = harness_with(
		py_only(),
		interpreters,
		test_session(),
		Arc::new(TestHost::default()),
		|settings| settings,
	)
	.await
	else {
		return;
	};
	let result = tool
		.execute(
			&json!({"language": "py", "code": "import time\ntime.sleep(0.3)\nprint('budget-ok')", "timeout": 1}),
			&ctx,
		)
		.await
		.expect("slow startup cell");
	assert!(!result.is_error, "probe/startup consumed the cell budget: {}", result.content);
	assert!(result.content.contains("budget-ok"), "{}", result.content);
	teardown(&runtime, cwd).await;
}

#[tokio::test]
async fn scheme_map_local_roots_are_visible_to_subprocess_helpers() {
	let Some((_tool, runtime, _ctx, cwd, _host)) = py_harness(4096).await else { return };
	let local_root = cwd.join("local-root");
	std::fs::create_dir_all(&local_root).expect("local root");
	let session = test_session().with_local_root("local", local_root.clone());
	let tool = Arc::new(EvalTool::new(
		Arc::clone(&runtime),
		Arc::new(TestHost::default()),
		session,
	));
	let ctx = AgentContext::new(cwd.clone());
	let result = tool
		.execute(
			&json!({
				"language": "py",
				"code": "write('local://note.txt', 'scheme-ok')\nprint(read('local://note.txt'))"
			}),
			&ctx,
		)
		.await
		.expect("local scheme cell");
	assert!(!result.is_error, "{}", result.content);
	assert!(result.content.contains("scheme-ok"), "{}", result.content);
	assert_eq!(
		std::fs::read_to_string(local_root.join("note.txt")).expect("local write"),
		"scheme-ok"
	);
	teardown(&runtime, cwd).await;
}

#[tokio::test]
async fn bridged_helpers_route_through_the_host_and_reject_direct_only_tools() {
	let Some((tool, runtime, ctx, cwd, host)) = py_harness(4096).await else { return };
	let result = tool
		.execute(
			&json!({
				"language": "py",
				"code": "print(tool.echo({'path': 'abc'}))\nprint(completion('hi'))\nprint(agent('do it'))\nprint(parallel([lambda: 1, lambda: 2]))\nprint(budget.total)"
			}),
			&ctx,
		)
		.await
		.expect("bridge cell");
	assert!(!result.is_error, "{}", result.content);
	assert!(result.content.contains("echo-ok:abc"), "{}", result.content);
	assert!(result.content.contains("completion-ok"), "{}", result.content);
	assert!(result.content.contains("agent-ok"), "{}", result.content);
	assert!(result.content.contains("[1, 2]"), "{}", result.content);
	let details = cell_details(&result);
	let events = details["statusEvents"].as_array().expect("status events");
	assert!(events.iter().any(|event| event["op"] == "echo"), "{events:?}");

	let bash = tool
		.execute(&json!({"language": "py", "code": "print(tool.bash({'command': 'echo hi'}))"}), &ctx)
		.await
		.expect("bash bridge cell");
	assert!(bash.content.contains("bash-ok"), "{}", bash.content);
	let events = cell_details(&bash)["statusEvents"].as_array().cloned().expect("events");
	let run = events.iter().find(|event| event["op"] == "run").expect("bash maps to run");
	assert_eq!(run["cmd"], "echo hi");

	let rejected = tool
		.execute(&json!({"language": "py", "code": "tool.checkpoint({})"}), &ctx)
		.await
		.expect("checkpoint rejection returns a failed cell");
	assert!(rejected.is_error, "{}", rejected.content);
	assert!(rejected.content.contains("cannot run through the eval bridge"), "{}", rejected.content);
	assert!(
		!host
			.calls
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
			.iter()
			.any(|(name, _)| name == "checkpoint"),
		"the direct-only tool must never reach the host"
	);
	teardown(&runtime, cwd).await;
}

#[tokio::test]
async fn bridged_images_surface_in_details_and_display_notes() {
	let Some((tool, runtime, ctx, cwd, _host)) = py_harness(4096).await else { return };
	let result = tool
		.execute(
			&json!({
				"language": "py",
				"code": format!(
					"class _Image:\n    def _repr_png_(self):\n        return '{TINY_PNG}'\nprint(tool.shot({{}}))\ndisplay(_Image())"
				)
			}),
			&ctx,
		)
		.await
		.expect("image cell");
	assert!(!result.is_error, "{}", result.content);
	assert!(result.content.contains("shot-ok"), "{}", result.content);
	assert!(result.content.contains("image/png"), "bridged image value missing: {}", result.content);
	let details = cell_details(&result);
	let images = details["images"].as_array().expect("details.images");
	assert_eq!(images[0]["mimeType"], "image/png");
	assert!(result.content.contains("display image 1"), "{}", result.content);
	teardown(&runtime, cwd).await;
}

#[tokio::test]
async fn agent_bridge_call_pauses_the_cell_watchdog() {
	let Some(python) = python_path() else { return };
	let host = Arc::new(TestHost {
		agent_delay: Mutex::new(Duration::from_millis(1_500)),
		..TestHost::default()
	});
	let Some((tool, runtime, ctx, cwd)) = harness_with(
		py_only(),
		Interpreters::default(),
		test_session(),
		Arc::clone(&host),
		|settings| settings,
	)
	.await
	else {
		return;
	};
	let _ = python;
	let started = Instant::now();
	let result = tool
		.execute(&json!({"language": "py", "code": "print(agent('slow'))", "timeout": 1}), &ctx)
		.await
		.expect("paused agent cell");
	assert!(started.elapsed() > Duration::from_millis(1_200));
	assert!(!result.is_error, "host-side agent time must not consume the cell budget: {}", result.content);
	assert!(result.content.contains("agent-ok"), "{}", result.content);
	teardown(&runtime, cwd).await;
}

#[tokio::test]
async fn external_abort_during_agent_is_deferred_and_not_replayed() {
	let Some((tool, runtime, ctx, cwd, host)) = py_harness(4096).await else { return };
	*host.agent_delay.lock().unwrap_or_else(|poison| poison.into_inner()) = Duration::from_millis(600);
	let cancellation = ctx.cancellation_token.clone();
	let runner_tool = Arc::clone(&tool);
	let runner_ctx = ctx.clone();
	let task = tokio::spawn(async move {
		runner_tool
			.execute(&json!({"language": "py", "code": "print(agent('cancel-me'))", "timeout": 30}), &runner_ctx)
			.await
	});
	let deadline = Instant::now() + Duration::from_secs(15);
	loop {
		let calls = *host
			.agent_calls
			.lock()
			.unwrap_or_else(|poison| poison.into_inner());
		if calls > 0 {
			break;
		}
		assert!(Instant::now() < deadline, "agent bridge call never started");
		tokio::time::sleep(Duration::from_millis(20)).await;
	}
	cancellation.cancel();
	let result = tokio::time::timeout(Duration::from_secs(10), task)
		.await
		.expect("shielded agent must settle")
		.expect("join");
	let agent_calls = *host
		.agent_calls
		.lock()
		.unwrap_or_else(|poison| poison.into_inner());
	assert_eq!(agent_calls, 1, "cancellation must not replay the agent call");
	match result {
		Ok(result) => assert!(result.is_error, "aborted cell must be an error: {}", result.content),
		Err(ToolError::Aborted(_)) => {},
		Err(other) => panic!("unexpected cancellation error: {other}"),
	}
	teardown(&runtime, cwd).await;
}

#[tokio::test]
async fn auto_background_returns_a_running_cell_and_delivers_the_result() {
	let Some(python) = python_path() else { return };
	let host = Arc::new(TestHost::default());
	let Some((tool, runtime, ctx, cwd)) = harness_with(
		py_only(),
		Interpreters::default(),
		test_session(),
		Arc::clone(&host),
		|settings| settings.with_auto_background(Some(AutoBackground { threshold_ms: 50 })),
	)
	.await
	else {
		return;
	};
	let _ = python;
	let started = Instant::now();
	let result = tool
		.execute(
			&json!({"language": "py", "code": "import time\ntime.sleep(1.2)\nprint('bg-done')", "timeout": 30}),
			&ctx,
		)
		.await
		.expect("background start");
	assert!(started.elapsed() < Duration::from_secs(1), "foreground wait ignored the threshold");
	assert!(!result.is_error, "{}", result.content);
	assert!(result.content.contains("Backgrounded as job"), "{}", result.content);
	let details = cell_details(&result);
	assert_eq!(details["async"]["state"], "running");
	assert_eq!(details["async"]["type"], "eval");
	assert_eq!(details["cells"][0]["status"], "running");
	assert_eq!(details["cells"][0]["language"], "python");
	assert!(details["async"]["jobId"].as_str().is_some());

	let mut delivered = None;
	for _ in 0..200 {
		if let Some((_, text)) = host
			.deliveries
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
			.first()
			.cloned()
		{
			delivered = Some(text);
			break;
		}
		tokio::time::sleep(Duration::from_millis(50)).await;
	}
	let delivered = delivered.expect("background result delivery");
	assert!(delivered.contains("bg-done"), "{}", delivered);
	teardown(&runtime, cwd).await;
}

#[tokio::test]
async fn cancelled_background_job_delivers_its_error() {
	let Some(python) = python_path() else { return };
	let host = Arc::new(TestHost::default());
	let Some((tool, runtime, ctx, cwd)) = harness_with(
		py_only(),
		Interpreters::default(),
		test_session(),
		Arc::clone(&host),
		|settings| settings.with_auto_background(Some(AutoBackground { threshold_ms: 50 })),
	)
	.await
	else {
		return;
	};
	let _ = python;
	let result = tool
		.execute(
			&json!({"language": "py", "code": "import time\ntime.sleep(30)", "timeout": 60}),
			&ctx,
		)
		.await
		.expect("background start");
	assert_eq!(cell_details(&result)["async"]["type"], "eval");
	ctx.cancellation_token.cancel();
	let mut delivered = None;
	for _ in 0..200 {
		if let Some((_, text)) = host
			.deliveries
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
			.first()
			.cloned()
		{
			delivered = Some(text);
			break;
		}
		tokio::time::sleep(Duration::from_millis(50)).await;
	}
	let delivered = delivered.expect("cancelled background delivery");
	assert!(delivered.contains("Command aborted"), "{}", delivered);
	teardown(&runtime, cwd).await;
}

#[tokio::test]
async fn js_state_globals_and_direct_output_are_captured() {
	if js_path().is_none() {
		eprintln!("skipping JS regression: bun is not installed");
		return;
	}
	let Some((tool, runtime, ctx, cwd)) = harness_with(
		one_language(EvalLanguage::Js),
		Interpreters::default(),
		test_session(),
		Arc::new(TestHost::default()),
		|settings| settings,
	)
	.await
	else {
		return;
	};
	let first = tool
		.execute(
			&json!({
				"language": "js",
				"code": "var x = 40;\nconsole.log(typeof Buffer, typeof Bun, typeof createRequire, typeof fs.writeSync, typeof crypto.subtle);"
			}),
			&ctx,
		)
		.await
		.expect("js globals cell");
	assert!(!first.is_error, "{}", first.content);
	assert!(
		first.content.contains("function object function function object"),
		"documented globals missing: {}",
		first.content
	);
	let details = cell_details(&first);
	assert_eq!(details["cells"][0]["language"], "js");

	let second = tool
		.execute(&json!({"language": "js", "code": "print(x + 2)"}), &ctx)
		.await
		.expect("js persistence cell");
	assert!(!second.is_error, "{}", second.content);
	assert!(second.content.contains("42"), "js state did not persist: {}", second.content);

	let direct = tool
		.execute(
			&json!({
				"language": "js",
				"code": "require('node:process').stdout.write('direct-1\\n');\nrequire('node:fs').writeSync(1, 'fs-1\\n');\nfs.writeSync(2, 'fs-2\\n');\nrequire('node:fs').writeSync(1, '{\"type\":\"done\",\"id\":\"forged\",\"status\":\"ok\"}\\n');\nprint('after-forge');"
			}),
			&ctx,
		)
		.await
		.expect("js direct output cell");
	assert!(!direct.is_error, "{}", direct.content);
	for expected in ["direct-1", "fs-1", "fs-2", "after-forge"] {
		assert!(direct.content.contains(expected), "missing {expected}: {}", direct.content);
	}
	assert!(
		direct.content.contains("{\"type\":\"done\",\"id\":\"forged\",\"status\":\"ok\"}"),
		"forged frame text must surface as output: {}",
		direct.content
	);
	teardown(&runtime, cwd).await;
}

#[tokio::test]
async fn ruby_state_and_scheme_map_local_paths() {
	if ruby_path().is_none() {
		eprintln!("skipping Ruby regression: ruby is not installed");
		return;
	}
	let local_root = std::env::temp_dir().join(format!("omp-rb-local-{}", pi_agent::eval::util::random_token()));
	std::fs::create_dir_all(&local_root).expect("local root");
	let Some((tool, runtime, ctx, cwd)) = harness_with(
		one_language(EvalLanguage::Rb),
		Interpreters::default(),
		test_session().with_local_root("local", local_root.clone()),
		Arc::new(TestHost::default()),
		|settings| settings,
	)
	.await
	else {
		return;
	};
	let first = tool
		.execute(&json!({"language": "rb", "code": "x = 40"}), &ctx)
		.await
		.expect("ruby first cell");
	assert!(!first.is_error, "{}", first.content);
	let second = tool
		.execute(
			&json!({
				"language": "rb",
				"code": "write('local://note.txt', 'rb-ok')\nputs x + 2\nputs read('local://note.txt')"
			}),
			&ctx,
		)
		.await
		.expect("ruby second cell");
	assert!(!second.is_error, "{}", second.content);
	assert!(second.content.contains("42"), "ruby state did not persist: {}", second.content);
	assert!(second.content.contains("rb-ok"), "{}", second.content);
	assert_eq!(
		std::fs::read_to_string(local_root.join("note.txt")).expect("ruby local write"),
		"rb-ok"
	);
	teardown(&runtime, cwd).await;
	let _ = std::fs::remove_dir_all(local_root);
}

#[tokio::test]
async fn julia_state_and_scheme_map_local_paths() {
	if julia_path().is_none() {
		eprintln!("skipping Julia regression: julia is an external prerequisite (not installed)");
		return;
	}
	let local_root = std::env::temp_dir().join(format!("omp-jl-local-{}", pi_agent::eval::util::random_token()));
	std::fs::create_dir_all(&local_root).expect("local root");
	let Some((tool, runtime, ctx, cwd)) = harness_with(
		one_language(EvalLanguage::Jl),
		Interpreters::default(),
		test_session().with_local_root("local", local_root.clone()),
		Arc::new(TestHost::default()),
		|settings| settings,
	)
	.await
	else {
		return;
	};
	let first = tool
		.execute(&json!({"language": "jl", "code": "x = 40"}), &ctx)
		.await
		.expect("julia first cell");
	assert!(!first.is_error, "{}", first.content);
	let second = tool
		.execute(
			&json!({
				"language": "jl",
				"code": "write(\"local://note.txt\", \"jl-ok\")\nprintln(x + 2)\nprintln(read(\"local://note.txt\"))"
			}),
			&ctx,
		)
		.await
		.expect("julia second cell");
	assert!(!second.is_error, "{}", second.content);
	assert!(second.content.contains("42"), "julia state did not persist: {}", second.content);
	assert_eq!(
		std::fs::read_to_string(local_root.join("note.txt")).expect("julia local write"),
		"jl-ok"
	);
	teardown(&runtime, cwd).await;
	let _ = std::fs::remove_dir_all(local_root);
}

/// Julia readiness is a real kernel handshake: no throwaway probe boot.
///
/// A protocol wrapper stands in for the interpreter (no Julia needed) so the
/// observable contract is testable anywhere: the first cell boots exactly one
/// interpreter, the second cell reuses it, and a broken candidate earlier on
/// `PATH` is skipped. Real Julia semantics stay covered by
/// `julia_state_and_scheme_map_local_paths`.
#[cfg(unix)]
#[tokio::test]
async fn julia_readiness_boots_one_interpreter_and_reuses_it() {
	use std::os::unix::fs::PermissionsExt as _;

	let Some(python) = python_path() else { return };
	let Some((tool, runtime, mut ctx, cwd)) = harness_with(
		one_language(EvalLanguage::Jl),
		Interpreters::default(),
		test_session(),
		Arc::new(TestHost::default()),
		|settings| settings,
	)
	.await
	else {
		return;
	};
	let write_script = |path: &std::path::Path, body: &str| {
		std::fs::write(path, body).expect("write script");
		std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
	};
	let Some(broken_script) = shell_script("exit 1") else { return };
	let broken = cwd.join("broken");
	let working = cwd.join("working");
	std::fs::create_dir_all(&broken).expect("broken dir");
	std::fs::create_dir_all(&working).expect("working dir");
	write_script(&broken.join("julia"), &broken_script);
	let log = cwd.join("julia-starts.log");
	let wrapper = format!(
		r#"#!{python}
import json, os, sys
with open({log:?}, "a") as handle:
    handle.write(str(os.getpid()) + "\n")
count = 0
for raw in sys.stdin:
    line = raw.rstrip("\n")
    if not line:
        continue
    if line == "exit":
        break
    parts = line.split("\t")
    if parts[0] != "run":
        continue
    rid = parts[1]
    count += 1
    sys.stdout.write(json.dumps({{"type": "stdout", "id": rid, "data": "pid:%d cell:%d\n" % (os.getpid(), count)}}) + "\n")
    sys.stdout.write(json.dumps({{"type": "done", "id": rid, "status": "ok", "execution_count": count}}) + "\n")
    sys.stdout.flush()
"#,
		python = python.display(),
		log = log.display().to_string(),
	);
	write_script(&working.join("julia"), &wrapper);
	ctx.env.insert(
		"PATH".to_string(),
		format!(
			"{}:{}:{}",
			broken.display(),
			working.display(),
			std::env::var("PATH").unwrap_or_default()
		),
	);

	let first = tool
		.execute(&json!({"language": "jl", "code": "x = 1"}), &ctx)
		.await
		.expect("first julia cell");
	assert!(!first.is_error, "{}", first.content);
	let second = tool
		.execute(&json!({"language": "jl", "code": "x + 1"}), &ctx)
		.await
		.expect("second julia cell");
	assert!(!second.is_error, "{}", second.content);

	let starts = std::fs::read_to_string(&log).expect("startup log");
	let pids: Vec<&str> = starts.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
	assert_eq!(pids.len(), 1, "exactly one interpreter startup: {starts}");
	let pid = format!("pid:{}", pids[0]);
	assert!(first.content.contains(&pid), "{}", first.content);
	assert!(second.content.contains(&pid), "interpreter was not reused: {}", second.content);

	// Reset boots exactly one replacement, bounded by the same cell budget.
	let third = tool
		.execute(&json!({"language": "jl", "code": "x = 1", "reset": true}), &ctx)
		.await
		.expect("reset julia cell");
	assert!(!third.is_error, "{}", third.content);
	let starts = std::fs::read_to_string(&log).expect("startup log");
	let pids: Vec<&str> = starts.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
	assert_eq!(pids.len(), 2, "reset starts one replacement: {starts}");
	let reset_pid = format!("pid:{}", pids[1]);
	assert!(third.content.contains(&reset_pid), "reset kernel served the cell: {}", third.content);
	assert_ne!(reset_pid, pid);
	teardown(&runtime, cwd).await;
}

/// Julia readiness spends the cell budget, so the cell body gets the remainder
/// instead of a fresh full timeout.
#[cfg(unix)]
#[tokio::test]
async fn julia_cell_body_stays_inside_the_readiness_budget() {
	let Some(python) = python_path() else { return };
	let Some((tool, runtime, mut ctx, cwd)) = harness_with(
		one_language(EvalLanguage::Jl),
		Interpreters::default(),
		test_session(),
		Arc::new(TestHost::default()),
		|settings| settings,
	)
	.await
	else {
		return;
	};
	let working = cwd.join("working");
	std::fs::create_dir_all(&working).expect("working dir");
	let wrapper = format!(
		r#"#!{python}
import json, sys, time
time.sleep(0.6)
count = 0
for raw in sys.stdin:
    line = raw.rstrip("\n")
    if not line:
        continue
    if line == "exit":
        break
    parts = line.split("\t")
    if parts[0] != "run":
        continue
    rid = parts[1]
    count += 1
    if count >= 3:
        time.sleep(1.8)
    sys.stdout.write(json.dumps({{"type": "done", "id": rid, "status": "ok", "execution_count": count}}) + "\n")
    sys.stdout.flush()
"#,
		python = python.display(),
	);
	let executable = working.join("julia");
	std::fs::write(&executable, wrapper).expect("write wrapper");
	{
		use std::os::unix::fs::PermissionsExt as _;
		std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).expect("chmod");
	}
	ctx.env.insert(
		"PATH".to_string(),
		format!("{}:{}", working.display(), std::env::var("PATH").unwrap_or_default()),
	);

	let started = Instant::now();
	let result = tool
		.execute(&json!({"language": "jl", "code": "1", "timeout": 2}), &ctx)
		.await
		.expect("julia cell");
	assert!(result.is_error, "the body must not get a fresh cell timeout: {}", result.content);
	assert!(result.content.contains("timeout after 2s"), "{}", result.content);
	assert!(started.elapsed() < Duration::from_secs(8), "cell budget was not enforced");
	teardown(&runtime, cwd).await;
}

/// A wrapper that exits cleanly is not a ready kernel: candidate failures are
/// reported in `PATH` order, with the reason, and never become a false ready.
#[cfg(unix)]
#[tokio::test]
async fn julia_candidate_failures_are_ordered_and_never_false_ready() {
	use std::os::unix::fs::PermissionsExt as _;

	let Some((tool, runtime, mut ctx, cwd)) = harness_with(
		one_language(EvalLanguage::Jl),
		Interpreters::default(),
		test_session(),
		Arc::new(TestHost::default()),
		|settings| settings.with_probe_timeout_ms(5_000),
	)
	.await
	else {
		return;
	};
	let write_script = |path: &std::path::Path, body: &str| {
		std::fs::write(path, body).expect("write script");
		std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
	};
	let Some(exiting_script) = shell_script("exit 1") else { return };
	// Speaks no protocol but exits 0: a working process, not a ready kernel.
	let Some(mute_script) = shell_script("echo hello\nexit 0") else { return };
	let first = cwd.join("first");
	let second = cwd.join("second");
	std::fs::create_dir_all(&first).expect("first dir");
	std::fs::create_dir_all(&second).expect("second dir");
	write_script(&first.join("julia"), &exiting_script);
	write_script(&second.join("julia"), &mute_script);
	ctx.env
		.insert("PATH".to_string(), format!("{}:{}", first.display(), second.display()));

	let error = tool
		.execute(&json!({"language": "jl", "code": "1"}), &ctx)
		.await
		.expect_err("no working julia");
	let message = error.to_string();
	assert!(message.contains("No working Julia interpreter found"), "{message}");
	let first_at = message
		.find(&first.join("julia").display().to_string())
		.expect("first candidate label");
	let second_at = message
		.find(&second.join("julia").display().to_string())
		.expect("second candidate label");
	assert!(first_at < second_at, "candidate order is preserved: {message}");
	assert_eq!(message.matches("kernel exited unexpectedly").count(), 2, "{message}");

	// Explicit interpreters are path-validated before any spawn is attempted.
	let settings = EvalSettings::new(
		one_language(EvalLanguage::Jl),
		Interpreters::new(None, None, Some(cwd.join("absent-julia").display().to_string())),
	);
	let explicit = Arc::new(EvalRuntime::start(settings).await.expect("runtime"));
	let explicit_tool = Arc::new(EvalTool::new(
		Arc::clone(&explicit),
		Arc::new(TestHost::default()),
		test_session(),
	));
	let error = explicit_tool
		.execute(&json!({"language": "jl", "code": "1"}), &ctx)
		.await
		.expect_err("absent explicit interpreter");
	assert!(error.to_string().contains("not executable"), "{error}");
	drop(explicit_tool);
	explicit.shutdown().await;
	teardown(&runtime, cwd).await;
}

/// Deadline expiry during readiness is bounded and sweeps the whole candidate
/// process group (the wrapper's own `sleep` child included).
#[cfg(target_os = "linux")]
#[tokio::test]
async fn julia_readiness_deadline_kills_the_candidate_group() {
	let Some((tool, runtime, mut ctx, cwd)) = harness_with(
		one_language(EvalLanguage::Jl),
		Interpreters::default(),
		test_session(),
		Arc::new(TestHost::default()),
		|settings| settings,
	)
	.await
	else {
		return;
	};
	let hang = cwd.join("hang");
	std::fs::create_dir_all(&hang).expect("hang dir");
	let grandchild = cwd.join("grandchild.pid");
	let Some(script) = hang_script(&grandchild) else { return };
	let executable = hang.join("julia");
	std::fs::write(&executable, script).expect("hang script");
	{
		use std::os::unix::fs::PermissionsExt as _;
		std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).expect("chmod");
	}
	ctx.env.insert("PATH".to_string(), hang.display().to_string());

	let started = Instant::now();
	let error = tool
		.execute(&json!({"language": "jl", "code": "1", "timeout": 2}), &ctx)
		.await
		.expect_err("readiness must time out");
	assert!(started.elapsed() < Duration::from_secs(6), "cell budget was not enforced");
	assert!(error.to_string().contains("readiness timed out"), "{error}");
	let pid: u32 = std::fs::read_to_string(&grandchild)
		.expect("wrapper child pid")
		.trim()
		.parse()
		.expect("numeric pid");
	let deadline = Instant::now() + Duration::from_secs(3);
	while process_alive(pid) && Instant::now() < deadline {
		tokio::time::sleep(Duration::from_millis(50)).await;
	}
	assert!(!process_alive(pid), "failed candidate process group survived the deadline");
	teardown(&runtime, cwd).await;
}

/// Cancelling during readiness aborts the cell and sweeps the candidate group.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn julia_readiness_cancel_kills_the_candidate_group() {
	let Some((tool, runtime, mut ctx, cwd)) = harness_with(
		one_language(EvalLanguage::Jl),
		Interpreters::default(),
		test_session(),
		Arc::new(TestHost::default()),
		|settings| settings,
	)
	.await
	else {
		return;
	};
	let hang = cwd.join("hang");
	std::fs::create_dir_all(&hang).expect("hang dir");
	let grandchild = cwd.join("grandchild.pid");
	let Some(script) = hang_script(&grandchild) else { return };
	let executable = hang.join("julia");
	std::fs::write(&executable, script).expect("hang script");
	{
		use std::os::unix::fs::PermissionsExt as _;
		std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).expect("chmod");
	}
	ctx.env.insert("PATH".to_string(), hang.display().to_string());

	let token = ctx.cancellation_token.clone();
	tokio::spawn(async move {
		tokio::time::sleep(Duration::from_millis(250)).await;
		token.cancel();
	});
	let result = tool
		.execute(&json!({"language": "jl", "code": "1"}), &ctx)
		.await
		.expect("cancelled readiness is an abort result, not an error");
	assert!(result.is_error, "aborted cell is reported as an error: {}", result.content);
	let pid: u32 = std::fs::read_to_string(&grandchild)
		.expect("wrapper child pid")
		.trim()
		.parse()
		.expect("numeric pid");
	let deadline = Instant::now() + Duration::from_secs(3);
	while process_alive(pid) && Instant::now() < deadline {
		tokio::time::sleep(Duration::from_millis(50)).await;
	}
	assert!(!process_alive(pid), "cancelled candidate process group survived");
	teardown(&runtime, cwd).await;
}

/// A process is still running while `/proc` reports a non-zombie state.
#[cfg(target_os = "linux")]
fn process_alive(pid: u32) -> bool {
	let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
		return false;
	};
	stat.rsplit(')').next().and_then(|rest| rest.trim_start().chars().next()) != Some('Z')
}
