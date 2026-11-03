#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
PATCHES_DIR="$SCRIPT_DIR/../patches"

cd "$ROOT_DIR"
profile="${OMP_PATCH_PROFILE:-extended}"
case "$profile" in
	standard|extended) ;;
	*)
		echo "error: OMP_PATCH_PROFILE must be standard or extended (got '$profile')" >&2
		exit 1
		;;
esac
if [[ "$profile" == standard ]]; then
	manifest="$PATCHES_DIR/MANIFEST.standard"
else
	manifest="$PATCHES_DIR/MANIFEST"
fi
echo "Applying Android patches from $PATCHES_DIR (profile: $profile)..."

# Fail closed when metadata is missing or manifest disagrees with active queue.
if [[ ! -d "$PATCHES_DIR" ]]; then
	echo "error: patches directory not found: $PATCHES_DIR" >&2
	exit 1
fi

if [[ ! -f "$manifest" ]]; then
	echo "error: missing $profile patch manifest: $manifest" >&2
	exit 1
fi

manifest_patches=()
while IFS= read -r line || [[ -n "$line" ]]; do
	line="${line#"${line%%[![:space:]]*}"}"
	line="${line%"${line##*[![:space:]]}"}"
	[[ -z "$line" || "${line:0:1}" == "#" ]] && continue
	if [[ "$line" == */* || "$line" == *..* || "$line" != *.patch ]]; then
		echo "error: invalid manifest entry: $line" >&2
		exit 1
	fi
	if [[ ! -f "$PATCHES_DIR/$line" ]]; then
		echo "error: manifest patch not found: $line" >&2
		exit 1
	fi
	manifest_patches+=("$line")
done < "$manifest"

if (( ${#manifest_patches[@]} == 0 )); then
	echo "error: no patches listed in $manifest" >&2
	exit 1
fi
if [[ "$profile" == standard ]]; then
	for name in "${manifest_patches[@]}"; do
		if [[ "$name" == "02-isolated-revive.patch" ]]; then
			echo "error: standard profile must not include 02-isolated-revive.patch" >&2
			exit 1
		fi
	done
fi

patches=()
for name in "${manifest_patches[@]}"; do
	patches+=("$PATCHES_DIR/$name")
done

# Extended remains the full active queue; standard is the manifest subset.
if [[ "$profile" == extended ]]; then
	shopt -s nullglob
	active_patches=("$PATCHES_DIR"/[0-9][0-9]-*.patch)
	shopt -u nullglob
	if (( ${#active_patches[@]} != ${#manifest_patches[@]} )); then
		echo "error: MANIFEST count (${#manifest_patches[@]}) != found patches (${#active_patches[@]})" >&2
		exit 1
	fi
	for i in "${!active_patches[@]}"; do
		if [[ "$(basename "${active_patches[$i]}")" != "${manifest_patches[$i]}" ]]; then
			echo "error: patch order mismatch at index $i: expected ${manifest_patches[$i]}, got $(basename "${active_patches[$i]}")" >&2
			exit 1
		fi
	done
fi
echo "Patch order verified against $(basename "$manifest") (${#patches[@]} patches)"

# Disabled patches must stay outside active queue.
if compgen -G "$PATCHES_DIR"/*.patch.disabled > /dev/null; then
	echo "error: disabled patch found in active patches directory" >&2
	exit 1
fi

# Standard must not silently accept a tree that already contains extended
# changes. Patch 02 is generated on top of 01, so reversing it is a reliable
# applied-tree probe without touching the working tree.
if [[ "$profile" == standard ]]; then
	for marker in \
		packages/coding-agent/test/cli/worktree-release-isolation.test.ts \
		packages/coding-agent/test/fixtures/isolation-lease-child.ts \
		packages/coding-agent/test/task/isolation-retained.test.ts; do
		if [[ -e "$ROOT_DIR/$marker" ]]; then
			echo "error: extended-only file present ($marker); standard profile requires a clean or standard tree" >&2
			exit 1
		fi
	done
fi
if [[ "$profile" == standard && -f "$PATCHES_DIR/02-isolated-revive.patch" ]]; then
	probe_dir=$(mktemp -d "${TMPDIR:-/tmp}/android-profile.XXXXXX")
	tar -C "$ROOT_DIR" --exclude=.git -cf - . | tar -C "$probe_dir" -xf -
	if git -C "$probe_dir" apply --reverse --check --whitespace=nowarn "$PATCHES_DIR/02-isolated-revive.patch" >/dev/null 2>&1; then
		rm -R "$probe_dir"
		echo "error: extended patch 02 is already applied; standard profile requires a clean or standard tree" >&2
		exit 1
	fi
	rm -R "$probe_dir"
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
