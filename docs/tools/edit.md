# edit

> Applies source edits. The default `hashline` mode consumes one line-anchored patch string and edits existing files directly.

## Source
- Entry and mode registration: `packages/coding-agent/src/edit/index.ts`
- Hashline schema: `packages/coding-agent/src/edit/hashline/params.ts`
- Model-facing hashline prompt: `packages/hashline/src/prompt.md`
- Canonical constrained-decoding grammar: `packages/hashline/src/grammar.lark`
- Parser and application: `packages/hashline/src/input.ts`, `packages/hashline/src/parser.ts`, `packages/hashline/src/apply.ts`
- Snapshot validation/recovery: `packages/hashline/src/snapshots.ts`, `packages/hashline/src/patcher.ts`, `packages/hashline/src/recovery.ts`
- Coding-agent execution/result shaping: `packages/coding-agent/src/edit/hashline/execute.ts`
- Streaming preview strategy: `packages/coding-agent/src/edit/streaming.ts`, `packages/coding-agent/src/edit/hashline/diff.ts`

## Mode selection and availability

`edit` is an essential built-in tool. `resolveEditMode()` selects the active wire contract in this order:

1. model-specific configured variant;
2. `PI_EDIT_VARIANT`;
3. `edit.mode`;
4. default `hashline`.

Supported modes are `hashline`, `apply_patch`, `patch`, and `replace`. Unless `PI_STRICT_EDIT_MODE` is set, a short model exclusion list can replace the default hashline contract with `replace`. This page documents the default hashline contract; the tool's schema, prompt, examples, renderer, and optional custom Lark format all switch with the selected mode. In `apply_patch` custom-tool mode the wire name is `apply_patch`; dispatch still reaches the same internal tool.

### Native Rust CLI

The Rust CLI ships all five wire contracts: `hashline` (default), `apply_patch`, `patch`, `replace`, and `sloppy`. Selection is native and follows upstream precedence: `edit.modelVariants` (first case-insensitive substring match in insertion order), `PI_EDIT_VARIANT`, `edit.mode`, then `hashline`; unless `PI_STRICT_EDIT_MODE` is set, a model on the exclusion list (Kimi, MiMo, DeepSeek V4 Flash, Step 3.7 Flash) falls back to `sloppy`. Custom Lark grammar and streaming preview are not implemented.

- Mode resolution and file display live in `crates/pi-agent/src/tools/edit_variants/mode.rs` (`EditModeSettings` is the config seam; the resolver never reads env itself). Hashline display (`read` line numbers + hash anchors) is active only when the edit tool is mounted, the read is not `raw`, the source is mutable, and the mode is `hashline`; `readLineNumbers` defaults to `false`.
- The non-hashline engines live in `crates/pi-agent/src/tools/edit_variants/`: `replace.rs` (exact, ambiguity refusal, `replace_all`, line-window fuzzy at `edit.fuzzyThreshold`), `patch.rs` (unified-diff hunks with trim/comment/prefix/substring/fuzzy sequence passes and character fallback), `apply_patch.rs` (Codex `*** Begin Patch` envelope), and `sloppy.rs` (`§`/`«` operations, `⟪old│new⟫`, `…` gaps, `＋` add lines). `apply_patch` keeps the wire name `apply_patch` via `contract(mode).wire_name`; `contract(mode).description` is a `&'static str` and the schema is built lazily by `contract(mode).schema()` only when the parent registers the tool. Registry lookup, approval tier, and child-tool selection stay parent-owned.
- Hashline remains the single native engine in `crates/pi-agent/src/tools/hashline/`; the parent `EditTool` dispatches `hashline` there and every other mode through `edit_variants::execute`, so no hashline logic is duplicated. Parser, clipboard registers, block anchors, bounded snapshot store, tag validation, and seen-line rejection are native and unchanged.
- Snapshot state is per agent object, not process-global and not persisted: one injected `Arc<SnapshotStore>` is shared with the `read` sink, and named `CUT` registers live on that object's `EditTool`. The CLI's `PiAgentRunner` keeps that pair for the whole process, so REPL turns — including `/new`, resume, and fork — reuse it, matching the TypeScript `AgentSession` lifetime; a fresh process starts empty. Nothing is keyed by a session id or written to the session log. Tags are content-derived, so the store scopes read provenance (which lines were displayed) and register contents; a child agent gets its own store and tool instances instead of a clone of the parent's.
- Accepted hashline input normalization matches upstream: bare `+`-less body rows (warned; one uniform read-prefix strip), ignored read-output elision rows, byte-identical duplicate hunks (last wins, warned), and the upstream `-` row policy (Markdown bullets kept, unified-diff `-old` rows paired with `+new` dropped, anything else rejected). `@@` headers and apply-patch sentinels are rejected with a pointer to the canonical form.
- A stale hashline tag first tries snapshot-chain recovery: anchors are mapped through unchanged lines, must all move by one offset, and the edit is replayed on live content with a recovery warning. Only when no safe remap exists is the edit rejected, with the current `#TAG` and numbered context around the anchored lines. Patches that only insert at head (`<1:`) or tail (`>$:`) apply on live content with a drift warning instead.
- Seen-line enforcement is opt-in: `EditToolOptions::enforce_seen_lines` defaults to `false`, matching the upstream `edit.enforceSeenLines` schema default. When enabled, edits anchored on lines the read never displayed are rejected with the reveal/retry diagnostic; when disabled (default), they apply.
- A byte-identical no-op is a tool failure; three consecutive identical payloads on one path escalate to a hard-stop diagnostic. State is per `edit` tool instance (one runner process).
- An edit that stops the file parsing while the pre-edit text parsed is still applied, and the result carries a syntax warning (upstream behavior; the probe reuses `pi-ast`'s tree-sitter cache).
- Expansion is bounded before anything is materialized: a section may declare at most 100,000 expanded lines/body rows, a clipboard paste at most 100,000 lines, and one call at most 200,000 resolved edits / 24 MiB of prepared text across its sections. Register reads, register captures, paste expansions, splice payloads, prepared text, and each per-section batch snapshot are charged against that budget *before* they are cloned or allocated, so a large register pasted many times fails early instead of materializing gigabytes. Line accounting counts `String` slots and names as well as content, so captures full of blank lines and many tiny named registers are not free. Named registers persisted for the session are capped at 16 MiB (a file update still lands; the…
- `MV` destinations participate in batch conflict preflight: a destination may not alias any source or another destination in the same call (symlink aliases included), so a later section cannot overwrite moved content.
- Each hashline commit re-verifies the target's canonical path, the exact bytes read during validation (never the 16-bit tag, which only labels diagnostics), and the file identity (`dev`/`ino` where the platform exposes it); bytes replaced, a file deleted or swapped, or a symlink retargeted after preparation abort with a "changed after the batch was prepared" error instead of being overwritten. `REM` re-checks before unlinking; moves re-check source bytes/identity before unlinking the source after the destination write. The re-check and the rename are separate syscalls, so an adversarial swap inside that window is mitigated, not atomically excluded.
- Non-hashline commits are atomic per file: the engine re-reads and compares the exact bytes it planned against, then publishes through the shared temp-file+rename helper (symlink-safe, permissions preserved). `patch`/`apply_patch` create refuses an existing file in `apply_patch` mode and overwrites only in `patch` mode; `Move to:`/`rename` refuses an existing destination; delete and move run only for an explicit `delete` entry or `rename`/`Move to:` command. `apply_patch` hunks apply in order and are not atomic across files (upstream semantics); each file's write is atomic.
- `replace`, `patch`, and `sloppy` preserve the file's BOM and dominant line ending; `apply_patch` Add content is written with LF endings and a trailing newline. `replace` and `sloppy` refuse a non-unique match; `sloppy` applies `«*` operations to every match and otherwise refuses with numbered previews. Sloppy parse recovery ports envelope stripping (`*** Begin/End Patch`, foreign sentinels, code fences), missing-`»` split inference, alternating-`«`/`»` separator pairs, bracket-style `«old»«new»` pairs, bare-desired and stray-`⟫`-divider repair, directive rewrites inside `»`, echoed-line expansion, bare-`⟪X⟫` desired text, diff-shaped bodies, and the closest-desired-block fallback under the upstream 0.35/0.1 score guards. Match retries port selection-echo dropping, echoed-line candidates, trailing-selection embedding, `⟫` seam gaps, gap-joined edit sites, and punctuation-run variants with the maximal-run guard. Not yet ported: overlap double-typed boundary trimming, non-consecutive inference, and the apply-side frame/alignment/full-line-expansion/overlap-reconciliation passes; those inputs fail closed with the "did not match" guidance.
- Internal/SSH URL paths (`artifact://`, `agent://`, `local://`, `vault://`, `mcp://`, `ssh://`, …) are refused with upstream's protocol-handler error. The TypeScript tool routes them through `InternalUrlRouter`/`resolvePlanPath`; the Rust seam is `paths::InternalUrlEditHost` (`resolve_write_target` → `read_target` → `write_target`) plus `paths::internal_url_scheme`, which the parent registry wires to existing mounts. No local-path fallback is invented.
- Each hashline commit lands one section at a time. A later failure or cancellation returns the sections already written and states that the remaining ones did not complete (a move may still have partially mutated disk, which the message names); a move whose destination write succeeded but whose source unlink or permission step failed is reported as a partial move. Named registers reflect landed sections only.
- Moves preserve the source file's permissions (e.g. `0600` stays `0600`, `0755` stays `0755`): the mode is applied to the unpublished temp file before the atomic rename, so the destination is never briefly world-readable; a move no longer chmods after publishing.

## Input

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `input` | `string` | Yes | One or more `[PATH#TAG]` sections containing hashline operations. The strict custom-tool grammar wraps the sections in `*** Begin Patch` / `*** End Patch`; the normal parser also accepts an unwrapped payload. |

Each section edits one existing file and MUST copy the four-uppercase-hex snapshot tag from the latest anchored `read`, `grep`, or successful `edit` result:

```text
[src/example.ts#1A2B]
PUT 4.=4:
+const value = 2;
```

Use `write` to create or wholly overwrite a file. Hashline rejects untagged anchored edits at application time.

## Canonical patch language

All line numbers refer to the original tagged snapshot, not to earlier hunks in the same call.

| Form | Effect |
| --- | --- |
| `PUT N.=M:` | Replace inclusive original lines `N..M` with the following `+TEXT` rows. |
| `PUT N*:` | Replace the multi-line syntactic block beginning on line `N`. |
| `PUT <N:` / `PUT >N:` | Insert body rows immediately before / after line `N`. `PUT <1:` is file head. |
| `PUT >$:` | Append body rows at file tail. |
| `PUT >N*:` | Insert after the syntactic block beginning on line `N`. |
| `CUT N.=M` / `CUT N*` | Delete and capture an inclusive range or resolved block. Add `@name` to write a named register. |
| `PUT <N` / `PUT >N` / `PUT >$` | Paste the anonymous register into a gap. |
| `PUT <N @name` / `PUT >N @name` / `PUT >$ @name` | Paste a named register into a gap. |
| `PUT N.=M @name` / `PUT N* @name` | Replace a range or block with a named register. Named registers are required for span/block paste. |
| `REM` | Delete the section file. |
| `MV DEST` | Move/rename the section file after any preceding edits in that section. Quote destinations containing spaces. |

Register names contain ASCII letters, digits, `_`, or `-`. The anonymous register is batch-local and starts empty on every call. Named registers persist for the session and are published only after their writes land. Operations run top-to-bottom across sections, so a cut in an earlier section can feed a later paste. Repeating a paste does not consume its register.

Only body-bearing `PUT ...:` headers take body rows. Every body row is `+TEXT`; `+` alone inserts a blank line. The body is final content, never a unified-diff before/after pair. Literal content beginning with `-` or `+` is written as `+-...` or `++...`. `CUT`, register-backed `PUT`, `REM`, and `MV` take no body.

### Block anchors

Block forms resolve from the opening line through the tree-sitter node's end. Anchor the construct opener, never a closing delimiter, last visible line, blank line, or inner statement. A single-line node is rejected with guidance to use the corresponding explicit-line operation. `PUT >N*:` lowers to ordinary `PUT >N:` with a warning when no block resolves; replace/cut block forms fail instead of guessing.

Leading decorators, attributes, and doc-comments may be separate syntax nodes. Anchor the first decorator when the parser groups it with the declaration; otherwise use an explicit range. Standalone line comments are not swept automatically. In Markdown, a heading's block includes its body and deeper subsections through the next heading of equal or higher level.

Use tight ranges and separate non-adjacent changes. Do not use `edit` merely to reformat or restyle code; run the project's formatter after the substantive edit.

## Examples

Given:

```text
[greet.py#A1B2]
1:@cache
2:def greet(name):
3:    print("Hello, " + name)
4:
5:greet("world")
```

Replace the decorated function without touching its caller:

```text
*** Begin Patch
[greet.py#A1B2]
PUT 1*:
+@cache
+def greet(name):
+    print(f"Hi, {name}")
*** End Patch
```

Move it to another previously-read file using a named register:

```text
*** Begin Patch
[greet.py#A1B2]
CUT 1* @fn
[lib/greet.py#3C4D]
PUT <1 @fn
*** End Patch
```

Rename after editing:

```text
*** Begin Patch
[greet.py#A1B2]
PUT 5.=5:
+greet("team")
MV lib/welcome.py
*** End Patch
```

## Output and side effects

Hashline applies in one tool call; it does not use the staged `xd://resolve` / `xd://reject` flow used by `ast_edit`.

A successful section returns a fresh `[path#TAG]` header, optional block-resolution and move lines, a compact post-edit preview when available, and a `Warnings:` block when recovery or normalization produced warnings. `EditToolDetails` can include the unified `diff`, `firstChangedLine`, diagnostics/format results, operation (`update` or `delete` in hashline mode), path/move metadata, snapshots, and per-file results. Multi-section input returns one aggregate result.

The streaming renderer parses complete portions of an in-flight payload and computes read-only diffs. Streaming preview skips transient unresolved blocks, stale tags, and empty pastes rather than presenting partial input as a final failure. Execution re-reads and validates normally.

For multi-section calls, every section is parsed and prepared before writes begin so syntax, anchor, and no-op failures fail fast. Files then write in order; an operating-system write failure can leave the already-landed prefix applied. Named-register session state is advanced only for that landed prefix.

## Limits and validation

- Snapshot tags are four uppercase hexadecimal characters derived from normalized file content and recorded in the session snapshot store.
- `read`/`grep` exposure matters: edits targeting lines outside the recorded visible ranges are rejected. Re-read elided or undisplayed ranges before editing them.
- Ranges are inclusive, must be ordered, and are bounded by a parser amplification limit of 100,000 expanded lines before the target file's actual bounds are checked.
- Overlapping edits or multiple operations targeting the same original anchor are rejected.
- Same-path sections are merged so their original line anchors apply together. Clipboard operations are rejected if interleaved same-path sections would make authored register order ambiguous.
- Stale tags first attempt snapshot-chain recovery (unchanged-line remap replayed on live content, or head/tail-only insert drift); only an unrecoverable audit falls back to a mismatch that reports the current `#TAG` and numbered context around the anchored lines.
- A byte-identical edit is an error. Repeating the same no-op payload three times escalates through the no-op loop guard (per session, keyed by canonical path and a hash of the raw patch input).

## Common failures

- Missing/malformed `[PATH#TAG]`, unknown snapshot tag, or a path that no longer exists.
- Anchor outside the file, outside the recorded seen-line ranges, in an elided region, or based on a stale snapshot that cannot be recovered safely.
- Reversed or overlapping ranges.
- Empty body for a body-backed `PUT`, body rows under a bodyless operation, unknown named register, or anonymous paste before an unambiguous anonymous cut.
- Block anchor on an unsupported/invalid syntax tree, blank/closing line, or single-line node.
- Unified-diff contamination (`@@`, apply-patch sentinels, `-old` rows) instead of hashline operations and final-content `+` rows.
- `REM` / `MV` conflicts, invalid move destinations, target collisions, or filesystem write failures.
- A patch that parses and applies to exactly the existing bytes (no change).

The parser has limited recovery for common model slips (optional envelope, benign header noise, some bare rows and range spellings), and surfaces warnings when it repairs input. Callers SHOULD emit only the canonical grammar above; recovery behavior is not a second public syntax.
