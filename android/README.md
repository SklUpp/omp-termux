# oh-my-pi on Termux (Android, aarch64)

Android/Termux port of [oh-my-pi](https://github.com/can1357/oh-my-pi).
Android-specific source, build scripts, and verification live under `android/`.

## Support status

- Target: Termux on aarch64 Android.
- Binary floor: API 24. `pidfd` process signaling needs an API 34-class kernel.
- Device coverage is limited. A successful CI build does not prove compatibility with every Android device.
- The browser tool requires an external Termux:X11 Chromium package. Chromium is not in the OMP archive.
- Chromium compatibility depends on the package and device. Use `android/scripts/verify-browser.sh` to check the reference package.

The installer verifies the release checksum. It stages the runtime and attempts rollback after a failed swap.
The release includes Android Bun. See [setup.md](docs/setup.md) for install and recovery instructions.


## Current capability boundary

| Surface | Status |
|---------|--------|
| Shell, PTY, process management, grep, glob, highlighting | Native |
| Text clipboard copy | JS fallback through `termux-clipboard-set` |
| Clipboard image, audio, WebRTC, desktop, local ONNX/STT | Unsupported |
| Browser `browser` tool (webpage) | Requires external Chromium. Package and device compatibility must be verified. |
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


### Quick browser check

```sh
pkg install -y x11-repo
pkg install -y chromium
bash android/scripts/verify-browser.sh          # pinned 149.0.7827.155 / SHA256 / ELF Bionic API24
bash android/scripts/smoke-browser.sh --no-network  # CDP + screenshots + daemon + rejection (no net)
bash android/scripts/verify-browser.sh --check-bundle ./omp-termux.tar.gz  # bundle contains no chromium
```
