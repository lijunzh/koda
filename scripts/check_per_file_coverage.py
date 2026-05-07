#!/usr/bin/env python3
"""CI guard for #1265 item 8c: per-file line-coverage thresholds for
security-boundary modules.

The existing crate-level gate in ``coverage.yml`` requires
``koda-core`` and ``koda-sandbox`` lib code to stay above 80% lines.
That's a coarse safety net: a single security-critical file can
regress (e.g. ``web_fetch.rs`` going from 81% → 60%) while the crate
average stays above the bar, because hundreds of low-risk files dilute
the signal.

This script tightens the gate around the files that protect SSRF,
symlink/TOCTOU, and sandbox path-escape boundaries — the same set
that #1265 item 8b's structural guard enumerates. Thresholds are set
at the current measured baseline minus a small buffer (3-5%) to
absorb instrumentation noise without inviting drift.

Usage:
    python3 scripts/check_per_file_coverage.py REPORT.json [REPORT.json ...]

Each REPORT.json is a ``cargo llvm-cov --json`` output. The script
unions across reports — useful when separate runs cover separate
crates. A file's effective coverage is the *highest* observed across
all input reports (any report that includes the file is sufficient
proof that lines are covered).

Exit codes:
    0 — every applicable file at or above its threshold
    1 — at least one file below threshold
    2 — reports were malformed or none of the configured files appeared
        in any report (probably a cargo-llvm-cov filter mistake — fail
        loud rather than silently passing on zero data)

Why a Python script (not a clippy lint, not inline bash):
    1. Per-file threshold config is data, not regex. Bash arrays would
       be hard to extend; YAML would mean another parser dep.
    2. JSON parsing and percentage arithmetic want a real language.
    3. Mirrors the existing scripts/check_tokio_test_flavor.sh pattern.

The threshold table is small and intentionally lives in this script
(not a separate config file) so the policy and its enforcement are
co-located: any change requires editing this file, which means the
review touches the rationale.
"""

from __future__ import annotations

import argparse
import json
import sys
from dataclasses import dataclass
from pathlib import Path


# ── Threshold table ────────────────────────────────────────────────────
#
# Each entry: (path-suffix to match, minimum line-coverage percent,
#              short reason / what regression would mean).
#
# `path-suffix` is matched against the END of the absolute path in the
# lcov JSON. Suffix matching (not basename) prevents a future
# `tools/web_fetch.rs` in another crate from accidentally satisfying
# the same gate as `koda-core/src/tools/web_fetch.rs`.
#
# Thresholds: current_baseline - 3% to ~5% buffer. Rounded down to
# 0.5% for memorability. Re-measure when raising; lowering needs an
# explicit comment + #1265 reference.
#
# Baseline measurements (2026-05-06, macOS, --features
# koda-core/test-support, --skip pool::tests for sandbox):
#
#   tools/web_fetch.rs        81.27%   →  78%  (SSRF defense)
#   tools/file_tools.rs       91.29%   →  88%  (TOCTOU/symlink defense)
#   engine/sink.rs            87.48%   →  84%  (just refactored 8a-PR2)
#   policy.rs                 97.31%   →  94%  (sandbox effect classification)
#   seatbelt.rs               98.10%   →  95%  (macOS sandbox profile)
#   workspace.rs              88.58%   →  85%  (workspace isolation)
#   worker.rs                 83.21%   →  80%  (sandbox worker entrypoint)
#   fs/policy.rs              82.21%   →  78%  (path-confinement primitives)
#
# Linux-only files (e.g. bwrap.rs) aren't in macOS reports; the
# `optional` flag below means we don't fail when the file is absent
# from any report — but if it IS present (Linux run), it's gated.
THRESHOLDS: list[tuple[str, float, str]] = [
    ("koda-core/src/tools/web_fetch.rs", 78.0, "SSRF + redirect defense (#1282)"),
    ("koda-core/src/tools/file_tools.rs", 88.0, "TOCTOU / symlink confinement (#1283)"),
    ("koda-core/src/engine/sink.rs", 84.0, "PersistingSink classifier + readiness helpers"),
    ("koda-sandbox/src/policy.rs", 94.0, "sandbox effect classification"),
    ("koda-sandbox/src/seatbelt.rs", 95.0, "macOS sandbox profile generation"),
    ("koda-sandbox/src/workspace.rs", 85.0, "workspace isolation + clonefile"),
    ("koda-sandbox/src/worker.rs", 80.0, "sandbox worker IPC entrypoint"),
    ("koda-sandbox/src/fs/policy.rs", 78.0, "path-confinement primitives"),
    # Linux-only — present only in Linux coverage runs.
    ("koda-sandbox/src/bwrap.rs", 75.0, "Linux bubblewrap profile (Linux-only)"),
]

# Files in THRESHOLDS that may legitimately be absent from a given
# report (platform-specific). Keyed by suffix; value is the platform
# where it IS expected. Used only for nicer messaging.
PLATFORM_NOTES: dict[str, str] = {
    "koda-sandbox/src/bwrap.rs": "Linux only",
    "koda-sandbox/src/seatbelt.rs": "macOS only",
    "koda-sandbox/src/workspace.rs": "macOS clonefile path is macOS only",
}


@dataclass
class FileCoverage:
    """Aggregated line coverage for one file across input reports."""

    suffix: str
    threshold: float
    reason: str
    best_pct: float | None = None  # None = not seen in any report
    best_count: int = 0
    best_covered: int = 0
    best_source_report: str = ""

    @property
    def status(self) -> str:
        if self.best_pct is None:
            return "absent"
        if self.best_pct + 1e-9 < self.threshold:
            return "below"
        return "ok"


def _load_report(path: Path) -> dict:
    try:
        with path.open() as f:
            return json.load(f)
    except (OSError, json.JSONDecodeError) as exc:
        print(f"::error::failed to read report {path}: {exc}", file=sys.stderr)
        sys.exit(2)


def _files_from_report(report: dict) -> list[dict]:
    # cargo-llvm-cov JSON structure: data[0].files[*]
    try:
        return report["data"][0]["files"]
    except (KeyError, IndexError, TypeError) as exc:
        print(
            f"::error::report has unexpected shape (no data[0].files): {exc}",
            file=sys.stderr,
        )
        sys.exit(2)


def collect(reports: list[Path]) -> list[FileCoverage]:
    """Aggregate per-file coverage across all reports.

    Returns one FileCoverage per THRESHOLDS entry, with the best
    observed percentage filled in (or left as None if no report
    mentioned the file).
    """
    out: list[FileCoverage] = [
        FileCoverage(suffix=s, threshold=t, reason=r) for s, t, r in THRESHOLDS
    ]

    for report_path in reports:
        report = _load_report(report_path)
        files = _files_from_report(report)
        for f in files:
            fn = f.get("filename", "")
            for entry in out:
                if not fn.endswith(entry.suffix):
                    continue
                summary = f.get("summary", {}).get("lines", {})
                pct = summary.get("percent")
                if pct is None:
                    continue
                if entry.best_pct is None or pct > entry.best_pct:
                    entry.best_pct = pct
                    entry.best_count = summary.get("count", 0)
                    entry.best_covered = summary.get("covered", 0)
                    entry.best_source_report = report_path.name
                break  # one suffix matches at most one entry per file

    return out


def render(rows: list[FileCoverage]) -> tuple[int, int, int]:
    """Print a table; return (ok, below, absent) counts."""
    print(f"{'FILE':<45} {'COVERED':>10} {'THRESHOLD':>10} {'STATUS':>8}")
    print("-" * 80)
    ok = below = absent = 0
    for r in rows:
        if r.best_pct is None:
            note = PLATFORM_NOTES.get(r.suffix, "missing from all reports")
            print(f"{r.suffix:<45} {'—':>10} {r.threshold:>9.2f}% {'absent':>8}  ({note})")
            absent += 1
            continue
        marker = "  ✓" if r.status == "ok" else "  ✗"
        print(
            f"{r.suffix:<45} {r.best_pct:>9.2f}% {r.threshold:>9.2f}% {r.status:>8}{marker}"
        )
        if r.status == "ok":
            ok += 1
        else:
            below += 1
    return ok, below, absent


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument(
        "reports",
        nargs="+",
        type=Path,
        help="One or more cargo-llvm-cov JSON report files",
    )
    parser.add_argument(
        "--allow-absent",
        action="store_true",
        help=(
            "Don't fail when a configured file is absent from every "
            "report. Use only on a single-platform run that legitimately "
            "lacks the platform-gated files (e.g. macOS run lacking bwrap.rs). "
            "Default behaviour fails on absent files to catch typos in "
            "the threshold suffix list."
        ),
    )
    args = parser.parse_args()

    rows = collect(args.reports)
    ok, below, absent = render(rows)

    print()
    print(f"Summary: {ok} ok, {below} below threshold, {absent} absent")

    if below:
        print()
        print("::error::Per-file coverage regression detected:")
        for r in rows:
            if r.status == "below":
                drop = r.threshold - r.best_pct
                print(
                    f"  - {r.suffix}: {r.best_pct:.2f}% (threshold {r.threshold:.2f}%, "
                    f"down {drop:.2f}%) — {r.reason}"
                )
        print()
        print("To fix: add tests covering the regressed lines in the file(s) above.")
        print("To intentionally lower a threshold: edit THRESHOLDS in this script and")
        print("document the rationale on issue #1265.")
        return 1

    if absent and not args.allow_absent:
        # Only fail on absent if the absent files aren't all platform-noted.
        unexplained_absent = [
            r for r in rows if r.status == "absent" and r.suffix not in PLATFORM_NOTES
        ]
        if unexplained_absent:
            print()
            print("::error::Configured files were absent from every report:")
            for r in unexplained_absent:
                print(f"  - {r.suffix}")
            print()
            print(
                "If this is expected (e.g. file deleted), remove from THRESHOLDS in "
                "scripts/check_per_file_coverage.py."
            )
            print("If running on a single platform that lacks platform-gated files,")
            print("pass --allow-absent.")
            return 2

    print("✓ All gated files at or above their per-file thresholds.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
