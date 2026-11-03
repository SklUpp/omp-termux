# Port changes: oh-my-pi → Termux/Android (aarch64)

Catalog of the **currently maintained** deltas from upstream, why each
exists, and how the port is re-based onto a new upstream. It distinguishes
**current state** (merged and shipped) from **approved pending changes**
(decided, not yet applied). Upstream source is unmodified until the
deterministic overlay runs; everything Android-specific lives in `android/`.

## Current state vs. approved pending

| Area | Current (merged) | Approved pending (implementation plan) |
|------|------------------|----------------------------------------|
| JS packaging | single `cli.js` bundle | code splitting (`splitting:true`) + whole-tree packaging |
| Runtime | Termux apt `bun` | bundled official Bionic Bun in tarball |
| Version pins | inline, floating nightly | single `android/versions.env` |
| Installer | destructive `rm -rf` then `mv` replacement | guarded two-rename swap with rollback; `omp update` reinstall guard |

Pending items land via `../../specs/ANDROID_IMPLEMENTATION_PLAN.md` and
are **not** claimed applied here.

## Target facts (verified)

- `rustc --print cfg --target aarch64-linux-android` →
  `target_family="unix"`, `target_os="android"`, `unix`. **`target_os="linux"`
  is NOT set.** Any code gated on `target_os = "linux"` is dead on Android;
  any `not(target_os = "linux")` branch fires on Android.
- Termux's own Rust reports `host: aarch64-linux-android`, so on-device
  builds are native (non-cross); only CI cross-compiles with the NDK.
- Bun on Android reports `process.platform === "android"`, `process.arch ===
  "arm64"` → loader tag `android-arm64`, addon `pi_natives.android-arm64.node`.
- bionic provides `forkpty`/`openpty` since API 23 → `portable-pty` works.
- `pidfd_open`/`pidfd_send_signal`: API 34 capability floor; on API 31–33
  seccomp may SIGSYS-kill the syscall. **Only one API 34-class device tested;
  no broad compatibility claim.** Binary floor is API 24.
- bionic has no X11/Wayland/AppKit/Win32 surfaces for native code →
  `arboard` (clipboard) has no backend on Android (text-only fallback).
- The `pi-voice` + webrtc/opus dependency graph OOM-kills free GitHub
  runners (~15 GB RAM + 24 GB swap still killed) → pi-voice gated off
  Android; audio/WebRTC/desktop/ONNX surfaces are unsupported.

## Delivery

There is **one** delivery mechanism: the sequential raw Git patch queue.

`android/scripts/apply-patches.sh` validates `android/patches/MANIFEST`, checks each
patch with `git apply --check`, then applies patches in manifest order. It also
accepts an already-patched tree without reapplying patches.

The queue is regenerated from clean upstream-derived source when upstream changes.
The sync workflow applies it in CI; release workflows apply it idempotently after
checking out immutable release tag.

Patch `01` also carries two formatter-only hunks (`test/interactive-click-focus.test.ts`
and `packages/tui/src/tui.ts`) so the upstream formatter check stays green without
a separate patch that would fight the queue on every regen.

Current port target: upstream oh-my-pi **18.16.0** (`544cacab86b0`). Upstream
requires Bun **>= 1.4.4**; `android/versions.env` pins official Bionic Bun
1.4.4 (`bun-linux-aarch64-android.zip`, SHA-256 verified at release).

## Transformation catalog (current)

The Android overlay transforms (patch `01`), one concern each:

### 1. Cargo.toml — gate arboard + pi-voice off Android

`arboard` has no bionic backend. `pi-voice` drags in the webrtc/opus graph
that OOM-kills CI runners. Both move from always-on `[dependencies]` to
`[target.'cfg(not(target_os = "android"))'.dependencies]`.

### 2. lib.rs — drop `feature(alloc_error_hook)`

Nightly-only feature; Termux ships stable Rust. Keeping it off lets the
crate compile on stable. Runtime alloc diagnostics are sacrificed (see 3).

### 3. crash_handler.rs — disable alloc hook

The `std::alloc::set_alloc_error_hook` registration block is replaced by a
comment; alloc-report helpers stay and are exercised by unit tests but
marked `#[allow(dead_code)]`, and unused statics/imports are removed.
Panic diagnostics are unaffected.

### 4. clipboard.rs — Android cfg gates + stubs

Because `target_os = "linux"` is false on Android, the existing
`not(target_os = "linux")` arm would otherwise select the arboard path.
The Linux arm is tightened to
`#[cfg(all(target_os = "linux", not(target_os = "android")))]`; macOS/Windows
arms to `#[cfg(all(not(target_os = "linux"), not(target_os = "android")))]`.
Android `set_clipboard_text` stub returns an error directing JS to
`termux-clipboard-set`; `read_image_from_clipboard` returns `Ok(None)`.
`#[napi]` export names stay identical across platforms.

### 5. process.rs (pi-shell) — enable platform module on Android

The Linux `mod platform` gate is widened to
`#[cfg(any(target_os = "linux", target_os = "android"))]`. macOS/Windows
blocks untouched.

### 6. pi-builtins — proc_snapshot.rs + ps.rs

Same widening: `proc_snapshot` module and its enclosing cfg block gain
`target_os = "android"`; `ps_total_memory_bytes()` gate widened to
`any(target_os = "linux", target_os = "android")`. Keeps process snapshotting
and memory reporting native.

### 7. loader-state.js — register android-arm64

`SUPPORTED_PLATFORMS` gains `"android-arm64"` so the loader accepts the
addon at startup and maps the tag to `pi_natives.android-arm64.node`.

### 8. pi-vfs — bionic `faccessat` flags

Bionic rejects any nonzero `faccessat` flag with `EINVAL`, and the Rust `libc`
Android module omits `AT_EACCESS`. `may_access` therefore passes `flags = 0`
(real IDs) only when `uid == euid` and `gid == egid`; on mismatch it returns
`false` conservatively, so FIFO cancellation skips its best-effort release
wake and can leave the helper blocked until the other end connects. Non-Android
still uses `AT_EACCESS` unchanged.

### 9. pi-builtins — bionic sync calls

Bionic exports `mkfifoat` since API 23, but the Rust `libc` Android module omits
the binding; the patch declares the bionic symbol locally (API 24+ target),
keeping the dirfd-relative call, `EINTR` retry and errno unchanged. Bionic also
declares `getnameinfo` host length as `size_t`, so Android passes `buf.len()` as
`usize`; other Unix targets keep `socklen_t`. Address, length and behavior are
unchanged.

## Fork feature: task isolation workspace lifecycle (patch `02`)

Not Android-gated — this is a port-carried feature that rides in the same
queue (`02-isolated-revive.patch`) so releases from this fork keep it.

Task-isolation workspaces now outlive a run: spawn writes
`.omp-isolation-state.json` beside the owner marker recording the id, parent
repo root, mount dir, backend and capture baseline. The workspace is destroyed
only by an explicit release (`AgentLifecycleManager.release`, e.g. cancelling
the agent) — run end, park, shutdown and kill retain it, including
`keepAlive:false` one-shot runs. `omp worktree clear`, with or without `--all`,
never destroys a sandbox that still has this descriptor: it only reclaims
marker-only leftovers from a crashed setup, because a descriptor means the
workspace may hold changes (or in-workspace commits) that only release can
capture. Retention moves the sandbox to a `.retained-*` sibling and the
transcript pointer (`<id>.isolation.json`) follows the move.

Cold revival resolves the descriptor (header cwd, then pointer), replays the
same transcript in the retained workspace cwd, and owns a release that
captures the delta against the persisted baseline, commits only when it differs
from the run-end handoff, then runs native teardown on the recorded backend.
The revive claims the workspace by rewriting the owner marker (so `clear`
cannot reclaim it while the revived session writes) and refuses a sandbox whose
marker names another live omp process. Claims and clear teardown share one
OS-backed lease (pi-utils file lock) keyed by the immutable slot in a directory
OUTSIDE the sandbox, so a retention rename never moves the inode a holder
locked, the original and `.retained-*` sibling share the same key, and a crashed
holder releases the lease with its process. Spawn setup takes the same lease
BEFORE its retained-workspace check, destructive slot replacement and fresh
owner-marker publication, so `clear` classified from a dead leftover cannot
delete the slot a spawn just rebuilt, and a simultaneous same-id spawn is
refused instead of racing the rebuild; a live marker-only owner is likewise
refused and preserved. Lock presence is never probed (the native probe reports
unsupported on some platforms): `clear` attempts the real lease and preserves
the sandbox on contention, so a stale leftover never hides it forever. When the retention-time pointer write fails or the process crashes
before it, revival recovers the renamed sandbox by scanning for a descriptor
bound to this transcript and refuses when two copies match instead of guessing. The descriptor is bound to its
transcript and derived slot (plus canonical worktrees root, source repo and
mount-dir containment), and release takes that expected transcript from the
caller rather than from the descriptor, so a copied pointer/descriptor cannot
authorize another session's capture or deletion. Release keeps the workspace
when capture OR the branch commit fails — the patch artifact alone does not
preserve commits made inside the workspace — and refuses when this process does
not own the sandbox. Revival renders its prompt at the SAME workspace it runs in:
the warm path overrides the spawn-time worktree input and the cold path rebinds
the persisted prompt blocks from the pre-rename path to the retained one.
An aborted isolated run is resumable like any other, because its retained
workspace revives through the persisted factory.

A retained sandbox whose agent is not running has a public explicit release
that never loads its session: `omp worktree release <dir>` resolves and binds
the descriptor, proves the transcript independently (its session header cwd is
this workspace's slot mount and the retention pointer, when present, names the
same sandbox mount; a substituted transcript is refused before any claim or artifact
write), claims the immutable slot for the issuing process, then runs the same
release (capture against the persisted baseline, task-branch commit, native
teardown). It refuses a live foreign owner, prints the artifact
directory, and leaves the sandbox intact when capture or the commit fails, so a
cold workspace can be finalized or discarded deliberately without relying on
`kill` (which retains).

`repoRoot` (added to the task schemas by patch `01`, preserved by every branch
of the dynamic `task.file` schema in patch `02`) selects a different repository
to isolate; `isolated` alone always uses the current cwd's repository, and
`repoRoot` alone enables nothing.
