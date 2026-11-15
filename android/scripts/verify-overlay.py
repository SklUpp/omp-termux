#!/usr/bin/env python3
"""Verify Android overlay is present in checked-out release tree."""
from pathlib import Path
import json
import os
import re
import sys
ROOT = Path.cwd()

def read(path):
    return (ROOT / path).read_text()

def require(path, needle):
    if needle not in read(path):
        raise SystemExit(f"overlay verification failed: {path}: missing {needle!r}")

def read_required(path):
    file = ROOT / path
    if not file.is_file():
        raise SystemExit(f"overlay verification failed: required file missing: {path}")
    return file.read_text()

version = json.loads(read("packages/coding-agent/package.json"))["version"]
tag = os.environ.get("RELEASE_TAG", "")
profile = os.environ.get("OMP_PATCH_PROFILE", "extended")
variant = os.environ.get("OMP_RELEASE_VARIANT") or profile
if profile not in ("standard", "extended"):
    raise SystemExit(f"version/tag mismatch: unknown patch profile {profile!r}")
if variant not in ("standard", "extended"):
    raise SystemExit(f"version/tag mismatch: unknown release variant {variant!r}")
if variant != profile:
    raise SystemExit(
        f"version/tag mismatch: release variant {variant!r} does not match patch profile {profile!r}"
    )
expected_tag = f"v{version}-termux" + ("-ex" if variant == "extended" else "")
if tag and not re.fullmatch(rf"{re.escape(expected_tag)}(?:-r[0-9a-f]{{12}})?", tag):
    raise SystemExit(
        f"version/tag mismatch: package={version}, tag={tag}; expected "
        f"{expected_tag} or {expected_tag}-r<12 lowercase hex>"
    )
if "--check-tag" in sys.argv:
    print(f"release tag verified: {tag or '<unset>'} ({variant})")
    raise SystemExit(0)

checks = {
    "crates/pi-natives/src/lib.rs": "#![cfg_attr(not(target_os = \"android\"), feature(alloc_error_hook))]",
    "crates/pi-natives/src/crash_handler.rs": "#[cfg(not(target_os = \"android\"))]",
    "crates/pi-natives/src/clipboard.rs": '#[cfg(target_os = "android")]',
    "crates/pi-shell/src/process.rs": '#[cfg(any(target_os = "linux", target_os = "android"))]',
    "crates/pi-builtins/src/proc_snapshot.rs": '#[cfg(any(target_os = "linux", target_os = "android"))]',
    "crates/pi-builtins/src/ps.rs": '#[cfg(any(target_os = "linux", target_os = "android"))]\nfn ps_total_memory_bytes',
    "crates/pi-builtins/src/sort.rs": '#[cfg(any(target_os = "linux", target_os = "android"))]\nfn linux_cgroup_memory_headroom',
    "packages/coding-agent/src/utils/clipboard.ts": "clipboard copy failed:",
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
if "No working Chromium executable found on Android" not in browser_source:
    raise SystemExit("overlay verification failed: Android browser install error missing")

update_path = ROOT / "packages/coding-agent/src/cli/update-cli.ts"
if not update_path.is_file() or "isAndroidUpdateHost" not in update_path.read_text():
    raise SystemExit("overlay verification failed: Android update guard missing")
if "isAndroidUpdateHost" not in read_required("packages/coding-agent/src/main.ts"):
    raise SystemExit("overlay verification failed: Android startup update gate missing")

endpoint_path = ROOT / "crates/pi-natives/src/ipc/endpoint.rs"
if "android_relocation_dir" not in read_required("crates/pi-natives/src/ipc/endpoint.rs"):
    raise SystemExit("overlay verification failed: Android IPC relocation policy missing")
if "ensure_trusted_base" not in read_required("crates/pi-natives/src/ipc/unix.rs"):
    raise SystemExit("overlay verification failed: IPC relocation directory validation missing")
if "OMP_ANDROID_PREFIX" not in read_required("install.sh"):
    raise SystemExit("overlay verification failed: installed IPC prefix export missing")
if "sendAndroidNotification" not in read_required(
    "packages/coding-agent/src/modes/controllers/event-controller.ts"
):
    raise SystemExit("overlay verification failed: Android notification wiring missing")
if "newAndroidAskNotificationId" not in read_required(
    "packages/coding-agent/src/utils/android-helpers.ts"
):
    raise SystemExit("overlay verification failed: per-process Android notification id missing")
if "await clearAndroidAskCard();" not in read_required("packages/coding-agent/src/tools/ask.ts"):
    raise SystemExit("overlay verification failed: ask notification cleanup missing")

sort_source = read_required("crates/pi-builtins/src/sort.rs")
for needle in ("/proc/meminfo", "/proc/self/cgroup", "physical_memory_bytes()"):
    if needle not in sort_source:
        raise SystemExit(f"overlay verification failed: sort memory fallback missing: {needle}")
if 'if let Some(bytes) =' not in sort_source:
    raise SystemExit("overlay verification failed: sort procfs read is not fallible")

clipboard_source = read_required("packages/coding-agent/src/utils/clipboard.ts")
for needle in (
    "termux-clipboard-set",
    "termux-clipboard-get",
    "\\x1b]52;c;",
    "const osc52Emitted",
    "let delivered = osc52Emitted;",
    "if (!delivered)",
):
    if needle not in clipboard_source:
        raise SystemExit(f"overlay verification failed: clipboard contract missing: {needle}")

terminal_source = read_required("packages/tui/src/active-terminal.ts")
if "export function writeTerminalSequence(data: string): boolean" not in terminal_source:
    raise SystemExit("overlay verification failed: writeTerminalSequence cannot report emission")

copy_command = read_required("packages/coding-agent/src/slash-commands/builtin-collaboration.ts")
if "Copy failed: ${errorMessage(error)}" not in copy_command:
    raise SystemExit("overlay verification failed: /copy does not report a rejected copy")
if "} finally {\n\t\t\t\tclearSubmittedText(runtime);" not in copy_command:
    raise SystemExit("overlay verification failed: /copy does not clear the draft after a rejection")

native_index = read_required("packages/natives/native/index.js")
if ": (nativeBindings.copyToClipboard ?? missingNativeExport(\"copyToClipboard\"));" not in native_index:
    raise SystemExit("overlay verification failed: Android clipboard proxy masks desktop stale-addon diagnostics")
if 'process.platform === "android"' not in native_index:
    raise SystemExit("overlay verification failed: Android-only native stubs are not platform-gated")

# `bun run build` regenerates index.js from this script, so the generator must
# emit the same platform gate or the next bindings build undoes the patch.
gen_enums = read_required("packages/natives/scripts/gen-enums.ts")
for needle in ('androidUnsupported', 'process.platform === \"android\"', 'missingNativeExport(\"${name}\")'):
    if needle not in gen_enums:
        raise SystemExit(f"overlay verification failed: gen-enums lost the platform gate: {needle}")

clipboard_rs = read_required("crates/pi-natives/src/clipboard.rs")
if '#[cfg(all(test, not(target_os = "android")))]\nmod tests {' not in clipboard_rs:
    raise SystemExit("overlay verification failed: Android cargo test would import the cfg-gated dib_to_png")

for path in ("crates/pi-builtins/src/proc_match.rs", "crates/pi-natives/src/held_files/mod.rs"):
    text = read_required(path)
    if 'target_os = "android"' in text:
        raise SystemExit(
            f"overlay verification failed: {path} must not enable pidfd/OFD without a device proof"
        )
if "relocated endpoint is stable across processes" not in read_required(
    "packages/natives/test/ipc.test.ts"
):
    raise SystemExit("overlay verification failed: IPC cross-process regression test missing")

present = sum((ROOT / path).exists() for path in checks)
print(f"Android overlay verified: {present} gates + browser/update/ipc overlays, version {version}")
