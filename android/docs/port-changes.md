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
