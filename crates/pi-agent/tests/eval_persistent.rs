//! Consumer-level eval regressions: real Python kernel persistence, env
//! filtering, timeout, and cancellation. These exercise the tool the model sees,
//! not the wiring of individual helper functions.

use std::{
	collections::HashMap,
	sync::Arc,
	time::{Duration, Instant},
};

use async_trait::async_trait;
use serde_json::{Value, json};

use pi_agent::{
	eval::{
		ArtifactRef, EvalAllowance, EvalHost, EvalRuntime, EvalSessionInfo, EvalSettings, EvalTool,
		Interpreters,
	},
	state::{AgentContext, ToolResult},
	tools::{Tool, ToolError},
};

struct TestHost;

#[async_trait]
impl EvalHost for TestHost {
	async fn call_tool(&self, name: &str, _args: Value, _ctx: &AgentContext) -> Result<ToolResult, ToolError> {
		Err(ToolError::ExecutionFailed(format!("unexpected eval bridge tool `{name}`")))
	}

	async fn completion(&self, _args: Value, _ctx: &AgentContext) -> Result<Value, String> {
		Err("completion is not configured for this test host".to_string())
	}

	async fn agent(&self, _args: Value, _ctx: &AgentContext) -> Result<Value, String> {
		Err("agent is not configured for this test host".to_string())
	}

	async fn budget(&self, _args: Value, _ctx: &AgentContext) -> Result<Value, String> {
		Ok(json!({"total": null, "spent": 0, "hard": false}))
	}

	fn concurrency(&self, _args: &Value) -> Result<Value, String> {
		Ok(json!({"limit": 2}))
	}

	async fn write_artifact(&self, _kind: &str, _bytes: &[u8]) -> Result<Option<ArtifactRef>, String> {
		Ok(Some(ArtifactRef { id: "artifact-test".to_string(), path: None }))
	}

	fn enqueue_background_result(&self, _job_id: &str, _text: String) -> bool {
		true
	}
}

fn python_path() -> Option<std::path::PathBuf> {
	let env: HashMap<String, String> = std::env::vars().collect();
	pi_agent::eval::env::which("python3", &env).or_else(|| pi_agent::eval::env::which("python", &env))
}

async fn harness(spill_bytes: usize) -> Option<(Arc<EvalTool>, Arc<EvalRuntime>, AgentContext, std::path::PathBuf)> {
	if python_path().is_none() {
		eprintln!("skipping eval regression: no python interpreter on PATH");
		return None;
	}
	let settings = EvalSettings::new(
		EvalAllowance { python: true, js: false, ruby: false, julia: false },
		Interpreters::default(),
	)
	.with_probe_timeout_ms(5_000)
	.with_spill_bytes(spill_bytes);
	let runtime = match EvalRuntime::start(settings).await {
		Ok(runtime) => Arc::new(runtime),
		Err(error) => {
			eprintln!("skipping eval regression: runtime start failed: {error}");
			return None;
		},
	};
	let tool = Arc::new(EvalTool::new(
		Arc::clone(&runtime),
		Arc::new(TestHost),
		EvalSessionInfo::new(format!("test-{}", pi_agent::eval::util::random_token())).with_owner(Some(
			"test-owner".to_string(),
		)),
	));
	let cwd = std::env::temp_dir().join(format!("omp-eval-test-{}", pi_agent::eval::util::random_token()));
	std::fs::create_dir_all(&cwd).expect("temp cwd");
	let ctx = AgentContext::new(cwd.clone());
	Some((tool, runtime, ctx, cwd))
}

fn cell_details(result: &ToolResult) -> Value {
	result.details.clone().expect("eval details")
}

#[tokio::test]
async fn python_state_persists_across_calls_and_secrets_are_filtered() {
	let Some((tool, runtime, mut ctx, cwd)) = harness(4096).await else { return };
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

	runtime.dispose_owner(Some("test-owner")).await;
	runtime.shutdown().await;
	let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn output_truncation_emits_artifact_metadata() {
	let Some((tool, runtime, ctx, cwd)) = harness(512).await else { return };
	let result = tool
		.execute(
			&json!({"language": "py", "code": "print('a' * 5000)"}),
			&ctx,
		)
		.await
		.expect("big output cell");
	assert!(!result.is_error, "{}", result.content);
	assert!(result.content.contains("output truncated"), "{}", result.content);
	let details = cell_details(&result);
	assert_eq!(details["meta"]["truncated"], true);
	assert_eq!(details["meta"]["artifactId"], "artifact-test");
	runtime.dispose_owner(Some("test-owner")).await;
	runtime.shutdown().await;
	let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn cell_timeout_aborts_without_hanging_the_tool() {
	let Some((tool, runtime, ctx, cwd)) = harness(4096).await else { return };
	let started = Instant::now();
	let result = tool
		.execute(
			&json!({"language": "py", "code": "import time\ntime.sleep(30)", "timeout": 2}),
			&ctx,
		)
		.await
		.expect("timeout cell returns a result");
	assert!(started.elapsed() < Duration::from_secs(10), "timeout escalated slowly");
	assert!(result.is_error, "timeout must be an error: {}", result.content);
	let details = cell_details(&result);
	assert_eq!(details["cells"][0]["status"], "error");
	runtime.dispose_owner(Some("test-owner")).await;
	runtime.shutdown().await;
	let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn external_cancellation_stops_the_cell() {
	let Some((tool, runtime, ctx, cwd)) = harness(4096).await else { return };
	let cancellation = ctx.cancellation_token.clone();
	let runner_tool = Arc::clone(&tool);
	let runner_ctx = ctx.clone();
	let task = tokio::spawn(async move {
		runner_tool
			.execute(
				&json!({"language": "py", "code": "import time\ntime.sleep(30)", "timeout": 60}),
				&runner_ctx,
			)
			.await
	});
	tokio::time::sleep(Duration::from_millis(400)).await;
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
	runtime.dispose_owner(Some("test-owner")).await;
	runtime.shutdown().await;
	let _ = std::fs::remove_dir_all(cwd);
}
