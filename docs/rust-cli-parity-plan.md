# Rust CLI parity plan

Status: plan and inventory updated 2026-10-29 against accepted `0f601b6f61`; execution continues only through the FIX-only queue below. Target: separate `omp-rs` artifact on Termux/aarch64 Bionic. Rust owns CLI, agent, sessions, tools, auth, and protocols; user-requested JavaScript runs lazily only in `eval(language="js")` and `browser.run`. Existing Bun-based Android release and installed `$PREFIX/bin/omp` remain unchanged until final acceptance.

## Current snapshot (2026-10-29)

- **Accepted baseline.** Commit `0f601b6f61`; [Actions 40827646802](https://github.com/SklUpp/omp-termux/actions/runs/40827646802) passed all 3 gates (host checks/tests, native grep/matcher, Android arm64/Bionic build). Downloaded Android artifact SHA256 `d79bdbb3c0e407f3fb5f7156605c8fbe08d9836751f81e77f566105125af630f` (`$PREFIX/tmp/omp-rust-final.ciBYAL/omp`): `--help` and `--version` exit 0; 5 PTY scenarios PASS in 10.36s. These are artifact-level checks, not a per-ID command matrix.
- **Local verification this pass: none.** Local Cargo hit the 600s dependency timeout; no local test pass is claimed. Test anchors below are source-tree assertions, not executions.
- **Two axes, never merged.** *Implementation/reachability* = code exists and a CLI/supervisor path can call it. *Acceptance* = that ID's own invocation, arguments, typed result, failure/cancel path and Android smoke are recorded. Shared CI green does not close any row. Every ID below remains acceptance-open unless explicitly marked excluded.
- **Reachable is not parity.** 14 of 32 tool IDs are reachable, but several are subset-level; 5 are partial/unwired, 12 deferred, 1 excluded. That is *not* tool equivalence, CLI parity, or provider parity.
- **Known-unwired despite source.** LSP and DAP sources plus parsed `lsp.enabled`/`debug.enabled` exist, but no `LspTool`/`DebugTool` registration in the CLI path. Parsed configuration is not a mounted tool.
- **Eval is partial.** `eval` is late-mounted, but the bridge strips `eval`/`task`/`hub`, rejects `checkpoint`/`rewind`, and its `budget()` fails closed (no Goal Mode accounting).
- **Delegation gaps.** `task` runs synchronously inside the call; it supports `outputSchema`/`schemaMode`/`effort` and rejects `isolated` only. Eval `agent()` additionally rejects `apply`/`merge`. `pi-iso` exists but no worktree wiring. `hub` ops are current (see row); no park/revive; process ops need a backend.
- **Provider dialects remain deferred, not excluded.** 14 wire dialects are a retained requirement.
- **Performance unmeasured**; the full parity gate stays open.

## Definition of done

Every user-facing TypeScript CLI mode, flag, subcommand, builtin tool, provider dialect, and configuration/session path must have one disposition: equivalent Rust behavior, or an explicit exclusion below. A rejected option must fail with a specific message; silently accepting an option with no effect is not parity. Each implemented class needs a consumer-visible test, Android build, and on-device smoke of its actual path. Compatibility fixtures must use real TypeScript session and protocol output, not source-text assertions.

Authoritative inventories are `packages/coding-agent/src/cli-commands.ts` (subcommands), `packages/coding-agent/src/cli/flag-tables.ts` (flags), `packages/coding-agent/src/tools/builtin-names.ts` (tools), `packages/ai/src/api-registry.ts` (wire dialects), and `packages/coding-agent/src/config/mcp-schema.json` (MCP). Before closing any phase, compare those registries against Rust CLI help and its behavior tests. New upstream entries remain open until classified.

## Boundary

Excluded: extension/hook/plugin execution and `plugin`/`install`; interactive TUI, `rpc-ui`, full-screen `git`/`join`/`shell`, native desktop `computer`, local audio capture/playback, live WebRTC, image clipboard, and `update` without an Android release target. Headless `gallery` ANSI, file/ANSI `render`, cloud TTS and image generation remain in scope. `browser-relay serve` remains conditional on an external Chrome connection. Explicit requests for excluded paths must fail with a specific reason, without side effects. External interpreters, Chromium, and services remain external; JS execution starts only on the user-requested JS paths. No new dependencies, installs, publication, or replacement of the installed CLI without separate approval.

Current defect: excluded verbs and most missing verbs are not rejected explicitly — they fall through to the positional `PROMPT` (see FIX queue).

## Current baseline and high-risk gaps

Baseline behavior recorded 2026-10-26, anchors re-pointed 2026-10-29 after refactor: `crates/omp-cli/src/runtime.rs:706-899` (`PiAgentRunner::run`) forwards live bounded `AgentLoop::run_dialog_message` events and emits `AgentEnd` from finished turns; preserve and test this user-modified path, not a synthetic replay. The provider path still constructs `OpenAiCompatibleProvider` at `runtime.rs:530`; seven tools and existing session/approval implementations need behavioral parity audits. User changes in `crates/omp-cli/src/config.rs`, `runtime.rs`, `tests/mcp_integration.rs`, and this document must be preserved. Existing release installer/workflow and `$PREFIX/bin/omp` are out of scope until acceptance.

High-risk defects confirmed by the 2026-10-29 read-only audits:

1. **Known upstream verbs become prompts.** `Option<Command>` plus positional `PROMPT` (`crates/omp-cli/src/args.rs:25-28,169-170`; `main.rs` falls through when `command.is_none()`) means `omp acp|config|plugin|…` is silently accepted as a prompt instead of a usage or boundary error. This violates the DoD and the Boundary.
2. **`omp models --no-extensions` is a silent no-op.** Accepted on `Command::Models` (`args.rs:443-445`) and dropped via `..` (`main.rs:170`). Contrast: `-e` is rejected explicitly (`local_models.rs:73-82`, test `explicit_extension_paths_are_rejected`).
3. **LSP/DAP settings are unreachable.** `pi-agent/src/lsp/mod.rs:3-9` describes registration behind `lsp.enabled`; no CLI registration exists. `dap`/`debug` likewise present in source, absent from `BUILTIN_TOOLS` (`runtime.rs:58-75`) and `build_registry` (`runtime.rs:1696-1731`).
4. **Excluded paths get a generic failure.** `computer` has no agent-side tool, so a model request would get "tool not found" rather than the boundary reason. Same class: excluded verbs fall into prompts.

## Execution order and acceptance

Priority update 2026-10-28: finish safety and data-loss fixes already in progress, then prioritize tools, delegation, CLI modes and flags, and live events. Defer porting frequently changing upstream code, including provider dialects; retain the existing provider path for testing and necessary correctness fixes. Browser and other rare capabilities follow later; the desktop exclusion remains unchanged. Deferred work and all acceptance criteria remain open.

Porting priorities: first complete core file/process tools, task/hub supervision and hidden coordination tools, ask/todo, live events, approvals, cancellation, sessions, headless CLI and MCP. Then port LSP, AST, debugging, checkpoint/rewind and on-demand eval, preserving existing work and rejecting unsafe operations. Later, as prerequisites permit, port remaining headless commands, GitHub/SSH/search/security integrations, image inspection, memory and skills; browser remains later and requires external Chromium. Defer frequently changing provider/auth implementations and rare cloud paths; keep the current provider functional. Excluded desktop, interactive UI, extensions and other boundary paths remain explicitly rejected, not silently ignored. This ordering does not waive the existing acceptance gate.

1. **Live execution and interruption.** Preserve the existing bounded live event channel in `runtime.rs`, test incremental NDJSON before provider completion and Ctrl-C during a real tool call, then fix any observed cancellation/history failures. No post-hoc synthetic replay. Final text exactly once; each started tool call closes with one result.
2. **Sessions and output contract.** Compare a real TypeScript JSONL session with Rust records. Read/write the per-directory layout, resume, fork, import, and provider-session fields without losing old Rust sessions; migration must be explicit and loss-safe. Match text/JSON error and exit semantics with real successful, interrupted, and failed turns. Preserve `--continue` behavior or document an intentional replacement before changing it.
3. **MCP and tool boundaries.** Consume the existing JSON configuration paths and schema; implement initialization, discovery, execution, cancellation, and process cleanup over stdio and HTTP/SSE. Use the upstream protocol version and `mcp__<server>_<tool>` naming; handle failures as tool errors, not empty results. Exercise a real local stdio server and HTTP fixture. Resolve the dynamic `Tool::name` contract before registration.
4. **Files and model payloads.** Expand `@file` using upstream 5 MiB text and 25 MiB image limits; validate format by content, preserve path/error context, and use model-supported image parts. Test text files, supported images, oversize, unreadable paths, and a provider request containing image data. Do not add an image decoder unless resizing becomes necessary for compatible requests.
   *Positional `@file` handling:* `@file` arguments separated into `file_args`, resolved relative to `cwd`, and formatted as `<file name="<abs_path>">\n{content}\n</file>\n` into the initial prompt. 5 MiB text limit (`MAX_AUTO_READ_TEXT_BYTES`) warns to stderr and emits `(skipped: too large, ...)`. Directories emit directory listing. Prevents falling into REPL (`cli.file_args.is_empty()`); `-p @file` does not wait on terminal stdin; piped stdin follows TS `readPipedInput` contract (reads to EOF if non-terminal, returns immediately if terminal TTY) and merges ahead of `@file` text (`stdin + "\n" + file_text + prompt[0]`).
   *Limitations and upgrade paths:* Images (PNG/JPEG/GIF/WebP detected by header sniffing) bounded to 25 MiB and attached to model wire requests as data URIs with text references; session log persists ordered text and image parts and preserves them across session resume; explicit text-only models fail closed; markit document converters (.pdf/.docx) omitted until markit integration.
5. **Provider and authentication parity.** Add the 14 TypeScript wire dialects, model capability routing, credential discovery/refresh, and thinking/effort options by protocol family. Reuse existing HTTP/TLS support; external OAuth flows and provider-specific attestation must have explicit integration tests before being called supported. Test representative request/response streams for every dialect; run live provider checks only where credentials are available, without logging secrets.
6. **Tools and delegated execution.** Port all Android-available builtin tools by observable function, including research, subagents, process/PTY, code intelligence, image inspection, storage, and remote integrations. JavaScript in `eval` uses a lazy persistent kernel only when explicitly called; `browser.run` executes page JS via external Chromium/CDP. No full JS agent. External interpreters, executables, or accounts must produce accurate prerequisite errors. Test concurrency, cancellation, approvals, trust boundaries, and typed tool-result payloads.
7. **CLI flags, modes, and subcommands.** Add upstream flag semantics, settings precedence/profiles, shell-friendly REPL, `rpc` and `acp`, then native-capable command families in dependency order: local read/grep/config/models/session, broker/auth, web/browser, stats/storage, and remaining commands. Do not add placeholder subcommands or accept no-op flags. For network/cloud commands, exercise either a local protocol fixture or a configured live service. Any command excluded because it *only* launches a React view must fail explicitly with the boundary reason.
8. **Parity gate.** Reconcile each upstream registry entry with implemented behavior or the approved exclusions. Run host tests and real CLI scenarios, Android arm64/Bionic build in Actions, and final Termux smoke of MCP, file/image, session, interrupt, provider, and tool delegation. Update Android docs only for shipped behavior; no release packaging without approval.

## Dependency and risk decisions

- Start with live events: MCP, RPC, subagents, and REPL otherwise need a second event model.
- Port session format before exposing resume/fork: silent writes to a parallel session tree risk apparent data loss.
- Preserve the platform boundary: no full Bun/Node agent fallback, no synthetic success, no ignored flags. Lazy JS kernel for `eval.js` and page JS for `browser.run` are expressly included; outside these requests JS must not start. TS compatibility is measured at behavior/protocol boundaries, not by matching internal class graphs.
- Use built-in/native or already-installed dependencies first. Proposed Rust crates or Termux packages require user approval before installation or lockfile changes.
- A login/item rejected without a specific message is a defect, not a disposition. Unknown flags already fail through clap; unknown and excluded verbs do not.

## Next action: FIX-only queue (no new capability)

1. Reject every known upstream verb that is missing or excluded, before prompt assembly, with a specific reason (missing vs boundary), zero side effects and no session/network/JS start. Covers `omp acp` (intended ACP server) and all excluded verbs.
2. Remove the `models --no-extensions` silent drop: honor it or reject it explicitly; keep `refresh`/`-e` explicit rejections intact.
3. Decide and enforce the LSP/DAP seam: either register the tools behind their parsed settings or report the feature as unavailable. Do not leave source + config that looks supported while unreachable.
4. Give excluded tool paths a typed boundary error instead of generic not-found.

These are fixes to recorded defects, not promises of added parity. After each fix: bounded local check, then push and wait for the exact-SHA Actions run before recording it.

## Execution ledger and evidence

Each ID below is **open**, not automatically supported. Close a row only after its invocation, arguments, typed result, success/error/cancellation test and Android smoke are recorded here. An unavailable account or executable is a runtime prerequisite error, not a reason to remove an otherwise supported interface. New upstream IDs must be reviewed before inclusion. Fixture tests compare observed behavior with the installed TypeScript CLI, never source text. Phase order: (1) inventory/capabilities; (2) live event, approval, v3 session and compaction; (3) file/remote/image tools; (4) all provider wires, auth and MCP; (5) delegation and remaining tools; (6) headless modes/commands; (7) Android artifact, corpus and metrics. Advance only after scoped red/green and actual smoke; record failures in this ledger.

Counts are split by axis. "Present" means registered and dispatchable, not accepted.

| Registry | Total IDs / tokens | Implementation / reachability | Acceptance |
|---|---|---|---|
| CLI subcommands | 41 registry rows + Rust-only `repl` | 8 present (2 implemented, 6 partial), 27 missing, 6 excluded; `repl` extra | 35 open + 6 excluded |
| Builtins & hidden tools | 32 tools (29 builtin + 3 hidden) | 14 reachable, 5 partial/unwired, 12 deferred, 1 excluded | 31 open + 1 excluded |
| Wire APIs | 14 wire protocols | 14 deferred (retained requirement) | 14 open |
| MCP transports | 3 transports | 3 partial (client path exists) | 3 open |
| Launch flags (string) | 30 canonical (31 tokens; `-e` alias of `--extension`) | 17 present (15 partial, 2 implemented wire subset), 9 missing, 4 excluded | 26 open + 4 excluded |
| Launch flags (optional) | 1 canonical (3 tokens; `-r`, `--session` aliases) | 1 partial | 1 open |
| Launch flags (valueless) | 23 canonical (24 tokens; `--yolo` alias) | 11 present, 12 missing | 23 open |
| Positional operands | 2 categories (`PROMPT`, `@FILE`) | 2 implemented | runtime re-check open |

*Counts:* `__complete` is a TypeScript registry entry (`packages/coding-agent/src/cli-commands.ts:70`); the 8 present / 27 missing / 6 excluded split is authoritative for these 41 rows.

### CLI subcommands (41)

Disposition vocabulary: **implemented** (registered + behavior path + consumer test anchor), **partial** (registered subset), **missing** (absent; invocation currently becomes a prompt), **excluded** (boundary; still lacks explicit reject unless noted). `SMOKE0f601` = artifact help/version/PTY only.

| ID (aliases) | Current source state | Evidence / behavior | Acceptance | Gap / prerequisite |
|---|---|---|---|---|
| `launch` | partial | Default path with no subcommand; text/json modes; `runtime` run path; TEST `tests/live_sse_integration.rs`; SMOKE0f601 PTY | open | `rpc`/`acp`/`rpc-ui` parse-reject; many launch flags missing. |
| `acp` | missing + silent-prompt risk | Not in clap `Command`; `omp acp` becomes positional prompt (`args.rs:25-28,169-170`) | open | ACP stdio server absent; needs explicit verb reject until built. |
| `auth-broker` | missing + silent-prompt risk | No Rust command | open | Local loopback port / auth store. |
| `auth-gateway` | missing + silent-prompt risk | No Rust command | open | Broker reachability / proxy port. |
| `agents` | partial | `args.rs:408-426`, `agents.rs`, `main.rs:144-168`; unpack/list subset; catalog smoke in history | open | No plugin agents (`agents.rs:5-7`); catalog-only validation. |
| `bench` | missing + silent-prompt risk | No Rust command | open | Benchmark fixtures / model access. |
| `browser-relay` | missing + silent-prompt risk | No Rust command | open | Conditional external Chrome; device Chromium linker broken. |
| `cleanse` | missing + silent-prompt risk | No Rust command | open | Subagent runner; model access. |
| `commit` | missing + silent-prompt risk | No Rust command | open | `git` CLI present in repository. |
| `completions` | implemented | `args.rs:394-397`, `completions.rs`, dispatch `main.rs:255-256`; TEST `tests/completions.rs` | open (host run not re-executed) | Completes only ported commands/flags, not full TS surface. |
| `__complete` | implemented | `args.rs:399-405`, `main.rs:258-260`; TEST completions + args unit | open | Only `models`/`sessions` kinds; read-only. |
| `compress` | missing + silent-prompt risk | No Rust command | open | File read/write. |
| `config` | missing + silent-prompt risk | No Rust command | open | Config read/write; profile flags exist but no `config` verb. |
| `dry-balance` | missing + silent-prompt risk | No Rust command | open | OAuth accounts. |
| `gc` | missing + silent-prompt risk | No Rust command | open | Storage directory. |
| `grep` | partial | `args.rs:458-484`; matcher/native grep in CI; device smoke in history | open | CLI flag surface is not the full tool contract. |
| `gallery` | missing + silent-prompt risk | No Rust command | open | ANSI preview in scope; `vhs` absent on Android. |
| `git` | excluded | No reject path; verb becomes prompt | excluded / not enforced | Fullscreen TUI boundary; needs explicit refusal. |
| `grievances` | missing + silent-prompt risk | No Rust command | open | Grievances store. |
| `images` (alias `img`) | missing + silent-prompt risk | No Rust command | open | Cache directory; no auto-install. |
| `if-bench` | missing + silent-prompt risk | No Rust command | open | Benchmark fixtures. |
| `install` | excluded | No reject path; verb becomes prompt | excluded / not enforced | Plugin/extension boundary; needs explicit refusal. |
| `join` | excluded | No reject path; verb becomes prompt | excluded / not enforced | TUI + WebRTC boundary. |
| `models` | partial | `args.rs:429-448`, `local_models.rs`, `main.rs:170-188`; device metadata smoke in history; TEST `explicit_extension_paths_are_rejected` | open | `refresh` and `-e` explicit reject; **`--no-extensions` silently dropped**; no OAuth discovery. |
| `plugin` | excluded | No reject path; reserved-word class; verb becomes prompt | excluded / not enforced | Plugin execution boundary; needs explicit refusal. |
| `ps` | partial | `args.rs:486-520`, `process_cli.rs`; TEST `tests/daemon_broker_smoke.rs` | open | Interactive monitor TUI excluded; headless list/logs/stop/kill/restart. |
| `say` | missing + silent-prompt risk | No Rust command | open | File output in scope; speaker playback excluded. |
| `share` | missing + silent-prompt risk | No Rust command | open | Session store / network. |
| `setup` | missing + silent-prompt risk | No Rust command | open | Non-interactive paths only; no package installs. |
| `shell` | excluded | No reject path; verb becomes prompt | excluded / not enforced | Interactive TUI boundary. |
| `read` | partial | `args.rs:451-456`; TEST `tool_commands.rs`, `read_contract`; device smoke in history | open | Archive/SQLite/PDF/markit/internal URIs still refused (`tools/read.rs:265-373`). |
| `render` | missing + silent-prompt risk | No Rust command | open | Session file / stdout. |
| `ssh` | missing + silent-prompt risk | No Rust command | open | `ssh` present; host auth required. |
| `stats` | missing + silent-prompt risk | No Rust command | open | Session DB; dashboard TUI excluded. |
| `update` | excluded | No reject path; verb becomes prompt | excluded / not enforced | No Android release target. |
| `usage` | missing + silent-prompt risk | No Rust command | open | Auth store / provider network. |
| `tiny-models` | missing + silent-prompt risk | No Rust command | open | ONNX runtime unverified on Android; download needs approval. |
| `token` | missing + silent-prompt risk | No Rust command | open | Auth database. |
| `ttsr` | missing + silent-prompt risk | No Rust command | open | Stream ruleset. |
| `worktree` (alias `wt`) | missing + silent-prompt risk | No Rust command | open | Git repo; task `isolated` worktree also unwired. |
| `search` (alias `q`) | missing + silent-prompt risk | No Rust command | open | Search credential / network. |

*Rust-only:* `repl` (`args.rs:390-391`, `main.rs:626-627`; SMOKE0f601 5 PTY scenarios) is a partial explicit REPL entry, not one of the 41 TS rows.

### Builtin tools (29)

| ID (aliases) | Reachability | Mount / behavior | Acceptance | Gap / prerequisite |
|---|---|---|---|---|
| `read` | reachable (subset) | `ReadTool` in `BUILTIN_TOOLS` (`runtime.rs:58-75`); range/dir selectors; archive/sqlite/pdf/image/`scheme://` refused | open | Full selector parity; device range smoke exists, not full matrix. |
| `bash` | reachable | `BashTool`; per-call `env`/`cwd`/`timeout`; 64 KiB output cap; async via process supervisor; foreground `pty:true` rejected; `timeout_ms` rejected | open | TEST `bash_contract.rs` incl. `async_bash_shares_the_supervisor_process_table`; async needs supervisor. |
| `edit` | reachable | `EditTool`; default `EditMode::Hashline`; `apply_patch` wire name when mode ApplyPatch (`tools/mod.rs:1031-1034`) | open | Mode selection from settings partial; approval alias `--allow edit` (`approve.rs:240-246`). |
| `ast_grep` | reachable (local only) | `AstGrepTool`; local file/dir/glob; ssh/internal refused explicitly | open | No remote/internal URI search. |
| `ast_edit` | reachable | `AstEditTool`; staged queue + PendingStore; glob scope, partial-write reporting | open | Policy/store coupling; device staged smokes are historical artifact evidence. |
| `ask` | reachable | `AskTool`; stripped when no ask channel (`allowed_tools`); child registry removes it | open | Needs interactive/RPC answer channel; PTY evidence historical. |
| `debug` | partial/unwired | `pub mod dap`, `dap/tool.rs`; `config.debug.enabled` parsed | open | **Not in `BUILTIN_TOOLS` or `build_registry`**; adapters external. |
| `eval` | partial/unwired | `EvalTool` late-mounted; kernels/preludes exist | open | Bridge strips `eval`/`task`/`hub`; rejects `checkpoint`/`rewind`; `budget()` fail-closed (`crates/omp-cli/src/eval_host.rs:407-414`); interpreters external. |
| `github` | deferred | No `GithubTool` in agent path | open | `gh` + auth when ported. |
| `glob` (alias `find`) | reachable | `GlobTool`; alias normalized in `agents.rs:396-399` | open | — |
| `grep` (alias `search`) | reachable | `GrepTool`; gitignore/PCRE2 path | open | Device grep smoke historical. |
| `lsp` | partial/unwired | `LspTool` source (`lsp/tool.rs:173+`); `lsp.enabled` parsed; seam comment `lsp/mod.rs:3-9` | open | **No CLI registration**; servers external. |
| `inspect_image` | deferred | TS tool only; read error text points to it | open | Image read + 25 MiB bound. |
| `browser` | deferred | No Rust agent browser tool | open | External Chromium; device linker broken (`_ZNSt6__ndk113__hash_memoryEPKvm`). |
| `computer` | excluded | No `ComputerTool` in `pi-agent`; desktop backend returns unavailable (`pi-natives/src/desktop/mod.rs:604-606`) | excluded | Generic "tool not found" instead of boundary reason. |
| `checkpoint` | reachable (gated) | Mount `runtime.rs:641-651`; requires tools on + allowlist + controller + `checkpoint.enabled` (default false) | open | Root-only; not in child template; eval bridge rejects. |
| `rewind` | reachable (gated) | Same mount/gate; branches conversation; abandoned JSONL entries remain | open | Active checkpoint required; no Git/file restore. |
| `security_scan` | deferred | TS only | open | Rules engine / history. |
| `task` | partial/unwired | `TaskTool`; synchronous delegation via `Supervisor`; supports `outputSchema`/`schemaMode`/`effort` (`task.rs:324-337`); rejects `isolated` only (`task.rs:1410-1421`); eval `agent()` additionally rejects `apply`/`merge` (`task.rs:973-975`); depth/concurrency bounded | open | No background scheduler; in-memory records; **worktree absent** (`crates/pi-agent/src/tools/task.rs:1413-1416`); TEST `delegation.rs` t1–t10. |
| `hub` | partial/unwired | `HubTool`; `SUPPORTED_OPS` = `jobs`, `cancel`, `wait`, `send`, `inbox`, `list`, `start`, `ps`, `logs`, `stop`, `restart`, `describe` (`crates/pi-agent/src/tools/hub.rs:37-38`); owner scoping | open | No park/revive; process ops need backend; "jobs/cancel only" text in history is stale. |
| `todo` | reachable | `TodoTool`; ops init/start/done/rm/drop/block/unblock/append/view; persists in session `toolResult.details`, restores on resume/fork | open | Child drops todo; TUI/`/todo` deferred. |
| `web_search` | deferred | TS only; `agents.rs` accepts the name but builder cannot construct it → allowlist drift | open | Credential/network when ported. |
| `write` | reachable | `WriteTool`; PendingStore; approval tier | open | Atomic existing-file path historically reviewed; acceptance open. |
| `memory_edit` | deferred | TS only | open | SQLite `agent.db`. |
| `retain` | deferred | TS only | open | Same DB. |
| `recall` | deferred | TS only | open | Same DB. |
| `reflect` | deferred | TS only | open | Model + DB. |
| `learn` | deferred | TS only | open | Skill directory. |
| `manage_skill` | deferred | TS only | open | Skill storage; plugin execution stays excluded. |

### Hidden tools (3)

| ID | Reachability | Behavior | Acceptance | Gap |
|---|---|---|---|---|
| `yield` | reachable | `YieldTool` registered only in `child_registry` (`crates/pi-agent/src/supervisor.rs:944`); result/error/streaming forms | open | Not at root; no standalone channel. |
| `goal` | deferred | No Rust `GoalTool`; eval `budget()` errors (`crates/omp-cli/src/eval_host.rs:407-414`) | open | Goal Mode unavailable by design. |
| `think` | reachable | `ThinkTool` always on supervisor root+child (`crates/pi-agent/src/supervisor.rs:891,945`) | open | Upstream gates on externalThinking; Rust always-on — known divergence. |

### Wire APIs (14)

All 14 remain a **deferred, retained requirement**; none is excluded. Runtime still uses the OpenAI-compatible provider path (`runtime.rs:530`); no per-dialect fixture was executed this pass.

| ID | State | Acceptance | Gate |
|---|---|---|---|
| `openai-completions` | deferred | open | Provider request/stream fixture; live smoke if key. |
| `openai-responses` | deferred | open | Same. |
| `openrouter` | deferred | open | Same. |
| `openai-codex-responses` | deferred | open | Same. |
| `azure-openai-responses` | deferred | open | Endpoint + deployment + key. |
| `anthropic-messages` | deferred | open | SSE fixture incl. thinking/tools. |
| `bedrock-converse-stream` | deferred | open | AWS creds/region/model access. |
| `google-generative-ai` | deferred | open | Studio key. |
| `google-gemini-cli` | deferred | open | Gemini CLI/Cloud auth. |
| `google-vertex` | deferred | open | Project/ADC/region. |
| `ollama-chat` | deferred | open | Reachable daemon. |
| `cursor-agent` | deferred | open | Cursor token. |
| `gitlab-duo-agent` | deferred | open | Duo token + instance. |
| `devin-agent` | deferred | open | Devin key. |

*Note:* custom API IDs fail explicitly with `custom API requires extensions`.

### MCP transports (3)

| Transport | State | Evidence | Acceptance | Gap |
|---|---|---|---|---|
| `stdio` | partial | Parse `config.rs:1574+`; connect `pi-agent/mcp/client.rs:88`; launch gate `is_stdio_mcp_launch_allowed*`; TEST `mcp_launch_gate.rs`, `mcp_integration.rs` | open | Full init/call/cancel/cleanup matrix still plan-open. |
| `http` | partial | Parse + `HttpTransport::new`; connect match arm | open | Live auth untested. |
| `sse` | partial | `LegacySseTransport`; connect | open | Live auth untested. |

Root keys `mcpServers`, `disabledServers`, `enabledServers` parse (denylist wins; name pattern validated). OAuth is `McpError::OAuthUnsupported` (`pi-agent/src/mcp/error.rs:9-10`); apikey partial.

### Launch flags: string (30 canonical IDs, 31 registered argv tokens)

| Flag (aliases) | State | Anchor / behavior | Acceptance |
|---|---|---|---|
| `--cwd <dir>` | partial | `args.rs:42-44` | open |
| `--config <path>` | partial | `args.rs:98-100`; profile/config tests | open |
| `--add-dir <dir>` | missing | Clap rejects unknown flag | open |
| `--mode <mode>` | partial | `args.rs:38-40`; enum only `text`/`json` (`output.rs:14-21`) | open; `rpc`/`acp`/`rpc-ui` parse-reject, `rpc-ui` lacks boundary message |
| `--fork <id>` | partial | `args.rs:117-119`; session lifecycle tests | open |
| `--provider <name>` | partial | `args.rs:58-60` | open; dialects deferred |
| `--model <name>` | partial | `args.rs:54-56` | open |
| `--smol <model>` | missing | Clap rejects | open |
| `--slow <model>` | missing | Clap rejects | open |
| `--plan <model>` | missing | Clap rejects; TS value shadowable by extension boolean | open |
| `--prewalk-into <model>` | missing | Clap rejects | open |
| `--plan-yolo-into <model>` | missing | Clap rejects | open |
| `--max-time <duration>` | partial | `args.rs:74-76`; `parse_max_time`; TEST rejects `soon` | open |
| `--service-tier <tier>` | missing | Clap rejects | open |
| `--api-key <key>` | partial | `args.rs:62-64`; redaction boundary test | open; never log |
| `--system-prompt <prompt>` | partial | `args.rs:66-68` | open |
| `--append-system-prompt <text>` | partial | `args.rs:70-72` | open |
| `--provider-session-id <id>` | implemented (wire subset) | `args.rs:129-131`; TEST `session_provider_fields.rs` | open |
| `--prompt-cache-key <key>` | implemented (wire subset) | `args.rs:133-135`; same tests | open; only `kimi-code` serializes the key |
| `--session-dir <dir>` | partial | `args.rs:145-147` | open |
| `--models <list>` | missing | Clap rejects | open |
| `--tools <list>` | partial | `args.rs:82-84`; MCP launch-gate tests | open |
| `--thinking <level>` | partial | `args.rs:137-139`; `parse_cli_thinking`, `thinking_wire.rs` | open |
| `--export <path>` | missing | Clap rejects | open |
| `--hook <hook>` | excluded | Not registered at root; clap reject | excluded only if a specific message is added |
| `--extension <path>` (`-e`) | excluded | Root missing; `models -e` rejected explicitly (`local_models.rs:73-82`) | excluded |
| `--trusted-extension <id>` | excluded | Root missing; clap reject | excluded |
| `--plugin-dir <dir>` | excluded | Root missing; clap reject | excluded |
| `--skills <list>` | partial | `args.rs:161-163` | open |
| `--approval-mode <mode>` | partial | `args.rs:86-88`; `approval_gate.rs`; values `always-ask`/`write`/`yolo` | open |

*Notes:* `flag-tables.ts` `STRING_SETTERS` has 31 registered keys; `-e` aliases `--extension`. Rust extras not in TS: `--profile`, `--alias`, `--allow`, `--session-id`. Missing flags are rejected by clap — rejection is not implementation.

### Launch flags: optional (1 canonical ID, 3 registered argv tokens)

| Flag (aliases) | State | Evidence | Acceptance | Gap |
|---|---|---|---|---|
| `--resume [id]` (alias `-r`, `--session`) | partial | `args.rs:106-115`; profile boundary test `args.rs:732-735`; session tests | open | Bare/picker/non-TTY and empty-value contracts not fully compared to TS. |

*Note:* `OPTIONAL_FLAGS` has 3 registered keys.

### Launch flags: valueless (23 canonical IDs, 24 registered argv tokens)

| Flag (aliases) | State | Anchor / evidence | Acceptance |
|---|---|---|---|
| `--help` | partial | clap; SMOKE0f601 exit 0 | open |
| `--version` | partial | clap `-V`; SMOKE0f601; completion asserts `-V` not `-v` | open |
| `--allow-home` | missing | Clap rejects | open |
| `--continue` / `-c` | partial | `args.rs:102-104` | open |
| `--from-claude` | partial / non-TTY reject | `args.rs:121-123`; TEST `foreign_import_flags_fail_explicitly_without_a_tty`; import readers `foreign_sessions.rs` | open; TTY picker excluded |
| `--from-codex` | partial / non-TTY reject | Same contract/tests | open |
| `--no-session` | partial | `args.rs:153-155` | open |
| `--no-tools` | partial | `args.rs:78-80` | open |
| `--no-lsp` | missing | Clap rejects | open |
| `--no-pty` | missing | Clap rejects | open |
| `--hide-thinking` | missing | Clap rejects | open |
| `--advisor` | missing | Clap rejects | open |
| `--external-thinking` | missing | Clap rejects | open |
| `--prewalk` | missing | Clap rejects | open |
| `--no-prewalk` | missing | Clap rejects | open |
| `--plan-yolo` | missing | Clap rejects | open |
| `--print` / `-p` | partial | `args.rs:34-36` | open |
| `--print-thoughts` | missing | Clap rejects | open |
| `--no-extensions` | missing at root; **silent no-op under `models`** | Root rejected by clap; `args.rs:443-445` accepted on `Command::Models` then dropped (`main.rs:170`) | open + defect |
| `--no-skills` | partial | `args.rs:157-159` | open |
| `--no-rules` | missing | Clap rejects | open |
| `--no-title` | partial | `args.rs:141-143`; `main.rs:537-541` `PI_NO_TITLE` | open |
| `--auto-approve` (alias `--yolo`) | partial | `args.rs:90-92`; conflicts with `--approval-mode` | open |

*Note:* `VALUELESS_FLAGS` has 24 registered keys; `--yolo` aliases `--auto-approve`.

### Historical snapshot notice

Every dated block from `### Capability probes (2026-10-26)` to the end is a **historical snapshot**. Entries labelled WIP, remote-only, no-push, or no-commit reflect the 2026-10-27 state; they are evidence, not active directives. Current user authorization (2026-10-29) permits bounded local checks (≤600s) plus commits, pushes, and Actions runs for this plan. Historical test/smoke results stay attached to the SHA and artifact they measured; they do not verify newer commits or close rows.

### Capability probes (2026-10-26, read-only)

`command -v`: present `ssh`, `gh`, `python`, `python3`, `ruby`, `termux-clipboard-get`, `clangd`, `rust-analyzer`, `node`, `bun`, `termux-tts-speak`, `termux-microphone-record`; absent `vhs`, `julia`, `lldb-vscode`, `codelldb`, `onnxruntime`, `tiny-models`. Presence does not establish working auth, clipboard service, LSP/DAP, JS kernel, or ONNX speech/tiny backend. `$PREFIX/lib/chromium/chrome` executable exists, but `bash android/scripts/verify-browser.sh --skip-deb-sha256` exits **1** because installed Chromium package `155.0.8059.39` differs from script-pinned `149.0.7827.155`; direct `chrome --version` also exits **1**: Bionic linker cannot locate `_ZNSt6__ndk113__hash_memoryEPKvm`. Live browser blocked by this broken prerequisite; no install/downgrade. `$PREFIX/lib/chromium/headless_shell` exists but is not the selected CDP executable. Recheck prerequisites at invocation. ONNX speech/tiny and screenshot gallery capability unavailable/unproved; do not claim platform support. Probe commands: `command -v <program>`, `bash android/scripts/verify-browser.sh --skip-deb-sha256`, executable `--version`, capability-specific read-only smoke; never pass `--install`.

LSP/DAP executable probe (`command -v`): present `debugpy`, `rdbg`, `gdb`, `lldb-dap`, `pyright-langserver`, `typescript-language-server`, `clangd`, `rust-analyzer`; absent `dlv`, `js-debug-adapter`, `lldb-vscode`, `codelldb`. Backend protocol and cancellation remain untested; presence alone is not support.

### Termux platform constraints (2026-10-28)

- **Hard links:** Android app-private storage (`$HOME`, `$TMPDIR`) denies `link(2)` with `EACCES`. Host coreutils `ln` fails identically; the Rust `ln` builtin reports `failed to create hard link ...: Permission denied` and never substitutes a copy, which would fabricate a different inode. Inode-equality coverage runs on non-Android Unix; Android has a denial test asserting exit 1, unchanged source bytes/inode/nlink, and no destination.
- **Temporary directory:** `sort` precedence is explicit `-T`/`--temporary-directory`, then exported `TMPDIR`, then `std::env::temp_dir()`. The fallback must not assume `/tmp`, which Termux does not provide; the stdin tests exercise the empty-host-env default path.

### Local execution ledger (2026-10-28)

- **Full4 package suite:** parent `bg_55` ran tests for `omp-cli`, `pi-agent`, `pi-builtins`, and `pi-shell`; **exit 0** (`artifact://2599`). Observed passes: `pi-builtins` 1100, `pi-shell` 862; all `omp-cli` unit/integration suites and `pi-agent` suites green. Doc-tests: 0 run, 1 ignored in `omp_cli`, 16 ignored in `pi_builtins`, 0 elsewhere. This proves the recorded local package/tool subset only; it is **not** full TypeScript parity, an Android Bionic build, or live-device verification.
- **Actual CLI smoke:** parent `~/tmp/smoke_cli.py` PASS (12 requests): task child bash with `env`/`cwd`/`timeout` plus `yield`, `hub jobs` id/name/status, `todo init` persistence and resumed `todo view`. The throwaway fixture was removed after the smoke; no user files touched.
- **Formatting baseline:** `cargo fmt --all -- --check` exits **1** (`artifact://2613`), covering roughly 80,529 diff lines of pre-existing drift, including vendor/ignored paths. Stable rustfmt ignores the nightly-only options in `rustfmt.toml` (`indent_style`, import grouping/granularity, vendor `ignore`, and others), so this is a baseline limitation, not a regression. No whole-tree formatting was performed; scoped changed hunks follow the surrounding tab/long-line style, and `git diff --check` is clean.
- **Warnings:** the unused imports/mut/dead constants reported by the full4 run already exist at `HEAD` in the same tracked files (`pi-agent`, `omp-cli` tests/bin, `pi-walker`); this integration introduced none, so no unrelated warning cleanup was made.

### Published block (2026-10-28)

- Commit `b82ea04a52` pushed to `feat/rust-agent-termux-bionic`: persistent todo, per-call bash environment/cwd/timeout, shared task/hub supervision and shell-tree cleanup on cancellation/drop.
- [Actions 40681101423](https://github.com/SklUpp/omp-termux/actions/runs/40681101423) succeeded: Linux x86_64 host tests and Android arm64/Bionic binary build. `gh run watch --exit-status` returned 0. Downloaded CI binary not exercised for this commit.
- Independent correctness and security reviews closed identity/ownership, queued cancellation, reservation-drop leaks, shared parent todo, and detached shell findings. Local behavioral regressions and CLI smoke passed as recorded above.
- Scope remains a local subset: synchronous task execution; hub supports only jobs/cancel; supervisor records are process-local. Full TypeScript parity, persistent background jobs and remaining hub operations are not complete.
- Record each next block here: scope and predecessor handoff, review findings and fixes, exact verification commands/results, commit and CI link, remaining limitations. Do not mark an unverified capability complete.

### Active block: local read and configuration (2026-10-28)

- Baseline: `b82ea04a52`, Actions `40681101423` green. User authorizes continued execution in priority order with parallel independent lanes and documentation at each milestone.
- `ReadParity` owns isolated local text selectors, directory reads, bounds/cancellation and behavioral regressions; predecessor contract: `CoreContractScout`. No changes to bash, todo or supervision.
- `ProfileConfigParity` owns isolated, CLI-wired profile bootstrap and supported configuration-layer precedence; predecessor contract: `HeadlessScout`. No transport-only scaffolds or no-op flags.
- `HashlineContractScout` researches native hashline reuse, exact snapshot/operation contracts and read-output coupling; read-only. Implementation follows a settled contract, not a partial replacement parser.
- Parent integrates one writer at a time, reviews combined changes, runs throttled checks and actual CLI scenarios, then publishes a logical block and waits for Actions. These lanes are in progress, not accepted capabilities.

- Runtime interruption left three implementation lanes unfinished. Revival was unavailable; all three were respawned together against their retained worktrees, with full predecessor transcripts and ownership contracts: `ResumeReadParity`, `ResumeProfileConfig`, `ResumeHashlineParity`.
- Read worktree: `~/.omp/wt/t7a8843031/m`; profile/config: `~/.omp/wt/tbf0b7567b/m`; hashline: `~/.omp/wt/t9b67f49a8/m`. Existing edits are retained, not reapplied from a clean baseline.
- Read implementation and behavioral regressions are integrated: `cargo test --offline --locked -j1 -p pi-agent --test read_contract -- --test-threads=1` PASS 31/31 (bg77); an inclusive-range fixture was corrected with endpoint/gap assertions, not by weakening output checks. Shared read/edit snapshot wiring and child-fresh store/register/seen isolation are integrated and covered by `agent_core`.
- Profile/config review is closed. Malformed project YAML remains fatal, matching upstream; Rust preserves original files instead of quarantining them. Profile-layer smoke and the full CLI suite passed as recorded below.
- Native hashline (parser/clipboard/apply/recovery/budget) is integrated with `tools/read.rs` through one canonical `SnapshotStore`; `hashline_contract` PASS 38/38 and `agent_core` PASS 84/84 (bg77). Three independent static reviews closed with fixes for bounded expansion/register accounting before cloning, move collisions, partial-write reporting, prepared-file identity and unpublished temp-file permissions; no remaining blockers.
- `pi-agent` library target PASS 45/45 via `cargo rustc --profile test -- -C link-arg=-Wl,--threads=1` followed by running the produced test executable; the default linker crashes on Android tagged pointers, so this is a toolchain workaround, not a test change.
- Actual CLI smoke PASS (parent): `python3 $PREFIX/tmp/omp-hashline-smoke.GITRpc/omp_hashline_smoke.py --bin ~/omp-work/target/debug/omp` covers `:raw`, hashline headers, project `true` config, opt-in seen rejection then retry, profile layers, resume and mode.
- Full `omp-cli` suite bg81 **PASS exit 0** (`cargo test --offline --locked -j1 -p omp-cli -- --test-threads=1`): 147 library tests plus every integration target, including `profile_config` 12, `live_sse` 2 and MCP 25. The earlier bg79 run stopped on `live_sse_integration` because its fixture pinned the pre-hashline `N|TEXT` read output instead of `[path#TAG]` + `N:TEXT`; the fixture now derives the tag independently with `hash::compute_file_hash` and asserts the same bytes through the tool result, session log and resumed provider request. pi-agent evidence stays as recorded above: `agent_core` 84, `hashline_contract` 38, `read_contract` 31 PASS (bg77), library 45/45 via `cargo rustc --profile test -p pi-agent -- -C link-arg=-Wl,--threads=1` plus the produced test executable (default linker crashes on Android tagged pointers), and the CLI smoke command listed above. Claims stay local-text/hashline subsets: archives, SQLite, document conversion, images and internal-URL routing remain planned integrations, and `:conflicts` is blocked on the conflict-history session store.

### Active block: async coordination and interactive ask (2026-10-28)

- Previous block published as `07bc2eb73e`; [Actions 40701566791](https://github.com/SklUpp/omp-termux/actions/runs/40701566791) passed Linux host checks/tests and Android arm64/Bionic build (`gh run watch --exit-status`: 0). Installed CLI unchanged.
- `AsyncCoordination` owns isolated supervisor/task/hub/mailbox/loop work; `InteractiveAsk` owns isolated question channel and REPL/config wiring. Parent integrates shared registration seams.
- Predecessor `CoordinationScout` corrected async settings, receipts and peer visibility against upstream. No invented detach flag, buffered-success receipt or headless-only ask stub is accepted.
- Acceptance requires background lifetime and exactly-once results, owner-scoped job control, peer messaging, cancellation/cleanup and a real terminal answer path. Those gates are now covered by the checks and PTY evidence below.
- `IntegrateCoordAsk` merged both lanes. Static reviews closed nested semaphore deadlock, lost mailbox handoffs, unbounded pending results and panic/drop cleanup; admission now bounds running plus undelivered jobs, not merely consumed history.
- Local checks: async coordination 12/12 and delegation 10/10 passed (bg152); full `omp-cli` suite bg157 passed 158 library tests plus all integration targets, and the final full-CLI rerun after the REPL-output correction bg164 passed 158 library tests plus every integration target. pi-agent library bg167 passed 64/64 using the existing `--threads 1` linker workaround and direct test binary; the earlier bg166 63/64 mailbox failure is explained in the next bullet.
- Actual PTY smoke exposed buffered REPL final text; per-turn flushing in `main.rs` is a genuine fix, after which answer selection, partial-line timeout cleanup and UTF-8 note near the deadline pass. The first Ctrl-C failure was a harness defect, not an application cancellation defect: the spawned REPL had no session or controlling terminal, so writing `0x03` produced no SIGINT and ask hit its 10s timeout auto-selection; the extra provider request was timeout continuation. The harness now calls `setsid` + `TIOCSCTTY`; bg163 ran all five actual PTY scenarios **PASS exit 0 in 6.54s**, including cancel recovery and async delegation (immediate job id, `hub jobs` consumes exactly one final envelope, async root completion visible before `/exit`). Loop guards were still strengthened so a tool-initiated cancellation stops the turn before any further provider request.
- Reviews are closed for every scoped finding. Changed paths are limited to the async/ask integration, shipped docs, the PTY smoke script and the existing workspace `tokio-util` edge (`crates/omp-cli/Cargo.toml`, `Cargo.lock`), plus the per-turn REPL flush and cancellation guards; no unrelated changes.
- bg166 pi-agent library first reported 63/64 with only `mailbox::tests::waiter_drop_does_not_swallow_later_mail` failing (expected unread 1, saw 0). The mailbox API was not losing mail: `deliver_to_waiter` returns `false` when no waiter accepts, and the message is then not consumed; the caller owns it. That test encoded the missing prerequisite (caller fallback/buffer) unlike the companion `take_or_register` test. Corrected by passing the same message to `buffer` after the `false`, asserting both unread 1 and retrieval; the doc comment records the ownership contract. After the correction bg167 passed 64/64; all new inflight/recovery tests also pass.

### Active block: process supervision and agent catalog (2026-10-28)

- Previous block published as `3f35852340`; [Actions 40719029552](https://github.com/SklUpp/omp-termux/actions/runs/40719029552) passed Linux host tests and Android arm64/Bionic build (`gh run watch --exit-status`: 0).
- `NativeProcessHub` owns isolated OS-process management and process-specific hub/bash paths. `AgentCatalogPolicy` owns isolated agent discovery, CLI unpack and task policy application. Shared runtime/registration seams are integrated by one parent-designated writer.
- Predecessors `CoreContractScout` and `HeadlessScout` mapped exact upstream contracts. Required policies include actual tools/model/blocking/output behavior, not discovery-only registration; process flags must perform their advertised lifecycle or fail explicitly.
- Process persistence/broker/PTY and catalog provider/isolation dependencies remain acceptance gates, not silently removed scope. Implementation, independent review, serial checks and live CLI scenarios are pending.
- Interrupted writers were respawned as `ResumeProcessHub`, `ResumeAgentCatalog` and `ResumeProcessBroker` in their retained worktrees. `IntegrateNativeBlock` then combined all three into the live tree; no cleanup removed those sources.
- `ProcessLifecycleReview` closed scoped manager and broker source findings after repeated targeted rechecks. Detached disk logs are unbounded, matching upstream; pipe/PTY logs and read allocations are bounded. Async spawn-to-metadata persistence retains a crash window; no zero-orphan guarantee is claimed.
- `AgentPolicyReview` rechecked and closed required-field validation, admission-slot release, effort precedence and provider-key isolation fixes statically. New integration reviews also drove scope-bound fallback, unique process-owner namespaces and one backend snapshot for both child Bash/Hub.
- Combined compilation reached runnable CLI and integration binaries. `daemon_broker_smoke` passed 4/4 and `bash_contract` 11/11 (bg49); corrected PTY environment/private-spec fixtures then passed `process_contracts` 10/10 (bg51). Commands used `--offline --locked -j1 -- --test-threads=1`.
- Full `cargo test --offline --locked -j1 -p omp-cli -- --test-threads=1` passes (bg88): 358 tests passed across library, binary and integration targets; one doctest ignored. `pi-agent --lib` passes 108/108, Bash contracts 11/11 and process contracts 10/10 (bg81). All five broker integration tests pass. Earlier LLVM 21.1.8 tagged-pointer linker crash was bypassed with target-only `cargo rustc --profile test --test mcp_launch_gate -- -C link-arg=-Wl,--threads=1` (bg86); that binary passed 25/25 (bg87), then the full CLI run completed normally. Existing compiler warnings remain; no package or installed CLI change.
- New-binary actual CLI smoke passed all five conversations with an over-budget logical socket root and a shortened v1 scope (bg82, exit 0): persist survives CLI exit; environment logs, stop and async timeout work; second generation restarts, broker is killed, reconnect adopts the exact same PID/startIdentity, then stops it. Short-root five-conversation smoke also passed earlier (bg62).
- Focused static re-review closed all three new concurrency findings: single-flight lazy initialization, process-global background names and flock-protected complete checkpoint publication. Concurrent fallback/name tests passed in bg77; current long-root CLI smoke covers the new physical-path branch.
- Actual catalog CLI smoke passed (bg80, exit 0): seven bundled agents unpack, skip existing and force overwrite; project child executes `read` with only `read/yield`; strict schema violation returns an invalid result, retry yields valid data; blocking receipts remain synchronous, async peer runs, and synthetic root/child credentials stay on their respective endpoints. Hidden root `think` is pre-existing behavior and still differs from the upstream external-thinking gate; classification remains open.
- Published as `714e5feb6b`. [Actions 40744568143](https://github.com/SklUpp/omp-termux/actions/runs/40744568143) passed Android arm64/Bionic build and uploaded the artifact, but failed host tests: `delegation::test_t5_hub_jobs_snapshot_after_child_completion` still expects `hub ps` to be unsupported. The command is now implemented; migrate the stale negative case and rerun CI before calling this publication accepted.
- User directive: after every push, locate or start CI for the exact pushed SHA, wait for terminal results and report both host and Android jobs. Continue independent project work in parallel; a successful push alone is not publication acceptance.
- Fixed the stale negative case and retained a positive `hub ps` assertion in `81d860d237`; local delegation tests passed 10/10 (bg99). [Actions 40747121341](https://github.com/SklUpp/omp-termux/actions/runs/40747121341) for that exact SHA passed Linux host checks/full tests and Android arm64/Bionic build with artifact upload; `gh run watch --exit-status` exited 0. Read/grep and models implementation continue as independent lanes; neither new slice is covered by this committed CI run.

### Active block: headless read/grep and models

- Parallel implementation: `HeadlessReadGrep` owns live read/grep and a shared native matcher extraction; `NativeModels`/`ModelsContractFix` own models in retained isolated tree `t361afe817/m`. Parent integrates common CLI seams only after contract review.
- Sequential scout → reviewer review found and corrected read/grep single-file count limits, oversized ordering, exact brace-union globs, direct-file glob validation and missing Bazel wiring for `pi-grep-pattern`. Final zero-limit adjustment still needs a fresh scoped run. Whole-file streaming instead of the native 4 MiB prefix and explicit unreadable-file errors are intentional documented safety differences.
- Actual isolated CLI smoke passed read raw range, grep content/count and missing read argument without a model catalog or session creation. Scoped run bg129 passed 6/7 and exposed missing `WhenCheap` size hints in oversized ordering; candidates now stat only missing hints before ordering. Native grep local build was cancelled by parent after user requested GitHub Actions instead of heavy Termux builds. The workflow now has a separate matcher/native-grep job; no native test pass is claimed yet.
- Models scout found discarded config, unavailable-provider listing and malformed custom catalog/cost handling; fixes and lexical path normalization passed focused reviewer recheck and are integrated. Rust library compilation succeeded after two boundary fixes, but final CLI link bg144 hit the known LLVM tagged-pointer abort on Android before tests ran. Full host tests, matcher/native grep and Android build must run in Actions for the new commit. Refresh/provider OAuth discovery and TypeScript extensions remain explicitly unavailable, not fabricated.
- User clarification: heavy builds/full suites belong in GitHub Actions, not an SSH server or the phone. Keep local checks short and bounded; never let a heavy shared-target Cargo build block lightweight smoke checks. Bazel wiring was reviewed but Bazel is unavailable locally.
- Published as `36b1942898`; [Actions 40756879200](https://github.com/SklUpp/omp-termux/actions/runs/40756879200) completed successfully for the exact SHA: host 717 passed, matcher 10 passed, native grep 28 passed; Android arm64/Bionic artifact uploaded. `gh run watch --exit-status` exited 0. Device smoke of the downloaded artifact is running separately; no installed CLI replacement.
- Downloaded that Android artifact and exercised it on-device: 47 assertions across 17 CLI subprocesses passed, including read ranges/errors, grep modes/PCRE2/zero-limit/oversized ordering, models metadata/cost, availability, config overlays and lexical path scopes. Minimal isolated environment, 10-second per-command timeout, no provider turn or session creation; installed `omp` unchanged. Binary SHA-256 `ee16fc77da2701886b60afcbb3b61b5d924aa482fdd805a32fa0abfd85aef569` matches its manifest. Retained task-owned proof: `$PREFIX/tmp/model-probe-cGpKdp/probe.py` and `probe-log.json`; artifact `$PREFIX/tmp/omp-art-Vgkfqv/omp`.
- While that CI ran, a separate implementation fixed `--no-title` and non-empty `PI_NO_TITLE` propagation into runtime-only session options; automatic titles are gated without clearing persisted titles. Fresh reviewer and fixture-based CLI checks are pending; this new change is not covered by the green `36b1942898` run.
- Title suppression published as `0338771409`, root thinking as `5cd6508440`; MCP test fixture connection handling fixed in `3134ee5478` without raising execution deadlines. [Actions 40766311711](https://github.com/SklUpp/omp-termux/actions/runs/40766311711) for `3134ee5478` passed host 729/0, matcher/native grep 38/0 and Android arm64/Bionic build. Earlier thinking run 40765362745 also passed all three jobs.
- Downloaded `3134ee5478` Android artifact passed 44 assertions across 20 real CLI invocations: per-invocation title suppression, non-empty environment truthiness, resume re-enabling titles, preserving existing names, slotless-v3 header integrity, thinking wire precedence and cache-key inheritance. Binary SHA-256 `395e791ca7cda83692780c8e046df0accf5d610ae5db250e6356b632a8e155ac` matches manifest; task-owned proof `$PREFIX/tmp/omp-art-AOXkNp/probe/`, installed CLI unchanged.

### Active block: staged actions and structural edits

- Pending queue and provider request-scoped tool choice prerequisites are published through `2b74a7dd9e`. [Actions 40777961497](https://github.com/SklUpp/omp-termux/actions/runs/40777961497) passed host 764/0, native grep/matcher 38/0 and Android arm64/Bionic build; `gh run watch --exit-status` exited 0. Earlier `9304c4ea86` failed host test compilation because five dispatch fixtures omitted context; this commit fixes those calls.
- Downloaded Android binary SHA-256 `3040401fa1f603256d0ebf776d45eca0b1149cbf1f4066a6b2b454420c560329` matches its manifest. Actual isolated CLI/model fixture exercised empty resolve refusal, empty reject success, device usage, unknown-device refusal, normal file writes and default requests omitting `tool_choice` (7/7). Probe exited 0; no `xd:` filesystem paths or sessions created. Retained evidence: `$PREFIX/tmp/smoke.gZGr7e/{wire.jsonl,run.jsonl,run.err}`. Installed CLI unchanged.
- This artifact cannot stage an AST preview or exercise forced resolution through CLI: `ast_edit` is not yet registered there and the loop still selects Auto. AST compute/apply base is in `63df5b0259`; per-entry glob scope, partial-write retry reporting, hashline preview, root/child store rebinding and staged reminder enforcement are under integration/review. No full staged-path or newer-artifact acceptance is claimed.
- Integrated staged AST and enforcement published as `24e569c54e`; `0d03fadbf2` removes three stale builtin-inventory assertions without changing runtime behavior. [Actions 40783559747](https://github.com/SklUpp/omp-termux/actions/runs/40783559747) passed Android build and native 38/0, but host remains red: four AST unit assertions fail (two expect full lines where previews render matched spans; two inspect action details outside the resolve wrapper). Upstream-contract investigation is active; no full CI acceptance yet.
- Real `0d03fadbf2` Android artifact SHA-256 `c6e79cb992932266d96029cca921317a3a00a24667865589e07e8152cee12c6a` matches manifest and is byte-identical to the prior artifact. Device probe exited 0 for apply 26/26, nested glob 20/20, reject 18/18, stale-byte refusal 19/19, cancel 15/15, baseline 18/18 and child queue isolation 28/28. Apply proves detour suppression, one forced write request and return to Auto; child proves independent staged queues and empty-child resolve refusal while parent preview remains pending. Evidence: `$PREFIX/tmp/smoke.gZGr7e/harness/run0d03-{core,child}.out` and `harness/runs/`. Isolated HOME, localhost fixture, no installed CLI replacement. Exact green-artifact repeat remains pending.
- Contract investigation confirmed upstream previews render matched spans, not full source lines; resolve action details live under `sourceResultDetails`. Test-only corrections shipped in `86dc94b4f7` alongside parsed core-tool settings. [Actions 40786179910](https://github.com/SklUpp/omp-termux/actions/runs/40786179910) is green: host 783/0 across 31 test binaries, native 38/0, Android arm64/Bionic build; watcher exit 0. All four previously failing AST tests now pass. Exact green-artifact device repeat is running; earlier device evidence above remains tied to `0d03fadbf2`.
- Exact green `86dc94b4f7` artifact passed all seven device scenarios again: apply 26/26, nested 20/20, reject 18/18, stale 19/19, cancel 15/15, baseline 18/18, child isolation 28/28; both harness runs exited 0. Manifest-verified binary SHA-256 `f0d7395f5c20ed0ff0e512f4173fdf2f841ba7ec4e6769b07dc9a8db96704513`; proof `$PREFIX/tmp/smoke.gZGr7e/harness/run-green-{core,child}.out`. Staged policy and default hashline AST apply path accepted. Remaining AST configuration work: upstream `edit.mode`/model-dependent display selection; constructor mode currently explicit and runtime always hashline.

### Remote-only verification queue (historical, 2026-10-27 — superseded 2026-10-29)

**Superseded:** the remote-only, no-push and no-commit constraints in this section are a historical snapshot, not active directives. Current authorization permits bounded local checks (≤600s) and normal commit/push/Actions publication. The commands below stay as historical evidence; the remote host was never reachable.

Parent-driven full4 reruns are recorded above; isolated agents make no further local build/test commands. The queue below was for Android Bionic/live-device verification at that time. Remote test target configured as `rust-build` (`user1@176.109.107.63:22`); `ssh rust-build` exits 255 (alias not resolved by local OpenSSH); direct `ssh -o BatchMode=yes -o ConnectTimeout=8 -o StrictHostKeyChecking=yes user1@176.109.107.63` exits 255 (port 22 timeout). No remote test or environment inspection succeeded. Do not claim verification of newer changes, copy secrets, push, or alter the installed CLI. Once SSH reachable, verify remote checkout HEAD and safely transfer only the exact reviewed uncommitted diff/new files to an isolated checkout, never overwrite remote user changes; run commands below there. Android Bionic/live device c…

Pending remote commands, sequential (`CARGO_BUILD_JOBS=1`, `nice -n 10`, `-j1`, `--test-threads=1`):

```sh
CARGO_BUILD_JOBS=1 nice -n 10 cargo test --offline --locked -p pi-agent -p omp-cli -j1 -- --test-threads=1
CARGO_BUILD_JOBS=1 nice -n 10 cargo build --offline --locked -p omp-cli --bin omp -j1
```

Before broad suite, run scoped changed-path tests in same throttled form: `cargo test --offline --locked -p omp-cli -j1 --test live_sse_integration -- --test-threads=1`, `--test behavioral_storage`, `--test approval_gate`, and `cargo test --offline --locked -p pi-agent -j1 --test agent_core -- --test-threads=1`. For each new stage record exit codes, observed CLI/API smoke, CPU and PSS here; static source review alone cannot advance an acceptance gate.

### Static review handoff for isolated agents (historical, 2026-10-27)

Historical WIP state; its "do not commit/reset" instruction applied to that snapshot and is superseded by the 2026-10-29 authorization. The user-file preservation and one-writer rules remain good practice.

`HEAD=e52a1961a1` is the last committed MCP implementation; working tree has user-owned `config.rs`, `runtime.rs`, `mcp_integration.rs` changes plus unfinished stage-2 changes. Do not commit, reset, checkout, or silently integrate them. An agent mistakenly checked out `runtime.rs`; the complete pre-checkout file was reconstructed from its recorded diff and restored. `git hash-object crates/omp-cli/src/runtime.rs` equals `feef59eb3a42a01271e07f1b4cb77601bb3f9d92`; user-only pre-approval blob `1753c1ab2a035b0ab75dcce4b7445a89dcd1fa4a` remains recoverable. Avoid further edits until an isolated slice explicitly owns that file.

Independent static review of committed `HEAD` vs parent found: **P1** `runtime.rs:899-914` starts stdio MCP before approval and `--tools` filtering (a project config can run code); **P1** `runtime.rs:134-150` leaves timeout task alive on discovery error, potentially cancelling caller's reused token; **P2** `runtime.rs:903-955` misreports connection failure as unknown `--tools` ID. Current WIP scheduler gates calls but NOT server launch; fix both boundaries before accepting the approval phase. Separate WIP security review found `runtime.rs:449-459` may abort `tokio::fs::write`/`edit` mid-write and leave a partial existing file. An isolated security slice proposed fixes in session storage, but those files are **not integrated** in this tree; preserve its separate worktree until reviewed. No current changed code was compiled after user froze local testing.

New agent contract: `isolated: true`, explicit `repoRoot: /data/data/com.termux/files/home/omp-work`, one writer per owned file. Isolated worktrees start at committed `HEAD`, **not** this uncommitted worktree: supply a reviewed, narrow patch of prerequisite WIP to each child and reject any checkout/reset of user files. Suggested first lane: MCP launch approval and early-error timer in `runtime.rs` (single owner/integration gate); second lane: atomic existing-file `write`/`edit` and cancellation in `crates/pi-agent/src/tools/mod.rs`; third read-only lane: merge assessment for isolated session/image diffs. No parallel changes to `runtime.rs` or `session.rs`. Parent reconciles WIP and coordinates remote checks in phase order; any other approved stage remains pending until these gates have remote evidence.

Remote SSH probe reached `user1@176.109.107.63` once (`git` found, `rustc` not in default PATH), later port 22 timed out. Rust toolchain under `~/.cargo/bin`, remote repository path and its state remain **unconfirmed**. Do not transfer a patch into an unknown remote tree or assume toolchain works. Read-only probe after reachability: `ssh -o BatchMode=yes -o ConnectTimeout=5 -o StrictHostKeyChecking=yes rust-build 'printf "PATH=%s\n" "$PATH"; command -v git; command -v "$HOME/.cargo/bin/cargo"; test -d "$HOME/omp-remote-lane/repo/.git"'` (avoid printing secrets or private environment). If repository differs, provision an isolated checkout only with explicit agreement and reviewed content; no secret copy or push. Browser and Android Bionic smoke still require Android runtime, regardless of Linux host results.

### Integration log (historical, 2026-10-27)

Base WIP committed as `a91690a20a` + `eafa98ab01`; reviewed atomic write layer merged as `2c2d6c4be0` (source `omp/task/AtomicFileToolRev2`, worktree `t137f1eaa0/m`, reviewer PASS). Pushed to `origin feat/rust-agent-termux-bionic`; Actions `rust-agent.yml` run `40534286595` is the first remote verification of this queue. MCP launch-gate layer NOT merged: review found attacker-shaping bypass (project `.omp/mcp.json` `enabled:false` suppresses competing server, `mcp__{X}` becomes sole candidate and launches silently). Agreed fix: structural fail-closed — bare `mcp__{X}` form never tool-matches, server-level allow only via explicit `mcp:X`/`mcp-server:X`/`X`; intentional TS parity divergence documented in tests. Awaiting fixed layer + positive review before its commit.

CI result run `40534286595` (commit `2c2d6c4be0`): **green**. Host tests `cargo check --locked -p pi-agent -p omp-cli` + `cargo test --locked -p pi-agent -p omp-cli` (7m6s) — full suites of both crates including `agent_core`, `live_sse_integration`, `behavioral_storage`, `approval_gate`, `mcp_integration`, `session_storage`, `foreign_sessions`. Android job `cargo build --release --locked -p omp-cli --target aarch64-linux-android` + `file` ELF-aarch64 check (4m8s), artifact uploaded. Limits: x86_64 test host, not Termux/Bionic runtime; no on-device smoke, no CPU/PSS/startup measurements; `browser`/Chromium paths unexercised. Actions annotations are infra deprecations (Node 20, ubuntu-latest migration), not code findings.

Integration log continued: MCP approval gate merged as `15fdde7838` (source `omp/task/McpLaunchGateFix`, worktree `te4730bc33/m`, `McpPatchReReview` PASS — discovery-shaping closed via union of all config layers; intentional TS parity divergence in tests), compile fixes `35c4c390a4` (edition-2024 `unsafe extern`, prompt type annotation) and `debede529b` (local `TempDir` in `mcp_launch_gate.rs`, libc-accurate `read` signature; static-only reviews cannot catch compile breaks — every reviewed layer must pass `cargo check --tests` before commit). Session store security hardening merged as `09f5b9c906` (`SessionSafetyLive` live-tree edits, `SessionSecurityReviewer` final PASS: O_EXCL create/collision-symlink immutability, terminal ID sanitization, fail-closed untyped records, leak-proof serde diagnostics on v1+v3 paths). Follow-up fixes: `c18b817245` (provider base URL validation at setup), `b797b0ff42` (explicit MCP unavailable message under pre-launch gate), `301b71e348` (session id stem parse after first `_`). CI run `40546857593` (commit `301b71e348`): **green**. Host tests 1m15s; Android Bionic 4m41s. Auto-merge of isolated agent branches enabled; isolated agents require explicit `repoRoot`.

On-device smoke for CI `40546857593`, commit `301b71e348`: downloaded `omp-aarch64-linux-android` to `/data/data/com.termux/files/home/tmp/omp-artifact-smoke-nKHEcx/`; `sha256sum -c omp.sha256` exit 0; `file` reports Android API 24 aarch64 ELF with `/system/bin/linker64`. `omp --version` and `--help` exit 0; `--mode rpc` exit 2; `--print hello` with empty isolated config exit 1 (`no models`). With a local mock OpenAI SSE server and isolated directories, session create and v3 fixture resume both exit 0 and append user/assistant JSONL records. These are not real-provider, tool-call, fork, or performance tests.

### Phase results (historical, 2026-10-27)

| Phase | Scoped test / actual smoke | CPU / PSS | Status / divergence |
|---|---|---|---|
| Inventory and capability | `git status --short`; dependency probes above; browser gate exit 1; `git diff --check` exit 0; per-ID tables reconcile 41 commands, 32 tools, 14 APIs, 58 flag tokens | not measured | Registry classification recorded, user changes preserved. Installed Chromium binary broken; live browser remains unverified. |
| Event/approval/session | CI `40546857593` green for host suites and Android Bionic build; on-device mock SSE session create and v3 resume exit 0. Stage-2 provider-field wire tests are added but not yet run on a host/build host. | not measured | Open: real model/tool turns, fork/import and provider-field acceptance remain (new tests pending host run); review identified MCP denial diagnostics and legacy session-stem matching regressions. |
| File/remote tools | pending | not measured | open |
| Provider/auth/MCP | CI `40546857593` green covers MCP launch gate + provider URL validation. | not measured | MCP pre-launch gate closed; remaining provider/auth surface (catalog auth flows, non-stdio MCP transports) still open. |
| Delegation/tools | pending | not measured | open |
| Headless CLI/modes | pending | not measured | open |
| Android parity/metrics | CI artifact checksum and basic CLI plus mock SSE session create/resume exercised on device. | not measured | Open: remaining on-device parity and CPU/PSS/startup comparisons; no `omp` installation switch. |

Build separate `omp-rs` from existing `crates/omp-cli` (`[[bin]] name = "omp"`), checksum independently in `.github/workflows/rust-agent.yml`; do not alter `install.sh`, `android-release.yml`, or `$PREFIX/bin/omp`. (The former "CI needs explicit push permission" sentence is superseded: the 2026-10-29 authorization covers bounded local checks and commit/push/Actions.) Final acceptance: Bionic ELF/API24, two real model/tool turns, resume/fork, MCP stdio/HTTP, RPC/ACP, task/hub, SSH, external Chromium screenshot/run if validated, no JS PID before `eval.js`/`browser.run`, cancellation cleanup, per-ID disposition, equal-history 8/30-turn CPU/PSS/startup comparisons against installed CLI. No performance claim without measurements.
