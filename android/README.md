# oh-my-pi on Termux (Android, aarch64)

Android/Termux port of [oh-my-pi](https://github.com/can1357/oh-my-pi).
Android-specific source, build scripts, and verification live under `android/`.

## Support status

- Target: Termux on aarch64 Android.
- Binary floor: API 24. `pidfd` process signaling needs an API 34-class kernel.
- Verified on one reference device: kernel 6.1.118, API 34-class environment.
- No broad Android-device compatibility claim.
- **Browser**: `browser` tool is supported via **external** Termux:X11
  `chromium` 149.0.7827.155 aarch64 (`$PREFIX/lib/chromium/chrome`, Bionic
  API 24, `/system/bin/linker64`). Chromium is **not bundled** with OMP;
  install separately with `pkg install x11-repo chromium` and validate with
  `bash android/scripts/verify-browser.sh`. `headless_shell` is not used for CDP.

Current releases use a guarded installer that bundles the official Bionic
`@oven/bun-linux-aarch64-android` and stages a two-rename swap with rollback
(see `android/docs/setup.md`). Do not interrupt the swap; `trap` restores the
previous tree.

Track delivery in
[`../specs/ANDROID_IMPLEMENTATION_PLAN.md`](../specs/ANDROID_IMPLEMENTATION_PLAN.md).

## Current capability boundary

| Surface | Status |
|---------|--------|
| Shell, PTY, process management, grep, glob, highlighting | Native |
| Text clipboard copy | JS fallback through `termux-clipboard-set` |
| Clipboard image, audio, WebRTC, desktop, local ONNX/STT | Unsupported |
| Browser `browser` tool (webpage) | **Supported via external Chromium** — `pkg install x11-repo chromium` → `bash android/scripts/verify-browser.sh` → `bash android/scripts/smoke-browser.sh`; executable `$PREFIX/lib/chromium/chrome` (`--headless=new`, loopback CDP `127.0.0.1:0`, profiles `$PREFIX/tmp`); screenshots: viewport/fullPage/element → model `ImageContent`; explicit `app`/`relay`/`cdpUrl`/`cmux` before headless; **no bundled Chromium, no glibc, no headless_shell for CDP** |
| Browser live CDP/relay | Explicit only — `app.path`, `browser.cdpUrl`, `browser.relay`/`relayUrl`, `cmux`; `app`/`relay`/`cdp`/`cmux` route before local headless; per-call `exec` approval still applies |
| Full Android display capture | Unsupported — `browser` screenshots are webpage compositor only; `MediaProjection`/`screencap` out of scope |

Unsupported surfaces must return a clear error. They must not report fake success.
Removing Chromium (`pkg uninstall chromium`) does not affect the OMP install.

## Documentation

| Document | Owner |
|----------|-------|
| [docs/setup.md](docs/setup.md) | Current install behavior, guarded swap, uninstall, recovery, **browser prerequisite** |
| [docs/port-architecture.md](docs/port-architecture.md) | Target Bionic runtime, split layout, **external Chromium resolution/validation/daemon/screenshot**, capability boundaries |
| [docs/ci-cd.md](docs/ci-cd.md) | Current vs target build/package/release pipeline |
| [docs/verification.md](docs/verification.md) | Implemented and pending acceptance gates, **browser pin + bundle-absence + smoke** |
| [docs/port-changes.md](docs/port-changes.md) | Maintained Android delta and rebase mechanics |

Decision evidence remains in
[`../specs/ANDROID_NATIVE_PLAN.md`](../specs/ANDROID_NATIVE_PLAN.md).
Browser reference tuple and device evidence live in
[`../docs/superpowers/specs/2026-08-28-termux-browser-integration-design.md`](../docs/superpowers/specs/2026-08-28-termux-browser-integration-design.md).

### Quick browser check

```sh
pkg install -y x11-repo chromium   # idempotent external install, not bundled
bash android/scripts/verify-browser.sh          # pinned 149.0.7827.155 / SHA256 / ELF Bionic API24
bash android/scripts/smoke-browser.sh --no-network  # CDP + screenshots + daemon + rejection (no net)
bash android/scripts/verify-browser.sh --check-bundle ./omp-termux.tar.gz  # bundle contains no chromium
```
