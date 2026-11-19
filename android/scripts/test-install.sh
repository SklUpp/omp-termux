#!/usr/bin/env bash
# Synthetic end-to-end regression for install.sh: URLs/checksums, upgrade,
# rollback paths, launcher contract. Stubs curl/pkg/uname; real tar/sha256sum.
# No network, no package manager, no native addon build.
# Usage: test-install.sh [path/to/install.sh]
# Temp root: $TMPDIR, else $PREFIX/tmp.
set -euo pipefail
src=${1:-$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd -P)/install.sh}
tmp_base=${TMPDIR:-${PREFIX:?set PREFIX or TMPDIR}/tmp}
root=$(mktemp -d "$tmp_base/omp-install-e2e.XXXXXX")
trap 'rm -rf "$root"' EXIT
base_path=$PATH
die() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }
ver() { cat "$LIB_T/version.txt" 2>/dev/null || echo '(none)'; }
expect_version() { [ "$(ver)" = "$1" ] || die "$CASE: version $(ver), want $1"; }
expect_absent() { [ ! -e "$1" ] || die "$CASE: unexpected $1"; }

# --- stubs ---------------------------------------------------------------
stub=$root/stub
mkdir -p "$stub"
cat > "$stub/uname" <<'SH'
#!/bin/sh
echo aarch64
SH
cat > "$stub/pkg" <<'SH'
#!/bin/sh
printf '%s\n' "$*" >> "${PKG_LOG:?}"
if [ -n "${PKG_FAIL:-}" ] && [ "$*" = "$PKG_FAIL" ]; then
	echo "stub pkg: simulated failure: $*" >&2
	exit 1
fi
exit 0
SH
cat > "$stub/curl" <<'SH'
#!/bin/sh
out= url=
while [ $# -gt 0 ]; do
	case $1 in
		-o) out=$2; shift 2 ;;
		-*) shift ;;
		*) url=$1; shift ;;
	esac
done
[ -n "$url" ] || { echo 'stub curl: no url' >&2; exit 2; }
printf '%s\n' "$url" >> "${CURL_LOG:?}"
case $url in
	https://api.github.com/*)
		[ "${CURL_API_FAIL:-0}" != 1 ] || { echo 'stub curl: api failure' >&2; exit 22; }
		api_json=${CURL_API_JSON:-}
		case $url in
			*page=2*) api_json=${CURL_API_JSON_PAGE2:-$api_json} ;;
		esac
		[ -n "$api_json" ] || { echo 'stub curl: missing CURL_API_JSON' >&2; exit 22; }
		if [ -n "$out" ]; then cp "$api_json" "$out"; else cat "$api_json"; fi
		exit 0
		;;
esac
[ -n "$out" ] || { echo 'stub curl: no -o' >&2; exit 2; }
src=$CURL_ASSET_DIR/$(basename "$url")
[ -f "$src" ] || { echo "stub curl: missing $src" >&2; exit 22; }
cp "$src" "$out"
SH
chmod 755 "$stub"/*

# --- fixtures ------------------------------------------------------------
make_bun() { # mode out
	local body
	case $1 in
		good) body='exit 0' ;;
		badpre) body='exit 1' ;;
		badpost) body='case "$0" in *.new*) exit 0 ;; *) exit 1 ;; esac' ;;
		badnative) body='has_e=0
for a in "$@"; do [ "$a" = "-e" ] && has_e=1; done
if [ "$has_e" = 1 ]; then
	case "$0" in *.new*) exit 0 ;; *) exit 1 ;; esac
fi
exit 0' ;;
		*) die "make_bun: unknown mode $1" ;;
	esac
	{
		cat <<'SH'
#!/bin/sh
# Logs one call per execution with each argv element separately.
# It never executes loader-state.js imports; mode only controls exit status.
{
	printf 'BEGIN|%s|%s|%s|%s\n' "$0" "${OMP_PLATFORM-}" "${LD_PRELOAD-}" "${OMP_ANDROID_PREFIX-}"
	for a in "$@"; do printf 'ARG=%s\n' "$a"; done
} >> "${BUN_LOG:?}"
SH
		printf '%s\n' "$body"
	} > "$2"
	chmod 755 "$2"
}

make_asset() { # dir version mode
	local dir=$1 v=$2 mode=$3
	rm -rf "$dir"
	mkdir -p "$dir/tree/node_modules/@oh-my-pi/pi-natives/native"
	printf '%s\n' "$v" > "$dir/tree/version.txt"
	make_bun "$mode" "$dir/tree/bun"
	printf '// fake cli\n' > "$dir/tree/cli.js"
	printf 'export function loadNative() {}\n' > "$dir/tree/node_modules/@oh-my-pi/pi-natives/native/loader-state.js"
	tar -czf "$dir/omp-termux.tar.gz" -C "$dir/tree" .
	(cd "$dir" && sha256sum omp-termux.tar.gz > omp-termux.tar.gz.sha256)
}

new_case() { # name [libdir]
	CASE=$root/$1
	PREFIX_T=$CASE/prefix
	LIB_T=${2:-$CASE/custom lib}
	BIN_T=$PREFIX_T/bin
	mkdir -p "$CASE/tmp" "$CASE/home"
}

run_install() { # asset_dir [VAR=value ...]
	local asset=$1
	shift
	: > "$CASE/curl.log"; : > "$CASE/pkg.log"; : > "$CASE/bun.log"
	env -i PATH="$stub:$base_path" HOME="$CASE/home" PREFIX="$PREFIX_T" \
		OMP_LIB_DIR="$LIB_T" TMPDIR="$CASE/tmp" BUN_LOG="$CASE/bun.log" \
		CURL_LOG="$CASE/curl.log" PKG_LOG="$CASE/pkg.log" CURL_ASSET_DIR="$asset" \
		CURL_API_JSON="${CURL_API_JSON:-}" CURL_API_JSON_PAGE2="${CURL_API_JSON_PAGE2:-}" \
		CURL_API_FAIL="${CURL_API_FAIL:-0}" PKG_FAIL="${PKG_FAIL:-}" \
		"$@" sh "$src" ${INSTALL_ARGS:-} < "${RUN_STDIN:-/dev/null}" > "$CASE/run.log" 2>&1
}

run_install_tty() { # answers asset [VAR=value ...]; answers fed through a real pty
	local answers=$1 asset=$2
	shift 2
	: > "$CASE/curl.log"; : > "$CASE/pkg.log"; : > "$CASE/bun.log"
	printf '%b' "$answers" | env -i PATH="$stub:$base_path" HOME="$CASE/home" PREFIX="$PREFIX_T" \
		OMP_LIB_DIR="$LIB_T" TMPDIR="$CASE/tmp" BUN_LOG="$CASE/bun.log" \
		CURL_LOG="$CASE/curl.log" PKG_LOG="$CASE/pkg.log" CURL_ASSET_DIR="$asset" \
		CURL_API_JSON="${CURL_API_JSON:-}" CURL_API_JSON_PAGE2="${CURL_API_JSON_PAGE2:-}" \
		CURL_API_FAIL="${CURL_API_FAIL:-0}" PKG_FAIL="${PKG_FAIL:-}" \
		"$@" script -qec "sh '$src' ${INSTALL_ARGS:-}" /dev/null > "$CASE/run.log" 2>&1
}

have_script=0
if command -v script >/dev/null 2>&1 &&
	script -qec ':' /dev/null < /dev/null > /dev/null 2>&1; then
	have_script=1
fi

# Chromium choice installs the external package and updates the shared libc++
# runtime in the same transaction sequence.
browser_pkgs=$'install -y x11-repo\ninstall -y libc++\ninstall -y chromium'

run_launcher() { # args...
	env -u PREFIX -u OMP_LIB_DIR -u LD_PRELOAD BUN_LOG="$CASE/bun.log" "$BIN_T/omp" "$@"
}

assets=$root/assets
make_asset "$assets/good-a" vA good
make_asset "$assets/good-b" vB good
make_asset "$assets/badpre" vB badpre
make_asset "$assets/badpost" vB badpost
make_asset "$assets/badnative" vB badnative
cp -a "$assets/good-a" "$assets/badsum"
{ printf '0%.0s' {1..64}; printf '  omp-termux.tar.gz\n'; } > "$assets/badsum/omp-termux.tar.gz.sha256"

api_extended=$root/api-extended.json
cat > "$api_extended" <<'JSON'
[
  {"tag_name":"v18.16.0-termux-r421bcd8505e7","prerelease":false},
  {"tag_name":"v18.16.0-termux-ex-r77f7e6811d20","prerelease":true}
]
JSON
api_missing=$root/api-missing.json
printf '[]\n' > "$api_missing"
api_page1=$root/api-page1.json
api_page2=$root/api-page2.json
printf '[{"tag_name":"v18.16.0-termux-r421bcd8505e7"}]\n' > "$api_page1"
printf '[{"tag_name":"v18.16.0-termux-ex"}]\n' > "$api_page2"

# 1. latest install: URL, checksum, no browser packages, launcher path/args/platform
new_case latest
run_install "$assets/good-a" || { cat "$CASE/run.log"; die 'latest install failed'; }
expect_version vA
grep -Fxq 'https://github.com/SklUpp/omp-termux/releases/latest/download/omp-termux.tar.gz' "$CASE/curl.log" || die 'latest tar URL'
grep -Fxq 'https://github.com/SklUpp/omp-termux/releases/latest/download/omp-termux.tar.gz.sha256' "$CASE/curl.log" || die 'latest sha URL'
[ ! -s "$CASE/pkg.log" ] || die "default install called pkg: $(cat "$CASE/pkg.log")"
expect_absent "$LIB_T.old"; expect_absent "$LIB_T.new"
: > "$CASE/bun.log"
out=$(run_launcher --version 'two words') || die 'launcher run failed'
[ "$out" = '' ] || die "launcher stdout: $out"
expected=$(printf 'BEGIN|%s|android||%s\nARG=%s\nARG=--version\nARG=two words' "$LIB_T/bun" "$PREFIX_T" "$LIB_T/cli.js")
[ "$(cat "$CASE/bun.log")" = "$expected" ] || die "launcher argv/platform:
$(cat "$CASE/bun.log")"
override="$CASE/override lib"
cp -a "$LIB_T" "$override"
: > "$CASE/bun.log"
env -u PREFIX -u LD_PRELOAD BUN_LOG="$CASE/bun.log" OMP_LIB_DIR="$override" \
	"$BIN_T/omp" --version 'two words' >/dev/null || die 'launcher runtime OMP_LIB_DIR override failed'
expected=$(printf 'BEGIN|%s|android||%s\nARG=%s\nARG=--version\nARG=two words' "$override/bun" "$PREFIX_T" "$override/cli.js")
[ "$(cat "$CASE/bun.log")" = "$expected" ] || die "launcher override argv:
$(cat "$CASE/bun.log")"
: > "$CASE/bun.log"
env -u LD_PRELOAD PREFIX="$CASE/elsewhere" BUN_LOG="$CASE/bun.log" \
	"$BIN_T/omp" --version >/dev/null \
	|| die 'launcher caller PREFIX override run failed'
expected=$(printf 'BEGIN|%s|android||%s\nARG=%s\nARG=--version' "$LIB_T/bun" "$PREFIX_T" "$LIB_T/cli.js")
[ "$(cat "$CASE/bun.log")" = "$expected" ] || die "launcher installed root followed caller PREFIX:
$(cat "$CASE/bun.log")"
pass 'latest install, checksum URL, no browser packages, launcher path/args/platform/installed-root'

# 2. pinned version URL
new_case version
run_install "$assets/good-a" OMP_VERSION=v18.16.0-termux || { cat "$CASE/run.log"; die 'version install failed'; }
expect_version vA
grep -Fxq 'https://github.com/SklUpp/omp-termux/releases/download/v18.16.0-termux/omp-termux.tar.gz' "$CASE/curl.log" || die 'version tar URL'
pass 'pinned version URL'

# 3. upgrade keeps previous .old cleanup
new_case upgrade
run_install "$assets/good-a" || die 'upgrade base install failed'
run_install "$assets/good-b" || die 'upgrade install failed'
expect_version vB
expect_absent "$LIB_T.old"; expect_absent "$LIB_T.new"
pass 'upgrade replaces install and removes backup'

# 4. bad checksum leaves previous install untouched
new_case badsum
run_install "$assets/good-a" || die 'badsum base install failed'
if run_install "$assets/badsum"; then die 'bad checksum accepted'; fi
expect_version vA
expect_absent "$LIB_T.new"; expect_absent "$LIB_T.old"
run_launcher --version >/dev/null || die 'badsum damaged previous launcher'
pass 'bad checksum preserves previous install'

# 5. pre-swap smoke failure keeps previous install
new_case badpre
run_install "$assets/good-a" || die 'badpre base install failed'
if run_install "$assets/badpre"; then die 'bad pre-swap bundle accepted'; fi
grep -q 'failed smoke test' "$CASE/run.log" || die 'pre-swap failure reason missing'
expect_version vA
expect_absent "$LIB_T.new"; expect_absent "$LIB_T.old"
run_launcher --version >/dev/null || die 'badpre damaged previous launcher'
pass 'pre-swap smoke rollback preserves previous install'

# 6. post-swap smoke failure restores previous install
new_case badpost
run_install "$assets/good-a" || die 'badpost base install failed'
if run_install "$assets/badpost"; then die 'bad post-swap bundle accepted'; fi
grep -q 'post-swap smoke test' "$CASE/run.log" || die 'post-swap failure reason missing'
expect_version vA
expect_absent "$LIB_T.new"; expect_absent "$LIB_T.old"
run_launcher --version >/dev/null || die 'badpost rollback did not restore launcher'
pass 'post-swap smoke rollback restores previous install'

# 7. native-loader post-swap failure restores previous install.
# Fake bun only reports argv/exit; the loader-state.js import is never executed,
# so this covers installer rollback plumbing, not native binary behavior.
new_case badnative
run_install "$assets/good-a" || die 'badnative base install failed'
if run_install "$assets/badnative"; then die 'native loader post-swap failure accepted'; fi
grep -q 'native addon failed load test' "$CASE/run.log" || die 'native post-swap failure reason missing'
expect_version vA
expect_absent "$LIB_T.new"; expect_absent "$LIB_T.old"
run_launcher --version >/dev/null || die 'badnative rollback did not restore launcher'
pass 'native-loader post-swap rollback restores previous install'

# 8. browser opt-in updates libc++ and installs packages in order
new_case browser
run_install "$assets/good-a" OMP_INSTALL_BROWSER=1 || { cat "$CASE/run.log"; die 'browser opt-in install failed'; }
[ "$(cat "$CASE/pkg.log")" = "$browser_pkgs" ] \
	|| die "unexpected pkg calls: $(cat "$CASE/pkg.log")"
grep -q 'shared libc++' "$CASE/run.log" || die 'browser opt-in warning missing'
expect_version vA
pass 'OMP_INSTALL_BROWSER=1 updates libc++, installs x11-repo then chromium'

# 8b. libc++ update failure aborts before the runtime swap
new_case browser-libcxx-fail
run_install "$assets/good-a" || die 'libc++-fail base install failed'
if run_install "$assets/good-b" OMP_INSTALL_BROWSER=1 PKG_FAIL='install -y libc++'; then
	die 'libc++ update failure accepted'
fi
grep -q 'stub pkg: simulated failure' "$CASE/run.log" || die 'pkg failure reason absent'
expect_version vA
expect_absent "$LIB_T.new"; expect_absent "$LIB_T.old"
run_launcher --version >/dev/null || die 'libc++ failure damaged previous launcher'
[ "$(cat "$CASE/pkg.log")" = $'install -y x11-repo\ninstall -y libc++' ] \
	|| die "libc++ failure continued past failure: $(cat "$CASE/pkg.log")"
pass 'libc++ update failure aborts before runtime swap'

# 9. optional launcher preload (space-free lib: LD_PRELOAD cannot name spaces).
preload=""
candidates=("${OMP_TEST_PRELOAD_LIB:-}" /system/lib64/libdl.so)
if command -v ldconfig >/dev/null 2>&1; then
	p=$(ldconfig -p 2>/dev/null | awk '/libdl\.so/{print $NF; exit}') || true
	candidates+=("$p")
fi
for c in "${candidates[@]}"; do
	if [ -n "$c" ] && [ -r "$c" ] && LD_PRELOAD=$c sh -c ':' 2>/dev/null; then
		preload=$c
		break
	fi
done
if [ -n "$preload" ]; then
	new_case preload "$root/lib-preload"
	make_asset "$assets/preload" vA good
	cp -L "$preload" "$assets/preload/tree/libomp-compat.so"
	(cd "$assets/preload" && tar -czf omp-termux.tar.gz -C tree . \
		&& sha256sum omp-termux.tar.gz > omp-termux.tar.gz.sha256)
	run_install "$assets/preload" || { cat "$CASE/run.log"; die 'preload install failed'; }
	: > "$CASE/bun.log"
	run_launcher --version 'two words' >/dev/null || die 'preload launcher run failed'
	expected=$(printf 'BEGIN|%s|android|%s|%s\nARG=%s\nARG=--version\nARG=two words' \
		"$LIB_T/bun" "$LIB_T/libomp-compat.so" "$PREFIX_T" "$LIB_T/cli.js")
	[ "$(cat "$CASE/bun.log")" = "$expected" ] || die "launcher preload:
$(cat "$CASE/bun.log")"
	pass 'launcher preload'
else
	echo 'SKIP launcher preload (no usable preload library)'
fi

# 10. -ex resolves latest extended release and installs the exact tag URL
new_case extended
INSTALL_ARGS=-ex CURL_API_JSON="$api_extended" run_install "$assets/good-a" \
	|| { cat "$CASE/run.log"; die '-ex extended latest install failed'; }
expect_version vA
grep -Fxq 'https://api.github.com/repos/SklUpp/omp-termux/releases?per_page=100&page=1' "$CASE/curl.log" \
	|| die 'extended API URL'
grep -Fxq 'https://github.com/SklUpp/omp-termux/releases/download/v18.16.0-termux-ex-r77f7e6811d20/omp-termux.tar.gz' "$CASE/curl.log" \
	|| die 'extended tag download URL'
! grep -q '/releases/latest/download/omp-termux.tar.gz' "$CASE/curl.log" \
	|| die 'extended install used latest standard'
pass '-ex resolves latest extended tag, exact download URL'

# 11. -ex missing extended does not fall back to standard latest
new_case extended-missing
run_install "$assets/good-a" || die 'extended-missing base install failed'
if INSTALL_ARGS=-ex CURL_API_JSON="$api_missing" run_install "$assets/good-a"; then
	die 'missing extended release accepted'
fi
grep -q 'no extended release found' "$CASE/run.log" || die 'missing extended reason absent'
expect_version vA
expect_absent "$LIB_T.new"; expect_absent "$LIB_T.old"
! grep -q '/releases/latest/download/omp-termux.tar.gz' "$CASE/curl.log" \
	|| die 'missing extended fell back to standard latest'
pass 'missing extended release fails closed without standard fallback'

# 12. -ex API failure does not fall back to standard latest
new_case extended-api-error
run_install "$assets/good-a" || die 'extended-api base install failed'
if INSTALL_ARGS=-ex CURL_API_FAIL=1 run_install "$assets/good-a"; then
	die 'extended API failure accepted'
fi
grep -q 'failed to query GitHub releases' "$CASE/run.log" || die 'API failure reason absent'
expect_version vA
! grep -q '/releases/latest/download/omp-termux.tar.gz' "$CASE/curl.log" \
	|| die 'API failure fell back to standard latest'
pass 'extended API failure fails closed without standard fallback'

# 13. pinned extended tag is exact; no release-list API call
new_case extended-pinned
INSTALL_ARGS=-ex run_install "$assets/good-a" OMP_VERSION=v18.16.0-termux-ex-r77f7e6811d20 \
	|| { cat "$CASE/run.log"; die 'pinned extended install failed'; }
expect_version vA
grep -Fxq 'https://github.com/SklUpp/omp-termux/releases/download/v18.16.0-termux-ex-r77f7e6811d20/omp-termux.tar.gz' "$CASE/curl.log" \
	|| die 'pinned extended exact URL'
! grep -q 'api.github.com' "$CASE/curl.log" || die 'pinned extended called release API'
pass 'pinned extended tag is exact and does not query latest'

# 14. pinned standard tag with -ex is rejected, not silently downgraded
new_case extended-pinned-mismatch
if INSTALL_ARGS=-ex run_install "$assets/good-a" OMP_VERSION=v18.16.0-termux; then
	die 'pinned standard tag accepted with -ex'
fi
grep -q 'requires an extended release tag' "$CASE/run.log" || die 'pinned -ex mismatch reason absent'
! grep -q 'api.github.com' "$CASE/curl.log" || die 'pinned -ex mismatch called API'
pass 'pinned standard tag with -ex fails closed'

# 15. stale/concurrent staging is not reused or deleted
new_case preexisting-stage
mkdir -p "$LIB_T.new"
printf 'preexisting\n' > "$LIB_T.new/keep.txt"
run_install "$assets/good-a" || { cat "$CASE/run.log"; die 'install with preexisting stage failed'; }
expect_version vA
[ "$(cat "$LIB_T.new/keep.txt" 2>/dev/null)" = preexisting ] || die 'preexisting staging was deleted or modified'
pass 'preexisting staging is preserved; unique stage is used'

# 16. destination lock rejects a concurrent installer
if command -v flock >/dev/null 2>&1; then
	new_case locked
	exec 8>"$LIB_T.install.lock"
	flock -n 8 || die 'could not hold test install lock'
	if run_install "$assets/good-a"; then
		exec 8>&-
		die 'concurrent install was not rejected'
	fi
	exec 8>&-
	grep -q 'already in progress' "$CASE/run.log" || die 'lock rejection reason absent'
	expect_absent "$LIB_T"
	pass 'concurrent install fails on destination lock'
else
	echo 'SKIP: flock unavailable'
fi

# 17. -ex follows release-list pagination to find an older extended tag
new_case extended-page2
INSTALL_ARGS=-ex CURL_API_JSON="$api_page1" CURL_API_JSON_PAGE2="$api_page2" run_install "$assets/good-a" \
	|| { cat "$CASE/run.log"; die 'extended page-2 install failed'; }
expect_version vA
grep -Fxq 'https://api.github.com/repos/SklUpp/omp-termux/releases?per_page=100&page=2' "$CASE/curl.log" \
	|| die 'extended API page 2 not requested'
grep -Fxq 'https://github.com/SklUpp/omp-termux/releases/download/v18.16.0-termux-ex/omp-termux.tar.gz' "$CASE/curl.log" \
	|| die 'extended page-2 tag URL'
pass '-ex follows API pagination'

# 18. piped non-terminal stdin keeps standard build and no browser
new_case noninteractive-default
printf 'y\ny\n' > "$CASE/answers"
RUN_STDIN="$CASE/answers" run_install "$assets/good-a" \
	|| { cat "$CASE/run.log"; die 'non-interactive install failed'; }
expect_version vA
grep -Fxq 'https://github.com/SklUpp/omp-termux/releases/latest/download/omp-termux.tar.gz' "$CASE/curl.log" \
	|| die 'piped stdin changed the standard release URL'
[ ! -s "$CASE/pkg.log" ] || die "piped stdin installed packages: $(cat "$CASE/pkg.log")"
pass 'piped non-terminal stdin keeps standard build and no browser'

# 19. OMP_EXTENDED=1 selects extended without a flag
new_case env-extended
run_install "$assets/good-a" OMP_EXTENDED=1 CURL_API_JSON="$api_extended" \
	|| { cat "$CASE/run.log"; die 'OMP_EXTENDED=1 install failed'; }
grep -Fxq 'https://github.com/SklUpp/omp-termux/releases/download/v18.16.0-termux-ex-r77f7e6811d20/omp-termux.tar.gz' "$CASE/curl.log" \
	|| die 'OMP_EXTENDED=1 did not select the extended tag'
pass 'OMP_EXTENDED=1 selects extended release'

# 20. explicit -ex flag overrides OMP_EXTENDED=0
new_case flag-extended
INSTALL_ARGS=-ex run_install "$assets/good-a" OMP_EXTENDED=0 CURL_API_JSON="$api_extended" \
	|| { cat "$CASE/run.log"; die '-ex with OMP_EXTENDED=0 failed'; }
grep -Fxq 'https://github.com/SklUpp/omp-termux/releases/download/v18.16.0-termux-ex-r77f7e6811d20/omp-termux.tar.gz' "$CASE/curl.log" \
	|| die '-ex did not override OMP_EXTENDED=0'
pass '-ex overrides OMP_EXTENDED=0'

# 21. explicit --standard flag overrides OMP_EXTENDED=1
new_case flag-standard
INSTALL_ARGS=--standard run_install "$assets/good-a" OMP_EXTENDED=1 CURL_API_JSON="$api_extended" \
	|| { cat "$CASE/run.log"; die '--standard with OMP_EXTENDED=1 failed'; }
grep -Fxq 'https://github.com/SklUpp/omp-termux/releases/latest/download/omp-termux.tar.gz' "$CASE/curl.log" \
	|| die '--standard did not override OMP_EXTENDED=1'
! grep -q 'api.github.com' "$CASE/curl.log" || die '--standard still queried the release API'
pass '--standard overrides OMP_EXTENDED=1'

# 22. browser flags override the environment in both directions
new_case browser-flag-on
INSTALL_ARGS=--browser run_install "$assets/good-a" OMP_INSTALL_BROWSER=0 \
	|| { cat "$CASE/run.log"; die '--browser with OMP_INSTALL_BROWSER=0 failed'; }
[ "$(cat "$CASE/pkg.log")" = "$browser_pkgs" ] \
	|| die "--browser packages: $(cat "$CASE/pkg.log")"
grep -q 'shared libc++' "$CASE/run.log" || die '--browser warning missing'
pass '--browser overrides OMP_INSTALL_BROWSER=0 and updates libc++'

new_case browser-flag-off
INSTALL_ARGS=--no-browser run_install "$assets/good-a" OMP_INSTALL_BROWSER=1 \
	|| { cat "$CASE/run.log"; die '--no-browser with OMP_INSTALL_BROWSER=1 failed'; }
[ ! -s "$CASE/pkg.log" ] || die "--no-browser installed packages: $(cat "$CASE/pkg.log")"
! grep -q 'shared libc++' "$CASE/run.log" || die '--no-browser printed the libc++ warning'
pass '--no-browser overrides OMP_INSTALL_BROWSER=1 and leaves libc++ alone'

new_case browser-env-off
run_install "$assets/good-a" OMP_INSTALL_BROWSER=0 \
	|| { cat "$CASE/run.log"; die 'OMP_INSTALL_BROWSER=0 install failed'; }
[ ! -s "$CASE/pkg.log" ] || die "OMP_INSTALL_BROWSER=0 installed packages: $(cat "$CASE/pkg.log")"
pass 'OMP_INSTALL_BROWSER=0 installs no browser packages'

# 23. pinned extended tag works without -ex, env, or prompt
new_case pinned-extended-implicit
run_install "$assets/good-a" OMP_VERSION=v18.16.0-termux-ex-r77f7e6811d20 \
	|| { cat "$CASE/run.log"; die 'implicit pinned extended install failed'; }
grep -Fxq 'https://github.com/SklUpp/omp-termux/releases/download/v18.16.0-termux-ex-r77f7e6811d20/omp-termux.tar.gz' "$CASE/curl.log" \
	|| die 'implicit pinned extended URL'
! grep -q 'api.github.com' "$CASE/curl.log" || die 'implicit pinned extended called the release API'
pass 'pinned extended tag installs without -ex'

# 24. invalid choice environment fails closed before installing
new_case bad-env-extended
if run_install "$assets/good-a" OMP_EXTENDED=2; then die 'OMP_EXTENDED=2 accepted'; fi
grep -q 'OMP_EXTENDED must be 0 or 1' "$CASE/run.log" || die 'OMP_EXTENDED validation message missing'
expect_absent "$LIB_T"
new_case bad-env-browser
if run_install "$assets/good-a" OMP_INSTALL_BROWSER=yes; then die 'OMP_INSTALL_BROWSER=yes accepted'; fi
grep -q 'OMP_INSTALL_BROWSER must be 0 or 1' "$CASE/run.log" || die 'OMP_INSTALL_BROWSER validation message missing'
expect_absent "$LIB_T"
pass 'invalid OMP_EXTENDED/OMP_INSTALL_BROWSER fail closed'

# 25. explicit standard choice conflicts with a pinned extended tag
new_case pinned-extended-standard-flag
if INSTALL_ARGS=--standard run_install "$assets/good-a" OMP_VERSION=v18.16.0-termux-ex-r77f7e6811d20; then
	die '--standard with pinned extended tag accepted'
fi
grep -q "standard build requested with extended release tag" "$CASE/run.log" \
	|| die "pinned mismatch reason absent: $(cat "$CASE/run.log")"
expect_absent "$LIB_T"
[ ! -s "$CASE/curl.log" ] || die 'pinned mismatch downloaded something'

new_case pinned-extended-standard-env
if run_install "$assets/good-a" OMP_VERSION=v18.16.0-termux-ex-r77f7e6811d20 OMP_EXTENDED=0; then
	die 'OMP_EXTENDED=0 with pinned extended tag accepted'
fi
grep -q "standard build requested with extended release tag" "$CASE/run.log" \
	|| die "pinned env mismatch reason absent: $(cat "$CASE/run.log")"
expect_absent "$LIB_T"
[ ! -s "$CASE/curl.log" ] || die 'pinned env mismatch downloaded something'
pass 'explicit standard choice rejects pinned extended tag'

# 26. malformed extended suffix fails closed instead of guessing a channel
new_case malformed-extended-tag
if run_install "$assets/good-a" OMP_VERSION=v18.16.0-termux-ex-r77f7e6811d2; then
	die 'malformed extended tag accepted'
fi
grep -q 'malformed extended release tag' "$CASE/run.log" \
	|| die "malformed tag reason absent: $(cat "$CASE/run.log")"
expect_absent "$LIB_T"
[ ! -s "$CASE/curl.log" ] || die 'malformed tag downloaded something'
pass 'malformed extended suffix fails closed'

# 27. missing prerequisite command fails before touching the install
new_case missing-prereq
limited=$CASE/bin
mkdir -p "$limited"
ln -s "$stub/uname" "$limited/uname"
for c in sh pkg curl tar sed grep find mktemp dirname sha256sum; do
	p=$(command -v "$c") || die "test lacks $c"
	ln -s "$p" "$limited/$c"
done
# 'env' is deliberately absent: it is required by the generated launcher.
if env -i PATH="$limited" HOME="$CASE/home" PREFIX="$PREFIX_T" \
	OMP_LIB_DIR="$LIB_T" TMPDIR="$CASE/tmp" sh "$src" > "$CASE/run.log" 2>&1; then
	die 'missing prerequisite accepted'
fi
grep -q 'missing required command(s): env' "$CASE/run.log" \
	|| die "prerequisite message: $(cat "$CASE/run.log")"
expect_absent "$LIB_T"
pass 'missing prerequisite fails before install with a clear message'

# 28. interactive terminal: each choice prompted separately, flags/env not needed
if [ "$have_script" = 1 ]; then
	new_case tty-both
	CURL_API_JSON="$api_extended" run_install_tty 'y\ny\n' "$assets/good-a" \
		|| { cat "$CASE/run.log"; die 'interactive both-yes install failed'; }
	grep -Fxq 'https://github.com/SklUpp/omp-termux/releases/download/v18.16.0-termux-ex-r77f7e6811d20/omp-termux.tar.gz' "$CASE/curl.log" \
		|| die 'interactive yes did not select extended'
	[ "$(cat "$CASE/pkg.log")" = "$browser_pkgs" ] \
		|| die "interactive yes did not update libc++/install browser: $(cat "$CASE/pkg.log")"
	grep -q 'shared libc++' "$CASE/run.log" || die 'interactive prompt lacks libc++ warning'
	pass 'interactive y,y selects extended, warns about libc++, updates libc++ and Chromium'

	new_case tty-declined
	run_install_tty 'n\nn\n' "$assets/good-a" \
		|| { cat "$CASE/run.log"; die 'interactive both-no install failed'; }
	grep -Fxq 'https://github.com/SklUpp/omp-termux/releases/latest/download/omp-termux.tar.gz' "$CASE/curl.log" \
		|| die 'interactive no did not keep standard release'
	[ ! -s "$CASE/pkg.log" ] || die "interactive no installed packages: $(cat "$CASE/pkg.log")"
	pass 'interactive n,n keeps standard and no browser'

	new_case tty-independent
	CURL_API_JSON="$api_extended" run_install_tty 'n\ny\n' "$assets/good-a" \
		|| { cat "$CASE/run.log"; die 'interactive mixed install failed'; }
	grep -Fxq 'https://github.com/SklUpp/omp-termux/releases/latest/download/omp-termux.tar.gz' "$CASE/curl.log" \
		|| die 'interactive mixed changed standard release'
	[ "$(cat "$CASE/pkg.log")" = "$browser_pkgs" ] \
		|| die "interactive mixed did not update libc++/install browser: $(cat "$CASE/pkg.log")"
	pass 'interactive choices are independent'

	new_case tty-eof
	# Two VEOF chars: tty EOF is per-read, so an empty pipe would block the
	# second prompt instead of reaching end-of-input.
	run_install_tty '\004\004' "$assets/good-a" \
		|| { cat "$CASE/run.log"; die 'interactive EOF install failed'; }
	grep -Fxq 'https://github.com/SklUpp/omp-termux/releases/latest/download/omp-termux.tar.gz' "$CASE/curl.log" \
		|| die 'interactive EOF changed standard release'
	[ ! -s "$CASE/pkg.log" ] || die "interactive EOF installed packages: $(cat "$CASE/pkg.log")"
	pass 'interactive EOF defaults to standard and no browser'

	new_case tty-invalid
	run_install_tty 'maybe\n\n' "$assets/good-a" \
		|| { cat "$CASE/run.log"; die 'interactive invalid-answer install failed'; }
	grep -Fxq 'https://github.com/SklUpp/omp-termux/releases/latest/download/omp-termux.tar.gz' "$CASE/curl.log" \
		|| die 'invalid interactive answers changed standard release'
	[ ! -s "$CASE/pkg.log" ] || die "invalid interactive answers installed packages: $(cat "$CASE/pkg.log")"
	pass 'interactive invalid/empty answers take defaults'
else
	echo 'SKIP interactive pty cases (script unavailable)'
fi

echo 'ALL INSTALL E2E CHECKS PASSED'
