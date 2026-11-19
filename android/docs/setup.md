# Setting up oh-my-pi on Termux

## Requirements

- Termux on aarch64 (arm64) Android.
- Release binaries target API 24 (Android 7.0) as the binary floor.
- Process signalling (pidfd) needs an API 34-class kernel. Verified on one
  reference device (kernel 6.1.118, API 34). This is not a broad compatibility
  claim.
- [Termux](https://github.com/termux/termux-app) from F-Droid or GitHub.
  Do not use the outdated Play Store build.
- **Clipboard (optional)**: install the `termux-api` package and the
  Termux:API app from the same source and signer as Termux. Copy uses
  `termux-clipboard-set` and paste uses `termux-clipboard-get`. Without it,
  copy fails with a visible error and does not claim success. Image paste is
  unsupported on every Android setup.
- **Browser (optional, for the `browser` tool)**: the external Termux:X11
  `chromium` package — see
  [Browser prerequisite](#browser-prerequisite-external-termuxx11-chromium).
  OMP does **not** bundle Chromium. Install it separately with `pkg`.

## Browser prerequisite: external Termux:X11 Chromium

The OMP `browser` tool on Android uses an **external** Bionic Chromium from the
Termux:X11 repository. The pinned reference tuple is:

```text
package:  chromium 149.0.7827.155 aarch64
source:   https://packages.termux.dev/apt/termux-x11 x11/main aarch64 Packages
deb:      chromium_149.0.7827.155_aarch64.deb
SHA256:   36500e23ad23bf2616bb4f215a297ba5a2b9e625992362b9b9c6bd05e0a27272
binary:   $PREFIX/lib/chromium/chrome
ELF:      ARM aarch64, Android API 24, Bionic, interpreter /system/bin/linker64
```

`headless_shell` is **not** used. That binary in the same package rejects
remote debugging (`Headless commands are not compatible with remote debugging`).
The main `chrome` binary supports `--headless=new` and CDP.

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

Validation is **fail-closed**. These conditions abort the check: a missing
package or binary, a wrong version, arch, or repo, a SHA256 mismatch, a
non-Bionic interpreter (`/lib/ld-linux-aarch64.so.1`), a `libc.so.6`
dependency, or a non-ARM64 ELF. OMP does not fall back to a glibc Chromium and
does not download a puppeteer-managed Linux build on Android. An invalid
explicit `browser.executablePath` or `PUPPETEER_EXECUTABLE_PATH` also fails
closed.

A future Chromium upgrade needs a new verified tuple: new version, new SHA256,
the same ELF/Bionic/API validation, and a device smoke test. Do not upgrade the
pin before you re-run `verify-browser.sh` and `smoke-browser.sh` on the
reference device.

Removing Chromium does not affect the OMP install:

```sh
pkg uninstall chromium
```

The OMP bundle remains `Bun + JS + pi-natives` only. See
[port-architecture.md](port-architecture.md#3-what-ships-in-the-release-bundle)
and `verify-browser.sh --check-bundle ./omp-termux.tar.gz`.

## Clipboard (optional)

Text copy and paste call Termux:API:

```sh
pkg install -y termux-api
```

Install the matching **Termux:API** app from the same source and with the same
signing key as Termux (both from F-Droid/GitHub, or both a compatible build).
A mismatched signer makes every call fail. To verify, run
`command -v termux-clipboard-set termux-clipboard-get` and run
`termux-clipboard-get` with a non-empty clipboard.

- Copy emits OSC 52 when stdout is a real TTY, then tries
  `termux-clipboard-set`. The interactive copy commands report "Copy failed"
  when every channel fails (missing `termux-api`, no TTY for OSC 52). They no
  longer print "Copied ..." unconditionally.
- Paste reads through `termux-clipboard-get`. If the call fails, OMP warns and
  treats the clipboard as empty. It does not block the input.
- Image paste is unsupported on Android. `readImageFromClipboard` returns
  `null` before it touches the native addon.

## Android notifications (optional, best effort)

When Termux:API is installed, OMP mirrors its turn notifications
(`completion.notify`, `error.notify`, `ask.notify`) into the Android shade
through `termux-notification`. Without Termux:API every call is a silent no-op.
OMP never installs packages for this.

- The global `PI_NOTIFICATIONS` switch (`off`, `0`, `false`) suppresses
  terminal and Android notifications. Per-event settings still gate each kind.
- Titles and bodies are fixed strings (`omp` plus `Complete` /
  `Stopped with error` / `Waiting for input`). Assistant text and the
  auto-generated session name never reach a card. A notification-listener app
  cannot read conversation content.
- The notification id is scoped to the process (`omp-status-<pid>`). Concurrent
  Termux sessions and tmux panes keep their own card. One session replaces its
  own card and does not stack cards.
- The `ask` card has its own per-prompt id (`omp-ask-<pid>-<n>`). Removing it
  never erases a completion/error card. Removal is serialized behind the post
  and happens on every dialog outcome: rich or plain UI, answer, cancel,
  timeout, abort, or throw.

## Release install: guarded installer (current)

```sh
curl -fsSL https://raw.githubusercontent.com/SklUpp/omp-termux/main/install.sh | sh
omp --version
```

If stdin is a terminal, the installer asks each question unless the choice is already set.
Flags, environment variables, and a pinned `OMP_VERSION` can select a choice.
The choices are extended release and Chromium. Both default to no. Without a terminal, the installer does not ask questions.

The standard release contains `01-termux.patch`. The extended release also contains `02-isolated-revive.patch`.
Use `-ex` or `--extended` to select the extended release. Use `--standard` to select the standard release.
Use `--browser` to install Chromium. Use `--no-browser` to skip Chromium.
`OMP_EXTENDED=0|1` and `OMP_INSTALL_BROWSER=0|1` set the same choices. Flags take priority over these variables.
`OMP_VERSION='<tag>'` selects an exact release tag. A conflicting explicit channel selection fails. The installer does not select another tag.

The installer requires Termux and arm64. It installs `curl` and `tar` through `pkg` when they are missing.
The release archive includes Bun. The installer does not install Bun through `pkg`.
When Chromium is selected, the installer installs `x11-repo` and updates Chromium and `libc++` through Termux packages.
`libc++` is shared with other Termux programs. Its update can affect those programs.
The Chromium package is external. A successful package install does not prove that Chromium runs on the device.

The installer checks the archive SHA-256 before extraction. It tests Bun, OMP, and the native addon in a staging directory.
It moves the previous runtime aside before it replaces the runtime. It attempts rollback if a post-swap check fails.
The installer writes the launcher to `$BIN_DIR/omp`. It checks the launcher after the swap.
Do not interrupt the swap. If installation fails, inspect the output and run the installer again.

On Android, `omp update` reports that it is unsupported and gives reinstall instructions.

## Update and reinstall

Android does not support `omp update` or `omp update --check`. Both commands
exit with an error and show reinstall instructions.

Reinstall with the installer used for this device. Keep the release profile:

```sh
# standard (default: applies only 01-termux.patch)
curl -fsSL https://raw.githubusercontent.com/SklUpp/omp-termux/main/install.sh | sh

# extended (adds 02-isolated-revive.patch)
curl -fsSL https://raw.githubusercontent.com/SklUpp/omp-termux/main/install.sh | sh -s -- -ex
```

The installer stages the new bundle and attempts rollback if a post-swap check
fails.

## Uninstall

```sh
rm -rf "$PREFIX/lib/omp-termux"
rm -f "$PREFIX/bin/omp"
# Browser is external — remove separately if desired:
pkg uninstall chromium
```

## Recovery after an interrupted install

- Before extraction or replacement: re-run the installer.
- During a swap: the exit trap restores `$LIB_DIR.old` if `$LIB_DIR` is
  missing. If not, inspect the directories before you retry.
- Checksum mismatch: re-run. Downloads already use `curl --retry 3`.
- `$LIB_DIR` intact but `omp` missing: re-run to restore `$PREFIX/bin/omp`.
- Browser missing or invalid after install: re-run
  `bash android/scripts/verify-browser.sh --install` or
  `pkg install -y x11-repo && pkg install -y chromium`. OMP itself remains
  intact.
- Failed Chromium validation: `verify-browser.sh` aborts without touching
  `$LIB_DIR`. Fix the package and re-run `verify-browser.sh`.

## Source build (maintainers only)

The source build needs the Rust toolchain and more memory than the release
install. See [../README.md](../README.md) and `android/scripts/` for the build
scripts.

## Troubleshooting

| Symptom | Cause / fix |
|---------|-------------|
| `pi_natives ... is not a function` | Addon/CLI mismatch. Reinstall the bundle, or rebuild after `git pull`. |
| Installer checksum mismatch | Corrupt download. Re-run the installer. |
| Process signals don't work | Kernel below API 34; seccomp blocks pidfd. Only the reference device (API 34) is verified. |
| `omp` starts but clipboard copy fails (`ENOENT: termux-clipboard-set`) | Install `pkg install -y termux-api` and the matching Termux:API app (same signer). Copy fails loudly instead of reporting success. |
| Clipboard paste always empty | Same `termux-api` requirement; check with `termux-clipboard-get`. |
| Clipboard image paste returns empty | Unsupported — expected. |
| Audio / WebRTC calls throw | Unsupported — stubbed on Android. Expected. |
| `browser` tool: `Chromium not found` / `No valid browser` | Chromium package missing or wrong version. Run `pkg install -y x11-repo && pkg install -y chromium && bash android/scripts/verify-browser.sh`. Check `dpkg -s chromium` shows `149.0.7827.155 aarch64` from `termux-x11`. |
| `browser` tool: `glibc` / `ld-linux` / `interpreter` error | Binary is glibc, not Bionic. Make sure `$PREFIX/lib/chromium/chrome` has `interpreter /system/bin/linker64` (`readelf -l`). Reinstall Termux:X11 chromium, not tur or glibc repo. |
| `browser` tool: `headless_shell` error | OMP must use `$PREFIX/lib/chromium/chrome`, not `headless_shell`. `headless_shell` rejects remote debugging by design. Do not set `PUPPETEER_EXECUTABLE_PATH` to it. |
| `verify-browser.sh` SHA256 mismatch | Cached `.deb` corrupt or version skew. Run `apt update && pkg install -y chromium`, then `bash android/scripts/verify-browser.sh`. |
| Screenshots blank / CDP timeout | Ensure loopback CDP: `"$PREFIX/lib/chromium/chrome" --headless=new --no-sandbox --disable-dev-shm-usage --remote-debugging-address=127.0.0.1 --remote-debugging-port=0 --dump-dom about:blank`. Check `bash android/scripts/smoke-browser.sh --no-network`. |
| `browser` shows glibc download attempt | On Android OMP never downloads managed Linux Chromium. Check that `browser.executablePath` / `PUPPETEER_EXECUTABLE_PATH` is invalid and that `verify-browser.sh` fails closed; no download occurs. |
| `browser` tool: `No working Chromium executable found on Android` | None of `$PREFIX/lib/chromium/chrome`, `$PREFIX/bin/chromium`, `$PREFIX/bin/chromium-browser`, `$PREFIX/bin/chrome` passed `--version`. Run `pkg install -y x11-repo && pkg install -y chromium` (two steps: the repo must exist first), then `bash android/scripts/verify-browser.sh`. |
| `no usable IPC relocation directory …` | The install root (`OMP_ANDROID_PREFIX`, else `PREFIX`) is bad. It is relative, contains a symlink, has a component owned by a different uid, or is world/foreign-group writable. Its `tmp`/`omp-<uid>` entry is missing, a file, a symlink, or foreign-owned. Use the launcher written by `install.sh` (it exports the baked root). Do not point `PREFIX` through a symlink. Make sure `$PREFIX/tmp` is a real directory owned by the app uid (`chmod 700 "$PREFIX/tmp"`). Remove a bad `omp-<uid>` entry and retry. |
| `socket path does not fit sockaddr_un …` | The installed root is too long for a 108-byte socket path. Reinstall under a shorter Termux prefix (the default is short enough). |

Unsupported capabilities fail loudly. They are not install errors.
