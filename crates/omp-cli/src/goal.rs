//! Goal mode state (TS `goals/state.ts`) and its session-log shape: the `goal`
//! payload of a `mode_change` entry plus the terminal `goal-completed` metric.
//!
//! Persistence only. Turn baselines, the wall clock and budget-steer dedup are
//! session-local and lost on reopen by design (TS `GoalRuntimeSnapshot`).

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

#[cfg(test)]
mod tests {
	use std::fs;
	use std::path::PathBuf;

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
}
