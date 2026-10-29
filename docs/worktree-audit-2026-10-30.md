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
