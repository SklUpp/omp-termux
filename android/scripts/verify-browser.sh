#!/usr/bin/env bash
# verify-browser.sh — validate external Termux:X11 Chromium prerequisite for OMP browser tool
#
# Pinned artifact gate (reference Termux device):
#   package: chromium 149.0.7827.155 aarch64
#   source:  https://packages.termux.dev/apt/termux-x11 x11/main aarch64 Packages
#   deb:     chromium_149.0.7827.155_aarch64.deb
#   SHA256:  36500e23ad23bf2616bb4f215a297ba5a2b9e625992362b9b9c6bd05e0a27272
#   binary:  $PREFIX/lib/chromium/chrome
#   ELF:     ARM aarch64, Android API 24, Bionic, interpreter /system/bin/linker64
#
# This script is idempotent and fails closed:
#   - missing Termux/arch, missing package/binary, or mismatched version/SHA256 → exit 1
#   - glibc loader/libc.so.6, wrong interpreter, or non-Bionic → exit 1
#   - headless_shell is NOT accepted for CDP; main chrome must pass --version and file checks
#
# Modes:
#   verify-browser.sh [--install] [--check-bundle <path>] [--skip-deb-sha256] [--help]
#   --install            idempotently ensure x11-repo + chromium are installed (pkg install if needed)
#   --check-bundle PATH  additionally verify PATH tar.gz does not bundle chromium
#   --skip-deb-sha256    skip .deb SHA256 gate (CI without cached .deb can still check ELF/version)
#   --help               usage
#
# Safe dry-run: `bash -n verify-browser.sh` and `verify-browser.sh --help`
# No Chromium is bundled; release workflow publishes this script/metadata only.
set -euo pipefail

CHROMIUM_VERSION="149.0.7827.155"
CHROMIUM_PACKAGE_SHA256="36500e23ad23bf2616bb4f215a297ba5a2b9e625992362b9b9c6bd05e0a27272"
CHROMIUM_DEB_NAME="chromium_${CHROMIUM_VERSION}_aarch64.deb"
CHROMIUM_REPO_URL="https://packages.termux.dev/apt/termux-x11"
CHROMIUM_REPO_DIST="x11/main"

# PREFIX must be initialized before any expansion under set -u
PREFIX_DIR="${PREFIX:-/data/data/com.termux/files/usr}"
CHROMIUM_BINARY_DEFAULT="$PREFIX_DIR/lib/chromium/chrome"
CHROMIUM_FALLBACKS=(
  "$PREFIX_DIR/lib/chromium/chrome"
  "$PREFIX_DIR/bin/chromium"
  "$PREFIX_DIR/bin/chromium-browser"
  "$PREFIX_DIR/bin/chrome"
)

CHROMIUM_BINARY="${CHROMIUM_BINARY_DEFAULT:-$PREFIX_DIR/lib/chromium/chrome}"
# Allow env override for testing, but canonical gate is $PREFIX/lib/chromium/chrome
if [ -n "${CHROMIUM_BINARY_OVERRIDE:-}" ]; then
  CHROMIUM_BINARY="$CHROMIUM_BINARY_OVERRIDE"
fi
# Also honor explicit user config via PUPPETEER_EXECUTABLE_PATH for precedence checks,
# but validation always probes the canonical binary unless overridden.
PUPPETEER_EXECUTABLE_PATH_VAL="${PUPPETEER_EXECUTABLE_PATH:-}"

INSTALL=0
CHECK_BUNDLE=""
SKIP_DEB_SHA256=0

usage() {
  cat <<EOF
verify-browser.sh — validate external Termux:X11 Chromium prerequisite

Pinned: chromium $CHROMIUM_VERSION aarch64 from $CHROMIUM_REPO_URL $CHROMIUM_REPO_DIST
Binary: \$PREFIX/lib/chromium/chrome  SHA256($CHROMIUM_DEB_NAME)=$CHROMIUM_PACKAGE_SHA256
ELF: ARM64, Android API 24, Bionic /system/bin/linker64 — rejects glibc ld-linux / libc.so.6

Usage:
  $0 [--install] [--check-bundle PATH] [--skip-deb-sha256] [--help]

  --install               idempotently install x11-repo + chromium if missing/mismatched
  --check-bundle PATH     verify tar.gz at PATH does not contain chromium
  --skip-deb-sha256       skip .deb SHA256 gate (useful on CI without apt cache)
  --help                  this help

Examples:
  $0
  $0 --install
  $0 --check-bundle ./omp-termux.tar.gz
  bash -n $0   # syntax check
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --install) INSTALL=1; shift ;;
    --check-bundle) CHECK_BUNDLE="${2:-}"; [ -n "$CHECK_BUNDLE" ] || { echo "error: --check-bundle requires PATH" >&2; exit 1; }; shift 2 ;;
    --skip-deb-sha256) SKIP_DEB_SHA256=1; shift ;;
    --help|-h) usage; exit 0 ;;
    --) shift; break ;;
    -*) echo "error: unknown flag $1" >&2; usage >&2; exit 1 ;;
    *) echo "error: unexpected arg $1" >&2; usage >&2; exit 1 ;;
  esac
done

fail() { echo "error: $*" >&2; exit 1; }
info() { echo "==> $*"; }
warn() { echo "warn: $*" >&2; }

# Bundle-only verification runs on Ubuntu release runners without a Termux PREFIX.
if [ -z "${PREFIX:-}" ] && [ -n "$CHECK_BUNDLE" ]; then
  PREFIX_DIR="/tmp"
else
  if [ -z "${PREFIX:-}" ]; then
    if [ -d "$PREFIX_DIR" ]; then
      warn "PREFIX not set; using $PREFIX_DIR"
    else
      fail "PREFIX not set and $PREFIX_DIR missing — run inside Termux"
    fi
  fi
  PREFIX_DIR="${PREFIX:-$PREFIX_DIR}"
fi

CHROMIUM_BINARY="$PREFIX_DIR/lib/chromium/chrome"
if [ -n "${CHROMIUM_BINARY_OVERRIDE:-}" ]; then
  CHROMIUM_BINARY="$CHROMIUM_BINARY_OVERRIDE"
fi

# 0. Architecture gate (allow x86_64 CI to run metadata-only checks with --skip-deb-sha256)
ARCH="$(uname -m 2>/dev/null || echo unknown)"
case "$ARCH" in
  aarch64|arm64) info "arch: $ARCH (Android Termux)" ;;
  x86_64|amd64)
    if [ "$SKIP_DEB_SHA256" -eq 1 ]; then
      warn "arch $ARCH: running metadata-only checks (device gate requires aarch64)"
    else
      warn "arch $ARCH: not Android aarch64 — full device validation requires Termux aarch64"
    fi
    ;;
  *) warn "arch $ARCH: unexpected — expected aarch64 for device gate" ;;
esac

# 1. Termux layout gate
[ -d "$PREFIX_DIR" ] || fail "PREFIX dir missing: $PREFIX_DIR — run inside Termux"
[ -d "$PREFIX_DIR/lib" ] || warn "PREFIX/lib missing — Termux prefix looks incomplete"

# 2. Optional idempotent install of x11-repo + chromium
ensure_x11_repo() {
  if dpkg -s x11-repo >/dev/null 2>&1; then
    info "x11-repo: installed"
    return 0
  fi
  if [ "$INSTALL" -eq 0 ]; then
    fail "x11-repo not installed — install with: pkg install -y x11-repo (or rerun with --install)"
  fi
  info "installing x11-repo (idempotent)"
  pkg install -y x11-repo || fail "pkg install x11-repo failed"
  info "x11-repo: installed"
}

ensure_chromium_package() {
  if dpkg -s chromium >/dev/null 2>&1; then
    local ver
    ver="$(dpkg -s chromium 2>/dev/null | awk -F': ' '/^Version:/{print $2}' || true)"
    if [ "$ver" = "$CHROMIUM_VERSION" ]; then
      info "chromium package: $ver (pinned)"
    else
      warn "chromium package version $ver != pinned $CHROMIUM_VERSION"
      if [ "$INSTALL" -eq 1 ]; then
        info "installing pinned chromium $CHROMIUM_VERSION (idempotent)"
        ensure_x11_repo
        pkg install -y "chromium=$CHROMIUM_VERSION" 2>/dev/null || pkg install -y chromium || fail "pkg install chromium failed"
        ver="$(dpkg -s chromium 2>/dev/null | awk -F': ' '/^Version:/{print $2}' || true)"
        [ "$ver" = "$CHROMIUM_VERSION" ] || fail "chromium version after install is $ver, expected $CHROMIUM_VERSION"
      else
        fail "chromium version $ver != pinned $CHROMIUM_VERSION — run with --install or: pkg install -y chromium=$CHROMIUM_VERSION"
      fi
    fi
    return 0
  fi
  if [ "$INSTALL" -eq 0 ]; then
    fail "chromium package not installed — install with: pkg install -y x11-repo && pkg install -y chromium (or rerun with --install)"
  fi
  info "installing chromium $CHROMIUM_VERSION (idempotent)"
  ensure_x11_repo
  pkg install -y "chromium=$CHROMIUM_VERSION" 2>/dev/null || pkg install -y chromium || fail "pkg install chromium failed"
  dpkg -s chromium >/dev/null 2>&1 || fail "chromium still not installed after pkg install"
  local ver
  ver="$(dpkg -s chromium 2>/dev/null | awk -F': ' '/^Version:/{print $2}' || true)"
  [ "$ver" = "$CHROMIUM_VERSION" ] || warn "installed chromium $ver != pinned $CHROMIUM_VERSION (pin requires $CHROMIUM_VERSION)"
}

# 2a. Validate package metadata (repository, arch, SHA256 source)
validate_package_metadata() {
  info "checking chromium package metadata"
  dpkg -s chromium >/dev/null 2>&1 || fail "chromium package not installed"
  local ver arch status
  ver="$(dpkg -s chromium 2>/dev/null | awk -F': ' '/^Version:/{print $2}' || true)"
  arch="$(dpkg -s chromium 2>/dev/null | awk -F': ' '/^Architecture:/{print $2}' || true)"
  status="$(dpkg -s chromium 2>/dev/null | awk -F': ' '/^Status:/{print $2}' || true)"
  [ "$ver" = "$CHROMIUM_VERSION" ] || fail "chromium Version $ver != pinned $CHROMIUM_VERSION"
  [ "$arch" = "aarch64" ] || fail "chromium Architecture $arch != aarch64"
  case "$status" in *installed*) ;; *) fail "chromium Status not installed: $status" ;; esac

  # Repository pin: APT-Sources or apt-cache policy must reference termux-x11
  local apt_sources
  apt_sources="$(dpkg -s chromium 2>/dev/null | grep -E 'APT-Sources|Filename' || true)"
  if echo "$apt_sources" | grep -q "packages.termux.dev/apt/termux-x11"; then
    info "chromium APT-Sources: termux-x11 (pinned)"
  else
    # Fallback to apt-cache policy
    local policy
    policy="$(apt-cache policy chromium 2>/dev/null || true)"
    if echo "$policy" | grep -q "packages.termux.dev/apt/termux-x11"; then
      info "chromium apt-cache policy: termux-x11 (pinned)"
    else
      warn "could not confirm chromium APT-Sources is $CHROMIUM_REPO_URL — policy output:"
      echo "$policy" | head -n 20 >&2 || true
      # Check apt sources list as fallback
      if grep -R -q "packages.termux.dev/apt/termux-x11" "$PREFIX_DIR/etc/apt/" 2>/dev/null; then
        info "apt sources list contains termux-x11"
      else
        fail "chromium repository not pinned to $CHROMIUM_REPO_URL — check x11-repo and apt sources"
      fi
    fi
  fi

  # Architecture table check
  local policy_arch
  policy_arch="$(apt-cache policy chromium 2>/dev/null | grep -E "Candidate|Version table" -A2 | head -n 20 || true)"
  echo "$policy_arch" | grep -q "$CHROMIUM_VERSION" || warn "apt-cache policy does not list pinned version $CHROMIUM_VERSION"

  info "package metadata OK: $ver $arch"
}

# 2b. Validate .deb SHA256 provenance
validate_deb_sha256() {
  if [ "$SKIP_DEB_SHA256" -eq 1 ]; then
    warn "skipping .deb SHA256 gate (--skip-deb-sha256)"
    return 0
  fi
  info "checking chromium .deb SHA256 ($CHROMIUM_PACKAGE_SHA256)"
  local deb_path=""
  # Prefer apt cache
  for d in "$PREFIX_DIR/var/cache/apt/archives" "/var/cache/apt/archives"; do
    if [ -f "$d/$CHROMIUM_DEB_NAME" ]; then deb_path="$d/$CHROMIUM_DEB_NAME"; break; fi
    # Fallback: any chromium deb in cache (if versioned filename differs)
    local any
    any="$(ls -1 "$d"/chromium*.deb 2>/dev/null | head -n1 || true)"
    if [ -n "$any" ] && [ -f "$any" ]; then
      # Only use if it matches pinned version via dpkg-deb -f
      local any_ver
      any_ver="$(dpkg-deb -f "$any" Version 2>/dev/null || true)"
      if [ "$any_ver" = "$CHROMIUM_VERSION" ]; then deb_path="$any"; break; fi
    fi
  done

  if [ -z "$deb_path" ]; then
    # Try apt metadata SHA256 without downloading full deb
    local apt_sha
    apt_sha="$(apt-cache show chromium 2>/dev/null | awk -F': ' '/^SHA256:/{print $2; exit}' || true)"
    if [ "$apt_sha" = "$CHROMIUM_PACKAGE_SHA256" ]; then
      info "chromium SHA256 OK via apt-cache show: $apt_sha"
      return 0
    fi
    # Try to download to /tmp for verification (idempotent, does not install)
    if command -v apt-get >/dev/null 2>&1 || command -v pkg >/dev/null 2>&1; then
      info "cached $CHROMIUM_DEB_NAME not found — fetching metadata for SHA256 check"
      local tmp_deb="/tmp/$CHROMIUM_DEB_NAME"
      rm -f "$tmp_deb"
      # Use apt download if available
      if command -v apt >/dev/null 2>&1; then
        (cd /tmp && apt download chromium="$CHROMIUM_VERSION" >/dev/null 2>&1 || apt download chromium >/dev/null 2>&1 || true)
        # apt download names file as chromium_<ver>_aarch64.deb
        if [ -f "/tmp/$CHROMIUM_DEB_NAME" ]; then deb_path="/tmp/$CHROMIUM_DEB_NAME"
        elif ls /tmp/chromium*.deb >/dev/null 2>&1; then deb_path="$(ls -1 /tmp/chromium*.deb | head -n1)"
        fi
      fi
      if [ -z "$deb_path" ] || [ ! -f "$deb_path" ]; then
        # Fallback: direct curl from repo URL (pinned)
        local url="$CHROMIUM_REPO_URL/pool/main/c/chromium/$CHROMIUM_DEB_NAME"
        info "downloading $url for SHA256 verification"
        if command -v curl >/dev/null 2>&1; then
          curl -fL --retry 3 "$url" -o "$tmp_deb" 2>/dev/null || true
          [ -f "$tmp_deb" ] && deb_path="$tmp_deb"
        elif command -v wget >/dev/null 2>&1; then
          wget -q "$url" -O "$tmp_deb" 2>/dev/null || true
          [ -f "$tmp_deb" ] && deb_path="$tmp_deb"
        fi
      fi
    fi
    if [ -z "$deb_path" ] || [ ! -f "$deb_path" ]; then
      fail "chromium .deb not found for SHA256 verification — cached $CHROMIUM_DEB_NAME missing and download failed; ensure package is installed or rerun with --skip-deb-sha256 only on CI"
    fi
  fi

  [ -f "$deb_path" ] || fail "chromium .deb path not found: $deb_path"
  local got
  got="$(sha256sum "$deb_path" 2>/dev/null | awk '{print $1}' || true)"
  [ -n "$got" ] || fail "sha256sum failed for $deb_path"
  if [ "$got" != "$CHROMIUM_PACKAGE_SHA256" ]; then
    fail "chromium .deb SHA256 mismatch: got $got expected $CHROMIUM_PACKAGE_SHA256 ($deb_path)"
  fi
  info "chromium .deb SHA256 OK: $got ($deb_path)"

  # Also verify apt-cache show SHA256 matches pin
  local apt_sha2
  apt_sha2="$(apt-cache show chromium 2>/dev/null | awk -F': ' '/^SHA256:/{print $2; exit}' || true)"
  if [ -n "$apt_sha2" ] && [ "$apt_sha2" != "$CHROMIUM_PACKAGE_SHA256" ]; then
    warn "apt-cache show SHA256 $apt_sha2 != pinned $CHROMIUM_PACKAGE_SHA256"
  fi
}

# 3. Binary existence and permission
validate_binary_exists() {
  info "checking Chromium binary: $CHROMIUM_BINARY"
  [ -e "$CHROMIUM_BINARY" ] || fail "Chromium binary missing: $CHROMIUM_BINARY — install chromium or set CHROMIUM_BINARY_OVERRIDE"
  [ -f "$CHROMIUM_BINARY" ] || fail "Chromium binary not a regular file: $CHROMIUM_BINARY"
  [ -x "$CHROMIUM_BINARY" ] || fail "Chromium binary not executable: $CHROMIUM_BINARY (chmod +x?)"
  # Also log fallback candidates for diagnostics
  for p in "${CHROMIUM_FALLBACKS[@]}"; do
    # Expand $PREFIX prefix already
    local exp="${p//\$PREFIX/$PREFIX_DIR}"
    exp="${exp//\$\{PREFIX\}/$PREFIX_DIR}"
    # Direct value already expanded above; just check existence
    if [ -e "$exp" ]; then
      if [ -x "$exp" ]; then info "candidate executable: $exp (executable)"; else warn "candidate: $exp (not executable)"; fi
    fi
  done
  info "binary exists and executable: $CHROMIUM_BINARY"
}

# 4. ELF/Bionic/API validation — reject glibc
validate_elf_bionic() {
  info "validating ELF/Bionic/API for $CHROMIUM_BINARY"
  command -v file >/dev/null 2>&1 || fail "file(1) not found — install with pkg install -y file"
  command -v readelf >/dev/null 2>&1 || fail "readelf not found — install binutils (pkg install -y binutils)"
  local file_out
  file_out="$(file -b "$CHROMIUM_BINARY" 2>&1 || true)"
  echo "    file: $file_out"
  echo "$file_out" | grep -q "ELF" || fail "not an ELF binary: $file_out"
  echo "$file_out" | grep -q "aarch64\|ARM aarch64" || fail "ELF arch not aarch64: $file_out"
  echo "$file_out" | grep -q "shared object" || fail "ELF type not shared object/PIE: $file_out"
  echo "$file_out" | grep -q "dynamically linked" || fail "ELF not dynamically linked: $file_out"
  # Interpreter must be Bionic linker
  local interp
  interp="$(readelf -l "$CHROMIUM_BINARY" 2>/dev/null | grep -o '/system/bin/linker64' || true)"
  [ "$interp" = "/system/bin/linker64" ] || fail "ELF interpreter not /system/bin/linker64 — got $interp (glibc ld-linux would be wrong)"
  if readelf -l "$CHROMIUM_BINARY" 2>/dev/null | grep -q "ld-linux"; then
    fail "ELF contains glibc loader ld-linux — Bionic binary required"
  fi
  # Dynamic deps: must NOT contain libc.so.6, must contain bionic libc.so
  local needed
  needed="$(readelf -d "$CHROMIUM_BINARY" 2>/dev/null | grep NEEDED || true)"
  echo "$needed" | head -n 20 | sed 's/^/    NEEDED: /'
  if echo "$needed" | grep -q "libc\.so\.6"; then
    fail "ELF NEEDED contains libc.so.6 (glibc) — Bionic binary required"
  fi
  echo "$needed" | grep -q "libc\.so" || fail "ELF NEEDED missing libc.so (Bionic)"
  # Confirm for Android 24 via note
  local notes
  notes="$(readelf -n "$CHROMIUM_BINARY" 2>/dev/null || true)"
  echo "$notes" | grep -q "NT_ANDROID_TYPE_IDENT" || fail "ELF .note.android.ident missing — Bionic Android binary required (NT_ANDROID_TYPE_IDENT)"
  # Decode API level from note data: first 4 bytes LE = API level (0x18 = 24)
  local api_hex
  api_hex="$(readelf -n "$CHROMIUM_BINARY" 2>/dev/null | grep -A1 'NT_ANDROID_TYPE_IDENT' -A1 | grep 'description data' | head -n1 | awk '{print $3}' || true)"
  [ -n "$api_hex" ] || fail "ELF .note.android.ident undecodable — cannot determine Android API level"
  # description data: 18 00 00 00 ... -> 0x18 = 24
  local api_dec
  api_dec="$(printf "%d" "0x$api_hex" 2>/dev/null || echo "?")"
  [ "$api_dec" != "?" ] || fail "ELF .note.android.ident API level undecodable (hex $api_hex)"
  info "Android API level from .note.android.ident: $api_dec (hex $api_hex)"
  [ "$api_dec" = "24" ] || fail "ELF Android API $api_dec != 24 — pinned binary must be API 24 (NDK r29)"
  # No glibc strings in binary?
  if strings "$CHROMIUM_BINARY" 2>/dev/null | grep -q "GLIBC_"; then
    warn "binary contains GLIBC_ strings — verify Bionic build"
  fi
  if readelf -p .comment "$CHROMIUM_BINARY" 2>/dev/null | grep -q "glibc"; then
    fail "ELF .comment contains glibc — Bionic required"
  fi
  info "ELF/Bionic/API validation OK"
}

# 5. --version probe
validate_version_probe() {
  info "probing Chromium version: $CHROMIUM_BINARY --version"
  local ver_out
  if ! ver_out="$("$CHROMIUM_BINARY" --version 2>&1 | head -n1)"; then
    fail "Chromium --version failed: $ver_out"
  fi
  echo "    $ver_out"
  echo "$ver_out" | grep -q "$CHROMIUM_VERSION" || fail "Chromium --version output does not contain pinned $CHROMIUM_VERSION — got: $ver_out"
  # Also confirm aarch64/Chrome/Chromium tag
  echo "$ver_out" | grep -qi "Chromium\|Chrome" || fail "Chromium --version output unexpected: $ver_out"
  info "Chromium version probe OK: $ver_out"
}

# 6. headless_shell is NOT the selected executable
validate_headless_shell_gate() {
  local hs="$PREFIX_DIR/lib/chromium/headless_shell"
  if [ -f "$hs" ]; then
    info "headless_shell present: $hs — verifying it rejects remote debugging (must not be used for CDP)"
    local file_out
    file_out="$(file -b "$hs" 2>/dev/null || true)"
    echo "    file: $file_out"
    # Try to prove it rejects remote debugging (expected failure)
    local hs_out
    hs_out="$(timeout 3 "$hs" --headless --remote-debugging-port=9222 --dump-dom about:blank 2>&1 || true)"
    if echo "$hs_out" | grep -q "not compatible with remote debugging"; then
      info "headless_shell correctly rejects remote debugging"
    else
      warn "headless_shell did not produce expected remote-debugging rejection — still must not be selected as executable"
    fi
    # Ensure canonical binary is NOT headless_shell
    if [ "$CHROMIUM_BINARY" = "$hs" ]; then
      fail "selected Chromium binary must be \$PREFIX/lib/chromium/chrome, not headless_shell"
    fi
  else
    info "headless_shell not present at $hs (optional)"
  fi
}

# 7. Bundled Chromium absence check — OMP bundle must stay free of Chromium (external Termux:X11 only)
validate_no_bundled_chromium() {
  local bundle_path="${1:-}"
  if [ -z "$bundle_path" ]; then
    for p in "termux-bundle" "termux-build" "omp-termux.tar.gz" "termux-js.tar.gz"; do
      if [ -e "$p" ]; then bundle_path="$p"; break; fi
    done
    [ -z "$bundle_path" ] && return 0
  fi
  if [ ! -e "$bundle_path" ]; then
    warn "bundle path not found for chromium check: $bundle_path"
    return 0
  fi
  info "checking bundle does not contain Chromium: $bundle_path"
  local hits=""
  if [ -f "$bundle_path" ]; then
    # Any tar archive (tar.gz, tgz, tar) — probe via tar, not extension
    if tar -tzf "$bundle_path" >/dev/null 2>&1; then
      # Catch any chromium/headless naming anywhere in archive
      hits="$(tar -tzf "$bundle_path" 2>/dev/null | grep -i -E "chromium|headless_shell|libchromium" || true)"
      if [ -n "$hits" ]; then
        echo "$hits" | sed 's/^/    bundled: /' >&2
        fail "bundle $bundle_path contains Chromium paths — OMP bundle must remain free of Chromium (external apt install only)"
      fi
      # Also catch plain chrome executable at any level (e.g. ./chrome, bin/chrome, ./chromium)
      hits="$(tar -tzf "$bundle_path" 2>/dev/null | grep -E '(^|/)(chrome|chromium)(\.exe)?$' || true)"
      if [ -n "$hits" ]; then
        echo "$hits" | sed 's/^/    bundled executable: /' >&2
        fail "bundle $bundle_path contains Chromium executable — external Termux:X11 package only, not bundled: $hits"
      fi
      # Legacy strict root check retained for clarity
      hits="$(tar -tzf "$bundle_path" 2>/dev/null | grep -E '^\./(chrome|chromium|headless_shell)$' || true)"
      [ -z "$hits" ] || fail "bundle $bundle_path contains unexpected Chromium executable at root: $hits"
    else
      # Not a tar.gz — try directory-style or plain file check
      if [ -d "$bundle_path" ]; then
        hits="$(find "$bundle_path" -type f \( -name "*chromium*" -o -name "*chrome*" -o -name "headless_shell" \) 2>/dev/null | head -n 20 || true)"
        if [ -n "$hits" ]; then
          echo "$hits" | sed 's/^/    bundled file: /' >&2
          # Only fail on binary-like names, not docs mentioning chrome
          local bin_hits
          bin_hits="$(find "$bundle_path" -maxdepth 2 -type f \( -name "chrome" -o -name "chromium" -o -name "headless_shell" \) 2>/dev/null || true)"
          [ -z "$bin_hits" ] || fail "bundle dir $bundle_path contains Chromium binary: $bin_hits"
          # If only chromium docs, warn
          warn "bundle dir $bundle_path contains chromium-named files (non-binary): $hits"
        fi
      fi
    fi
  elif [ -d "$bundle_path" ]; then
    hits="$(find "$bundle_path" -type f \( -name "*chromium*" -o -name "*chrome*" -o -name "headless_shell" \) 2>/dev/null | head -n 20 || true)"
    if [ -n "$hits" ]; then
      echo "$hits" | sed 's/^/    bundled file: /' >&2
      local bin_hits
      bin_hits="$(find "$bundle_path" -maxdepth 2 -type f \( -name "chrome" -o -name "chromium" -o -name "headless_shell" \) 2>/dev/null || true)"
      [ -z "$bin_hits" ] || fail "bundle dir $bundle_path contains Chromium binary: $bin_hits"
      warn "bundle dir $bundle_path contains chromium-named files (non-binary): $hits"
    fi
  fi
  info "bundle chromium absence OK: $bundle_path"
}

# 8. Explicit PUPPETEER_EXECUTABLE_PATH precedence check (if set, must be valid)
validate_explicit_path() {
  if [ -n "$PUPPETEER_EXECUTABLE_PATH_VAL" ]; then
    info "PUPPETEER_EXECUTABLE_PATH is set: $PUPPETEER_EXECUTABLE_PATH_VAL"
    [ -e "$PUPPETEER_EXECUTABLE_PATH_VAL" ] || fail "PUPPETEER_EXECUTABLE_PATH points to missing file: $PUPPETEER_EXECUTABLE_PATH_VAL"
    [ -x "$PUPPETEER_EXECUTABLE_PATH_VAL" ] || fail "PUPPETEER_EXECUTABLE_PATH not executable: $PUPPETEER_EXECUTABLE_PATH_VAL"
    # Probe it as Chromium
    local ver
    ver="$("$PUPPETEER_EXECUTABLE_PATH_VAL" --version 2>&1 | head -n1 || true)"
    echo "$ver" | grep -qi "Chrom" || fail "PUPPETEER_EXECUTABLE_PATH probe failed — not Chromium: $ver"
    info "PUPPETEER_EXECUTABLE_PATH probe OK: $ver"
  fi
  # Also check hypothetical browser.executablePath setting file if present (OMP config)
  local omp_config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/omp"
  # Not failing if missing; just informational
  if [ -f "$omp_config_dir/config.json" ]; then
    if grep -q "executablePath" "$omp_config_dir/config.json" 2>/dev/null; then
      info "OMP config contains browser.executablePath (explicit path will be validated by OMP launch)"
    fi
  fi
}

# `--check-bundle` is a standalone CI/release operation. It must work on an
# Ubuntu runner without Termux, dpkg Chromium, or Android ELF tools.
if [ -n "$CHECK_BUNDLE" ]; then
  validate_no_bundled_chromium "$CHECK_BUNDLE"
  info "bundle check standalone PASS: $CHECK_BUNDLE"
  echo "TERMUX_OMP_BROWSER_BUNDLE_OK=1"
  exit 0
fi

# Main device verification.
info "verify-browser: chromium $CHROMIUM_VERSION from $CHROMIUM_REPO_URL"
info "PREFIX=$PREFIX_DIR binary=$CHROMIUM_BINARY"

# Install gate first if requested — ensure x11-repo before Chromium.
if [ "$INSTALL" -eq 1 ]; then
  ensure_x11_repo
  ensure_chromium_package
fi

if dpkg -s chromium >/dev/null 2>&1; then
  validate_package_metadata
else
  fail "chromium package not installed — run: pkg install -y x11-repo && pkg install -y chromium (or $0 --install)"
fi

validate_deb_sha256
validate_binary_exists
validate_elf_bionic
validate_version_probe
validate_headless_shell_gate
validate_explicit_path

for auto in "omp-termux.tar.gz" "termux-bundle" "termux-js.tar.gz"; do
  if [ -e "$auto" ]; then
    validate_no_bundled_chromium "$auto"
  fi
done


info "verify-browser: OK — $CHROMIUM_BINARY is pinned $CHROMIUM_VERSION, Bionic ARM64 API24, no glibc, $CHROMIUM_REPO_URL"
echo "TERMUX_OMP_BROWSER_VERIFY_OK=1"
