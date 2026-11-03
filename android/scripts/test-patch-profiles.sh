#!/usr/bin/env bash
# Lightweight tests for standard/extended patch profiles and release-input
# validation. No network, no package installs, no native/JS build.
set -euo pipefail

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd -P)
tmp_base=${TMPDIR:-${PREFIX:?set PREFIX or TMPDIR}/tmp}
root=$(mktemp -d "$tmp_base/omp-patch-profiles.XXXXXX")
trap 'rm -rf "$root"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

mkdir -p "$root/repo/android/scripts" "$root/repo/android/patches"
cp "$repo_root/android/scripts/apply-patches.sh" "$root/repo/android/scripts/"
git -C "$root/repo" init -q
git -C "$root/repo" config user.name Test
git -C "$root/repo" config user.email test@example.invalid
printf 'base\n' > "$root/repo/base.txt"

cat > "$root/repo/android/patches/01-one.patch" <<'PATCH'
diff --git a/one.txt b/one.txt
new file mode 100644
index 0000000..5626abf
--- /dev/null
+++ b/one.txt
@@ -0,0 +1 @@
+one
PATCH
cat > "$root/repo/android/patches/02-isolated-revive.patch" <<'PATCH'
diff --git a/two.txt b/two.txt
new file mode 100644
index 0000000..7d51b31
--- /dev/null
+++ b/two.txt
@@ -0,0 +1 @@
+two
PATCH
printf '01-one.patch\n02-isolated-revive.patch\n' > "$root/repo/android/patches/MANIFEST"
printf '01-one.patch\n' > "$root/repo/android/patches/MANIFEST.standard"
git -C "$root/repo" add base.txt android
git -C "$root/repo" commit -qm base

apply() { # profile
	(cd "$root/repo" && OMP_PATCH_PROFILE="$1" bash android/scripts/apply-patches.sh >/dev/null)
}
reset_repo() {
	rm -f "$root/repo/one.txt" "$root/repo/two.txt"
	git -C "$root/repo" checkout -q -- .
	git -C "$root/repo" clean -qfd
}

apply extended
[ -f "$root/repo/one.txt" ] || fail 'extended did not apply 01'
[ -f "$root/repo/two.txt" ] || fail 'extended did not apply 02'
pass 'extended profile applies 01+02'

if apply standard; then fail 'standard accepted an already-extended tree'; fi
pass 'standard rejects already-extended tree'

reset_repo
apply standard
[ -f "$root/repo/one.txt" ] || fail 'standard did not apply 01'
[ -e "$root/repo/two.txt" ] && fail 'standard applied 02'
pass 'standard profile applies 01 only'

apply standard
grep -q 'All patches already applied' <(cd "$root/repo" && OMP_PATCH_PROFILE=standard bash android/scripts/apply-patches.sh) \
	|| fail 'standard idempotence did not skip applied patches'
pass 'standard profile is idempotent'

if (cd "$root/repo" && OMP_PATCH_PROFILE=bogus bash android/scripts/apply-patches.sh >/dev/null 2>&1); then
	fail 'invalid profile accepted'
fi
pass 'invalid profile rejected'

printf '02-isolated-revive.patch\n' > "$root/repo/android/patches/MANIFEST.standard"
if (cd "$root/repo" && OMP_PATCH_PROFILE=standard bash android/scripts/apply-patches.sh >/dev/null 2>&1); then
	fail 'standard profile accepted 02'
fi
printf '01-one.patch\n' > "$root/repo/android/patches/MANIFEST.standard"
pass 'standard profile rejects patch 02'

# check-release-inputs.py profile/tag validation in a minimal release root.
release="$root/release"
mkdir -p "$release/android/scripts" "$release/android/patches" "$release/packages/natives/native"
cp "$repo_root/android/scripts/check-release-inputs.py" "$release/android/scripts/"
printf '{"version":"18.16.0"}\n' > "$release/package.json"
: > "$release/bun.lock"
printf 'BUN_VERSION=1\nBUN_ARCHIVE_NAME=x\nBUN_SHA256=y\n' > "$release/android/versions.env"
: > "$release/android/scripts/apply-patches.sh"
: > "$release/android/scripts/build-termux.sh"
: > "$release/packages/natives/native/loader-state.js"
cp "$root/repo/android/patches/01-one.patch" "$release/android/patches/"
cp "$root/repo/android/patches/02-isolated-revive.patch" "$release/android/patches/"
printf '01-one.patch\n02-isolated-revive.patch\n' > "$release/android/patches/MANIFEST"
printf '01-one.patch\n' > "$release/android/patches/MANIFEST.standard"

check() { # profile tag expected
	local profile=$1 tag=$2 want=$3
	set +e
	(cd "$release" && RELEASE_TAG="$tag" OMP_PATCH_PROFILE="$profile" \
		python3 android/scripts/check-release-inputs.py >/dev/null 2>&1)
	local got=$?
	set -e
	if [ "$want" = pass ] && [ "$got" -ne 0 ]; then fail "check-release rejected $profile/$tag"; fi
	if [ "$want" = fail ] && [ "$got" -eq 0 ]; then fail "check-release accepted $profile/$tag"; fi
}
check standard v18.16.0-termux pass
check standard v18.16.0-termux-r0123456789ab pass
check extended v18.16.0-termux-ex pass
check extended v18.16.0-termux-ex-r0123456789ab pass
check standard v18.16.0-termux-ex fail
check extended v18.16.0-termux fail
mkdir -p "$release/packages/coding-agent/test/task"
: > "$release/packages/coding-agent/test/task/isolation-retained.test.ts"
check standard v18.16.0-termux fail
rm -f "$release/packages/coding-agent/test/task/isolation-retained.test.ts"
pass 'release-input profile/tag matching and extended-file rejection'

# read-latest-tag.sh must distinguish 404 from auth/rate/network and reject
# malformed or empty latest tags.
ghstub="$root/gh"
mkdir -p "$ghstub"
cat > "$ghstub/gh" <<'SH'
#!/bin/sh
cat "${GH_FIXTURE:?}"
exit "${GH_RC:-0}"
SH
chmod 755 "$ghstub/gh"
latest_fixture() { # file status json
	{
		printf 'HTTP/2.0 %s\n\n' "$2"
		printf '%s\n' "$3"
	} > "$1"
}
latest() { # fixture rc
	PATH="$ghstub:$PATH" GH_FIXTURE="$1" GH_RC="$2" \
		bash "$repo_root/android/scripts/read-latest-tag.sh" SklUpp/omp-termux
}
latest_fixture "$root/latest-200" '200 OK' '{"tag_name":"v18.16.0-termux"}'
out=$(latest "$root/latest-200" 0) || fail 'valid latest lookup failed'
[ "$out" = v18.16.0-termux ] || fail "valid latest tag: $out"
latest_fixture "$root/latest-404" '404 Not Found' '{"message":"Not Found"}'
out=$(latest "$root/latest-404" 1) || fail '404 latest must be treated as no latest'
[ -z "$out" ] || fail "404 latest output: $out"
latest_fixture "$root/latest-403" '403 Forbidden' '{"message":"rate limited"}'
if latest "$root/latest-403" 1 >/dev/null; then fail '403 latest lookup accepted'; fi
latest_fixture "$root/latest-empty" '200 OK' '{"tag_name":""}'
if latest "$root/latest-empty" 0 >/dev/null; then fail 'empty latest tag accepted'; fi
latest_fixture "$root/latest-badjson" '200 OK' 'not-json'
if latest "$root/latest-badjson" 0 >/dev/null; then fail 'malformed latest response accepted'; fi
pass 'latest lookup distinguishes 404 and rejects auth/malformed/empty responses'

promote() { # candidate current expected
	local out
	out=$(python3 "$repo_root/android/scripts/should-promote-latest.py" "$1" "$2")
	[ "$out" = "$3" ] || fail "should-promote-latest $1 vs $2: expected $3, got $out"
}
promote v18.16.1-termux v18.16.0-termux true
promote v18.16.0-termux v18.16.1-termux false
promote v18.16.0-termux-r0123456789ab v18.16.0-termux true
promote v18.16.0-termux v18.16.0-termux-r421bcd8505e7 true
promote v18.16.0-termux v18.16.0-termux-ex true
promote v18.15.9-termux v18.16.0-termux-ex false
promote v18.16.1-termux v18.16.0-termux-ex true
promote v18.16.0-termux '' true
promote v18.16.0-termux-ex v18.16.0-termux false
promote v18.16.0-termux nonsense false
pass 'latest promotion stays monotonic and prefers standard over equal extended'

echo 'ALL PATCH PROFILE CHECKS PASSED'
