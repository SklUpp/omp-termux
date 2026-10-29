# Rust CLI project context — cold-start handoff

Read this first, then use [`rust-cli-parity-plan.md`](rust-cli-parity-plan.md) as the authoritative inventory and evidence ledger. This file is orientation only; it deliberately does not duplicate the plan's per-ID audit tables, provider matrix, flag tables or dated execution history.

Repository: `SklUpp/omp-termux`, branch `feat/rust-agent-termux-bionic`, worktree `/data/data/com.termux/files/home/omp-work`. Upstream reference checkout: `packages/` (TypeScript) in the same tree.

## Goal and boundaries

Build a separate Rust `omp` (`crates/omp-cli`, target Termux arm64/Bionic) that owns the CLI, agent loop, sessions, tools, auth and protocols. Do not replace the existing Bun-based Android release or the installed `$PREFIX/bin/omp` until final acceptance. Do not modify `install.sh`, `android-release.yml`, or packaging.

Rust must not start a JavaScript runtime for general operation. JS is allowed lazily only for `eval(language="js")` and `browser.run`. External interpreters, Chromium, SSH, provider accounts and desktop backends are external prerequisites; absence must produce a precise error, never a fabricated success or silent fallback.

Approved exclusions remain: extension/hook/plugin execution and `plugin`/`install`; interactive TUI, `rpc-ui`, full-screen `git`/`join`/`shell`; native desktop `computer`; local audio capture/playback; live WebRTC; image clipboard; `update` without an Android release target. Excluded paths still need an explicit refusal instead of a generic failure or prompt absorption.

Objective: complete the full plan, not a FIX-only queue. Current priority: safety/data-loss and confirmed CI failures first, then core tools, delegation, sessions, CLI modes/flags and live events. Authorization/discovery and frequently changing provider dialects have lower priority; preserve the existing provider path and unfinished auth work. No placeholders, no-op flags, external dependency installs or installed-CLI replacement.

## Current evidence state (2026-10-30)

| Item | State |
|---|---|
| Published HEAD | `abe3d8c274`; remote feature branch matched this commit. Current uncommitted slices are not covered by that SHA. |
| Last exact-SHA CI | [40944110426](https://github.com/SklUpp/omp-termux/actions/runs/40944110426): Android build PASS, native grep PASS, host FAIL (467 passed / 1 failed in `omp-cli --lib`, stale JSON-result assertion). Not a full-suite pass. |
| Historical Android smoke | `0f601b6f61` / run `40827646802`, artifact SHA-256 `d79bdbb3c0e407f3fb5f7156605c8fbe08d9836751f81e77f566105125af630f`: help/version and 5 PTY scenarios passed. Its 10.36 s is functional wall time, not current CPU/PSS/startup evidence. |
| Historical artifact path | `$PREFIX/tmp/omp-rust-final.ciBYAL/omp` — ephemeral, tied only to the historical SHA/run above. |
| Latest local JSON checks | Unit 1/1 and live SSE 2/2 passed; actual localhost-provider JSON CLI `read` turn passed. Test-only LLVM single-thread link workaround was required. Scoped evidence only. |
| Performance / coverage | Equal-history 8/30-turn CPU/PSS/startup comparison: not measured. Rust coverage percentage: not measured. |

Artifact-level help/version/PTY checks are not a per-ID command matrix. Every parity row stays **acceptance-open** unless the plan records that ID's own invocation, arguments, typed result, error/cancel path and Android smoke. Shared green CI does not close rows.

### Implementation-axis truth (summary)

- Core present: CLI/agent loop, live bounded events and cancellation, session storage plus continue/resume/fork paths, approvals, read/write/edit (hashline plus variants), bash/process supervision, glob/grep, AST grep/edit, todo, ask, MCP client, task/hub delegation, checkpoint/rewind, `yield`/`think`. Foreign session import readers exist with non-TTY rejection. This is presence/reachability, not per-ID acceptance or full TypeScript equivalence.
- CLI presence recount (2026-10-30): 41 canonical TypeScript IDs / 44 tokens; 10 present, 25 missing canonical / 27 missing tokens, 6 excluded; Rust-only `repl` extra. Explicit verb refusals are wired before prompt assembly, not implementations of missing commands. Per-command behavioral acceptance remains open.
- Tool presence recount: 32 IDs; 19 source-present including gated/special mounts, 12 deferred, 1 excluded (`computer`). Provenance: read-only audit of the published code basis `abe3d8c274`; commits after it are `088367d8b9` (unregistered `inspect_image` loader, not a tool) and `260a314843` (FixOwnerClearRace owner-clear fix, regression unrun). Neither changes presence counts. `apply_patch` remains an `edit` wire alias. Presence is not full behavior or runtime acceptance; unfinished image/isolation work is not counted.
- Wire APIs: 14 dialects deferred, retained requirement, none excluded, and not the next slice (2026-10-30 auth-LOW decision).
- MCP: stdio/HTTP/SSE client paths exist; OAuth unsupported.
- Flags: string 17 present / 9 missing / 4 excluded; optional 1 partial; valueless 11 present / 12 missing.

### New audit truths that must not be lost

- **LSP and DAP are mounted behind settings.** CLI runtime registers them under `lsp.enabled`/`debug.enabled` and tool selection, with session-scoped ownership. Source reachability is established; per-ID Android invocation/result/cancellation acceptance remains open.
- **Eval is partial.** The bridge rejects `checkpoint`/`rewind`; the older claim that it strips `eval`/`task`/`hub` is obsolete. `budget()` still fails closed because Goal Mode accounting is absent; no Rust `GoalTool` is implemented.
- **Task supports blocking and background execution.** With a background-ready supervisor, non-blocking agents spawn as detached jobs that return real job IDs and auto-deliver exactly once unless a settled `hub jobs`/`wait` snapshot consumes the result first; blocking agents run inline. Covered by `crates/pi-agent/tests/async_coordination.rs` and `crates/pi-agent/src/tools/task.rs` (`spawn_background_task`/`run_background_child`). `task` supports `outputSchema`, `schemaMode` and `effort`; at published `abe3d8c274` it rejected `isolated` only (`task.rs:1461-1472`), and the active isolation slice now modifying `task.rs` is unverified either way and is not counted. Eval `agent()` additionally rejects `apply` and `merge`. Worktree create/apply/cleanup is absent at that SHA.
- **Hub ops are current.** Supported ops are `jobs`, `cancel`, `wait`, `send`, `inbox`, `list`, `start`, `ps`, `logs`, `stop`, `restart`, `describe`; park/revive lifecycle is absent, and process ops require the configured process backend.
- **`think` is always on** at supervisor root and child. Upstream gates it on external thinking; this is a known divergence.
- **Checkpoint/rewind are root-only and gated** by tools enabled, allowlist, a controller and `checkpoint.enabled`.

## Authorization and publication workflow

Current user authorization covers this Rust-port work only: bounded local checks (≤600 s, `jobs1`, no concurrent heavy runs), commits, pushes and GitHub Actions runs. It does **not** cover publishing, tagging, releasing, replacing the installed CLI, unrelated changes, or new dependencies. Heavy/full suites belong in GitHub Actions, not on the phone.

Current user decision on ownership: fixes to already-merged code may be performed by main as the single writer. A separate independent reviewer remains mandatory. Prefer the original executor for its own slice; if an executor must be revived or respawned, preserve and continue in its retained worktree — never reapply from a clean baseline or discard saved edits.

Exact workflow (run only when publication is intended):

```sh
cd /data/data/com.termux/files/home/omp-work
git status --short
git push origin feat/rust-agent-termux-bionic

gh run list --workflow rust-agent.yml --limit 5
gh run view <RUN_ID> --repo SklUpp/omp-termux
gh run watch <RUN_ID> --exit-status --repo SklUpp/omp-termux

ART="${TMPDIR:-$PREFIX/tmp}/omp-art-<RUN_ID>"
mkdir -p "$ART"
gh run download <RUN_ID> -n omp-aarch64-linux-android -D "$ART" --repo SklUpp/omp-termux
chmod +x "$ART/omp"

# The manifest records the build path, e.g.
#   <sha>  target/aarch64-linux-android/release/omp
# not the downloaded flat `omp`. Compare the first token against the file
# actually downloaded; do not use `sha256sum -c omp.sha256` here.
python3 - "$ART/omp.sha256" "$ART/omp" <<'PY'
import hashlib, pathlib, sys
manifest, binary = map(pathlib.Path, sys.argv[1:])
fields = manifest.read_text().split()
if not fields or len(fields[0]) != 64:
    raise SystemExit("manifest does not start with a sha256 hex digest")
expected = fields[0]
digest = hashlib.sha256()
with binary.open("rb") as handle:
    for chunk in iter(lambda: handle.read(1 << 20), b""):
        digest.update(chunk)
actual = digest.hexdigest()
if actual != expected:
    raise SystemExit(f"sha256 mismatch: {actual} != {expected}")
print(f"sha256 OK {actual}")
PY
file "$ART/omp"
```

Only `push` to `main` or `feat/rust-agent-termux-bionic`, PR to `main`, or `workflow_dispatch` triggers `rust-agent.yml`. `workflow_dispatch` runs the committed branch HEAD, so it cannot verify unpushed changes. After every push, locate/start CI for the exact pushed SHA, wait for terminal results, and report both host and Android jobs.

CI gate commands (from `.github/workflows/rust-agent.yml`):

```sh
cargo check --locked -p pi-agent -p omp-cli
cargo test --locked -p pi-agent -p omp-cli
cargo test --locked -p pi-grep-pattern
cargo test --locked -p pi-natives grep
cargo build --release --locked -p omp-cli --target aarch64-linux-android
```

Pinned CI toolchain is `nightly-2026-08-08`; `android/versions.env` says `RUST_TOOLCHAIN=nightly-2026-07-28` and `NDK_VERSION=r27c`, `ANDROID_API=24`. The toolchain mismatch is recorded, not proven to break the build; use the workflow pin for CI reproducibility.

## Runtime, registry and ownership contracts

Link to these instead of copying internals:

| Concern | Anchor | Contract |
|---|---|---|
| Tool trait / wire name | `crates/pi-agent/src/tools/mod.rs:90-98`; `EditTool` `:1020-1035` | Registry key stays internal (`edit`); `wire_name()` becomes `apply_patch` only in apply-patch mode. No aliases in registry keys or allowlists. |
| Registry lookup | `crates/pi-agent/src/tools/registry.rs:97-130` | `get` resolves internal name first, then a unique wire name; ambiguous wire matches return `None`. `names()` is the internal allowlist surface; `schemas()` emits wire names. |
| Root registry mount | `crates/omp-cli/src/runtime.rs:58-75`; `:1674` | `BUILTIN_TOOLS` plus `DELEGATION_TOOLS`; `build_registry_with_display` receives resolved session display/edit modes. Unknown names are a hard error. |
| Checkpoint / eval mount | `crates/omp-cli/src/runtime.rs:641-651`; `:656+` | Checkpoint pair is root-only and gated; eval mounts after the final registry so the bridge sees the real tool set and children do not inherit a root-bound host. |
| Supervisor registries | `crates/pi-agent/src/supervisor.rs:881`, `:908`, `:920` | Root gets `think` + task/hub; child drops `todo`/`ask`, adds `yield` + `think` + eval binder, no checkpoint. Child templates are rebound after allowlist filtering. |
| Session display rebind | `crates/pi-agent/src/supervisor.rs:482` | `rebind_session_tools` replaces read/ast_grep/edit/ast_edit/write with fresh snapshot/pending stores and the resolved display inputs, after filtering so `has_edit_tool` reflects the final registry. |
| Child provider ownership | `crates/pi-agent/src/supervisor.rs:571-585`; `crates/pi-agent/src/tools/task.rs:512,576` | A child eval host is rebound to the exact child-resolved provider; root provider must never answer child `completion(model="default")`. Covered by `task.rs` tests around `:2616-2703`. |
| Eval artifact publication | `crates/omp-cli/src/eval_host.rs:512`, `:546`, `:574-585` | Complete sibling is staged, then published by no-clobber atomic `hard_link`; `Ok(false)` means another writer owns the ID, so retry the next ID. No copy fallback by design. |

Termux app-private storage can deny `link(2)` with `EACCES`; the artifact path therefore needs on-device proof and must not be replaced by a silent copy.

Tool/wire conventions: internal registry keys and approval names stay stable; only model-facing schema names change (`apply_patch`). Approval `--allow edit` authorizes the wire alias. `search`→`grep` and `find`→`glob` are legacy input aliases, not extra tools. MCP tools use `mcp__<server>_<tool>` naming with fail-closed launch/approval gates.

## Agent pipeline and worktrees

Required pipeline: executor completes READY → separate independent reviewer → same executor fixes → focused recheck → parent integration → CI plus Android smoke. No integration acceptance before review and the runtime/device gate. One writer per shared file; parent integrates only after lanes stop.

Recovery rule: revive/respawn with the retained worktree, predecessor transcript and ownership contract. Do not start from a clean baseline, reapply already-reviewed edits, or delete saved work.

Worktree audit (2026-10-29): 9 isolated dirs were audited; the user removed 8 old dirs. Only this remains:

- `~/.omp/wt/td6ddbf803/m` — owner `EvalHostImplement`, retained accepted/integrated backup. HEAD `7c4d6d67cf`, 50 tracked files modified plus 16 untracked (`+8562/−1007` tracked), not the current branch. Do not merge from it, delete it, or treat it as source of truth; it preserves pre-integration eval work only.

Published HEAD is `abe3d8c274`; later commits are `088367d8b9` (unregistered `inspect_image` loader) and `260a314843` (FixOwnerClearRace owner-clear fix, regression unrun). The working tree also carries active slice edits (not counted) and these docs. Verify before acting:

```sh
git -C /data/data/com.termux/files/home/omp-work status --short
git -C /data/data/com.termux/files/home/omp-work rev-parse HEAD
```

## Historical FIX subset (source-done 2026-10-30; acceptance gates retained, not scope limit)

1. Reject every known upstream verb that is missing or excluded **before prompt assembly**, with a specific reason (missing vs boundary), zero side effects, and no session/network/JS start. Includes `omp acp` and all excluded verbs. **Source-done:** `main.rs:118-121` + `args.rs:409-497`, tests `:1231-1323`; Android smoke open.
2. Remove the `models --no-extensions` silent drop: honor it or reject it explicitly; keep `refresh`/`-e` explicit rejections intact. **Source-done:** honored by construction in `local_models.rs:91-103`; CLI invocation and Android smoke open.
3. Decide and enforce the LSP/DAP seam: register behind parsed settings or report the feature unavailable. Never leave source + config that looks supported but unreachable. **Source-done:** mounted behind `lsp.enabled`/`debug.enabled` (`runtime.rs:1995-2036,2103-2186`); per-ID acceptance open.
4. Give excluded tool paths a typed boundary error instead of generic not-found (`computer` and analogues). **Source-done for the selection/diagnostic path** (`registry.rs:11-20,82-88`); model-request path and Android smoke open.

These remain acceptance gates. Do not treat them as the scope limit, and after any further fix: bounded local check, then push and wait for the exact-SHA Actions run before recording it in the plan.

## Verification lessons — root causes and guardrails

Observed failure causes, not blame:

- **Stale fixtures encoded old behavior.** Live SSE pinned the pre-hashline read format; AST tests expected full source lines where previews render matched spans; delegation kept a negative `hub ps` case after the op shipped; protocol fixtures had wrong counters; bash tests pinned prose. Fix the fixture to the real contract; if the product is wrong, fix the product.
- **Test-only budgets were tighter than the contract.** A hardcoded 5 s eval probe override was too tight for cold Julia on CI; the test now uses the contract default instead of a local guess. No fixed duration is a portable guarantee: do not inflate global timeouts or skip tests to get green, and keep a dedicated short-timeout test with its own explicit budget.
- **Julia probe inherited user startup state.** Probe now uses `--startup-file=no --history-file=no --color=no -e 'exit(0)'` (`crates/pi-agent/src/eval/languages.rs`).
- **Shared environment/path state raced across tests.** Scope paths and env were isolated per test instead of mutating process globals.
- **MCP discovery assumed a single handshake.** Fixtures now accept reconnects during discovery.
- **Static-only review misses compilation.** Every reviewed layer needs `cargo check --tests`; missing imports and fixture call signatures reached CI before.
- **Do not weaken assertions.** Preserve output/byte/inode/cancellation contracts; correct invalid assumptions with independent derivation, not by deleting the check.

Local guards: `CARGO_BUILD_JOBS=1 nice -n 10`, `-j1 -- --test-threads=1`, timeout 600 s, no concurrent heavy runs. If local fails or remains unresolved at 10 minutes, move the full gate to Actions with iterative code fixes — not to unbounded local builds.

## Remaining full-parity gates

All still open:

- Bionic API24 ELF; two real model/tool turns; resume and fork.
- MCP stdio and HTTP; RPC native auth wiring and its consumer contract; ACP; task/hub delegation; SSH.
- External Chromium screenshot/run only if validated; zero JS PID before `eval.js`/`browser.run`, one managed PID during, cleanup on cancel/EOF.
- Cancellation cleanup; per-ID disposition for every CLI/tool/flag/MCP/provider row.
- Equal-history 8-turn and 30-turn CPU/PSS/startup comparison against the installed CLI; no performance claim without measurements.

Known blockers/limits: ACP is absent (`--mode acp|rpc-ui` parse-reject), while `--mode rpc` exists and passed a loopback smoke with native auth wiring and the metaharness consumer contract still open; known missing and excluded verbs are refused before boot, so `omp acp` no longer becomes a prompt. LSP/DAP are mounted behind `lsp.enabled`/`debug.enabled`, with per-ID acceptance open. Goal Mode and `GoalTool` are absent. Browser is blocked by the installed Chromium Bionic linker error; external interpreters and services are environment prerequisites; remote SSH verification was historically unreachable and must not be assumed. `inspect_image` is deferred by explicit user decision with no image/resvg/WebP dependency approved; its merged loader is unregistered and not counted. The 14 deferred wire dialects are `openai-completions`, `openai-responses`, `openrouter`, `openai-codex-responses`, `azure-openai-responses`, `anthropic-messages`, `bedrock-converse-stream`, `google-generative-ai`, `google-gemini-cli`, `google-vertex`, `ollama-chat`, `cursor-agent`, `gitlab-duo-agent`, `devin-agent`.

## Evidence hygiene

- Temporary proof lives under `$PREFIX/tmp` (`omp-rust-final.ciBYAL`, older `omp-rust-smoke.toZZRV` and smoke/probe dirs). These are ephemeral; re-download by exact SHA/run and never make them a build or documentation dependency.
- Do not copy credentials, API keys, tokens, account identifiers, host addresses, session databases, broker snapshots or bulk transcripts into docs, commits or logs. API-key tests use redaction/local loopback fixtures only.
- Apply-patch corpus: `packages/coding-agent/test/fixtures/apply-patch/scenarios/` has 21 end-to-end cases. Each case is a directory with `input/` (initial tree; absent for add-only cases), `patch.txt`, and `expected/` (final tree compared recursively, including directory entries and file text). The contract is documented in `scenarios/README.md`.
- Direct offline oracle: `bun test packages/coding-agent/test/core/apply-patch.test.ts` exercises those cases at `:212+` by copying `input/`, applying `patch.txt` with the TypeScript `applyLegacyPatch`, and comparing the full tree snapshot to `expected/`. No provider or network is involved. This oracle has not been executed in this docs pass.
- TS fixture ≠ Rust coverage: no current harness runs this corpus against Rust. Rust `apply_patch` coverage lives in `crates/pi-agent/tests/file_mutation_safety.rs` and `display_wiring.rs`; a passing TypeScript corpus does not establish corpus-wide Rust parity.
- Metaharness expects `omp --mode rpc` (`packages/metaharness/README.md`); Rust now has that mode (`output.rs:14-24`, `rpc_protocol` tests) but the runner has never been exercised against it. Until that consumer contract is verified, use direct CLI/PTY fixtures and do not present metaharness as a Rust runner.
- `android/scripts/bench.sh` measures the installed Bun CLI (`$LIB_DIR/bun` + `cli.js`), not Rust; it is not valid Rust performance evidence. `scripts/ask-pty-smoke.py` is the device PTY/cancellation/delegation gate. Test fixtures live under `crates/omp-cli/tests/fixtures` and `crates/pi-agent/tests/fixtures`.
- Historical plan blocks and local proof files stay attached to the SHA/artifact they measured. They do not verify newer commits.
- This repository has no `specs/` lifecycle tree. Do not scaffold one as a second status system; the plan and this handoff are the durable project state.

## Active execution checkpoint (2026-10-29)

Commit `11a3490fb34ec1477d52a02bbcc6a34e71a3c7c1` adds failure-only Julia diagnostics. Actions `40867084933`: Android build and native grep passed; host suite failed at the unchanged Julia 10-second availability probe (18 eval tests passed, 1 failed). Diagnostic probe used inherited environment and took 5.98 seconds; root cause remains unproven. No timeout bypass or skipped test.

Active pipelines: CLI boundary fixes with independent review; full SQLite read selectors in an isolated worktree; full LSP/DAP registration and ownership cleanup in another isolated worktree; failure-only inherited-versus-filtered Julia environment diagnostics. None of these unverified implementation pipelines closes parity rows yet. Preserve earlier user edits in this document and the plan. The user explicitly authorizes isolated agents with `repoRoot`, push to the working feature branch, and Actions; no release, tag, PR, package installation, or installed-CLI replacement is authorized.

## Active execution checkpoint (2026-10-30; supersedes earlier active status)

Full `docs/rust-cli-parity-plan.md` completion remains the objective; FIX-only queue is not the scope limit. No placeholders or temporary feature refusals count as implementation. Current source of truth is the live feature branch `feat/rust-agent-termux-bionic`; earlier claims above about absent RPC describe older checkpoints, not current source.

Verified current local block:
- Native auth lifecycle: 139 `omp-cli --lib auth::` tests passed (8.14 s). Browser callback/manual input completion joins the losing worker; per-dialog cancellation and registration-race regressions passed. Native auth backend exists, but RPC runtime auth integration and online provider catalog refresh remain unfinished.
- `nice -n 15 cargo test --locked --offline -j1 -p omp-cli --test rpc_protocol -- --test-threads=1`: 16 passed, 0 failed (0.52 s).
- `nice -n 15 cargo test --locked --offline -j1 -p pi-agent --test subagent_observability -- --test-threads=1`: 5 passed, 0 failed (0.09 s). Both commands completed in one successful serial shell run (139.45 s including compilation); warnings remain.
- Actual Android `target/debug/omp --mode rpc --no-session` smoke passed: register custom `notes` host URI scheme, model calls `read`, host receives `host_uri_request`, returns content, next provider request contains that content, agent ends, stdin EOF exits 0. Local loopback fixture only; no live provider authentication claim.

Root causes resolved in this block: RPC abort fixture released SSE before command consumption; tests now wait for the real aborted frame without weakening assertions. Observer fixture `Collect::take` cloned instead of draining; now drains. Supervisor event/progress and terminal lifecycle emission now share a gate before state inspection; deterministic cross-thread regression passes. Observer callbacks must not re-enter emitting supervisor methods; current observers were source-audited for this constraint.

Rejected smoke assumption: `notes://memo:2-3:raw` is opaque in upstream TypeScript too. No custom-scheme selector expansion was added. Successful host URI smoke uses `notes://memo`; it does not prove custom-scheme line selection.

Next: commit and push the accepted RPC/URI/auth-source block after scoped review, excluding unrelated user edits; exact-SHA Actions verification next. Then finish RPC native auth wiring (shared backend, startup DB errors must fail closed before ready/session), successful-login online catalog refresh, child provider/credential override semantics, and remaining full-plan gates including Goal Mode, budget, ACP and provider dialect coverage. Do not mark full parity complete from these scoped passes.

Last recorded published baseline: `93d`, Actions `40908155304`: Android build/native grep passed; host Julia suite failed after SIGINT. These results do not verify current uncommitted source. New current block has not yet been committed or pushed. Installed CLI remains untouched; no package install, release, tag or PR authorized.

Publication update: accepted block committed as `faad6b5355ceead19d702927559baea56379b577` (73 files). Push exited 1: GitHub GH013 push protection flagged Google OAuth client IDs/secrets in `crates/omp-cli/src/auth/flows/oauth_code/google.rs:57-58,67-68`. Remote branch was not updated and no exact-SHA Actions run exists for this commit. Do not bypass protection, obfuscate constants or print their values. Security review is checking upstream provenance before choosing a correction or requesting owner approval. Native RPC auth runtime wiring continues locally; publication remains a separate unresolved gate.

User decision (2026-10-30): postpone push. Earlier push/Actions publication authorization is suspended until renewed explicit approval. Continue local implementation and scoped verification; do not retry push or bypass GH013. No TS-auth backend cutover was approved; discussion established feasibility only.

Local S1–S4 native RPC auth integration is implemented after `faad6b5355` and source-reviewed: mandatory profile-resolved `agent.db` opens before session/ready, native provider listing/login replaces refusal stubs, explicit API key precedes store resolver and connection fallback, actual turn cancellation reaches credential resolution, auth cancellation/shutdown precedes UI teardown. Scoped protocol suite passed 18/18 (exit 0, 75.09 s including compilation). Actual Android CLI with an isolated configured model catalog listed `google-gemini-cli` as unauthenticated from a fresh DB and exited 0; corrupting that DB made startup exit 1 with no stdout/ready frame. An initial smoke without a model catalog stopped at the existing no-model prerequisite; the corrected smoke supplied the catalog. Live Google login remains untested. S5 online provider discovery and S6 per-child model/credential construction remain unfinished; no no-op discovery implementation or new logout wire command was added. Provider-listing regression is being tightened from presence/type checks to explicit fresh-store authentication state.

Subsequent auth-owner changes modified Google/Anthropic constant decoding and made `Client::config` fallible. A scoped compile exposed `E0308` because its match still returned `Config`; parent fixed only the return wrapper to `Ok(match ...)` in `auth/flows/oauth_code/google.rs:62-86`, preserving the owner's other edits. Verification is running; earlier passing results do not certify these newer changes. Push remains postponed; no secret-scanning exception, publication or history rewrite was performed.

Compile correction verified: `nice -n 15 cargo test --locked --offline -j1 -p omp-cli --test rpc_protocol login_providers_come_from_the_process_credential_store -- --test-threads=1` exited 0 (42.97 s including compilation; 1 passed, 17 filtered). The strengthened regression asserts the specific Gemini provider is unauthenticated in a fresh store. This proves the E0308 correction compiles and that RPC path passes, not real OAuth token exchange or all new decoding tests. Existing warnings remain.

Latest user authorization supersedes the earlier push pause: try normal push, integrate/commit remaining saved work, and audit worktrees for possible cleanup. No deletion or force-push authorized. Normal push of `faad6b5355` again exited 1 with GH013 against that historical commit; changing later source does not remove earlier history. Latest serial local checks exited 0: auth 141/141, RPC protocol 18/18 (90.61 s including compilation). Fresh actual CLI smoke again verified provider listing (exit 0) and corrupt DB rejection before any stdout (exit 1). Review and retained-tree integration audit are in progress; no saved checkout has been removed.

Final pending-block verification after review fixes: auth tests passed 140/140; RPC protocol passed 19/19, including interactive-login compact/handoff rejection and EOF cleanup (serial command exit 0, 95.99 s including compilation). Independent review accepted the scoped changes. The one-test count reduction removes redundant credential-literal copying, not behavior coverage. Manual compact/handoff now reject during login; automatic compaction skips that boundary before emitting a start frame. Detailed retained-directory evidence is in `docs/worktree-audit-2026-10-30.md`. No directory or snapshot ref has been removed.

Pending main-tree changes committed as `077bac1709` (`feat(auth): connect RPC credentials and prevent login compaction deadlocks`, 13 files). Normal push of this new HEAD exited 1: GH013 still flags Google OAuth material in ancestor `faad6b5355`. No remote update, no bypass, no rewrite. Worktree snapshots remain preserved; integration audit distinguishes content already ported from actual ancestry.









Priority update from user: authorization work has lower priority. Preserve the existing uncommitted child-credential slice in `runtime.rs`, but defer further auth/discovery changes. First restore CI, then continue core tools, delegation, sessions, CLI modes/flags and live events in the plan's order.

Published HEAD `abe3d8c274` was confirmed against the remote. Its Actions run `40944110426` passed Android build and native grep, but host tests failed: `output::tests::json_mode_tool_batch_printable_envelope`, `output.rs:647`, expected a string where actual output carried a structured text-content object. Failure is established; investigate the TS consumer contract before changing output or assertions. No full CI pass claimed.

Physical-copy audit completed: neither retained snapshot needs integration; details in `docs/worktree-audit-2026-10-30.md:69`. Neither copy nor build cache has been removed. Cleanup discussion did not authorize deletion.

JSON output root cause resolved: tests still expected pre-parity string results, while TS emits complete `AgentToolResult` objects. Production output stayed unchanged; assertions now cover text/image/details and the live SSE reader uses the structured text content. Local unit test passed 1/1; live SSE integration passed 2/2, including interruption/resume. Actual JSON CLI smoke completed a localhost-provider `read` turn, preserved structured content in `tool_execution_end`, continued the provider turn and exited 0.

Local integration linking initially hit the known Android LLVM tagged-pointer abort. Re-linking only the test target with `cargo rustc --locked --offline -j1 -p omp-cli --test live_sse_integration -- -C link-arg=-Wl,--threads=1` succeeded; the resulting test binary exited 0. This is not an exact-SHA full CI pass.

Next core slice selected from actual source: `task isolated:true` still rejected at published `abe3d8c274` (`task.rs:1461-1472`). Existing `pi-iso` PAL and TS baseline/capture/merge/owner lifecycle are the implementation basis. Required: preserve parent dirty baseline, execute in sandbox, capture child-only changes, retain recoverable output on conflicts, and clean owned resources on cancellation. Authorization work stays lower priority. The active `TaskIsolationSlice` now modifies `crates/pi-agent/src/tools/task.rs` in the working tree; it is unverified, and this handoff counts neither the old rejection nor any new isolation behavior.

Image inspection decision (2026-10-30): the user deferred `inspect_image`. No image decoder, resvg or WebP dependency is approved. The merged partial loader (`088367d8b9`) stays unregistered and must never be counted as implemented or accepted; the plan's tool row and §6 record the deferral. Read-side image bounds and header sniff remain source facts only.

Docs provenance for this reconciliation: presence claims reflect the published code basis `abe3d8c274` plus later commits `088367d8b9` (loader unregistered) and `260a314843` (FixOwnerClearRace, regression unrun); neither changes presence counts. Behavior stays unknown unless a recorded run exists, and no runtime verification was performed by the docs writers.


Isolation acceptance remains blocked. Initial `cargo check --locked --offline -j1 -p pi-agent -p omp-cli` failed on an unavailable xxh3 feature; writer switched to the enabled xxh64 API. The next check passed `pi-agent` checking but exited 101 on nine CLI config type errors (heterogeneous merge tuple array and nested `Option<bool>`). Writer owns the fixes; no new successful compile claimed.

Independent review identified and drove corrections for baseline-object reachability, uncaptured-result retention, merge-lock lifetime and terminal ordering. Still under correction: branch capture of a commit followed by an uncommitted revert, cancellation before queued merge mutation, and private metadata for initialized submodules. No task-isolation runtime acceptance yet. Legacy session import now preserves validated ordered user parts and canonical stop reasons; independent source review accepted it, but its regression and CLI smoke are not yet run.


Latest verification supersedes the preceding blocked status: `cargo check --locked --offline -j1 -p pi-agent -p omp-cli` exited 0. Isolation regressions passed 11/11 (9.65 s): dirty baseline/binary deltas, committed-plus-uncommitted and net-zero branch cases, cleanup/cancellation, merge conflict/lock lifetime, private nested and submodule metadata. Parent-cancellation forwarding regression passed 1/1. CLI config regressions passed 2/2; late-owner clear refusal passed 1/1; legacy import/reopen/fork preservation passed 1/1. Independent targeted safety/contract rechecks found no remaining reviewed must-fix; these are scoped results, not full parity or CI.

Actual freshly linked Android CLI smoke passed against an isolated localhost model fixture: root calls `task isolated:true`, child writes inside the rcopy sandbox, patch merges into the parent, sandbox is cleaned, CLI exits 0 (four provider requests). No installed CLI was replaced. Android LLVM tagged-pointer abort interrupted one test relink; single-thread linker commands succeeded and the resulting binaries were exercised directly. Current source still needs exact-SHA CI; full local suites were not run.

Core slice published as `5fdcf5ebee`; exact-SHA Actions `40960858745` passed Android build/native grep but failed host `pi-agent --lib` (355 passed / 2 failed): immutable-source fixture used an unmounted `artifact://`, and internal URL selector test returned all lines. Root contract investigation is active; no full green CI claimed.

Actual current CLI MCP stdio acceptance passed with isolated localhost fixtures: initialize/initialized/list/call, marker returned into model continuation (exit 0), RPC abort of a pending call, EOF during a pending call, and child PID reaping. Cancellation was client-side; no `notifications/cancelled` reached the fixture. HTTP/SSE MCP and interactive approval remain unverified. Duplicate cancellation text was observed in `tool_execution_end` but not the persisted tool result; not yet investigated. Source remains unchanged by these smoke checks.

Read CI failures were stale fixtures, not production regressions: immutable reads now mount actual handlers; bare `:N` remains open-ended, and exact multi-range uses `:1-1,4-5`. All 11 read display regressions passed (exit 0). RPC duplicate error rendering now deduplicates exact equal result/error only, preserving distinct context, images and details; regression passed 1/1. Independent source review accepted both fixes. Fresh CLI MCP abort confirmed exactly one terminal frame, one turn end, one cancellation string in each, exit 0 and child reaped.

MCP transport acceptance extended: HTTP JSON-response and legacy SSE initialize/list/call marker turns passed; both pending-call abort/close cases passed. Cancellation remains client-side. HTTP SSE-response bodies, session negotiation, auth/redirects/timeouts and interactive approval were not exercised. Early SIGINT before any provider response byte passed 5/5 synchronized runs; synthetic aborted assistant persisted, then resume preserved one prompt, linear parents and the same session ID (exit 0). No data-loss bug found in that scenario.

Actions `40963460079` for published `2d79b976ca` passed Android/native grep, `pi-agent --lib` 357/357 and `agent_core` 96/96, then failed `async_coordination` 7/12. Cause: background event forwarding selects provider streaming, but two old test providers returned `stream unused in test`. Fixture-only stream implementations preserve the existing gates and assertions; all 12 coordination tests now passed locally (exit 0, 0.32 s). Full CI not rerun yet.

Actual LSP diagnostics/refresh and DAP launch/stack/evaluate/explicit terminate passed with stdlib adapters. Normal CLI exit nevertheless skips graceful teardown: static runner ownership prevents Drop; LSP receives EOF without shutdown/exit, and an EOF-ignoring DAP adapter survived until fixture cleanup. Explicit runner shutdown is being implemented and independently reviewed; no fixed-runtime claim yet. `--continue <uuid>` normalization is integrated but not yet exercised in the fresh CLI; multi-message UUID fallback remains under contract review.

Coordination fixture correction was committed and published as `84dcc13ee6`; Actions `40966213788` is the current exact-SHA run. Two localhost-provider background-task scenarios also passed, without live credentials. Subsequent `continue` and runner shutdown changes remain under acceptance; publishing a previous slice does not certify them.

MCP cancellation research correction: this client negotiates protocol `2025-11-25`, where cancellation is optional and explicit HTTP cancellation notification is SHOULD, not the draft transport rules previously reported. Both TS and Rust currently cancel locally and ignore late responses. No parity regression or mandatory stdio-notification requirement was established for the pinned version. See https://modelcontextprotocol.io/specification/2025-11-25/basic/utilities/cancellation and https://modelcontextprotocol.io/specification/2025-11-25/basic/transports .

User reiterated integration requirements: accepted work must be merged, committed, pushed and checked against its published SHA. Agent instructions are now written in clear English; user-facing reporting stays in Russian. Full plan remains active; authorization has lower priority and inspect_image remains deferred.

Actions `40966213788` for `84dcc13ee6` passed the corrected coordination suite (12/12), then failed six delegation tests (4/10 passed). The delegation HTTP mock ignored `stream:true` on child requests and returned ordinary JSON, causing `stream ended before completion` before yield/tool execution. Fixture-only SSE conversion preserves response routing, text and tool-call IDs/arguments; no assertion or production behavior changed. Independent source review accepted it. The linked Android delegation regression binaries all reported 10/10 PASS (exit 0); the executable glob also ran three retained binaries, so this is not four distinct coverage sets. Fresh exact-SHA CI remains pending.

Continue raw-argv normalization is independently source-accepted, including first literal continue flag, exact prompt membership and preserved follow-up. Shutdown source review cleared the cancellation-unsafe global timeout and Tokio-worker join deadlock. Acceptance is still blocked by two newly confirmed unbounded graceful I/O stages in DAP dispose and eval kernel shutdown; their owner is fixing those before fresh-binary adapter checks. Goal persistence implementation runs in an isolated worktree; full Goal runtime and ACP remain unfinished requirements.

Subsequent shutdown correction bounds DAP writer-lock/flush and eval stdin-lock/exit writes individually, then always continues kill/reap; DAP dispose now awaits its bounded reap instead of detaching it. Independent source review accepted the fixes. Fresh transport regressions passed 1/1 each with explicit oneshot readiness (exit 0); shutdown latch regression passed 1/1. Fresh Android CLI DAP exit smoke passed: `initialize, launch, configurationDone, stackTrace, evaluate, terminate, disconnect`; EOF-ignoring adapter PID was already gone at exit, no fixture kill (exit 0). LSP print shutdown/exit passed; corrected RPC and single-worker signal scenarios are still running.

Fresh continue normalization checks passed 7/7 (including one existing newest-session check), and actual CLI unknown-UUID cases passed with and without a follow-up prompt: exit 2, exact session-not-found diagnostic, empty stdout and no session JSONL writes. The initial smoke lacked a model catalog and failed before selector resolution; only the isolated fixture prerequisite was corrected. Evidence retained at `$PREFIX/tmp/omp-continue-uuid.cN634g`.

Goal persistence auto-integrated as `166776e1ca` before parent acceptance. Parent added the missing module export, repaired two JSON test delimiters and a test temporary-borrow error, and rejected oversized float-to-u64 saturation. Independent source recheck accepted the narrow corrections; branch/reopen/fork and validation regressions passed 2/2. This is persistence only: no Goal tool registration, runtime accounting, budget bridge or headless continuation is claimed. Authoritative TS goal delta excludes cacheRead (input + cacheWrite + output); the older research sum including cacheRead was wrong.

LSP fresh-binary acceptance completed: print, RPC EOF and single-worker RPC EOF exited 0; idle SIGINT with one Tokio worker exited 130 in 66 ms. Each scenario observed exactly one `shutdown` response, one `exit`, mock exit and child PID disappearance; the provider received diagnostics in its second request. A stale-output FIFO race in the temporary harness initially closed stdin mid-turn; current-output truncation fixed the harness without changing production or weakening the oracle. Logs retained under `$PREFIX/tmp/omp-lsp-shutdown-recheck/runs/{print,rpc,rpc-w1,sigint-w1}/`. Continue and runner teardown are accepted for this scoped behavior; full plan and exact-SHA CI remain open.

