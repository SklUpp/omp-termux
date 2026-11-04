# Verification Procedures

## Automated CI checks

1. `python3 android/scripts/verify-overlay.py` checks the Android overlay
   markers.
2. `python3 android/scripts/check-release-inputs.py` checks the package metadata
   and the official Bun manifest fields.
3. The release downloads `BUN_ARCHIVE_NAME` from the official Bun release URL.
   It verifies `BUN_SHA256`, extracts the executable `bun`, and checks the
   aarch64 ELF architecture.
4. The release checks the native addon version stamp and package contents. It does not execute Android Bun in CI:

```sh
sha256sum -c omp-termux.tar.gz.sha256
tar -tzf omp-termux.tar.gz | grep -Fx './cli.js'
tar -tzf omp-termux.tar.gz | grep -Fx './bun'
tar -tzf omp-termux.tar.gz | grep -F './node_modules/@oh-my-pi/pi-natives/native/pi_natives.android-arm64.node'
```

5. The release checks that the OMP bundle **does not bundle Chromium** and that
   the browser pin is intact:

```sh
# browser pin metadata (fail closed on mismatch)
grep -q '149.0.7827.155' android/scripts/verify-browser.sh
grep -q '36500e23ad23bf2616bb4f215a297ba5a2b9e625992362b9b9c6bd05e0a27272' android/scripts/verify-browser.sh
grep -q 'packages.termux.dev/apt/termux-x11' android/scripts/verify-browser.sh
bash android/scripts/verify-browser.sh --help >/dev/null
bash android/scripts/smoke-browser.sh --help >/dev/null
bash -n android/scripts/verify-browser.sh
bash -n android/scripts/smoke-browser.sh
# bundle must remain free of Chromium (external apt install only)
bash android/scripts/verify-browser.sh --check-bundle ./omp-termux.tar.gz
! tar -tzf omp-termux.tar.gz | grep -qi chromium
! tar -tzf omp-termux.tar.gz | grep -q './chrome'
```

   `shellcheck` is not available on the runners, so `bash -n` and `--help`
   cover script syntax. On device, full validation also probes the installed
   Chromium (see [Device browser gate](#device-browser-gate-external-termuxx11-chromium)).

6. The release runs Android behavior tests on the applied patch queue:

```sh
# browser discovery runs the real probe fixture with a forced android platform;
# update refusal asserts no upstream fetch and non-zero exit.
bun test ./packages/coding-agent/test/tools/browser-android-chromium.test.ts
bun test ./packages/coding-agent/test/cli/update-cli.test.ts
bun test ./packages/coding-agent/test/termux-notifications.test.ts
# Termux copy/read argv + stdin, OSC 52 preserved, and loud failure when every
# clipboard channel is missing (no silent "Copied" success).
bun test ./packages/coding-agent/test/utils/clipboard.test.ts
# OSC 52 routing: worker thread and failed stdout write must not count as
# delivery; /copy reports a rejection and clears the draft.
bun test ./packages/coding-agent/test/utils/clipboard-osc52.test.ts
bun test ./packages/coding-agent/test/slash-commands/copy.test.ts
# gen-enums emits platform-gated native proxies: Android fails loud, desktop
# keeps the missing-export diagnostic (includes a real generator round-trip).
bun test ./packages/coding-agent/test/generator-android.test.ts
# Android memory hint uses MemAvailable when /proc/meminfo is readable and
# treats unreadable /sys/fs/cgroup files as no limit (physical RAM is only the
# last fallback when /proc/meminfo itself fails).
cargo test --profile local -p pi-builtins --lib sort::
# endpoint naming is pure/stable; relocation-directory validation covers
# creation, owner-mode repair, file/symlink leaves, and an untrusted base.
cargo test --profile local -p pi-natives --lib ipc::
# real bind/connect over a relocated socket, including a second process with a
# different TMPDIR that must compute and dial the same endpoint.
bun test ./packages/natives/test/ipc.test.ts
# pure Android naming policies (installed-root precedence, exact/overlong
# sun_path limits, multibyte roots) live in the ipc::endpoint unit tests above.
```

   `cargo test` and `bun test` run on the Linux CI host. They compile and
   execute the relocation logic, but the `target_os = "android"` branch only
   runs on the device build.

## Local verification

```sh
python3 android/scripts/verify-overlay.py
python3 android/scripts/check-release-inputs.py
# browser prerequisite metadata (no device needed)
bash android/scripts/verify-browser.sh --help
bash -n android/scripts/verify-browser.sh && echo "syntax OK"
bash -n android/scripts/smoke-browser.sh && echo "syntax OK"
# on-device full checks (require Termux aarch64 + chromium package):
bash android/scripts/verify-browser.sh
bash android/scripts/verify-browser.sh --check-bundle ./omp-termux.tar.gz
bash android/scripts/smoke-browser.sh --no-network --skip-daemon  # quick probe
# applied-tree behavior tests (requires workspace deps + cargo):
bun test ./packages/coding-agent/test/tools/browser-android-chromium.test.ts
bun test ./packages/coding-agent/test/cli/update-cli.test.ts
bun test ./packages/coding-agent/test/termux-notifications.test.ts
bun test ./packages/coding-agent/test/utils/clipboard.test.ts
bun test ./packages/coding-agent/test/utils/clipboard-osc52.test.ts
bun test ./packages/coding-agent/test/slash-commands/copy.test.ts
bun test ./packages/coding-agent/test/generator-android.test.ts
cargo test --profile local -p pi-builtins --lib sort::
cargo test --profile local -p pi-natives --lib ipc::
bun test ./packages/natives/test/ipc.test.ts
```

`verify-browser.sh` is **fail-closed**. An explicit error means:

- The `chromium` package is missing.
- `$PREFIX/lib/chromium/chrome` is missing.
- The version, architecture, or repository is wrong.
- The SHA256 does not match.
- The interpreter is not Bionic (`ld-linux`).
- The binary depends on `libc.so.6`.
- The ELF is not ARM64.

Use `--install` for an idempotent `pkg install -y x11-repo chromium`. If no
`.deb` is cached, use `--skip-deb-sha256` on CI only.

## Device smoke gate

After you install a release on Android ARM64 / Termux:

```sh
omp --version
bundled_bun="$PREFIX/lib/omp-termux/bun"
"$bundled_bun" --version
omp models --help
omp logs --help
omp stats --json
```

Then exercise one read tool and one bash tool, and launch interactive `omp`. Do
not call a release fully verified until `stats --json` and interactive startup
pass on device.

### Device browser gate (external Termux:X11 Chromium)

The browser tool has its own hard gate. It requires the pinned external
Chromium. If the package or the binary is absent, it **fails closed**. An
absent package is not a passing `SKIP` for local-browser acceptance.

```text
package:  chromium 149.0.7827.155 aarch64
source:   https://packages.termux.dev/apt/termux-x11 x11/main
deb:      chromium_149.0.7827.155_aarch64.deb
SHA256:   36500e23ad23bf2616bb4f215a297ba5a2b9e625992362b9b9c6bd05e0a27272
binary:   $PREFIX/lib/chromium/chrome
ELF:      ARM aarch64, Android API 24, Bionic /system/bin/linker64
```

`headless_shell` is **not** accepted. That binary rejects remote debugging. The
main `chrome` binary supports `--headless=new` with CDP.

Run:

```sh
# 1. prerequisite + ELF/Bionic/API + glibc rejection + version probe
bash android/scripts/verify-browser.sh

# 2. full smoke: headless launch, CDP attach, navigation, screenshots, daemon, rejection
bash android/scripts/smoke-browser.sh
#   equivalently, with more control:
bash android/scripts/smoke-browser.sh --no-network   # use data URL if offline
PUPPETEER_EXECUTABLE_PATH="$PREFIX/lib/chromium/chrome" "$PREFIX/lib/omp-termux/bun" "$PREFIX/lib/omp-termux/cli.js" --version  # OMP still runs

# 3. manual probes used by the scripts (for diagnosis):
file "$PREFIX/lib/chromium/chrome"
readelf -l "$PREFIX/lib/chromium/chrome" | grep -q '/system/bin/linker64'
readelf -d "$PREFIX/lib/chromium/chrome" | grep -q 'libc.so'
! readelf -d "$PREFIX/lib/chromium/chrome" | grep -q 'libc.so.6'
readelf -n "$PREFIX/lib/chromium/chrome" | grep -q 'NT_ANDROID_TYPE_IDENT'
"$PREFIX/lib/chromium/chrome" --version  # → Chromium 149.0.7827.155
"$PREFIX/lib/chromium/chrome" --headless=new --no-sandbox --disable-dev-shm-usage \
  --remote-debugging-address=127.0.0.1 --remote-debugging-port=0 \
  --dump-dom about:blank  # → <html>…

# 4. Chromium must NOT be inside the OMP bundle:
bash android/scripts/verify-browser.sh --check-bundle ./omp-termux.tar.gz
tar -tzf omp-termux.tar.gz | grep -qi chromium && echo "FAIL: bundled chromium" || echo "OK: no bundled chromium"
```

`smoke-browser.sh` covers:

1. The `chromium` version, architecture, and repository are
   `149.0.7827.155 aarch64` from Termux:X11.
2. The cached `.deb` SHA256 is `36500e23a…`, or the provenance gate fails.
3. ELF/Bionic/API validation: ARM64, `/system/bin/linker64`, API 24 from
   `.note.android.ident`, no `ld-linux`, no `libc.so.6`. The script does not verify the Chromium NDK version.
4. `"$PREFIX/lib/chromium/chrome" --version`.
5. Headless launch (`--headless=new --no-sandbox --disable-dev-shm-usage`).
6. Loopback CDP attach (`http://127.0.0.1:<ephemeral>/json/version` →
   `ws://127.0.0.1`).
7. Navigation to `https://example.com`, or to a data URL with `--no-network`.
8. Viewport screenshot.
9. Full-page screenshot.
10. Element screenshot.
11. Model `ImageContent` delivery (PNG signature + base64 length).
12. Shared daemon reuse.
13. Daemon restart after an executable or spec change.
14. Failed-binary rejection and rollback, with the prior daemon and install
    preserved, and **no** puppeteer-managed Linux download on Android.

CI on `x86_64` cannot replace this device gate. CI can only validate the
scripts and the metadata (`bash -n`, `--help`, `--skip-deb-sha256`,
`--check-bundle`).

A future Chromium upgrade requires a new pinned version, a new SHA256, the same
ELF/Bionic/API checks, and the same device smoke. The scripts and this doc are
the source of truth for the current pin.
