//! Goal mode: state and session-log shape (TS `goals/state.ts`), the `goal`
//! payload of a `mode_change` entry, the terminal `goal-completed` metric, and
//! the session-local [`GoalRuntime`] (TS `goals/runtime.ts`).
//!
//! The runtime is persistence-free: lifecycle transitions queue [`GoalPersist`]
//! intents and [`GoalRuntimeEvent`]s, and the single session writer (CLI or RPC)
//! drains them in event order. Turn baselines, the wall clock and budget-steer
//! dedup are session-local and lost on reopen by design (TS
//! `GoalRuntimeSnapshot`).

use std::collections::HashSet;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::session::{SessionStore, Usage};

/// Generic `mode_change` value that clears any mode.
pub const MODE_NONE: &str = "none";
/// `mode_change` value of a live goal; `data.goal` carries the state.
pub const MODE_GOAL: &str = "goal";
/// `mode_change` value of a paused goal; `data.goal` carries the state.
pub const MODE_GOAL_PAUSED: &str = "goal_paused";
/// `customType` of the terminal metric entry written when goal mode exits.
pub const GOAL_COMPLETED_CUSTOM_TYPE: &str = "goal-completed";
/// `customType` of the hidden between-turn continuation prompt.
pub const GOAL_CONTINUATION_CUSTOM_TYPE: &str = "goal-continuation";
/// `customType` of the persisted background child usage report.
pub const BACKGROUND_USAGE_CUSTOM_TYPE: &str = "background-usage";

/// TS `GoalStatus`. [`GoalStatus::Unknown`] only ever comes from a log written
/// by a newer build: restore keeps it so the goal is paused instead of
/// discarded, it never accrues, and it is never written back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum GoalStatus {
	Active,
	Paused,
	BudgetLimited,
	Complete,
	Dropped,
	Unknown,
}

impl GoalStatus {
	/// Parses a stored status; anything outside the TS union stays unknown.
	#[must_use]
	pub fn parse(raw: &str) -> Self {
		match raw {
			"active" => Self::Active,
			"paused" => Self::Paused,
			"budget-limited" => Self::BudgetLimited,
			"complete" => Self::Complete,
			"dropped" => Self::Dropped,
			_ => Self::Unknown,
		}
	}

	/// TS `isAccountingStatus`: only these two accrue tokens and time.
	#[must_use]
	pub fn is_accounting(self) -> bool {
		matches!(self, Self::Active | Self::BudgetLimited)
	}

	/// `Unknown` is read-only state; a write boundary must reject it instead of
	/// serializing a status TS cannot parse back.
	fn is_writable(self) -> bool {
		self != Self::Unknown
	}
}

/// TS `Goal`, persisted verbatim inside `mode_change.data.goal`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Goal {
	pub id:        String,
	pub objective: String,
	pub status:    GoalStatus,
	#[serde(rename = "tokenBudget", skip_serializing_if = "Option::is_none")]
	pub token_budget:      Option<u64>,
	#[serde(rename = "tokensUsed")]
	pub tokens_used:       u64,
	#[serde(rename = "timeUsedSeconds")]
	pub time_used_seconds: u64,
	#[serde(rename = "createdAt")]
	pub created_at:        u64,
	#[serde(rename = "updatedAt")]
	pub updated_at:        u64,
}

impl Goal {
	/// TS `remainingTokens`: `None` for an unbudgeted goal, never negative.
	#[must_use]
	pub fn remaining_tokens(&self) -> Option<u64> {
		self
			.token_budget
			.map(|budget| budget.saturating_sub(self.tokens_used))
	}

	/// Write boundary: a tolerated unknown status and a zero budget are not
	/// persistable state.
	pub fn validate_for_write(&self) -> Result<(), String> {
		if !self.status.is_writable() {
			return Err("cannot persist goal with an unknown status".to_string());
		}
		validate_token_budget(self.token_budget)
	}
}

/// Live goal mode: TS `GoalModeState` minus session-local bookkeeping. `Exiting`
/// implies `reason: "completed"` and is never persisted — restore always yields
/// [`GoalMode::Active`].
#[derive(Debug, Clone, PartialEq)]
pub struct GoalModeState {
	pub enabled: bool,
	pub mode:    GoalMode,
	pub goal:    Goal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalMode {
	Active,
	Exiting,
}

/// TS `validateTokenBudget`, runtime wording. Callers that parse raw tool input
/// own the tool wording; this is the state write boundary.
pub fn validate_token_budget(token_budget: Option<u64>) -> Result<(), String> {
	match token_budget {
		Some(0) => Err("goal token_budget must be a positive integer when provided".to_string()),
		_ => Ok(()),
	}
}

/// TS `goalTokenDelta`: new work only. `cache_read` is a reused prefix and is
/// deliberately excluded; `cache_write` is billed on Anthropic/Bedrock.
#[must_use]
pub fn goal_token_delta(current: &Usage, baseline: &Usage) -> u64 {
	current.input.saturating_sub(baseline.input)
		+ current.cache_write.saturating_sub(baseline.cache_write)
		+ current.output.saturating_sub(baseline.output)
}

/// `data` payload of a `mode_change` entry for [`MODE_GOAL`] /
/// [`MODE_GOAL_PAUSED`] (TS `appendModeChange(mode, {goal})`).
///
/// # Errors
/// Propagates [`Goal::validate_for_write`].
pub fn encode_mode_change_data(goal: &Goal) -> Result<Value, String> {
	goal.validate_for_write()?;
	let mut data = Map::new();
	data.insert("goal".to_string(), serde_json::to_value(goal).map_err(|e| e.to_string())?);
	Ok(Value::Object(data))
}

/// `data` payload of the terminal `custom goal-completed` entry (TS
/// `appendCustomEntry("goal-completed", {...})`). `tokenBudget` is omitted for
/// an unbudgeted goal, exactly like `JSON.stringify` dropping `undefined`.
#[must_use]
pub fn encode_completed_data(goal: &Goal) -> Value {
	let mut data = Map::new();
	data.insert("objective".to_string(), Value::String(goal.objective.clone()));
	data.insert("tokensUsed".to_string(), Value::from(goal.tokens_used));
	if let Some(budget) = goal.token_budget {
		data.insert("tokenBudget".to_string(), Value::from(budget));
	}
	data.insert("timeUsedSeconds".to_string(), Value::from(goal.time_used_seconds));
	Value::Object(data)
}

/// TS `#goalFromModeData`: rebuilds a goal from `mode_change.data` and returns
/// `None` for a malformed payload instead of inventing fields. A non-number
/// `tokenBudget` (say `"50k"`) means unbudgeted, not an error.
#[must_use]
pub fn goal_from_mode_data(mode_data: Option<&Value>) -> Option<Goal> {
	let goal = mode_data?.get("goal")?.as_object()?;
	Some(Goal {
		id:                goal.get("id")?.as_str()?.to_string(),
		objective:         goal.get("objective")?.as_str()?.to_string(),
		status:            GoalStatus::parse(goal.get("status")?.as_str()?),
		token_budget:      goal.get("tokenBudget").and_then(count),
		tokens_used:       goal.get("tokensUsed").and_then(count)?,
		time_used_seconds: goal.get("timeUsedSeconds").and_then(count)?,
		created_at:        goal.get("createdAt").and_then(count)?,
		updated_at:        goal.get("updatedAt").and_then(count)?,
	})
}

/// `typeof value === "number"`, narrowed to a non-negative integer so no field
/// is fabricated from a string, a fraction or a negative.
fn count(value: &Value) -> Option<u64> {
	if let Some(count) = value.as_u64() {
		return Some(count);
	}
	let float = value.as_f64()?;
	(float.is_finite() && float >= 0.0 && float < u64::MAX as f64 && float.fract() == 0.0)
		.then_some(float as u64)
}

/// What a reopened/forked/branched session must do with the persisted mode
/// (TS `#reconcileModeFromSession`).
#[derive(Debug, Clone, PartialEq)]
pub enum RestorePlan {
	/// No `mode_change` on the active branch, or the last one is `none`: clear
	/// live accounting, write nothing.
	Idle,
	/// The last mode is a goal mode but unusable — malformed `data.goal`, or
	/// the `goal.enabled` setting is off: clear accounting and append
	/// `mode_change none`.
	ClearGoalMode,
	/// Restore this goal, then run thread-resume reconciliation: an active goal
	/// replayed without an explicit preserve is paused, a paused one stays
	/// paused.
	Restore { enabled: bool, goal: Goal },
}

/// Plans the restore from the last `mode_change` of the *active branch* only, so
/// a session branched before a later goal transition restores the pre-branch
/// state instead of the whole-file last entry. `goal_feature_enabled` is the
/// `goal.enabled` setting.
#[must_use]
pub fn plan_restore(store: &SessionStore, goal_feature_enabled: bool) -> RestorePlan {
	let Some((mode, data)) = store.latest_mode_change() else {
		return RestorePlan::Idle;
	};
	if mode != MODE_GOAL && mode != MODE_GOAL_PAUSED {
		return RestorePlan::Idle;
	}
	match goal_from_mode_data(data) {
		Some(goal) if goal_feature_enabled => {
			RestorePlan::Restore { enabled: mode == MODE_GOAL, goal }
		},
		_ => RestorePlan::ClearGoalMode,
	}
}

// ------------------------------------------------------------- turn budget --

/// TS `TurnBudget` (`modes/turn-budget.ts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnBudget {
	/// Output-token ceiling for the turn.
	pub total: u64,
	/// Whether the ceiling is enforced (eval `agent()` throws past it).
	pub hard:  bool,
}

/// TS `parseTurnBudget`: a standalone `+<number>[k|m][!]` token, anchored to
/// whitespace/string boundaries so prices and versions in prose do not match.
/// The first valid directive wins; `0`, a non-finite value or a trailing
/// non-boundary character makes it no directive at all.
#[must_use]
pub fn parse_turn_budget(text: &str) -> Option<TurnBudget> {
	let mut index = 0;
	while index < text.len() {
		let boundary = index == 0
			|| text[..index].chars().next_back().is_some_and(char::is_whitespace);
		if boundary
			&& text[index..].starts_with('+')
			&& let Some(budget) = parse_turn_budget_at(&text[index + 1..])
		{
			return Some(budget);
		}
		index += text[index..].chars().next().map_or(1, char::len_utf8);
	}
	None
}

/// Parses the directive body (everything after `+`) at the string start.
fn parse_turn_budget_at(rest: &str) -> Option<TurnBudget> {
	let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
	if digits == 0 {
		return None;
	}
	let mut number_len = digits;
	if rest[number_len..].starts_with('.') {
		let fraction = rest[number_len + 1..]
			.bytes()
			.take_while(u8::is_ascii_digit)
			.count();
		if fraction == 0 {
			return None;
		}
		number_len += 1 + fraction;
	}
	let value: f64 = rest.get(..number_len)?.parse().ok()?;
	let (multiplier, after_unit) = match rest[number_len..].chars().next() {
		Some('k' | 'K') => (1_000.0, number_len + 1),
		Some('m' | 'M') => (1_000_000.0, number_len + 1),
		_ => (1.0, number_len),
	};
	let hard = rest[after_unit..].starts_with('!');
	let tail = &rest[after_unit + usize::from(hard)..];
	if !tail.is_empty() && !tail.chars().next().is_some_and(char::is_whitespace) {
		return None;
	}
	if !value.is_finite() || value <= 0.0 {
		return None;
	}
	Some(TurnBudget { total: (value * multiplier).round() as u64, hard })
}

/// TS `GoalTokenUsage`: the four billed counters of the session view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenCounters {
	pub input:       u64,
	pub output:      u64,
	pub cache_read:  u64,
	pub cache_write: u64,
}

impl TokenCounters {
	#[must_use]
	pub fn from_usage(usage: &Usage) -> Self {
		Self {
			input:       usage.input,
			output:      usage.output,
			cache_read:  usage.cache_read,
			cache_write: usage.cache_write,
		}
	}

	#[must_use]
	pub fn from_token_usage(usage: &pi_agent::TokenUsage) -> Self {
		Self {
			input:       usage.input,
			output:      usage.output,
			cache_read:  usage.cache_read,
			cache_write: usage.cache_write,
		}
	}

	fn saturating_add(self, other: Self) -> Self {
		Self {
			input:       self.input.saturating_add(other.input),
			output:      self.output.saturating_add(other.output),
			cache_read:  self.cache_read.saturating_add(other.cache_read),
			cache_write: self.cache_write.saturating_add(other.cache_write),
		}
	}
}

/// [`goal_token_delta`] over plain counters.
#[must_use]
pub fn counters_delta(current: &TokenCounters, baseline: &TokenCounters) -> u64 {
	current.input.saturating_sub(baseline.input)
		+ current.cache_write.saturating_sub(baseline.cache_write)
		+ current.output.saturating_sub(baseline.output)
}

/// TS `EvalBudgetResult`: the eval `budget()` helper snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetSnapshot {
	pub total: Option<u64>,
	pub spent: u64,
	pub hard:  bool,
}

/// TS `GoalPromptKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalPromptKind {
	Active,
	Continuation,
	BudgetLimit,
}

const GOAL_MODE_ACTIVE_PROMPT: &str = include_str!(
	"../../../packages/coding-agent/src/prompts/goals/goal-mode-active.md"
);
const GOAL_CONTINUATION_PROMPT: &str = include_str!(
	"../../../packages/coding-agent/src/prompts/goals/goal-continuation.md"
);
const GOAL_BUDGET_LIMIT_PROMPT: &str = include_str!(
	"../../../packages/coding-agent/src/prompts/goals/goal-budget-limit.md"
);
const GOAL_TODO_CONTEXT_PROMPT: &str = include_str!(
	"../../../packages/coding-agent/src/prompts/goals/goal-todo-context.md"
);

/// TS `escapeXmlText`: only the three markup characters are escaped.
#[must_use]
pub fn escape_xml_text(input: &str) -> String {
	let mut out = String::with_capacity(input.len());
	for character in input.chars() {
		match character {
			'&' => out.push_str("&amp;"),
			'<' => out.push_str("&lt;"),
			'>' => out.push_str("&gt;"),
			other => out.push(other),
		}
	}
	out
}

fn budget_value(goal: &Goal) -> String {
	goal.token_budget.map_or_else(|| "none".to_string(), |budget| budget.to_string())
}

fn remaining_value(goal: &Goal) -> String {
	goal
		.token_budget
		.map_or_else(|| "unbounded".to_string(), |budget| {
			budget.saturating_sub(goal.tokens_used).to_string()
		})
}

/// TS `renderGoalPrompt`: the goal templates with escaped substitution.
#[must_use]
pub fn render_goal_prompt(kind: GoalPromptKind, goal: &Goal) -> String {
	let template = match kind {
		GoalPromptKind::Active => GOAL_MODE_ACTIVE_PROMPT,
		GoalPromptKind::Continuation => GOAL_CONTINUATION_PROMPT,
		GoalPromptKind::BudgetLimit => GOAL_BUDGET_LIMIT_PROMPT,
	};
	render_template(
		template,
		&[
			("objective", escape_xml_text(&goal.objective)),
			("tokensUsed", goal.tokens_used.to_string()),
			("tokenBudget", budget_value(goal)),
			("remainingTokens", remaining_value(goal)),
			("timeUsedSeconds", goal.time_used_seconds.to_string()),
		],
	)
}

/// Minimal `{{name}}` substitution. A token with no value is kept verbatim, so
/// a template edit can never silently drop text.
fn render_template(template: &str, values: &[(&str, String)]) -> String {
	let mut out = String::with_capacity(template.len() + 64);
	let mut rest = template;
	while let Some(start) = rest.find("{{") {
		out.push_str(&rest[..start]);
		let after = &rest[start + 2..];
		let Some(end) = after.find("}}") else {
			out.push_str(&rest[start..]);
			return out;
		};
		let key = after[..end].trim();
		match values.iter().find(|(name, _)| *name == key) {
			Some((_, value)) => out.push_str(value),
			None => out.push_str(&rest[start..start + 2 + end + 2]),
		}
		rest = &after[end + 2..];
	}
	out.push_str(rest);
	out
}

/// TS `completionBudgetReport`: the one-line report appended to a completed
/// goal tool result, or `None` when nothing was spent.
#[must_use]
pub fn completion_budget_report(goal: &Goal) -> Option<String> {
	let mut parts = Vec::new();
	if let Some(budget) = goal.token_budget {
		parts.push(format!("tokens used: {} of {budget}", goal.tokens_used));
	}
	if goal.time_used_seconds > 0 {
		parts.push(format!("time used: {} seconds", goal.time_used_seconds));
	}
	if parts.is_empty() {
		return None;
	}
	Some(format!(
		"Goal achieved. Report final budget usage to the user: {}.",
		parts.join("; ")
	))
}

/// TS `#buildGoalModeMessage` content: the active prompt plus the optional
/// persisted-todo context.
#[must_use]
pub fn render_goal_mode_context(goal_context: &str, todo_context: Option<&str>) -> String {
	match todo_context {
		Some(todo) => format!("{goal_context}\n{todo}"),
		None => goal_context.to_string(),
	}
}

/// TS `#buildGoalTodoContext`: live persisted todos, omitted when the `todo`
/// tool is unavailable or no phase holds a task.
#[must_use]
pub fn build_goal_todo_context(
	phases: &[pi_agent::tools::todo::TodoPhase],
	todo_tool_active: bool,
) -> Option<String> {
	if !todo_tool_active {
		return None;
	}
	let phases: Vec<&pi_agent::tools::todo::TodoPhase> = phases
		.iter()
		.filter(|phase| !phase.tasks.is_empty())
		.collect();
	if phases.is_empty() {
		return None;
	}
	let mut total = 0usize;
	let mut closed = 0usize;
	let mut open = 0usize;
	let mut body = String::new();
	for phase in phases {
		body.push_str(&format!("- {}\n", sanitize_goal_todo_text(&phase.name)));
		for task in &phase.tasks {
			total += 1;
			match task.status {
				pi_agent::tools::todo::TodoStatus::Completed
				| pi_agent::tools::todo::TodoStatus::Abandoned => closed += 1,
				_ => open += 1,
			}
			body.push_str(&format!(
				"  - [{}] {}\n",
				todo_status_label(task.status),
				sanitize_goal_todo_text(&task.content)
			));
		}
	}
	let rendered = GOAL_TODO_CONTEXT_PROMPT
		.replace("{{closed}}", &closed.to_string())
		.replace("{{total}}", &total.to_string())
		.replace("{{open}}", &open.to_string());
	Some(replace_phase_block(&rendered, &body))
}

/// Replaces the template's whole `{{#each phases}}…{{/each}}` block with the
/// rendered phase list, keeping the surrounding text byte-identical.
fn replace_phase_block(template: &str, body: &str) -> String {
	let (Some(start), Some(end)) = (template.find("{{#each phases}}"), template.rfind("{{/each}}"))
	else {
		return template.to_string();
	};
	if end < start {
		return template.to_string();
	}
	let mut out = String::with_capacity(template.len() + body.len());
	out.push_str(&template[..start]);
	out.push_str(body);
	out.push_str(&template[end + "{{/each}}".len()..]);
	out
}

/// TS `TodoStatus` wire label.
fn todo_status_label(status: pi_agent::tools::todo::TodoStatus) -> &'static str {
	use pi_agent::tools::todo::TodoStatus;
	match status {
		TodoStatus::Pending => "pending",
		TodoStatus::InProgress => "in_progress",
		TodoStatus::Completed => "completed",
		TodoStatus::Abandoned => "abandoned",
		TodoStatus::Blocked => "blocked",
	}
}

/// TS `#sanitizeGoalTodoText`: escaped, newlines/tabs made visible.
fn sanitize_goal_todo_text(text: &str) -> String {
	let mut out = String::with_capacity(text.len());
	for character in escape_xml_text(text).chars() {
		match character {
			'\n' => out.push_str("\\n"),
			'\r' => out.push_str("\\r"),
			'\t' => out.push_str("\\t"),
			'\u{2028}' | '\u{2029}' => out.push(' '),
			control if control.is_control() => out.push(' '),
			other => out.push(other),
		}
	}
	out
}

// ------------------------------------------------------------ goal runtime --

/// One queued session-log write. Drained by the session's single writer, never
/// applied here: a tool call runs inside the agent loop, which must not open a
/// second writer on the same log.
#[derive(Debug, Clone, PartialEq)]
pub enum GoalPersist {
	/// `mode_change` with this mode; `data` is the `{goal}` payload (absent for
	/// [`MODE_NONE`]).
	ModeChange { mode: &'static str, data: Option<Value> },
	/// Terminal `custom goal-completed` metric.
	Completed(Value),
	/// `custom background-usage` entry: the child's summed usage for this job,
	/// persisted once so a reopened log can still account it. `id` stays for
	/// downgrade compatibility; `result_key` is the durable identity when the
	/// writer knows it.
	BackgroundUsage {
		id: String,
		result_key: Option<String>,
		usage: pi_agent::TokenUsage,
	},
}

/// TS `GoalRuntimeEvent`, as the RPC/live host forwards it.
#[derive(Debug, Clone, PartialEq)]
pub enum GoalRuntimeEvent {
	Updated { goal: Option<Goal>, state: Option<GoalModeState> },
}

/// TS `goal_updated` session event frame.
#[must_use]
pub fn goal_updated_frame(event: &GoalRuntimeEvent) -> Value {
	let GoalRuntimeEvent::Updated { goal, state } = event;
	let mut frame = Map::new();
	frame.insert("type".to_string(), Value::String("goal_updated".to_string()));
	frame.insert(
		"goal".to_string(),
		goal.as_ref()
			.and_then(|goal| serde_json::to_value(goal).ok())
			.unwrap_or(Value::Null),
	);
	if let Some(state) = state {
		let mut body = Map::new();
		body.insert("enabled".to_string(), Value::Bool(state.enabled));
		body.insert(
			"mode".to_string(),
			Value::String(
				match state.mode {
					GoalMode::Active => "active",
					GoalMode::Exiting => "exiting",
				}
				.to_string(),
			),
		);
		if state.mode == GoalMode::Exiting {
			body.insert("reason".to_string(), Value::String("completed".to_string()));
		}
		body.insert(
			"goal".to_string(),
			serde_json::to_value(&state.goal).unwrap_or(Value::Null),
		);
		frame.insert("state".to_string(), Value::Object(body));
	}
	Value::Object(frame)
}

/// TS `GoalBudgetSteering`: whether a budget flip may steer this flush.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalSteering {
	Allowed,
	Suppressed,
}

/// TS `onTaskAborted` reason. Only an interrupt pauses an active goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalAbortReason {
	Interrupted,
	Internal,
}

/// Hidden-message delivery installed by the live run's steering channel.
pub type GoalSteerFn = std::sync::Arc<dyn Fn(&str) -> bool + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
struct TurnSnapshot {
	baseline:       TokenCounters,
	active_goal_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WallClock {
	last_accounted_at: u64,
	active_goal_id:    Option<String>,
}

/// TS `getTurnBudget` window: one output baseline per user prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct TurnWindow {
	total:           Option<u64>,
	hard:            bool,
	output_baseline: u64,
	eval_output:     u64,
}

impl TurnWindow {
	fn spent(&self, usage: TokenCounters) -> u64 {
		usage.output.saturating_sub(self.output_baseline).saturating_add(self.eval_output)
	}
}

struct Inner {
	state:               Option<GoalModeState>,
	usage:               TokenCounters,
	turn:                Option<TurnSnapshot>,
	wall:                WallClock,
	budget_reported_for: Option<String>,
	window:              TurnWindow,
	pending:             Vec<GoalPersist>,
	events:              Vec<GoalRuntimeEvent>,
	steer:               Option<GoalSteerFn>,
	/// Turn-scoped durable seed: keys already in the retained log (or just
	/// appended this turn). Replaced at `begin_turn` / after append; never a
	/// log-lifetime index on `SessionStore`.
	durable_background_ids: HashSet<String>,
	/// In-flight exact-once keys charged this turn but not yet confirmed
	/// durable. Grows with hub/task/delivery charges; retired after a
	/// successful `background-usage` append.
	seen_background_ids: HashSet<String>,
}

/// Session-local Goal Mode controller (TS `GoalRuntime` + the turn-budget
/// window). Clone-free: owners share it through an `Arc`.
pub struct GoalRuntime {
	inner: std::sync::Mutex<Inner>,
	clock: Box<dyn Fn() -> u64 + Send + Sync>,
}

impl std::fmt::Debug for GoalRuntime {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let inner = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		formatter
			.debug_struct("GoalRuntime")
			.field("state", &inner.state)
			.field("pending", &inner.pending.len())
			.finish()
	}
}

impl Default for GoalRuntime {
	fn default() -> Self {
		Self::new()
	}
}

impl GoalRuntime {
	#[must_use]
	pub fn new() -> Self {
		Self::with_clock(Box::new(|| {
			std::time::SystemTime::now()
				.duration_since(std::time::UNIX_EPOCH)
				.map_or(0, |duration| duration.as_millis() as u64)
		}))
	}

	/// Test seam: a deterministic wall clock in epoch milliseconds.
	#[must_use]
	pub fn with_clock(clock: Box<dyn Fn() -> u64 + Send + Sync>) -> Self {
		let now = clock();
		Self {
			inner: std::sync::Mutex::new(Inner {
				state:               None,
				usage:               TokenCounters::default(),
				turn:                None,
				wall:                WallClock { last_accounted_at: now, active_goal_id: None },
				budget_reported_for: None,
				window:              TurnWindow::default(),
				pending:             Vec::new(),
				events:              Vec::new(),
				steer:               None,
				durable_background_ids: HashSet::new(),
				seen_background_ids: HashSet::new(),
			}),
			clock,
		}
	}

	fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
		self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
	}

	fn now_ms(&self) -> u64 {
		(self.clock)()
	}

	#[must_use]
	pub fn state(&self) -> Option<GoalModeState> {
		self.lock().state.clone()
	}

	#[must_use]
	pub fn has_goal(&self) -> bool {
		self.lock().state.is_some()
	}

	/// Install (or replace) the live hidden-message sink. A replaced run's sink
	/// dies with its channel, so a stale hook cannot reach a later turn.
	pub fn set_steer(&self, steer: Option<GoalSteerFn>) {
		self.lock().steer = steer;
	}

	/// Restore a persisted mode without persisting or emitting: the reconcile
	/// path decides what to write next.
	pub fn set_restored_state(&self, state: Option<GoalModeState>) {
		let mut inner = self.lock();
		if state.is_none() {
			inner.durable_background_ids.clear();
			inner.seen_background_ids.clear();
		}
		inner.state = state;
	}

	/// Drain queued session-log writes (in order).
	#[must_use]
	pub fn take_pending(&self) -> Vec<GoalPersist> {
		std::mem::take(&mut self.lock().pending)
	}

	/// Drain queued `goal_updated` events (in order).
	#[must_use]
	pub fn take_events(&self) -> Vec<GoalRuntimeEvent> {
		std::mem::take(&mut self.lock().events)
	}

	/// TS `clearAccounting`: drop turn/wall/dedup state, keep the goal.
	pub fn clear_accounting(&self) {
		let now = self.now_ms();
		let mut inner = self.lock();
		inner.turn = None;
		inner.budget_reported_for = None;
		inner.durable_background_ids.clear();
		inner.seen_background_ids.clear();
		clear_active_accounting(&mut inner, now);
	}

	/// Replace the turn-scoped durable seed with `keys` from a retained-log
	/// scan and clear in-flight charges. Call at turn start (and after a
	/// successful background append batch) so reopen / mid-turn hub duplicates
	/// do not re-charge, while the in-flight set stays bounded.
	pub fn seed_background_dedup(&self, keys: HashSet<String>) {
		let mut inner = self.lock();
		inner.durable_background_ids = keys;
		inner.seen_background_ids.clear();
	}

	/// Move keys that just became durable out of the in-flight set into the
	/// durable seed. In-flight-only keys stay until the next seed or turn.
	pub fn retire_durable_background_keys<I, S>(&self, durable: I)
	where
		I: IntoIterator<Item = S>,
		S: AsRef<str>,
	{
		let mut inner = self.lock();
		for key in durable {
			let key = key.as_ref();
			inner.seen_background_ids.remove(key);
			if !key.is_empty() {
				inner.durable_background_ids.insert(key.to_string());
			}
		}
	}

	/// Observable size of the in-flight (non-durable) background dedup set.
	#[cfg(test)]
	#[must_use]
	pub(crate) fn background_dedup_len(&self) -> usize {
		self.lock().seen_background_ids.len()
	}

	#[cfg(test)]
	#[must_use]
	pub(crate) fn durable_background_dedup_len(&self) -> usize {
		self.lock().durable_background_ids.len()
	}

	/// Open the budget window and accounting baseline for one turn.
	///
	/// `text` is `Some` for a user-authored prompt (which may carry a `+Nk`
	/// directive) and `None` for a synthetic/continuation turn, which keeps the
	/// window the user set. `seed` is the session's cumulative usage.
	///
	/// Clears the transient background dedup set; the session writer then calls
	/// [`Self::seed_background_dedup`] with durable log keys so a reopened
	/// session does not re-charge retained children.
	pub fn begin_turn(&self, text: Option<&str>, seed: TokenCounters) {
		let now = self.now_ms();
		let mut inner = self.lock();
		inner.usage = seed;
		inner.durable_background_ids.clear();
		inner.seen_background_ids.clear();
		if let Some(text) = text {
			inner.window = match parse_turn_budget(text) {
				Some(budget) => TurnWindow {
					total:           Some(budget.total),
					hard:            budget.hard,
					output_baseline: seed.output,
					eval_output:     0,
				},
				None => TurnWindow {
					total:           None,
					hard:            false,
					output_baseline: seed.output,
					eval_output:     0,
				},
			};
		}
		inner.turn = Some(TurnSnapshot { baseline: seed, active_goal_id: None });
		let Some(state) = inner.state.clone() else {
			return;
		};
		if !state.enabled || !state.goal.status.is_accounting() {
			return;
		}
		if let Some(turn) = inner.turn.as_mut() {
			turn.active_goal_id = Some(state.goal.id.clone());
		}
		if inner.wall.active_goal_id.as_deref() != Some(state.goal.id.as_str()) {
			inner.wall = WallClock {
				last_accounted_at: now,
				active_goal_id:    Some(state.goal.id),
			};
		}
	}

	/// One provider request finished. Only [`pi_agent::Message::usage`] counts:
	/// `TurnEnd`/`AgentEnd` repeat the same message and must never add again,
	/// and an unreported request stays absent instead of adding zeros.
	pub fn observe_message(&self, message: &pi_agent::Message) {
		if message.role != pi_agent::Role::Assistant {
			return;
		}
		let Some(usage) = message.usage.as_ref() else {
			return;
		};
		let counters = TokenCounters::from_token_usage(usage);
		let mut inner = self.lock();
		inner.usage = inner.usage.saturating_add(counters);
	}

	/// One background child result was injected. `usage` is the child's summed
	/// report; `result_key` is the namespaced accounting key (raw `id` is the
	/// legacy fallback). A job key counts once even if a hub snapshot repeats
	/// it, and a key is never recorded without usage: an absent report must not
	/// suppress a later delivery that carries one.
	pub fn observe_background_result(
		&self,
		id: &str,
		result_key: Option<&str>,
		usage: Option<&pi_agent::TokenUsage>,
	) {
		let key = result_key.filter(|key| !key.is_empty()).unwrap_or(id);
		if key.is_empty() {
			return;
		}
		let Some(usage) = usage else {
			return;
		};
		let now = self.now_ms();
		let steer = {
			let mut inner = self.lock();
			if inner.durable_background_ids.contains(key)
				|| !inner.seen_background_ids.insert(key.to_string())
			{
				return;
			}
			inner.pending.push(GoalPersist::BackgroundUsage {
				id:   id.to_string(),
				result_key: result_key
					.filter(|key| !key.is_empty())
					.map(str::to_string),
				usage: *usage,
			});
			inner.usage = inner.usage.saturating_add(TokenCounters::from_token_usage(usage));
			flush_locked(&mut inner, now, GoalSteering::Allowed, false)
		};
		self.send_steer(steer);
	}

	/// One tool finished. Delegated child usage rides on `details.accounting`
	/// (one entry per child, with that child's own counters) and is charged once
	/// per result key; goal-tool flushes are suppressed so the goal cannot charge
	/// itself for its own transitions.
	///
	/// Fresh keyed hub/task charges also queue [`GoalPersist::BackgroundUsage`]
	/// so the session writer can durably append them before the transient key
	/// is retired — hub-only keys must not live only in RAM across turns.
	pub fn observe_tool_end(&self, tool: &str, details: Option<&Value>) {
		if let Some(details) = details {
			let accounting = details.get("accounting").and_then(Value::as_array);
			match accounting {
				// Keyed: a batch that partly overlaps an already-charged ledger or
				// delivery contributes exactly the unseen children, never the sum.
				Some(entries) => {
					let mut inner = self.lock();
					for entry in entries {
						let Some(key) = entry
							.get("resultKey")
							.and_then(Value::as_str)
							.filter(|key| !key.is_empty())
						else {
							continue;
						};
						let Some(usage) =
							entry.get("usage").and_then(usage_counters_from_value)
						else {
							continue;
						};
						if inner.durable_background_ids.contains(key)
							|| !inner.seen_background_ids.insert(key.to_string())
						{
							continue;
						}
						inner.usage = inner.usage.saturating_add(usage);
						// Persist so reopen / next-turn seed sees the key even
						// when no auto-delivery later writes background-usage.
						let id = key
							.rsplit_once(':')
							.map(|(_, raw)| raw.to_string())
							.unwrap_or_else(|| key.to_string());
						inner.pending.push(GoalPersist::BackgroundUsage {
							id,
							result_key: Some(key.to_string()),
							usage: token_usage_from_counters(usage),
						});
					}
				},
				// Legacy aggregate: only a payload with no per-child identity at all
				// (a log written before keys existed) is charged as one batch.
				None => {
					if let Some(child) = details
						.get("usage")
						.and_then(usage_counters_from_value)
					{
						let mut inner = self.lock();
						inner.usage = inner.usage.saturating_add(child);
					}
				},
			}
		}
		// A hub `jobs` snapshot repeats each background child's summed usage;
		// the job id is the exactly-once key shared with auto-delivery.
		if tool == "hub"
			&& let Some(jobs) = details
				.and_then(|details| details.get("jobs"))
				.and_then(Value::as_array)
		{
			let mut inner = self.lock();
			for job in jobs {
				let Some(usage) = job.get("usage").and_then(usage_counters_from_value) else {
					continue;
				};
				// `resultKey` alone when present, else the legacy raw id: a
				// reopened CLI reuses ids, so consulting both would merge runs.
				let Some(key) = crate::session::background_usage_key(job) else {
					continue;
				};
				if inner.durable_background_ids.contains(key)
					|| !inner.seen_background_ids.insert(key.to_string())
				{
					continue;
				}
				inner.usage = inner.usage.saturating_add(usage);
				let id = job
					.get("id")
					.and_then(Value::as_str)
					.filter(|id| !id.is_empty())
					.unwrap_or(key)
					.to_string();
				let result_key = job
					.get("resultKey")
					.and_then(Value::as_str)
					.filter(|key| !key.is_empty())
					.map(str::to_string);
				inner.pending.push(GoalPersist::BackgroundUsage {
					id,
					result_key,
					usage: token_usage_from_counters(usage),
				});
			}
		}
		if tool == "goal" {
			self.flush_usage(GoalSteering::Suppressed);
		} else {
			self.flush_usage(GoalSteering::Allowed);
		}
	}

	/// TS `onAgentEnd`: final suppressed flush, then the turn snapshot ends.
	pub fn on_agent_end(&self) {
		let now = self.now_ms();
		let steer = {
			let mut inner = self.lock();
			if !has_accounting_state(&inner) {
				inner.turn = None;
				return;
			}
			let steer = flush_locked(&mut inner, now, GoalSteering::Suppressed, false);
			inner.turn = None;
			steer
		};
		self.send_steer(steer);
	}

	/// TS `onTaskAborted`: flush, and pause an interrupted active goal once.
	pub fn on_task_aborted(&self, reason: GoalAbortReason) {
		let now = self.now_ms();
		let mut inner = self.lock();
		let needs_accounting = has_accounting_state(&inner);
		let needs_pause = reason == GoalAbortReason::Interrupted
			&& inner.state.as_ref().is_some_and(|state| {
				state.enabled && state.goal.status == GoalStatus::Active
			});
		if !needs_accounting && !needs_pause {
			inner.turn = None;
			return;
		}
		let _ = flush_locked(
			&mut inner,
			now,
			GoalSteering::Suppressed,
			reason == GoalAbortReason::Internal,
		);
		inner.turn = None;
		if reason != GoalAbortReason::Interrupted {
			return;
		}
		let Some(state) = inner.state.clone() else {
			return;
		};
		if !state.enabled || state.goal.status != GoalStatus::Active {
			return;
		}
		let mut paused = state;
		paused.enabled = false;
		paused.goal.status = GoalStatus::Paused;
		paused.goal.updated_at = now;
		clear_active_accounting(&mut inner, now);
		inner.budget_reported_for = None;
		commit_locked(&mut inner, Some(paused), Some(MODE_GOAL_PAUSED), true);
	}

	/// TS `flushUsage`: accrual for the current goal, if it owns this turn.
	pub fn flush_usage(&self, steering: GoalSteering) {
		if !has_accounting_state(&self.lock()) {
			return;
		}
		let now = self.now_ms();
		let steer = {
			let mut inner = self.lock();
			flush_locked(&mut inner, now, steering, false)
		};
		self.send_steer(steer);
	}

	/// TS `getTurnBudget` + `getGoalModeState` precedence for eval `budget()`:
	/// a `+Nk` window wins, then an enabled goal budget, else the advisory
	/// output delta with no ceiling.
	#[must_use]
	pub fn budget_snapshot(&self) -> BudgetSnapshot {
		let inner = self.lock();
		if inner.window.total.is_some() {
			return BudgetSnapshot {
				total: inner.window.total,
				spent: inner.window.spent(inner.usage),
				hard:  inner.window.hard,
			};
		}
		if let Some(state) = inner.state.as_ref().filter(|state| state.enabled) {
			return BudgetSnapshot {
				total: state.goal.token_budget,
				spent: state.goal.tokens_used,
				hard:  state.goal.token_budget.is_some(),
			};
		}
		BudgetSnapshot { total: None, spent: inner.window.spent(inner.usage), hard: false }
	}

	/// TS `recordEvalSubagentOutput`: a positive child output count is added to
	/// this turn's spend (invalid values are ignored, never fabricated).
	pub fn record_eval_subagent_output(&self, output: u64) {
		if output == 0 {
			return;
		}
		let mut inner = self.lock();
		inner.window.eval_output = inner.window.eval_output.saturating_add(output);
	}

	// ---------------------------------------------------------- transitions --

	pub fn create_goal(
		&self,
		objective: &str,
		token_budget: Option<u64>,
	) -> Result<GoalModeState, String> {
		let objective = objective.trim();
		if objective.is_empty() {
			return Err("objective is required when op=create".to_string());
		}
		validate_token_budget(token_budget)?;
		let now = self.now_ms();
		let mut inner = self.lock();
		if let Some(existing) = &inner.state
			&& existing.goal.status != GoalStatus::Dropped
			&& existing.goal.status != GoalStatus::Complete
		{
			return Err("cannot create a new goal because this session already has a goal".to_string());
		}
		let state = create_goal_state(objective, token_budget, now);
		inner.budget_reported_for = None;
		mark_active_accounting(&mut inner, &state.goal, false, now);
		commit_locked(&mut inner, Some(state.clone()), Some(MODE_GOAL), true);
		Ok(state)
	}

	/// TS `replaceGoal`: supersede a running goal without dropping its history.
	pub fn replace_goal(
		&self,
		objective: &str,
		token_budget: Option<u64>,
	) -> Result<GoalModeState, String> {
		let objective = objective.trim();
		if objective.is_empty() {
			return Err("objective is required when op=replace".to_string());
		}
		validate_token_budget(token_budget)?;
		let now = self.now_ms();
		let mut inner = self.lock();
		let replaceable = inner.state.as_ref().is_some_and(|state| {
			state.enabled && state.goal.status.is_accounting()
		});
		if !replaceable {
			return Err("cannot replace goal because no goal is active".to_string());
		}
		let _ = flush_locked(&mut inner, now, GoalSteering::Suppressed, false);
		let state = create_goal_state(objective, token_budget, now);
		inner.budget_reported_for = None;
		mark_active_accounting(&mut inner, &state.goal, false, now);
		commit_locked(&mut inner, Some(state.clone()), Some(MODE_GOAL), true);
		Ok(state)
	}

	pub fn resume_goal(&self) -> Result<GoalModeState, String> {
		let now = self.now_ms();
		let mut inner = self.lock();
		let Some(mut state) = inner.state.clone() else {
			return Err("No paused goal.".to_string());
		};
		if state.goal.status == GoalStatus::Complete {
			return Err("Goal is already complete.".to_string());
		}
		state.enabled = true;
		state.mode = GoalMode::Active;
		state.goal.status = GoalStatus::Active;
		state.goal.updated_at = now;
		inner.budget_reported_for = None;
		mark_active_accounting(&mut inner, &state.goal, false, now);
		commit_locked(&mut inner, Some(state.clone()), Some(MODE_GOAL), true);
		Ok(state)
	}

	pub fn pause_goal(&self) -> Option<GoalModeState> {
		let now = self.now_ms();
		let mut inner = self.lock();
		let _ = flush_locked(&mut inner, now, GoalSteering::Suppressed, false);
		let Some(mut state) = inner.state.clone() else {
			return None;
		};
		state.enabled = false;
		state.mode = GoalMode::Active;
		if matches!(state.goal.status, GoalStatus::Active | GoalStatus::BudgetLimited) {
			state.goal.status = GoalStatus::Paused;
		}
		state.goal.updated_at = now;
		clear_active_accounting(&mut inner, now);
		inner.budget_reported_for = None;
		commit_locked(&mut inner, Some(state.clone()), Some(MODE_GOAL_PAUSED), true);
		Some(state)
	}

	pub fn drop_goal(&self) -> Option<Goal> {
		let now = self.now_ms();
		let mut inner = self.lock();
		let _ = flush_locked(&mut inner, now, GoalSteering::Suppressed, false);
		let state = inner.state.clone()?;
		let mut dropped = state.goal;
		dropped.status = GoalStatus::Dropped;
		dropped.updated_at = now;
		clear_active_accounting(&mut inner, now);
		inner.budget_reported_for = None;
		inner.events.push(GoalRuntimeEvent::Updated {
			goal:  Some(dropped.clone()),
			state: Some(GoalModeState { enabled: false, mode: GoalMode::Active, goal: dropped.clone() }),
		});
		commit_locked(&mut inner, None, Some(MODE_NONE), false);
		Some(dropped)
	}

	pub fn complete_goal_from_tool(&self) -> Result<Goal, String> {
		let now = self.now_ms();
		let mut inner = self.lock();
		let _ = flush_locked(&mut inner, now, GoalSteering::Suppressed, false);
		let Some(mut state) = inner.state.clone() else {
			return Err("cannot complete goal because no goal is active".to_string());
		};
		match state.goal.status {
			GoalStatus::Complete => return Err("goal is already complete".to_string()),
			GoalStatus::Dropped => return Err("cannot complete a dropped goal".to_string()),
			_ => {},
		}
		state.enabled = false;
		state.goal.status = GoalStatus::Complete;
		state.goal.updated_at = now;
		state.mode = GoalMode::Exiting;
		clear_active_accounting(&mut inner, now);
		inner.budget_reported_for = None;
		commit_locked(&mut inner, Some(state), Some(MODE_GOAL), true);
		Ok(inner
			.state
			.as_ref()
			.map(|state| state.goal.clone())
			.expect("completed goal is set"))
	}

	/// TS `onBudgetMutated` (`/goal budget`): adjust or clear the ceiling and
	/// flip the status with the new values. The TUI surface is excluded, so this
	/// is reached through the runtime API only.
	pub fn on_budget_mutated(
		&self,
		token_budget: Option<u64>,
	) -> Result<Option<GoalModeState>, String> {
		validate_token_budget(token_budget)?;
		let now = self.now_ms();
		let mut inner = self.lock();
		inner.budget_reported_for = None;
		let _ = flush_locked(&mut inner, now, GoalSteering::Suppressed, false);
		let Some(mut state) = inner.state.clone() else {
			return Ok(None);
		};
		state.goal.token_budget = token_budget;
		state.goal.updated_at = now;
		let mut should_steer = false;
		if token_budget.is_some_and(|budget| state.goal.tokens_used >= budget) {
			if state.goal.status == GoalStatus::Active {
				state.goal.status = GoalStatus::BudgetLimited;
				should_steer = true;
			}
		} else if state.goal.status == GoalStatus::BudgetLimited {
			state.goal.status = GoalStatus::Active;
			state.enabled = true;
			mark_active_accounting(&mut inner, &state.goal, false, now);
		}
		let persist = if state.enabled { MODE_GOAL } else { MODE_GOAL_PAUSED };
		commit_locked(&mut inner, Some(state.clone()), Some(persist), true);
		if should_steer {
			let prompt = render_goal_prompt(GoalPromptKind::BudgetLimit, &state.goal);
			inner.budget_reported_for = Some(state.goal.id.clone());
			let steer = inner.steer.clone();
			drop(inner);
			if let Some(send) = steer {
				let _ = send(&prompt);
			}
			return Ok(Some(state));
		}
		Ok(Some(state))
	}

	/// TS `onThreadResumed`: reconcile the restored state with the live thread.
	///
	/// An active goal replayed without an explicit preserve is paused and
	/// persisted (crash/abort semantics); a preserved active goal only rebases
	/// accounting.
	pub fn on_thread_resumed(&self, preserve_active_goal: bool) -> Option<GoalModeState> {
		let now = self.now_ms();
		let mut inner = self.lock();
		let mut state = inner.state.clone()?;
		if preserve_active_goal && state.enabled && state.goal.status == GoalStatus::Active {
			mark_active_accounting(&mut inner, &state.goal, true, now);
			commit_locked(&mut inner, Some(state.clone()), None, true);
			return Some(state);
		}
		if state.goal.status == GoalStatus::Active {
			state.enabled = false;
			state.goal.status = GoalStatus::Paused;
			state.goal.updated_at = now;
			clear_active_accounting(&mut inner, now);
			inner.budget_reported_for = None;
			commit_locked(&mut inner, Some(state.clone()), Some(MODE_GOAL_PAUSED), true);
			return Some(state);
		}
		if state.enabled && state.goal.status.is_accounting() {
			mark_active_accounting(&mut inner, &state.goal, false, now);
		} else {
			clear_active_accounting(&mut inner, now);
		}
		commit_locked(&mut inner, Some(state.clone()), None, true);
		Some(state)
	}

	/// Complete-exit (after the turn drains): take the exiting goal, clear the
	/// mode, and let the owner write `mode_change none` + `goal-completed`.
	pub fn take_exiting(&self) -> Option<Goal> {
		let now = self.now_ms();
		let mut inner = self.lock();
		let goal = inner
			.state
			.as_ref()
			.filter(|state| state.mode == GoalMode::Exiting)
			.map(|state| state.goal.clone())?;
		inner.state = None;
		inner.budget_reported_for = None;
		clear_active_accounting(&mut inner, now);
		inner.events.push(GoalRuntimeEvent::Updated { goal: None, state: None });
		Some(goal)
	}

	/// TS `buildActivePrompt`: the hidden per-turn goal context.
	#[must_use]
	pub fn build_active_prompt(&self) -> Option<String> {
		let inner = self.lock();
		inner
			.state
			.as_ref()
			.filter(|state| state.enabled && state.goal.status == GoalStatus::Active)
			.map(|state| render_goal_prompt(GoalPromptKind::Active, &state.goal))
	}

	/// TS `buildContinuationPrompt`: the between-turn continuation prompt.
	#[must_use]
	pub fn build_continuation_prompt(&self) -> Option<String> {
		let inner = self.lock();
		inner
			.state
			.as_ref()
			.filter(|state| state.enabled && state.goal.status == GoalStatus::Active)
			.map(|state| render_goal_prompt(GoalPromptKind::Continuation, &state.goal))
	}

	fn send_steer(&self, text: Option<String>) {
		let Some(text) = text else {
			return;
		};
		let steer = { self.lock().steer.clone() };
		if let Some(send) = steer {
			let _ = send(&text);
		}
	}
}

/// Continuation modes accepted by `goal.continuationModes`.
pub const CONTINUATION_MODE_INTERACTIVE: &str = "interactive";
/// Headless CLI continuation (text/json); an explicit opt-in.
pub const CONTINUATION_MODE_HEADLESS: &str = "headless";

/// Whether the headless driver may auto-continue. The upstream default is
/// `["interactive"]`, so this is `false` until the user opts in.
#[must_use]
pub fn headless_continuation_allowed(modes: &[String]) -> bool {
	modes.iter().any(|mode| mode == CONTINUATION_MODE_HEADLESS)
}

fn has_accounting_state(inner: &Inner) -> bool {
	inner
		.state
		.as_ref()
		.is_some_and(|state| state.enabled && state.goal.status.is_accounting())
}

fn create_goal_state(objective: &str, token_budget: Option<u64>, now: u64) -> GoalModeState {
	GoalModeState {
		enabled: true,
		mode:    GoalMode::Active,
		goal:    Goal {
			id:                crate::output::session_id(),
			objective:         objective.to_string(),
			status:            GoalStatus::Active,
			token_budget,
			tokens_used:       0,
			time_used_seconds: 0,
			created_at:        now,
			updated_at:        now,
		},
	}
}

fn mark_active_accounting(inner: &mut Inner, goal: &Goal, reset_wall_clock: bool, now: u64) {
	let usage = inner.usage;
	if reset_wall_clock || inner.wall.active_goal_id.as_deref() != Some(goal.id.as_str()) {
		inner.wall = WallClock { last_accounted_at: now, active_goal_id: Some(goal.id.clone()) };
	}
	if let Some(turn) = inner.turn.as_mut() {
		turn.active_goal_id = Some(goal.id.clone());
		turn.baseline = usage;
	}
}

fn clear_active_accounting(inner: &mut Inner, now: u64) {
	inner.wall = WallClock { last_accounted_at: now, active_goal_id: None };
	if let Some(turn) = inner.turn.as_mut() {
		turn.active_goal_id = None;
	}
}

fn commit_locked(
	inner: &mut Inner,
	state: Option<GoalModeState>,
	persist: Option<&'static str>,
	emit: bool,
) {
	inner.state = state;
	if let Some(mode) = persist {
		queue_mode_change(inner, mode);
	}
	if emit {
		let goal = inner.state.as_ref().map(|state| state.goal.clone());
		let snapshot = inner.state.clone();
		inner.events.push(GoalRuntimeEvent::Updated { goal, state: snapshot });
	}
}

fn queue_mode_change(inner: &mut Inner, mode: &'static str) {
	if mode == MODE_NONE {
		inner.pending.push(GoalPersist::ModeChange { mode, data: None });
		return;
	}
	let Some(state) = inner.state.as_ref() else {
		return;
	};
	match encode_mode_change_data(&state.goal) {
		Ok(data) => inner.pending.push(GoalPersist::ModeChange { mode, data: Some(data) }),
		Err(error) => eprintln!("warning: cannot persist goal mode: {error}"),
	}
}

/// TS `#flushUsageLocked`. Returns the limit prompt when a flip may steer.
fn flush_locked(
	inner: &mut Inner,
	now: u64,
	steering: GoalSteering,
	persist_wall_clock: bool,
) -> Option<String> {
	let state = inner.state.clone()?;
	if !state.enabled || !state.goal.status.is_accounting() {
		return None;
	}
	let usage = inner.usage;
	let turn_owns = inner
		.turn
		.as_ref()
		.is_some_and(|turn| turn.active_goal_id.as_deref() == Some(state.goal.id.as_str()));
	let wall_owns = inner.wall.active_goal_id.as_deref() == Some(state.goal.id.as_str());
	if !turn_owns && !wall_owns {
		return None;
	}
	let token_delta = if turn_owns {
		inner
			.turn
			.as_ref()
			.map_or(0, |turn| counters_delta(&usage, &turn.baseline))
	} else {
		0
	};
	let wall_seconds = if wall_owns {
		now.saturating_sub(inner.wall.last_accounted_at) / 1000
	} else {
		0
	};
	if token_delta == 0 && wall_seconds == 0 {
		return None;
	}

	let mut goal = state.goal;
	goal.tokens_used = goal.tokens_used.saturating_add(token_delta);
	goal.time_used_seconds = goal.time_used_seconds.saturating_add(wall_seconds);
	goal.updated_at = now;
	let flipped = goal.status == GoalStatus::Active
		&& goal.token_budget.is_some_and(|budget| goal.tokens_used >= budget);
	if flipped {
		goal.status = GoalStatus::BudgetLimited;
	}
	if turn_owns
		&& let Some(turn) = inner.turn.as_mut()
	{
		turn.baseline = usage;
	}
	if wall_owns && wall_seconds > 0 {
		inner.wall.last_accounted_at =
			inner.wall.last_accounted_at.saturating_add(wall_seconds * 1000);
	}
	// Wall-clock-only flushes stay in memory unless the active runtime is
	// leaving: persisting them on every tool event bloats the log with full
	// objective snapshots.
	let should_persist = token_delta > 0 || flipped || (persist_wall_clock && wall_seconds > 0);
	let committed = GoalModeState { enabled: state.enabled, mode: state.mode, goal: goal.clone() };
	commit_locked(inner, Some(committed), should_persist.then_some(MODE_GOAL), true);

	if goal.status != GoalStatus::BudgetLimited {
		inner.budget_reported_for = None;
	}
	if steering == GoalSteering::Allowed
		&& flipped
		&& inner.budget_reported_for.as_deref() != Some(goal.id.as_str())
	{
		inner.budget_reported_for = Some(goal.id.clone());
		return Some(render_goal_prompt(GoalPromptKind::BudgetLimit, &goal));
	}
	None
}

/// `details.usage` of a delegation result, as a counter view. A missing or
/// malformed payload yields `None` (never a zero-filled count).
fn usage_counters_from_value(value: &Value) -> Option<TokenCounters> {
	let counters = TokenCounters {
		input:       value.get("input").and_then(Value::as_u64).unwrap_or(0),
		output:      value.get("output").and_then(Value::as_u64).unwrap_or(0),
		cache_read:  value.get("cacheRead").and_then(Value::as_u64).unwrap_or(0),
		cache_write: value.get("cacheWrite").and_then(Value::as_u64).unwrap_or(0),
	};
	(counters != TokenCounters::default()).then_some(counters)
}

fn token_usage_from_counters(usage: TokenCounters) -> pi_agent::TokenUsage {
	pi_agent::TokenUsage {
		input: usage.input,
		output: usage.output,
		cache_read: usage.cache_read,
		cache_write: usage.cache_write,
		total_tokens: usage
			.input
			.saturating_add(usage.output)
			.saturating_add(usage.cache_read)
			.saturating_add(usage.cache_write),
		reasoning_tokens: None,
	}
}


#[cfg(test)]
mod tests {
	use std::fs;
	use std::path::PathBuf;
	use std::sync::Arc;

	use serde_json::{Value, json};

	use super::*;
	use crate::session::{SessionOptions, fork_session};

	static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

	fn temp_dir() -> PathBuf {
		let unique = format!(
			"omp-cli-goal-{}-{}",
			std::process::id(),
			COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
		);
		let dir = std::env::temp_dir().join(unique);
		fs::create_dir_all(&dir).expect("temp dir");
		dir
	}

	fn options(dir: PathBuf, id: &str) -> SessionOptions {
		SessionOptions {
			enabled: true,
			cwd: PathBuf::from("/work"),
			model: Some("local/llama3".to_string()),
			provider: Some("local".to_string()),
			dir: Some(dir),
			id: Some(id.to_string()),
			continue_latest: false,
			no_title: false,
			parent_session: None,
		}
	}

	fn budgeted(status: GoalStatus, tokens_used: u64) -> Goal {
		Goal {
			id: "g1".to_string(),
			objective: "ship".to_string(),
			status,
			token_budget: Some(50_000),
			tokens_used,
			time_used_seconds: 30,
			created_at: 1_761_792_000_000,
			updated_at: 1_761_792_000_100,
		}
	}

	/// Persistence and restore use the active branch, not the journal tail: an
	/// abandoned `none` after a goal must not clear it, and a pause on the new
	/// branch is what reopens and forks see.
	#[test]
	fn goal_mode_persists_and_restores_from_the_active_branch() {
		let dir = temp_dir();
		let file;
		let abandoned_none;
		{
			let mut store = SessionStore::open(&options(dir.clone(), "goal-mode"));
			assert!(store.enabled(), "store: {:?}", store.corrupt_reason());
			store.append_mode_change(MODE_NONE, None).expect("root mode");
			let root = store.branch_entries().first().copied().expect("root entry");
			assert_eq!(root.extra, json!({"mode": "none"}), "`none` carries no data");

			let active = budgeted(GoalStatus::Active, 1_200);
			let goal_data = encode_mode_change_data(&active).expect("encode active");
			let goal_entry = store
				.append_mode_change(MODE_GOAL, Some(goal_data))
				.expect("persist goal");
			let goal_persisted = store
				.branch_entries()
				.into_iter()
				.find(|entry| entry.id == goal_entry)
				.expect("goal entry");
			assert_eq!(
				goal_persisted.extra.get("mode").and_then(Value::as_str),
				Some(MODE_GOAL)
			);
			assert_eq!(
				goal_persisted
					.extra
					.get("data")
					.and_then(|data| data.get("goal"))
					.and_then(|goal| goal.get("tokensUsed"))
					.and_then(Value::as_u64),
				Some(1_200),
				"the goal payload keeps its TS field names"
			);
			assert_eq!(
				plan_restore(&store, true),
				RestorePlan::Restore { enabled: true, goal: active.clone() }
			);

			// Abandoned later: the whole-file last mode says `none`.
			abandoned_none = store.append_mode_change(MODE_NONE, None).expect("abandoned none");
			assert_eq!(plan_restore(&store, true), RestorePlan::Idle);

			// Branching back before that entry drops it from the active path.
			store.branch_to(&goal_entry).expect("branch to goal");
			assert_eq!(
				plan_restore(&store, true),
				RestorePlan::Restore { enabled: true, goal: active }
			);

			let paused = budgeted(GoalStatus::Paused, 1_200);
			store
				.append_mode_change(
					MODE_GOAL_PAUSED,
					Some(encode_mode_change_data(&paused).expect("encode paused")),
				)
				.expect("persist pause");
			file = store.file().expect("session file").to_path_buf();
		}

		let reopened = SessionStore::open(&options(dir.clone(), "goal-mode"));
		assert!(reopened.enabled());
		assert!(
			reopened.branch_entries().iter().all(|entry| entry.id != abandoned_none),
			"the abandoned mode is off the active branch"
		);
		let paused = budgeted(GoalStatus::Paused, 1_200);
		assert_eq!(
			plan_restore(&reopened, true),
			RestorePlan::Restore { enabled: false, goal: paused.clone() }
		);

		// Fork copies the log; the files then diverge independently.
		let fork_dir = temp_dir();
		let fork_file = fork_session(&file, None, Some(&fork_dir)).expect("fork session");
		let fork_stem = fork_file
			.file_stem()
			.and_then(std::ffi::OsStr::to_str)
			.expect("fork stem")
			.to_string();
		let mut forked = SessionStore::open(&options(fork_dir.clone(), &fork_stem));
		assert!(forked.enabled(), "forked session: {:?}", forked.corrupt_reason());
		assert_eq!(
			plan_restore(&forked, true),
			RestorePlan::Restore { enabled: false, goal: paused.clone() }
		);
		forked.append_mode_change(MODE_NONE, None).expect("clear in fork");
		assert_eq!(plan_restore(&forked, true), RestorePlan::Idle);

		let mut parent = SessionStore::open(&options(dir.clone(), "goal-mode"));
		assert_eq!(
			plan_restore(&parent, true),
			RestorePlan::Restore { enabled: false, goal: paused },
			"a fork clear never touches the parent log"
		);

		// The terminal metric follows `none` and never restores state.
		let completed = budgeted(GoalStatus::Complete, 1_200);
		let completed_data = encode_completed_data(&completed);
		parent.append_mode_change(MODE_NONE, None).expect("exit mode");
		parent.append_goal_completed(completed_data.clone()).expect("completed metric");
		assert_eq!(plan_restore(&parent, true), RestorePlan::Idle);
		let entries = parent.branch_entries();
		let metric = entries.last().expect("metric entry");
		assert_eq!(metric.kind, "custom");
		assert_eq!(
			metric.extra.get("customType").and_then(Value::as_str),
			Some(GOAL_COMPLETED_CUSTOM_TYPE)
		);
		assert_eq!(metric.extra.get("data"), Some(&completed_data));
		assert_eq!(
			completed_data,
			json!({
				"objective": "ship",
				"tokensUsed": 1_200,
				"tokenBudget": 50_000,
				"timeUsedSeconds": 30
			})
		);

		fs::remove_dir_all(dir).ok();
		fs::remove_dir_all(fork_dir).ok();
	}

	/// Write-boundary rejections, tolerated read edges and restore clearing are
	/// consumer-visible, so they are pinned.
	#[test]
	fn goal_write_and_read_edges_match_the_ts_contract() {
		let mut goal = budgeted(GoalStatus::Active, 0);
		goal.token_budget = Some(0);
		assert_eq!(
			encode_mode_change_data(&goal),
			Err("goal token_budget must be a positive integer when provided".to_string())
		);
		goal.token_budget = None;
		assert!(encode_mode_change_data(&goal).is_ok(), "an unbudgeted goal is valid");

		goal.status = GoalStatus::Unknown;
		assert!(encode_mode_change_data(&goal).is_err(), "unknown status is read-only");
		goal.status = GoalStatus::Active;

		// Malformed payloads restore nothing rather than a zero-filled goal.
		assert_eq!(goal_from_mode_data(Some(&json!({"goal": {"id": 1, "tokensUsed": "x"}}))), None);
		assert_eq!(goal_from_mode_data(Some(&json!({"goal": {}}))), None);
		assert_eq!(goal_from_mode_data(None), None);
		let mut oversized = encode_mode_change_data(&goal).expect("valid goal");
		oversized["goal"]["tokensUsed"] = json!(1e30);
		assert_eq!(goal_from_mode_data(Some(&oversized)), None);

		let unbounded = goal_from_mode_data(Some(&json!({"goal": {
			"id": "g1",
			"objective": "ship",
			"status": "active",
			"tokenBudget": "50k",
			"tokensUsed": 1200,
			"timeUsedSeconds": 30,
			"createdAt": 1,
			"updatedAt": 2
		}})))
		.expect("a string budget is unbounded, not an error");
		assert_eq!(unbounded.token_budget, None);
		assert_eq!(unbounded.remaining_tokens(), None);

		let unknown = goal_from_mode_data(Some(&json!({"goal": {
			"id": "g1",
			"objective": "ship",
			"status": "hibernating",
			"tokensUsed": 1200,
			"timeUsedSeconds": 30,
			"createdAt": 1,
			"updatedAt": 2
		}})))
		.expect("an unknown status is tolerated on read");
		assert_eq!(unknown.status, GoalStatus::Unknown);
		assert!(!unknown.status.is_accounting(), "an unknown status never accrues");

		// Budget math: remaining clamps at zero, cache reads are not new work.
		let used = budgeted(GoalStatus::Active, 1_200);
		assert_eq!(used.remaining_tokens(), Some(48_800));
		assert_eq!(Goal { tokens_used: 60_000, ..used }.remaining_tokens(), Some(0));
		let baseline = Usage {
			input: 100,
			output: 40,
			cache_read: 900,
			cache_write: 10,
			..Usage::default()
		};
		let current = Usage {
			input: 160,
			output: 70,
			cache_read: 5_000,
			cache_write: 35,
			..Usage::default()
		};
		assert_eq!(goal_token_delta(&current, &baseline), 60 + 25 + 30);

		// Store-level plan: missing/`none` is idle, unusable data clears, a
		// disabled setting clears a stored goal.
		let dir = temp_dir();
		let mut store = SessionStore::open(&options(dir.clone(), "goal-edges"));
		assert!(store.enabled());
		assert_eq!(plan_restore(&store, true), RestorePlan::Idle, "no mode entry yet");

		store
			.append_mode_change(MODE_GOAL, Some(json!({"goal": {"id": 1}})))
			.expect("malformed goal mode");
		assert_eq!(plan_restore(&store, true), RestorePlan::ClearGoalMode);

		let valid = budgeted(GoalStatus::Active, 0);
		let valid_data = encode_mode_change_data(&valid).expect("encode valid");
		store
			.append_mode_change(MODE_GOAL, Some(valid_data))
			.expect("valid goal mode");
		assert_eq!(
			plan_restore(&store, true),
			RestorePlan::Restore { enabled: true, goal: valid }
		);
		assert_eq!(
			plan_restore(&store, false),
			RestorePlan::ClearGoalMode,
			"goal.enabled=false clears a stored goal"
		);

		fs::remove_dir_all(dir).ok();
	}

	fn clock_at(start: u64) -> (Arc<std::sync::atomic::AtomicU64>, GoalRuntime) {
		let now = Arc::new(std::sync::atomic::AtomicU64::new(start));
		let handle = Arc::clone(&now);
		let runtime = GoalRuntime::with_clock(Box::new(move || {
			handle.load(std::sync::atomic::Ordering::SeqCst)
		}));
		(now, runtime)
	}

	fn assistant_with_usage(input: u64, output: u64, cache_write: u64) -> pi_agent::Message {
		let mut message = pi_agent::Message::assistant("reply");
		message.usage = Some(pi_agent::TokenUsage {
			input,
			output,
			cache_write,
			total_tokens: input + output + cache_write,
			..pi_agent::TokenUsage::default()
		});
		message
	}

	fn seed(input: u64, output: u64) -> TokenCounters {
		TokenCounters { input, output, ..TokenCounters::default() }
	}

	/// The directive grammar is the user-visible contract: a wrong match spends
	/// (or fails to spend) a real token ceiling.
	#[test]
	fn turn_budget_parser_matches_the_ts_directive() {
		assert_eq!(parse_turn_budget("+500k"), Some(TurnBudget { total: 500_000, hard: false }));
		assert_eq!(parse_turn_budget("+2m!"), Some(TurnBudget { total: 2_000_000, hard: true }));
		assert_eq!(parse_turn_budget("+1.5k"), Some(TurnBudget { total: 1_500, hard: false }));
		assert_eq!(parse_turn_budget("do it +5!"), Some(TurnBudget { total: 5, hard: true }));
		assert_eq!(parse_turn_budget("+1K"), Some(TurnBudget { total: 1_000, hard: false }));
		// Boundaries, units and zero values.
		assert_eq!(parse_turn_budget("a+b"), None);
		assert_eq!(parse_turn_budget("+0"), None);
		assert_eq!(parse_turn_budget("+0.0k"), None);
		assert_eq!(parse_turn_budget("+12kpx"), None);
		assert_eq!(parse_turn_budget("price +1.5x"), None);
		assert_eq!(parse_turn_budget("v1+2"), None);
		assert_eq!(parse_turn_budget("+"), None);
		assert_eq!(parse_turn_budget("no directive"), None);
		// First directive wins.
		assert_eq!(parse_turn_budget("+1k then +2k"), Some(TurnBudget { total: 1_000, hard: false }));
	}

	/// Flip-once and steer-once are the load-bearing accounting rules: a goal
	/// must never be charged twice for one turn, auto-complete on pressure, or
	/// steer the limit prompt repeatedly.
	#[test]
	fn goal_accounting_flips_budget_once_and_steers_once() {
		let steers = Arc::new(std::sync::Mutex::new(Vec::new()));
		let sink = Arc::clone(&steers);
		let (_, runtime) = clock_at(1_000);
		runtime.set_steer(Some(Arc::new(move |text: &str| {
			sink.lock().expect("steers").push(text.to_string());
			true
		})));
		let created = runtime.create_goal("ship", Some(100)).expect("create");
		assert_eq!(created.goal.status, GoalStatus::Active);
		assert_eq!(runtime.take_events().len(), 1, "create emits goal_updated");
		assert_eq!(
			runtime.take_pending(),
			vec![GoalPersist::ModeChange {
				mode: MODE_GOAL,
				data: Some(json!({"goal": {
					"id": created.goal.id,
					"objective": "ship",
					"status": "active",
					"tokenBudget": 100,
					"tokensUsed": 0,
					"timeUsedSeconds": 0,
					"createdAt": 1_000,
					"updatedAt": 1_000
				}})),
			}],
			"create persists the goal mode"
		);

		runtime.begin_turn(Some("work"), seed(0, 0));
		runtime.observe_message(&assistant_with_usage(10, 60, 5));
		runtime.observe_tool_end("read", None);
		let after_first = runtime.state().expect("state");
		assert_eq!(after_first.goal.tokens_used, 70 + 5, "input+cacheWrite+output");
		assert_eq!(after_first.goal.status, GoalStatus::Active, "under budget");
		assert!(steers.lock().expect("steers").is_empty());
		assert_eq!(runtime.take_events().len(), 1, "usage flush emits");

		runtime.observe_message(&assistant_with_usage(5, 40, 0));
		runtime.observe_tool_end("read", None);
		let limited = runtime.state().expect("state");
		assert_eq!(limited.goal.tokens_used, 120);
		assert_eq!(limited.goal.status, GoalStatus::BudgetLimited);
		assert_eq!(
			limited.goal.status,
			GoalStatus::BudgetLimited,
			"budget pressure never completes the goal"
		);
		let sent = steers.lock().expect("steers").clone();
		assert_eq!(sent.len(), 1, "one limit steer per goal");
		assert!(sent[0].contains("Active goal token budget reached"));
		assert!(runtime.take_pending().iter().any(|intent| matches!(
			intent,
			GoalPersist::ModeChange { mode: MODE_GOAL, .. }
		)));

		// More spend after the flip must not steer again or accrue output twice.
		runtime.observe_message(&assistant_with_usage(0, 30, 0));
		runtime.observe_tool_end("read", None);
		assert_eq!(runtime.state().expect("state").goal.tokens_used, 150);
		assert_eq!(steers.lock().expect("steers").len(), 1);
	}

	/// A background child's summed usage is charged once per job id, whether it
	/// arrives through auto-delivery or a hub `jobs` snapshot.
	#[test]
	fn background_usage_counts_once_across_delivery_and_hub_snapshot() {
		let runtime = GoalRuntime::with_clock(Box::new(|| 1_000));
		runtime.create_goal("ship", None).expect("create goal");
		runtime.begin_turn(Some("go"), TokenCounters::default());
		let usage: pi_agent::TokenUsage = serde_json::from_value(json!({
			"input": 4,
			"output": 6,
			"cacheRead": 1,
			"cacheWrite": 0,
			"totalTokens": 11,
		}))
		.expect("token usage");
		runtime.observe_background_result("bg-1", None, Some(&usage));
		let pending = runtime.take_pending();
		assert!(
			pending.iter().any(|intent| matches!(
				intent,
				GoalPersist::BackgroundUsage { id, .. } if id == "bg-1"
			)),
			"an auto-delivered result persists its usage once"
		);
		assert_eq!(
			runtime.state().expect("goal state").goal.tokens_used,
			10,
			"input + output; cacheRead is a reused prefix"
		);

		let hub = json!({
			"op": "jobs",
			"jobs": [{ "id": "bg-1", "usage": {
				"input": 4,
				"output": 6,
				"cacheRead": 1,
				"cacheWrite": 0,
				"totalTokens": 11,
			} }],
		});
		runtime.observe_tool_end("hub", Some(&hub));
		assert_eq!(
			runtime.state().expect("goal state").goal.tokens_used,
			10,
			"the same job id is never charged twice"
		);

		let second = json!({
			"jobs": [{ "id": "bg-2", "usage": {
				"input": 1,
				"output": 2,
				"cacheRead": 0,
				"cacheWrite": 0,
				"totalTokens": 3,
			} }],
		});
		runtime.observe_tool_end("hub", Some(&second));
		assert_eq!(
			runtime.state().expect("goal state").goal.tokens_used,
			13,
			"a new job id accrues normally"
		);
		let pending = runtime.take_pending();
		assert!(
			pending.iter().any(|intent| matches!(
				intent,
				GoalPersist::BackgroundUsage { id, .. } if id == "bg-2"
			)),
			"a hub-only charge must queue BackgroundUsage so reopen can seed it"
		);
	}

	/// The durable key is `resultKey` when present, else the raw id. A hub
	/// snapshot and an auto-delivery share one key, a report without usage never
	/// claims one, and two runs that reused `task-1` stay distinct.
	#[test]
	fn background_usage_keys_namespaced_runs_once() {
		let runtime = GoalRuntime::with_clock(Box::new(|| 1_000));
		runtime.create_goal("ship", None).expect("create goal");
		runtime.begin_turn(Some("go"), TokenCounters::default());
		let usage: pi_agent::TokenUsage = serde_json::from_value(json!({
			"input": 4,
			"output": 6,
			"cacheRead": 1,
			"cacheWrite": 0,
			"totalTokens": 11,
		}))
		.expect("token usage");

		// A snapshot keyed without usage must not suppress the settled report.
		runtime.observe_tool_end(
			"hub",
			Some(&json!({
				"jobs": [{ "id": "task-1", "resultKey": "ns1:task-1", "status": "running" }],
			})),
		);
		runtime.observe_background_result("task-1", Some("ns1:task-1"), None);
		assert_eq!(runtime.state().expect("goal state").goal.tokens_used, 0);

		runtime.observe_background_result("task-1", Some("ns1:task-1"), Some(&usage));
		assert_eq!(runtime.state().expect("goal state").goal.tokens_used, 10);

		// The hub snapshot repeats the same key with usage: still once.
		runtime.observe_tool_end(
			"hub",
			Some(&json!({
				"jobs": [{
					"id": "task-1",
					"resultKey": "ns1:task-1",
					"usage": {
						"input": 4,
						"output": 6,
						"cacheRead": 1,
						"cacheWrite": 0,
						"totalTokens": 11,
					},
				}],
			})),
		);
		assert_eq!(
			runtime.state().expect("goal state").goal.tokens_used,
			10,
			"one key is charged once across delivery and snapshot"
		);

		// A reopened CLI reuses `task-1` under a new namespace.
		runtime.observe_background_result("task-1", Some("ns2:task-1"), Some(&usage));
		assert_eq!(
			runtime.state().expect("goal state").goal.tokens_used,
			20,
			"distinct namespaces accrue separately"
		);
		let keys: Vec<Option<String>> = runtime
			.take_pending()
			.into_iter()
			.filter_map(|intent| match intent {
				GoalPersist::BackgroundUsage { id, result_key, .. } if id == "task-1" => {
					Some(result_key)
				},
				_ => None,
			})
			.collect();
		assert_eq!(
			keys,
			vec![
				Some("ns1:task-1".to_string()),
				Some("ns2:task-1".to_string())
			],
			"each persisted entry keeps its namespaced key beside the legacy id"
		);
	}

	/// A blocking `task` result reports one `{resultKey, usage}` entry per child.
	/// The ledger hands the same child over separately, so both orders must
	/// charge it once — and a batch that only partly overlaps what was already
	/// charged adds the unseen child's own counters, never the aggregate.
	#[test]
	fn task_details_accounting_charges_only_unseen_children() {
		let runtime = GoalRuntime::with_clock(Box::new(|| 1_000));
		runtime.create_goal("ship", None).expect("create goal");
		runtime.begin_turn(Some("go"), TokenCounters::default());
		let usage: pi_agent::TokenUsage = serde_json::from_value(json!({
			"input": 4,
			"output": 6,
			"cacheRead": 0,
			"cacheWrite": 0,
			"totalTokens": 10,
		}))
		.expect("token usage");

		// Details first: each entry claims its own key.
		runtime.observe_tool_end(
			"task",
			Some(&json!({
				"usage": {
					"input": 8,
					"output": 12,
					"cacheRead": 0,
					"cacheWrite": 0,
					"totalTokens": 20,
				},
				"accounting": [
					{ "resultKey": "ns1:task-1", "usage": {
						"input": 4, "output": 6, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 10,
					}},
					{ "resultKey": "ns1:task-2", "usage": {
						"input": 4, "output": 6, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 10,
					}},
				],
			})),
		);
		assert_eq!(runtime.state().expect("goal state").goal.tokens_used, 20);
		runtime.observe_background_result("task-1", Some("ns1:task-1"), Some(&usage));
		runtime.observe_background_result("task-2", Some("ns1:task-2"), Some(&usage));
		assert_eq!(
			runtime.state().expect("goal state").goal.tokens_used,
			20,
			"the ledger entries for the quoted batch add nothing"
		);

		// Partial overlap: task-9 is already charged, so only task-10's own 2
		// counts — the 12 aggregate would double-charge task-9.
		runtime.observe_background_result("task-9", Some("ns1:task-9"), Some(&usage));
		assert_eq!(runtime.state().expect("goal state").goal.tokens_used, 30);
		runtime.observe_tool_end(
			"task",
			Some(&json!({
				"usage": {
					"input": 5,
					"output": 7,
					"cacheRead": 0,
					"cacheWrite": 0,
					"totalTokens": 12,
				},
				"accounting": [
					{ "resultKey": "ns1:task-9", "usage": {
						"input": 4, "output": 6, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 10,
					}},
					{ "resultKey": "ns1:task-10", "usage": {
						"input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 2,
					}},
				],
			})),
		);
		assert_eq!(
			runtime.state().expect("goal state").goal.tokens_used,
			32,
			"the already-charged child is not charged again"
		);
	}

	/// A legacy payload with no per-child identity is still charged as one batch,
	/// and one without usage charges nothing.
	#[test]
	fn task_details_without_accounting_charges_legacy_aggregate_once() {
		let runtime = GoalRuntime::with_clock(Box::new(|| 1_000));
		runtime.create_goal("ship", None).expect("create goal");
		runtime.begin_turn(Some("go"), TokenCounters::default());
		runtime.observe_tool_end(
			"task",
			Some(&json!({
				"usage": {
					"input": 4,
					"output": 6,
					"cacheRead": 0,
					"cacheWrite": 0,
					"totalTokens": 10,
				},
			})),
		);
		assert_eq!(runtime.state().expect("goal state").goal.tokens_used, 10);
		runtime.observe_tool_end("task", Some(&json!({ "accounting": [] })));
		assert_eq!(
			runtime.state().expect("goal state").goal.tokens_used,
			10,
			"an empty keyed batch contributes nothing"
		);
	}

	/// Hub-only charge that was already persisted must not re-accrue when the
	/// same key arrives again after the writer retires the transient entry and
	/// re-seeds from the durable log.
	#[test]
	fn hub_only_duplicate_after_persist_charges_once() {
		let runtime = GoalRuntime::with_clock(Box::new(|| 1_000));
		runtime.create_goal("ship", None).expect("create goal");
		runtime.begin_turn(Some("go"), TokenCounters::default());
		let _ = runtime.take_pending();

		let hub = json!({
			"jobs": [{
				"id": "task-1",
				"resultKey": "ns1:task-1",
				"usage": {
					"input": 4,
					"output": 6,
					"cacheRead": 0,
					"cacheWrite": 0,
					"totalTokens": 10,
				},
			}],
		});
		runtime.observe_tool_end("hub", Some(&hub));
		assert_eq!(runtime.state().expect("goal").goal.tokens_used, 10);
		let pending = runtime.take_pending();
		assert!(
			pending.iter().any(|intent| matches!(
				intent,
				GoalPersist::BackgroundUsage {
					result_key: Some(key),
					..
				} if key == "ns1:task-1"
			)),
			"hub-only must queue durable BackgroundUsage"
		);

		// Writer append succeeded: retire moves the key into the durable seed.
		runtime.retire_durable_background_keys(["ns1:task-1"]);
		assert_eq!(runtime.background_dedup_len(), 0);
		assert_eq!(runtime.durable_background_dedup_len(), 1);
		// Next turn would re-seed from the retained log scan.
		runtime.seed_background_dedup(
			["ns1:task-1".to_string()].into_iter().collect(),
		);
		assert_eq!(runtime.background_dedup_len(), 0);
		assert_eq!(runtime.durable_background_dedup_len(), 1);

		runtime.observe_tool_end("hub", Some(&hub));
		runtime.observe_background_result(
			"task-1",
			Some("ns1:task-1"),
			Some(&serde_json::from_value(json!({
				"input": 4, "output": 6, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 10,
			})).expect("usage")),
		);
		assert_eq!(
			runtime.state().expect("goal").goal.tokens_used,
			10,
			"durable seed suppresses hub/delivery duplicates"
		);
		assert!(
			runtime.take_pending().iter().all(|intent| {
				!matches!(
					intent,
					GoalPersist::BackgroundUsage {
						result_key: Some(key),
						..
					} if key == "ns1:task-1"
				)
			}),
			"no second BackgroundUsage for an already-durable key"
		);
	}

	/// Reopen path: begin_turn clears, seed from durable keys, same resultKey
	/// never charges again.
	#[test]
	fn reopen_seed_charges_exact_once() {
		let runtime = GoalRuntime::with_clock(Box::new(|| 1_000));
		runtime.create_goal("ship", None).expect("create goal");
		let _ = runtime.take_pending();
		runtime.begin_turn(Some("go"), TokenCounters::default());
		runtime.seed_background_dedup(
			["ns1:task-1".to_string()].into_iter().collect(),
		);
		assert_eq!(runtime.background_dedup_len(), 0);
		assert_eq!(runtime.durable_background_dedup_len(), 1);

		let usage: pi_agent::TokenUsage = serde_json::from_value(json!({
			"input": 4, "output": 6, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 10,
		}))
		.expect("usage");
		runtime.observe_background_result("task-1", Some("ns1:task-1"), Some(&usage));
		assert_eq!(runtime.state().expect("goal").goal.tokens_used, 0);
		assert!(
			!runtime.take_pending().iter().any(|intent| matches!(
				intent,
				GoalPersist::BackgroundUsage { .. }
			)),
			"seeded durable key must not re-persist"
		);

		runtime.observe_background_result("task-2", Some("ns1:task-2"), Some(&usage));
		assert_eq!(runtime.state().expect("goal").goal.tokens_used, 10);
	}

	/// Long sequential goal: after each successful append retirement the
	/// transient set does not keep every historical key — only the durable seed
	/// (empty here) plus in-flight keys.
	#[test]
	fn long_sequential_goal_transient_set_not_monotonic() {
		let runtime = GoalRuntime::with_clock(Box::new(|| 1_000));
		runtime.create_goal("ship", None).expect("create goal");
		let _ = runtime.take_pending();
		runtime.begin_turn(Some("go"), TokenCounters::default());
		assert_eq!(runtime.background_dedup_len(), 0);

		let mut peak = 0usize;
		for i in 0..32u64 {
			let key = format!("ns1:task-{i}");
			let usage: pi_agent::TokenUsage = serde_json::from_value(json!({
				"input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 2,
			}))
			.expect("usage");
			runtime.observe_background_result(
				&format!("task-{i}"),
				Some(key.as_str()),
				Some(&usage),
			);
			peak = peak.max(runtime.background_dedup_len());
			// Successful append: retire the key that just became durable.
			let _ = runtime.take_pending();
			runtime.retire_durable_background_keys([key]);
			assert_eq!(
				runtime.background_dedup_len(),
				0,
				"after retirement no historical key remains in the in-flight set"
			);
		}
		assert!(
			peak <= 1,
			"transient set must not grow with lifetime children; peak was {peak}"
		);
		assert_eq!(
			runtime.state().expect("goal").goal.tokens_used,
			64,
			"each sequential child still accrues exactly once"
		);
	}

	/// Partial overlapping batch still charges only unseen children, and each
	/// fresh key queues its own BackgroundUsage for durable append.
	#[test]
	fn partial_overlap_batch_queues_only_unseen_background_usage() {
		let runtime = GoalRuntime::with_clock(Box::new(|| 1_000));
		runtime.create_goal("ship", None).expect("create goal");
		runtime.begin_turn(Some("go"), TokenCounters::default());
		let _ = runtime.take_pending();

		let usage: pi_agent::TokenUsage = serde_json::from_value(json!({
			"input": 4, "output": 6, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 10,
		}))
		.expect("usage");
		runtime.observe_background_result("task-9", Some("ns1:task-9"), Some(&usage));
		let _ = runtime.take_pending();

		runtime.observe_tool_end(
			"task",
			Some(&json!({
				"usage": {
					"input": 5, "output": 7, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 12,
				},
				"accounting": [
					{ "resultKey": "ns1:task-9", "usage": {
						"input": 4, "output": 6, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 10,
					}},
					{ "resultKey": "ns1:task-10", "usage": {
						"input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 2,
					}},
				],
			})),
		);
		assert_eq!(runtime.state().expect("goal").goal.tokens_used, 12);
		let pending = runtime.take_pending();
		let keys: Vec<_> = pending
			.iter()
			.filter_map(|intent| match intent {
				GoalPersist::BackgroundUsage {
					result_key: Some(key),
					..
				} => Some(key.as_str()),
				_ => None,
			})
			.collect();
		assert_eq!(
			keys,
			vec!["ns1:task-10"],
			"only the unseen child must queue durable BackgroundUsage"
		);
	}


	/// Transition guards and their exact wording, plus the exit handshake.
	#[test]
	fn goal_transitions_validate_and_hand_off_exit() {
		let (now, runtime) = clock_at(5_000);
		assert_eq!(
			runtime.create_goal("   ", None),
			Err("objective is required when op=create".to_string())
		);
		assert_eq!(
			runtime.create_goal("ship", Some(0)),
			Err("goal token_budget must be a positive integer when provided".to_string())
		);
		assert_eq!(runtime.resume_goal(), Err("No paused goal.".to_string()));
		assert_eq!(runtime.pause_goal(), None);
		assert_eq!(runtime.drop_goal(), None);
		assert_eq!(
			runtime.complete_goal_from_tool(),
			Err("cannot complete goal because no goal is active".to_string())
		);
		assert_eq!(
			runtime.replace_goal("ship", None),
			Err("cannot replace goal because no goal is active".to_string())
		);

		runtime.create_goal("ship", Some(50)).expect("create");
		assert_eq!(
			runtime.create_goal("again", None),
			Err("cannot create a new goal because this session already has a goal".to_string())
		);

		// Budget mutation flips with the new value and persists.
		now.store(6_000, std::sync::atomic::Ordering::SeqCst);
		runtime.take_events();
		runtime.take_pending();
		runtime.begin_turn(None, seed(0, 0));
		runtime.observe_message(&assistant_with_usage(0, 40, 0));
		runtime.observe_tool_end("read", None);
		assert_eq!(runtime.state().expect("state").goal.tokens_used, 40);
		runtime.take_pending();
		let mutated = runtime
			.on_budget_mutated(Some(10))
			.expect("budget mutation")
			.expect("state");
		assert_eq!(mutated.goal.status, GoalStatus::BudgetLimited);
		assert_eq!(mutated.goal.token_budget, Some(10));
		let pending = runtime.take_pending();
		assert_eq!(pending.len(), 1, "budget write is queued");
		assert!(matches!(pending[0], GoalPersist::ModeChange { mode: MODE_GOAL, .. }));
		let reset = runtime
			.on_budget_mutated(None)
			.expect("budget clear")
			.expect("state");
		assert_eq!(reset.goal.status, GoalStatus::Active, "clearing re-activates");
		assert_eq!(reset.goal.token_budget, None);
		runtime.take_pending();

		// Complete keeps the turn data, sets exiting, and the exit handshake is
		// the only path to the terminal metric.
		runtime.take_events();
		let completed = runtime.complete_goal_from_tool().expect("complete");
		assert_eq!(completed.status, GoalStatus::Complete);
		assert_eq!(runtime.state().expect("state").mode, GoalMode::Exiting);
		assert_eq!(
			runtime.complete_goal_from_tool(),
			Err("goal is already complete".to_string())
		);
		let exiting = runtime.take_exiting().expect("exiting goal");
		assert_eq!(exiting.objective, "ship");
		assert!(runtime.state().is_none());
		assert!(runtime.build_active_prompt().is_none());
		assert_eq!(
			encode_completed_data(&exiting),
			json!({
				"objective": "ship",
				"tokensUsed": 40,
				"timeUsedSeconds": 1
			})
		);

		// Drop emits the dropped goal, then clears mode with `none` and no metric.
		let dropped = runtime.create_goal("discard", Some(9)).expect("create");
		assert!(dropped.enabled);
		runtime.take_events();
		runtime.take_pending();
		let dropped_goal = runtime.drop_goal().expect("dropped");
		assert_eq!(dropped_goal.status, GoalStatus::Dropped);
		let events = runtime.take_events();
		assert_eq!(events.len(), 1, "drop emits the dropped goal");
		assert!(matches!(
			&events[0],
			GoalRuntimeEvent::Updated { goal: Some(goal), state: Some(state) }
				if goal.status == GoalStatus::Dropped && !state.enabled
		));
		assert_eq!(
			runtime.take_pending(),
			vec![GoalPersist::ModeChange { mode: MODE_NONE, data: None }],
			"drop writes no terminal metric"
		);
		assert!(!runtime.has_goal());
	}

	/// An interrupt pauses the active goal exactly once; a pause replay and a
	/// resume rebase accounting without inventing usage.
	#[test]
	fn goal_interrupt_pauses_once_and_resume_rebases() {
		let (now, runtime) = clock_at(9_000);
		runtime.create_goal("ship", None).expect("create");
		// The create transition is its own persisted write; the session writer
		// drains it before the turn, so the abort sequence starts clean.
		runtime.take_events();
		runtime.take_pending();
		runtime.begin_turn(Some("go"), seed(100, 10));
		runtime.observe_message(&assistant_with_usage(0, 25, 0));
		now.store(9_600, std::sync::atomic::Ordering::SeqCst);
		runtime.on_task_aborted(GoalAbortReason::Interrupted);

		let paused = runtime.state().expect("state");
		assert!(!paused.enabled);
		assert_eq!(paused.goal.status, GoalStatus::Paused);
		assert_eq!(paused.goal.tokens_used, 25, "flush precedes the pause");
		assert_eq!(paused.goal.time_used_seconds, 0, "sub-second floor");
		let pending = runtime.take_pending();
		assert!(
			matches!(
				pending.last(),
				Some(GoalPersist::ModeChange { mode: MODE_GOAL_PAUSED, .. })
			),
			"the pause is the last persisted transition"
		);
		assert_eq!(
			pending
				.iter()
				.filter(|intent| matches!(
					intent,
					GoalPersist::ModeChange { mode: MODE_GOAL_PAUSED, .. }
				))
				.count(),
			1,
			"an interrupt pauses exactly once"
		);

		// A second abort adds nothing: the goal is no longer active.
		runtime.on_task_aborted(GoalAbortReason::Interrupted);
		assert!(runtime.take_pending().is_empty());

		// Resume rebases: prior spend stays, new delta is measured from here.
		now.store(10_000, std::sync::atomic::Ordering::SeqCst);
		let resumed = runtime.resume_goal().expect("resume");
		assert!(resumed.enabled);
		assert_eq!(resumed.goal.status, GoalStatus::Active);
		assert_eq!(resumed.goal.tokens_used, 25);
		runtime.begin_turn(None, seed(100, 25));
		runtime.observe_message(&assistant_with_usage(0, 5, 0));
		runtime.observe_tool_end("read", None);
		assert_eq!(runtime.state().expect("state").goal.tokens_used, 30);
	}

	/// Restore semantics: an active goal replayed without a preserve is paused
	/// and persisted; a preserved one only rebases.
	#[test]
	fn goal_thread_resume_pauses_active_and_preserves_explicitly() {
		let (now, runtime) = clock_at(1);
		let mut goal = budgeted(GoalStatus::Active, 30);
		goal.token_budget = None;
		runtime.set_restored_state(Some(GoalModeState {
			enabled: true,
			mode:    GoalMode::Active,
			goal:    goal.clone(),
		}));
		now.store(2_000, std::sync::atomic::Ordering::SeqCst);
		let reconciled = runtime.on_thread_resumed(false).expect("state");
		assert!(!reconciled.enabled);
		assert_eq!(reconciled.goal.status, GoalStatus::Paused);
		assert_eq!(
			runtime.take_pending(),
			vec![GoalPersist::ModeChange {
				mode: MODE_GOAL_PAUSED,
				data: Some(json!({"goal": serde_json::to_value(&reconciled.goal).expect("goal")})),
			}]
		);

		runtime.set_restored_state(Some(GoalModeState {
			enabled: true,
			mode:    GoalMode::Active,
			goal:    goal.clone(),
		}));
		let preserved = runtime.on_thread_resumed(true).expect("state");
		assert!(preserved.enabled, "preserve keeps the goal live");
		assert_eq!(preserved.goal.status, GoalStatus::Active);
		assert!(runtime.take_pending().is_empty(), "preserve writes nothing");
	}

	/// `budget()` precedence: turn directive, then an enabled goal, then the
	/// advisory output delta. No snapshot invents a ceiling.
	#[test]
	fn budget_snapshot_precedence_and_eval_output() {
		let (_, runtime) = clock_at(1);
		// Advisory: no window ceiling, no goal.
		runtime.begin_turn(Some("nothing"), seed(0, 100));
		runtime.observe_message(&assistant_with_usage(0, 40, 0));
		assert_eq!(
			runtime.budget_snapshot(),
			BudgetSnapshot { total: None, spent: 40, hard: false }
		);

		// Goal budget wins over the advisory delta but not over a turn window.
		runtime.create_goal("ship", Some(1_000)).expect("create");
		runtime.begin_turn(None, seed(0, 140));
		assert_eq!(
			runtime.budget_snapshot(),
			BudgetSnapshot { total: Some(1_000), spent: 0, hard: true }
		);
		runtime.begin_turn(Some("go +2k!"), seed(0, 140));
		assert_eq!(
			runtime.budget_snapshot(),
			BudgetSnapshot { total: Some(2_000), spent: 0, hard: true }
		);
		runtime.observe_message(&assistant_with_usage(0, 60, 0));
		runtime.record_eval_subagent_output(200);
		assert_eq!(
			runtime.budget_snapshot(),
			BudgetSnapshot { total: Some(2_000), spent: 260, hard: true },
			"eval child output is part of the turn spend"
		);
	}

	/// Prompt rendering, escaping and the completion report are user-visible.
	#[test]
	fn goal_prompts_and_completion_report_render() {
		let mut goal = budgeted(GoalStatus::Active, 1_200);
		goal.objective = "ship <v1> & test".to_string();
		let active = render_goal_prompt(GoalPromptKind::Active, &goal);
		assert!(active.contains("ship &lt;v1&gt; &amp; test"));
		assert!(active.contains("Tokens used: 1200"));
		assert!(active.contains("Token budget: 50000"));
		assert!(active.contains("Tokens remaining: 48800"));

		let mut unbounded = goal.clone();
		unbounded.token_budget = None;
		let continuation = render_goal_prompt(GoalPromptKind::Continuation, &unbounded);
		assert!(continuation.contains("Token budget: none"));
		assert!(continuation.contains("Tokens remaining: unbounded"));

		let limit = render_goal_prompt(GoalPromptKind::BudgetLimit, &goal);
		assert!(limit.contains("Active goal token budget reached"));

		assert_eq!(
			completion_budget_report(&goal),
			Some(
				"Goal achieved. Report final budget usage to the user: tokens used: 1200 of \
				 50000; time used: 30 seconds."
					.to_string()
			)
		);
		let mut empty = goal;
		empty.token_budget = None;
		empty.time_used_seconds = 0;
		assert_eq!(completion_budget_report(&empty), None);

		// Todo context: counts, sanitizing, and omission when empty/unavailable.
		use pi_agent::tools::todo::{TodoItem, TodoPhase, TodoStatus};
		let phases = vec![TodoPhase {
			name: "Phase <1>".to_string(),
			tasks: vec![
				TodoItem {
					content: "done\titem".to_string(),
					status: TodoStatus::Completed,
					blocker: None,
				},
				TodoItem {
					content: "open\nitem".to_string(),
					status: TodoStatus::InProgress,
					blocker: None,
				},
			],
		}];
		let context = build_goal_todo_context(&phases, true).expect("context");
		assert!(context.contains("Overall: 1/2 done, 1 open."));
		assert!(context.contains("- Phase &lt;1&gt;"));
		assert!(context.contains("  - [in_progress] open\\nitem"));
		assert!(build_goal_todo_context(&phases, false).is_none());
		assert!(build_goal_todo_context(&[], true).is_none());
		assert_eq!(render_goal_mode_context("goal", None), "goal");
		assert_eq!(render_goal_mode_context("goal", Some("todo")), "goal\ntodo");
	}

	/// Child usage rides on the delegation result details and is added exactly
	/// once, even when the provider reported nothing for the root message.
	#[test]
	fn goal_child_usage_adds_once_and_never_zero_fills() {
		let (_, runtime) = clock_at(1);
		runtime.create_goal("ship", None).expect("create");
		runtime.begin_turn(None, seed(0, 0));
		runtime.take_pending();

		// An unreported provider request adds nothing.
		runtime.observe_message(&pi_agent::Message::assistant("no usage"));
		assert_eq!(runtime.state().expect("state").goal.tokens_used, 0);

		runtime.observe_tool_end(
			"task",
			Some(&json!({"usage": {"input": 7, "output": 30, "cacheRead": 900, "cacheWrite": 2}})),
		);
		assert_eq!(
			runtime.state().expect("state").goal.tokens_used,
			39,
			"child input+cacheWrite+output, cacheRead excluded"
		);
		// A malformed usage object is ignored, never coerced to zeros.
		runtime.observe_tool_end("task", Some(&json!({"usage": {"output": "lots"}})));
		assert_eq!(runtime.state().expect("state").goal.tokens_used, 39);
	}

	/// The wall clock accrues whole seconds only, and only while the goal owns
	/// the window; a wall-only flush is not persisted.
	#[test]
	fn goal_wall_clock_uses_whole_seconds() {
		let (now, runtime) = clock_at(1_000);
		runtime.create_goal("ship", None).expect("create");
		runtime.take_pending();
		runtime.begin_turn(None, seed(0, 0));
		now.store(3_999, std::sync::atomic::Ordering::SeqCst);
		runtime.on_agent_end();
		assert_eq!(runtime.state().expect("state").goal.time_used_seconds, 2);
		assert!(runtime.take_pending().is_empty(), "wall-only flush stays in memory");

		// Whole seconds only, with the sub-second remainder carried over.
		now.store(4_500, std::sync::atomic::Ordering::SeqCst);
		runtime.begin_turn(None, seed(0, 0));
		runtime.on_agent_end();
		assert_eq!(runtime.state().expect("state").goal.time_used_seconds, 3);
	}
}
