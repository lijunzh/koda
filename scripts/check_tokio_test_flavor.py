#!/usr/bin/env python3
"""CI guard for #1109 F2: every `#[tokio::test]` in a Rust file that
exercises tokio::spawn / BgAgentRegistry / sub-agent dispatch / watch /
broadcast channels MUST use `flavor = "multi_thread"` to match production
runtime semantics.

The default `current_thread` runtime queues `tokio::spawn`'d futures but
only polls them when the test future yields — which doesn't happen on a
single-task await chain. Production runs multi-thread, so the test
flavor mismatch silently hides bugs (#1090, etc.).

Usage:
    python3 scripts/check_tokio_test_flavor.py           # check workspace
    python3 scripts/check_tokio_test_flavor.py --fix     # auto-promote (local dev)

Exit codes:
    0 — all clear
    1 — at least one offending test found

This is intentionally a Python script rather than a clippy lint or rustc
plugin because (a) zero added build deps, (b) it runs in <1s, (c) it
greps for textual patterns which is exactly the granularity we want
(adding a clippy lint would require nightly/macro_rules wizardry).

Refs #1109 (Phase 2 / F2).
"""
from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

# Modules that exercise spawn-style concurrency. If a file imports any of
# these patterns AND has a bare `#[tokio::test]`, the test is at risk.
DANGER_PATTERNS = re.compile(
    r"\btokio::spawn\b"
    r"|\bBgAgentRegistry\b"
    r"|\bsub_agent_dispatch\b"
    r"|\btokio::sync::watch\b"
    r"|\btokio::sync::broadcast\b"
)

# Bare `#[tokio::test]` — flavor not specified.
BARE_TOKIO_TEST = re.compile(r"#\[tokio::test\](?!\()")
# What we want it replaced with.
MULTI_THREAD_TEST = '#[tokio::test(flavor = "multi_thread", worker_threads = 2)]'

# Skip these directories under workspace root.
SKIP_DIRS = {"target", ".git", "node_modules", "dist", "build"}


def find_rs_files(root: Path) -> list[Path]:
    """Walk root, return every .rs file outside SKIP_DIRS."""
    files = []
    for p in root.rglob("*.rs"):
        if any(part in SKIP_DIRS for part in p.parts):
            continue
        files.append(p)
    return files


def check_file(path: Path) -> list[tuple[int, str]]:
    """Return [(line_no, line)] for every offending bare #[tokio::test]
    in `path`. Empty list = file is clean (or doesn't import any
    danger patterns)."""
    src = path.read_text(encoding="utf-8", errors="replace")
    if not DANGER_PATTERNS.search(src):
        return []
    offenders: list[tuple[int, str]] = []
    for i, line in enumerate(src.splitlines(), start=1):
        if BARE_TOKIO_TEST.search(line):
            offenders.append((i, line.strip()))
    return offenders


def fix_file(path: Path) -> int:
    """Promote every bare #[tokio::test] in `path` to multi_thread.
    Returns the count of replacements made."""
    src = path.read_text(encoding="utf-8", errors="replace")
    new_src, n = BARE_TOKIO_TEST.subn(MULTI_THREAD_TEST, src)
    if n > 0:
        path.write_text(new_src, encoding="utf-8")
    return n


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--fix",
        action="store_true",
        help="Auto-promote offenders (developer convenience).",
    )
    parser.add_argument(
        "--root",
        type=Path,
        default=Path(__file__).resolve().parent.parent,
        help="Workspace root (default: repo root inferred from script location).",
    )
    args = parser.parse_args()

    files = find_rs_files(args.root)

    if args.fix:
        total = 0
        changed_files = 0
        for f in files:
            src = f.read_text(encoding="utf-8", errors="replace")
            if not DANGER_PATTERNS.search(src):
                continue
            n = fix_file(f)
            if n:
                changed_files += 1
                total += n
                print(f"  promoted {n:3d} test(s) in {f.relative_to(args.root)}")
        print(f"\nTotal: {total} test(s) promoted across {changed_files} file(s).")
        return 0

    # Default mode: report only.
    bad: list[tuple[Path, list[tuple[int, str]]]] = []
    for f in files:
        offenders = check_file(f)
        if offenders:
            bad.append((f, offenders))

    if not bad:
        print("[#1109 F2 guard] OK — every #[tokio::test] in danger-pattern modules uses multi_thread.")
        return 0

    print("[#1109 F2 guard] FAIL — found bare #[tokio::test] in modules that")
    print("                 use tokio::spawn / BgAgentRegistry / sub-agent dispatch /")
    print("                 watch / broadcast channels. These tests run on the")
    print("                 current_thread runtime by default, which silently")
    print("                 hides spawned-future bugs (see #1090).")
    print()
    print("Offenders:")
    for path, offenders in bad:
        rel = path.relative_to(args.root)
        for ln, line in offenders:
            print(f"  {rel}:{ln}: {line}")
    print()
    print("Fix: add (flavor = \"multi_thread\", worker_threads = 2) to each.")
    print("Or run: python3 scripts/check_tokio_test_flavor.py --fix")
    return 1


if __name__ == "__main__":
    sys.exit(main())
