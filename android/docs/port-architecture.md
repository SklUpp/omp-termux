# Port architecture: oh-my-pi on Android arm64 / Termux

This document explains *how* the oh-my-pi native stack runs on Android.
It is the design companion to [port-changes.md](port-changes.md) (what
changed, current vs. pending) and [ci-cd.md](ci-cd.md) (how CI builds it).
Everything Android-specific lives under `android/`.

The upstream tree is only ever modified by the deterministic overlay —
see [port-changes.md](port-changes.md#delivery). The architecture below is
the decided target; staged rollout is tracked in
`../../specs/ANDROID_IMPLEMENTATION_PLAN.md`.

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

Consequences that drive every patch:

- `#[cfg(target_os = "linux")]` code is **dead** on Android.
- `#[cfg(not(target_os = "linux"))]` branches **fire** on Android.
- Gate shared Linux/Android code with
  `#[cfg(any(target_os = "linux", target_os = "android"))]`.

### Builds are native on-device, cross in CI

Termux's own Rust reports `host: aarch64-linux-android`, so an on-device
build is a *native* build with no NDK and no cross toolchain. Only GitHub
Actions cross-compiles (Linux x86_64 → aarch64-linux-android) with the NDK.

## 2. The runtime contract

### Bundled Bionic Bun, not apt/proot Bun

The release tarball is **self-contained and Bionic-only**. It bundles the
official `@oven/bun-linux-aarch64-android` Bionic binary (PIE, interpreter
`/system/bin/linker64`) inside `$LIB_DIR`. A thin shim executes it directly:

```sh
exec env OMP_PLATFORM=android "$LIB_DIR/bun" "$LIB_DIR/cli.js" "$@"
```

The shim runs `$LIB_DIR/bun`, **never** `$PREFIX/bin/bun`. The installer no
longer installs Termux's apt `bun` package. Mixing a glibc-wrapper Bun with
a Bionic NDK `.node` is an unsupported ABI configuration and is prohibited.
`bun build --compile` is also prohibited: the built Android standalone
binary segfaulted on every attempt, so the runtime remains packaged Bun
plus JS chunks.

Bun version pins live in a single `android/versions.env` (BUN, NDK,
ANDROID_API, RUST), read by both workflows and the install/package scripts.
Version changes land only together with the addon and after the on-device
smoke test.

### External Bionic Chromium, not bundled (browser tool)

The `browser` tool on Android uses an **external** Termux:X11 Chromium,
not a bundled binary. The OMP release tarball remains `Bun + JS + pi-natives`
only; Chromium is installed separately via `pkg` and validated by
`android/scripts/verify-browser.sh`. Release CI publishes `verify-browser.sh` /
`smoke-browser.sh` and metadata — it never copies `$PREFIX/lib/chromium/chrome`
into the bundle (see `android/scripts/verify-browser.sh --check-bundle`).

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
selected executable: it rejects remote debugging
(`Headless commands are not compatible with remote debugging`) while the main
`chrome` binary supports `--headless=new` with loopback CDP and screenshots
— verified on device.

**Executable resolution (headless local mode only)** — existing router stays
first: `app.path` / relay / `browser.cdpUrl` / `cmux` before headless. Only
headless resolves a local binary, in this order:

1. `browser.executablePath` (config, `~` expanded) — validated, fails closed if invalid;
2. `PUPPETEER_EXECUTABLE_PATH` (env) — validated, fails closed if invalid;
3. `$PREFIX/lib/chromium/chrome` — canonical Termux:X11 binary;
4. `$PREFIX/bin/chromium`;
5. `$PREFIX/bin/chromium-browser`;
6. `$PREFIX/bin/chrome`.

Every candidate is validated through one async validator:

- absolute path, regular executable file;
- `--version` contains `Chromium`/`Chrome`/`Edge` and (on device) pinned `149.0.7827.155`;
- `file(1)` confirms ARM64 shared-object/PIE, `readelf -l` shows
  interpreter `/system/bin/linker64`, `readelf -d` shows `libc.so` (Bionic)
  and **no** `libc.so.6` nor `ld-linux-aarch64.so.1`;
- `.note.android.ident` API level `24`, NDK r29 build.

Invalid configured paths produce an explicit `ToolError` and never silently
fall through to another binary. When no valid Android binary exists, the tool
throws an actionable install/configuration error. Puppeteer **never** downloads
a managed Linux Chromium on Android (`android` platform short-circuits the
download path — `verify-browser.sh` and the `no-linux-download` unit test
prove it).

The validated executable is threaded from `BrowserTool` → `acquireBrowser`
→ `launchHeadlessBrowser` / `resolveSharedBrowserLaunchSpec`; daemon identity
includes the canonical resolved executable and complete launch spec (see
§2.3). Existing webpage `tab.screenshot()` / `ImageContent` contracts are
preserved.

### Measured split packaging

The JS package is built with `Bun.build` and **`splitting: true`** required.
Whole-tree packaging is used — the build copies the entire `outDir`; the
shebang sits only on `cli.js`; there is no manifest; all chunks and assets
are tarred.

| Metric | Monolith | Split | Delta |
|--------|----------|-------|-------|
| Entry `cli.js` | — | 22,608 B / 322 files | — |
| `omp --version` median | 640 ms | 134 ms | ~4.8x |
| RSS | 129–144 MB | 54.9 MB | ~2x |

Targets: startup `--version` median ≤ 200 ms, HWM ≤ 65 MB. Splitting moves
runtime resources off the cold path — it does **not** remove them.

### Runtime resources

`models.json`, protobuf data, `CHANGELOG`, and `templates/` + tool views
are required at runtime. Splitting relocates them to lazy chunks/payloads;
it never drops them. OpenTelemetry must stay bundled into lazy chunks —
a stale local `node_modules` once caused the experimental-subcommand
failure, not splitting.

### Browser launch and daemon identity

Preserved flags plus the one device-proven Android supplement:

```text
--headless=new
--no-sandbox
--disable-dev-shm-usage
--remote-debugging-address=127.0.0.1
--remote-debugging-port=0        # ephemeral, loopback-only; never 0.0.0.0
--user-data-dir=$PREFIX/tmp/omp-chrome-profile-*
```

Do not add `--disable-gpu`, `--single-process`, or `--no-zygote` without
new device evidence.

Profiles live under `$PREFIX/tmp/omp-chrome-profile-*` (same filesystem as
the binary), not `$HOME/tmp` nor `/tmp`. Failed launch removes the
OMP-owned profile and stops the matching daemon. The shared broker daemon
(`omp.browser.headless` / `omp.browser.headed`) identity includes **every**
launch-affecting value:

- canonical executable path,
- complete launch args,
- headless mode,
- profile identity.

A different executable or incompatible launch spec never reuses an old daemon;
the broker starts a fresh one. CDP binds loopback-only with an ephemeral port.

Relevant files:

- `packages/coding-agent/src/tools/browser/launch.ts`
- `packages/coding-agent/src/tools/browser/registry.ts`
- `packages/coding-agent/src/tools/browser/shared-daemon.ts`
- `packages/coding-agent/src/tools/browser/attach.ts`
- `android/scripts/verify-browser.sh` / `smoke-browser.sh`

### Screenshots and model delivery (webpage only)

`tab.screenshot()` captures **webpage compositor output only** — viewport,
full-page, and element-by-selector — and delivers a PNG/WebP/JPEG
`ImageContent` to the model via `packages/coding-agent/src/utils/image-resize.ts`.
It does **not** capture Android system UI, other apps, or the full device
display; full Android capture would require `MediaProjection`/`Termux:API` and
is out of scope.

Contracts are preserved in:

- `packages/coding-agent/src/tools/browser/tab-worker.ts`
- `packages/coding-agent/src/tools/browser/tab-protocol.ts`
- `packages/coding-agent/src/utils/image-resize.ts`

Remote relay remains explicit via `browser.relay: true` or `browser.relayUrl`;
the existing configurable `exec`-tier approval policy is preserved.

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

The tarball needs no external runtime download. It owns Bun, JS chunks/assets,
and pi-natives as one versioned Bionic unit. **It never contains Chromium**
— verify with `tar -tzf omp-termux.tar.gz | grep -i chromium` (should be
empty) and `bash android/scripts/verify-browser.sh --check-bundle ./omp-termux.tar.gz`.
The guarded installer and Android update guard shown here are target contracts;
current rollout status lives in
[port-changes.md](port-changes.md#current-state-vs-approved-pending).

### Bun platform tag and the loader

Under Bun on Termux: `process.platform === "android"`, `process.arch ===
"arm64"`. The loader builds its tag as `${process.platform}-${process.arch}`
= **`android-arm64`**; `packages/natives/native/loader-state.js`
(`SUPPORTED_PLATFORMS`) must contain it or the addon is rejected at
startup. `getAddonFilenames()` maps it to `pi_natives.android-arm64.node`.

### The addon

- Built by napi-rs from `crates/pi-natives` plus the `pi-shell`,
  `pi-builtins`, `pi-iso` crate family.
- `cargo build` produces `libpi_natives.so`; packaging renames it to
  `pi_natives.android-arm64.node` (a shared ELF object dlopen'd by Bun) and
  strips it with `llvm-strip --strip-unneeded`.
- Resident footprint is **only 6–8 MB after use**, and **0 KB on the
  `--version` split path**. Do not claim the 100 MB figure for the shipped
  addon.

### The browser artifact (separate)

Browser is **not** in the bundle. If OMP ever manages a browser artifact
(a separate archive), it would live under a separate path such as
`$PREFIX/lib/omp-browser/` with its own pinned version/URL/SHA256 and
ELF/Bionic validation. Current releases use the external Termux:X11 package
only.

## 4. ABI and capability floors

| Floor | Value | Meaning |
|-------|-------|---------|
| Binary / minSdk | **API 24** | `aarch64-linux-android24-clang`; ELF floor, minSdk 24 |
| pidfd capability | **API 34** | `pidfd_open` / `pidfd_send_signal`; on API 31–33 seccomp may SIGSYS-kill the syscall |
| Browser ELF | **API 24 / Bionic / ARM64** | `$PREFIX/lib/chromium/chrome` interpreter `/system/bin/linker64`; rejects glibc |

API 34 is a **claimed capability floor, not a tested compatibility gate**:
only one API 34-class device has been tested. No broad compatibility claim
is made. bionic provides `forkpty`/`openpty` since API 23, so `portable-pty`
works natively.

## 5. What runs natively vs. what is unsupported

| Surface | Android status |
|---------|----------------|
| Shell, PTY, process management (pidfd), grep, glob, file discovery, syntax highlighting, tokenization, AST edit/grep, diff, iso diff, sixel, fs-scan cache | **native**, unchanged |
| Clipboard | text only — JS falls back to `termux-clipboard-set` (`termux-api`) |
| Clipboard image paste | unsupported; current implementation returns empty |
| Audio capture / playback | unsupported; current Android implementation returns an error |
| Codex live WebRTC peer | unsupported; current Android implementation returns an error |
| Desktop surfaces | unsupported / not applicable |
| Local ONNX / STT / tiny inference | unsupported; optional dependencies are not shipped |
| Browser `browser` tool (webpage) | **supported via external Termux:X11 Chromium** — see §2 external Chromium; `tab.screenshot` (viewport/fullPage/element) → model `ImageContent`; CDP `127.0.0.1` ephemeral; profiles `$PREFIX/tmp`; **full device display capture not supported** |
| Browser live CDP/relay | explicit only: `app.path` / `browser.cdpUrl` / `browser.relay` / `cmux` before headless; relay requires `browser.relay: true` or `browser.relayUrl` |
| Clipboard text read (computer worker) | target contract (pending): per-read host approval if exposed |

Unsupported surfaces return real errors — no fake or stubbed success.
Audio/WebRTC stubs exist because the `pi-voice` + webrtc/opus dependency
graph OOM-kills free GitHub runners (~15 GB RAM + 24 GB swap still killed);
they are committed in-tree and gated on `target_os = "android"`.

## 6. Related docs

- [port-changes.md](port-changes.md) — the maintained Android deltas, current vs. approved-pending.
- [ci-cd.md](ci-cd.md) — GitHub Actions pipeline, NDK wiring, memory strategy.
- [verification.md](verification.md) — on-device and CI verification; reference-device only.
- [setup.md](setup.md) — user-facing install and troubleshooting.
