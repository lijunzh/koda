#!/usr/bin/env bash
# CI guard for #1265 item 8c: per-file line-coverage thresholds for
# security-boundary modules.
#
# The crate-level gate in `coverage.yml` requires `koda-core` and
# `koda-sandbox` lib code to stay above 80% lines. That's a coarse
# safety net: a single security-critical file can regress (e.g.
# `web_fetch.rs` going from 81% → 60%) while the crate average stays
# above the bar, because hundreds of low-risk files dilute the signal.
# `cargo llvm-cov --fail-under-lines` only gates a whole report, so the
# per-file check lives here.
#
# This script tightens the gate around the files that protect SSRF,
# symlink/TOCTOU, and sandbox path-escape boundaries — the same set
# that #1265 item 8b's structural guard enumerates. Thresholds are set
# at the measured baseline minus a small buffer (3-5%) to absorb
# instrumentation noise without inviting drift.
#
# Usage:
#   scripts/check_per_file_coverage.sh REPORT.json [REPORT.json ...]
#   scripts/check_per_file_coverage.sh --allow-absent REPORT.json ...
#
# Each REPORT.json is a `cargo llvm-cov --json` output. A file's
# effective coverage is the *highest* observed across all reports (any
# report that includes the file is sufficient proof that lines are
# covered), which is useful when separate runs cover separate crates.
#
# Exit codes:
#   0 — every applicable file at or above its threshold
#   1 — at least one file below threshold
#   2 — a report was malformed, or a configured file without a platform
#       note appeared in no report (probably a typo in the table or a
#       cargo-llvm-cov filter mistake — fail loud rather than silently
#       passing on zero data); --allow-absent turns the latter off
#
# Bash + jq port of the original Python script, for the same reason as
# check_tokio_test_flavor.sh (#1328): no Python in a Rust repo. jq is
# preinstalled on the GitHub-hosted Ubuntu and macOS runners.
#
# Refs #1265 (8c), #1309 (original script), #1328.

set -euo pipefail

# ── Threshold table ────────────────────────────────────────────────────
#
# Tab-separated: path suffix, minimum line-coverage percent, platform
# note, reason.
#
# The suffix is matched against the END of the absolute path in the
# report. Suffix matching (not basename) keeps a future
# `tools/web_fetch.rs` in another crate from satisfying the gate meant
# for `koda-core/src/tools/web_fetch.rs`.
#
# The platform note ("-" for none) marks a file that is legitimately
# absent from one platform's report; it is gated wherever it appears.
#
# Thresholds: baseline minus 3% to ~5%, rounded down to 0.5% for
# memorability. Re-measure when raising; lowering needs an explicit
# comment and a #1265 reference.
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
THRESHOLDS=$(cat <<'EOF'
koda-core/src/tools/web_fetch.rs	78.0	-	SSRF + redirect defense (#1282)
koda-core/src/tools/file_tools.rs	88.0	-	TOCTOU / symlink confinement (#1283)
koda-core/src/engine/sink.rs	84.0	-	PersistingSink classifier + readiness helpers
koda-sandbox/src/policy.rs	94.0	-	sandbox effect classification
koda-sandbox/src/seatbelt.rs	95.0	macOS only	macOS sandbox profile generation
koda-sandbox/src/workspace.rs	85.0	macOS clonefile path is macOS only	workspace isolation + clonefile
koda-sandbox/src/worker.rs	80.0	-	sandbox worker IPC entrypoint
koda-sandbox/src/fs/policy.rs	78.0	-	path-confinement primitives
koda-sandbox/src/bwrap.rs	75.0	Linux only	Linux bubblewrap profile (Linux-only)
EOF
)

allow_absent=0
reports=()
for arg in "$@"; do
    case "$arg" in
        --allow-absent) allow_absent=1 ;;
        -h | --help)
            sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'
            exit 0
            ;;
        *) reports+=("$arg") ;;
    esac
done
if [ "${#reports[@]}" -eq 0 ]; then
    echo "usage: $0 [--allow-absent] REPORT.json [REPORT.json ...]" >&2
    exit 2
fi

for report in "${reports[@]}"; do
    # cargo-llvm-cov JSON structure: data[0].files[*]
    if ! jq -e '.data[0].files | type == "array"' "$report" >/dev/null 2>&1; then
        echo "::error::report $report is unreadable or has no data[0].files" >&2
        exit 2
    fi
done

# One TSV row per table entry: status, suffix, best %, threshold, drop
# below threshold, note, reason. Best % and drop are "-" when no report
# mentions the file.
rows=$(jq -n -r --arg table "$THRESHOLDS" '
    [inputs.data[0].files[]] as $files
    | $table | split("\n")[] | select(length > 0) | split("\t")
    | . as [$suffix, $min, $note, $reason]
    | ($min | tonumber) as $min
    | ([$files[] | select(.filename | endswith($suffix))
        | .summary.lines.percent | numbers] | max) as $best
    | [ (if $best == null then "absent"
         elif $best + 1e-9 < $min then "below"
         else "ok" end),
        $suffix,
        (if $best == null then "-" else $best end),
        $min,
        (if $best == null then "-" else $min - $best end),
        $note, $reason ]
    | @tsv
' "${reports[@]}")

ok=0 below=0 absent=0
unexplained=()
regressions=()
printf '%-45s %10s %10s %8s\n' FILE COVERED THRESHOLD STATUS
printf '%.0s-' {1..80}
echo
while IFS=$'\t' read -r status suffix best min drop note reason; do
    case "$status" in
        absent)
            absent=$((absent + 1))
            if [ "$note" = "-" ]; then
                unexplained+=("$suffix")
                note="missing from all reports"
            fi
            # "—" is three bytes and printf pads by bytes: 12 shows as 10.
            printf '%-45s %12s %9.2f%% %8s  (%s)\n' "$suffix" "—" "$min" absent "$note"
            ;;
        ok)
            ok=$((ok + 1))
            printf '%-45s %9.2f%% %9.2f%% %8s  ✓\n' "$suffix" "$best" "$min" ok
            ;;
        below)
            below=$((below + 1))
            printf '%-45s %9.2f%% %9.2f%% %8s  ✗\n' "$suffix" "$best" "$min" below
            regressions+=("$(printf '  - %s: %.2f%% (threshold %.2f%%, down %.2f%%) — %s' \
                "$suffix" "$best" "$min" "$drop" "$reason")")
            ;;
    esac
done <<<"$rows"

echo
echo "Summary: $ok ok, $below below threshold, $absent absent"

if [ "$below" -gt 0 ]; then
    echo
    echo "::error::Per-file coverage regression detected:"
    printf '%s\n' "${regressions[@]}"
    echo
    echo "To fix: add tests covering the regressed lines in the file(s) above."
    echo "To intentionally lower a threshold: edit THRESHOLDS in this script and"
    echo "document the rationale on issue #1265."
    exit 1
fi

if [ "${#unexplained[@]}" -gt 0 ] && [ "$allow_absent" -eq 0 ]; then
    echo
    echo "::error::Configured files were absent from every report:"
    printf '  - %s\n' "${unexplained[@]}"
    echo
    echo "If this is expected (e.g. file deleted), remove it from THRESHOLDS in"
    echo "scripts/check_per_file_coverage.sh."
    echo "If running on a single platform that lacks platform-gated files,"
    echo "pass --allow-absent."
    exit 2
fi

echo "✓ All gated files at or above their per-file thresholds."
