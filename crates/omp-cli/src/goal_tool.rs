//! Hidden `goal` tool (TS `goals/tools/goal-tool.ts`): the headless surface of
//! Goal Mode lifecycle. The tool is mounted only when `goal.enabled` (a live or
//! paused goal always keeps it, issue #9444) and never reaches child agents.
//!
//! All transitions live on [`GoalRuntime`]; this file only validates raw model
//! input, calls one operation, and renders the TS result text/details.

use std::sync::Arc;

use async_trait::async_trait;
use pi_agent::{
	state::{AgentContext, ToolResult},
	tools::{Tool, ToolError},
};
use serde_json::{Value, json};

use crate::goal::{Goal, GoalRuntime, GoalStatus, completion_budget_report};

/// Tool name advertised to the model.
pub const GOAL_TOOL_NAME: &str = "goal";

const GOAL_TOOL_DESCRIPTION: &str =
	include_str!("../../../packages/coding-agent/src/prompts/tools/goal.md");

/// TOOL OPS accepted by the TS schema.
const GOAL_OPS: [&str; 5] = ["create", "get", "complete", "resume", "drop"];

/// Tool result payload (TS `GoalToolResponse`).
#[derive(Debug, Clone, PartialEq)]
pub struct GoalToolResponse {
	pub goal:                      Option<Goal>,
	pub remaining_tokens:          Option<u64>,
	pub completion_budget_report:  Option<String>,
}

/// TS `buildGoalToolResponse`.
#[must_use]
pub fn build_goal_tool_response(
	goal: Option<Goal>,
	include_completion_report: bool,
) -> GoalToolResponse {
	let remaining = goal.as_ref().and_then(|goal| goal.remaining_tokens());
	let report = if include_completion_report {
		goal.as_ref()
			.filter(|goal| goal.status == GoalStatus::Complete)
			.and_then(completion_budget_report)
	} else {
		None
	};
	GoalToolResponse {
		goal,
		remaining_tokens: remaining,
		completion_budget_report: report,
	}
}

/// Live "plan mode holds the exclusive mode slot" probe. `None` for sessions
/// with no plan mode (RPC, print, REPL).
pub type PlanModeProbe = Arc<dyn Fn() -> bool + Send + Sync>;

/// TS `interactive-mode.ts` guard: a Goal cannot be created or resumed while
/// plan mode owns the mode slot.
pub const EXIT_PLAN_MODE_FIRST: &str = "Exit plan mode first.";

pub struct GoalTool {
	runtime:   Arc<GoalRuntime>,
	plan_mode: Option<PlanModeProbe>,
}

impl GoalTool {
	pub fn new(runtime: Arc<GoalRuntime>) -> Self {
		Self { runtime, plan_mode: None }
	}

	/// Mount the plan-mode probe; `create`/`resume` then refuse while plan mode
	/// is active.
	#[must_use]
	pub fn with_plan_mode(mut self, probe: Option<PlanModeProbe>) -> Self {
		self.plan_mode = probe;
		self
	}

	fn plan_mode_active(&self) -> bool {
		self.plan_mode.as_ref().is_some_and(|probe| probe())
	}
}

#[async_trait]
impl Tool for GoalTool {
	fn name(&self) -> &str {
		GOAL_TOOL_NAME
	}

	fn description(&self) -> &str {
		GOAL_TOOL_DESCRIPTION
	}

	fn schema(&self) -> Value {
		json!({
			"type": "object",
			"properties": {
				"op": {
					"type": "string",
					"enum": GOAL_OPS,
					"description": "goal operation"
				},
				"objective": { "type": "string", "description": "goal objective" },
				"token_budget": { "type": "integer", "description": "token budget" }
			},
			"required": ["op"],
			"additionalProperties": false
		})
	}

	fn is_exclusive(&self) -> bool {
		false
	}

	async fn execute(&self, args: &Value, _ctx: &AgentContext) -> Result<ToolResult, ToolError> {
		let object = args
			.as_object()
			.ok_or_else(|| ToolError::InvalidArguments("goal arguments must be an object".to_string()))?;
		let op = object
			.get("op")
			.and_then(Value::as_str)
			.ok_or_else(|| ToolError::InvalidArguments("Missing required parameter 'op'".to_string()))?;
		if !GOAL_OPS.contains(&op) {
			return Err(ToolError::InvalidArguments(format!(
				"Unknown goal op '{op}'; expected one of {}",
				GOAL_OPS.join(", ")
			)));
		}

		if matches!(op, "create" | "resume") && self.plan_mode_active() {
			return Err(ToolError::Rejected(EXIT_PLAN_MODE_FIRST.to_string()));
		}

		let response = match op {
			"create" => {
				let (objective, token_budget) = validate_create_params(object)?;
				let created = self
					.runtime
					.create_goal(&objective, token_budget)
					.map_err(ToolError::Rejected)?;
				build_goal_tool_response(Some(created.goal), false)
			},
			"get" => build_goal_tool_response(
				self.runtime.state().map(|state| state.goal),
				false,
			),
			"resume" => {
				let resumed = self.runtime.resume_goal().map_err(ToolError::Rejected)?;
				build_goal_tool_response(Some(resumed.goal), false)
			},
			"drop" => build_goal_tool_response(self.runtime.drop_goal(), false),
			_ => {
				let completed = self
					.runtime
					.complete_goal_from_tool()
					.map_err(ToolError::Rejected)?;
				build_goal_tool_response(Some(completed), true)
			},
		};

		let text = render_text(&response);
		let goal_value = response
			.goal
			.as_ref()
			.and_then(|goal| serde_json::to_value(goal).ok())
			.unwrap_or(Value::Null);
		Ok(ToolResult {
			tool_call_id: String::new(),
			content:      text,
			is_error:     false,
			terminate:    false,
			details:      Some(json!({
				"op": op,
				"goal": goal_value,
				"remainingTokens": response.remaining_tokens,
				"completionBudgetReport": response.completion_budget_report,
			})),
		})
	}
}

/// TS `validateCreateParams`, with the tool-level wording (the runtime keeps
/// its own wording for the same rejections).
fn validate_create_params(
	object: &serde_json::Map<String, Value>,
) -> Result<(String, Option<u64>), ToolError> {
	let objective = match object.get("objective") {
		None | Some(Value::Null) => String::new(),
		Some(Value::String(value)) => value.trim().to_string(),
		Some(_) => {
			return Err(ToolError::InvalidArguments(
				"objective must be a string when provided".to_string(),
			));
		},
	};
	if objective.is_empty() {
		return Err(ToolError::Rejected("objective is required when op=create".to_string()));
	}
	let token_budget = match object.get("token_budget") {
		None | Some(Value::Null) => None,
		Some(value) => match integer_value(value) {
			Some(budget) if budget > 0 => Some(budget),
			_ => {
				return Err(ToolError::Rejected(
					"token_budget must be a positive integer when provided".to_string(),
				));
			},
		},
	};
	Ok((objective, token_budget))
}

/// Integral JSON number as `u64`; fractions and negatives are rejected.
fn integer_value(value: &Value) -> Option<u64> {
	if let Some(value) = value.as_u64() {
		return Some(value);
	}
	let float = value.as_f64()?;
	(float.is_finite() && float >= 0.0 && float.fract() == 0.0 && float < u64::MAX as f64)
		.then_some(float as u64)
}

/// TS `goal-tool.ts` result text.
fn render_text(response: &GoalToolResponse) -> String {
	let Some(goal) = &response.goal else {
		return "No active goal.".to_string();
	};
	let mut text = format!(
		"Goal: {}\nStatus: {}\nTokens: {} used",
		goal.objective,
		status_label(goal.status),
		goal.tokens_used
	);
	if let Some(budget) = goal.token_budget {
		text.push_str(&format!(" / {budget} budget"));
	}
	if let Some(remaining) = response.remaining_tokens {
		text.push_str(&format!("\nRemaining tokens: {remaining}"));
	}
	if let Some(report) = &response.completion_budget_report {
		text.push_str(&format!("\n\n{report}"));
	}
	text
}

fn status_label(status: GoalStatus) -> &'static str {
	match status {
		GoalStatus::Active => "active",
		GoalStatus::Paused => "paused",
		GoalStatus::BudgetLimited => "budget-limited",
		GoalStatus::Complete => "complete",
		GoalStatus::Dropped => "dropped",
		GoalStatus::Unknown => "unknown",
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn goal(status: GoalStatus, tokens_used: u64) -> Goal {
		Goal {
			id:                "g1".to_string(),
			objective:         "ship".to_string(),
			status,
			token_budget:      Some(50),
			tokens_used,
			time_used_seconds: 7,
			created_at:        1,
			updated_at:        2,
		}
	}

	async fn run(tool: &GoalTool, args: Value) -> Result<ToolResult, ToolError> {
		tool.execute(&args, &AgentContext::new(std::path::PathBuf::from(".")))
			.await
	}

	/// Op text/details and the exact validation wording are model-visible.
	#[tokio::test]
	async fn goal_tool_ops_render_and_validate() {
		let runtime = Arc::new(GoalRuntime::new());
		let tool = GoalTool::new(Arc::clone(&runtime));

		let created = run(&tool, json!({"op": "create", "objective": "  ship  ", "token_budget": 50}))
			.await
			.expect("create");
		assert_eq!(created.content, "Goal: ship\nStatus: active\nTokens: 0 used / 50 budget\nRemaining tokens: 50");
		assert_eq!(created.details.as_ref().expect("details")["op"], "create");
		assert_eq!(created.details.as_ref().expect("details")["remainingTokens"], 50);

		let duplicate = run(&tool, json!({"op": "create", "objective": "again"}))
			.await
			.expect_err("duplicate create");
		assert_eq!(
			duplicate.to_string(),
			"cannot create a new goal because this session already has a goal"
		);
		let empty = run(&tool, json!({"op": "create", "objective": " "}))
			.await
			.expect_err("empty objective");
		assert_eq!(empty.to_string(), "objective is required when op=create");
		let bad_budget = run(&tool, json!({"op": "create", "objective": "x", "token_budget": 0}))
			.await
			.expect_err("bad budget");
		assert_eq!(bad_budget.to_string(), "token_budget must be a positive integer when provided");

		let got = run(&tool, json!({"op": "get"})).await.expect("get");
		assert!(got.content.contains("Status: active"));
		assert_eq!(got.details.as_ref().expect("details")["goal"]["objective"], "ship");

		// Resume with no paused goal, then a paused goal renders as paused.
		assert_eq!(
			run(&tool, json!({"op": "resume"})).await.expect("resume").content,
			"Goal: ship\nStatus: active\nTokens: 0 used / 50 budget\nRemaining tokens: 50"
		);
		runtime.pause_goal().expect("pause");
		let paused = run(&tool, json!({"op": "get"})).await.expect("get paused");
		assert!(paused.content.contains("Status: paused"));

		// Complete reports the budget metric once, in text and details.
		runtime.set_restored_state(Some(crate::goal::GoalModeState {
			enabled: true,
			mode:    crate::goal::GoalMode::Active,
			goal:    goal(GoalStatus::Active, 40),
		}));
		let completed = run(&tool, json!({"op": "complete"})).await.expect("complete");
		assert!(completed.content.contains("Status: complete"));
		assert!(completed.content.contains("Remaining tokens: 10"));
		assert!(completed.content.contains(
			"Goal achieved. Report final budget usage to the user: tokens used: 40 of 50; time \
			 used: 7 seconds."
		));
		let details = completed.details.as_ref().expect("details");
		assert_eq!(details["goal"]["status"], "complete");
		assert_eq!(
			details["completionBudgetReport"],
			"Goal achieved. Report final budget usage to the user: tokens used: 40 of 50; time used: 7 seconds."
		);
		assert_eq!(
			run(&tool, json!({"op": "complete"})).await.expect_err("complete twice").to_string(),
			"goal is already complete"
		);

		// Drop the completed goal, then `get` has nothing to report.
		assert_eq!(
			run(&tool, json!({"op": "drop"})).await.expect("drop").content,
			"Goal: ship\nStatus: dropped\nTokens: 40 used / 50 budget\nRemaining tokens: 10"
		);
		assert_eq!(
			run(&tool, json!({"op": "get"})).await.expect("get").content,
			"No active goal."
		);

		let unknown = run(&tool, json!({"op": "restart"})).await.expect_err("unknown op");
		assert!(unknown.to_string().contains("Unknown goal op 'restart'"));
		let missing = run(&tool, json!({"objective": "x"})).await.expect_err("missing op");
		assert!(missing.to_string().contains("Missing required parameter 'op'"));
	}

	/// Plan mode owns the exclusive mode slot: `create`/`resume` refuse.
	#[tokio::test]
	async fn goal_create_and_resume_refuse_while_plan_mode_is_active() {
		use std::sync::atomic::{AtomicBool, Ordering};

		let runtime = Arc::new(GoalRuntime::new());
		let plan_active = Arc::new(AtomicBool::new(true));
		let probe = Arc::clone(&plan_active);
		let tool = GoalTool::new(Arc::clone(&runtime))
			.with_plan_mode(Some(Arc::new(move || probe.load(Ordering::SeqCst))));

		let refused = run(&tool, json!({"op": "create", "objective": "ship"}))
			.await
			.expect_err("create refused");
		assert_eq!(refused.to_string(), EXIT_PLAN_MODE_FIRST);
		let refused_resume = run(&tool, json!({"op": "resume"})).await.expect_err("resume refused");
		assert_eq!(refused_resume.to_string(), EXIT_PLAN_MODE_FIRST);
		// Read-only ops still answer while plan mode is active.
		assert!(run(&tool, json!({"op": "get"})).await.is_ok());

		plan_active.store(false, Ordering::SeqCst);
		assert!(run(&tool, json!({"op": "create", "objective": "ship"})).await.is_ok());
	}
}
