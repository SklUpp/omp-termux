# Port changes: oh-my-pi → Termux/Android (aarch64)

This document describes the Android changes that the patch queue maintains.
The queue applies these changes to a clean upstream checkout.

## Current delivery

The release uses split JavaScript chunks and bundles Android Bun.
`android/versions.env` pins the build versions. The installer stages the
runtime and tries to roll back after a failed swap. On Android, `omp update`
gives reinstall instructions.

## Target facts (verified)

- `rustc --print cfg --target aarch64-linux-android` →
  `target_family="unix"`, `target_os="android"`, `unix`. It does **not** set
  `target_os="linux"`. Code gated on `target_os = "linux"` is dead on Android.
  Code in a `not(target_os = "linux")` branch runs on Android.
- Termux Rust reports `host: aarch64-linux-android`. On-device builds are
  native. Only CI cross-compiles with the NDK.
- Bun on Android reports `process.platform === "android"` and
  `process.arch === "arm64"`. The loader tag is `android-arm64` and the addon
  is `pi_natives.android-arm64.node`.
- bionic provides `forkpty` and `openpty` since API 23, so `portable-pty`
  works.
- `pidfd_open` and `pidfd_send_signal` need API 34. On API 31–33, seccomp can
  kill the syscall with SIGSYS. Only one API 34-class device was tested. There
  is no broad compatibility claim. The binary floor is API 24.
- bionic has no X11, Wayland, AppKit or Win32 surface for native code. Thus
  `arboard` (clipboard) has no Android backend and uses a text-only fallback.
- The `pi-voice` dependency graph (webrtc/opus) OOM-kills free GitHub runners.
  A runner with 15 GB RAM and 24 GB swap still dies. Thus pi-voice is off on
  Android. Audio, WebRTC, desktop and ONNX surfaces are unsupported.

## Delivery

There is **one** delivery mechanism: the sequential raw Git patch queue.

`android/scripts/apply-patches.sh` selects `OMP_PATCH_PROFILE=standard` or
`extended` (default `extended`). It validates the matching manifest
(`MANIFEST.standard` = 01 only, `MANIFEST` = 01+02). It checks each patch with
`git apply --check`, then applies patches in manifest order. It also accepts a
tree that already has the patches.

When upstream changes, the queue is regenerated from clean upstream-derived
source. The sync workflow applies it in CI. Release workflows apply it again,
idempotently, after they check out the immutable release tag.

Patch `01` also carries two formatter-only hunks
(`test/interactive-click-focus.test.ts` and `packages/tui/src/tui.ts`). This
keeps the upstream formatter check green. A separate patch fights the queue on
every regen.

Current port target: upstream oh-my-pi **18.16.0**, commit
`83de60ad992d12e572ec3ed6fe2dd9e160cdc4a2`. `android/versions.env` pins
Android Bun 1.4.4. The release workflow verifies its SHA-256.

## Transformation catalog (current)

The Android overlay (patch `01`) transforms these files.

### 1. Cargo.toml — gate arboard + pi-voice off Android

`arboard` has no bionic backend. `pi-voice` pulls in the webrtc/opus graph that
OOM-kills CI runners. Both move from always-on `[dependencies]` to
`[target.'cfg(not(target_os = "android"))'.dependencies]`.

### 2. lib.rs — drop `feature(alloc_error_hook)`

The Android build does not use the allocation error hook. The patch drops
`feature(alloc_error_hook)`. The CI toolchain stays pinned in
`android/versions.env`.

### 3. crash_handler.rs — disable alloc hook

The patch replaces the `std::alloc::set_alloc_error_hook` registration block
with a comment. The alloc-report helpers stay and unit tests exercise them.
They get `#[allow(dead_code)]`. The patch removes unused statics and imports.
Panic diagnostics do not change.

### 4. clipboard.rs — Android cfg gates + stubs

`target_os = "linux"` is false on Android. Without a fix, the existing
`not(target_os = "linux")` arm selects the arboard path. The patch
tightens the Linux arm to
`#[cfg(all(target_os = "linux", not(target_os = "android")))]`. It tightens the
macOS/Windows arms to
`#[cfg(all(not(target_os = "linux"), not(target_os = "android")))]`.

The Android `set_clipboard_text` stub returns an error. The error tells JS to
use `termux-clipboard-set`. `read_image_from_clipboard` returns `Err(...)`.
The JS shim short-circuits to `null` on Termux before the addon. `#[napi]`
export names stay identical on all platforms.

On the JS side, `copyToClipboard` no longer swallows a total failure. If a
channel emits or accepts the text, `copyToClipboard` resolves. Otherwise it
rejects. `writeTerminalSequence` now reports whether it emitted OSC 52. A Bun
worker cannot reach the main thread terminal. A closed stdout is swallowed.
Neither counts. Emission stays best-effort: the external terminal can ignore
OSC 52. "Resolved" means "handed to a channel", not "paste will work".

The interactive copy paths and `/copy` report the rejection. They do not print
"Copied ...". `/copy` also clears the draft in `finally`. A missing
`termux-api` is now visible instead of a silent no-op: spawn `ENOENT`, native
stub `Err`, or no TTY or no emission for OSC 52. The Termux read path uses
`termux-clipboard-get`. It still degrades to an empty string with a warning.
The paste UI treats an unreadable clipboard as empty, not as an error.

`packages/natives/native/index.js` has Android-only unsupported proxies:
`copyToClipboard`, `readImageFromClipboard`, the audio classes and
`LiveWebRtcPeer`. The patch gates them on `process.platform === "android"`. On
other platforms they fall back to the upstream expressions, including
`missingNativeExport`. A stale desktop addon thus keeps its rebuild diagnostic.
It does not get a misleading "unsupported on Android/Termux" error.
`packages/natives/scripts/gen-enums.ts` emits the same gate. `bun run build`
regenerates `index.js` from it, so generator drift silently undoes the fix.
The stub target is a normal `function`, not an arrow. Thus `new AudioCapture()`
reaches the trap and throws the Android message. It does not throw a generic
"not a constructor" error.

### 5. process.rs (pi-shell) — enable platform module on Android

The patch widens the Linux `mod platform` gate to
`#[cfg(any(target_os = "linux", target_os = "android"))]`. The macOS/Windows
blocks do not change.

### 6. pi-builtins — proc_snapshot.rs + ps.rs

The patch uses the same widening. The `proc_snapshot` module and its enclosing
cfg block gain `target_os = "android"`. The `ps_total_memory_bytes()` gate
becomes `any(target_os = "linux", target_os = "android")`. Process snapshots
and memory reports stay native.

The patch does not widen process **waiting**. `proc_match::wait_for_watchable_exits`
compiles the pidfd arm only for `target_os = "linux"`. Thus `pidwait` and
`pgrep --wait` poll every 50 ms on Android. `pidfd_open` needs API 34, and
seccomp can SIGSYS-kill it on API 31–33. The arm needs a runtime API-level
guard. Without it, the process dies on those devices. The polling fallback is
correct but less efficient.

### 6.1 pi-builtins — sort memory hint (Android uses the same procfs)

The patch widens these gates to `any(target_os = "linux", target_os = "android")`:
`available_memory_hint`, `system_available_memory`, `meminfo_available_bytes`,
`linux_cgroup_memory_headroom` and `cgroup_memory_headroom`. It also widens the
`MemoryFiles`, `CGROUP_V1`, `CGROUP_V2` and `headroom_along` helpers that the
cgroup path needs.

Android does **not** guarantee these Linux procfs/cgroup interfaces. On stock
Termux, `/proc/meminfo` and `/proc/self/cgroup` are readable.
`/sys/fs/cgroup` and `/dev/memcg` give `EACCES`/`ENOENT` for the app UID. Both
cgroup branches then return `None`. Every read is fallible
(`read_to_string(..).ok()?`).

When `/proc/meminfo` is readable, the size hint uses `MemAvailable`. When
`/proc/meminfo` is unreadable, it falls back to `physical_memory_bytes()`
(total RAM). Only a source that was actually read wins. No cgroup or
availability assumption is made. Before this patch, Android skipped procfs and
used total RAM for the size hint. Thus `sort` sized chunks from the whole
machine. An 8 GB phone risked an OOM/LMK kill under pressure. The cgroup part
stays best-effort and dormant on platforms that deny `/sys/fs/cgroup`.

### 6.2 Held-file / SQLite locks stay POSIX on Android (known risk)

`held_files` and the SQLite OFD-lock shim stay `target_os = "linux"`-only.
`installSqliteLockShim()` effectively returns `false` on Android. These are
unproven on Android:

- Bionic `F_OFD_*` behaviour.
- `OFDLCK` visibility in `/proc/locks`.
- The static-Bun SQLite table scan needed to patch `close()`.

Enabling them blind can silently corrupt a database. Until a device proof and
an Android test exist, the port keeps POSIX locks. The historical
lock-release-on-close hazard (upstream issue #15586) therefore still applies on
Android. Closing any descriptor for the same inode can release the live
connection's locks. Examples are `cat agent.db-shm` and a grep walk over
`~/.omp`. A peer can then checkpoint or unlink the WAL underneath it. Do not
enable OFD on Android without three things. First, a bionic `F_OFD_SETLK`
probe. Second, an `OFDLCK` check in `/proc/locks`. Third, a test that a peer's
WAL inode stays stable.

### 7. loader-state.js — register android-arm64

`SUPPORTED_PLATFORMS` gains `"android-arm64"`. The loader accepts the addon at
startup and maps the tag to `pi_natives.android-arm64.node`.

### 8. pi-vfs — bionic `faccessat` flags

Bionic rejects any nonzero `faccessat` flag with `EINVAL`. The Rust `libc`
Android module omits `AT_EACCESS`. When `uid == euid` and `gid == egid`,
`may_access` passes `flags = 0` (real IDs). On a mismatch it returns `false`
conservatively. FIFO cancellation then skips its best-effort release wake.
Until the other end connects, the helper can stay blocked. Non-Android still
uses `AT_EACCESS` unchanged.

### 9. pi-builtins — bionic sync calls

Bionic exports `mkfifoat` since API 23, but the Rust `libc` Android module
omits the binding. The patch declares the bionic symbol locally (API 24+
target). The dirfd-relative call, `EINTR` retry and errno stay unchanged.
Bionic declares the `getnameinfo` host length as `size_t`. Thus Android passes
`buf.len()` as `usize`. Other Unix targets keep `socklen_t`. Address, length
and behavior do not change.

## Fork feature: task isolation workspace lifecycle (patch `02`)

This feature is not Android-gated. The port carries it in the same queue
(`02-isolated-revive.patch`), so releases from this fork keep it.

Task-isolation workspaces now outlive a run. Spawn writes
`.omp-isolation-state.json` beside the owner marker. It records the id, parent
repo root, mount dir, backend and capture baseline. Only an explicit release
destroys the workspace (`AgentLifecycleManager.release`, for example
cancelling the agent). Run end, park, shutdown and kill retain it, including
`keepAlive:false` one-shot runs. `omp worktree clear`, with or without `--all`,
never destroys a sandbox that still has this descriptor. It only reclaims
marker-only leftovers from a crashed setup. A descriptor means the workspace
can hold changes or in-workspace commits. Only release can capture them.
Retention moves the sandbox to a `.retained-*` sibling. The transcript pointer
(`<id>.isolation.json`) follows the move.

Cold revival resolves the descriptor (header cwd, then pointer). It replays the
same transcript in the retained workspace cwd. It owns a release that captures
the delta against the persisted baseline. When the result differs from the
run-end handoff, the release commits. Then it runs native teardown on the
recorded backend. The revive claims the workspace by rewriting the owner
marker, so `clear` cannot reclaim it while the revived session writes. It
refuses a sandbox whose marker names another live omp process.

Claims and clear teardown share one OS-backed lease (pi-utils file lock). The
lease is keyed by the immutable slot in a directory OUTSIDE the sandbox. Thus a
retention rename never moves the inode a holder locked. The original and
`.retained-*` sibling share the same key. A crashed holder releases the lease
with its process. Spawn setup takes the same lease BEFORE three steps: its
retained-workspace check, destructive slot replacement, and fresh owner-marker
publication. Thus `clear` classified from a dead leftover cannot delete the
slot a spawn just rebuilt. A simultaneous same-id spawn is refused instead of
racing the rebuild. A live marker-only owner is likewise refused and preserved.
The code never probes lock presence, because the native probe reports
unsupported on some platforms. `clear` attempts the real lease and preserves
the sandbox on contention. A stale leftover thus never hides it forever.

Suppose the retention-time pointer write fails, or the process crashes before
it. Revival then recovers the renamed sandbox by scanning for a descriptor
bound to this transcript. When two copies match, the scan refuses instead of
guessing.

The descriptor is bound to its transcript and derived slot. The binding also
covers the canonical worktrees root, source repo and mount-dir containment.
Release takes the expected transcript from the caller, not from the descriptor.
Thus a copied pointer or descriptor cannot authorize another session's capture
or deletion. When this process does not own the sandbox, release refuses.
When capture OR the branch commit fails, release keeps the workspace. The patch
artifact alone does not preserve commits made inside the workspace. Revival
renders its prompt at the SAME workspace it runs in. The warm path overrides
the spawn-time worktree input. The cold path rebinds the persisted prompt
blocks from the pre-rename path to the retained one. An aborted isolated run is
resumable like any other, because its retained workspace revives through the
persisted factory.

A retained sandbox whose agent is not running has a public explicit release.
It never loads the session: `omp worktree release <dir>`. The command resolves
and binds the descriptor. It proves the transcript independently. The session
header cwd must be this workspace's slot mount. When present, the retention
pointer must name the same sandbox mount. A substituted transcript is refused
before any claim or artifact write. The command claims the immutable slot for
the issuing process. Then it runs the same release: capture against the
persisted baseline, task-branch commit, native teardown. It refuses a live
foreign owner. It prints the artifact directory. When capture or the commit
fails, it leaves the sandbox intact. Thus you can finalize or discard a cold
workspace deliberately, without `kill` (which retains).

`repoRoot` selects a different repository to isolate. Patch `01` adds it to the
task schemas. Patch `02` preserves it in every branch of the dynamic
`task.file` schema. `isolated` alone always uses the current cwd's repository.
`repoRoot` alone enables nothing.

## Fork feature: deferred integration (patch `02`)

`task.isolation.deferIntegration` (default `false`) is the explicit-consent
variant of `task.isolation.apply`. It is shown only while
`task.isolation.apply` is on. Both must be on, and the effective apply must be
true. That means task `isolated: true`, or eval
`agent(..., { isolated: true, apply: true })`. A successful isolated run then
captures its patch/nested artifacts. It marks the descriptor `deferred` and
returns without touching the parent checkout. The result and follow-up hint
name the signal:

```
write  path: "worktree://<id>"  content: "/integrate"
```

`worktree://<id>` is the isolation manager endpoint, not the child agent. The
write is a reserved coordination control, not chat and not a prompt. The
handoff never depends on the child running or accepting a message. The handoff
does these things:

- It binds the child transcript's session-header id, the spawning parent
  session file + header id, and the descriptor slot/repo/baseline. A legacy
  descriptor without that tuple is refused for integration (release still
  works). A refused signal never cleans it up.
- It holds a shared lifecycle reservation that blocks revive/park/release
  races. Until the handoff finishes, it keeps the workspace owner claim inside
  the immutable-slot lease.
- It captures the latest delta against the persisted baseline, including work
  done after a revive. It calls the existing merge path with the
  `task.isolation.merge` strategy recorded at spawn. `patch` applies the
  captured diff. `branch` commits the latest delta onto `omp/task/<id>` and
  merges that branch through the existing cherry-pick path. The merge engine
  itself does not change.
- It validates nested patches for unambiguous application before the handoff.
  On conflict it fails closed before mutation. Suppose the stock merge fails,
  or nested apply reports warnings after the root landed. The journal then
  records the partial state. The workspace and artifacts are kept. The error
  names exactly which parts can be present in the parent. The handoff never
  attempts a custom rollback.
- It writes an `applying` journal before the first parent mutation. Then it
  writes a `merged` journal plus a `<id>.integration.json` tombstone beside the
  child transcript. A retry after a crashed or partial attempt fails closed
  instead of replaying.
- It reads `<id>.integration.json` state as final. A completed handoff is a
  pure no-op and never removes a workspace (which can be revived and edited).
- It checks owner markers with the shared start-token liveness rule under the
  lease. A dead process is reclaimed and a live foreign owner is refused.
- It records `deferredReady` only from the run-end success path. A failed or
  aborted deferred run is refused, exactly like the automatic apply path.
- Its write policy is `workspace`-scoped. Thus `worktree://` cannot bypass the
  plan-mode write guard.

## Legacy retained workspace recovery

Cold revive and automatic/lifecycle release refuse descriptors written before
`childSessionId` existed. There is one explicit recovery path:
`omp worktree release <sandbox>`. It re-verifies three things before
claim/capture: the transcript session header cwd, the retention pointer, and
the slot/repo/baseline binding. When `childSessionId` is absent, the pointer is
mandatory. An existing pointer must be an object with a non-empty `workspace`
string that canonically resolves to the selected mount. The command refuses
`null`, `{}`, strings, and malformed or mismatched pointers before
claim/capture. It never trusts the descriptor's id/path match alone.

1. Locate the sandbox and read `<sandbox>/.omp-isolation-state.json`. Note
   `sessionFile` and `repoRoot`.
2. Check the first transcript line: `head -1 <sessionFile>`. Its `cwd` must
   point at `<sandbox>/m` (or the retained mount). A child killed before its
   header completed can lack `id`, so `id` is not proof.
3. Copy everything out first:
   `cp -a <sandbox> <safe-dir>/workspace-copy` and
   `cp -a <artifactsDir>/<id>.jsonl <artifactsDir>/<id>.patch <safe-dir>/`.
4. Run the explicit release and review the reported capture:
   `omp worktree release <sandbox>`.
5. If release refuses, the independent checks did not pass. Keep the sandbox
   and artifacts and recover manually. Do not delete.
6. After you verify the copy, you can move it aside:
   `mv <sandbox> <sandbox>.legacy-retained`. Deletion is your decision, never
   automatic.

## Generation binding

Every isolated run records its child session-header id twice. First, as soon as
the child transcript header exists, before the child executes. Second, again in
the run-end descriptor patch. An abrupt crash before run end thus still leaves
a cold-revivable descriptor. `bindIsolationState` (the shared revive/release
binding) refuses a descriptor whose generation is missing or does not match the
current transcript header. Thus reusing an agent id and `<id>.jsonl` path
cannot revive/release/overwrite a workspace from an earlier generation. Older
descriptors without the token fail safe and use only the explicit CLI recovery
path.

`apply=false` keeps its existing contract: capture artifacts, never apply.
Turning the setting off restores automatic apply unchanged. Deferral also
requires the global `task.isolation.apply=true`. A per-call `apply=true` does
not activate a hidden hold while that setting is off. These descriptor fields
are optional only for reading old files: `deferred`, `deferredReady`,
`mergeMode`, `childSessionId`, `parentSessionFile`, `parentSessionId`. A
descriptor missing the session tuple is refused for revive/release and
integration. It never falls back to id-only authorization. The handoff never
takes its repository from the message. It always uses the descriptor's parent
`repoRoot`, so a signal cannot redirect the merge.
