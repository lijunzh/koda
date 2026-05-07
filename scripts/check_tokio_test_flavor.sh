#!/usr/bin/env bash
# CI guard for #1109 F2: every `#[tokio::test]` in a Rust file that
# exercises tokio::spawn / BgAgentRegistry / sub-agent dispatch / watch /
# broadcast channels MUST use `flavor = "multi_thread"` to match
# production runtime semantics.
#
# The default `current_thread` runtime queues `tokio::spawn`'d futures
# but only polls them when the test future yields — which doesn't
# happen on a single-task await chain. Production runs multi-thread,
# so the test flavor mismatch silently hides bugs (#1090, etc.).
#
# Usage:
#   scripts/check_tokio_test_flavor.sh           # check workspace
#   scripts/check_tokio_test_flavor.sh --fix     # auto-promote
#
# Exit codes:
#   0 — all clear
#   1 — at least one offending test found
#
# Bash port of the original Python script (#1328 follow-up: kept
# everything-in-Rust shop free of stray Python). We use `git ls-files`
# instead of walking the tree manually — faster, and honors
# .gitignore so target/ etc. are skipped automatically. We use
# `perl -i` for the --fix mode because GNU sed and BSD sed disagree
# on `-i` semantics, and perl ships everywhere we run CI.
#
# Refs #1109 (Phase 2 / F2), #1328 (Python-removal motivation).

set -euo pipefail

# Modules that exercise spawn-style concurrency. If a file imports any
# of these patterns AND has a bare `#[tokio::test]`, the test is at
# risk. KEEP IN SYNC with the comment block in the original Python.
DANGER_PATTERN='\btokio::spawn\b|\bBgAgentRegistry\b|\bsub_agent_dispatch\b|\btokio::sync::watch\b|\btokio::sync::broadcast\b'

# Bare `#[tokio::test]` — flavor not specified. NOTE: the literal
# `]` immediately after `test` is what distinguishes the bare form
# from the flavored form `#[tokio::test(flavor=..., ...)]`. So a
# plain ERE match for `#\[tokio::test\]` catches ONLY the bare
# form — no PCRE lookahead needed. (The original Python used
# `(?!\()` lookahead which was redundant. Dropping it lets us
# stay on portable BREs/EREs that work on BSD grep too — important
# for macOS devs running this locally.)
BARE_TOKIO_TEST_ERE='#\[tokio::test\]'

# What we want bare attrs replaced with.
MULTI_THREAD_TEST='#[tokio::test(flavor = "multi_thread", worker_threads = 2)]'

# Repo root (script lives in scripts/ at repo root).
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

mode="check"
if [ "${1:-}" = "--fix" ]; then
  mode="fix"
elif [ -n "${1:-}" ]; then
  echo "Usage: $0 [--fix]" >&2
  exit 2
fi

# `git ls-files '*.rs'` — every tracked .rs file. Honors .gitignore
# so target/, build artifacts, etc. are excluded for free. If we're
# not in a git repo (rare — happens in some CI extract scenarios),
# fall back to `find`.
if git rev-parse --git-dir > /dev/null 2>&1; then
  rs_files=$(git ls-files '*.rs')
else
  rs_files=$(find . -name '*.rs' -not -path './target/*' -not -path './.git/*')
fi

# First pass: filter to files that contain a danger pattern. This is
# the same short-circuit the Python had — most test files don't
# touch spawn-style concurrency, so we skip them entirely.
# `grep -lE` lists matching files; `|| true` keeps set -e happy when
# zero files match (which would make grep exit 1).
danger_files=$(printf '%s\n' "$rs_files" | xargs grep -lE "$DANGER_PATTERN" 2>/dev/null || true)

if [ -z "$danger_files" ]; then
  echo "[#1109 F2 guard] OK — no files import danger-pattern modules."
  exit 0
fi

if [ "$mode" = "fix" ]; then
  total=0
  changed_files=0
  while IFS= read -r f; do
    [ -z "$f" ] && continue
    # Count bare attrs first (perl -i is silent about counts).
    n=$(grep -cE "$BARE_TOKIO_TEST_ERE" "$f" 2>/dev/null || true)
    if [ "${n:-0}" -gt 0 ]; then
      # Use perl for the rewrite — same regex semantics as the
      # check side, and `perl -i` is portable across BSD/GNU
      # (unlike `sed -i` which differs).
      perl -i -pe 's/\Q#[tokio::test]\E/'"$(printf '%s' "$MULTI_THREAD_TEST" | sed 's:[\/&]:\\&:g')"'/g' "$f"
      printf '  promoted %3d test(s) in %s\n' "$n" "$f"
      total=$((total + n))
      changed_files=$((changed_files + 1))
    fi
  done <<< "$danger_files"
  echo
  echo "Total: $total test(s) promoted across $changed_files file(s)."
  exit 0
fi

# Default mode: report only.
# Collect offenders into a single list of "file:line:content" entries.
offenders=$(printf '%s\n' "$danger_files" \
  | xargs grep -nE "$BARE_TOKIO_TEST_ERE" 2>/dev/null || true)

if [ -z "$offenders" ]; then
  echo "[#1109 F2 guard] OK — every #[tokio::test] in danger-pattern modules uses multi_thread."
  exit 0
fi

cat <<'EOF'
[#1109 F2 guard] FAIL — found bare #[tokio::test] in modules that
                 use tokio::spawn / BgAgentRegistry / sub-agent dispatch /
                 watch / broadcast channels. These tests run on the
                 current_thread runtime by default, which silently
                 hides spawned-future bugs (see #1090).

Offenders:
EOF
# Prefix each offender line with two-space indent to match the
# Python output verbatim (other tooling may scrape this format).
printf '%s\n' "$offenders" | sed 's/^/  /'
cat <<'EOF'

Fix: add (flavor = "multi_thread", worker_threads = 2) to each.
Or run: scripts/check_tokio_test_flavor.sh --fix
EOF
exit 1
