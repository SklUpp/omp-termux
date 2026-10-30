//! ACP plan mode: state, settle-driven reminder scheduling, proposal
//! resolution and write-guard classification.
//!
//! Ground truth is `session/agent-session.ts` (`#buildPlanModeMessage`
//! 5503-5554, `#enforcePlanModeDecisionAtSettle` 7670-7735, reminder resets at
//! 2821-2828/5230-5234/5749-5753), `plan-mode/state.ts`, and
//! `plan-mode/approved-plan.ts` / `plan-mode-guard.ts`.
//!
//! The reminder is a *deferred next-turn message*, not part of the prompt that
//! triggered it: the ACP prompt response resolves first, then the queued
//! continuation is drained when the client sends the next prompt
//! (`#scheduleAgentContinue` + `deferAgentInitiatedTurns`). [`PlanGate`] keeps
//! that pending message and the reminder bookkeeping; the runner-side guard and
//! `xd://propose` dispatch are wired by the pi-agent composition slice through
//! [`PlanGate::reminder_pending`], [`PlanGate::generation`] and
//! [`is_propose_tool_call`].

use serde_json::Value;

/// Plan mode id advertised by ACP.
pub const ACP_PLAN_MODE_ID: &str = "plan";
/// Default mode id advertised by ACP.
pub const ACP_DEFAULT_MODE_ID: &str = "default";
/// Default plan artifact when plan mode is first entered.
pub const DEFAULT_PLAN_FILE_URL: &str = "local://PLAN.md";
/// TS `PLAN_MODE_REMINDER_MAX`: after this many unanswered reminders the agent
/// yields to the user instead of looping.
pub const PLAN_MODE_REMINDER_MAX: u32 = 3;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PlanWorkflow {
	#[default]
	Parallel,
	Iterative,
}

impl PlanWorkflow {
	#[must_use]
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Parallel => "parallel",
			Self::Iterative => "iterative",
		}
	}
}

/// Persisted plan-mode state (TS `PlanModeState`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpPlanState {
	pub enabled: bool,
	pub plan_file_path: String,
	pub workflow: PlanWorkflow,
	pub reentry: bool,
}

impl AcpPlanState {
	#[must_use]
	pub fn enter(previous: Option<&Self>) -> Self {
		Self {
			enabled: true,
			plan_file_path: previous
				.map(|state| state.plan_file_path.clone())
				.unwrap_or_else(|| DEFAULT_PLAN_FILE_URL.to_string()),
			workflow: previous.map_or(PlanWorkflow::Parallel, |state| state.workflow),
			reentry: previous.is_some(),
		}
	}
}

/// Reminder bookkeeping at settle (TS `#planModeReminderCount` /
/// `#planModeReminderAwaitingProgress`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlanSettleState {
	pub reminder_count: u32,
	pub awaiting_progress: bool,
}

/// One assistant tool call observed in a settled turn.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanToolCall {
	pub name: String,
	pub args: Value,
}

/// Result of applying one settle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanSettle {
	/// Not in plan mode, or the settle needs no reminder.
	Noop,
	/// Schedule one deferred reminder continuation.
	Reminder(String),
	/// Decision tool seen; counters were reset.
	Decision,
	/// Reminder cap reached; yielded to the user.
	Yield,
}

/// Apply the settle-time decision rules. Ordering is load-bearing:
/// error/abort → no reset; decision → reset; any other tool call → no count;
/// already awaiting → no count; cap → yield; missing ask/write → no count.
pub fn observe_settle(
	state: Option<&AcpPlanState>,
	settle: &mut PlanSettleState,
	stop_reason: &str,
	tool_calls: &[PlanToolCall],
	has_ask: bool,
	has_write: bool,
) -> PlanSettle {
	if !state.is_some_and(|state| state.enabled) {
		return PlanSettle::Noop;
	}
	if stop_reason == "error" || stop_reason == "aborted" || stop_reason == "cancelled" {
		return PlanSettle::Noop;
	}
	if tool_calls.iter().any(is_decision_tool_call) {
		settle.reminder_count = 0;
		settle.awaiting_progress = false;
		return PlanSettle::Decision;
	}
	if !tool_calls.is_empty() {
		return PlanSettle::Noop;
	}
	if settle.awaiting_progress {
		return PlanSettle::Noop;
	}
	if settle.reminder_count >= PLAN_MODE_REMINDER_MAX {
		return PlanSettle::Yield;
	}
	if !has_ask || !has_write {
		return PlanSettle::Noop;
	}
	settle.reminder_count += 1;
	settle.awaiting_progress = true;
	PlanSettle::Reminder(plan_tool_decision_reminder("ask"))
}

/// Reset on a new user prompt / mode exit (TS 5230-5234, 5749-5753).
pub fn reset_reminders(settle: &mut PlanSettleState) {
	settle.reminder_count = 0;
	settle.awaiting_progress = false;
}

/// `tool_execution_end` bookkeeping: every end clears `awaiting_progress`,
/// while `ask` / `write xd://propose` also reset the count.
pub fn observe_tool_end(settle: &mut PlanSettleState, name: &str, args: Option<&Value>) {
	settle.awaiting_progress = false;
	if name == "ask" || args.is_some_and(|args| is_propose_tool_call(name, args)) {
		settle.reminder_count = 0;
	}
}

/// Restore plan mode from the shared `mode_change` slot. Only `"plan"` with a
/// string `planFilePath` restores; `"goal*"`, `"vibe"`, `"none"` leave plan
/// mode off (TS `#reconcileModeFromSession`).
#[must_use]
pub fn restore_from_mode_change(mode: &str, data: Option<&Value>) -> Option<AcpPlanState> {
	if mode != ACP_PLAN_MODE_ID {
		return None;
	}
	let plan_file_path = data?
		.get("planFilePath")
		.and_then(Value::as_str)
		.filter(|path| !path.is_empty())?;
	Some(AcpPlanState {
		enabled: true,
		plan_file_path: plan_file_path.to_string(),
		workflow: data
			.and_then(|data| data.get("workflow"))
			.and_then(Value::as_str)
			.and_then(|workflow| match workflow {
				"iterative" => Some(PlanWorkflow::Iterative),
				"parallel" => Some(PlanWorkflow::Parallel),
				_ => None,
			})
			.unwrap_or(PlanWorkflow::Parallel),
		reentry: true,
	})
}

/// Plan-mode proposal handler bookkeeping plus the deferred reminder message.
#[derive(Debug, Clone)]
pub struct PlanGate {
	pub state: Option<AcpPlanState>,
	pub settle: PlanSettleState,
	/// Bumped on every enter/exit; the runner guard compares this so a stale
	/// forced-tool requirement cannot leak onto an unrelated turn.
	pub generation: u64,
	/// `plan.enabled` config gate (TS `session.settings.get("plan.enabled")`).
	pub enabled: bool,
	/// Approved plan injected into the next turn's context
	/// (`setPlanReferencePath`); the plan-proposal slice reads this.
	plan_reference: Option<String>,
	pending_reminder: Option<String>,
}

impl PlanGate {
	#[must_use]
	pub fn new(enabled: bool) -> Self {
		Self {
			state: None,
			settle: PlanSettleState::default(),
			generation: 0,
			enabled,
			plan_reference: None,
			pending_reminder: None,
		}
	}

	#[must_use]
	pub fn current_mode_id(&self) -> &'static str {
		if self.state.as_ref().is_some_and(|state| state.enabled) {
			ACP_PLAN_MODE_ID
		} else {
			ACP_DEFAULT_MODE_ID
		}
	}

	/// ACP mode list: `default` always, `plan` only when configured.
	#[must_use]
	pub fn modes(&self) -> Value {
		let mut modes = vec![serde_json::json!({
			"id": ACP_DEFAULT_MODE_ID,
			"name": "Default",
			"description": "Standard ACP headless mode",
		})];
		if self.enabled {
			modes.push(serde_json::json!({
				"id": ACP_PLAN_MODE_ID,
				"name": "Plan",
				"description": "Read-only planning mode that drafts a plan to a markdown file before any code changes",
			}));
		}
		Value::Array(modes)
	}

	#[must_use]
	pub fn mode_state(&self) -> Value {
		serde_json::json!({
			"availableModes": self.modes(),
			"currentModeId": self.current_mode_id(),
		})
	}

	/// Apply `session/set_mode` / `mode` config option. `Err` is the exact
	/// user-facing message; the dispatcher turns it into `-32603`.
	pub fn set_mode(&mut self, mode_id: &str) -> Result<(), String> {
		if mode_id == ACP_DEFAULT_MODE_ID {
			self.state = None;
			reset_reminders(&mut self.settle);
			self.pending_reminder = None;
			self.generation = self.generation.wrapping_add(1);
			return Ok(());
		}
		if mode_id != ACP_PLAN_MODE_ID || !self.enabled {
			return Err(format!("Unsupported ACP mode: {mode_id}"));
		}
		self.state = Some(AcpPlanState::enter(self.state.as_ref()));
		reset_reminders(&mut self.settle);
		self.pending_reminder = None;
		self.generation = self.generation.wrapping_add(1);
		Ok(())
	}

	/// Build the plan context that precedes a turn while plan mode is active.
	#[must_use]
	pub fn prompt_prefix(
		&self,
		ask_available: bool,
		scout_available: bool,
		task_available: bool,
	) -> Option<String> {
		let state = self.state.as_ref().filter(|state| state.enabled)?;
		// `reentry` is the only native signal that a plan artifact already
		// existed when plan mode was entered; exact artifact resolution lives
		// in the plan/propose composition slice.
		let plan_exists = state.reentry;
		Some(plan_mode_message(
			state,
			PlanPromptContext {
				plan_exists,
				ask_available,
				scout_available,
				task_available,
				hashline_edit: false,
				write_tool_name: "write",
				edit_tool_name: "edit",
				ask_tool_name: "ask",
			},
		))
	}

	/// Drain the deferred reminder scheduled by the last settle. Called when the
	/// next client prompt starts, matching TS `#pendingNextTurnMessages`.
	#[must_use]
	pub fn take_reminder(&mut self) -> Option<String> {
		self.pending_reminder.take()
	}

	#[must_use]
	pub fn reminder_pending(&self) -> bool {
		self.pending_reminder.is_some()
	}

	/// Apply one settle and store its deferred reminder.
	pub fn settle_turn(
		&mut self,
		stop_reason: &str,
		tool_calls: &[PlanToolCall],
		has_ask: bool,
		has_write: bool,
	) -> PlanSettle {
		let outcome = observe_settle(
			self.state.as_ref(),
			&mut self.settle,
			stop_reason,
			tool_calls,
			has_ask,
			has_write,
		);
		if let PlanSettle::Reminder(message) = &outcome {
			self.pending_reminder = Some(message.clone());
		}
		outcome
	}

	/// Apply the approved-plan result: keep the reviewed path, clear plan mode,
	/// bump the generation so no forced requirement survives.
	pub fn exit_after_approval(&mut self, plan_file_path: &str) {
		self.state = None;
		reset_reminders(&mut self.settle);
		self.pending_reminder = None;
		self.plan_reference = Some(plan_file_path.to_string());
		self.generation = self.generation.wrapping_add(1);
	}

	/// Approved plan path the next turn composer injects as context.
	#[must_use]
	pub fn plan_reference(&self) -> Option<&str> {
		self.plan_reference.as_deref()
	}

	/// Mode-change payload for the shared session slot.
	///
	/// TS keeps one exclusive `mode_change` slot: plan entry writes `"plan"`
	/// with `{planFilePath}`, exit writes `"none"` (`interactive-mode.ts:3194,
	/// 3062`). Goal mode uses the same slot, and the two modes cannot coexist.
	#[must_use]
	pub fn persisted_mode_change(&self) -> (&'static str, Option<Value>) {
		match self.state.as_ref().filter(|state| state.enabled) {
			Some(state) => {
				(ACP_PLAN_MODE_ID, Some(serde_json::json!({ "planFilePath": state.plan_file_path })))
			},
			None => ("none", None),
		}
	}

	/// Keep plan mode active after a refinement request and promote the reviewed
	/// path so the next plan message targets it (TS `#handleAcpPlanProposal`).
	pub fn stay_after_refinement(&mut self, plan_file_path: &str) {
		if let Some(state) = self.state.as_mut() {
			state.plan_file_path = plan_file_path.to_string();
		}
	}
}

/// Inputs the plan-message template branches on.
#[derive(Debug, Clone, Copy)]
pub struct PlanPromptContext {
	pub plan_exists: bool,
	pub ask_available: bool,
	pub scout_available: bool,
	pub task_available: bool,
	pub hashline_edit: bool,
	pub write_tool_name: &'static str,
	pub edit_tool_name: &'static str,
	pub ask_tool_name: &'static str,
}

/// Render `prompts/system/plan-mode-active.md` with the context the Rust
/// surface can actually honour. Conditional sections collapse to the branch
/// that matches installed tools.
#[must_use]
pub fn plan_mode_message(state: &AcpPlanState, ctx: PlanPromptContext) -> String {
	let mut text = String::new();
	text.push_str(
		"<critical>\nPlan mode active.\n- Working tree/system read-only: NEVER create, edit, delete, or rename working-tree files; NEVER run state-changing commands (`git commit`, `npm install`, migrations) or otherwise change the system.\n- `local://`: session-local planning artifacts; MAY create/update only when explicitly requested or needed for the plan; NEVER delete/rename.\n- Canonical plan: MUST write `local://<slug>-plan.md`.\n\n",
	);
	text.push_str(&format!(
		"Implementing: write the plan `<slug>`/title, plain text, to `xd://propose` with `{}`; `<slug>` MUST match `local://<slug>-plan.md`, allowed characters: letters, numbers, underscores, hyphens. User then selects an execution option; full write access restored.\n\n",
		ctx.write_tool_name
	));
	text.push_str(&format!(
		"NEVER ask user to exit plan mode or request approval in prose/with `{}`; approval ONLY via `xd://propose` write.\n</critical>\n\n## What a plan is\n\nPlan: execution spec, not design doc. Approval may clear/compact the conversation; another engineer/fresh agent implements solely from the file. A competent implementer unfamiliar with the conversation MUST execute top-to-bottom with ZERO design decisions; file contains every choice.\n\nDetail removes implementer decisions, not padding. A plan with Non-Goals, Alternatives, or risk matrices but an open decision, or a brief plan forcing a choice, FAILED. Decision-completeness > brevity.\n\n## Plan file\n\n",
		ctx.ask_tool_name
	));
	if ctx.plan_exists {
		text.push_str(&format!(
			"Existing plan: `{}`; read, incrementally update with `{}`. Different task → retain it; create `local://<slug>-plan.md`.\n\n",
			state.plan_file_path, ctx.edit_tool_name
		));
	} else {
		text.push_str(&format!(
			"Choose short kebab-case task `<slug>`; create `local://<slug>-plan.md` (e.g. `local://auth-token-refresh-plan.md`). File NEVER renamed on approval; submit this same `<slug>` to `xd://propose` for approval.\n\n",
		));
	}
	text.push_str(&format!(
		"`{}`: incremental edits only. `{}`: create/full replacement only. MUST record findings as learned; NEVER defer all writing to the end.\n\n",
		ctx.edit_tool_name, ctx.write_tool_name
	));
	if ctx.hashline_edit {
		text.push_str("## Hashline edit\n\nUse `##`/`###` sections.\n\n");
	}
	text.push_str("## Ground every claim\n\nResolve unknowns by discovery, not questions.\n\n");
	text.push_str("- Discoverable facts — locations, behavior, signatures, configs: MUST discover with `glob`, `grep`, `read`");
	if ctx.scout_available && ctx.task_available {
		text.push_str(", or parallel `scout` subagents (via `task`)");
	}
	text.push_str(". Every asserted path, symbol, signature, behavior: actually read this session. Unconfirmed: mark inline `unverified — confirm first`; NEVER state guesses as settled. Ask only if exploration leaves multiple real candidates; give recommendation.\n");
	text.push_str("- Preferences/tradeoffs — intent, UX, scope edges, performance vs. simplicity: not code-derivable. ");
	if ctx.ask_available {
		text.push_str(&format!(
			"Ask early via `{}`: 2–4 mutually exclusive options + recommended default. Unanswered → use default; record under Assumptions.\n\n",
			ctx.ask_tool_name
		));
	} else {
		text.push_str("Record as Assumptions with a recommended default and proceed — a prose question cannot end the turn. Unanswered → use default; record under Assumptions.\n\n");
	}
	text.push_str("Every question MUST alter plan or resolve load-bearing choice; batch. NEVER ask what exploration answers or filler.\n\n");
	if state.reentry {
		text.push_str(&format!(
			"## Re-entry\n\nNew request primary; existing plan reference only. NEVER reconcile old plan while dropping new request.\n\n<procedure>\n1. Read new request; plan it this turn.\n2. Read existing plan only as reference.\n3. Continuing same task → update with `{}`, delete outdated sections. Different task → retain old plan; create fresh `local://<slug>-plan.md`.\n4. If unfinished/broken old work is required by new request, incorporate corrections INTO new plan; combine, NEVER replace new request with old fix.\n5. Decision-complete new request → call `resolve` with `action: \"apply\"` and `extra: {{ title }}`.\n</procedure>\n\n",
			ctx.edit_tool_name
		));
	}
	if state.workflow == PlanWorkflow::Iterative {
		text.push_str(&format!(
			"## Workflow — iterative\n\n<procedure>\n1. **Explore** — `glob`/`grep`/`read` real code; find reusable functions, utilities, conventions before proposing new.\n2. **Interview** — {}3. **Update** — revise plan with `{}` while learning.\n4. **Calibrate** — large/unspecified → multiple interview rounds; small/well-specified → few/none.\n</procedure>\n\n",
			if ctx.ask_available {
				format!("`{}` only for preferences/tradeoffs; batch; NEVER ask what exploration answers.\n", ctx.ask_tool_name)
			} else {
				"record preferences/tradeoffs as Assumptions with a recommended default; NEVER ask what exploration answers.\n".to_string()
			},
			ctx.edit_tool_name
		));
	} else {
		text.push_str(&format!(
			"## Workflow — parallel\n\n<procedure>\n1. **Understand** — request and supporting code.{} Find reusable code before proposing new.\n2. **Design** — draft approach from findings, briefly weigh tradeoffs, commit. Large/cross-cutting → MAY spawn critique subagent before commitment.\n3. **Review** — read intended files; validate approach against code and literal request; {}4. **Write** — plan per **Plan contents**.\n</procedure>\n\n",
			if ctx.scout_available && ctx.task_available {
				" Scope spans areas → parallel `scout` subagents via `task`, distinct focuses: implementations, related components, test patterns."
			} else {
				""
			},
			if ctx.ask_available {
				format!("`{}` resolves remaining preferences.\n", ctx.ask_tool_name)
			} else {
				"record remaining preference questions as Assumptions with a recommended default.\n".to_string()
			}
		));
	}
	text.push_str("## Plan contents\n\nScannable markdown; depth follows change: one-file fix → few bullets; cross-cutting change → ordered behavior steps.\n\n- **Context** — literal ask, need, intended end state; 2–4 sentences. Every requested outcome maps to a step; add nothing beyond ask.\n- **Approach** — load-bearing ordered change steps. Order for a building tree and passing existing tests after each; state dependencies and independencies. Group by behavior, NEVER file. Each step:\n  - Concrete edit: verb, exact target, new behavior; NEVER merely area to “update”/“handle”.\n  - Existing functions/utilities to reuse, paths; new code only with one-line statement that no equivalent exists.\n  - New/changed symbol with conforming callers, or load-bearing value (enum member, error/log string, config key, wire/JSON field): exact signature/literal.\n  - Rename, signature change, removal: every callsite (or exact `grep` returning exactly them) plus deletions; default clean cutover, no dead code/compatibility aliases.\n  - Rival patterns: copy and avoid named.\n  - Every new path: empty/missing/conflict/error handling; or no handling and why.\n- **Critical files & anchors** — ≤5 files disambiguating non-obvious work: path, symbol/region, one-line reason. Line numbers hints; implementer rereads before edit. Omit Approach-obvious files.\n- **Verification** — end-to-end proof; ≥1 new-behavior check: concrete input → expected observable output, not just build/typecheck/existing suite. Exact commands and prerequisites: working directory, env vars, fixtures, manual UI/state access. Tie risky-step checks to steps.\n- **Assumptions & contingencies** — only user-overridable decisions. NEVER put implementer decisions here; they belong in Approach.\n\nCut decision-free material: restated invariants, unaffected behavior, mechanical repetition, narration. Specify what implementer would otherwise invent.\n\n<directives>\n- NEVER include decision-free sections: Non-Goals, Out of Scope, Alternatives Considered, Risks/Mitigations, Future Work. Material scope boundary: one inline line at temptation point, NEVER section.\n- NEVER plan mechanical cleanup tail: changelog/release notes, doc updates, formatter/linter runs, scaffold removal. These run automatically after working change; no planning. Behavior-defining tests/end-to-end proof are not cleanup: retain in **Verification**.\n- NEVER reference planning conversation (`the option we chose above`, `as discussed`); unavailable to reader. State choice/reason inline.\n- NEVER invent request-unspecified schema, precedence, fallback policy, unless needed to prevent concrete implementation mistake; then state decision, not open question.\n</directives>\n\n<caution>\nReview options:\n- **Approve and execute** — fresh context (session cleared).\n- **Approve and compact context** — discussion distilled, then executes here.\n- **Approve and keep context** — executes here with exploration history.\n- **Save and quit** — copies the plan to a chosen path, then starts a new session.\n\nAll require self-contained file.\n</caution>\n\n");
	text.push_str(&format!(
		"<critical>\nBefore approval: engineer unfamiliar with conversation can execute every step without design decision and determine success at each step. Otherwise deepen any choice-forcing or ambiguous-done step.\n\nTurn ends ONLY:\n1. {}2. `{}` writes plan `<slug>`/title as plain text to `xd://propose` (`local://<slug>-plan.md` slug).\n\nNEVER request plan approval via prose/{}; MUST use `xd://propose` write. MUST continue until decision-complete.\n</critical>",
		if ctx.ask_available {
			format!("`{}` gathers requirements/chooses approaches; OR\n", ctx.ask_tool_name)
		} else {
			"Record preference questions as Assumptions and proceed with the recommended default; OR\n".to_string()
		},
		ctx.write_tool_name,
		if ctx.ask_available {
			format!("`{}`", ctx.ask_tool_name)
		} else {
			"a question".to_string()
		}
	));
	text
}

/// `prompts/system/plan-mode-tool-decision-reminder.md`.
#[must_use]
pub fn plan_tool_decision_reminder(ask_tool_name: &str) -> String {
	format!(
		"<system-reminder>\nPlan mode turn ended without a required tool call.\n\nYou MUST choose exactly one next action now:\n1. Call `{ask_tool_name}` to gather required clarification, OR\n2. Write the plan slug/title (`<slug>`, matching `local://<slug>-plan.md`) as plain text to `xd://propose` with `write` to finish planning and request approval\n\nYou NEVER output plain text in this turn.\n</system-reminder>\n"
	)
}

/// TS `isPlanDecisionTool`: `ask`, or a plan proposal write.
#[must_use]
pub fn is_decision_tool_call(call: &PlanToolCall) -> bool {
	call.name == "ask" || is_propose_tool_call(&call.name, &call.args)
}

/// TS `isProposeToolCall`: `write` whose `path`/`file_path` is `xd://propose`.
#[must_use]
pub fn is_propose_tool_call(name: &str, args: &Value) -> bool {
	if name != "write" {
		return false;
	}
	args
		.get("path")
		.or_else(|| args.get("file_path"))
		.and_then(Value::as_str)
		.and_then(xd_device_name)
		.is_some_and(|device| device == "propose")
}

fn xd_device_name(url: &str) -> Option<&str> {
	let rest = url.strip_prefix("xd://")?;
	let name = rest.split(['/', '?', '#']).next()?;
	(!name.is_empty()).then_some(name)
}

/// `normalizePlanTitle`: safe filename stem, or the exact TS error text.
pub fn normalize_plan_title(title: &str) -> Result<(String, String), String> {
	let trimmed = title.trim();
	if trimmed.is_empty() {
		return Err("Plan title is required and must not be empty.".to_string());
	}
	if trimmed.contains('/') || trimmed.contains('\\') || trimmed.contains("..") {
		return Err("Plan title must not contain path separators or '..'.".to_string());
	}
	let without_ext = if trimmed.to_ascii_lowercase().ends_with(".md") {
		&trimmed[..trimmed.len() - 3]
	} else {
		trimmed
	};
	let mut sanitized = String::with_capacity(without_ext.len());
	let mut previous_hyphen = false;
	for ch in without_ext.chars() {
		if ch.is_whitespace() {
			if !previous_hyphen {
				sanitized.push('-');
				previous_hyphen = true;
			}
			continue;
		}
		if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
			sanitized.push(ch);
			previous_hyphen = ch == '-';
		}
	}
	let sanitized = sanitized.trim_matches('-').to_string();
	if sanitized.is_empty() {
		return Err(
			"Plan title must contain at least one letter, number, underscore, or hyphen after sanitization."
				.to_string(),
		);
	}
	Ok((sanitized.clone(), format!("{sanitized}.md")))
}

/// `resolvePlanTitle`: supplied title, first H1, filename stem, then `plan`.
#[must_use]
pub fn resolve_plan_title(
	supplied: Option<&str>,
	plan_content: &str,
	plan_file_path: &str,
) -> (String, String) {
	let mut candidates: Vec<&str> = Vec::new();
	if let Some(supplied) = supplied.map(str::trim).filter(|title| !title.is_empty()) {
		candidates.push(supplied);
	}
	if let Some(heading) = first_level_one_heading(plan_content) {
		candidates.push(heading);
	}
	if let Some(stem) = plan_filename_stem(plan_file_path) {
		candidates.push(stem);
	}
	candidates.push("plan");
	for candidate in candidates {
		if let Ok(normalized) = normalize_plan_title(candidate) {
			return normalized;
		}
	}
	("plan".to_string(), "plan.md".to_string())
}

fn first_level_one_heading(content: &str) -> Option<&str> {
	for line in content.lines() {
		let trimmed = line.trim_start();
		if let Some(heading) = trimmed.strip_prefix("# ") {
			let heading = heading.trim();
			if !heading.is_empty() {
				return Some(heading);
			}
		}
	}
	None
}

fn plan_filename_stem(path: &str) -> Option<&str> {
	let without_scheme = path.trim_start_matches("local:").trim_start_matches('/');
	let last = without_scheme.rsplit(['/', '\\']).next()?;
	let stem = last
		.strip_suffix(".md")
		.or_else(|| last.strip_suffix(".MD"))
		.unwrap_or(last);
	(!stem.is_empty()).then_some(stem)
}

/// TS `planFileUrlForSlug`.
#[must_use]
pub fn plan_file_url_for_slug(slug: &str) -> String {
	format!("local://{slug}-plan.md")
}

fn plan_slug_from_supplied(supplied: Option<&str>) -> Option<String> {
	let title = normalize_plan_title(supplied?).ok()?.0;
	let slug = title
		.strip_suffix("-plan")
		.or_else(|| title.strip_suffix("-PLAN"))
		.unwrap_or(&title)
		.to_string();
	(!slug.is_empty()).then_some(slug)
}

/// Canonicalize a `local:` spelling so a resumed state path still matches the
/// scanner's `local://` entry (`normalizeLocalScheme`).
#[must_use]
pub fn normalize_local_scheme(url: &str) -> String {
	if let Some(rest) = url.strip_prefix("local:") {
		let rest = rest.trim_start_matches('/');
		return format!("local://{rest}");
	}
	url.to_string()
}

/// Closed plan lookup: state path and scanned artifacts, newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPlan {
	pub plan_file_path: String,
	pub plan_content: String,
	pub title: String,
}

/// TS `resolveApprovedPlan` without async: try the supplied title's slug, keep
/// a state plan the scan cannot see ahead of scanned files, then the newest
/// scan, then the state path. `read_plan` returns `None` for a missing file.
pub fn resolve_approved_plan(
	supplied_title: Option<&str>,
	state_plan_file_path: &str,
	read_plan: &dyn Fn(&str) -> Option<String>,
	list_plan_files: &dyn Fn() -> Vec<String>,
) -> Result<ResolvedPlan, String> {
	let mut ordered: Vec<String> = Vec::new();
	let mut consider = |url: Option<String>| {
		if let Some(url) = url
			&& !ordered.iter().any(|existing| existing == &url)
		{
			ordered.push(url);
		}
	};
	consider(plan_slug_from_supplied(supplied_title).map(|slug| plan_file_url_for_slug(&slug)));
	let listed = list_plan_files();
	let canonical_listed: std::collections::BTreeSet<String> = listed
		.iter()
		.map(|url| normalize_local_scheme(url))
		.collect();
	if !state_plan_file_path.is_empty()
		&& !canonical_listed.contains(&normalize_local_scheme(state_plan_file_path))
	{
		consider(Some(state_plan_file_path.to_string()));
	}
	for url in listed {
		consider(Some(url));
	}
	consider(Some(state_plan_file_path.to_string()));

	for url in &ordered {
		if let Some(content) = read_plan(url) {
			let (title, _) = resolve_plan_title(supplied_title, &content, url);
			return Ok(ResolvedPlan { plan_file_path: url.clone(), plan_content: content, title });
		}
	}
	let target = ordered
		.first()
		.cloned()
		.unwrap_or_else(|| state_plan_file_path.to_string());
	Err(format!(
		"Plan file not found at {target}. Write the finalized plan to {target} before requesting approval."
	))
}

/// Write-guard classification. `None` means the write is allowed; otherwise the
/// exact TS message. Guarded tools are write/edit/delete/move and the archive
/// writers; `bash` is deliberately not guarded (TS gap, retained).
#[must_use]
pub fn plan_write_error(tool_name: &str, args: &Value) -> Option<&'static str> {
	match tool_name {
		"delete" => Some("Plan mode: deleting files is not allowed."),
		"move" => Some("Plan mode: renaming files is not allowed."),
		"write" => {
			let raw = args
				.get("path")
				.or_else(|| args.get("file_path"))?
				.as_str()?;
			if is_local_artifact(raw) || is_propose_path(raw) {
				None
			} else {
				Some(
					"Plan mode: the working tree is read-only. Write your plan to a local://<slug>-plan.md file instead.",
				)
			}
		},
		"edit" | "apply_patch" | "ast_edit" => {
			if let Some(edits) = args.get("edits").and_then(Value::as_array) {
				for edit in edits {
					match edit.get("op").and_then(Value::as_str) {
						Some("delete") => return Some("Plan mode: deleting files is not allowed."),
						Some(op) if op != "create" && edit.get("rename").is_some() => {
							return Some("Plan mode: renaming files is not allowed.");
						},
						_ => {},
					}
				}
			}
			let raw = args.get("path").and_then(Value::as_str).unwrap_or("");
			if is_local_artifact(raw) {
				None
			} else {
				Some(
					"Plan mode: the working tree is read-only. Write your plan to a local://<slug>-plan.md file instead.",
				)
			}
		},
		_ => None,
	}
}

fn is_local_artifact(path: &str) -> bool {
	let normalized = normalize_local_scheme(path);
	normalized.starts_with("local://") || path.starts_with("vault://")
}

fn is_propose_path(path: &str) -> bool {
	xd_device_name(path) == Some("propose")
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn settle_rules_match_ts_ordering() {
		let state = AcpPlanState::enter(None);
		let mut settle = PlanSettleState::default();
		assert_eq!(
			observe_settle(Some(&state), &mut settle, "stop", &[], true, true),
			PlanSettle::Reminder(plan_tool_decision_reminder("ask"))
		);
		assert_eq!(settle.reminder_count, 1);
		assert!(settle.awaiting_progress);
		assert_eq!(
			observe_settle(Some(&state), &mut settle, "stop", &[], true, true),
			PlanSettle::Noop
		);
		observe_tool_end(&mut settle, "read", None);
		assert_eq!(
			observe_settle(Some(&state), &mut settle, "stop", &[], true, true),
			PlanSettle::Reminder(plan_tool_decision_reminder("ask"))
		);
		assert_eq!(settle.reminder_count, 2);
		observe_tool_end(&mut settle, "read", None);
		observe_settle(Some(&state), &mut settle, "stop", &[], true, true);
		assert_eq!(settle.reminder_count, 3);
		observe_tool_end(&mut settle, "read", None);
		assert_eq!(
			observe_settle(Some(&state), &mut settle, "stop", &[], true, true),
			PlanSettle::Yield
		);

		let decision = PlanToolCall {
			name: "write".to_string(),
			args: serde_json::json!({ "path": "xd://propose" }),
		};
		assert_eq!(
			observe_settle(Some(&state), &mut settle, "stop", &[decision], true, true),
			PlanSettle::Decision
		);
		assert_eq!(settle.reminder_count, 0);
	}

	#[test]
	fn error_and_tool_calls_do_not_count() {
		let state = AcpPlanState::enter(None);
		let mut settle = PlanSettleState::default();
		assert_eq!(
			observe_settle(Some(&state), &mut settle, "error", &[], true, true),
			PlanSettle::Noop
		);
		let call = PlanToolCall { name: "read".to_string(), args: Value::Null };
		assert_eq!(
			observe_settle(Some(&state), &mut settle, "stop", &[call], true, true),
			PlanSettle::Noop
		);
		assert_eq!(settle.reminder_count, 0);
	}

	#[test]
	fn plan_titles_and_urls_match_ts() {
		assert_eq!(plan_file_url_for_slug("auth"), "local://auth-plan.md");
		assert_eq!(normalize_plan_title("My feature plan").expect("title").0, "My-feature-plan");
		assert!(normalize_plan_title("a/b").is_err());
		assert_eq!(
			resolve_plan_title(Some(" token refresh "), "", "local://PLAN.md").0,
			"token-refresh"
		);
		assert_eq!(normalize_local_scheme("local:/PLAN.md"), "local://PLAN.md");
	}

	#[test]
	fn approved_plan_prefers_supplied_slug_then_state() {
		let read = |url: &str| match url {
			"local://auth-plan.md" => Some("# Auth\n".to_string()),
			"local://PLAN.md" => Some("# Old\n".to_string()),
			_ => None,
		};
		let resolved = resolve_approved_plan(Some("auth"), "local://PLAN.md", &read, &|| Vec::new())
			.expect("plan");
		assert_eq!(resolved.plan_file_path, "local://auth-plan.md");
		assert_eq!(resolved.title, "auth");
	}

	#[test]
	fn write_guard_matches_ts_scope() {
		assert_eq!(
			plan_write_error("write", &serde_json::json!({ "path": "src/main.rs" })),
			Some(
				"Plan mode: the working tree is read-only. Write your plan to a local://<slug>-plan.md file instead."
			)
		);
		assert_eq!(
			plan_write_error("write", &serde_json::json!({ "path": "local://x-plan.md" })),
			None
		);
		assert_eq!(plan_write_error("write", &serde_json::json!({ "path": "xd://propose" })), None);
		assert_eq!(
			plan_write_error("move", &serde_json::json!({ "path": "a", "newPath": "b" })),
			Some("Plan mode: renaming files is not allowed.")
		);
		assert_eq!(plan_write_error("bash", &serde_json::json!({ "command": "rm -rf x" })), None);
	}
}
