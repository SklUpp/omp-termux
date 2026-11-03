# Setting up oh-my-pi on Termux

## Requirements

- Termux on aarch64 (arm64) Android.
- Release binaries target API 24 (Android 7.0) as the binary floor.
- Process signalling (pidfd) needs an API 34-class kernel. Verified on one
  reference device (kernel 6.1.118, API 34). No broad compatibility claim.
- [Termux](https://github.com/termux/termux-app) from F-Droid or GitHub.
  The Play Store build is outdated — do not use it.
- **Clipboard (optional)**: `termux-api` package plus the Termux:API app from
  the same source/signer as Termux — copy uses `termux-clipboard-set`, paste
  uses `termux-clipboard-get`. Without it, copy fails with a visible error
  instead of claiming success; image paste is unsupported on every Android
  setup.
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

## Clipboard (optional)

Text copy and paste shell out to Termux:API:

```sh
pkg install -y termux-api
```

The matching **Termux:API** app must be installed from the same source and with
the same signing key as Termux itself (both from F-Droid/GitHub, or both a
compatible build); a mismatched signer makes every call fail. To verify:
`command -v termux-clipboard-set termux-clipboard-get` and
`termux-clipboard-get` with a non-empty clipboard.

- Copy emits OSC 52 when stdout is a real TTY, then tries
  `termux-clipboard-set`. The interactive copy commands report "Copy failed"
  when every channel fails (missing `termux-api`, no TTY for OSC 52); they no
  longer print "Copied ..." unconditionally.
- Paste reads through `termux-clipboard-get`; if the call fails, OMP warns and
  treats the clipboard as empty rather than blocking the input.
- Image paste is unsupported on Android: `readImageFromClipboard` returns
  `null` before touching the native addon.

## Android notifications (optional, best-effort)

When Termux:API is installed, OMP mirrors its existing turn notifications
(`completion.notify`, `error.notify`, `ask.notify`) into the Android shade
through `termux-notification`. Without Termux:API every call is a silent
no-op; OMP never installs packages for this.

- The global `PI_NOTIFICATIONS` switch (`off`, `0`, `false`) suppresses both
  terminal and Android notifications. Per-event settings still gate each kind.
- Titles and bodies are fixed strings (`omp` plus `Complete` /
  `Stopped with error` / `Waiting for input`). Assistant text and the
  auto-generated session name never reach a card, so a notification-listener
  app cannot read conversation content.
- The notification id is scoped to the process (`omp-status-<pid>`), so
  concurrent Termux sessions and tmux panes keep their own card instead of
  overwriting each other, while one session replaces its own card rather than
  stacking.
- The `ask` card has its own per-prompt id (`omp-ask-<pid>-<n>`), so removing
  it never erases a completion/error card. Removal is serialized behind the
  post and happens on every dialog outcome — rich or plain UI, answer, cancel,
  timeout, abort, or throw.

## Release install: guarded installer (current)

```sh
curl -fsSL https://raw.githubusercontent.com/SklUpp/omp-termux/main/install.sh | sh
omp --version
```

Standard release (default) applies only `01-termux.patch`. The extended release
adds the isolation lifecycle patch (`02-isolated-revive.patch`) and is selected
with `-ex`:

```sh
curl -fsSL https://raw.githubusercontent.com/SklUpp/omp-termux/main/install.sh | sh -s -- -ex
```

`-ex` resolves the newest `v*-termux-ex` release tag through the GitHub API and
fails closed if none exists. `OMP_VERSION='<tag>'` pins the exact tag and never
falls back to another release.

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
`omp update` on Android refuses and prints the reinstall commands.

## Update and reinstall

Upstream release binaries and npm natives do not target Android, so OMP never
updates itself in place here. Both `omp update` and `omp update --check` report
that updates are unsupported and print the reinstall commands below. Neither
queries upstream release metadata, and both exit non-zero so scripts do not
mistake the guidance for an available update. The interactive startup
version notice is suppressed for the same reason.

Reinstall with the installer used for this device, keeping the release profile:

```sh
# standard (default: applies only 01-termux.patch)
curl -fsSL https://raw.githubusercontent.com/SklUpp/omp-termux/main/install.sh | sh

# extended (adds 02-isolated-revive.patch)
curl -fsSL https://raw.githubusercontent.com/SklUpp/omp-termux/main/install.sh | sh -s -- -ex
```

The installer stages the new bundle and swaps it with rollback; re-running it
is the supported update path.

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
| `omp` starts but clipboard copy fails (`ENOENT: termux-clipboard-set`) | Install `pkg install -y termux-api` and the matching Termux:API app (same signer). Copy now fails loudly instead of reporting success. |
| Clipboard paste always empty | Same `termux-api` requirement; check with `termux-clipboard-get`. |
| Clipboard image paste returns empty | Unsupported — expected. |
| Audio / WebRTC calls throw | Unsupported — stubbed on Android. Expected. |
| `browser` tool: `Chromium not found` / `No valid browser` | Chromium package missing or wrong version. `pkg install -y x11-repo && pkg install -y chromium && bash android/scripts/verify-browser.sh`. Check `dpkg -s chromium` shows `149.0.7827.155 aarch64` from `termux-x11`. |
| `browser` tool: `glibc` / `ld-linux` / `interpreter` error | Binary is glibc, not Bionic. Ensure `$PREFIX/lib/chromium/chrome` has `interpreter /system/bin/linker64` (`readelf -l`). Reinstall Termux:X11 chromium, not tur or glibc repo. |
| `browser` tool: `headless_shell` error | OMP must use `$PREFIX/lib/chromium/chrome`, not `headless_shell`. `headless_shell` rejects remote debugging by design. Do not set `PUPPETEER_EXECUTABLE_PATH` to it. |
| `verify-browser.sh` SHA256 mismatch | Cached `.deb` corrupt or version skew. `apt update && pkg install -y chromium` then `bash android/scripts/verify-browser.sh`. |
| Screenshots blank / CDP timeout | Ensure loopback CDP: `"$PREFIX/lib/chromium/chrome" --headless=new --no-sandbox --disable-dev-shm-usage --remote-debugging-address=127.0.0.1 --remote-debugging-port=0 --dump-dom about:blank`. Check `bash android/scripts/smoke-browser.sh --no-network`. |
| `browser` shows glibc download attempt | On Android OMP never downloads managed Linux Chromium. Check `browser.executablePath` / `PUPPETEER_EXECUTABLE_PATH` invalid and `verify-browser.sh` fails closed; no download occurs. |
| `browser` tool: `No working Chromium executable found on Android` | None of `$PREFIX/lib/chromium/chrome`, `$PREFIX/bin/chromium`, `$PREFIX/bin/chromium-browser`, `$PREFIX/bin/chrome` passed `--version`. Run `pkg install -y x11-repo && pkg install -y chromium` (two steps: the repo must exist first), then `bash android/scripts/verify-browser.sh`. |
| `no usable IPC relocation directory …` | The install root (`OMP_ANDROID_PREFIX`, else `PREFIX`) is relative, contains a symlink, has a component owned by a different uid, is world/foreign-group writable, or its `tmp`/`omp-<uid>` entry is missing, a file, a symlink, or foreign-owned. Use the launcher written by `install.sh` (it exports the baked root), do not point `PREFIX` through a symlink, and ensure `$PREFIX/tmp` is a real directory owned by the app uid (`chmod 700 "$PREFIX/tmp"`); remove a bad `omp-<uid>` entry and retry. |
| `socket path does not fit sockaddr_un …` | The installed root is too long for a 108-byte socket path. Reinstall under a shorter Termux prefix (the default is short enough). |

Unsupported capabilities fail loudly; they are not install errors.
