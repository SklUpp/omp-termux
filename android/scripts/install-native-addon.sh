#!/usr/bin/env bash
# Install pi_natives.android-arm64.node from the latest v*-termux release
# into the exact loader path without manual ambiguity.
#
# Source path (default): packages/natives/native/pi_natives.android-arm64.node
# Packaged runtime path:  $PREFIX/lib/omp-termux/node_modules/@oh-my-pi/pi-natives/native/pi_natives.android-arm64.node
#
# Usage:
#   bash android/scripts/install-native-addon.sh              # -> source path
#   bash android/scripts/install-native-addon.sh --bundle     # -> source + packaged runtime (if installed)
#   DEST=/custom/path/node bash android/scripts/install-native-addon.sh
#   OMP_REPO=owner/repo OMP_TAG=v18.0.7-termux bash android/scripts/install-native-addon.sh
set -euo pipefail

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)"
REPO_ROOT="$(CDPATH= cd -- "$SCRIPT_DIR/../.." && pwd -P)"
cd "$REPO_ROOT"

REPO="${OMP_REPO:-sasazemzulin058-debug/omp-termux}"
FILENAME="pi_natives.android-arm64.node"

# Resolve tag: OMP_TAG > RELEASE_TAG > OMP_VERSION (if not 'latest') > v<package-version>-termux
resolve_tag() {
  if [ -n "${OMP_TAG:-}" ]; then
    printf '%s' "$OMP_TAG"
    return
  fi
  if [ -n "${RELEASE_TAG:-}" ]; then
    printf '%s' "$RELEASE_TAG"
    return
  fi
  if [ -n "${OMP_VERSION:-}" ] && [ "$OMP_VERSION" != "latest" ]; then
    printf '%s' "$OMP_VERSION"
    return
  fi
  local ver
  ver="$(python3 -c 'import json; print(json.load(open("packages/coding-agent/package.json"))["version"])')"
  printf 'v%s-termux' "$ver"
}

TAG="$(resolve_tag)"
BASE="https://github.com/$REPO/releases/download/$TAG"
URL="$BASE/$FILENAME"
SHA_URL="$BASE/$FILENAME.sha256"

# Destination(s)
DEST_SOURCE="${DEST:-$REPO_ROOT/packages/natives/native/$FILENAME}"
INSTALL_BUNDLE=0
for arg in "$@"; do
  case "$arg" in
    --bundle) INSTALL_BUNDLE=1 ;;
    --help|-h)
      echo "Usage: $0 [--bundle] [--help]"
      echo "  Downloads $FILENAME ($TAG) from $REPO"
      echo "  Default dest: $DEST_SOURCE"
      echo "  --bundle also installs to \$PREFIX/lib/omp-termux/node_modules/@oh-my-pi/pi-natives/native/$FILENAME"
      echo "Env: OMP_REPO, OMP_TAG, RELEASE_TAG, OMP_VERSION, DEST, PREFIX"
      exit 0
      ;;
    *) echo "error: unknown arg: $arg" >&2; exit 1 ;;
  esac
done

PREFIX_DIR="${PREFIX:-/data/data/com.termux/files/usr}"
DEST_BUNDLE="$PREFIX_DIR/lib/omp-termux/node_modules/@oh-my-pi/pi-natives/native/$FILENAME"

echo "==> Fetching $FILENAME $TAG from $REPO"
echo "    $URL"
echo "    dest (source): $DEST_SOURCE"
if [ "$INSTALL_BUNDLE" -eq 1 ]; then
  echo "    dest (bundle): $DEST_BUNDLE"
fi

tmpdir="$(mktemp -d "${TMPDIR:-$PREFIX_DIR/tmp}/omp-addon.XXXXXX")"
cleanup() { rm -rf "$tmpdir"; }
trap cleanup EXIT INT TERM

curl -fL --retry 3 --retry-delay 2 "$URL" -o "$tmpdir/$FILENAME"
curl -fL --retry 3 --retry-delay 2 "$SHA_URL" -o "$tmpdir/$FILENAME.sha256"
echo "    verifying sha256"
(cd "$tmpdir" && sha256sum -c "$FILENAME.sha256")

# Verify ELF before install when `file` is available (Bionic check)
if command -v file >/dev/null 2>&1; then
  ftype="$(file -b "$tmpdir/$FILENAME" 2>/dev/null || echo unknown)"
  echo "    file: $ftype"
  case "$ftype" in
    *ELF*aarch64*) ;;
    *ELF*) echo "warn: unexpected ELF arch: $ftype" >&2 ;;
    *) echo "warn: unexpected file type: $ftype" >&2 ;;
  esac
fi

mkdir -p "$(dirname "$DEST_SOURCE")"
# atomic move via tmp + rename
cp "$tmpdir/$FILENAME" "$DEST_SOURCE.tmp"
mv "$DEST_SOURCE.tmp" "$DEST_SOURCE"
cp "$tmpdir/$FILENAME.sha256" "$DEST_SOURCE.sha256"
echo "==> Installed $DEST_SOURCE"
cat "$DEST_SOURCE.sha256"
ls -lh "$DEST_SOURCE" | awk '{print "    size:", $5, $9}'

if [ "$INSTALL_BUNDLE" -eq 1 ]; then
  if [ -d "$(dirname "$DEST_BUNDLE")" ] || [ -d "$PREFIX_DIR/lib/omp-termux" ]; then
    mkdir -p "$(dirname "$DEST_BUNDLE")"
    cp "$tmpdir/$FILENAME" "$DEST_BUNDLE.tmp"
    mv "$DEST_BUNDLE.tmp" "$DEST_BUNDLE"
    echo "==> Installed $DEST_BUNDLE"
    ls -lh "$DEST_BUNDLE" | awk '{print "    size:", $5, $9}'
  else
    echo "warn: bundle dir not found at $(dirname "$DEST_BUNDLE"); skipping bundle install (run install.sh first)" >&2
  fi
fi

# Quick loader sanity: filename must equal loader's expectation for android-arm64
if command -v bun >/dev/null 2>&1; then
  bun -e '
import {getAddonFilenames} from "./packages/natives/native/loader-state.js";
const got = getAddonFilenames({tag:"android-arm64", arch:"arm64", variant:null});
const exp = ["pi_natives.android-arm64.node"];
if (JSON.stringify(got) !== JSON.stringify(exp)) {
  console.error("loader filename mismatch: got", got, "expected", exp);
  process.exit(1);
}
console.log("loader check: android-arm64 ->", got[0], "OK");
' || echo "warn: loader sanity skipped (bun not available)" >&2
fi

echo "==> Done"
