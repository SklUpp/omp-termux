#!/usr/bin/env python3
"""Verify Android overlay is present in checked-out release tree."""
from pathlib import Path
import json
import os
import re
ROOT = Path.cwd()

def read(path):
    return (ROOT / path).read_text()

def require(path, needle):
    if needle not in read(path):
        raise SystemExit(f"overlay verification failed: {path}: missing {needle!r}")

version = json.loads(read("packages/coding-agent/package.json"))["version"]
tag = os.environ.get("RELEASE_TAG", "")
if tag and not re.fullmatch(rf"v{re.escape(version)}-termux(?:-r[0-9a-f]{{12}})?", tag):
    raise SystemExit(
        f"version/tag mismatch: package={version}, tag={tag}; expected "
        f"v{version}-termux or v{version}-termux-r<12 lowercase hex>"
    )

checks = {
    "crates/pi-natives/src/lib.rs": "#![cfg_attr(not(target_os = \"android\"), feature(alloc_error_hook))]",
    "crates/pi-natives/src/crash_handler.rs": "#[cfg(not(target_os = \"android\"))]",
    "crates/pi-natives/src/clipboard.rs": '#[cfg(target_os = "android")]',
    "crates/pi-shell/src/process.rs": '#[cfg(any(target_os = "linux", target_os = "android"))]',
    "crates/pi-builtins/src/proc_snapshot.rs": '#[cfg(any(target_os = "linux", target_os = "android"))]',
    "crates/pi-builtins/src/ps.rs": '#[cfg(any(target_os = "linux", target_os = "android"))]\nfn ps_total_memory_bytes',
    "packages/natives/native/loader-state.js": '"android-arm64"',
}

if checks["crates/pi-natives/src/lib.rs"] not in read("crates/pi-natives/src/lib.rs"):
    raise SystemExit("overlay verification failed: alloc_error_hook cfg missing")
for path, needle in checks.items():
    if path.endswith("/lib.rs"):
        continue
    if not (ROOT / path).exists():
        if path.startswith("crates/pi-builtins/"):
            continue
        raise SystemExit(f"overlay verification failed: required file missing: {path}")
    require(path, needle)

browser_path = ROOT / "packages/coding-agent/src/tools/browser/launch.ts"
if not browser_path.is_file():
    raise SystemExit("overlay verification failed: browser launch file missing")
browser_source = browser_path.read_text()
if 'case "android"' not in browser_source or "systemChromiumCandidates" not in browser_source:
    raise SystemExit("overlay verification failed: Android browser resolution missing")

present = sum((ROOT / path).exists() for path in checks)
print(f"Android overlay verified: {present} gates + browser overlay, version {version}")
