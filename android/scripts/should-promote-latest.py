#!/usr/bin/env python3
"""Decide whether a standard release may become GitHub latest.

Usage: should-promote-latest.py <candidate-tag> [<current-latest-tag>]

Prints "true" or "false". Fails closed (false) for malformed input or an
extended candidate. Equal-version standard candidates may replace another
build (including legacy extended tags), but older versions are never promoted.
"""
from __future__ import annotations

import re
import sys

STANDARD = re.compile(r"^v(\d+)\.(\d+)\.(\d+)-termux(?:-r[0-9a-f]{12})?$")
EXTENDED = re.compile(r"^v(\d+)\.(\d+)\.(\d+)-termux-ex(?:-r[0-9a-f]{12})?$")


def version(tag: str) -> tuple[int, int, int] | None:
    match = STANDARD.fullmatch(tag) or EXTENDED.fullmatch(tag)
    if not match:
        return None
    return tuple(int(part) for part in match.groups())


def main() -> int:
    args = [arg for arg in sys.argv[1:] if arg != "--strict"]
    strict = "--strict" in sys.argv[1:]
    if len(args) not in (1, 2):
        print(
            "usage: should-promote-latest.py [--strict] <candidate-tag> [<current-latest-tag>]",
            file=sys.stderr,
        )
        return 2

    candidate = args[0]
    current = args[1] if len(args) == 2 else ""
    candidate_version = version(candidate)
    if candidate_version is None or EXTENDED.fullmatch(candidate):
        print("false")
        return 0

    if not current:
        print("true")
        return 0
    if current == candidate:
        print("true")
        return 0

    current_version = version(current)
    if current_version is None:
        if strict:
            print(f"error: current latest tag is invalid: {current!r}", file=sys.stderr)
            return 2
        print("false")
        return 0

    if EXTENDED.fullmatch(current):
        # Standard is the only normal latest channel. Promote it over an
        # extended latest only when it is not older than that release.
        print("true" if candidate_version >= current_version else "false")
        return 0

    # Equal-version standard candidates may replace another build (including
    # legacy extended tags that predate the -ex suffix), but never older.
    print("true" if candidate_version >= current_version else "false")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
