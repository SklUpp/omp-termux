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
	printf 'BEGIN|%s|%s|%s\n' "$0" "${OMP_PLATFORM-}" "${LD_PRELOAD-}"
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
		"$@" sh "$src" > "$CASE/run.log" 2>&1
}

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
expected=$(printf 'BEGIN|%s|android|\nARG=%s\nARG=--version\nARG=two words' "$LIB_T/bun" "$LIB_T/cli.js")
[ "$(cat "$CASE/bun.log")" = "$expected" ] || die "launcher argv/platform:
$(cat "$CASE/bun.log")"
override="$CASE/override lib"
cp -a "$LIB_T" "$override"
: > "$CASE/bun.log"
env -u PREFIX -u LD_PRELOAD BUN_LOG="$CASE/bun.log" OMP_LIB_DIR="$override" \
	"$BIN_T/omp" --version 'two words' >/dev/null || die 'launcher runtime OMP_LIB_DIR override failed'
expected=$(printf 'BEGIN|%s|android|\nARG=%s\nARG=--version\nARG=two words' "$override/bun" "$override/cli.js")
[ "$(cat "$CASE/bun.log")" = "$expected" ] || die "launcher override argv:
$(cat "$CASE/bun.log")"
pass 'latest install, checksum URL, no browser packages, launcher path/args/platform/override'

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

# 8. browser opt-in installs packages in order
new_case browser
run_install "$assets/good-a" OMP_INSTALL_BROWSER=1 || { cat "$CASE/run.log"; die 'browser opt-in install failed'; }
[ "$(cat "$CASE/pkg.log")" = $'install -y x11-repo\ninstall -y chromium' ] \
	|| die "unexpected pkg calls: $(cat "$CASE/pkg.log")"
expect_version vA
pass 'OMP_INSTALL_BROWSER=1 installs x11-repo then chromium'

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
	expected=$(printf 'BEGIN|%s|android|%s\nARG=%s\nARG=--version\nARG=two words' \
		"$LIB_T/bun" "$LIB_T/libomp-compat.so" "$LIB_T/cli.js")
	[ "$(cat "$CASE/bun.log")" = "$expected" ] || die "launcher preload:
$(cat "$CASE/bun.log")"
	pass 'launcher preload'
else
	echo 'SKIP launcher preload (no usable preload library)'
fi

echo 'ALL INSTALL E2E CHECKS PASSED'
