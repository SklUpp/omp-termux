#!/usr/bin/env python3
"""Preflight CLI for release input validation in CI."""
import argparse
import json
import os
import re
import sys
from pathlib import Path

def parse_args():
    parser = argparse.ArgumentParser(description="Validate release inputs before build.")
    parser.add_argument("--root", type=Path, default=Path.cwd(), help="Root directory of release staging tree")
    return parser.parse_args()

def main():
    args = parse_args()
    root = args.root.resolve()

    # 1. Require package.json, bun.lock, and Android runtime manifest.
    pkg_json_path = root / "package.json"
    bun_lock_path = root / "bun.lock"
    versions_env = root / "android" / "versions.env"

    if not pkg_json_path.is_file():
        sys.exit(f"error: missing required package.json at {pkg_json_path}")
    if not bun_lock_path.is_file():
        sys.exit(f"error: missing required bun.lock at {bun_lock_path}")
    if not versions_env.is_file():
        sys.exit(f"error: missing required {versions_env}")

    manifest = {}
    for line in versions_env.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line and not line.startswith("#") and "=" in line:
            key, value = line.split("=", 1)
            manifest[key] = value.strip()
    for key in ("BUN_VERSION", "BUN_ARCHIVE_NAME", "BUN_SHA256"):
        if not manifest.get(key):
            sys.exit(f"error: {key} not specified in {versions_env}")

    # 2. Package version check against optional RELEASE_TAG
    coding_agent_pkg = root / "packages" / "coding-agent" / "package.json"
    target_pkg = coding_agent_pkg if coding_agent_pkg.is_file() else pkg_json_path

    try:
        data = json.loads(target_pkg.read_text(encoding="utf-8"))
        version = data.get("version")
    except Exception as err:
        sys.exit(f"error: failed to read version from {target_pkg}: {err}")

    if not version:
        sys.exit(f"error: no version specified in {target_pkg}")

    release_tag = os.environ.get("RELEASE_TAG", "").strip()
    if release_tag:
        expected_tag = f"v{version}-termux"
        safe_tag = rf"{re.escape(expected_tag)}(?:-r[0-9a-f]{{12}})?"
        if not re.fullmatch(safe_tag, release_tag):
            sys.exit(
                f"error: RELEASE_TAG mismatch: got '{release_tag}', expected '{expected_tag}' "
                f"or collision form '{expected_tag}-r<12 lowercase hex>' (package version: {version})"
            )

    # 3. Require patch queue metadata and key patched-tree marker files
    required_patch_markers = [
        "android/scripts/apply-patches.sh",
        "android/patches/MANIFEST",
        "android/scripts/build-termux.sh",
    ]
    for marker in required_patch_markers:
        marker_path = root / marker
        if not marker_path.is_file():
            sys.exit(f"error: missing required patch queue marker file: {marker}")

    manifest_path = root / "android" / "patches" / "MANIFEST"
    active_patches = sorted(
        p.name for p in manifest_path.parent.glob("[0-9][0-9]-*.patch")
    )
    manifest_entries = [
        line.strip()
        for line in manifest_path.read_text(encoding="utf-8").splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    ]
    if manifest_entries != active_patches:
        sys.exit(
            "error: active patch queue does not match MANIFEST: "
            f"manifest={manifest_entries}, files={active_patches}"
        )

    required_overlay_markers = [
        "packages/natives/native/loader-state.js",
    ]
    for marker in required_overlay_markers:
        marker_path = root / marker
        if not marker_path.is_file():
            sys.exit(f"error: missing required patched-tree marker file: {marker}")

    # 4. Reject runtime cache directories in fresh staging tree
    # In CI, checkout is fresh before bun install.
    if os.environ.get("CI_CHECK_STAGING") == "1":
        reject_dirs = [
            "node_modules",
            "target",
            ".cache",
        ]
        found_rejects = [d for d in reject_dirs if (root / d).exists()]
        if found_rejects:
            sys.exit(f"error: staging tree contains runtime cache directory: {', '.join(found_rejects)}")

    print(f"Release preflight passed for version {version}")

if __name__ == "__main__":
    main()
