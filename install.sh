#!/bin/sh
set -eu
unset LD_PRELOAD || true

# One-command Termux installer. CI publishes one self-contained app archive.
# Device installs only packaged Bun runtime; no bun install or source build.
# Browser is external: Termux:X11 chromium 149.0.7827.155 at $PREFIX/lib/chromium/chrome,
# validated by android/scripts/verify-browser.sh — OMP bundle never contains Chromium.

PREFIX_DIR="${PREFIX:-/data/data/com.termux/files/usr}"
BIN_DIR="${OMP_BIN_DIR:-$PREFIX_DIR/bin}"
LIB_DIR="${OMP_LIB_DIR:-$PREFIX_DIR/lib/omp-termux}"
REPO="${OMP_REPO:-SklUpp/omp-termux}"
TAG="${OMP_VERSION:-latest}"
# Empty means "not chosen yet": flags > environment > prompt > default.
EXTENDED=""
EXTENDED_EXPLICIT=0
BROWSER=""
BROWSER_ASKED=0

for arg in "$@"; do
    case "$arg" in
        -ex|--extended) EXTENDED=1; EXTENDED_EXPLICIT=1 ;;
        --standard) EXTENDED=0; EXTENDED_EXPLICIT=1 ;;
        --browser) BROWSER=1 ;;
        --no-browser) BROWSER=0 ;;
        --) ;;
        *) echo "error: unknown installer argument: $arg" >&2; exit 2 ;;
    esac
done

resolve_extended_tag() {
    page=1
    while [ "$page" -le 10 ]; do
        api="https://api.github.com/repos/$REPO/releases?per_page=100&page=$page"
        if ! json=$(curl -fsSL --retry 3 "$api"); then
            echo "error: failed to query GitHub releases (network, rate limit, or API error)" >&2
            return 2
        fi
        tags=$(printf '%s\n' "$json" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
        [ -n "$tags" ] || return 1
        for candidate in $tags; do
            if printf '%s\n' "$candidate" | grep -Eq -- '-termux-ex(-r[0-9a-f]{12})?$'; then
                printf '%s\n' "$candidate"
                return 0
            fi
        done
        page=$((page + 1))
    done
    return 1
}

case "$(uname -m)" in aarch64|arm64) ;; *) echo "error: Termux aarch64 required" >&2; exit 1 ;; esac
[ -n "${PREFIX:-}" ] || [ -n "${OMP_LIB_DIR:-}" ] || { echo 'error: run inside Termux' >&2; exit 1; }

pkg_deps=""
command -v curl >/dev/null 2>&1 || pkg_deps="$pkg_deps curl"
command -v tar >/dev/null 2>&1 || pkg_deps="$pkg_deps tar"
if [ -n "$pkg_deps" ]; then
    command -v pkg >/dev/null 2>&1 || { echo 'error: curl/tar missing and Termux pkg unavailable' >&2; exit 1; }
    pkg install -y $pkg_deps
fi

# Commands used directly by the install path and the generated launcher.
# curl/tar are handled above; these have no fallback, so fail early with a
# clear message instead of a confusing error mid-swap. flock is optional
# (mkdir fallback below).
missing=""
for dep in sha256sum sed grep find mktemp dirname env; do
    command -v "$dep" >/dev/null 2>&1 || missing="$missing $dep"
done
if [ -n "$missing" ]; then
    echo "error: missing required command(s):$missing (Termux: pkg install coreutils findutils)" >&2
    exit 1
fi

# Extended and Chromium are independent choices. Flags beat environment, the
# environment beats an interactive prompt. Non-terminals (`curl ... | sh`,
# redirected stdin) never read stdin: they keep the standard build and no
# browser, so a piped installer cannot hang waiting for an answer.
case "${OMP_EXTENDED:-}" in
    "") ;;
    0|1)
        if [ -z "$EXTENDED" ]; then
            EXTENDED=$OMP_EXTENDED
            EXTENDED_EXPLICIT=1
        fi
        ;;
    *) echo "error: OMP_EXTENDED must be 0 or 1" >&2; exit 2 ;;
esac
case "${OMP_INSTALL_BROWSER:-}" in
    "") ;;
    0|1) [ -n "$BROWSER" ] || BROWSER=$OMP_INSTALL_BROWSER ;;
    *) echo "error: OMP_INSTALL_BROWSER must be 0 or 1" >&2; exit 2 ;;
esac

# A pinned tag is authoritative: classify it once. Reject a suffix that looks
# extended but is malformed, and reject an explicit choice that disagrees with
# the pinned tag instead of silently installing another channel.
PINNED_EXTENDED=0
if [ "$TAG" != latest ]; then
    case "$TAG" in
        *-termux-ex|*-termux-ex-r[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]) PINNED_EXTENDED=1 ;;
        *-termux-ex-*) echo "error: malformed extended release tag (got '$TAG')" >&2; exit 2 ;;
    esac
fi
if [ "$EXTENDED_EXPLICIT" = 1 ] && [ "$EXTENDED" = 0 ] && [ "$PINNED_EXTENDED" = 1 ]; then
    echo "error: standard build requested with extended release tag '$TAG' (drop --standard/OMP_EXTENDED=0 or use -ex)" >&2
    exit 2
fi

ask() { # prompt default -> REPLY; EOF or any other answer takes the default
    printf '%s ' "$1" >&2
    if ! IFS= read -r REPLY; then
        REPLY=
        printf '\n' >&2
    fi
    case "$REPLY" in
        [Yy]|[Yy][Ee][Ss]) REPLY=1 ;;
        [Nn]|[Nn][Oo]) REPLY=0 ;;
        *) REPLY=$2 ;;
    esac
}

if [ -z "$EXTENDED" ]; then
    if [ "$TAG" != latest ]; then
        # Infer from the pinned tag so existing pinned installs never prompt
        # or change meaning.
        EXTENDED=$PINNED_EXTENDED
    elif [ -t 0 ]; then
        ask 'Install extended build (-ex)? [y/N]' 0
        EXTENDED=$REPLY
    else
        EXTENDED=0
    fi
fi
if [ -z "$BROWSER" ]; then
    if [ -t 0 ]; then
        ask 'Install optional Chromium? This also updates the shared libc++ runtime used by other apps. [y/N]' 0
        BROWSER=$REPLY
        BROWSER_ASKED=1
    else
        BROWSER=0
    fi
fi

# Resolve -ex only after curl is available. A pinned tag never calls the API.
if [ "$TAG" = latest ] && [ "$EXTENDED" = 1 ]; then
    if TAG=$(resolve_extended_tag); then
        :
    else
        rc=$?
        [ "$rc" -eq 2 ] || echo "error: no extended release found for $REPO" >&2
        exit 1
    fi
fi
if [ "$EXTENDED" = 1 ] && [ "$TAG" != latest ]; then
    case "$TAG" in
        *-termux-ex|*-termux-ex-r[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]) ;;
        *) echo "error: -ex requires an extended release tag (got '$TAG')" >&2; exit 2 ;;
    esac
fi

BASE="https://github.com/$REPO/releases"
[ "$TAG" = latest ] && BASE="$BASE/latest/download" || BASE="$BASE/download/$TAG"

# Chromium stays outside OMP bundle. Installing it also updates the shared
# libc++ runtime used by other Termux apps; warn and update it explicitly in
# the same choice. These transactions run before runtime staging/swap, so any
# failure aborts with the previous runtime untouched (set -e, no fallback).
if [ "$BROWSER" = 1 ]; then
    if [ "$BROWSER_ASKED" = 0 ]; then
        printf '%s\n' 'warning: installing Chromium also updates the shared libc++ runtime (affects other Termux apps)' >&2
    fi
    pkg install -y x11-repo
    pkg install -y libc++
    pkg install -y chromium
fi

mkdir -p "$BIN_DIR" "$(dirname "$LIB_DIR")"

STAGE_DIR="$LIB_DIR.new.$$"
BACKUP_DIR="$LIB_DIR.old.$$"
tmp="$(mktemp -d "${TMPDIR:-$PREFIX_DIR/tmp}/omp-install.XXXXXX")"
had_old=0
LOCK_DIR_OWNED=0
STAGE_OWNED=0

cleanup() {
    rm -rf "$tmp"
    if [ "$STAGE_OWNED" -eq 1 ]; then
        rm -rf "$STAGE_DIR"
    fi
    if [ "$had_old" -eq 1 ] && [ ! -d "$LIB_DIR" ] && [ -d "$BACKUP_DIR" ]; then
        mv "$BACKUP_DIR" "$LIB_DIR"
    fi
    if [ "$LOCK_DIR_OWNED" -eq 1 ]; then
        rmdir "$LOCK_DIR" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

# Serialize installs sharing a destination. flock is preferred; the mkdir
# fallback is atomic and fails closed on a stale lock instead of deleting it.
if command -v flock >/dev/null 2>&1; then
    exec 9>"$LIB_DIR.install.lock" || { echo "error: cannot open install lock" >&2; exit 1; }
    if ! flock -n 9; then
        echo "error: another omp-termux install is already in progress" >&2
        exit 1
    fi
else
    LOCK_DIR="$LIB_DIR.install.lock.d"
    if ! mkdir "$LOCK_DIR" 2>/dev/null; then
        echo "error: another omp-termux install is already in progress (or stale lock: $LOCK_DIR)" >&2
        exit 1
    fi
    LOCK_DIR_OWNED=1
fi

mkdir "$STAGE_DIR"
STAGE_OWNED=1
curl -fL --retry 3 "$BASE/omp-termux.tar.gz" -o "$tmp/omp-termux.tar.gz"
curl -fL --retry 3 "$BASE/omp-termux.tar.gz.sha256" -o "$tmp/omp-termux.tar.gz.sha256"
(cd "$tmp" && sha256sum -c omp-termux.tar.gz.sha256)
tar -xzf "$tmp/omp-termux.tar.gz" -C "$STAGE_DIR"
chmod +x "$STAGE_DIR/bun"

# Bundle Chromium-absence gate — OMP never bundles Chromium (external Termux:X11 package only).
# Fail closed before swap so previous install is preserved via rollback.
if [ -f "$STAGE_DIR/chrome" ] || [ -f "$STAGE_DIR/chromium" ] || [ -f "$STAGE_DIR/headless_shell" ]; then
    echo 'error: bundle unexpectedly contains Chromium binary — OMP bundle must remain free of Chromium' >&2
    exit 1
fi
# Also reject any stray chromium-named binaries at top level or under bin/lib
if find "$STAGE_DIR" -maxdepth 2 -type f \( -name "chrome" -o -name "chromium" -o -name "headless_shell" \) 2>/dev/null | grep -q .; then
    echo 'error: bundle contains Chromium executable — external Termux:X11 package only, not bundled' >&2
    find "$STAGE_DIR" -maxdepth 2 -type f \( -name "chrome" -o -name "chromium" -o -name "headless_shell" \) 2>&1 | head -n 5 >&2 || true
    exit 1
fi

# Pre-swap smoke test on .new
COMPAT_SO="$STAGE_DIR/libomp-compat.so"
FALLBACK_SO="$PREFIX_DIR/lib/libfix_close_range.so"
PRELOAD_PRE=""
if ! "$STAGE_DIR/bun" --version >/dev/null 2>&1; then
    if [ -f "$COMPAT_SO" ]; then
        PRELOAD_PRE="$COMPAT_SO"
    elif [ -f "$FALLBACK_SO" ]; then
        PRELOAD_PRE="$FALLBACK_SO"
    fi
fi

if [ -n "$PRELOAD_PRE" ]; then
    LD_PRELOAD="$PRELOAD_PRE" "$STAGE_DIR/bun" "$STAGE_DIR/cli.js" --version >/dev/null 2>&1 || {
        echo 'error: new OMP bundle failed smoke test' >&2
        exit 1
    }
else
    "$STAGE_DIR/bun" "$STAGE_DIR/cli.js" --version >/dev/null 2>&1 || {
        echo 'error: new OMP bundle failed smoke test' >&2
        exit 1
    }
fi

# Pre-swap native addon verification
if [ -f "$STAGE_DIR/node_modules/@oh-my-pi/pi-natives/native/loader-state.js" ]; then
    (cd "$STAGE_DIR" && env OMP_PLATFORM=android ${PRELOAD_PRE:+LD_PRELOAD="$PRELOAD_PRE"} "$STAGE_DIR/bun" -e \
        'import { loadNative } from "./node_modules/@oh-my-pi/pi-natives/native/loader-state.js"; loadNative();') >/dev/null 2>&1 || {
        echo 'error: new OMP native addon failed load test (check release version stamp)' >&2
        exit 1
    }
fi

# Guarded two-rename swap
if [ -d "$LIB_DIR" ]; then
    if [ -e "$BACKUP_DIR" ]; then
        echo "error: backup staging already exists: $BACKUP_DIR" >&2
        exit 1
    fi
    mv "$LIB_DIR" "$BACKUP_DIR"
    had_old=1
fi

if ! mv "$STAGE_DIR" "$LIB_DIR"; then
    echo 'error: swap failed, restoring previous installation' >&2
    if [ "$had_old" -eq 1 ]; then
        mv "$BACKUP_DIR" "$LIB_DIR"
    fi
    exit 1
fi

# Post-swap smoke test
POST_COMPAT_SO="$LIB_DIR/libomp-compat.so"
PRELOAD_POST=""
if ! "$LIB_DIR/bun" --version >/dev/null 2>&1; then
    if [ -f "$POST_COMPAT_SO" ]; then
        PRELOAD_POST="$POST_COMPAT_SO"
    elif [ -f "$FALLBACK_SO" ]; then
        PRELOAD_POST="$FALLBACK_SO"
    fi
fi

if [ -n "$PRELOAD_POST" ]; then
    LD_PRELOAD="$PRELOAD_POST" "$LIB_DIR/bun" "$LIB_DIR/cli.js" --version >/dev/null 2>&1 || {
        echo 'error: installed OMP bundle failed post-swap smoke test' >&2
        rm -rf "$LIB_DIR"
        if [ "$had_old" -eq 1 ]; then
            mv "$BACKUP_DIR" "$LIB_DIR"
        fi
        exit 1
    }
else
    "$LIB_DIR/bun" "$LIB_DIR/cli.js" --version >/dev/null 2>&1 || {
        echo 'error: installed OMP bundle failed post-swap smoke test' >&2
        rm -rf "$LIB_DIR"
        if [ "$had_old" -eq 1 ]; then
            mv "$BACKUP_DIR" "$LIB_DIR"
        fi
        exit 1
    }
fi

# Post-swap native addon verification
if [ -f "$LIB_DIR/node_modules/@oh-my-pi/pi-natives/native/loader-state.js" ]; then
    (cd "$LIB_DIR" && env OMP_PLATFORM=android ${PRELOAD_POST:+LD_PRELOAD="$PRELOAD_POST"} "$LIB_DIR/bun" -e \
        'import { loadNative } from "./node_modules/@oh-my-pi/pi-natives/native/loader-state.js"; loadNative();') >/dev/null 2>&1 || {
        echo 'error: installed OMP native addon failed load test (check release version stamp)' >&2
        rm -rf "$LIB_DIR"
        if [ "$had_old" -eq 1 ]; then
            mv "$BACKUP_DIR" "$LIB_DIR"
        fi
        exit 1
    }
fi
rm -f "$BIN_DIR/omp"
cat > "$BIN_DIR/omp" <<EOF
#!/bin/sh
PREFIX_DIR="\${PREFIX:-$PREFIX_DIR}"
# Install identity for native IPC endpoint naming: baked here, never derived
# from the caller's environment, so every process of this install names one
# socket even when PREFIX differs or is unset in the invoking shell.
OMP_ANDROID_PREFIX="$PREFIX_DIR"
LIB_DIR="\${OMP_LIB_DIR:-$LIB_DIR}"
COMPAT_SO="\$LIB_DIR/libomp-compat.so"
FALLBACK_SO="\$PREFIX_DIR/lib/libfix_close_range.so"

if [ -f "\$COMPAT_SO" ]; then
    export LD_PRELOAD="\$COMPAT_SO\${LD_PRELOAD:+:\$LD_PRELOAD}"
elif [ -f "\$FALLBACK_SO" ]; then
    export LD_PRELOAD="\$FALLBACK_SO\${LD_PRELOAD:+:\$LD_PRELOAD}"
fi
exec env OMP_PLATFORM=android OMP_ANDROID_PREFIX="\$OMP_ANDROID_PREFIX" "\$LIB_DIR/bun" "\$LIB_DIR/cli.js" "\$@"
EOF
chmod 755 "$BIN_DIR/omp"

if ! "$BIN_DIR/omp" --version >/dev/null 2>&1; then
    echo 'error: installed omp launcher failed verification' >&2
    rm -rf "$LIB_DIR"
    if [ "$had_old" -eq 1 ]; then
        mv "$BACKUP_DIR" "$LIB_DIR"
    fi
    exit 1
fi

if [ "$had_old" -eq 1 ]; then
    rm -rf "$BACKUP_DIR"
fi
had_old=0
printf '%s\n' 'Installed omp. Run: omp'
# Browser is external — not bundled (Termux:X11 chromium 149.0.7827.155 at $PREFIX/lib/chromium/chrome, verified by android/scripts/verify-browser.sh).
# If you have the omp-termux clone, run from repo root:
#   pkg install -y x11-repo
#   pkg install -y libc++
#   pkg install -y chromium
#   bash android/scripts/verify-browser.sh
#   bash android/scripts/smoke-browser.sh --no-network
# If you installed via curl | sh and have no clone, fetch verifiers from the release (published as separate assets, never bundled):
#   curl -fsSL "https://github.com/$REPO/releases/latest/download/verify-browser.sh" -o "$PREFIX/tmp/verify-browser.sh"
#   bash "$PREFIX/tmp/verify-browser.sh"
#   curl -fsSL "https://github.com/$REPO/releases/latest/download/smoke-browser.sh" -o "$PREFIX/tmp/smoke-browser.sh"
#   bash "$PREFIX/tmp/smoke-browser.sh" --no-network
printf '%s\n' 'Browser: for OMP browser tool, install external Chromium separately:'
printf '%s\n' '  pkg install -y x11-repo'
printf '%s\n' '  pkg install -y libc++'
printf '%s\n' '  pkg install -y chromium'
printf '%s\n' '  bash android/scripts/verify-browser.sh  # from clone, or fetch from release:'
printf '%s\n' "  curl -fsSL https://github.com/$REPO/releases/latest/download/verify-browser.sh -o \"\${TMPDIR:-$PREFIX_DIR/tmp}/verify-browser.sh\" && bash \"\${TMPDIR:-$PREFIX_DIR/tmp}/verify-browser.sh\""
