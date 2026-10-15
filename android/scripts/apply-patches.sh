#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
PATCHES_DIR="$SCRIPT_DIR/../patches"

cd "$ROOT_DIR"
echo "Applying Android patches from $PATCHES_DIR..."

# Fail closed when metadata is missing or manifest disagrees with active queue.
if [[ ! -d "$PATCHES_DIR" ]]; then
	echo "error: patches directory not found: $PATCHES_DIR" >&2
	exit 1
fi

shopt -s nullglob
patches=("$PATCHES_DIR"/[0-9][0-9]-*.patch)
shopt -u nullglob
if (( ${#patches[@]} == 0 )); then
	echo "error: no active patches found in $PATCHES_DIR" >&2
	exit 1
fi

if [[ ! -f "$PATCHES_DIR/MANIFEST" ]]; then
	echo "error: missing active patch MANIFEST" >&2
	exit 1
fi

manifest_patches=()
while IFS= read -r line || [[ -n "$line" ]]; do
	line="${line#"${line%%[![:space:]]*}"}"
	line="${line%"${line##*[![:space:]]}"}"
	[[ -z "$line" || "${line:0:1}" == "#" ]] && continue
	if [[ "$line" == */* || "$line" == *..* || "$line" != *.patch ]]; then
		echo "error: invalid MANIFEST entry: $line" >&2
		exit 1
	fi
	manifest_patches+=("$line")
done < "$PATCHES_DIR/MANIFEST"

if (( ${#manifest_patches[@]} != ${#patches[@]} )); then
	echo "error: MANIFEST count (${#manifest_patches[@]}) != found patches (${#patches[@]})" >&2
	exit 1
fi
for i in "${!patches[@]}"; do
	if [[ "$(basename "${patches[$i]}")" != "${manifest_patches[$i]}" ]]; then
		echo "error: patch order mismatch at index $i: expected ${manifest_patches[$i]}, got $(basename "${patches[$i]}")" >&2
		exit 1
	fi
done
echo "Patch order verified against MANIFEST (${#patches[@]} patches)"

# Disabled patches must stay outside active queue.
if compgen -G "$PATCHES_DIR"/*.patch.disabled > /dev/null; then
	echo "error: disabled patch found in active patches directory" >&2
	exit 1
fi

all_applied=false
probe_dir=$(mktemp -d "${TMPDIR:-/tmp}/android-patches.XXXXXX")
cleanup_probe() {
	rm -R "$probe_dir"
}
trap cleanup_probe EXIT
tar -C "$ROOT_DIR" --exclude=.git -cf - . | tar -C "$probe_dir" -xf -
all_applied=true
for ((i = ${#patches[@]} - 1; i >= 0; i--)); do
	if ! git -C "$probe_dir" apply --reverse --check --whitespace=nowarn "${patches[$i]}" >/dev/null 2>&1; then
		all_applied=false
		break
	fi
	git -C "$probe_dir" apply --reverse --whitespace=nowarn "${patches[$i]}"
done
cleanup_probe
trap - EXIT
if [[ "$all_applied" == true ]]; then
	echo "All patches already applied. (${#patches[@]} skipped)"
	exit 0
fi

# Mixed or partial state is unsafe: apply every patch in manifest order,
# checking each against tree produced by preceding patch.
for patch in "${patches[@]}"; do
	name=$(basename "$patch")
	echo "Checking $name..."
	git apply --check --whitespace=nowarn "$patch"
	echo "Applying $name..."
	git apply --whitespace=nowarn "$patch"
done

echo "All patches applied successfully. (${#patches[@]} applied, 0 skipped)"
