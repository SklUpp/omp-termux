# checkpoint

> Mark the current top-level conversation position so a later `rewind` can collapse exploratory context into a report.

## Source
- Tool: `crates/pi-agent/src/tools/checkpoint.rs` (`CheckpointTool`, `CheckpointController`).
- Session host: `crates/omp-cli/src/session_checkpoint.rs` (`CheckpointHost`).
- Branch persistence: `crates/omp-cli/src/session.rs` (`SessionStore::branch_with_summary`, `append_custom_message`).
- Upstream anchors: `packages/coding-agent/src/tools/checkpoint.ts`, `packages/coding-agent/src/session/agent-session.ts`, `packages/coding-agent/src/session/checkpoint-entries.ts`.

## Registration / Visibility
- `approval = read`, strict arguments, discoverable built-in. Execution is single-shot; no progress updates, no job handle.
- Gated by the shared `checkpoint.enabled` setting, default `false`.
- `checkpoint` and `rewind` are a sister pair registered together whenever `checkpoint.enabled` is on; `--tools` selection does not split them.
- Both tools are `is_exclusive`, so two `checkpoint` calls in one tool batch cannot both observe an empty state.
- The controller is per session: construct one `CheckpointHost`, register `host.checkpoint_tool()` and `host.rewind_tool()`, and hand `Arc::clone(host.controller())` to the run request. Children never get these tools, so they cannot capture the root store.

## Inputs

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `goal` | `string` | Yes | Investigation goal. Echoed unchanged; not trimmed and not rejected when empty. |

Unknown parameters and non-object arguments are rejected as invalid arguments.

## Outputs
Text body:

```text
Checkpoint: <goal>
Finish exploration and formulate findings.
```

`details`:

- `goal: string`
- `startedAt: string` — RFC 3339 UTC timestamp with milliseconds, generated inside `execute()`

No checkpoint ID, artifact URI, job handle, file path, or restore token is returned.

## Flow
1. `CheckpointTool::execute()` rejects an open checkpoint with `Checkpoint already active.`.
2. On success it records the live checkpoint synchronously (`note_checkpoint` with a placeholder message count and no entry id), so a `rewind` in the very next step already sees `checkpoint_state()`. If the request is abandoned before persistence, rehydration from the branch clears the phantom state on resume.
3. The session persists the tool-call and tool-result message entries as usual.
4. After `SessionStore::append_messages`, the host method `CheckpointHost::note_persisted_tool_results(store, &entries)` records:
   - `goal` and `startedAt` from result details (falling back to the entry timestamp),
   - the persisted checkpoint tool-result entry id,
   - `checkpoint_message_count` = that entry's 1-based position in the log (the in-memory cut point; not the end of a multi-result batch).
   It clears any pending rewind report and retained completed rewind. A replayed checkpoint result already followed by a retained rewind report on the branch is ignored.
5. In no-session mode the same capture runs from the turn-end tool results (`note_unpersisted_tool_results`) using `turn_start`.
6. `CheckpointHost::needs_rewind_before_yield()` reports an open checkpoint without a captured report. The turn-end ack reply sets `continue_turn` and `warning` from `yield_warning()`, so the loop runs another turn instead of finishing.
7. On resume, session switch, or tree navigation, `CheckpointHost::rehydrate(&store)` scans the active branch (`SessionStore::branch_entries`) for the latest successful checkpoint result. A later retained `rewind-report` entry turns it into the completed-rewind guard instead.

## Side Effects
- Runtime state: goal, message count, persisted entry id, timestamp (never written as its own entry).
- A non-displayed `checkpoint-active-reminder` custom entry follows the checkpoint result so branch rebuilds replay the notice to the model; the rewind cut abandons it.
- Nothing about the filesystem, git, artifacts, blobs, processes, or auth is captured or restored.

## Limits & Caps
- Availability: `checkpoint.enabled`, default `false`.
- One active checkpoint per session; there is no checkpoint list, label, or ID parameter.
- Report/summary/report entries are subject to the normal session persistence rules.

## Errors
- `Checkpoint already active.` — an earlier checkpoint has not been rewound (or rehydrated as open).
- Invalid arguments — non-object arguments, unknown keys, or a non-string `goal`.

## Notes
- Checkpoint state is conversation metadata only. No git operation exists anywhere in this lane.
- Persisted session history is append-only; branching never rewrites or deletes earlier `.jsonl` lines.
