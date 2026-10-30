# Goal Mode (headless/RPC)

Rust port of TS `goals/` + `modes/turn-budget.ts`. The interactive TUI, status
footer and `/goal` menu are excluded; everything below runs in `--print`,
text/json REPL and `--mode rpc`.

## State and persistence

- `GoalRuntime` (`crates/omp-cli/src/goal.rs`) owns the session goal state,
  turn snapshot, wall clock and budget-steer dedup. It never writes the log.
- Lifecycle transitions queue `GoalPersist` intents; the single session writer
  (CLI `main.rs`, RPC `rpc/session.rs`) drains them after each streamed event,
  so a tool call cannot interleave a second writer.
- Log shapes are unchanged: `mode_change goal|goal_paused|none` with
  `data.goal`, and a terminal `custom goal-completed` metric
  (`objective`, `tokensUsed`, optional `tokenBudget`, `timeUsedSeconds`).
- Restore reads the *active branch*: malformed mode data or
  `goal.enabled=false` clears it and appends `none`; an active goal replayed
  without an explicit preserve is paused (appends `goal_paused`), and a branch
  or session without goal mode drops any stale live goal.

## Tool

`goal` (`crates/omp-cli/src/goal_tool.rs`) is a hidden tool with ops
`create|get|complete|resume|drop` and the TS result text/details. It is
offered when `goal.enabled` (default true) and either no `--tools` restriction
excludes it or a goal exists (a live/paused goal always keeps the tool). It is
never mounted for child agents. `--tools goal` is accepted as a hidden name.

`replace`, `pause` and budget mutation exist only as runtime methods: their
upstream surface is the excluded TUI `/goal set|pause|budget`, so they are not
reachable from headless/RPC and are not counted as supported ops.

## Accounting

- `+N[.N][k|m][!]` in a user prompt opens a per-turn output window; without a
  directive the window has no ceiling. Synthetic continuation turns keep the
  window the user set.
- Goal spend = `input + cacheWrite + output` deltas since the turn baseline
  (`cacheRead` is a reused prefix, excluded). Only `MessageEnd` usage is
  summed; unreported requests add nothing.
- Delegated child usage is added once per job: blocking `task` batches and eval
  `agent()` results carry `details.usage`; background jobs deliver usage via
  `AgentEvent::BackgroundResult` (persisted as `custom background-usage`) or
  `hub` job snapshots, deduplicated by job id.
- Budget pressure flips the goal to `budget-limited`, steers the hidden limit
  prompt once per goal id, persists, and never completes the goal.

## Prompts and continuation

- Each user turn gets a hidden `goal-mode-context` message (active prompt plus
  live todos) as the last message before the user message.
- Completion exit: after the turn drains, `mode_change none` + the
  `goal-completed` metric are written once.
- Interrupt pauses an active goal and persists `goal_paused`.
- Headless auto-continuation is opt-in: `goal.continuationModes` must contain
  `"headless"` (upstream default `["interactive"]` keeps no loop). The loop
  submits the continuation prompt as a hidden `goal-continuation`
  `custom_message` turn until the goal is completed, dropped or paused. RPC
  never starts turns on its own; hosts drive the next prompt.

## Settings

| Key | Default | Effect |
|---|---|---|
| `goal.enabled` | `true` | Enable Goal Mode and the hidden `goal` tool |
| `goal.continuationModes` | `["interactive"]` | Add `"headless"` to let the headless CLI auto-continue |

## RPC events

`goal_updated` frames `{type, goal: Goal|null, state?: {enabled, mode, reason?,
goal}}` are emitted after every transition and at turn settle, in order with
the event frames that produced them.

## Verification

Deterministic tests: `cargo test -p omp-cli goal` (turn-budget parser,
flip/steer-once, transition guards, interrupt pause, resume reconcile,
`budget()` precedence, prompt rendering, child/background usage, wall clock) and
`cargo test -p pi-agent child_usage`.

Runtime smoke: point `--provider`/`--model` at a local OpenAI-compatible SSE
stub that calls `goal({op:"create",…})`, then `goal({op:"complete"})` and
reports `usage`; run `omp --print --tools read,goal` with a `+50k!` prompt, then
reopen the session file and confirm `goal_paused`, `none`, and the
`goal-completed` metric.
