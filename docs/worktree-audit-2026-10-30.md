# Saved worktree audit — 2026-10-30

Context: preserve pending changes before cleanup. Baseline `faad6b5355`; feature branch `feat/rust-agent-termux-bionic`. No directories or refs removed. No force-push or history rewrite.

## Physical directories

`git worktree list --porcelain` lists only the main checkout. Two retained directories under `~/.omp/wt` are plain snapshots without `.git`, not registered linked worktrees.

| Snapshot | Identical files | Different files | Files absent from main | Bytes compared, excluding build/dependency dirs | Disposition |
|---|---:|---:|---:|---:|---|
| `~/.omp/wt/t9c709bb6a/m` | 6985 | 43 | 0 | 174595651 | Preserve until remaining differing hunks are classified or separately archived |
| `~/.omp/wt/tdc1339c2a/m` | 6959 | 40 | 0 | 174318151 | Preserve until remaining differing hunks are classified or separately archived |

Comparison skipped `.git`, `node_modules`, `target` and symlinks; byte counts are not reclaimable disk-space estimates. Missing files in the snapshots do not imply deletion requests. Stale `FETCH_HEAD` mentions missing `~/.omp/wt/t81cf45cee/m`; this is not an existing checkout.

## Confirmed superseded differences

- t9 workspace `sha2` dependency is superseded by the direct pinned CLI dependency; no need to copy it back.
- Both old `yaml.rs` copies predate `93d9e3412b`'s consumer-value round-trip assertion fix.
- t9 `auth/mod.rs`, `auth/flows/mod.rs` and `rpc/auth.rs` lack the current required per-dialog cancellation contract. Restoring them would regress joined prompt cleanup.
- tdc `rpc/host_uris.rs` retains obsolete mount-prerequisite/is-bound handling. Current code propagates mount errors, protects built-in schemes and removes obsolete refusal gates.
- Snapshot-only function names found in differing Rust files are old mount/event-bus prerequisite helpers and replaced abort/replacement controls. Name comparison alone does not prove semantic equivalence.
- Most differing snapshot blobs occur in main history. Seven t9 and thirteen tdc differing files had no exact historical blob match; those intermediate versions must not be called byte-identical backups.

## Snapshot refs

There are 19 `refs/heads/omp/task/*` snapshot tips. None is an ancestor of baseline HEAD; each reports one `+` commit and zero `-` commits under `git cherry HEAD <ref>`. None of their complete patches passes reverse-apply checking against the newer working tree. These facts do NOT prove lost changes: conflict-resolved/squashed integrations and subsequent fixes change patch IDs. Preserve these refs pending the semantic mapping below. Never blindly cherry-pick an old snapshot over newer fixes.

Snapshot refs: AgentCatalogPolicy, AsyncCoordination, BashParity, EvalHostImplement, FinishCompletions, HubDelegation, InteractiveAsk, LspDapFullPipeline, NativeAuthPipeline, NativeModels, NativeProcessBroker, NativeProcessHub, ProcessCli, ProfileConfigParity, ReadParity, RootThinking, RpcPipelineRecovery, RpcUriSubagentBackend, TodoParity.

Integration-history mapping is being checked separately. No unique missing behavior has yet been established by the physical-copy checks above. Cleanup is not approved as safe from a mere matching filename, HEAD or line-count.

## Current pending block

Native RPC auth runtime wiring and owner-authored constant decoding are in the main working tree. Review found a manual compact/handoff deadlock during interactive login; commands now reject with `LOGIN_IN_PROGRESS`, and automatic compaction skips while login owns the store. Duplicate credential-literal tests were replaced by one generic decode/error-redaction test. S5 online provider discovery and S6 child credential/model overrides remain open; committing this slice does not close full parity.

Normal push was attempted and rejected with GH013 against historical commit `faad6b5355`. Later source edits do not remove that historical scan finding. No bypass performed.

## Integration mapping

Read-only history/declaration audit found the following later integrations. These are content-port mappings, not literal merges of snapshot tips. No missing feature was established; unique-string inspection covered 7/19 snapshots, so this is not proof that every old hunk is redundant. Keep refs until a pinned cleanup review or archive.

| Snapshot branch (`omp/task/`) | Later integration commits |
|---|---|
| AgentCatalogPolicy | `714e5feb6b` |
| AsyncCoordination | `3f35852340` |
| BashParity | `b82ea04a52` |
| EvalHostImplement | `63915d1631`, `74c61c3d72`, `f6dc0b9bce`, `faad6b5355` |
| FinishCompletions | `ce219f6b47` |
| HubDelegation | `3f35852340`, `714e5feb6b` |
| InteractiveAsk | `3f35852340` |
| LspDapFullPipeline | `4becf171d7`, `7d05791eaa`, `cadbb81035` |
| NativeAuthPipeline | `faad6b5355` |
| NativeModels | `36b1942898` |
| NativeProcessBroker | `714e5feb6b` |
| NativeProcessHub | `714e5feb6b`, `f50a1961a2` |
| ProcessCli | `f50a1961a2` |
| ProfileConfigParity | `07bc2eb73e`, `0472889572` |
| ReadParity | `07bc2eb73e`, `efe29a2204`, `b77d7659be` |
| RootThinking | `5cd6508440`; all added lines contained, both added test files byte-identical |
| RpcPipelineRecovery | `5c3c15b9db`, `5f4fa72507`, `faad6b5355` |
| RpcUriSubagentBackend | `faad6b5355` |
| TodoParity | `b82ea04a52` |

Do not merge these old snapshots wholesale: that would reintroduce pre-review callback, lifecycle and protocol code. No additional integration patch was identified by this audit. Physical copies retain intermediate bytes absent from history; removal still requires archival or complete hunk review. No directories or refs were removed.

Publication subsequently confirmed: remote feature branch matched local `abe3d8c274`; exact-SHA CI run `40944110426` started. This supersedes the earlier blocked-push status, not its historical evidence.

## Completed physical-copy audit

This section supersedes the earlier pending classification. Baseline HEAD: `abe3d8c274cd98e5246d3c3560de1e7920d6c48f`; current uncommitted child-credential work was preserved. Source comparison only; no builds/tests required or run for these read-only copies.

| Copy | Byte-identical files | Historical differing blobs, reachable from HEAD | Intermediate differing files | Needed integration |
|---|---:|---:|---:|---|
| `t9c709bb6a/m` | 6985 | 36 | 7 | None identified |
| `tdc1339c2a/m` | 6959 | 27 | 13 | None identified |

No snapshot-only files or symlinks were found outside excluded build directories. Only `target/` actually existed among excluded `.git`, `node_modules`, `target` directories. `du -sk` measured 300977 KiB and 270677 KiB respectively, including build output; not an exact reclaimable-space estimate.

### Auth copy: seven intermediate files

- Root and CLI `Cargo.toml`: workspace dependency path replaced by pinned direct `sha2` dependency; do not restore the redundant workspace entry.
- `auth/mod.rs`, `auth/flows/mod.rs`: old callback omitted the owning cancellation token. Current required token contract supersedes it.
- `auth/flows/oauth_code/mod.rs`: abandoned manual-input worker replaced by cancellation plus `ManualWait` drop/join; current decoder and lifecycle regressions extend the old implementation.
- `rpc/auth.rs`: global `cancel_all` and blocking prompt replaced by targeted cancellable requests. Restoring the snapshot could close unrelated dialogs or stall shutdown.
- `rpc/auth/tests.rs`: old tests are a subset; current suite adds per-dialog cancellation and registration-race coverage.

### URI copy: thirteen intermediate files

- `rpc/host_uris.rs`, `rpc/mode.rs`, `rpc/subagents.rs`: old unbound/unattached prerequisite gates and direct observer attachment replaced by lifetime-bound routing and runner-owned observation. Do not restore refusal gates.
- `rpc/mod.rs`: current additionally exports native auth.
- `tools/internal_urls.rs`, `tools/mod.rs`, `tools/read.rs`: current adds reserved `local` routing, registration errors, scoped read/write contexts and selector handling. Old variants weaken these protections.
- `supervisor.rs`: current retains snapshot behavior and adds real event delivery, emission ordering, scoped child URLs and idempotent subscription.
- `tools/task.rs`: current preserves parent tool-call ID instead of the snapshot's `None`.
- `tests/subagent_observability.rs`: current drains captured frames correctly and adds real-event/terminal-order regressions.
- `tests/rpc_protocol.rs`: complete diff contains fixture bookkeeping/abort-consumption synchronization changes plus added tests, not a removed unique snapshot scenario. Remaining snapshot replacements are superseded harness fields/constructor and old SSE server return/control handling.
- Both project documents are strict older prefixes of the current versions; diff contains additions only.

### Cleanup decision

Do not merge either copy: no needed missing implementation was identified. Historical differing blobs are recoverable because their introducing/removing commits remain ancestors of HEAD; `git log --find-object` alone does not mean those blobs exist at HEAD.

The 20 intermediate files contain exact old bytes not proven stored in Git. Their behavior is superseded, but removing the directories still discards those historical bytes. Archive first if exact historical recovery matters. No directory, build output or ref was deleted, moved or archived by this audit.

## Current retained-tree audit at `e868c405a4`

Five physical source trees were present in the final read-only sweep. Two active slices (`t401179558/m`, FullGoalRuntime; `t0e073d1c0/m`, AcpTransportSlice) are unfinished and therefore not merged. `t94fb43aa1/m` production changes are integrated; its workspace-root continue smoke script is intentionally retained. `tf5493458e/m` is a stale Goal persistence backup, lacking the parent's module export and numeric/test repairs; do not merge it over current code. `tb04124386/m` is the deferred inspect-image copy; its output/SSE/image/docs edits are integrated, and every dirty runtime child-credential hunk was located in current main (`runtime.rs:929-936,1691-1698,1876-1972,4030-4129`). No missing production hunk was identified in these retained historical trees.

The parent ran `git status --short --untracked-files=normal` for all five trees; known untracked source files were `goal.rs` in the stale persistence copy and `inspect_image.rs` in the deferred copy, both present in main. This is an integration audit, not cleanup approval. Some previously observed tree directories were absent on later reads; actor/cause is unknown. No deletion was performed by this audit. Two active pipelines remain outside the all-transferred claim; their future edits still require review and integration.

