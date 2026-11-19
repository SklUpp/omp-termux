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
    parser.add_argument(
        "--profile",
        choices=("standard", "extended"),
        default=os.environ.get("OMP_PATCH_PROFILE", "extended"),
        help="Patch profile to validate",
    )
    parser.add_argument(
        "--variant",
        choices=("standard", "extended"),
        default=os.environ.get("OMP_RELEASE_VARIANT"),
        help="Release variant; defaults to profile",
    )
    return parser.parse_args()

def main():
    args = parse_args()
    root = args.root.resolve()
    profile = args.profile
    variant = args.variant or profile
    if variant != profile:
        sys.exit(f"error: release variant '{variant}' does not match patch profile '{profile}'")

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
        if variant == "extended":
            safe_tag = rf"{re.escape(expected_tag)}-ex(?:-r[0-9a-f]{{12}})?"
        else:
            safe_tag = rf"{re.escape(expected_tag)}(?:-r[0-9a-f]{{12}})?"
        if not re.fullmatch(safe_tag, release_tag):
            if variant == "extended":
                expected_display = f"{expected_tag}-ex"
                collision = f"{expected_display}-r<12 lowercase hex>"
            else:
                expected_display = expected_tag
                collision = f"{expected_tag}-r<12 lowercase hex>"
            sys.exit(
                f"error: RELEASE_TAG mismatch for {variant}: got '{release_tag}', expected "
                f"'{expected_display}' or collision form '{collision}' (package version: {version})"
            )

    # 3. Require patch queue metadata and key patched-tree marker files
    manifest_name = "MANIFEST.standard" if profile == "standard" else "MANIFEST"
    required_patch_markers = [
        "android/scripts/apply-patches.sh",
        f"android/patches/{manifest_name}",
        "android/scripts/build-termux.sh",
    ]
    for marker in required_patch_markers:
        marker_path = root / marker
        if not marker_path.is_file():
            sys.exit(f"error: missing required patch queue marker file: {marker}")

    manifest_path = root / "android" / "patches" / manifest_name
    active_patches = sorted(
        p.name for p in manifest_path.parent.glob("[0-9][0-9]-*.patch")
    )
    manifest_entries = [
        line.strip()
        for line in manifest_path.read_text(encoding="utf-8").splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    ]
    if len(manifest_entries) != len(set(manifest_entries)):
        sys.exit(f"error: duplicate patch entries in {manifest_name}")
    if profile == "extended":
        if manifest_entries != active_patches:
            sys.exit(
                "error: active patch queue does not match MANIFEST: "
                f"manifest={manifest_entries}, files={active_patches}"
            )
    else:
        missing = [entry for entry in manifest_entries if entry not in active_patches]
        if missing:
            sys.exit(f"error: standard manifest references missing patches: {missing}")
        if "02-isolated-revive.patch" in manifest_entries:
            sys.exit("error: standard manifest must not include 02-isolated-revive.patch")
        for marker in (
            "packages/coding-agent/test/cli/worktree-release-isolation.test.ts",
            "packages/coding-agent/test/fixtures/isolation-lease-child.ts",
            "packages/coding-agent/test/task/isolation-retained.test.ts",
        ):
            if (root / marker).exists():
                sys.exit(f"error: extended-only file present in standard tree: {marker}")

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
