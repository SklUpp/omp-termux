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

## Subagent isolation repository selection

`task(isolated: true, repoRoot: "/path/to/repo")` supports a session cwd outside Git. The active `01-termux.patch` passes explicit `repoRoot` to the existing isolation backend. Static task schemas retained it, but dynamic schemas (used with effort, seance, and other options) deleted it before spawn. All four dynamic single/batch, isolation-on/off schemas now retain `repoRoot`; only `isolated: true` requests isolation. No `task.isolation.repoRoot` setting or alternate backend is added.

Regression: `packages/coding-agent/test/repo-root-isolation.test.ts` parses real dynamic schemas for flat and batch requests and checks that non-isolated requests do not gain `isolated`. It runs schema imports in a child process so test-only module mocks cannot affect other suites. On a clean pinned upstream checkout, apply the active queue, run that test, then exercise an isolated worker from outside the Git fixture before calling the Android path verified; parser coverage alone does not prove native worker creation or cleanup.

SSH Termux socket fallback lives in the active queue as `02-ssh-socket-fix.patch` (MANIFEST order 01 → 02): `isFallbackRootAvailable` gates SSH ControlMaster on a writable fallback root, forces `ControlMaster=no` on read-only `/tmp` (Android/Termux), and keeps direct host-key checks; includes `ssh-control-path` and `sshfs-mount` regression tests. It is the queue port of upstream-side commit `ab2e5180e3` (`fix/termux-ssh-control`); the parallel port on `termux-18.12.6-sshfix` (`e5a9fd1bf0`) is identical except one blank line, so only one port is kept. Verified: both patches apply to clean pinned upstream in MANIFEST order.

Concurrent isolated workers lost edits at merge: every worker's delta patch is captured against the spawn-time baseline, so an earlier merge shifts the hunk line positions of a later patch and the patch-mode merge refused it ("Patches were not applied"). `03-isolation-merge-three-way.patch` (MANIFEST order 01 → 02 → 03) adds the designed three-way fallback in `mergeIsolatedChanges` (patch mode): exact apply first, then the native `threeWay` merge (fail-clean `Err(Conflict)` on genuine conflicts, no conflict markers), then the existing honest refusal with the patch artifact. Covered by `isolation-merge-three-way.test.ts` (drifted delta composes; conflicting delta refuses fail-clean), run in the device workflow alongside the repoRoot schema test. Source of truth for `task/isolation-runner.ts` is the patch queue (`01-termux.patch` owns that file), not the fork-branch copy.

Device smoke 2026-10-27 (Actions run 40484828276, branch `fix/subagent-isolation-termux`, bundle `omp/18.12.6`, `pi_natives` loaded with 146 exports): normal `omp` print launch from a cwd outside Git; the main agent called `task` batch shape with `isolated: true, repoRoot: <git fixture>`. `repoRoot` reached `tool_execution_start` args intact; the subagent ran in `OMP_WORKTREE_DIR/tc226ceceb/m/` — a copy of the requested root containing the fixture marker — read `marker.txt` verbatim, captured an empty patch (read-only, `task.isolation.apply=false`), and the workspace was cleaned afterwards. Fixture `git status` stayed clean, `~/.omp/wt` has no leftovers. Limitation: the `scout` child had no `bash` in its tool profile, so `pwd`/`git rev-parse` outputs are unverified; isolation is proven by the transcript workspace paths and file reads instead. Checks: schema unit test red→green (1 pass), active patch applies to clean pinned upstream in both directions, `sha256sum -c omp-termux.tar.gz.sha256` OK.

## Transformation catalog (current)

The eight overlay transforms, one concern each:

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
