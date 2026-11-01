# Setting up oh-my-pi on Termux

## Requirements

- Termux on aarch64 (arm64) Android.
- Release binaries target API 24 (Android 7.0) as the binary floor.
- Process signalling (pidfd) needs an API 34-class kernel. Verified on one
  reference device (kernel 6.1.118, API 34). No broad compatibility claim.
- [Termux](https://github.com/termux/termux-app) from F-Droid or GitHub.
  The Play Store build is outdated — do not use it.
- **Browser (optional, for `browser` tool)**: external Termux:X11 `chromium`
  package — see [Browser prerequisite](#browser-prerequisite-external-termuxx11-chromium) below.
  OMP does **not** bundle Chromium; the device installs it separately via `pkg`.

## Browser prerequisite: external Termux:X11 Chromium

OMP's `browser` tool on Android uses an **external** Bionic Chromium installed
from the Termux:X11 repository. The pinned reference tuple is:

```text
package:  chromium 149.0.7827.155 aarch64
source:   https://packages.termux.dev/apt/termux-x11 x11/main aarch64 Packages
deb:      chromium_149.0.7827.155_aarch64.deb
SHA256:   36500e23ad23bf2616bb4f215a297ba5a2b9e625992362b9b9c6bd05e0a27272
binary:   $PREFIX/lib/chromium/chrome
ELF:      ARM aarch64, Android API 24, Bionic, interpreter /system/bin/linker64
```

`headless_shell` is **not** used — that binary in the same package rejects
remote debugging (`Headless commands are not compatible with remote debugging`);
the main `chrome` binary supports `--headless=new` and CDP.

Idempotent setup (safe to rerun):

```sh
pkg install -y x11-repo
pkg install -y chromium
# validate pinned version/repo/SHA256 and ELF/Bionic/API/glibc gates
bash android/scripts/verify-browser.sh
# full device smoke: CDP, navigation, viewport/fullPage/element screenshots, daemon, rejection
bash android/scripts/smoke-browser.sh
# quick probe without network-dependent navigation
bash android/scripts/smoke-browser.sh --no-network --skip-daemon
```

Validation is **fail-closed**: missing package/binary, wrong version/arch/repo,
SHA256 mismatch, non-Bionic interpreter (`/lib/ld-linux-aarch64.so.1`),
`libc.so.6` dependency, or non-ARM64 ELF all abort. No fallback to a glibc
Chromium and no puppeteer-managed Linux download occurs on Android; an invalid
explicit `browser.executablePath` or `PUPPETEER_EXECUTABLE_PATH` fails closed
rather than silently trying another binary.

Future Chromium upgrades require a new verified tuple (new version + new
SHA256 + same ELF/Bionic/API validation + device smoke). Do not upgrade the
pin without re-running `verify-browser.sh` and `smoke-browser.sh` on the
reference device.

Removing Chromium does not affect the OMP install:

```sh
pkg uninstall chromium
```

OMP bundle remains `Bun + JS + pi-natives` only; see
[port-architecture.md](port-architecture.md#3-what-ships-in-the-release-bundle)
and `verify-browser.sh --check-bundle ./omp-termux.tar.gz`.

## Release install: guarded installer (current)

```sh
curl -fsSL https://raw.githubusercontent.com/sasazemzulin058-debug/omp-termux/main/install.sh | sh
omp --version
```

Current `install.sh` (see `install.sh` in this repo) is now a **guarded**
installer. It:

1. Requires Termux and aarch64.
2. Installs only `curl`/`tar` if missing (does **not** `pkg install bun`; the
   release tarball bundles the official Bionic Bun).
3. Downloads and checks `omp-termux.tar.gz` + `.sha256`.
4. Stages to `$LIB_DIR.new` and pre-swap smoke-tests
   `"$LIB_DIR.new/bun" "$LIB_DIR.new/cli.js" --version`.
5. Performs a guarded two-rename swap (`$LIB_DIR` → `$LIB_DIR.old`,
   `$LIB_DIR.new` → `$LIB_DIR`) with rollback on failure; `trap` restores
   `$LIB_DIR.old` if `$LIB_DIR` disappears.
6. Post-swap smoke-tests, writes shim
   `exec env OMP_PLATFORM=android "$LIB_DIR/bun" "$LIB_DIR/cli.js" "$@"`,
   and verifies `$BIN/omp --version`.

Do not interrupt during the swap; the `trap` will restore the previous tree.
Re-run the installer if any step fails. Chromium is **not** installed or
bundled by `install.sh`; manage it separately via the browser prerequisite above.

The prior preview limitation (Termux `bun` glibc wrapper + non-atomic `rm -rf`)
is now resolved; the installer matches the target contract in
[`../../specs/ANDROID_IMPLEMENTATION_PLAN.md`](../../specs/ANDROID_IMPLEMENTATION_PLAN.md).
`omp update` on Android now prints a reinstall instruction.

## Update and reinstall

`omp update` is Android-aware and prints a reinstall instruction:

```sh
# On Android, omp update exits with:
# error: OMP on Termux/Android does not support in-place update.
# Re-run the installer: curl -fsSL .../install.sh | sh
```

Re-run `install.sh` instead.

## Uninstall

```sh
rm -rf "$PREFIX/lib/omp-termux"
rm -f "$PREFIX/bin/omp"
# Browser is external — remove separately if desired:
pkg uninstall chromium
```

## Recovery after an interrupted install

- Before extraction/replacement: re-run the installer.
- During guarded swap: `trap` restores `$LIB_DIR.old` → `$LIB_DIR` automatically
  if `$LIB_DIR` is missing; otherwise re-run the installer.
- Checksum mismatch: re-run; downloads already use `curl --retry 3`.
- `$LIB_DIR` intact but `omp` missing: re-run to restore `$PREFIX/bin/omp`.
- Browser missing/invalid after install: re-run
  `bash android/scripts/verify-browser.sh --install` or
  `pkg install -y x11-repo && pkg install -y chromium`; OMP itself remains intact.
- Failed Chromium validation: `verify-browser.sh` aborts without touching
  `$LIB_DIR`; fix the package and re-run `verify-browser.sh`.

## Source build (maintainers only)

The on-device source build compiles the native addon on the device from a
source clone. It needs Rust, the full toolchain, and a long OOM-controlled
build. This is not the supported user path; use the release install. See
[../README.md](../README.md) and the scripts under `android/scripts/` for
how to run it.

## Troubleshooting

| Symptom | Cause / fix |
|---------|-------------|
| `pi_natives ... is not a function` | Addon/CLI mismatch. Reinstall the bundle, or rebuild after `git pull`. |
| Installer checksum mismatch | Corrupt download. Re-run the installer. |
| Process signals don't work | Kernel below API 34; seccomp blocks pidfd. Only the reference device (API 34) is verified. |
| `omp` starts but clipboard copy fails | Unsupported — expected. Use `termux-clipboard-set` (`termux-api`). |
| Clipboard image paste returns empty | Unsupported — expected. |
| Audio / WebRTC calls throw | Unsupported — stubbed on Android. Expected. |
| `browser` tool: `Chromium not found` / `No valid browser` | Chromium package missing or wrong version. `pkg install -y x11-repo && pkg install -y chromium && bash android/scripts/verify-browser.sh`. Check `dpkg -s chromium` shows `149.0.7827.155 aarch64` from `termux-x11`. |
| `browser` tool: `glibc` / `ld-linux` / `interpreter` error | Binary is glibc, not Bionic. Ensure `$PREFIX/lib/chromium/chrome` has `interpreter /system/bin/linker64` (`readelf -l`). Reinstall Termux:X11 chromium, not tur or glibc repo. |
| `browser` tool: `headless_shell` error | OMP must use `$PREFIX/lib/chromium/chrome`, not `headless_shell`. `headless_shell` rejects remote debugging by design. Do not set `PUPPETEER_EXECUTABLE_PATH` to it. |
| `verify-browser.sh` SHA256 mismatch | Cached `.deb` corrupt or version skew. `apt update && pkg install -y chromium` then `bash android/scripts/verify-browser.sh`. |
| Screenshots blank / CDP timeout | Ensure loopback CDP: `"$PREFIX/lib/chromium/chrome" --headless=new --no-sandbox --disable-dev-shm-usage --remote-debugging-address=127.0.0.1 --remote-debugging-port=0 --dump-dom about:blank`. Check `bash android/scripts/smoke-browser.sh --no-network`. |
| `browser` shows glibc download attempt | On Android OMP never downloads managed Linux Chromium. Check `browser.executablePath` / `PUPPETEER_EXECUTABLE_PATH` invalid and `verify-browser.sh` fails closed; no download occurs. |

Unsupported capabilities fail loudly; they are not install errors.
