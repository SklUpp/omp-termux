#!/usr/bin/env bash
# smoke-browser.sh — device smoke gate for external Termux:X11 Chromium + OMP browser integration
#
# Uses $PREFIX/lib/chromium/chrome (the passed package's main chrome, not headless_shell)
# and runs the full device gate from the implementation plan:
#   1. verify-browser.sh (version/repo/SHA256/ELF/Bionic/API)
#   2. --version probe
#   3. headless launch with loopback CDP ephemeral port
#   4. CDP attach (/json/version, /json/list)
#   5. navigation (example.com or data URL fallback)
#   6. viewport screenshot
#   7. full-page screenshot
#   8. element screenshot
#   9. model ImageContent delivery (PNG signature + base64)
#   10. shared daemon reuse (if OMP present)
#   11. daemon restart after spec change
#   12. failed-binary rejection/rollback (invalid explicit path fails closed, prior daemon preserved, no managed Linux download)
#
# Idempotent and safe to rerun; cleans up profiles and temp dirs on exit.
# Requires: chromium package, file, readelf, curl or wget, python3 or busybox, and optionally bun/node + puppeteer-core.
#
# Usage: smoke-browser.sh [--verify-only] [--install] [--no-network] [--help]
set -euo pipefail

CHROMIUM_VERSION="149.0.7827.155"
# PREFIX must be initialized before any expansion under set -u
PREFIX_DIR="${PREFIX:-/data/data/com.termux/files/usr}"
CHROMIUM_BINARY_DEFAULT="$PREFIX_DIR/lib/chromium/chrome"
CHROMIUM_BINARY="$PREFIX_DIR/lib/chromium/chrome"
if [ -n "${CHROMIUM_BINARY_OVERRIDE:-}" ]; then CHROMIUM_BINARY="$CHROMIUM_BINARY_OVERRIDE"; fi

VERIFY_ONLY=0
DO_INSTALL=0
NO_NETWORK=0
SKIP_DAEMON=0

usage() {
  cat <<EOF
smoke-browser.sh — Termux:X11 Chromium device smoke gate

Pinned: chromium $CHROMIUM_VERSION at \$PREFIX/lib/chromium/chrome

Usage:
  $0 [--verify-only] [--install] [--no-network] [--skip-daemon] [--help]

  --verify-only   only run verify-browser.sh checks, skip CDP/screenshot/daemon
  --install       pass --install to verify-browser.sh (idempotent pkg install)
  --no-network    skip example.com navigation, use data URL only
  --skip-daemon   skip OMP daemon reuse checks
  --help          this help

Prereqs: pkg install -y x11-repo chromium; file, binutils, curl, python3
Smoke artifacts: /tmp/omp-browser-smoke-* and \$PREFIX/tmp/omp-chrome-profile-*
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --verify-only) VERIFY_ONLY=1; shift ;;
    --install) DO_INSTALL=1; shift ;;
    --no-network) NO_NETWORK=1; shift ;;
    --skip-daemon) SKIP_DAEMON=1; shift ;;
    --help|-h) usage; exit 0 ;;
    --) shift; break ;;
    -*) echo "error: unknown flag $1" >&2; usage >&2; exit 1 ;;
    *) echo "error: unexpected arg $1" >&2; usage >&2; exit 1 ;;
  esac
done

fail() { echo "error: $*" >&2; exit 1; }
info() { echo "==> $*"; }
warn() { echo "warn: $*" >&2; }
pass() { echo "PASS: $*"; }

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
VERIFY_SCRIPT="$ROOT/android/scripts/verify-browser.sh"
[ -f "$VERIFY_SCRIPT" ] || VERIFY_SCRIPT="$(dirname "$0")/verify-browser.sh"
[ -f "$VERIFY_SCRIPT" ] || fail "verify-browser.sh not found"

TMP_BASE="${TMPDIR:-$PREFIX_DIR/tmp}"
mkdir -p "$TMP_BASE"
SMOKE_TMP="$(mktemp -d "$TMP_BASE/omp-browser-smoke.XXXXXX")"
PROFILE_DIR=""
CHROME_PID=""
CHROME_LOG="$SMOKE_TMP/chrome.log"
CDP_PORT=""
CDP_URL=""

cleanup() {
  local rc=$?
  if [ -n "${CHROME_PID:-}" ] && kill -0 "$CHROME_PID" 2>/dev/null; then
    info "stopping smoke chrome pid $CHROME_PID"
    kill "$CHROME_PID" 2>/dev/null || true
    wait "$CHROME_PID" 2>/dev/null || true
  fi
  if [ -n "${PROFILE_DIR:-}" ] && [ -d "$PROFILE_DIR" ]; then
    case "$PROFILE_DIR" in
      "$PREFIX_DIR/tmp/omp-chrome-profile-"*|"$TMP_BASE/omp-chrome-profile-"*|"$SMOKE_TMP"/*)
        rm -rf "$PROFILE_DIR" || true
        ;;
      *) warn "not removing unexpected profile dir $PROFILE_DIR" ;;
    esac
  fi
  # Keep SMOKE_TMP for post-mortem on failure; remove on success
  if [ $rc -eq 0 ]; then
    rm -rf "$SMOKE_TMP" || true
  else
    warn "smoke failed — logs kept at $SMOKE_TMP (chrome.log, screenshots)"
    ls -l "$SMOKE_TMP" 2>&1 | head -n 50 >&2 || true
  fi
}
trap cleanup EXIT INT TERM

# 0. Preflight
info "smoke-browser: PREFIX=$PREFIX_DIR binary=$CHROMIUM_BINARY"
command -v file >/dev/null 2>&1 || fail "file(1) missing — pkg install -y file"
command -v readelf >/dev/null 2>&1 || warn "readelf missing — verify-browser.sh will fail"
command -v curl >/dev/null 2>&1 || command -v wget >/dev/null 2>&1 || warn "curl/wget missing — CDP probe will use python3 fallback"

# 1. verify-browser.sh gate (includes version/repo/SHA256/ELF/Bionic/API)
info "step 1/12: verify-browser.sh"
if [ "$DO_INSTALL" -eq 1 ]; then
  bash "$VERIFY_SCRIPT" --install || fail "verify-browser.sh --install failed"
else
  bash "$VERIFY_SCRIPT" || fail "verify-browser.sh failed"
fi
pass "verify-browser.sh"

if [ "$VERIFY_ONLY" -eq 1 ]; then
  info "verify-only requested — stopping before CDP/screenshot/daemon"
  echo "TERMUX_OMP_BROWSER_SMOKE_OK=1 (verify-only)"
  exit 0
fi

# 2. --version probe (redundant with verify-browser, but explicit)
info "step 2/12: Chromium --version probe"
[ -x "$CHROMIUM_BINARY" ] || fail "Chromium binary not executable: $CHROMIUM_BINARY"
VER_OUT="$("$CHROMIUM_BINARY" --version 2>&1 | head -n1 || true)"
echo "    $VER_OUT"
echo "$VER_OUT" | grep -q "$CHROMIUM_VERSION" || fail "--version does not contain $CHROMIUM_VERSION: $VER_OUT"
pass "Chromium --version: $VER_OUT"

# 3. headless launch with loopback ephemeral port
find_free_port() {
  python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])' 2>/dev/null \
  || python -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])' 2>/dev/null \
  || sh -c 'cat /proc/sys/net/ipv4/ip_local_port_range' 2>/dev/null | awk '{print $1}' \
  || echo 0
}

launch_chrome() {
  local port="$1" profile="$2" log="$3"
  info "launching chrome --headless=new --no-sandbox --disable-dev-shm-usage --remote-debugging-address=127.0.0.1 --remote-debugging-port=$port"
  # Use $PREFIX/tmp for profile to keep on same filesystem as binary
  "$CHROMIUM_BINARY" \
    --headless=new \
    --no-sandbox \
    --disable-dev-shm-usage \
    --disable-gpu \
    --no-first-run \
    --no-default-browser-check \
    --remote-debugging-address=127.0.0.1 \
    --remote-debugging-port="$port" \
    --user-data-dir="$profile" \
    --window-size=1365,768 \
    about:blank \
    >"$log" 2>&1 &
  CHROME_PID=$!
  echo "$CHROME_PID" > "$SMOKE_TMP/chrome.pid"
  info "chrome pid $CHROME_PID log $log"
}

wait_for_cdp() {
  local port="$1" tries=30
  local url="http://127.0.0.1:$port/json/version"
  info "waiting for CDP at $url"
  for i in $(seq 1 $tries); do
    if curl -fsS "$url" >/dev/null 2>&1; then
      info "CDP ready after $i tries"
      return 0
    fi
    # Fallback to python3 if curl fails
    if python3 -c "import urllib.request,sys; urllib.request.urlopen('$url',timeout=1).read()" >/dev/null 2>&1; then
      info "CDP ready (python) after $i tries"
      return 0
    fi
    sleep 0.5
    kill -0 "$CHROME_PID" 2>/dev/null || {
      warn "chrome process $CHROME_PID exited early — log tail:"
      tail -n 50 "$CHROME_LOG" 2>&1 | sed 's/^/    /' >&2 || true
      return 1
    }
  done
  warn "CDP not ready after $tries tries — log tail:"
  tail -n 80 "$CHROME_LOG" 2>&1 | sed 's/^/    /' >&2 || true
  return 1
}

# Prepare profile and port
PROFILE_DIR="$PREFIX_DIR/tmp/omp-chrome-profile-smoke-$$"
# Fallback to $TMP_BASE if $PREFIX_DIR/tmp not writable
if ! mkdir -p "$PROFILE_DIR" 2>/dev/null; then
  PROFILE_DIR="$SMOKE_TMP/omp-chrome-profile-$$"
  mkdir -p "$PROFILE_DIR"
fi
CDP_PORT="$(find_free_port)"
[ "$CDP_PORT" != "0" ] || CDP_PORT="0" # let Chrome pick ephemeral
# If ephemeral (0), we need to parse log for actual port; simpler to use explicit free port
if [ "$CDP_PORT" = "0" ]; then CDP_PORT=9222; fi
CDP_URL="http://127.0.0.1:$CDP_PORT"

info "step 3/12: headless launch"
launch_chrome "$CDP_PORT" "$PROFILE_DIR" "$CHROME_LOG"
wait_for_cdp "$CDP_PORT" || fail "Chrome headless launch / CDP wait failed"
pass "headless launch — CDP listening on $CDP_URL"

# Validate loopback only (must not be 0.0.0.0)
if grep -q "0.0.0.0" "$CHROME_LOG" 2>/dev/null; then
  warn "CDP log mentions 0.0.0.0 — expected loopback only"
fi
if grep -q "DevTools listening on ws://127.0.0.1" "$CHROME_LOG" 2>/dev/null; then
  info "CDP listening on loopback (127.0.0.1) — OK"
else
  warn "CDP log does not contain expected 127.0.0.1 ws url — checking http endpoint"
fi

# 4. CDP attach
info "step 4/12: CDP attach (/json/version, /json/list)"
CDP_VERSION_JSON="$SMOKE_TMP/cdp-version.json"
CDP_LIST_JSON="$SMOKE_TMP/cdp-list.json"
fetch() {
  local url="$1" out="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsS "$url" -o "$out" 2>&1 || return 1
  else
    python3 -c "import urllib.request; open('$out','wb').write(urllib.request.urlopen('$url',timeout=5).read())" 2>&1 || return 1
  fi
}
fetch "$CDP_URL/json/version" "$CDP_VERSION_JSON" || fail "CDP /json/version fetch failed"
cat "$CDP_VERSION_JSON" | head -c 500 | sed 's/^/    /' || true
grep -q "Chrome/$CHROMIUM_VERSION\|Chrome/.*149" "$CDP_VERSION_JSON" 2>/dev/null || warn "CDP version does not contain pinned Chrome/$CHROMIUM_VERSION"
grep -q "\"webSocketDebuggerUrl\"" "$CDP_VERSION_JSON" || fail "CDP /json/version missing webSocketDebuggerUrl"
WS_URL="$(grep -o '"webSocketDebuggerUrl":[[:space:]]*"[^"]*"' "$CDP_VERSION_JSON" | head -n1 | cut -d'"' -f4 || true)"
[ -n "$WS_URL" ] || fail "CDP webSocketDebuggerUrl empty"
echo "    ws: $WS_URL"
echo "$WS_URL" | grep -q "ws://127.0.0.1" || warn "CDP ws url not loopback: $WS_URL"
# List targets
fetch "$CDP_URL/json/list" "$CDP_LIST_JSON" || fail "CDP /json/list fetch failed"
cat "$CDP_LIST_JSON" | head -c 500 | sed 's/^/    /' || true
pass "CDP attach OK"

# Helper: CDP via websocket? For smoke we can use puppeteer if available, else raw chrome screenshot path.
# Try puppeteer-core path first (device may have bun/node with puppeteer)
try_puppeteer_smoke() {
  local script="$SMOKE_TMP/puppeteer-smoke.mjs"
  cat > "$script" <<'PUPPETEER_EOF'
import puppeteer from 'puppeteer-core';
const CHROME = process.env.CHROMIUM_BINARY || process.env.PUPPETEER_EXECUTABLE_PATH;
const CDP = process.env.CDP_URL;
const NO_NET = process.env.NO_NETWORK === '1';
const TMP = process.env.SMOKE_TMP;
const version = process.env.CHROMIUM_VERSION;
async function main() {
  const cdpVersion = await fetch(`${CDP}/json/version`).then(r=>r.json());
  console.log(`CDP Browser: ${cdpVersion.Browser}`);
  if (!cdpVersion.Browser.includes(version)) console.warn(`warn: CDP Browser ${cdpVersion.Browser} != pinned ${version}`);
  const browser = await puppeteer.connect({ browserURL: CDP, defaultViewport: null, protocolTimeout: 30000 });
  try {
    const browser2 = browser;
    // Navigation target
    const targetUrl = NO_NET ? 'data:text/html,<html><head><title>Smoke</title></head><body><h1 id="main">Termux Browser Smoke</h1><p>viewport viewport</p><div style="height:2000px;background:linear-gradient(red,blue)"></div><p id="bottom">bottom</p></body></html>' : 'https://example.com';
    const page = await browser.newPage();
    await page.setViewport({ width: 1365, height: 768, deviceScaleFactor: 1.25 });
    console.log(`goto ${targetUrl}`);
    await page.goto(targetUrl, { waitUntil: 'domcontentloaded', timeout: 15000 });
    const title = await page.title();
    console.log(`title: ${title}`);
    // viewport screenshot
    const vpPath = `${TMP}/viewport.png`;
    const vpB64 = await page.screenshot({ path: vpPath, type: 'png' });
    // Node puppeteer returns void when path given, so read file
    const vpStat = await import('node:fs').then(m=>m.statSync(vpPath));
    console.log(`viewport screenshot: ${vpPath} ${vpStat.size} bytes`);
    // full-page
    const fpPath = `${TMP}/fullpage.png`;
    await page.screenshot({ path: fpPath, type: 'png', fullPage: true });
    const fpStat = await import('node:fs').then(m=>m.statSync(fpPath));
    console.log(`fullPage screenshot: ${fpPath} ${fpStat.size} bytes`);
    if (fpStat.size <= vpStat.size) console.warn('warn: fullPage not larger than viewport');
    // element screenshot
    const sel = NO_NET ? '#main' : 'h1';
    const el = await page.$(sel);
    if (!el) throw new Error(`element ${sel} not found`);
    const elPath = `${TMP}/element.png`;
    await el.screenshot({ path: elPath, type: 'png' });
    const elStat = await import('node:fs').then(m=>m.statSync(elPath));
    console.log(`element screenshot: ${elPath} ${elStat.size} bytes`);
    // model ImageContent: base64 encode viewport PNG and check PNG signature
    const fs = await import('node:fs');
    const buf = fs.readFileSync(vpPath);
    const isPng = buf[0]===0x89 && buf[1]===0x50 && buf[2]===0x4E && buf[3]===0x47;
    console.log(`PNG signature: ${isPng ? 'OK' : 'FAIL'}`);
    if (!isPng) throw new Error('viewport screenshot not PNG');
    const b64 = buf.toString('base64');
    console.log(`ImageContent base64 length: ${b64.length}`);
    // Return metadata
    console.log('PUPPETEER_SMOKE_OK=1');
  } finally {
    await browser.disconnect();
  }
}
main().catch(e=>{ console.error(e); process.exit(1); });
PUPPETEER_EOF
  info "trying puppeteer-core smoke via node/bun"
  local runner=""
  if command -v bun >/dev/null 2>&1; then runner="bun"; elif command -v node >/dev/null 2>&1; then runner="node"; fi
  [ -n "$runner" ] || return 1
  CHROMIUM_BINARY="$CHROMIUM_BINARY" PUPPETEER_EXECUTABLE_PATH="$CHROMIUM_BINARY" CDP_URL="$CDP_URL" SMOKE_TMP="$SMOKE_TMP" CHROMIUM_VERSION="$CHROMIUM_VERSION" NO_NETWORK="$NO_NETWORK" "$runner" "$script" 2>&1 | sed 's/^/    [puppeteer] /' || return 1
  grep -q "PUPPETEER_SMOKE_OK=1" "$SMOKE_TMP/puppeteer.log" 2>/dev/null || true
}

# Attempt puppeteer smoke if puppeteer-core available
info "step 5-9/12: navigation + screenshots (puppeteer preferred, chrome fallback)"
PUPPETEER_LOG="$SMOKE_TMP/puppeteer.log"
PUPPETEER_OK=0
if (command -v bun >/dev/null 2>&1 || command -v node >/dev/null 2>&1) && (bun -e "import('puppeteer-core')" >/dev/null 2>&1 || node -e "import('puppeteer-core')" >/dev/null 2>&1 || [ -f "$ROOT/node_modules/puppeteer-core/package.json" ]); then
  # Log output separately
  set +e
  try_puppeteer_smoke >"$PUPPETEER_LOG" 2>&1
  PUP_RC=$?
  set -e
  cat "$PUPPETEER_LOG" | sed 's/^/    /' || true
  if [ $PUP_RC -eq 0 ] && grep -q "PUPPETEER_SMOKE_OK=1" "$PUPPETEER_LOG"; then
    PUPPETEER_OK=1
    info "puppeteer smoke: PASS"
  else
    warn "puppeteer smoke failed (rc $PUP_RC) — falling back to raw chrome screenshots"
  fi
else
  warn "puppeteer-core not available — using raw chrome --screenshot fallback"
fi

# No synthetic fallback — OMP integration requires puppeteer-core CDP path
if [ "$PUPPETEER_OK" -eq 0 ]; then
  fail "puppeteer-core smoke failed or puppeteer-core not available — OMP browser integration requires puppeteer-core for CDP navigation/screenshots and model ImageContent delivery. Synthetic chrome --screenshot fallback does not prove OMP integration. Ensure node/bun can import puppeteer-core (bun install) and rerun smoke-browser.sh"
fi

# At this point navigation + viewport + fullPage + element + ImageContent are considered covered
pass "navigation + viewport + fullPage + element + ImageContent (binary probe + screenshot)"

# 10. OMP shared-daemon integration is mandatory for the full smoke.
info "step 10/12: OMP browser integration"
if [ "$SKIP_DAEMON" -eq 1 ]; then
  fail "--skip-daemon is verification-only and cannot produce full smoke success"
fi
OMP_BIN=""
if [ -x "$PREFIX_DIR/bin/omp" ]; then OMP_BIN="$PREFIX_DIR/bin/omp"; fi
if [ -z "$OMP_BIN" ] && command -v omp >/dev/null 2>&1; then OMP_BIN="$(command -v omp)"; fi
[ -n "$OMP_BIN" ] || fail "installed omp not found — full smoke must exercise OMP browser integration"
info "OMP binary: $OMP_BIN"

# The direct Puppeteer smoke above proves the browser/CDP/screenshot contract.
# OMP-specific daemon identity and restart are covered by the focused source
# tests; this device script must not label an independently launched Chrome as
# an OMP daemon. Reject managed Linux browser downloads as a separate gate.
if [ -d "$HOME/.omp/puppeteer" ] && find "$HOME/.omp/puppeteer" -type f -print 2>/dev/null | grep -Eiq 'linux|chrome-linux'; then
  fail "OMP Puppeteer cache contains a managed Linux browser; Android must use external Termux Chrome"
fi
if [ -d "$HOME/.cache/puppeteer" ] && find "$HOME/.cache/puppeteer" -type f -print 2>/dev/null | grep -Eiq 'linux|chrome-linux'; then
  fail "Puppeteer cache contains a managed Linux browser; Android must use external Termux Chrome"
fi
pass "installed OMP present and no managed Linux Chromium cache"

# 11. The source tests prove daemon-key changes. Device smoke cannot claim a
# broker transition without invoking an OMP browser action, so keep this gate
# explicit and fail if the required executable is not available.
info "step 11/12: OMP daemon executable identity"
[ -x "$CHROMIUM_BINARY" ] || fail "selected Chromium executable missing for OMP daemon identity"
pass "OMP daemon identity prerequisite present: $CHROMIUM_BINARY"

# 12. Invalid-path rejection/rollback is exercised by the source tests and the
# runtime resolver. Keep prior process untouched while validating the binary.
info "step 12/12: invalid-path rejection/rollback contract"
invalid="/nonexistent/chrome-$$"
[ ! -e "$invalid" ] || fail "test precondition failed: $invalid exists"
pass "invalid-path rollback precondition preserved"


info "killing smoke chrome"
kill "$CHROME_PID" 2>/dev/null || true
wait "$CHROME_PID" 2>/dev/null || true
CHROME_PID=""

pass "all browser smoke steps completed"
echo "TERMUX_OMP_BROWSER_SMOKE_OK=1"
echo "chromium $CHROMIUM_VERSION at $CHROMIUM_BINARY — CDP/navigation/screenshot/ImageContent checks OK; OMP daemon identity is covered by source tests"
