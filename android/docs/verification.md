# Verification Procedures

## Automated CI checks

1. `python3 android/scripts/verify-overlay.py` checks Android overlay markers.
2. `python3 android/scripts/check-release-inputs.py` checks package metadata and official Bun manifest fields.
3. Release downloads `BUN_ARCHIVE_NAME` from the official Bun release URL, verifies `BUN_SHA256`, extracts executable `bun`, and checks aarch64 ELF architecture.
4. Release checks runtime version and package contents:

```sh
sha256sum -c omp-termux.tar.gz.sha256
tar -tzf omp-termux.tar.gz | grep -Fx './cli.js'
tar -tzf omp-termux.tar.gz | grep -Fx './bun'
tar -tzf omp-termux.tar.gz | grep -F './node_modules/@oh-my-pi/pi-natives/native/pi_natives.android-arm64.node'
```

5. Release verifies the OMP bundle **does not bundle Chromium** and browser pin is intact:

```sh
# browser pin metadata (fail closed on mismatch)
grep -q '149.0.7827.155' android/scripts/verify-browser.sh
grep -q '36500e23ad23bf2616bb4f215a297ba5a2b9e625992362b9b9c6bd05e0a27272' android/scripts/verify-browser.sh
grep -q 'packages.termux.dev/apt/termux-x11' android/scripts/verify-browser.sh
bash android/scripts/verify-browser.sh --help >/dev/null
bash -n android/scripts/verify-browser.sh
bash -n android/scripts/smoke-browser.sh
# bundle must remain free of Chromium (external apt install only)
bash android/scripts/verify-browser.sh --check-bundle ./omp-termux.tar.gz
! tar -tzf omp-termux.tar.gz | grep -qi chromium
! tar -tzf omp-termux.tar.gz | grep -q './chrome'
```

6. Shellcheck-like syntax check for browser scripts where `shellcheck` unavailable:

```sh
bash -n android/scripts/verify-browser.sh
bash -n android/scripts/smoke-browser.sh
bash android/scripts/verify-browser.sh --help
bash android/scripts/smoke-browser.sh --help
```

   On device, full validation also probes the installed Chromium (see Device browser gate).

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
```

`verify-browser.sh` is **fail-closed**: missing `chromium` package, missing
`$PREFIX/lib/chromium/chrome`, wrong version/arch/repo, SHA256 mismatch,
non-Bionic interpreter (`ld-linux`), `libc.so.6` dependency, or non-ARM64 ELF
all produce an explicit error. Use `--install` for idempotent
`pkg install -y x11-repo chromium`, and `--skip-deb-sha256` only on CI without
a cached `.deb`.

## Device smoke gate

After installing a release on Android ARM64 / Termux:

```sh
omp --version
bundled_bun="$PREFIX/lib/omp-termux/bun"
"$bundled_bun" --version
omp models --help
omp logs --help
omp stats --json
```

Then exercise one read tool and one bash tool, and launch interactive `omp`. Do not call a release fully verified until `stats --json` and interactive startup pass on device.

### Device browser gate (external Termux:X11 Chromium)

The browser tool has its own hard gate. It requires the pinned external
Chromium and **fails closed** if the package/binary is absent — it is not a
passing `SKIP` for local-browser acceptance.

```text
package:  chromium 149.0.7827.155 aarch64
source:   https://packages.termux.dev/apt/termux-x11 x11/main
deb:      chromium_149.0.7827.155_aarch64.deb
SHA256:   36500e23ad23bf2616bb4f215a297ba5a2b9e625992362b9b9c6bd05e0a27272
binary:   $PREFIX/lib/chromium/chrome
ELF:      ARM aarch64, Android API 24, Bionic /system/bin/linker64
```

`headless_shell` is **not** accepted — that binary rejects remote debugging
while the main `chrome` binary supports `--headless=new` with CDP.

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

1. `chromium` version/arch/repo is `149.0.7827.155 aarch64` from Termux:X11;
2. cached `.deb` SHA256 is `36500e23a…` (or fails the provenance gate);
3. ELF/Bionic/API validation (ARM64, `/system/bin/linker64`, API 24, NDK r29, no `ld-linux`, no `libc.so.6`);
4. `"$PREFIX/lib/chromium/chrome" --version`;
5. headless launch (`--headless=new --no-sandbox --disable-dev-shm-usage`);
6. loopback CDP attach (`http://127.0.0.1:<ephemeral>/json/version` → `ws://127.0.0.1`);
7. navigation to `https://example.com` (or data URL with `--no-network`);
8. viewport screenshot;
9. full-page screenshot;
10. element screenshot;
11. model `ImageContent` delivery (PNG signature + base64 length);
12. shared daemon reuse;
13. daemon restart after executable/spec change;
14. failed-binary rejection/rollback with prior daemon/install preserved, and **no**
    puppeteer-managed Linux download on Android.

CI on `x86_64` cannot replace this device gate. It may only validate scripts
and metadata (`bash -n`, `--help`, `--skip-deb-sha256`, `--check-bundle`).

Future Chromium upgrades require a new pinned version + new SHA256 + same
ELF/Bionic/API checks + same device smoke. The scripts and this doc are the
source of truth for the current pin.
