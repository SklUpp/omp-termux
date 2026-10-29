use std::sync::Arc;

use futures::future::join_all;

use super::{
	ToolError,
	registry::{ToolRegistry, excluded_tool_reason},
};
use crate::state::{AgentContext, ToolApprover, ToolCall, ToolResult};
/// Scheduler partitions a tool batch into concurrent non-exclusive runs and
/// sequential exclusive ones, preserving call order in the results.
pub struct ToolScheduler {
	registry: ToolRegistry,
	approver: Option<Arc<dyn ToolApprover>>,
}

impl ToolScheduler {
	pub fn new(registry: ToolRegistry) -> Self {
		Self { registry, approver: None }
	}

	pub fn with_approver(registry: ToolRegistry, approver: Arc<dyn ToolApprover>) -> Self {
		Self { registry, approver: Some(approver) }
	}

	pub fn approver(&self) -> Option<&Arc<dyn ToolApprover>> {
		self.approver.as_ref()
	}
	/// Execute a batch of tool calls:
	/// - Non-exclusive tools (read, grep, glob) run concurrently.
	/// - Exclusive tools (bash, write, todo) run one at a time.
	///
	/// Always returns exactly one result per call, in call order.
	pub async fn execute_batch(&self, calls: &[ToolCall], ctx: &AgentContext) -> Vec<ToolResult> {
		let mut results = Vec::with_capacity(calls.len());
		let mut concurrent: Vec<&ToolCall> = Vec::new();

		for call in calls {
			let is_exclusive = self
				.registry
				.get(&call.name)
				.is_some_and(|tool| tool.is_exclusive());
			if !is_exclusive {
				concurrent.push(call);
				continue;
			}
			if !concurrent.is_empty() {
				results.extend(self.run_concurrent(&concurrent, ctx).await);
				concurrent.clear();
			}
			results.push(self.run_single(call, ctx).await);
		}

		if !concurrent.is_empty() {
			results.extend(self.run_concurrent(&concurrent, ctx).await);
		}
		results
	}

	async fn run_single(&self, call: &ToolCall, ctx: &AgentContext) -> ToolResult {
		let Some(tool) = self.registry.get(&call.name) else {
			// A known-but-excluded tool carries a machine-readable boundary marker
			// in `details`; unknown names keep the empty diagnostic shape.
			let details = excluded_tool_reason(&call.name).map(|reason| {
				serde_json::json!({
					"code": "platform_boundary",
					"tool": &call.name,
					"reason": reason,
				})
			});
			return ToolResult {
				tool_call_id: call.id.clone(),
				content: self.registry.missing_tool_diagnostic(&call.name),
				is_error: true,
				terminate: false,
				details,
			};
		};

		let approver = self.approver.as_ref().or(ctx.approver.as_ref());
		if let Some(approver) = approver {
			// A pending prompt must not outlive the turn: dropping this future
			// aborts the approver's blocking read, and no approved-but-cancelled
			// call reaches `execute`.
			let decision = tokio::select! {
				biased;
				decision = approver.approve(&call.name, &call.arguments) => decision,
				_ = ctx.cancellation_token.cancelled() => Err("approval cancelled".to_string()),
			};
			if let Err(denial) = decision {
				return ToolResult {
					tool_call_id: call.id.clone(),
					content: format!("Tool execution denied: {denial}"),
					is_error: true,
					terminate: false,
					details: None,
				};
			}
			if ctx.cancellation_token.is_cancelled() {
				return ToolResult {
					tool_call_id: call.id.clone(),
					content: "Tool execution aborted before start".to_string(),
					is_error: true,
					terminate: false,
					details: None,
				};
			}
		}

		let result = match tool.execute_with_call_id(&call.id, &call.arguments, ctx).await {
			Ok(result) => result,
			Err(err) => tool_error_result(&call.id, err),
		};
		ToolResult { tool_call_id: call.id.clone(), ..result }
	}

	async fn run_concurrent(&self, calls: &[&ToolCall], ctx: &AgentContext) -> Vec<ToolResult> {
		join_all(calls.iter().map(|call| self.run_single(call, ctx))).await
	}
}

fn tool_error_result(tool_call_id: &str, err: ToolError) -> ToolResult {
	ToolResult {
		tool_call_id: tool_call_id.to_string(),
		content: format!("Tool error: {err}"),
		is_error: true,
		terminate: false,
		details: None,
	}
}

#[cfg(test)]
mod tests {
	use std::path::PathBuf;
	use std::sync::Arc;
	use std::time::Duration;

	use async_trait::async_trait;
	use serde_json::json;

	use super::*;
	use crate::CancellationToken;
	use crate::tools::{BashTool, registry::ToolRegistry};

	struct PendingApprover {
		started: tokio::sync::mpsc::Sender<()>,
	}

	#[async_trait]
	impl ToolApprover for PendingApprover {
		async fn approve(&self, _name: &str, _args: &serde_json::Value) -> Result<(), String> {
			let _ = self.started.send(()).await;
			std::future::pending::<()>().await;
			Ok(())
		}
	}

	struct AllowApprover;

	#[async_trait]
	impl ToolApprover for AllowApprover {
		async fn approve(&self, _name: &str, _args: &serde_json::Value) -> Result<(), String> {
			Ok(())
		}
	}

	/// Counts approval prompts so a test can prove none was asked.
	struct CountingApprover(Arc<std::sync::atomic::AtomicUsize>);

	#[async_trait]
	impl ToolApprover for CountingApprover {
		async fn approve(&self, _name: &str, _args: &serde_json::Value) -> Result<(), String> {
			self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
			Ok(())
		}
	}

	fn temp_dir(label: &str) -> PathBuf {
		static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
		let path = std::env::temp_dir().join(format!(
			"pi-agent-scheduler-{label}-{}-{}",
			std::process::id(),
			NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
		));
		std::fs::create_dir_all(&path).expect("create temp dir");
		path
	}

	fn bash_call(marker: &std::path::Path) -> Vec<ToolCall> {
		vec![ToolCall {
			id: "call_1".to_string(),
			name: "bash".to_string(),
			arguments: json!({"command": format!("touch {}", marker.display())}),
		}]
	}

	fn bash_scheduler(approver: Arc<dyn ToolApprover>) -> ToolScheduler {
		let mut registry = ToolRegistry::new();
		registry.register(Arc::new(BashTool::new()));
		ToolScheduler::with_approver(registry, approver)
	}

	#[tokio::test]
	async fn excluded_tool_never_prompts_or_runs() {
		let approvals = Arc::new(std::sync::atomic::AtomicUsize::new(0));
		let scheduler = ToolScheduler::with_approver(
			ToolRegistry::new(),
			Arc::new(CountingApprover(approvals.clone())),
		);
		let ctx = AgentContext::new(temp_dir("computer-boundary"));
		let calls = vec![ToolCall {
			id:        "call_1".to_string(),
			name:      "computer".to_string(),
			arguments: json!({ "action": "screenshot" }),
		}];

		let results = scheduler.execute_batch(&calls, &ctx).await;
		assert_eq!(results.len(), 1);
		assert!(results[0].is_error, "an excluded tool must not succeed");
		assert!(
			results[0].content.contains("excluded by the platform boundary"),
			"{}",
			results[0].content
		);
		let details = results[0].details.as_ref().expect("typed boundary details");
		assert_eq!(details["code"], "platform_boundary");
		assert_eq!(details["tool"], "computer");
		assert_eq!(
			details["reason"].as_str(),
			excluded_tool_reason("computer"),
			"details must reuse the exclusion table"
		);
		assert_eq!(
			approvals.load(std::sync::atomic::Ordering::SeqCst),
			0,
			"an excluded tool must not ask for approval"
		);
	}

	#[tokio::test]
	async fn cancelled_approval_never_executes_the_tool() {
		let dir = temp_dir("approval-cancel");
		let marker = dir.join("ran");
		let (started_tx, mut started_rx) = tokio::sync::mpsc::channel(1);
		let scheduler = bash_scheduler(Arc::new(PendingApprover { started: started_tx }));
		let token = CancellationToken::new();
		let ctx = AgentContext::with_cancellation(dir.clone(), token.clone());
		let calls = bash_call(&marker);

		let batch = tokio::spawn(async move { scheduler.execute_batch(&calls, &ctx).await });
		started_rx
			.recv()
			.await
			.expect("approval prompt must be pending");
		token.cancel();

		let results = tokio::time::timeout(Duration::from_secs(5), batch)
			.await
			.expect("cancelled approval must settle")
			.expect("join");
		assert_eq!(results.len(), 1);
		assert!(results[0].is_error, "cancelled approval must not succeed");
		assert!(!marker.exists(), "cancelled approval still ran the command");
	}

	#[tokio::test]
	async fn approval_after_cancellation_does_not_execute_the_tool() {
		let dir = temp_dir("approval-late");
		let marker = dir.join("ran");
		let scheduler = bash_scheduler(Arc::new(AllowApprover));
		let token = CancellationToken::new();
		token.cancel();
		let ctx = AgentContext::with_cancellation(dir.clone(), token);
		let calls = bash_call(&marker);

		let results = scheduler.execute_batch(&calls, &ctx).await;
		assert_eq!(results.len(), 1);
		assert!(results[0].is_error, "cancelled turn must not execute the tool");
		assert!(!marker.exists(), "approved call ran after cancellation");
	}
}
