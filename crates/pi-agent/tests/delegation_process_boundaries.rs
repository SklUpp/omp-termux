//! Hub process boundaries through the public tool surface:
//! owner-scoped visibility and control, restart/queue rejection across owners,
//! backend pinning across a supervisor swap, and wait-then-stop cleanup.

use std::sync::Arc;
use std::time::Duration;

use pi_agent::tools::process::{ProcessBackend, ProcessManager};
use pi_agent::{
	AgentContext, DelegationLimits, HubTool, ModelProvider, OpenAiCompatibleProvider, Supervisor,
	Tool, ToolRegistry,
};
use serde_json::{Value, json};

fn temp_root(prefix: &str) -> std::path::PathBuf {
	let dir = std::env::temp_dir().join(format!(
		"omp-hub-boundaries-{prefix}-{}",
		std::process::id()
	));
	std::fs::create_dir_all(&dir).expect("create temp root");
	dir
}

fn supervisor() -> Arc<Supervisor> {
	let provider: Arc<dyn ModelProvider> = Arc::new(
		OpenAiCompatibleProvider::new("http://127.0.0.1:9", "k", "m").expect("provider"),
	);
	Supervisor::new(
		ToolRegistry::new(),
		provider,
		None,
		DelegationLimits::default(),
	)
}

fn daemons(result: &pi_agent::ToolResult) -> Vec<Value> {
	result
		.details
		.as_ref()
		.and_then(|details| details.get("daemons"))
		.and_then(Value::as_array)
		.cloned()
		.unwrap_or_default()
}

fn daemon(result: &pi_agent::ToolResult) -> Value {
	result
		.details
		.as_ref()
		.and_then(|details| details.get("daemon"))
		.cloned()
		.expect("daemon details")
}

#[tokio::test]
async fn process_visibility_and_control_are_owner_scoped() {
	let root = temp_root("owner");
	let manager = Arc::new(ProcessManager::new(root.clone()));
	let backend: Arc<dyn ProcessBackend> = manager.clone();
	let supervisor = supervisor();
	supervisor.set_processes(Arc::clone(&backend));
	let ctx = AgentContext::new(root.clone());

	let owner_a = HubTool::root(Arc::clone(&supervisor))
		.with_processes(Arc::clone(&backend), Some("owner-a".to_string()));
	let owner_b = HubTool::root(Arc::clone(&supervisor))
		.with_processes(Arc::clone(&backend), Some("owner-b".to_string()));
	let root_hub =
		HubTool::root(Arc::clone(&supervisor)).with_processes(Arc::clone(&backend), None);

	let started = owner_a
		.execute(
			&json!({
				"op": "start",
				"name": "owned",
				"application": "sh",
				"args": ["-c", "sleep 30"],
				"pty": false,
			}),
			&ctx,
		)
		.await
		.expect("start");
	assert_eq!(daemon(&started)["owner"], "owner-a");

	let seen_by_a = owner_a.execute(&json!({ "op": "ps" }), &ctx).await.expect("ps a");
	let names_a: Vec<&str> = daemons(&seen_by_a)
		.iter()
		.filter_map(|daemon| daemon["name"].as_str())
		.collect();
	assert_eq!(names_a, ["owned"]);

	let seen_by_b = owner_b.execute(&json!({ "op": "ps" }), &ctx).await.expect("ps b");
	assert!(
		daemons(&seen_by_b).is_empty(),
		"another owner must not see the process: {}",
		seen_by_b.content
	);

	let foreign_stop = owner_b
		.execute(&json!({ "op": "stop", "name": "owned", "timeout": 1.0 }), &ctx)
		.await
		.expect_err("foreign stop must be rejected");
	assert!(
		foreign_stop.to_string().contains("Unknown process"),
		"another owner must not resolve the process: {foreign_stop}"
	);

	let root_ps = root_hub.execute(&json!({ "op": "ps" }), &ctx).await.expect("ps root");
	assert_eq!(daemons(&root_ps).len(), 1, "root sees every process");

	let stopped = root_hub
		.execute(&json!({ "op": "stop", "name": "owned", "timeout": 2.0 }), &ctx)
		.await
		.expect("root stop");
	assert_eq!(daemon(&stopped)["state"], "exited");

	let remaining = root_hub.execute(&json!({ "op": "ps" }), &ctx).await.expect("ps after stop");
	assert!(
		daemons(&remaining)
			.iter()
			.all(|daemon| daemon["state"] == "exited"),
		"no active process may survive stop: {}",
		remaining.content
	);

	manager.shutdown();
	let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn hub_keeps_its_process_backend_snapshot_across_supervisor_swap() {
	let root = temp_root("pinned");
	std::fs::create_dir_all(root.join("primary")).expect("primary root");
	std::fs::create_dir_all(root.join("swapped")).expect("swapped root");
	let primary = Arc::new(ProcessManager::new(root.join("primary")));
	let swapped = Arc::new(ProcessManager::new(root.join("swapped")));
	let primary_backend: Arc<dyn ProcessBackend> = primary.clone();
	let swapped_backend: Arc<dyn ProcessBackend> = swapped.clone();
	let supervisor = supervisor();
	supervisor.set_processes(Arc::clone(&primary_backend));
	let ctx = AgentContext::new(root.clone());

	let pinned = HubTool::root(Arc::clone(&supervisor))
		.with_processes(Arc::clone(&primary_backend), Some("owner-a".to_string()));
	pinned
		.execute(
			&json!({
				"op": "start",
				"name": "pinned-job",
				"application": "sh",
				"args": ["-c", "sleep 30"],
				"pty": false,
			}),
			&ctx,
		)
		.await
		.expect("start on pinned backend");

	// A later root swap must not re-point an already bound child tool.
	supervisor.set_processes(Arc::clone(&swapped_backend));
	let still_pinned = pinned.execute(&json!({ "op": "ps" }), &ctx).await.expect("ps pinned");
	assert_eq!(daemons(&still_pinned).len(), 1, "{}", still_pinned.content);

	let on_swapped = HubTool::root(Arc::clone(&supervisor))
		.with_processes(Arc::clone(&swapped_backend), None)
		.execute(&json!({ "op": "ps" }), &ctx)
		.await
		.expect("ps swapped");
	assert!(
		daemons(&on_swapped).is_empty(),
		"job tables must stay scoped to one backend: {}",
		on_swapped.content
	);

	primary.shutdown();
	swapped.shutdown();
	let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn wait_and_stop_settle_without_leaking_an_active_process() {
	let root = temp_root("wait");
	let manager = Arc::new(ProcessManager::new(root.clone()));
	let backend: Arc<dyn ProcessBackend> = manager.clone();
	let supervisor = supervisor();
	supervisor.set_processes(Arc::clone(&backend));
	let ctx = AgentContext::new(root.clone());
	let owner_a = HubTool::root(Arc::clone(&supervisor))
		.with_processes(Arc::clone(&backend), Some("owner-a".to_string()));
	let root_hub =
		HubTool::root(Arc::clone(&supervisor)).with_processes(Arc::clone(&backend), None);

	owner_a
		.execute(
			&json!({
				"op": "start",
				"name": "slow",
				"application": "sh",
				"args": ["-c", "sleep 30"],
				"pty": false,
			}),
			&ctx,
		)
		.await
		.expect("start");

	// A short wait times out while the process is still running, and reports
	// that honestly instead of claiming an exit.
	let short_wait = owner_a
		.execute(
			&json!({ "op": "wait", "name": "slow", "for": "exit", "timeout": 0.1 }),
			&ctx,
		)
		.await
		.expect("short wait");
	assert_eq!(
		short_wait.details.as_ref().and_then(|details| details.get("timedOut")),
		Some(&json!(true))
	);

	let waiting = {
		let waiter = owner_a.clone();
		let wait_ctx = ctx.clone();
		tokio::spawn(async move {
			waiter
				.execute(
					&json!({ "op": "wait", "name": "slow", "for": "exit", "timeout": 10.0 }),
					&wait_ctx,
				)
				.await
		})
	};
	root_hub
		.execute(&json!({ "op": "stop", "name": "slow", "timeout": 2.0 }), &ctx)
		.await
		.expect("stop");
	let outcome = tokio::time::timeout(Duration::from_secs(5), waiting)
		.await
		.expect("wait must settle after stop")
		.expect("wait task")
		.expect("wait result");
	assert_eq!(daemon(&outcome)["state"], "exited");
	assert_eq!(
		outcome.details.as_ref().and_then(|details| details.get("timedOut")),
		Some(&json!(false)),
		"a stop-settled wait must not claim timeout: {}",
		outcome.content
	);

	// The terminal record is owner-bound: another owner cannot reuse the name.
	let reclaim = HubTool::root(Arc::clone(&supervisor))
		.with_processes(Arc::clone(&backend), Some("owner-b".to_string()))
		.execute(
			&json!({
				"op": "start",
				"name": "slow",
				"application": "sh",
				"args": ["-c", "sleep 1"],
				"pty": false,
			}),
			&ctx,
		)
		.await
		.expect_err("foreign reuse must be rejected");
	assert!(reclaim.to_string().contains("another agent"), "{reclaim}");

	let listed = root_hub.execute(&json!({ "op": "ps" }), &ctx).await.expect("ps");
	assert!(
		daemons(&listed)
			.iter()
			.all(|daemon| daemon["state"] == "exited"),
		"{}",
		listed.content
	);

	manager.shutdown();
	let _ = std::fs::remove_dir_all(root);
}
