use std::sync::Arc;

use futures::future::join_all;

use super::{ToolError, registry::ToolRegistry};
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
			return ToolResult {
				tool_call_id: call.id.clone(),
				content: self.registry.missing_tool_diagnostic(&call.name),
				is_error: true,
				terminate: false,
				details: None,
			};
		};

		let approver = self.approver.as_ref().or(ctx.approver.as_ref());
		if let Some(approver) = approver {
			if let Err(denial) = approver.approve(&call.name, &call.arguments).await {
				return ToolResult {
					tool_call_id: call.id.clone(),
					content: format!("Tool execution denied: {denial}"),
					is_error: true,
					terminate: false,
					details: None,
				};
			}
		}

		let result = match tool.execute(&call.arguments, ctx).await {
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
