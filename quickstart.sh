#!/bin/sh
set -eu

# One-command installer for omp-termux (Termux Android aarch64).
PREFIX_DIR="${PREFIX:-/data/data/com.termux/files/usr}"
LIB_DIR="$PREFIX_DIR/lib/omp-termux"
BIN_DIR="$PREFIX_DIR/bin"
REPO="${OMP_REPO:-SklUpp/omp-termux}"
TAG="${OMP_VERSION:-latest}"
BASE="https://github.com/$REPO/releases"
[ "$TAG" = latest ] && BASE="$BASE/latest/download" || BASE="$BASE/download/$TAG"

case "$(uname -m)" in aarch64|arm64) ;; *) echo 'error: Android arm64 required' >&2; exit 1 ;; esac
[ -n "${PREFIX:-}" ] || { echo 'error: run inside Termux' >&2; exit 1; }

TMP="$PREFIX_DIR/tmp/omp-termux.$$"
trap 'rm -rf "$TMP" "$LIB_DIR.new"' EXIT INT TERM
mkdir -p "$TMP" "$LIB_DIR.new" "$BIN_DIR"
pkg install -y curl tar
curl -fL --retry 5 --retry-all-errors --retry-delay 2 \
  "$BASE/omp-termux.tar.gz" -o "$TMP/omp-termux.tar.gz"
curl -fL --retry 5 --retry-all-errors --retry-delay 2 \
  "$BASE/omp-termux.tar.gz.sha256" -o "$TMP/omp-termux.tar.gz.sha256"
(cd "$TMP" && sha256sum -c omp-termux.tar.gz.sha256)
tar -xzf "$TMP/omp-termux.tar.gz" -C "$LIB_DIR.new"
rm -rf "$LIB_DIR"
mv "$LIB_DIR.new" "$LIB_DIR"

cat > "$BIN_DIR/omp" <<EOF
#!/bin/sh
exec env OMP_PLATFORM=android "$LIB_DIR/bun" "$LIB_DIR/cli.js" "\$@"
EOF
chmod 755 "$BIN_DIR/omp"
"$BIN_DIR/omp" --version
printf '%s\n' 'Installed omp-termux. Run: omp'
