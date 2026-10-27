# Rust CLI parity plan

Status: execution approved 2026-10-26; acceptance pending. Target: separate `omp-rs` artifact on Termux/aarch64 Bionic. Rust owns CLI, agent, sessions, tools, auth, and protocols; user-requested JavaScript runs lazily only in `eval(language="js")` and `browser.run`. Existing Bun-based Android release and installed `$PREFIX/bin/omp` remain unchanged until final acceptance.

## Definition of done

Every user-facing TypeScript CLI mode, flag, subcommand, builtin tool, provider dialect, and configuration/session path must have one disposition: equivalent Rust behavior, or an explicit exclusion below. A rejected option must fail with a specific message; silently accepting an option with no effect is not parity. Each implemented class needs a consumer-visible test, Android build, and on-device smoke of its actual path. Compatibility fixtures must use real TypeScript session and protocol output, not source-text assertions.

Authoritative inventories are `packages/coding-agent/src/cli-commands.ts` (subcommands), `packages/coding-agent/src/cli/flag-tables.ts` (flags), `packages/coding-agent/src/tools/builtin-names.ts` (tools), `packages/ai/src/api-registry.ts` (wire dialects), and `packages/coding-agent/src/config/mcp-schema.json` (MCP). Before closing any phase, compare those registries against Rust CLI help and its behavior tests. New upstream entries remain open until classified.

## Boundary

Excluded: extension/hook/plugin execution and `plugin`/`install`; interactive TUI, `rpc-ui`, full-screen `git`/`join`/`shell`, native desktop `computer`, local audio capture/playback, live WebRTC, image clipboard, and `update` without an Android release target. Headless `gallery` ANSI, file/ANSI `render`, cloud TTS and image generation remain in scope. `browser-relay serve` remains conditional on an external Chrome connection. Explicit requests for excluded paths must fail with a specific reason, without side effects. External interpreters, Chromium, and services remain external; JS execution starts only on the user-requested JS paths. No new dependencies, installs, publication, or replacement of the installed CLI without separate approval.

## Current baseline and high-risk gaps

Baseline rechecked 2026-10-26: `crates/omp-cli/src/runtime.rs:190-409` already forwards live bounded `AgentLoop::run_dialog_with` events and emits `AgentEnd` from finished turns; preserve and test this user-modified path, not rewrite it as a synthetic replay. Current provider path still constructs `OpenAiCompatibleProvider` at `runtime.rs:177`; seven tools and existing session/approval implementations need behavioral parity audits. User changes in `crates/omp-cli/src/config.rs`, `runtime.rs`, `tests/mcp_integration.rs`, and this untracked document must be preserved. Existing release installer/workflow and `$PREFIX/bin/omp` are out of scope until acceptance.

## Execution order and acceptance

1. **Live execution and interruption.** Preserve the existing bounded live event channel in `runtime.rs`, test incremental NDJSON before provider completion and Ctrl-C during a real tool call, then fix any observed cancellation/history failures. No post-hoc synthetic replay. Final text exactly once; each started tool call closes with one result.
2. **Sessions and output contract.** Compare a real TypeScript JSONL session with Rust records. Read/write the per-directory layout, resume, fork, import, and provider-session fields without losing old Rust sessions; migration must be explicit and loss-safe. Match text/JSON error and exit semantics with real successful, interrupted, and failed turns. Preserve `--continue` behavior or document an intentional replacement before changing it.
3. **MCP and tool boundaries.** Consume the existing JSON configuration paths and schema; implement initialization, discovery, execution, cancellation, and process cleanup over stdio and HTTP/SSE. Use the upstream protocol version and `mcp__<server>_<tool>` naming; handle failures as tool errors, not empty results. Exercise a real local stdio server and HTTP fixture. Resolve the dynamic `Tool::name` contract before registration.
4. **Files and model payloads.** Expand `@file` using upstream 5 MiB text and 25 MiB image limits; validate format by content, preserve path/error context, and use model-supported image parts. Test text files, supported images, oversize, unreadable paths, and a provider request containing image data. Do not add an image decoder unless resizing becomes necessary for compatible requests.
   *Positional `@file` handling:* `@file` arguments separated into `file_args`, resolved relative to `cwd`, and formatted as `<file name="<abs_path>">\n{content}\n</file>\n` into the initial prompt. 5 MiB text limit (`MAX_AUTO_READ_TEXT_BYTES`) warns to stderr and emits `(skipped: too large, ...)`. Directories emit directory listing. Prevents falling into REPL (`cli.file_args.is_empty()`); `-p @file` does not wait on terminal stdin; piped stdin follows TS `readPipedInput` contract (reads to EOF if non-terminal, returns immediately if terminal TTY) and merges ahead of `@file` text (`stdin + "\n" + file_text + prompt[0]`).
   *Limitations and upgrade paths:* Images (PNG/JPEG/GIF/WebP detected by header sniffing) bounded to 25 MiB and attached to model wire requests as data URIs with text references; session log persists ordered text and image parts and preserves them across session resume; explicit text-only models fail closed; markit document converters (.pdf/.docx) omitted until markit integration.
5. **Provider and authentication parity.** Add the 14 TypeScript wire dialects, model capability routing, credential discovery/refresh, and thinking/effort options by protocol family. Reuse existing HTTP/TLS support; external OAuth flows and provider-specific attestation must have explicit integration tests before being called supported. Test representative request/response streams for every dialect; run live provider checks only where credentials are available, without logging secrets.
6. **Tools and delegated execution.** Port all Android-available builtin tools by observable function, including research, subagents, process/PTY, code intelligence, image inspection, storage, and remote integrations. JavaScript in `eval` uses a lazy persistent kernel only when explicitly called; `browser.run` executes page JS via external Chromium/CDP. No full JS agent. External interpreters, executables, or accounts must produce accurate prerequisite errors. Test concurrency, cancellation, approvals, trust boundaries, and typed tool-result payloads.
7. **CLI flags, modes, and subcommands.** Add upstream flag semantics, settings precedence/profiles, shell-friendly REPL, `rpc` and `acp`, then native-capable command families in dependency order: local read/grep/config/models/session, broker/auth, web/browser, stats/storage, and remaining commands. Do not add placeholder subcommands or accept no-op flags. For network/cloud commands, exercise either a local protocol fixture or a configured live service. Any command excluded because it *only* launches a React view must fail explicitly with the boundary reason.
8. **Parity gate.** Reconcile each upstream registry entry with implemented behavior or the approved exclusions. Run host tests and real CLI scenarios, Android arm64/Bionic build in Actions once a push is authorized, and final Termux smoke of MCP, file/image, session, interrupt, provider, and tool delegation. Update Android docs only for shipped behavior; no release packaging without approval.

## Dependency and risk decisions

- Start with live events: MCP, RPC, subagents, and REPL otherwise need a second event model.
- Port session format before exposing resume/fork: silent writes to a parallel session tree risk apparent data loss.
- Preserve the platform boundary: no full Bun/Node agent fallback, no synthetic success, no ignored flags. Lazy JS kernel for `eval.js` and page JS for `browser.run` are expressly included; outside these requests JS must not start. TS compatibility is measured at behavior/protocol boundaries, not by matching internal class graphs.
- Use built-in/native or already-installed dependencies first. Proposed Rust crates or Termux packages require user approval before installation or lockfile changes.
- CI requires publishing a branch; do not push or make a release from this plan without an explicit user request.

Next action: verify the user-modified live stream with a local two-turn HTTP model/tool fixture and interrupted/resumed session; make only test-driven fixes. Then advance to approval, typed content and v3 session/compaction before the tool and provider phases.

## Execution ledger and evidence

Each ID below is **open**, not automatically supported. Close a row only after its invocation, arguments, typed result, success/error/cancellation test and Android smoke are recorded here. An unavailable account or executable is a runtime prerequisite error, not a reason to remove an otherwise supported interface. New upstream IDs must be reviewed before inclusion. Fixture tests compare observed behavior with the installed TypeScript CLI, never source text. Phase order: (1) inventory/capabilities; (2) live event, approval, v3 session and compaction; (3) file/remote/image tools; (4) all provider wires, auth and MCP; (5) delegation and remaining tools; (6) headless modes/commands; (7) Android artifact, corpus and metrics. Advance only after scoped red/green and actual smoke; record failures in this ledger.

| Registry | Total IDs / tokens | Disposition | Verification methodology |
|---|---|---|---|
| CLI subcommands | 41 subcommands | 35 open, 6 excluded | `--help`, safe invocation with arguments/output, invalid input/diagnostic; rejected modes produce no session/network/JS process. |
| Builtins & hidden tools | 32 tools (29 builtin + 3 hidden) | 31 open, 1 excluded | Actual args/content and typed result, refusal, cancel, and dangerous input on real file/local service. |
| Wire APIs | 14 wire protocols | 14 open | TS-recorded request/stream fixture, tool/image/reasoning/usage/cache/errors/cancel, live smoke when credential exists. |
| MCP transports | 3 transports | 3 open | Initialization, pagination, call, cancellation, timeout, cleanup and auth/header boundary via actual local server. |
| Launch flags (string) | 30 canonical (31 tokens; `-e` alias of `--extension`) | 26 open, 4 excluded | Parser precedence and non-no-op behavior, or explicit excluded usage error; `--plan` extension-shadowable. |
| Launch flags (optional) | 1 canonical (3 tokens; `-r`, `--session` aliases of `--resume`) | 1 open | Bare/value and empty-value rejection, matching session selection. |
| Launch flags (valueless) | 23 canonical (24 tokens; `--yolo` alias of `--auto-approve`) | 23 open | Each changes observed behavior or rejects; `--mode`: `text/json/rpc/acp`, reject `rpc-ui`. |
| Positional operands | 2 categories (`PROMPT`, `@FILE`) | 2 implemented | Separation of `@file` vs prompt messages, `<file name="...">` prompt block injection, 5 MiB limit, image wire rejection, stdin/REPL gating. |

### CLI subcommands (41)

| ID (aliases) | Android disposition | Invocation / arguments / expected result | Verification test | Prerequisite / dependency / refusal reason |
|---|---|---|---|---|
| `launch` | open | pending — not yet recorded (TS: AI coding assistant; modes: text, json, rpc, acp) | pending | Headless modes open; interactive TUI excluded by boundary. |
| `acp` | open | pending — not yet recorded (TS: Run Oh My Pi as an ACP server over stdio) | pending | None; stdio protocol loop. |
| `auth-broker` | open | pending — not yet recorded (TS: Manage the omp auth-broker credential vault) | pending | Local loopback port / auth store access. |
| `auth-gateway` | open | pending — not yet recorded (TS: Run an auth-gateway forward proxy backed by the configured broker) | pending | Broker reachability / proxy port. |
| `agents` | open | pending — not yet recorded (TS: Manage bundled task agents) | pending | Agent directory / configuration access. |
| `bench` | open | pending — not yet recorded (TS: Benchmark models: TTFT/prefill vs decode throughput) | pending | Benchmark suite fixtures / model access. |
| `browser-relay` | open | pending — not yet recorded (TS: Run local CDP relay letting browser tool drive Chrome tabs) | pending | Conditional on external Chrome connection; extension install excluded. |
| `cleanse` | open | pending — not yet recorded (TS: Detect and fix project diagnostics with weighted parallel subagents) | pending | Subagent runner; model access. |
| `commit` | open | pending — not yet recorded (TS: Generate a commit message and update changelogs) | pending | `git` CLI present in repository. |
| `completions` | open | pending — not yet recorded (TS: Print a shell completion script: bash, zsh, or fish) | pending | Shell parser. |
| `__complete` | open | pending — not yet recorded (TS: Hidden internal shell completion) | pending | Argument parser completion engine. |
| `compress` | open | pending — not yet recorded (TS: Rewrite a text file into dense prompt register, reporting drops) | pending | File read/write access. |
| `config` | open | pending — not yet recorded (TS: Manage configuration settings) | pending | Config file read/write access. |
| `dry-balance` | open | pending — not yet recorded (TS: Dry-run OAuth account balancing across random session ids) | pending | OAuth accounts configured. |
| `gc` | open | pending — not yet recorded (TS: Run storage garbage collection) | pending | Storage directory access. |
| `grep` | open | pending — not yet recorded (TS: Test grep tool) | pending | Filesystem read access. |
| `gallery` | open | pending — not yet recorded (TS: Preview tool renderers across streaming/success/failure states) | pending | ANSI preview open; screenshot capture excluded (`vhs` absent on Android). |
| `git` | excluded | pending — not yet recorded (TS: Interactive fullscreen git UI) | pending | Boundary: interactive fullscreen TUI excluded. |
| `grievances` | open | pending — not yet recorded (TS: View, clean, or push reported tool issues) | pending | Grievances store access. |
| `images` (alias `img`) | open | pending — not yet recorded (TS: Inspect, diagnose, probe, and purge image publication backends) | pending | Cache directory access; no auto-installing packages. |
| `if-bench` | open | pending — not yet recorded (TS: Benchmark instruction following and working memory) | pending | Benchmark fixtures. |
| `install` | excluded | pending — not yet recorded (TS: Install or link an extension package) | pending | Boundary: plugin/extension installation excluded. |
| `join` | excluded | pending — not yet recorded (TS: Join a shared collab session) | pending | Boundary: interactive TUI and live WebRTC excluded. |
| `models` | open | pending — not yet recorded (TS: List, search, and refresh available models) | pending | Model registry access. |
| `plugin` | excluded | pending — not yet recorded (TS: Manage plugins: install, uninstall, list, etc.) | pending | Boundary: plugin execution excluded. |
| `ps` | open | pending — not yet recorded (TS: List and control daemon-supervised background processes: logs, stop, kill) | pending | Process table inspection; interactive monitor TUI excluded. |
| `say` | open | pending — not yet recorded (TS: Synthesize text with local TTS engine and play through speakers) | pending | File output open (`say --out`); local speaker playback excluded by boundary. |
| `share` | open | pending — not yet recorded (TS: Share a saved session via an encrypted link) | pending | Session store / network. |
| `setup` | open | pending — not yet recorded (TS: Run onboarding setup or install dependencies for optional features) | pending | Non-interactive paths open; interactive TUI excluded; no auto-package installs. |
| `shell` | excluded | pending — not yet recorded (TS: Interactive shell console) | pending | Boundary: interactive TUI excluded. |
| `read` | open | pending — not yet recorded (TS: Show what the read tool will return for path, URL, or internal URI) | pending | Filesystem / internal URI router. |
| `render` | open | pending — not yet recorded (TS: Draw session thread through transcript pipeline) | pending | Session file / stdout rendering. |
| `ssh` | open | pending — not yet recorded (TS: Manage SSH host configurations) | pending | `ssh` CLI present; host auth required. |
| `stats` | open | pending — not yet recorded (TS: View usage statistics) | pending | Session database access; dashboard TUI excluded. |
| `update` | excluded | pending — not yet recorded (TS: Check for and install updates) | pending | Boundary: no Android release target. |
| `usage` | open | pending — not yet recorded (TS: Show provider usage limits for every authenticated account) | pending | Auth store / provider network access. |
| `tiny-models` | open | pending — not yet recorded (TS: Download tiny local models: session titles + memory) | pending | ONNX runtime dependency unverified on Android; download requires explicit approval. |
| `token` | open | pending — not yet recorded (TS: Get API key or OAuth token for a provider) | pending | Auth database access. |
| `ttsr` | open | pending — not yet recorded (TS: Inspect and test Time-Traveling Stream Rules) | pending | Stream ruleset / session stream access. |
| `worktree` (alias `wt`) | open | pending — not yet recorded (TS: List or clear agent-managed git worktrees ~/.omp/wt) | pending | `git` repository required. |
| `search` (alias `q`) | open | pending — not yet recorded (TS: Test web search providers) | pending | Web search provider credential / network. |

### Builtin tools (29)

| ID (aliases) | Android disposition | Invocation / arguments / expected result | Verification test | Prerequisite / dependency / refusal reason |
|---|---|---|---|---|
| `read` | open | pending — not yet recorded (path with line/archive/sqlite/pdf selectors, internal URIs) | pending | Filesystem / internal URI router; dangerous/outside bounds refused. |
| `bash` | open | pending — not yet recorded (command, timeout?, cwd?, pty?, async?, name?, ready?) | pending | Shell execution; timeout / approval policy enforced. |
| `edit` | open | pending — not yet recorded (hashline patch with snapshot tags PUT/CUT/REM/MV) | pending | Exact tag matching; stale tag / conflict rejected. |
| `ast_grep` | open | pending — not yet recorded (pattern, path?, language?) | pending | Tree-sitter / AST parser for language. |
| `ast_edit` | open | pending — not yet recorded (pattern, rewrite, path) | pending | AST rewrite engine and approval gate. |
| `ask` | open | pending — not yet recorded (question user clarification prompt) | pending | Interactive REPL stdin or RPC/ACP response; refusal on closed channel. |
| `debug` | open | pending — not yet recorded (action, args DAP debugging commands) | pending | DAP adapter executable (e.g. `lldb-dap`, `debugpy`). |
| `eval` | open | pending — not yet recorded (language, code, timeout?, reset? for py/js/rb/jl) | pending | External interpreters; JS kernel starts lazily on first JS request. |
| `github` | open | pending — not yet recorded (action, args GitHub API/PR/issue operations) | pending | `gh` CLI present and authenticated. |
| `glob` (legacy alias `find`) | open | pending — not yet recorded (path?, hidden?, gitignore?, limit?) | pending | Filesystem read access; respects gitignore. |
| `grep` (legacy alias `search`) | open | pending — not yet recorded (pattern, path?, case?, gitignore?, skip?) | pending | PCRE2/Rust regex engine; respects gitignore. |
| `lsp` | open | pending — not yet recorded (action, uri, position? language server queries) | pending | LSP server executable for language (e.g. `rust-analyzer`, `clangd`). |
| `inspect_image` | open | pending — not yet recorded (path decode image dimensions/content) | pending | Image file read access; ≤25 MiB payload limit. |
| `browser` | open | pending — not yet recorded (action, url?, selector?, script? CDP automation) | pending | External Chromium required; blocked by Bionic linker in package 155. |
| `computer` | excluded | pending — not yet recorded (action, coordinate?, text? OS desktop actions) | pending | Boundary: Android desktop AX unavailable (`crates/pi-natives/src/desktop/mod.rs:591-606`). |
| `checkpoint` | open | pending — not yet recorded (name save snapshot of working tree) | pending | Git / working directory storage access. |
| `rewind` | open | pending — not yet recorded (checkpoint_id restore snapshot) | pending | Clean working tree; uncommitted conflicts rejected. |
| `security_scan` | open | pending — not yet recorded (path?, rules? vulnerability scanning) | pending | Security rules engine / git history. |
| `task` | open | pending — not yet recorded (context, tasks[] concurrent delegated subagents) | pending | Subagent supervisor; bounded by `task.maxConcurrency`. |
| `hub` | open | pending — not yet recorded (action, target, payload? subagent IPC and control) | pending | Active supervisor tree / subagent session channels. |
| `todo` | open | pending — not yet recorded (action, items? manage task checklist items) | pending | Session state access. |
| `web_search` | open | pending — not yet recorded (query, limit? query web search API) | pending | Search provider API key and network access. |
| `write` | open | pending — not yet recorded (path, content create or overwrite file) | pending | Approval policy enforced; protected paths refused. |
| `memory_edit` | open | pending — not yet recorded (action, id?, content? update memories) | pending | SQLite memory database access (`~/.omp/agent/agent.db`). |
| `retain` | open | pending — not yet recorded (fact, tags? store durable fact to memory) | pending | SQLite memory database access. |
| `recall` | open | pending — not yet recorded (query, limit? semantic query against memories) | pending | SQLite memory database access. |
| `reflect` | open | pending — not yet recorded (query synthesize answer from recalled memories) | pending | Provider model turn and SQLite memory database access. |
| `learn` | open | pending — not yet recorded (topic, evidence extract recurring rule or skill) | pending | Skill storage directory access. |
| `manage_skill` | open | pending — not yet recorded (action, name? inspect/enable skills) | pending | Skill storage directory access; extension execution excluded. |

### Hidden tools (3)

| ID | Android disposition | Invocation / arguments / expected result | Verification test | Prerequisite / dependency / refusal reason |
|---|---|---|---|---|
| `yield` | open | pending — not yet recorded (status, summary? return control to supervisor) | pending | Supervisor task session channel. |
| `goal` | open | pending — not yet recorded (goal update active supervisor goal) | pending | Supervisor task session channel. |
| `think` | open | pending — not yet recorded (thought attach private model scratchpad) | pending | Provider reasoning block forwarding. |

### Wire APIs (14)

| ID | Android disposition | Invocation / arguments / expected result | Verification test | Prerequisite / dependency / refusal reason |
|---|---|---|---|---|
| `openai-completions` | open | pending — not yet recorded (OpenAI /v1/chat/completions wire format) | pending | OpenAI API key or compatible endpoint. |
| `openai-responses` | open | pending — not yet recorded (OpenAI /v1/responses wire format) | pending | OpenAI API key or compatible endpoint. |
| `openrouter` | open | pending — not yet recorded (OpenRouter /api/v1/chat/completions wire format) | pending | OpenRouter API key. |
| `openai-codex-responses` | open | pending — not yet recorded (OpenAI Codex responses wire format) | pending | OpenAI Codex API key / endpoint. |
| `azure-openai-responses` | open | pending — not yet recorded (Azure OpenAI deployment responses wire format) | pending | Azure OpenAI endpoint, deployment name, API key. |
| `anthropic-messages` | open | pending — not yet recorded (Anthropic /v1/messages SSE stream with thinking/tools) | pending | Anthropic API key. |
| `bedrock-converse-stream` | open | pending — not yet recorded (AWS Bedrock ConverseStream binary event stream) | pending | AWS credentials, region, model access. |
| `google-generative-ai` | open | pending — not yet recorded (Google Gemini Studio generateContent stream) | pending | Google AI Studio API key. |
| `google-gemini-cli` | open | pending — not yet recorded (Google Gemini CLI protocol stream) | pending | Gemini CLI / Google Cloud authentication. |
| `google-vertex` | open | pending — not yet recorded (Google Vertex AI predictStream / generateContent) | pending | Google Cloud project, ADC credentials, region. |
| `ollama-chat` | open | pending — not yet recorded (Ollama /api/chat JSON stream) | pending | Local or reachable remote Ollama daemon. |
| `cursor-agent` | open | pending — not yet recorded (Cursor agent protocol stream) | pending | Cursor authentication token. |
| `gitlab-duo-agent` | open | pending — not yet recorded (GitLab Duo agent REST/SSE stream) | pending | GitLab Duo token and instance URL. |
| `devin-agent` | open | pending — not yet recorded (Devin agent sessions protocol stream) | pending | Devin API key. |

*Note:* Custom API IDs fail explicitly with diagnostic: `custom API requires extensions`.

### MCP transports (3)

| Transport / Setting | Android disposition | Invocation / arguments / expected result | Verification test | Prerequisite / dependency / refusal reason |
|---|---|---|---|---|
| `stdio` | open | pending — not yet recorded (stdio transport: spawn child process with stdin/stdout) | pending | Server executable present and executable. |
| `http` | open | pending — not yet recorded (HTTP transport: POST JSON-RPC to endpoint URL) | pending | Remote HTTP endpoint reachability and valid auth headers. |
| `sse` | open | pending — not yet recorded (SSE transport: GET SSE stream + POST endpoint URL) | pending | Remote SSE endpoint reachability and valid auth headers. |

*Note:* Configuration schema keys `mcpServers`, `disabledServers`, `enabledServers`; auth `oauth` / `apikey`.

### Launch flags: string (30 canonical IDs, 31 registered argv tokens)

| Flag (aliases) | Android disposition | Invocation / arguments / expected result | Verification test | Prerequisite / dependency / refusal reason |
|---|---|---|---|---|
| `--cwd <dir>` | open | pending — not yet recorded (TS: Directory to start in) | pending | Target directory must exist and be accessible. |
| `--config <path>` | open | pending — not yet recorded (TS: Load extra config overlay, repeatable) | pending | Config file must exist and be valid format. |
| `--add-dir <dir>` | open | pending — not yet recorded (TS: Add workspace directory beyond cwd, repeatable) | pending | Path must exist. |
| `--mode <mode>` | open | pending — not yet recorded (TS options: text, json, rpc, acp, rpc-ui) | pending | Valid mode; `rpc-ui` rejected by boundary. |
| `--fork <id>` | open | pending — not yet recorded (TS: Fork existing session from ID) | pending | Target session ID must exist in session store. |
| `--provider <name>` | open | pending — not yet recorded (TS: Provider to use, legacy; prefer --model) | pending | Provider ID must be registered in API registry. |
| `--model <name>` | open | pending — not yet recorded (TS: Model to use fuzzy match) | pending | Model ID recognized by selected provider. |
| `--smol <model>` | open | pending — not yet recorded (TS: Smol/fast model for lightweight tasks) | pending | Model recognized by provider. |
| `--slow <model>` | open | pending — not yet recorded (TS: Slow/reasoning model for thorough analysis) | pending | Model recognized by provider. |
| `--plan <model>` | open | pending — not yet recorded (TS: Plan model for architectural planning) | pending | Value consumed unless shadowed by extension boolean. |
| `--prewalk-into <model>` | open | pending — not yet recorded (TS: Target model for prewalk, default smol) | pending | Target model recognized by provider. |
| `--plan-yolo-into <model>` | open | pending — not yet recorded (TS: Target model for plan-yolo execution, default smol) | pending | Target model recognized by provider. |
| `--max-time <duration>` | open | pending — not yet recorded (TS: Stop session after duration e.g. 600, 10m, 1h) | pending | Positive finite duration required; invalid syntax rejected. |
| `--service-tier <tier>` | open | pending — not yet recorded (TS options: none, auto, default, flex, scale, priority) | pending | Must match supported provider tier. |
| `--api-key <key>` | open | pending — not yet recorded (TS: API key defaults to env vars) | pending | Never logged to stdout/NDJSON/disk. |
| `--system-prompt <prompt>` | open | pending — not yet recorded (TS: System prompt default coding assistant) | pending | None. |
| `--append-system-prompt <text>` | open | pending — not yet recorded (TS: Append text or file contents to system prompt) | pending | None. |
| `--provider-session-id <id>` | open | pending — not yet recorded (TS: Pass sticky session ID to provider) | pending | Provider support for session sticky routing. |
| `--prompt-cache-key <key>` | open | pending — not yet recorded (TS: Set provider prompt cache breakpoint/key) | pending | Provider support for prompt caching. |
| `--session-dir <dir>` | open | pending — not yet recorded (TS: Directory for session storage and lookup) | pending | Directory must be writable. |
| `--models <list>` | open | pending — not yet recorded (TS: Comma-separated model patterns for Ctrl+P cycling) | pending | None. |
| `--tools <list>` | open | pending — not yet recorded (TS: Comma-separated list of tools to enable, default all) | pending | Unknown tools produce warning/rejection. |
| `--thinking <level>` | open | pending — not yet recorded (TS options: off, minimal, low, medium, high, xhigh, max, auto) | pending | Provider/model support for thinking level. |
| `--export <path>` | open | pending — not yet recorded (TS: Export session file to HTML and exit) | pending | Destination path must be writable. |
| `--hook <hook>` | excluded | pending — not yet recorded (TS: Load hook/extension file, repeatable) | pending | Boundary: hook execution excluded. |
| `--extension <path>` (alias `-e`) | excluded | pending — not yet recorded (TS: Load extension file, repeatable) | pending | Boundary: extension execution excluded. |
| `--trusted-extension <id>` | excluded | pending — not yet recorded (TS: Allow trusted extension execution) | pending | Boundary: extension execution excluded. |
| `--plugin-dir <dir>` | excluded | pending — not yet recorded (TS: Directory to search for plugins) | pending | Boundary: plugin execution excluded. |
| `--skills <list>` | open | pending — not yet recorded (TS: Comma-separated glob patterns to filter skills) | pending | Skill files must exist in skill directory. |
| `--approval-mode <mode>` | open | pending — not yet recorded (TS options: always-ask, write, yolo) | pending | Value must be one of `always-ask`, `write`, `yolo`. |

*Note:* Sourced from `packages/coding-agent/src/cli/flag-tables.ts` (`STRING_SETTERS`: 31 registered keys; `-e` aliases `--extension`).

### Launch flags: optional (1 canonical ID, 3 registered argv tokens)

| Flag (aliases) | Android disposition | Invocation / arguments / expected result | Verification test | Prerequisite / dependency / refusal reason |
|---|---|---|---|---|
| `--resume [id]` (alias `-r`, `--session`) | open | pending — not yet recorded (TS: Resume session by ID prefix, path, or picker if omitted) | pending | Target session must exist; empty string value rejected. |

*Note:* Sourced from `packages/coding-agent/src/cli/flag-tables.ts` (`OPTIONAL_FLAGS`: 3 registered keys; `-r` and `--session` alias `--resume`).

### Launch flags: valueless (23 canonical IDs, 24 registered argv tokens)

| Flag (aliases) | Android disposition | Invocation / arguments / expected result | Verification test | Prerequisite / dependency / refusal reason |
|---|---|---|---|---|
| `--help` | open | pending — not yet recorded (TS: Print usage help and exit) | pending | None. |
| `--version` | open | pending — not yet recorded (TS: Print version string and exit) | pending | None. |
| `--allow-home` | open | pending — not yet recorded (TS: Allow starting in ~ without auto-switching to temp dir) | pending | None. |
| `--continue` | open | pending — not yet recorded (TS: Continue previous session) | pending | Historical session must exist in directory. |
| `--from-claude` | open | pending — not yet recorded (TS: Import Claude Code session into OMP) | pending | Valid Claude export format. |
| `--from-codex` | open | pending — not yet recorded (TS: Import Codex session into OMP) | pending | Valid Codex export format. |
| `--no-session` | open | pending — not yet recorded (TS: Don't save session ephemeral) | pending | None. |
| `--no-tools` | open | pending — not yet recorded (TS: Disable all built-in tools) | pending | None. |
| `--no-lsp` | open | pending — not yet recorded (TS: Disable LSP tools, formatting, and diagnostics) | pending | None. |
| `--no-pty` | open | pending — not yet recorded (TS: Disable PTY-based interactive bash execution) | pending | None. |
| `--hide-thinking` | open | pending — not yet recorded (TS: Hide thinking blocks in TUI output) | pending | None. |
| `--advisor` | open | pending — not yet recorded (TS: Enable advisor runtime passively reviews turns) | pending | None. |
| `--external-thinking` | open | pending — not yet recorded (TS: Use private scratchpad while disabling provider reasoning) | pending | Provider support for thinking stream. |
| `--prewalk` | open | pending — not yet recorded (TS: Switch to fast/cheap model at first edit/write after plan todo) | pending | None. |
| `--no-prewalk` | open | pending — not yet recorded (TS: Disable prewalk even if prewalk.enabled is set) | pending | None. |
| `--plan-yolo` | open | pending — not yet recorded (TS: Force read-only plan mode, auto-approve, switch to plan-yolo-into) | pending | None. |
| `--print` | open | pending — not yet recorded (TS: Non-interactive mode process prompt and exit) | pending | Non-interactive mode. |
| `--print-thoughts` | open | pending — not yet recorded (TS: Include thinking blocks in print mode text output) | pending | None. |
| `--no-extensions` | open | pending — not yet recorded (TS: Disable extension discovery) | pending | Aligns with Android boundary. |
| `--no-skills` | open | pending — not yet recorded (TS: Disable skills discovery and loading) | pending | None. |
| `--no-rules` | open | pending — not yet recorded (TS: Disable rules discovery and loading) | pending | None. |
| `--no-title` | open | pending — not yet recorded (TS: Disable title auto-generation) | pending | None. |
| `--auto-approve` (alias `--yolo`) | open | pending — not yet recorded (TS: Auto-approve all tool calls skip approval prompts) | pending | None. |

*Note:* Sourced from `packages/coding-agent/src/cli/flag-tables.ts` (`VALUELESS_FLAGS`: 24 registered keys; `--yolo` aliases `--auto-approve`).

### Capability probes (2026-10-26, read-only)

`command -v`: present `ssh`, `gh`, `python`, `python3`, `ruby`, `termux-clipboard-get`, `clangd`, `rust-analyzer`, `node`, `bun`, `termux-tts-speak`, `termux-microphone-record`; absent `vhs`, `julia`, `lldb-vscode`, `codelldb`, `onnxruntime`, `tiny-models`. Presence does not establish working auth, clipboard service, LSP/DAP, JS kernel, or ONNX speech/tiny backend. `$PREFIX/lib/chromium/chrome` executable exists, but `bash android/scripts/verify-browser.sh --skip-deb-sha256` exits **1** because installed Chromium package `155.0.8059.39` differs from script-pinned `149.0.7827.155`; direct `chrome --version` also exits **1**: Bionic linker cannot locate `_ZNSt6__ndk113__hash_memoryEPKvm`. Live browser blocked by this broken prerequisite; no install/downgrade. `$PREFIX/lib/chromium/headless_shell` exists but is not the selected CDP executable. Recheck prerequisites at invocation. ONNX speech/tiny and screenshot gallery capability unavailable/unproved; do not claim platform support. Probe commands: `command -v <program>`, `bash android/scripts/verify-browser.sh --skip-deb-sha256`, executable `--version`, capability-specific read-only smoke; never pass `--install`.

LSP/DAP executable probe (`command -v`): present `debugpy`, `rdbg`, `gdb`, `lldb-dap`, `pyright-langserver`, `typescript-language-server`, `clangd`, `rust-analyzer`; absent `dlv`, `js-debug-adapter`, `lldb-vscode`, `codelldb`. Backend protocol and cancellation remain untested; presence alone is not support.

### Remote-only verification queue (user directive, 2026-10-27)

No further local build/test commands. Local work limited to static inspection, source edits and `git diff --check`. Remote test target configured as `rust-build` (`user1@176.109.107.63:22`); `ssh rust-build` exits 255 (alias not resolved by local OpenSSH); direct `ssh -o BatchMode=yes -o ConnectTimeout=8 -o StrictHostKeyChecking=yes user1@176.109.107.63` exits 255 (port 22 timeout). No remote test or environment inspection succeeded. Do not claim verification of newer changes, copy secrets, push, or alter the installed CLI. Once SSH reachable, verify remote checkout HEAD and safely transfer only the exact reviewed uncommitted diff/new files to an isolated checkout, never overwrite remote user changes; run commands below there. Android Bionic/live device checks still require a working Android runner and cannot be inferred from a Linux host.

Pending remote commands, sequential (`CARGO_BUILD_JOBS=1`, `nice -n 10`, `-j1`, `--test-threads=1`):

```sh
CARGO_BUILD_JOBS=1 nice -n 10 cargo test --offline --locked -p pi-agent -p omp-cli -j1 -- --test-threads=1
CARGO_BUILD_JOBS=1 nice -n 10 cargo build --offline --locked -p omp-cli --bin omp -j1
```

Before broad suite, run scoped changed-path tests in same throttled form: `cargo test --offline --locked -p omp-cli -j1 --test live_sse_integration -- --test-threads=1`, `--test behavioral_storage`, `--test approval_gate`, and `cargo test --offline --locked -p pi-agent -j1 --test agent_core -- --test-threads=1`. For each new stage record exit codes, observed CLI/API smoke, CPU and PSS here; static source review alone cannot advance an acceptance gate.

### Static review handoff for isolated agents (2026-10-27)

`HEAD=e52a1961a1` is the last committed MCP implementation; working tree has user-owned `config.rs`, `runtime.rs`, `mcp_integration.rs` changes plus unfinished stage-2 changes. Do not commit, reset, checkout, or silently integrate them. An agent mistakenly checked out `runtime.rs`; the complete pre-checkout file was reconstructed from its recorded diff and restored. `git hash-object crates/omp-cli/src/runtime.rs` equals `feef59eb3a42a01271e07f1b4cb77601bb3f9d92`; user-only pre-approval blob `1753c1ab2a035b0ab75dcce4b7445a89dcd1fa4a` remains recoverable. Avoid further edits until an isolated slice explicitly owns that file.

Independent static review of committed `HEAD` vs parent found: **P1** `runtime.rs:899-914` starts stdio MCP before approval and `--tools` filtering (a project config can run code); **P1** `runtime.rs:134-150` leaves timeout task alive on discovery error, potentially cancelling caller's reused token; **P2** `runtime.rs:903-955` misreports connection failure as unknown `--tools` ID. Current WIP scheduler gates calls but NOT server launch; fix both boundaries before accepting the approval phase. Separate WIP security review found `runtime.rs:449-459` may abort `tokio::fs::write`/`edit` mid-write and leave a partial existing file. An isolated security slice proposed fixes in session storage, but those files are **not integrated** in this tree; preserve its separate worktree until reviewed. No current changed code was compiled after user froze local testing.

New agent contract: `isolated: true`, explicit `repoRoot: /data/data/com.termux/files/home/omp-work`, one writer per owned file. Isolated worktrees start at committed `HEAD`, **not** this uncommitted worktree: supply a reviewed, narrow patch of prerequisite WIP to each child and reject any checkout/reset of user files. Suggested first lane: MCP launch approval and early-error timer in `runtime.rs` (single owner/integration gate); second lane: atomic existing-file `write`/`edit` and cancellation in `crates/pi-agent/src/tools/mod.rs`; third read-only lane: merge assessment for isolated session/image diffs. No parallel changes to `runtime.rs` or `session.rs`. Parent reconciles WIP and coordinates remote checks in phase order; any other approved stage remains pending until these gates have remote evidence.

Remote SSH probe reached `user1@176.109.107.63` once (`git` found, `rustc` not in default PATH), later port 22 timed out. Rust toolchain under `~/.cargo/bin`, remote repository path and its state remain **unconfirmed**. Do not transfer a patch into an unknown remote tree or assume toolchain works. Read-only probe after reachability: `ssh -o BatchMode=yes -o ConnectTimeout=5 -o StrictHostKeyChecking=yes rust-build 'printf "PATH=%s\n" "$PATH"; command -v git; command -v "$HOME/.cargo/bin/cargo"; test -d "$HOME/omp-remote-lane/repo/.git"'` (avoid printing secrets or private environment). If repository differs, provision an isolated checkout only with explicit agreement and reviewed content; no secret copy or push. Browser and Android Bionic smoke still require Android runtime, regardless of Linux host results.

### Integration log (2026-10-27)

Base WIP committed as `a91690a20a` + `eafa98ab01`; reviewed atomic write layer merged as `2c2d6c4be0` (source `omp/task/AtomicFileToolRev2`, worktree `t137f1eaa0/m`, reviewer PASS). Pushed to `origin feat/rust-agent-termux-bionic`; Actions `rust-agent.yml` run `40534286595` is the first remote verification of this queue. MCP launch-gate layer NOT merged: review found attacker-shaping bypass (project `.omp/mcp.json` `enabled:false` suppresses competing server, `mcp__{X}` becomes sole candidate and launches silently). Agreed fix: structural fail-closed — bare `mcp__{X}` form never tool-matches, server-level allow only via explicit `mcp:X`/`mcp-server:X`/`X`; intentional TS parity divergence documented in tests. Awaiting fixed layer + positive review before its commit.

CI result run `40534286595` (commit `2c2d6c4be0`): **green**. Host tests `cargo check --locked -p pi-agent -p omp-cli` + `cargo test --locked -p pi-agent -p omp-cli` (7m6s) — full suites of both crates including `agent_core`, `live_sse_integration`, `behavioral_storage`, `approval_gate`, `mcp_integration`, `session_storage`, `foreign_sessions`. Android job `cargo build --release --locked -p omp-cli --target aarch64-linux-android` + `file` ELF-aarch64 check (4m8s), artifact uploaded. Limits: x86_64 test host, not Termux/Bionic runtime; no on-device smoke, no CPU/PSS/startup measurements; `browser`/Chromium paths unexercised. Actions annotations are infra deprecations (Node 20, ubuntu-latest migration), not code findings.

Integration log continued: MCP approval gate merged as `15fdde7838` (source `omp/task/McpLaunchGateFix`, worktree `te4730bc33/m`, `McpPatchReReview` PASS — discovery-shaping closed via union of all config layers; intentional TS parity divergence in tests), compile fixes `35c4c390a4` (edition-2024 `unsafe extern`, prompt type annotation) and `debede529b` (local `TempDir` in `mcp_launch_gate.rs`, libc-accurate `read` signature; static-only reviews cannot catch compile breaks — every reviewed layer must pass `cargo check --tests` before commit). Session store security hardening merged as `09f5b9c906` (`SessionSafetyLive` live-tree edits, `SessionSecurityReviewer` final PASS: O_EXCL create/collision-symlink immutability, terminal ID sanitization, fail-closed untyped records, leak-proof serde diagnostics on v1+v3 paths). Follow-up fixes: `c18b817245` (provider base URL validation at setup), `b797b0ff42` (explicit MCP unavailable message under pre-launch gate), `301b71e348` (session id stem parse after first `_`). CI run `40546857593` (commit `301b71e348`): **green**. Host tests 1m15s; Android Bionic 4m41s. Auto-merge of isolated agent branches enabled; isolated agents require explicit `repoRoot`.

On-device smoke for CI `40546857593`, commit `301b71e348`: downloaded `omp-aarch64-linux-android` to `/data/data/com.termux/files/home/tmp/omp-artifact-smoke-nKHEcx/`; `sha256sum -c omp.sha256` exit 0; `file` reports Android API 24 aarch64 ELF with `/system/bin/linker64`. `omp --version` and `--help` exit 0; `--mode rpc` exit 2; `--print hello` with empty isolated config exit 1 (`no models`). With a local mock OpenAI SSE server and isolated directories, session create and v3 fixture resume both exit 0 and append user/assistant JSONL records. These are not real-provider, tool-call, fork, or performance tests.

### Phase results

| Phase | Scoped test / actual smoke | CPU / PSS | Status / divergence |
|---|---|---|---|
| Inventory and capability | `git status --short`; dependency probes above; browser gate exit 1; `git diff --check` exit 0; per-ID tables reconcile 41 commands, 32 tools, 14 APIs, 58 flag tokens | not measured | Registry classification recorded, user changes preserved. Installed Chromium binary broken; live browser remains unverified. |
| Event/approval/session | CI `40546857593` green for host suites and Android Bionic build; on-device mock SSE session create and v3 resume exit 0. | not measured | Open: real model/tool turns, fork/import and provider-field acceptance remain; review identified MCP denial diagnostics and legacy session-stem matching regressions. |
| File/remote tools | pending | not measured | open |
| Provider/auth/MCP | CI `40546857593` green covers MCP launch gate + provider URL validation. | not measured | MCP pre-launch gate closed; remaining provider/auth surface (catalog auth flows, non-stdio MCP transports) still open. |
| Delegation/tools | pending | not measured | open |
| Headless CLI/modes | pending | not measured | open |
| Android parity/metrics | CI artifact checksum and basic CLI plus mock SSE session create/resume exercised on device. | not measured | Open: remaining on-device parity and CPU/PSS/startup comparisons; no `omp` installation switch. |

Build separate `omp-rs` from existing `crates/omp-cli` (`[[bin]] name = "omp"`), checksum independently in `.github/workflows/rust-agent.yml`; do not alter `install.sh`, `android-release.yml`, or `$PREFIX/bin/omp`. CI execution needs explicit push permission. Final acceptance: Bionic ELF/API24, two real model/tool turns, resume/fork, MCP stdio/HTTP, RPC/ACP, task/hub, SSH, external Chromium screenshot/run if validated, no JS PID before `eval.js`/`browser.run`, cancellation cleanup, per-ID disposition, equal-history 8/30-turn CPU/PSS/startup comparisons against installed CLI. No performance claim without measurements.
