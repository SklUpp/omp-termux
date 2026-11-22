#!/usr/bin/env bash
# Print the current GitHub latest release tag.
# Exit 0 with empty output when the repository has no latest release (HTTP 404).
# Exit nonzero for auth, rate-limit, network, malformed, or empty-tag responses.
set -euo pipefail

repo=${1:?usage: read-latest-tag.sh <owner/repo>}

set +e
response=$(gh api "repos/$repo/releases/latest" --include 2>&1)
rc=$?
set -e

status=$(printf '%s\n' "$response" | head -n 1)
if [ "$rc" -ne 0 ]; then
    if printf '%s\n' "$status" | grep -q ' 404 '; then
        exit 0
    fi
    echo "error: failed to query current latest release (status: ${status:-network error})" >&2
    exit 1
fi
if ! printf '%s\n' "$status" | grep -q ' 200 '; then
    echo "error: unexpected latest-release response (status: ${status:-empty})" >&2
    exit 1
fi

body=$(printf '%s\n' "$response" | awk 'seen { print } /^[[:space:]]*$/ { seen = 1 }')
[ -n "$body" ] || { echo "error: empty latest-release response body" >&2; exit 1; }
tag=$(printf '%s\n' "$body" | python3 -c '
import json, sys
try:
    print(json.load(sys.stdin)["tag_name"])
except Exception:
    print("error: malformed latest-release JSON", file=sys.stderr)
    sys.exit(1)
')
[ -n "$tag" ] || { echo "error: latest release has empty tag_name" >&2; exit 1; }
printf '%s\n' "$tag"
