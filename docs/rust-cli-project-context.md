# Rust CLI project context — cold-start handoff

Read this first, then use [`rust-cli-parity-plan.md`](rust-cli-parity-plan.md) as the authoritative inventory and evidence ledger. This file is orientation only; it deliberately does not duplicate the plan's per-ID audit tables, provider matrix, flag tables or dated execution history.

Repository: `SklUpp/omp-termux`, branch `feat/rust-agent-termux-bionic`, worktree `/data/data/com.termux/files/home/omp-work`. Upstream reference checkout: `packages/` (TypeScript) in the same tree.

## Goal and boundaries

Build a separate Rust `omp` (`crates/omp-cli`, target Termux arm64/Bionic) that owns the CLI, agent loop, sessions, tools, auth and protocols. Do not replace the existing Bun-based Android release or the installed `$PREFIX/bin/omp` until final acceptance. Do not modify `install.sh`, `android-release.yml`, or packaging.

Rust must not start a JavaScript runtime for general operation. JS is allowed lazily only for `eval(language="js")` and `browser.run`. External interpreters, Chromium, SSH, provider accounts and desktop backends are external prerequisites; absence must produce a precise error, never a fabricated success or silent fallback.

Approved exclusions remain: extension/hook/plugin execution and `plugin`/`install`; interactive TUI, `rpc-ui`, full-screen `git`/`join`/`shell`; native desktop `computer`; local audio capture/playback; live WebRTC; image clipboard; `update` without an Android release target. Excluded paths still need an explicit refusal instead of a generic failure or prompt absorption.

Work is **FIX-only** until the recorded defects are closed: no new capability, no placeholders, no no-op flags, no new dependency/install without approval. Latest priority: safety/data-loss fixes first, then tools, delegation, CLI modes/flags and live events; keep the existing OpenAI-compatible provider path functional and defer the 14 provider dialects.

## Current verified state (2026-10-29)

| Item | State |
|---|---|
| Docs HEAD | `216179883b` — docs-only parity reconciliation; no code change. The handoff edits in this working tree are uncommitted pending review. |
| Last tested executable | `0f601b6f61` — Actions [40827646802](https://github.com/SklUpp/omp-termux/actions/runs/40827646802), all 3 jobs pass: host check/tests, native grep/matcher, Android arm64/Bionic build |
| Android artifact | SHA-256 `d79bdbb3c0e407f3fb5f7156605c8fbe08d9836751f81e77f566105125af630f`; `--help`/`--version` exit 0; 5 PTY scenarios PASS in 10.36 s |
| Artifact path | `$PREFIX/tmp/omp-rust-final.ciBYAL/omp` — ephemeral evidence, not a durable dependency |
| Local verification | Local Cargo hit the 600 s dependency timeout under `jobs1`; no local test pass is claimed for `0f601` or the docs HEAD |
| CI coverage of docs HEAD | None. Docs commit did not change code and was not re-run; do not claim it is CI-verified |

Artifact-level help/version/PTY checks are not a per-ID command matrix. Every parity row stays **acceptance-open** unless the plan records that ID's own invocation, arguments, typed result, error/cancel path and Android smoke. Shared green CI does not close rows.

### Implementation-axis truth (summary)

- Core present: CLI/agent loop, live bounded events and cancellation, session storage plus continue/resume/fork paths, approvals, read/write/edit (hashline plus variants), bash/process supervision, glob/grep, AST grep/edit, todo, ask, MCP client, task/hub delegation, checkpoint/rewind, `yield`/`think`. Foreign session import readers exist with non-TTY rejection. This is presence/reachability, not per-ID acceptance or full TypeScript equivalence.
- CLI: 41 TypeScript subcommand IDs → 8 present (2 implemented, 6 partial), 27 missing, 6 excluded; Rust-only `repl`. Missing/excluded verbs currently fall through to the positional prompt unless explicitly rejected.
- Tools: 32 registry IDs → 14 reachable, 5 partial/unwired, 12 deferred, 1 excluded (`computer`); `apply_patch` is a wire alias of `edit`, not a 33rd ID.
- Wire APIs: 14 dialects deferred, retained requirement, none excluded.
- MCP: stdio/HTTP/SSE client paths exist; OAuth unsupported.
- Flags: string 17 present / 9 missing / 4 excluded; optional 1 partial; valueless 11 present / 12 missing.

### New audit truths that must not be lost

- **LSP and DAP are source-present but unmounted.** `pi-agent` exposes `lsp` and `dap`; `lsp.enabled`/`debug.enabled` are parsed, but no `LspTool`/`DebugTool` registration exists in `crates/omp-cli`. Configuration is not a mounted tool.
- **Eval is partial.** `eval` is late-mounted; the eval bridge strips `eval`/`task`/`hub`, rejects `checkpoint`/`rewind`, and `budget()` fails closed because there is no Goal Mode accounting. There is no Rust `GoalTool`.
- **Task supports blocking and background execution.** With a background-ready supervisor, non-blocking agents spawn as detached jobs that return real job IDs and auto-deliver exactly once unless a settled `hub jobs`/`wait` snapshot consumes the result first; blocking agents run inline. Covered by `crates/pi-agent/tests/async_coordination.rs` and `crates/pi-agent/src/tools/task.rs` (`spawn_background_task`/`run_background_child`). `task` supports `outputSchema`, `schemaMode` and `effort`; it rejects `isolated` only. Eval `agent()` additionally rejects `apply` and `merge`. Worktree create/apply/cleanup is absent.
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

Current main HEAD is `216179883b`; only this docs handoff and its plan link are uncommitted. Verify before acting:

```sh
git -C /data/data/com.termux/files/home/omp-work status --short
git -C /data/data/com.termux/files/home/omp-work rev-parse HEAD
```

## Next FIX-only queue (priority order)

1. Reject every known upstream verb that is missing or excluded **before prompt assembly**, with a specific reason (missing vs boundary), zero side effects, and no session/network/JS start. Includes `omp acp` and all excluded verbs.
2. Remove the `models --no-extensions` silent drop: honor it or reject it explicitly; keep `refresh`/`-e` explicit rejections intact.
3. Decide and enforce the LSP/DAP seam: register behind parsed settings or report the feature unavailable. Never leave source + config that looks supported but is unreachable.
4. Give excluded tool paths a typed boundary error instead of generic not-found (`computer` and analogues).

Do not silently narrow this queue. After each fix: bounded local check, then push and wait for the exact-SHA Actions run before recording it in the plan.

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
- MCP stdio and HTTP; RPC and ACP; task/hub delegation; SSH.
- External Chromium screenshot/run only if validated; zero JS PID before `eval.js`/`browser.run`, one managed PID during, cleanup on cancel/EOF.
- Cancellation cleanup; per-ID disposition for every CLI/tool/flag/MCP/provider row.
- Equal-history 8-turn and 30-turn CPU/PSS/startup comparison against the installed CLI; no performance claim without measurements.

Known blockers/limits: RPC/ACP protocols are absent (`--mode rpc|acp|rpc-ui` parse-reject; `omp acp` still becomes a prompt); LSP/DAP unmounted; Goal Mode absent; browser blocked by the installed Chromium Bionic linker error; external interpreters and services are environment prerequisites; remote SSH verification was historically unreachable and must not be assumed. The 14 deferred wire dialects are `openai-completions`, `openai-responses`, `openrouter`, `openai-codex-responses`, `azure-openai-responses`, `anthropic-messages`, `bedrock-converse-stream`, `google-generative-ai`, `google-gemini-cli`, `google-vertex`, `ollama-chat`, `cursor-agent`, `gitlab-duo-agent`, `devin-agent`.

## Evidence hygiene

- Temporary proof lives under `$PREFIX/tmp` (`omp-rust-final.ciBYAL`, older `omp-rust-smoke.toZZRV` and smoke/probe dirs). These are ephemeral; re-download by exact SHA/run and never make them a build or documentation dependency.
- Do not copy credentials, API keys, tokens, account identifiers, host addresses, session databases, broker snapshots or bulk transcripts into docs, commits or logs. API-key tests use redaction/local loopback fixtures only.
- Apply-patch corpus: `packages/coding-agent/test/fixtures/apply-patch/scenarios/` has 21 end-to-end cases. Each case is a directory with `input/` (initial tree; absent for add-only cases), `patch.txt`, and `expected/` (final tree compared recursively, including directory entries and file text). The contract is documented in `scenarios/README.md`.
- Direct offline oracle: `bun test packages/coding-agent/test/core/apply-patch.test.ts` exercises those cases at `:212+` by copying `input/`, applying `patch.txt` with the TypeScript `applyLegacyPatch`, and comparing the full tree snapshot to `expected/`. No provider or network is involved. This oracle has not been executed in this docs pass.
- TS fixture ≠ Rust coverage: no current harness runs this corpus against Rust. Rust `apply_patch` coverage lives in `crates/pi-agent/tests/file_mutation_safety.rs` and `display_wiring.rs`; a passing TypeScript corpus does not establish corpus-wide Rust parity.
- Metaharness is blocked on RPC: its native Terminal-Bench runner expects `omp --mode rpc` (`packages/metaharness/README.md`), while Rust has no RPC mode. Use direct CLI/PTY fixtures until RPC lands; do not present metaharness as a Rust runner.
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

