#!/usr/bin/env sh
set -eu

# Fast path: CI-built artifacts. Device only installs Termux's packaged Bun.
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/../.." && pwd -P)
if [ -f "$REPO_ROOT/install.sh" ]; then exec sh "$REPO_ROOT/install.sh" "$@"; fi
exec sh -c 'curl -fsSL https://raw.githubusercontent.com/sasazemzulin058-debug/omp-termux/main/install.sh | sh'
