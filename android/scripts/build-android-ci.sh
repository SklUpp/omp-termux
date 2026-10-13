#!/usr/bin/env bash
# Cross-compile pi-natives for aarch64-linux-android on a Linux x86_64 CI
# runner using the Android NDK directly (not cargo-zigbuild, which has no
# bionic libc support). Outputs pi_natives.android-arm64.node at the canonical
# location and verifies it.
#
# Required env:
#   ANDROID_NDK_ROOT — root of an NDK that ships aarch64-linux-android clang,
#                      e.g. r27 from nttld/setup-ndk
#
# Optional env:
#   CARGO_BUILD_JOBS — parallel cargo jobs (default 2)
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

: "${ANDROID_NDK_ROOT:?ANDROID_NDK_ROOT must be set to the NDK root}"

NATIVE_DIR="packages/natives/native"
ADDON="$NATIVE_DIR/pi_natives.android-arm64.node"
JOBS="${CARGO_BUILD_JOBS:-8}"
export CARGO_BUILD_JOBS="$JOBS"

ARCH_NAME="$(uname -m)"
OS_NAME="$(uname -s)"
case "$OS_NAME:$ARCH_NAME" in
	Linux:x86_64) NDK_HOST_TAG="linux-x86_64"; NDK_EXE_SUFFIX="" ;;
	Linux:aarch64|Linux:arm64) NDK_HOST_TAG="linux-aarch64"; NDK_EXE_SUFFIX="" ;;
	MINGW*:x86_64|MSYS*:x86_64|CYGWIN*:x86_64) NDK_HOST_TAG="windows-x86_64"; NDK_EXE_SUFFIX=".exe" ;;
	*) echo "error: unsupported host: $OS_NAME/$ARCH_NAME" >&2; exit 1 ;;
esac

NDK_BIN="$ANDROID_NDK_ROOT/toolchains/llvm/prebuilt/$NDK_HOST_TAG/bin"
NDK_CLANG="$NDK_BIN/aarch64-linux-android24-clang$NDK_EXE_SUFFIX"
NDK_CXX="$NDK_BIN/aarch64-linux-android24-clang++$NDK_EXE_SUFFIX"
NDK_AR="$NDK_BIN/llvm-ar$NDK_EXE_SUFFIX"
NDK_RANLIB="$NDK_BIN/llvm-ranlib$NDK_EXE_SUFFIX"
NDK_STRIP="$NDK_BIN/llvm-strip$NDK_EXE_SUFFIX"

for tool in "$NDK_CLANG" "$NDK_CXX" "$NDK_AR" "$NDK_RANLIB" "$NDK_STRIP"; do
	[ -x "$tool" ] || { echo "error: NDK tool not found: $tool" >&2; exit 1; }
done

# Preflight: compile a trivial C object with the selected NDK clang and verify
# it is aarch64. Catches host-compiler leakage (setup-ndk wrapper/PATH
# contamination or mis-set CC) before the expensive cargo build — audiopus_sys
# would otherwise emit x86_64 objects that fail late at link with
# "incompatible with aarch64linux".
echo "==> Preflight: verifying NDK clang emits aarch64 objects"
TMP_PREFLIGHT="$(mktemp -d)"
cat > "$TMP_PREFLIGHT/check.c" <<'CCEOF'
int preflight_check = 42;
CCEOF
"$NDK_CLANG" -c "$TMP_PREFLIGHT/check.c" -o "$TMP_PREFLIGHT/check.o"
if ! file "$TMP_PREFLIGHT/check.o" | grep -qi 'aarch64'; then
	echo "error: NDK clang did not emit aarch64 object: $(file "$TMP_PREFLIGHT/check.o")" >&2
	file "$TMP_PREFLIGHT/check.o" >&2 || true
	"$NDK_BIN/llvm-readelf" --file-header "$TMP_PREFLIGHT/check.o" 2>/dev/null | head -n 20 >&2 || true
	rm -rf "$TMP_PREFLIGHT"
	exit 1
fi
echo "    preflight OK: NDK clang emits aarch64 ($(file -b "$TMP_PREFLIGHT/check.o"))"
rm -rf "$TMP_PREFLIGHT"

# Point cargo + cc-rs + cmake at the NDK clang for the aarch64-linux-android
# target at API 24. `cc` reads CC_<target>; cargo reads
# CARGO_TARGET_<TARGET>_LINKER/_AR; cmake reads CMAKE_C_COMPILER etc.;
# ring/audiopus_sys also honour TARGET_CC/TARGET_CXX. Setting all
# deterministically from the NDK path (not from setup-ndk PATH wrappers)
# prevents CMake/cc from picking host gcc/clang and emitting x86_64
# objects that later fail to link (previous release failure: audiopus_sys
# objects incompatible with aarch64linux when linking with NDK clang).
NDK_BIN="$(dirname "$NDK_CLANG")"
# Do NOT create recursive symlinks on clang!
export PATH="$NDK_BIN:$PATH"
export CC="$NDK_CLANG"
export CXX="$NDK_CXX"
export CC_aarch64_linux_android="$NDK_CLANG"
export CXX_aarch64_linux_android="$NDK_CXX"
export AR_aarch64_linux_android="$NDK_AR"
export RANLIB_aarch64_linux_android="$NDK_RANLIB"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$NDK_CLANG"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_AR="$NDK_AR"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_RANLIB="$NDK_RANLIB"
# Deterministic CMake / cc-rs target toolchain (cmake crate propagates these
# to -DCMAKE_C_COMPILER / -DCMAKE_CXX_COMPILER / -DCMAKE_AR etc.; TARGET_*
# covers ring and other crates reading TARGET_CC).
export CMAKE_C_COMPILER="$NDK_CLANG"
export CMAKE_CXX_COMPILER="$NDK_CXX"
export CMAKE_AR="$NDK_AR"
export CMAKE_RANLIB="$NDK_RANLIB"
export CMAKE_STRIP="$NDK_STRIP"
export TARGET_CC="$NDK_CLANG"
export TARGET_CXX="$NDK_CXX"
export TARGET_AR="$NDK_AR"
export TARGET_RANLIB="$NDK_RANLIB"
export TARGET_STRIP="$NDK_STRIP"
# Per-target overrides that some cmake/cc invocations read (CMAKE_<VAR>_<triple>)
export CMAKE_C_COMPILER_aarch64_linux_android="$NDK_CLANG"
export CMAKE_CXX_COMPILER_aarch64_linux_android="$NDK_CXX"
export CMAKE_AR_aarch64_linux_android="$NDK_AR"
export CMAKE_RANLIB_aarch64_linux_android="$NDK_RANLIB"
export CFLAGS="-Os -g0 -fvisibility=hidden"
export CXXFLAGS="-Os -g0 -fvisibility=hidden"
export CFLAGS_aarch64_linux_android="-Os -g0 -fvisibility=hidden"
export CXXFLAGS_aarch64_linux_android="-Os -g0 -fvisibility=hidden"
export TARGET_CFLAGS="-Os -g0 -fvisibility=hidden"
export TARGET_CXXFLAGS="-Os -g0 -fvisibility=hidden"

# napi may inject its own Android NDK linker. Cargo config wins over that
# default and keeps all native objects on same NDK toolchain.
mkdir -p .cargo
CARGO_CONFIG_BACKUP=""
CARGO_MANIFEST_BACKUP="$(mktemp)"
cp Cargo.toml "$CARGO_MANIFEST_BACKUP"
TMP_DIR=""
restore_cargo_config() {
	if [ -n "$CARGO_CONFIG_BACKUP" ]; then cp "$CARGO_CONFIG_BACKUP" .cargo/config.toml; rm -f "$CARGO_CONFIG_BACKUP"; else rm -f .cargo/config.toml; fi
	cp "$CARGO_MANIFEST_BACKUP" Cargo.toml
	rm -f "$CARGO_MANIFEST_BACKUP"
	if [ -n "$TMP_DIR" ] && [ -d "$TMP_DIR" ]; then
		rm -rf "$TMP_DIR"
	fi
}
if [ -f .cargo/config.toml ]; then
	CARGO_CONFIG_BACKUP="$(mktemp)"
	cp .cargo/config.toml "$CARGO_CONFIG_BACKUP"
fi
trap restore_cargo_config EXIT
cat > .cargo/config.toml <<EOF
[target.aarch64-linux-android]
linker = "$NDK_CLANG"
ar = "$NDK_AR"
runner = "$NDK_CLANG"
EOF

# Locate the napi bin. The CLI is declared as a dev dependency in
# packages/natives, so `bun install` should hoist `node_modules/.bin/napi` to
# the repo root, but workspace hoisting varies; check several locations and
# fall back to a direct JS entry.
NAPI_BIN=""
for candidate in \
	"$REPO_ROOT/node_modules/.bin/napi" \
	"$REPO_ROOT/packages/natives/node_modules/.bin/napi"
do
	if [ -x "$candidate" ]; then
		NAPI_BIN="$candidate"
		break
	fi
done
if [ -z "$NAPI_BIN" ]; then
	NAPI_BIN="$(command -v napi 2>/dev/null || true)"
fi
# Last resort: invoke the JS entry directly via node.
if [ -z "$NAPI_BIN" ] || [ ! -x "$NAPI_BIN" ]; then
	ENTRY=""
	for c in \
		"$REPO_ROOT/node_modules/@napi-rs/cli/cli.mjs" \
		"$REPO_ROOT/node_modules/@napi-rs/cli/dist/cli.js" \
		"$REPO_ROOT/packages/natives/node_modules/@napi-rs/cli/cli.mjs"
	do
		if [ -f "$c" ]; then ENTRY="$c"; break; fi
	done
	if [ -n "$ENTRY" ] && command -v node >/dev/null 2>&1; then
		NAPI_BIN="node"
		# Use a wrapper: shift args to put the entry first.
		napi_entry="$ENTRY"
	fi
fi
if [ -z "$NAPI_BIN" ]; then
	echo "error: napi CLI not found in workspace node_modules" >&2
	echo "rerun 'bun install' from repo root" >&2
	exit 1
fi

echo "==> Cross-compiling pi-natives (aarch64-linux-android, jobs=$JOBS)"
echo "    NDK clang: $NDK_CLANG"
echo "    napi bin:  $NAPI_BIN"

mkdir -p "$NATIVE_DIR/.build"
TMP_DIR="$(mktemp -d "$NATIVE_DIR/.build/cross-XXXXXX")"
# Memory-safe Android overrides on existing workspace [profile.ci].
# Do NOT redefine [profile.ci] (duplicate key breaks cargo).
python3 - <<'PY'
from pathlib import Path
p = Path("Cargo.toml")
text = p.read_text()
# Force low-memory keys on existing [profile.ci] block until next [profile.
import re
m = re.search(r"(?ms)^\[profile\.ci\]\n(.*?)(?=^\[|\Z)", text)
if not m:
    raise SystemExit("missing [profile.ci] in Cargo.toml")
body = m.group(1)
replacements = {
    # Inherit release (not dev): dev defaults CGU=256 and package.* opt=2.
    # cargo#17205: CGU too low also OOMs (one huge unit). Keep moderate CGU
    # and serialize via cargo -j1 + RAYON_NUM_THREADS=1 so units run one-by-one.
    "inherits": 'inherits = "release"',
    "lto": "lto = false",
    "codegen-units": "codegen-units = 256",
    "debug": "debug = false",
    "opt-level": 'opt-level = 3',
    "incremental": "incremental = true",
    "strip": 'strip = "symbols"',
    "panic": 'panic = "abort"',
}
lines = []
seen = set()
for line in body.splitlines(True):
    key = line.split("=", 1)[0].strip() if "=" in line else ""
    if key in replacements:
        lines.append(replacements[key] + "\n")
        seen.add(key)
    else:
        lines.append(line)
for key, val in replacements.items():
    if key not in seen:
        lines.append(val + "\n")
new_body = "".join(lines)
text = text[: m.start(1)] + new_body + text[m.end(1) :]
if '[profile.ci.package."*"]' not in text:
    text += """

[profile.ci.package."*"]
opt-level = 3
debug = false
codegen-units = 256
"""
p.write_text(text)
print("patched [profile.ci] for low-memory Android build")
PY

# Cap C/C++/cmake/ninja + rustc frontend parallelism. Cargo -j1 alone does not
# constrain ring/opus cmake or rayon inside rustc; those still OOM the runner.
# CGU=16 + RAYON_NUM_THREADS=1: smaller units, processed serially (lower peak
# than CGU=1 which puts the whole crate in one LLVM unit).
export MAKEFLAGS="${MAKEFLAGS:--j2}"
export CMAKE_BUILD_PARALLEL_LEVEL="${CMAKE_BUILD_PARALLEL_LEVEL:-2}"
export NINJAFLAGS="${NINJAFLAGS:--j2}"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
export NUM_JOBS="${NUM_JOBS:-2}"
export RAYON_NUM_THREADS="${RAYON_NUM_THREADS:-2}"
export CARGO_BUILD_PIPELINING=true
if command -v sccache >/dev/null 2>&1; then
	export RUSTC_WRAPPER=sccache
fi
export RUSTFLAGS="${RUSTFLAGS:--C opt-level=3 -C debuginfo=0 -C strip=symbols -C linker-plugin-lto=no -C link-arg=-fuse-ld=lld}"
echo "    MAKEFLAGS=$MAKEFLAGS RAYON_NUM_THREADS=$RAYON_NUM_THREADS RUSTFLAGS=$RUSTFLAGS"
echo "    memory before cargo:"
free -h || true
swapon --show || true
trap restore_cargo_config EXIT
# Build with cargo directly. napi injects runner NDK r29 linker into target
# builds, which creates incompatible Opus objects. Cargo config above keeps all
# C/C++/Rust objects on selected NDK r27.
set +e
cargo build --manifest-path crates/pi-natives/Cargo.toml \
	--target aarch64-linux-android --profile ci --locked -j "$CARGO_BUILD_JOBS"
CARGO_RC=$?
set -e
if [ "$CARGO_RC" -ne 0 ]; then
  echo "error: cargo exited $CARGO_RC" >&2
  free -h >&2 || true
  dmesg -T 2>/dev/null | tail -n 40 >&2 || true
  exit "$CARGO_RC"
fi

BUILT="$REPO_ROOT/target/aarch64-linux-android/ci/libpi_natives.so"
cp "$BUILT" "$TMP_DIR/pi_natives.android-arm64.node"
BUILT="$TMP_DIR/pi_natives.android-arm64.node"
if [ ! -f "$BUILT" ]; then
	echo "error: cargo build did not produce $BUILT" >&2
	ls -la "$TMP_DIR" >&2
	exit 1
fi

# Strip in place (NDK debug symbols can bloat the .node to 100+ MB) and copy
# to the canonical filename.
"$NDK_STRIP" --strip-unneeded "$BUILT" 2>/dev/null || true
cp "$BUILT" "$ADDON"

# Verify
file_type="$(file -b "$ADDON")"
echo "==> Built: $ADDON"
echo "    $file_type"
case "$file_type" in
	*ELF*aarch64*) echo "    OK: aarch64 ELF shared object" ;;
	*) echo "error: unexpected file type: $file_type" >&2; exit 1 ;;
esac

# Emit sha256
(cd "$NATIVE_DIR" && sha256sum "$(basename "$ADDON")" > "$(basename "$ADDON").sha256")
echo "    sha256: $(cat "$ADDON.sha256")"
echo "==> Done"
