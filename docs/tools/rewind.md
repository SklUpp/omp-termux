# rewind

> End an active checkpoint by pruning exploratory context and retaining a concise report.

## Source
- Tool: `crates/pi-agent/src/tools/checkpoint.rs` (`RewindTool`).
- Session host: `crates/omp-cli/src/session_checkpoint.rs` (`CheckpointHost::apply_pending_rewind`, `apply_pending_rewind_in_memory`).
- Branch persistence: `crates/omp-cli/src/session.rs` (`SessionStore::branch_with_summary`, `append_custom_message`, `branch_history`).
- Upstream anchors: `packages/coding-agent/src/tools/checkpoint.ts`, `packages/coding-agent/src/session/agent-session.ts` (`#applyRewind`), `packages/coding-agent/src/session/session-manager.ts` (`branchWithSummary`).

## Registration / Visibility
- `approval = read`, strict arguments, discoverable built-in. Single-shot; the tool result is not the final rewind.
- Gated by the shared `checkpoint.enabled` setting, default `false`; sister pair with `checkpoint`.
- Both tools are `is_exclusive`, so a `rewind` in the same batch as its `checkpoint` runs after it and sees the live checkpoint state.
- The controller tools are constructed from one `CheckpointHost`: `host.checkpoint_tool()` / `host.rewind_tool()`.

## Inputs

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `report` | `string` | Yes | Investigation findings. Trimmed; the empty result is rejected. |

## Outputs
Text body:

```text
Rewind requested.
Report captured for context replacement.
```

`details`:

- `report: string` — trimmed report
- `rewound: true`

## Flow
1. `RewindTool::execute()` requires a live checkpoint from `CheckpointController::checkpoint_state()`. Without one it distinguishes the retained completed rewind from no checkpoint at all.
2. It trims and validates the report, then returns the tool result. Side effects are deferred.
3. The caller persists the tool-result entry. At the same turn end, `CheckpointHost::note_persisted_tool_results` stores the pending report (no-session mode: `note_unpersisted_tool_results`).
4. At turn end the caller invokes `CheckpointHost::apply_pending_rewind(&mut store)`:
   - `store.branch_with_summary(checkpoint_entry_id, report, {startedAt})` appends a `branch_summary` at the checkpoint entry and moves the leaf. A referenced entry that no longer resolves logs a warning and branches from root; a `None` entry id branches from root without a warning.
   - `store.append_custom_message("rewind-report", render_rewind_report(report), false, {report, startedAt, rewoundAt})` appends the hidden retained report.
   - `CheckpointController::complete_rewind` records `last_completed_rewind` and clears the active checkpoint plus pending report.
   - `store.branch_history()` returns the branch-only `TurnRecord` view, exposed as `RewindApplied::history`. Sibling `toolResult` entries of a retained assistant message (the checkpoint sharing its batch with other calls) are not ancestors; `branch_messages` explicitly includes them so no retained call is replayed as an empty success.
5. `CheckpointHost::context_messages(&store)` rebuilds provider-ready messages in branch order: rebuilt turns, `branch_summary` as a user-role wrapper, and the retained report as system/developer guidance. It replays same-batch sibling results right after their assistant call, errors if the branch references a message entry that is missing instead of silently dropping context; the caller then warns and falls back to `branch_history()`. All other custom-message types are replayed too.
6. The session main loop builds the next request from `history_messages` (this branch context) whenever `SessionStore::has_branch_context()` is true; incoherent `parentId` chains fall back to journal order so a resumed conversation is never silently truncated.
   - Without a session (`--no-session`) `Session` keeps its own branch conversation: each finished assistant turn is mirrored, `apply_in_memory_rewind` cuts it after the checkpoint's whole tool batch (sibling results stay; the live loop does the same via `tool_batch_end`), and the cut mirror is sent back as a wholesale `TurnRewrite::Replace`. That replaces the live (possibly compacted) context in the **current run**, so correctness does not depend on the checkpoint result surviving compaction; every later prompt also replays the mirror as `history_messages`. Retained summary/report survive, abandoned exploration does not, and the runtime appends exactly one new user turn.
   - The controller stays pending while the no-session cut is in flight; `finish_in_memory_rewind` commits it only after the turn settles, while `abandon_in_memory_rewind` on error reopens the checkpoint (drops the unsettled report) so the yield guard and a later retry still work.
7. On resume or tree navigation `CheckpointHost::rehydrate(&store)` restores either the open checkpoint or the completed-rewind guard from the active branch.
8. If the assistant yields while a checkpoint is still open and produced no tool calls, the turn-end ack reply continues the turn with the yield warning. Repeats stop when `rewind` records its report or the loop's turn budget is reached.

### In-loop seam
`pi_agent::TurnAck` is the agent-loop rendezvous:

```rust
pub struct TurnAckRequest {
    pub turn_start: usize,
    pub message_count: usize,
}
pub struct TurnAckDecision {
    pub rewrite: Option<TurnRewrite>, // Replace(messages) | InMemory { keep, summary, report }
    pub continue_turn: bool,
    pub warning: Option<String>,
}
```

The live cut also locates the checkpoint result by `checkpoint_tool_call_id`; `keep` is a fallback only, and that fallback finishes a partially kept tool batch and drops a dangling trailing assistant call so the provider history stays valid after compaction removed the anchor.

After the loop emits `Event::TurnEnd`, it waits for the session owner to reply. `crates/omp-cli/src/main.rs` handles the event synchronously: it persists the assistant/tool-result entries, calls `note_persisted_tool_results`, applies the pending rewind, and answers with `TurnRewrite::Replace(context_messages)` (or, without persistence, `TurnRewrite::InMemory`). A rewrite forces another provider request. If the consumer drops the stream early, a guard cancels the ack so the loop never hangs.

## Modes / Variants
- Normal rewind: the checkpoint entry resolves; the branch is cut at that entry.
- Root fallback: the checkpoint entry id is missing from the current tree; the branch summary starts a new root and the warning path is reported via `RewindApplied::fell_back_to_root`.
- Deferred turn-end apply: nothing changes while the tool call is running.
- Resumed checkpoint: an unfinished successful checkpoint result on the active branch rehydrates the checkpoint so `rewind` can complete later.

## Side Effects
- Appends a `branch_summary` and a `custom_message` (`customType = rewind-report`, `display = false`) to the session `.jsonl`; the leaf follows them.
- Rebuilds the active conversation only. Files, git, artifacts, blobs, processes, and auth are untouched.
- Usage/cost fields on retained messages are preserved verbatim; abandoned entries are never rewritten or removed.
- Resets checkpoint runtime state and sets the completed-rewind guard.

## Limits & Caps
- `checkpoint.enabled`, default `false`.
- One active checkpoint per session; no checkpoint id parameter.
- Report must be non-empty after `trim()`.
- Session persistence caps apply as usual.

## Errors
- `Checkpoint already completed; continue from the retained rewind report instead of calling rewind again.`
- `No active checkpoint. Create a checkpoint before calling rewind.`
- `Report cannot be empty.`
- Invalid arguments for non-object args, unknown keys, or a non-string report.
- A missing checkpoint entry during apply does not fail the tool call; it branches from root and sets `fell_back_to_root`.

## Notes
- Rewind is not destructive to persisted history: abandoned `.jsonl` lines remain but leave the active branch, so a later resume cannot see them in `branch_entries`/`branch_history`.
- There is no file or git restore path and no concurrent-edit reconciliation.
