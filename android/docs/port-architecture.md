# Port architecture: oh-my-pi on Android arm64 / Termux

Design companion to [port-changes.md](port-changes.md) (what changed, current
vs. pending) and [ci-cd.md](ci-cd.md) (how CI builds it). Everything
Android-specific lives under `android/`.

The deterministic overlay is the only thing that modifies the upstream tree —
see [port-changes.md](port-changes.md#delivery). Staged rollout is tracked in
[port-changes.md](port-changes.md#current-delivery).

## 1. The target

| Fact | Value |
|------|-------|
| Target triple | `aarch64-linux-android` |
| CPU | aarch64 (arm64) |
| libc | **bionic** (Android's libc), not glibc, not musl |
| Rust `target_os` | `"android"` |
| Runtime | official Bionic Bun, bundled in the release tarball |
| Native addon | N-API (napi-rs) shared object, `pi_natives.android-arm64.node` |
| Browser | **external** Termux:X11 `chromium` 149.0.7827.155, **not bundled** — `$PREFIX/lib/chromium/chrome` |

### The single most important rustc fact

For `--target aarch64-linux-android`, rustc sets
`target_os = "android"` **and does NOT set `target_os = "linux"`**:

```
$ rustc --print cfg --target aarch64-linux-android
target_arch="aarch64"
target_family="unix"
target_os="android"
unix
```

Every patch follows from these consequences:

- `#[cfg(target_os = "linux")]` code is **dead** on Android.
- `#[cfg(not(target_os = "linux"))]` branches **fire** on Android.
- Gate shared Linux/Android code with
  `#[cfg(any(target_os = "linux", target_os = "android"))]`.

### Builds are native on-device, cross in CI

Termux's own Rust reports `host: aarch64-linux-android`. An on-device build is
therefore a *native* build, with no NDK and no cross toolchain. Only GitHub
Actions cross-compiles (Linux x86_64 → aarch64-linux-android) with the NDK.

## 2. The runtime contract

### Bundled Bionic Bun, not apt/proot Bun

The release tarball is **self-contained and Bionic-only**. It bundles the
official `@oven/bun-linux-aarch64-android` Bionic binary inside `$LIB_DIR`.
That binary is a PIE with interpreter `/system/bin/linker64`. A thin shim
executes it directly:

```sh
exec env OMP_PLATFORM=android "$LIB_DIR/bun" "$LIB_DIR/cli.js" "$@"
```

The shim runs `$LIB_DIR/bun`, **never** `$PREFIX/bin/bun`. The installer no
longer installs the Termux `bun` apt package. A glibc-wrapper Bun with a
Bionic NDK `.node` is an unsupported ABI configuration, and it is prohibited.
`bun build --compile` is also prohibited: the built Android standalone binary
segfaulted on every attempt. The runtime therefore stays a packaged Bun plus
JS chunks.

Bun version pins live in a single `android/versions.env` (BUN, NDK,
ANDROID_API, RUST). Both workflows and the install/package scripts read that
file. A version change lands only together with the addon, and only after the
on-device smoke test.

### External Bionic Chromium, not bundled (browser tool)

The `browser` tool on Android uses an **external** Termux:X11 Chromium, not a
bundled binary. The OMP release tarball stays `Bun + JS + pi-natives` only.
Chromium is installed separately via `pkg` and validated by
`android/scripts/verify-browser.sh`. Release CI publishes `verify-browser.sh`
and `smoke-browser.sh` plus their metadata. CI never copies
`$PREFIX/lib/chromium/chrome` into the bundle (see
`android/scripts/verify-browser.sh --check-bundle`).

Pinned reference tuple (device-proven, see `android/docs/verification.md`):

```text
package:  chromium 149.0.7827.155 aarch64
source:   https://packages.termux.dev/apt/termux-x11 x11/main aarch64 Packages
deb:      chromium_149.0.7827.155_aarch64.deb
SHA256:   36500e23ad23bf2616bb4f215a297ba5a2b9e625992362b9b9c6bd05e0a27272
binary:   $PREFIX/lib/chromium/chrome
ELF:      ARM aarch64, Android API 24, Bionic, interpreter /system/bin/linker64
```

`headless_shell` (`$PREFIX/lib/chromium/headless_shell`) is **not** the
selected executable. It rejects remote debugging:
`Headless commands are not compatible with remote debugging`. The main `chrome`
binary supports `--headless=new` with loopback CDP and screenshots — verified
on device.

Executable resolution (headless local mode only) — the existing router stays
first: `app.path` / relay / `browser.cdpUrl` / `cmux` before headless. Only the
headless path resolves a local binary, in this order:

1. `browser.executablePath` (config, `~` expanded). An invalid value fails closed.
2. `PUPPETEER_EXECUTABLE_PATH` (env). An invalid value fails closed.
3. `$PREFIX/lib/chromium/chrome`, the canonical Termux:X11 binary.
4. `$PREFIX/bin/chromium`.
5. `$PREFIX/bin/chromium-browser`.
6. `$PREFIX/bin/chrome`.

One async validator checks every candidate:

- Absolute path, regular executable file.
- `--version` contains `Chromium`/`Chrome`/`Edge`, and on device the pinned
  `149.0.7827.155`.
- `file(1)` confirms an ARM64 shared-object/PIE. `readelf -l` shows the
  interpreter `/system/bin/linker64`. `readelf -d` shows `libc.so` (Bionic)
  and **no** `libc.so.6` nor `ld-linux-aarch64.so.1`.
- `.note.android.ident` API level `24`.

The repository NDK pin is `r27c` (`android/versions.env`).

An invalid configured path produces an explicit `ToolError`. It never falls
through silently to another binary. When no valid Android binary exists, the
tool throws an actionable install/configuration error that names the two-step
`pkg install -y x11-repo` / `pkg install -y chromium` sequence. Puppeteer
**never** downloads a managed Linux Chromium on Android. The `android`
platform short-circuits the download path — `verify-browser.sh` and
`test/tools/browser-android-chromium.test.ts` prove this. That test runs the
real probe fixture with a forced `android` platform.

The validated executable is threaded from `BrowserTool` → `acquireBrowser`
→ `launchHeadlessBrowser` / `resolveSharedBrowserLaunchSpec`. Daemon identity
includes the canonical resolved executable and the complete launch spec (see
§2.3). The existing webpage `tab.screenshot()` / `ImageContent` contracts are
preserved.

### Measured split packaging

The JS package is built with `Bun.build`, and **`splitting: true`** is
required. Packaging covers the whole tree. The build copies the entire
`outDir`. The shebang sits only on `cli.js`, and there is no manifest. CI tars
all chunks and assets.

| Metric | Monolith | Split | Delta |
|--------|----------|-------|-------|
| Entry `cli.js` | — | 22,608 B / 322 files | — |
| `omp --version` median | 640 ms | 134 ms | ~4.8x |
| RSS | 129–144 MB | 54.9 MB | ~2x |

Targets: startup `--version` median ≤ 200 ms, HWM ≤ 65 MB.

### Runtime resources

`models.json`, protobuf data, `CHANGELOG`, and `templates/` plus tool views are
required at runtime. Splitting relocates them to lazy chunks or payloads and
never drops them. OpenTelemetry must stay bundled into lazy chunks. A stale
local `node_modules` once caused the experimental-subcommand failure, not
splitting.

### Browser launch and daemon identity

The preserved flags plus the one device-proven Android supplement:

```text
--headless=new
--no-sandbox
--disable-dev-shm-usage
--remote-debugging-address=127.0.0.1
--remote-debugging-port=0        # ephemeral, loopback-only; never 0.0.0.0
--user-data-dir=$PREFIX/tmp/omp-chrome-profile-*
```

Do not add `--disable-gpu`, `--single-process`, or `--no-zygote` without new
device evidence.

Profiles live under `$PREFIX/tmp/omp-chrome-profile-*` (the same filesystem as
the binary), not under `$HOME/tmp` and not under `/tmp`. A failed launch
removes the OMP-owned profile and stops the matching daemon. The shared broker
daemon (`omp.browser.headless` / `omp.browser.headed`) identity includes
**every** launch-affecting value:

- canonical executable path,
- the complete launch args,
- headless mode,
- profile identity.

A different executable or an incompatible launch spec never reuses an old
daemon. The broker starts a fresh one. CDP binds to loopback only, with an
ephemeral port.

Relevant files:

- `packages/coding-agent/src/tools/browser/launch.ts`
- `packages/coding-agent/src/tools/browser/registry.ts`
- `packages/coding-agent/src/tools/browser/shared-daemon.ts`
- `packages/coding-agent/src/tools/browser/attach.ts`
- `android/scripts/verify-browser.sh` / `smoke-browser.sh`

### IPC endpoints and `sun_path` relocation

Unix IPC paths have the form `<dir>/<stem>.sock`. Linux and Android allow 108
bytes in `sockaddr_un::sun_path`, including the NUL. Longer paths are relocated
to a per-user private directory with a hashed basename (see
`crates/pi-natives/src/ipc/endpoint.rs`).

Android has no `/tmp` in Termux, so the relocation directory is
`<install root>/tmp/omp-<euid>`. The install root is the value the installer
bakes into the launcher and exports as `OMP_ANDROID_PREFIX`. `PREFIX` is only a
fallback. If neither value is an absolute path,
`/data/data/com.termux/files/usr` (the default Termux root) is used. Naming is
a pure function of that root and the effective uid. `TMPDIR` is ignored, and no
filesystem probe takes part. Shells and clients with different
`PREFIX`/`TMPDIR` values still name one socket per install. Non-Android Unix
keeps `/tmp/omp-<euid>`.

If the installed root is long enough that
`<root>/tmp/omp-<euid>/<hash16>.sock` cannot fit `sun_path`, `ipcEndpoint`
throws `socket path does not fit sockaddr_un …`. It does not return a path that
cannot bind.

The socket bind validates the directory, not the naming step:

- The base (`<root>/tmp`) is checked lexically, component by component, without
  following symlinks. A symlinked install root is rejected rather than
  resolved, so validation and use share one path. No second path can be swapped
  in.
- Every component must be owned by the app uid, root, or Android's system uid.
  A foreign owner is rejected even at `0755`, because an owner can chmod and
  rename its own entries. Ownership alone is not enough: a non-sticky directory
  must not be group- or world-writable. A write-open directory is accepted only
  with the sticky bit, where renames are limited to the entry owner (the
  `/tmp` rule). Group write is accepted only for the app's own group and the
  Android system group (`/data/data` is `0771 system:system`). The base must
  grant owner write and search. A root/system base counts only as sticky,
  world-writable and searchable, because a root-owned `0755` is not writable by
  the app.
- `omp-<euid>` must be a real directory owned by us. Any mode other than
  `0700` is repaired. A file, a symlink, or a foreign-owned directory fails
  with `EACCES` and the message `no usable IPC relocation directory …`.

There is no silent fallback. A broken directory or an over-long root is an
explicit failure, so the name a client dials is always the directory the server
validates.

Relevant files:

- `crates/pi-natives/src/ipc/endpoint.rs`
- `crates/pi-natives/src/ipc/unix.rs`
- `crates/pi-natives/src/ipc/mod.rs`
- `install.sh` (the launcher exports `OMP_ANDROID_PREFIX`)
- `packages/natives/test/ipc.test.ts`

### Android notifications (best-effort)

`EventController.sendCompletionNotification` / `sendErrorNotification` and
`AskTool.#sendAskNotification` keep their existing terminal notification. They
also mirror the card into the Android shade. The mirror runs after every
existing gate (config `*.notify`, Warp OSC protocol, stop reason,
`session.hasUI`). The helper in `utils/android-helpers.ts` re-checks the global
`PI_NOTIFICATIONS` suppression, truncates the fixed text, and makes no call
without `termux-notification`. Failures never block a turn.

Completion and error cards use `omp-status-<pid>`. Termux:API treats `--id` as
a global `NotificationManager` tag, so a literal id can let a second omp
process overwrite the first session's card. The ask card uses its own
`omp-ask-<pid>-<n>` id. `execute` clears it in a `finally` block that covers the
rich and plain dialog paths, answers, cancels, timeouts, aborts and throws. The
`termux-notification-remove` call is serialized behind the earlier post. If the
post was suppressed or failed, `execute` skips that call. Cards carry fixed text
only — never assistant output or the conversation-derived session name.

### Screenshots and model delivery (webpage only)

`tab.screenshot()` captures **webpage compositor output only**: viewport,
full-page, and element-by-selector. It delivers a PNG/WebP/JPEG `ImageContent`
to the model via `packages/coding-agent/src/utils/image-resize.ts`. It does
**not** capture Android system UI, other apps, or the full device display. Full
Android capture requires `MediaProjection`/`Termux:API`, and it is out of scope.

Contracts are preserved in:

- `packages/coding-agent/src/tools/browser/tab-worker.ts`
- `packages/coding-agent/src/tools/browser/tab-protocol.ts`
- `packages/coding-agent/src/utils/image-resize.ts`

Remote relay remains explicit via `browser.relay: true` or `browser.relayUrl`.
The existing configurable `exec`-tier approval policy is preserved.

## 3. What ships in the release bundle

The release bundle `omp-termux.tar.gz` is a self-contained Bun app:

```
$LIB_DIR/
├── bun                  # official @oven/bun-linux-aarch64-android (Bionic PIE)
├── cli.js               # split entry, shebang #!/usr/bin/env bun
├── chunks/              # lazy code-split chunks + assets
└── node_modules/@oh-my-pi/pi-natives/native/
    ├── index.js
    ├── loader-state.js
    ├── embedded-addon.js
    ├── clipboard.js
    └── pi_natives.android-arm64.node   # the stripped aarch64 addon
```

The tarball needs no external runtime download. It owns Bun, the JS
chunks/assets, and pi-natives as one versioned Bionic unit. **It never contains
Chromium.** Check with `tar -tzf omp-termux.tar.gz | grep -i chromium` (it
prints nothing) and
`bash android/scripts/verify-browser.sh --check-bundle ./omp-termux.tar.gz`.
The guarded installer and the Android update guard shown here are target
contracts. Current rollout status lives in
[port-changes.md](port-changes.md#current-delivery).

### Bun platform tag and the loader

Under Bun on Termux, `process.platform === "android"` and `process.arch ===
"arm64"`. The loader builds its tag as `${process.platform}-${process.arch}`
= **`android-arm64`**. `packages/natives/native/loader-state.js`
(`SUPPORTED_PLATFORMS`) must contain that tag, or the addon is rejected at
startup. `getAddonFilenames()` maps it to `pi_natives.android-arm64.node`.

### The addon

- Built by napi-rs from `crates/pi-natives` plus the `pi-shell`,
  `pi-builtins`, `pi-iso` crate family.
- `cargo build` produces `libpi_natives.so`. Packaging renames it to
  `pi_natives.android-arm64.node` (a shared ELF object that Bun dlopen's) and
  strips it with `llvm-strip --strip-unneeded`.
- Resident footprint is **only 6–8 MB after use**, and **0 KB on the
  `--version` split path**. Do not claim the 100 MB figure for the shipped
  addon.

### The browser artifact (separate)

The browser is **not** in the bundle. Current releases use the external
Termux:X11 package only. A future browser artifact will live under a separate
path such as `$PREFIX/lib/omp-browser/`, with its own pinned version/URL/SHA256
and ELF/Bionic validation.

## 4. ABI and capability floors

| Floor | Value | Meaning |
|-------|-------|---------|
| Binary / minSdk | **API 24** | `aarch64-linux-android24-clang`; ELF floor, minSdk 24 |
| pidfd capability | **API 34** | `pidfd_open` / `pidfd_send_signal`; on API 31–33 seccomp may SIGSYS-kill the syscall |
| Browser ELF | **API 24 / Bionic / ARM64** | `$PREFIX/lib/chromium/chrome` interpreter `/system/bin/linker64`; rejects glibc |

API 34 is a **claimed capability floor, not a tested compatibility gate**.
Tests cover only one API 34-class device. bionic provides
`forkpty`/`openpty` since API 23, so `portable-pty` works natively.

## 5. What runs natively vs. what is unsupported

| Surface | Android status |
|---------|----------------|
| Shell, PTY, process management (pidfd), grep, glob, file discovery, syntax highlighting, tokenization, AST edit/grep, diff, iso diff, sixel, fs-scan cache | **native**, unchanged |
| Clipboard | text only — copy uses `termux-clipboard-set`, paste uses `termux-clipboard-get` (`termux-api`); copy rejects when no channel (OSC 52 / Termux / native) accepted the text |
| Clipboard image paste | unsupported; JS returns `null` on Termux and the native stub errors |
| Audio capture / playback | unsupported; the current Android implementation returns an error |
| Codex live WebRTC peer | unsupported; the current Android implementation returns an error |
| Desktop surfaces | unsupported / not applicable |
| Local ONNX / STT / tiny inference | unsupported; optional dependencies are not shipped |
| Browser `browser` tool (webpage) | **supported via external Termux:X11 Chromium** — see §2 external Chromium; `tab.screenshot` (viewport/fullPage/element) → model `ImageContent`; CDP `127.0.0.1` ephemeral; profiles under `$PREFIX/tmp`; **full device display capture not supported** |
| Browser live CDP/relay | explicit only: `app.path` / `browser.cdpUrl` / `browser.relay` / `cmux` before headless; relay requires `browser.relay: true` or `browser.relayUrl` |
| Clipboard text read (computer worker) | target contract (pending): per-read host approval if exposed |

Unsupported surfaces return real errors. There is no fake or stubbed success.
The audio/WebRTC stubs exist because the `pi-voice` plus webrtc/opus dependency
graph OOM-kills free GitHub runners (~15 GB RAM + 24 GB swap still killed).
They are committed in-tree and gated on `target_os = "android"`.

## 6. Related docs

- See [port-changes.md](port-changes.md) — the maintained Android deltas, current vs. approved-pending.
- See [ci-cd.md](ci-cd.md) — GitHub Actions pipeline, NDK wiring, memory strategy.
- See [verification.md](verification.md) — on-device and CI verification, on a reference device only.
- See [setup.md](setup.md) — user-facing install and troubleshooting.
