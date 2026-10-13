#!/usr/bin/env bash
# On-device native build for Termux (aarch64 / bionic).
#
# Compiles crates/pi-natives into packages/natives/native/pi_natives.android-arm64.node
# using the Termux-provided stable Rust toolchain (no NDK, no cross-compile — the
# host triple already is aarch64-linux-android). Thread count is capped low
# because a full-parallelism cargo build OOM-kills Termux on most devices.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

JOBS="${CARGO_BUILD_JOBS:-2}"
export CARGO_BUILD_JOBS="$JOBS"

echo "==> Termux native build (jobs=$JOBS)"

# --- preflight ---------------------------------------------------------------
for tool in cargo rustc bun; do
	if ! command -v "$tool" >/dev/null 2>&1; then
		echo "error: '$tool' not found. Install with: pkg install rust && (bun via npm or termux pkg)" >&2
		exit 1
	fi
done

host_triple="$(rustc -vV | awk '/^host:/ {print $2}')"
if [ "$host_triple" != "aarch64-linux-android" ]; then
	echo "warn: host triple is '$host_triple', expected aarch64-linux-android" >&2
	echo "      this script targets on-device Termux builds; use build-android.sh for cross-compile" >&2
fi

# --- apply deterministic patch queue -----------------------------------------
bash "$REPO_ROOT/android/scripts/apply-patches.sh"

# --- build -------------------------------------------------------------------
echo "==> Building pi-natives (release)"
bun --cwd=packages/natives run build

# Post-link release stamp: upstream oh-my-pi >= 18.3.x requires native addons
# to carry the package version in VERSION_STAMP.
if [ -f "$REPO_ROOT/scripts/stamp-native-version.ts" ]; then
	bun "$REPO_ROOT/scripts/stamp-native-version.ts" "packages/natives/native/pi_natives.android-arm64.node" --no-sign || true
fi

python3 - <<'PY'
import json, sys
from pathlib import Path

addon = Path("packages/natives/native/pi_natives.android-arm64.node")
pkg_file = Path("packages/natives/package.json")
if not addon.is_file() or not pkg_file.is_file():
    sys.exit(0)

version = json.loads(pkg_file.read_text(encoding="utf-8"))["version"]
magic = b"PI_NATIVES_VERSION_STAMP:"
expected = magic + version.encode("utf-8") + b"\x00"

data = addon.read_bytes()
if expected in data:
    print(f"    version stamp verified: {version}")
    sys.exit(0)

idx = data.find(magic)
if idx == -1:
    sys.exit(0)

slot_len = 64
payload = version.encode("utf-8")
if len(magic) + len(payload) >= slot_len:
    sys.exit(f"error: version {version} too long for stamp slot ({slot_len} bytes)")

replacement = magic + payload + b"\x00" * (slot_len - len(magic) - len(payload))
data = data[:idx] + replacement + data[idx + slot_len:]
addon.write_bytes(data)
PY

# --- verify ------------------------------------------------------------------
addon="packages/natives/native/pi_natives.android-arm64.node"
if [ ! -f "$addon" ]; then
	echo "error: expected addon not produced: $addon" >&2
	echo "       native/ contents:" >&2
	ls -la packages/natives/native/*.node 2>/dev/null >&2 || true
	exit 1
fi

file_type="$(file -b "$addon" 2>/dev/null || echo unknown)"
echo "==> Built: $addon"
echo "    $file_type"
case "$file_type" in
	*ELF*aarch64*) echo "    OK: aarch64 ELF shared object" ;;
	*) echo "    warn: unexpected file type" >&2 ;;
esac
