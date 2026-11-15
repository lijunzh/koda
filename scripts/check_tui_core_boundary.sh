#!/usr/bin/env bash
# CI guard for the client boundary (PR 8): the TUI and `koda exec`
# (`koda-cli`) reach Koda's core only through the client protocol, as
# Codex's `verify_tui_core_boundary.py` keeps `codex-core` out of its TUI.
# Shared crates such as `koda-protocol` and `koda-config` are fine; only
# `koda-core` is forbidden.
#
# `koda-cli` still imports `koda-core` while PR 8 moves it out. The uses
# left are listed in scripts/tui_core_boundary.allow as
# `<file> koda_core::<module>`, so the list can only shrink:
#   - a new pair (a file or module not listed) fails: use a protocol
#     request, or a shared crate, instead;
#   - a listed pair no longer found fails too: run with --update to take
#     it off, so the list stays the true remainder;
#   - once the list is empty, `koda-core` must also be gone from
#     koda-cli's [dependencies].
# Integration tests (koda-cli/tests) may set up state through core and
# aren't checked.
#
# Usage:
#   scripts/check_tui_core_boundary.sh            # check
#   scripts/check_tui_core_boundary.sh --update   # rewrite the list
#
# Exit codes: 0 all clear, 1 a violation.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
allow=scripts/tui_core_boundary.allow

current() {
    git ls-files 'koda-cli/src/*.rs' 'koda-cli/src/**/*.rs' 'koda-cli/benches/*.rs' \
        | sort -u \
        | while read -r file; do
            # Code only: a comment may name a core module, such as a log
            # target, without using it.
            grep -vE '^[[:space:]]*//' "$file" 2>/dev/null \
                | grep -oE 'koda_core::(\{|[A-Za-z_][A-Za-z0-9_]*)' \
                | sort -u \
                | sed "s|^|$file |" || true
        done
}

if [ "${1:-}" = "--update" ]; then
    {
        echo "# koda-cli's koda_core uses left, by file and module (PR 8 removes them)."
        echo "# Written by scripts/check_tui_core_boundary.sh --update; only shrinks."
        current
    } > "$allow"
    echo "Wrote $allow ($(grep -vc '^#' "$allow" || true) entries)."
    exit 0
fi

now=$(current)
listed=$(grep -v '^#' "$allow" | sed '/^$/d' || true)

added=$(comm -13 <(echo "$listed" | sort -u) <(echo "$now" | sed '/^$/d' | sort -u))
gone=$(comm -23 <(echo "$listed" | sort -u) <(echo "$now" | sed '/^$/d' | sort -u))

status=0
if [ -n "$added" ]; then
    echo "::error::koda-cli must not reach koda-core directly. New uses:"
    echo "$added" | sed 's/^/  /'
    echo "Use a client protocol request or a shared crate (koda-protocol, koda-config) instead."
    status=1
fi
if [ -n "$gone" ]; then
    echo "::error::These listed koda-core uses are gone; run scripts/check_tui_core_boundary.sh --update:"
    echo "$gone" | sed 's/^/  /'
    status=1
fi
if [ -z "$listed" ] && awk '/^\[dependencies\]/{d=1;next} /^\[/{d=0} d' koda-cli/Cargo.toml | grep -q '^koda-core'; then
    echo "::error::No koda-core uses are left; remove koda-core from koda-cli's [dependencies]."
    status=1
fi
if [ "$status" -eq 0 ]; then
    echo "TUI core boundary: $(echo "$listed" | sed '/^$/d' | wc -l | tr -d ' ') koda-core uses left, none new."
fi
exit "$status"
