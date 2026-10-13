#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
UPSTREAM_URL="${OFFICIAL_UPSTREAM_URL:-https://github.com/can1357/oh-my-pi.git}"
PIN_FILE="$ROOT_DIR/android/UPSTREAM_COMMIT"
METADATA_ARCHIVE="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/android-metadata-${RANDOM}.tar.gz"

cd "$ROOT_DIR"

if [[ ! -f "$PIN_FILE" ]]; then
	echo "error: missing recorded upstream commit: $PIN_FILE" >&2
	exit 1
fi
upstream_commit=$(tr -d '[:space:]' < "$PIN_FILE")
if [[ ! "$upstream_commit" =~ ^[0-9a-fA-F]{40}$ ]]; then
	echo "error: invalid upstream commit in $PIN_FILE: $upstream_commit" >&2
	exit 1
fi

metadata_dir="$(dirname "$METADATA_ARCHIVE")"
mkdir -p "$metadata_dir"
tar -C "$ROOT_DIR" -czf "$METADATA_ARCHIVE" android .github
cleanup() {
	rm -f "$METADATA_ARCHIVE"
}
trap cleanup EXIT

git remote add official "$UPSTREAM_URL" 2>/dev/null || git remote set-url official "$UPSTREAM_URL"
git fetch --no-tags official main
if ! git cat-file -e "$upstream_commit^{commit}" 2>/dev/null; then
	echo "error: recorded upstream commit is not reachable: $upstream_commit" >&2
	exit 1
fi
if ! git merge-base --is-ancestor "$upstream_commit" official/main 2>/dev/null; then
	echo "error: recorded upstream commit is not an ancestor of official/main: $upstream_commit" >&2
	exit 1
fi

# Replace checked-out fork source with exact recorded official source. Metadata
# remains outside upstream and is restored after clean removes downstream files.
git reset --hard "$upstream_commit"
git clean -ffdx
tar -C "$ROOT_DIR" -xzf "$METADATA_ARCHIVE"

echo "Prepared clean upstream tree at $upstream_commit"
bash "$ROOT_DIR/android/scripts/apply-patches.sh"
