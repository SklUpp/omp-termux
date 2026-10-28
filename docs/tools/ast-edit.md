# ast_edit

> Preview and apply structural rewrites over source files via native ast-grep.

## Source
- Entry: `packages/coding-agent/src/tools/ast-edit.ts`
- Rust port: `crates/pi-agent/src/tools/ast_edit.rs`
- Model-facing prompt: `packages/coding-agent/src/prompts/tools/ast-edit.md`
- Key collaborators:
  - `crates/pi-agent/src/tools/ast_edit.rs` — preview compute, exact-byte stale gate, prepared-output apply
  - `crates/pi-ast/src/ops.rs` — per-language pattern compilation, AST matching, rewrite edits
  - `crates/pi-agent/src/tools/ast_grep.rs` — shared target planning, file-URL handling, root guard, candidate discovery
  - `crates/pi-agent/src/tools/hashline/snapshot.rs` — tag minting and `[PATH#TAG]` header formatting reused by hashline previews
  - `crates/pi-agent/src/tools/pending.rs` — staged action queue; failed applies requeue the same entry
  - `crates/pi-agent/src/tools/mod.rs` — `write_atomic` (mode/symlink preserve) and builtin registration
  - `crates/pi-natives/src/ast.rs` — upstream native rewrite reference
  - `packages/coding-agent/src/tools/path-utils.ts` — upstream path/glob parsing and multi-path resolution
  - `packages/coding-agent/src/tools/resolve.ts` — upstream preview/apply queueing

## Inputs

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `ops` | `{ pat: string; out: string }[]` | Yes | One or more rewrite rules. `pat` must be non-empty. Duplicate `pat` values fail before native execution. Empty `out` deletes the matched node. |
| `paths` | `string[]` | Yes | One or more files, directories, globs, or path-backed internal URLs. At least one non-empty entry is required. Internal-URL globs are rejected; fetched external URLs are read-only and cannot be rewritten. |

Shared AST pattern grammar and language catalog: see [`ast_grep`](./ast-grep.md#inputs).

- `ast_edit` uses the same `$NAME`, `$_`, `$$$NAME`, and `$$$` metavariable semantics.
- The tool prompt adds rewrite-specific constraints:
  - metavariable names must be uppercase and must stand for whole AST nodes,
  - captures from `pat` are substituted into `out`,
  - each rewrite is a 1:1 structural substitution; one capture cannot expand into multiple sibling nodes unless the grammar itself permits that expansion at that position.

`ast_edit` is enabled by default by `astEdit.enabled`. It is discoverable rather than part of the essential tool set.

## Outputs
- Single-shot preview result from `ast_edit` itself. A non-empty proposal begins with `Staged as a proposal — files NOT modified yet...` and names the resolve/reject device paths.
- Model-facing `content` is one text block showing proposed edits, grouped by file for directory/multi-file runs.
  - Rendering follows the explicit `PreviewDisplay` input (`AstEditTool::with_display`):
    - `Plain`: `-LINE:COLUMN before` / `+LINE:COLUMN after`, no tag headers (grouped runs still get the folded `# dir/` / `## file (N replacement)` headers).
    - `Hashline`: a `[PATH#TAG]` header above a single-file preview, `-LINE:before` / `+LINE:after` changes (no column), and grouped headers with the tag appended (`# dir/`, `## file#TAG (N replacement)`).
    - Grouped output is the upstream `formatGroupedFiles` tree: one `#` per nesting level, single-child directory chains folded into one header, files listed under their deepest directory header, and blank lines before directory headers and root-level files.
    - `details.displayContent` carries the TUI code-frame view: `-LINE│text` / `+LINE│text` per change, gutter padded to the widest line number in the file. Grouped frames repeat the same grouped headers; a single-file frame has no header. This is display metadata, not model content.
    - Tags shown in a hashline preview are minted from the pre-edit bytes and recorded in the session snapshot store, so a follow-up `edit` can anchor to them without a re-read. Successful apply re-records post-apply tags and appends fresh `[PATH#TAG]` headers only in `Hashline` mode.
  - Only the first line of each `before`/`after` snippet is shown, truncated to 120 characters.
  - `Limit reached; narrow paths.` and formatted parse issues are appended when applicable.
- If no rewrites match, text is `No replacements made` plus formatted parse issues when present.
- `details` includes aggregate preview metadata:
  - `totalReplacements`, `filesTouched`, `filesSearched`, `applied`, `limitReached`
  - optional `parseErrors`, `parseErrorsTotal`, `scopePath`, `files`, `fileReplacements`, `displayContent`, `searchPath`, `cwd`
  - `changes`: per-match `path`, `before`, `after`, byte/line/column ranges, and metavariable `captures`
- The tool always previews first (`applied: false` in the direct result). Actual file writes happen only later through a plain-text `write` to `xd://resolve`; the body is the reason.
- When preview produced replacements, `ast_edit` also queues a pending resolve action. Successful apply returns a separate resolve dispatch result (on the `write` call), not another `ast_edit` result.

## Flow
1. `AstEditTool.execute()` validates each op in `crates/pi-agent/src/tools/ast_edit.rs`:
   - empty `pat` fails,
   - at least one op is required,
   - duplicate `pat` values fail,
   - ops are converted to a `Record<pattern, replacement>`.
2. The wrapper reads `PI_MAX_AST_FILES` via `$envpos(..., 1000)` and uses that as the native `maxFiles` cap for both preview and apply.
3. Each `paths` entry is resolved independently through `plan_targets_single` (per-entry `parseSearchPath` semantics, not `ast_grep`'s multi-path union): a leading glob stays at its entry's root (`*.ts` is top-level, `**/*.ts` recurses), every entry keeps its own base, overlapping entries dedupe to one canonical file, and missing-path partitioning plus the `/` root guard still apply. The port has no internal-URL or remote resolver, so non-local schemes are rejected.
4. The scope's `isDirectory` flag (set by a stat in `resolveToolSearchScope`) decides whether to render grouped directory output.
5. `runAstEditOnce(...)` always runs native `astEdit(...)` with `dryRun: true` and `failOnParseError: false` on the first pass.
6. Rust preview (`crates/pi-agent/src/tools/ast_edit.rs`):
   - sorts `ops` by pattern string,
   - resolves `smart` strictness and writes no files,
   - reuses `ast_grep` target planning/candidate discovery (explicit errors for unknown/internal/SSH targets; `file://` maps locally; root guard preserved),
   - infers a language per candidate and compiles each rule for every discovered language, reporting per-language parse issues,
   - parses each file once, skips error trees, collects `replace_by(...)` edits, deduplicates byte-identical edits, and enforces the `maxFiles` cap,
   - stores the **exact original bytes** of every affected file plus the prepared rewritten bytes; no content hash stands in for the bytes,
   - renders the preview in the tool's `PreviewDisplay` mode (plain `-LINE:COLUMN` pairs or hashline `[PATH#TAG]`/tag-header output), builds grouped runs as the folded `formatGroupedFiles` tree, emits code frames as `details.displayContent`, and exposes captures in `details.changes`.
7. Parse errors are deduplicated and capped at 20; `details.parseErrors` carries the capped list and `parseErrorsTotal` the pre-cap count.
8. A non-zero preview registers `AstEditPending` in the session `PendingStore`. Zero replacements register nothing. Apply/discard is a plain `write` to `xd://resolve` / `xd://reject`.
9. Apply first re-reads **all** affected files and compares bytes to the preview's originals. Any changed or unreadable file is stale: zero files are written and the action is requeued. A same-count content change fails this gate; counts alone never validate a preview.
10. After the all-files gate, each prepared output is written through `write_atomic` (mode and symlink preserved). A per-file recheck narrows the gate-to-write window; there is no filesystem lock, so a writer racing inside that window can still be overwritten.
11. A failure or cancellation after some writes is reported as a partial apply: it lists written files and snapshot invalidations, does not rewrite the remainder, and requeues the action. Retrying then fails the byte gate because already-written files no longer match the preview.
12. On success the result is `Applied N replacements in M files.` followed, only in `Hashline` mode, by fresh `[path#TAG]` headers for snapshots re-recorded from the post-apply bytes (tags are always re-recorded so `edit` can anchor); on discard (`write xd://reject`) no file is mutated.

## Modes / Variants
- Single file: preview or apply against one file.
- Directory + optional glob: native scan walks the directory, then filters by compiled glob.
- Multiple explicit paths/globs: wrapper unions them into one synthetic scope or runs per-target native calls when paths only meet at root.
- Internal URL inputs: only supported when the router resolves them to a backing file path.
- Preview mode: always the direct `ast_edit` tool result.
- Apply mode: only reachable through the queued resolve callback (a `write` to `xd://resolve` or `xd://reject`) after a preview.
- Preview display mode: explicit `PreviewDisplay` constructor input. `Plain` renders `LINE:COLUMN`; `Hashline` renders `[PATH#TAG]` / tagged grouped headers and records the shown tags from pre-edit bytes. `builtin_tools()` selects `Hashline` (upstream's default `edit.mode` is `hashline`; `read` in this registry always shows hashline headers), while standalone `AstEditTool::new` keeps `Plain`. No runtime `edit.mode`/model-variant settings resolver exists in this port yet, so the mode is fixed at construction.

## Side Effects
- Filesystem
  - Preview reads files and scans directories; it never writes.
  - Apply verifies every affected file's exact bytes before the first write, then writes prepared outputs one by one. A compute/overlap failure happens in the preview phase, before any write. A write failure or cancellation after the first write is a partial apply and is reported honestly; it is not a multi-file transaction.
  - Writes go through `write_atomic`: destination mode is preserved, symlinks are resolved to their target, and cancellation before the rename leaves the target untouched.
- Session state (transcript, memory, jobs, checkpoints, registries)
  - Registers an `AstEditPending` action in the session `PendingStore` when the preview has replacements.
  - Failed, stale, or cancelled applies requeue the same action; a successful resolve or a reject consumes it.
  - The Rust port does not wire provider `tool_choice` forcing or a soft-tool reminder; that policy seam belongs to the runtime/approval step.
- User-visible prompts / interactive UI
  - Direct `ast_edit` results are previews.
  - Follow-up apply/discard is exposed through writes to `xd://resolve` and `xd://reject`.
- Background work / cancellation
  - The Rust preview compute runs synchronously inside the tool call and checks the session cancellation token between candidates, rules, and matches.
  - Apply checks cancellation before the all-files gate, between gate reads, and before each write; a cancellation after some writes returns a partial result instead of continuing.

## Limits & Caps
- File cap: `PI_MAX_AST_FILES`, default `1000`; the Rust port reads it from the agent environment (`crates/pi-agent/src/tools/ast_edit.rs`), upstream from `packages/coding-agent/src/tools/ast-edit.ts`.
- Native `maxFiles` and `maxReplacements` are both clamped to at least `1` when provided in `crates/pi-natives/src/ast.rs`.
- The wrapper never sets `maxReplacements`; native behavior therefore defaults to effectively unbounded replacements for a run.
- Parse issues are deduplicated and capped at `PARSE_ERRORS_LIMIT = 20` entries via `capParseErrors(...)` in `packages/coding-agent/src/tools/render-utils.ts`; `details.parseErrors` carries the capped list and `details.parseErrorsTotal` the pre-cap deduplicated count.
- Directory scans use `include_hidden: true`, `use_gitignore: true`, and skip `node_modules` unless the glob text explicitly mentions `node_modules` in `crates/pi-natives/src/ast.rs`.
- No separate glob-expansion count cap exists. Candidate count is whatever the resolved path/glob expands to after gitignore filtering, then native `maxFiles` stops mutations after the configured number of touched files.
- Preview text truncates each rendered `before` and `after` first line to 120 characters in `packages/coding-agent/src/tools/ast-edit.ts`.

## Errors
- TS wrapper throws `ToolError` for empty patterns, duplicate rewrite patterns, empty path entries, unsupported internal-URL globs, internal URLs without `sourcePath`, and missing paths.
- Native code returns hard errors for:
  - inability to infer a supported language for a candidate (reported as a parse issue in the wrapper's best-effort mode),
  - unsupported explicit `lang` in internal/native calls,
  - bad glob compilation or unreadable search roots,
  - overlapping computed edits (`Overlapping replacements detected; refine pattern to avoid ambiguous edits`),
  - out-of-bounds edit ranges or non-UTF-8 replacement text,
  - write failures during apply,
  - cancellation or timeout.
- With `failOnParseError: false` (the wrapper always uses this), pattern compile failures and file parse failures become `parseErrors` instead of aborting the whole run.
- If every rewrite pattern fails to compile, native `ast_edit` returns a successful zero-replacement result with `parseErrors` populated.
- Files containing tree-sitter error nodes are skipped for rewriting; they do not get partial edits.
- Apply can fail after a successful preview if the preview becomes stale. The Rust gate compares the exact original bytes of every affected file; a same-count content change is stale and writes nothing. Counts are never used as the stale check.
- A partial write/cancel result is an error and requeues the action; the next resolve fails the byte gate instead of reapplying already-written files.

## Notes
- `ast_edit` does not expose the native `lang`, `strictness`, `selector`, `maxReplacements`, `failOnParseError`, or `timeoutMs` fields to the model. The runtime fixes the call shape to a preview-first, smart-strictness, best-effort parse mode.
- Mixed-language scopes are supported: the native layer infers each candidate's language and compiles each rule per discovered language. A pattern that parses for only some languages rewrites those files and reports parse issues for incompatible languages.
- Idempotency is not enforced syntactically. A rewrite like `foo($A) -> foo($A)` previews zero changes because output equals input; a rewrite that keeps matching its own output may still produce replacements on repeated calls.
- Rewrites are accumulated per file, then applied from the end of the file backward after an overlap check. Independent matches can coexist; overlapping matches abort the run.
- Native rewrite rule order is by pattern-string sort, not by the original `ops` array order, because `normalize_rewrite_map(...)` sorts the `(pattern, rewrite)` pairs.
- Preview/apply parity is validated by byte-exact comparison of every affected file before the write phase; the prepared outputs are then written without rerunning the rewrite. Replacement totals and per-file counts are reporting only.
- Display parity: the Rust preview implements the `resolveFileDisplayMode(...).hashLines` branch — pre-edit snapshot recording, `[PATH#TAG]` (single file) and tagged folded grouped headers, `-LINE:text` / `+LINE:text`, upstream's `formatGroupedFiles` directory tree, and `details.displayContent` code frames. The remaining difference is selection input: an explicit constructor mode here versus session `edit.mode`/model-variant settings upstream (a CLI/config resolver task).