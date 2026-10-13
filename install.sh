#!/bin/sh
set -eu

# One-command Termux installer. CI publishes one self-contained app archive.
# Device installs only packaged Bun runtime; no bun install or source build.
# Browser is external: Termux:X11 chromium 149.0.7827.155 at $PREFIX/lib/chromium/chrome,
# validated by android/scripts/verify-browser.sh — OMP bundle never contains Chromium.

PREFIX_DIR="${PREFIX:-/data/data/com.termux/files/usr}"
BIN_DIR="${OMP_BIN_DIR:-$PREFIX_DIR/bin}"
LIB_DIR="${OMP_LIB_DIR:-$PREFIX_DIR/lib/omp-termux}"
REPO="${OMP_REPO:-sasazemzulin058-debug/omp-termux}"
TAG="${OMP_VERSION:-latest}"
BASE="https://github.com/$REPO/releases"
[ "$TAG" = latest ] && BASE="$BASE/latest/download" || BASE="$BASE/download/$TAG"

case "$(uname -m)" in aarch64|arm64) ;; *) echo "error: Termux aarch64 required" >&2; exit 1 ;; esac
[ -n "${PREFIX:-}" ] || [ -n "${OMP_LIB_DIR:-}" ] || { echo 'error: run inside Termux' >&2; exit 1; }

pkg_deps=""
command -v curl >/dev/null 2>&1 || pkg_deps="$pkg_deps curl"
command -v tar >/dev/null 2>&1 || pkg_deps="$pkg_deps tar"
if [ -n "$pkg_deps" ]; then
    pkg install -y $pkg_deps
fi

# Chromium stays outside OMP bundle. Install Termux:X11 package in separate
# transactions so repository setup completes before Chromium resolution.
if [ "${OMP_INSTALL_BROWSER:-1}" != "0" ]; then
    pkg install -y x11-repo
    pkg install -y chromium
fi

mkdir -p "$BIN_DIR" "$(dirname "$LIB_DIR")"
tmp="$(mktemp -d "${TMPDIR:-$PREFIX_DIR/tmp}/omp-install.XXXXXX")"
had_old=0

cleanup() {
    rm -rf "$tmp" "$LIB_DIR.new"
    if [ "$had_old" -eq 1 ] && [ ! -d "$LIB_DIR" ] && [ -d "$LIB_DIR.old" ]; then
        mv "$LIB_DIR.old" "$LIB_DIR"
    fi
}
trap cleanup EXIT INT TERM

mkdir -p "$LIB_DIR.new"
curl -fL --retry 3 "$BASE/omp-termux.tar.gz" -o "$tmp/omp-termux.tar.gz"
curl -fL --retry 3 "$BASE/omp-termux.tar.gz.sha256" -o "$tmp/omp-termux.tar.gz.sha256"
(cd "$tmp" && sha256sum -c omp-termux.tar.gz.sha256)
tar -xzf "$tmp/omp-termux.tar.gz" -C "$LIB_DIR.new"
chmod +x "$LIB_DIR.new/bun"

# Bundle Chromium-absence gate — OMP never bundles Chromium (external Termux:X11 package only).
# Fail closed before swap so previous install is preserved via rollback.
if [ -f "$LIB_DIR.new/chrome" ] || [ -f "$LIB_DIR.new/chromium" ] || [ -f "$LIB_DIR.new/headless_shell" ]; then
    echo 'error: bundle unexpectedly contains Chromium binary — OMP bundle must remain free of Chromium' >&2
    exit 1
fi
# Also reject any stray chromium-named binaries at top level or under bin/lib
if find "$LIB_DIR.new" -maxdepth 2 -type f \( -name "chrome" -o -name "chromium" -o -name "headless_shell" \) 2>/dev/null | grep -q .; then
    echo 'error: bundle contains Chromium executable — external Termux:X11 package only, not bundled' >&2
    find "$LIB_DIR.new" -maxdepth 2 -type f \( -name "chrome" -o -name "chromium" -o -name "headless_shell" \) 2>&1 | head -n 5 >&2 || true
    exit 1
fi

# Pre-swap smoke test on .new
COMPAT_SO="$LIB_DIR.new/libomp-compat.so"
FALLBACK_SO="$PREFIX_DIR/lib/libfix_close_range.so"
PRELOAD_PRE=""
if ! "$LIB_DIR.new/bun" --version >/dev/null 2>&1; then
    if [ -f "$COMPAT_SO" ]; then
        PRELOAD_PRE="$COMPAT_SO"
    elif [ -f "$FALLBACK_SO" ]; then
        PRELOAD_PRE="$FALLBACK_SO"
    fi
fi

if [ -n "$PRELOAD_PRE" ]; then
    LD_PRELOAD="$PRELOAD_PRE" "$LIB_DIR.new/bun" "$LIB_DIR.new/cli.js" --version >/dev/null 2>&1 || {
        echo 'error: new OMP bundle failed smoke test' >&2
        exit 1
    }
else
    "$LIB_DIR.new/bun" "$LIB_DIR.new/cli.js" --version >/dev/null 2>&1 || {
        echo 'error: new OMP bundle failed smoke test' >&2
        exit 1
    }
fi

# Guarded two-rename swap
if [ -d "$LIB_DIR" ]; then
    rm -rf "$LIB_DIR.old"
    mv "$LIB_DIR" "$LIB_DIR.old"
    had_old=1
fi

if ! mv "$LIB_DIR.new" "$LIB_DIR"; then
    echo 'error: swap failed, restoring previous installation' >&2
    if [ "$had_old" -eq 1 ]; then
        mv "$LIB_DIR.old" "$LIB_DIR"
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
            mv "$LIB_DIR.old" "$LIB_DIR"
        fi
        exit 1
    }
else
    "$LIB_DIR/bun" "$LIB_DIR/cli.js" --version >/dev/null 2>&1 || {
        echo 'error: installed OMP bundle failed post-swap smoke test' >&2
        rm -rf "$LIB_DIR"
        if [ "$had_old" -eq 1 ]; then
            mv "$LIB_DIR.old" "$LIB_DIR"
        fi
        exit 1
    }
fi
rm -f "$BIN_DIR/omp"
cat > "$BIN_DIR/omp" <<EOF
#!/bin/sh
PREFIX_DIR="\${PREFIX:-$PREFIX_DIR}"
LIB_DIR="\$PREFIX_DIR/lib/omp-termux"
COMPAT_SO="\$LIB_DIR/libomp-compat.so"
FALLBACK_SO="\$PREFIX_DIR/lib/libfix_close_range.so"

if [ -f "\$COMPAT_SO" ]; then
    export LD_PRELOAD="\$COMPAT_SO\${LD_PRELOAD:+:\$LD_PRELOAD}"
elif [ -f "\$FALLBACK_SO" ]; then
    export LD_PRELOAD="\$FALLBACK_SO\${LD_PRELOAD:+:\$LD_PRELOAD}"
fi
exec env OMP_PLATFORM=android "\$LIB_DIR/bun" "\$LIB_DIR/cli.js" "\$@"
EOF
chmod 755 "$BIN_DIR/omp"

if ! "$BIN_DIR/omp" --version >/dev/null 2>&1; then
    echo 'error: installed omp launcher failed verification' >&2
    rm -rf "$LIB_DIR"
    if [ "$had_old" -eq 1 ]; then
        mv "$LIB_DIR.old" "$LIB_DIR"
    fi
    exit 1
fi

rm -rf "$LIB_DIR.old"
had_old=0
printf '%s\n' 'Installed omp. Run: omp'
# Browser is external — not bundled (Termux:X11 chromium 149.0.7827.155 at $PREFIX/lib/chromium/chrome, verified by android/scripts/verify-browser.sh).
# If you have the omp-termux clone, run from repo root:
#   pkg install -y x11-repo
#   pkg install -y chromium
#   bash android/scripts/verify-browser.sh
#   bash android/scripts/smoke-browser.sh --no-network
# If you installed via curl | sh and have no clone, fetch verifiers from the release (published as separate assets, never bundled):
#   curl -fsSL "https://github.com/$REPO/releases/latest/download/verify-browser.sh" -o /tmp/verify-browser.sh
#   bash /tmp/verify-browser.sh
#   curl -fsSL "https://github.com/$REPO/releases/latest/download/smoke-browser.sh" -o /tmp/smoke-browser.sh
#   bash /tmp/smoke-browser.sh --no-network
printf '%s\n' 'Browser: for OMP browser tool, install external Chromium separately:'
printf '%s\n' '  pkg install -y x11-repo'
printf '%s\n' '  pkg install -y chromium'
printf '%s\n' '  bash android/scripts/verify-browser.sh  # from clone, or fetch from release:'
printf '%s\n' "  curl -fsSL https://github.com/$REPO/releases/latest/download/verify-browser.sh -o /tmp/verify-browser.sh && bash /tmp/verify-browser.sh"
