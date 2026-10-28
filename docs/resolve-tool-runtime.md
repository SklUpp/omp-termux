# Resolution devices runtime

Pending previews and plan approval do not use a `resolve` tool. They finalize through plain-text `write` calls to virtual `xd://` devices implemented in `packages/coding-agent/src/tools/resolve.ts`:

- `xd://resolve` — apply the pending staged preview; body = a one-sentence reason
- `xd://reject` — discard the pending staged preview; body = a one-sentence reason
- `xd://propose` — submit a plan for approval while plan mode is active; body = the plan slug (`<slug>` for `local://<slug>-plan.md`)

These are internal URLs, not filesystem paths. `read xd://resolve`, `read xd://reject`, and `read xd://propose` return a one-line usage hint. Completed device writes carry `details.xdev` metadata; consumers recover the inner result through `writeDeviceDispatch()` and `resolveDispatchDetails()`.

## Preview flows

Preview producers call `queueResolveHandler(...)` with `apply(reason)` and optional `reject(reason)` callbacks. Each preview receives a unique pending-invoker ID in `ToolChoiceQueue`, so stacked previews do not overwrite one another.

While a preview is pending, `AgentSession.nextToolChoiceDirective()` returns a soft requirement:

- `toolName: "write"`
- `satisfies: isPreviewResolutionToolCall`
- reminder from `resolve-device-reminder.md`

The model complies by writing to `xd://resolve` or `xd://reject`. A different write does not resolve the preview and is skipped or escalated by the soft-requirement lifecycle.

Dispatch invokes the pending queue head through `runResolveInvocation(...)`.

- A successful apply or discard consumes that pending invoker exactly once.
- If apply throws, the same preview is re-registered so the model can reject it or retry after fixing the cause.
- Rejecting with no pending action succeeds with `Nothing to reject; no pending action remains.`
- Resolving with no pending action throws.
- An apply callback's ordinary error becomes `ToolError("Apply failed: ...")`; an existing `ToolError` is preserved.

## Rust runtime (`crates/pi-agent`, `crates/omp-cli`)

The Rust port enforces the same soft requirement without a `resolve` tool:

- `PendingStore` (`crates/pi-agent/src/tools/pending.rs`) owns the queue head identity `PendingHead { id, generation, source_tool, label }`. `clear` bumps the generation, so an in-flight apply cannot resurrect a discarded preview and a stale reminder never matches a new head.
- `StagedPreviewPolicy` (`crates/pi-agent/src/state.rs`) is the session-owned `ToolPolicy`: it peeks the head and considers only `write` to `xd://resolve`/`xd://reject` (`is_resolution_device_path`) compliant. `build_registry_full` (`crates/omp-cli/src/runtime.rs`) mounts the policy from the runner session onto each rebuilt registry; `Supervisor::child_registry` binds a fresh policy, `write`, and `ast_edit` to one child queue and snapshot store so a child preview can never resolve against the root queue.
- The loop (`crates/pi-agent/src/loop.rs`) injects the resolve reminder once per `(id, generation)`, keeps `tool_choice: auto` for the first request, marks every non-compliant batch as skipped (`is_error`, `Not executed: call the write tool…`) without running its calls, and forces `write` for exactly the next provider request. It aborts after three escalations, drops the requirement when `write` is absent from the active registry (upstream `agent.ts` gate), and keeps the per-run escalation state local so prompts and child agents cannot inherit a stale force.
- `ast_edit` previews mount `PreviewDisplay::Hashline` to match `read`'s snapshot sink, so previewed tags anchor the next `edit`.

## Plan approval

Plan mode installs a separate proposal handler through `setPlanProposalHandler(...)`.

- Interactive mode hands `PlanApprovalDetails` to the plan-review UI.
- ACP mode runs elicitation/approval and emits mode updates.
- PlanYolo auto-approves and switches to the execution target.

`xd://propose` dispatches the written slug to the installed plan proposal handler and is valid only while plan mode is active.

## Why `write` is guaranteed

Because previews and plan approval ride `write`, the harness keeps `write` available whenever needed:

- `createTools(...)` auto-appends `write` when a deferrable tool such as `ast_edit` is active.
- `createAgentSession(...)` keeps `write` registered when a deferrable tool exists or plan mode is enabled.

## Custom tools

Custom tools still stage previews through `pushPendingAction(...)`; the loader forwards them into `queueResolveHandler(...)`. The custom-tool preview API is unchanged except for the model-facing finalization step: follow up with a plain-text write to `xd://resolve` or `xd://reject`, not a `resolve` tool call.
