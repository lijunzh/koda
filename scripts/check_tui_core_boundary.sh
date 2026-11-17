#!/usr/bin/env bash
# CI guard for the client boundary: the TUI (`koda-tui`) and the `koda`
# command (`koda-cli`, with `koda exec`) reach Koda's engine only through
# the client protocol (`koda-client`), as Codex's
# `verify_tui_core_boundary.py` keeps `codex-core` out of its TUI. Shared
# crates such as `koda-protocol`, `koda-config` and `koda-state` are fine;
# `koda-core` is not:
#   - it must not be in either crate's [dependencies] (dev-dependencies may
#     keep it: integration tests in koda-cli/tests set up state through
#     core);
#   - no `koda_core::` path in their sources, comments aside.
#
# Usage: scripts/check_tui_core_boundary.sh
# Exit codes: 0 all clear, 1 a violation.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

status=0

for crate in koda-cli koda-tui; do
    if awk '/^\[dependencies\]/{d=1;next} /^\[/{d=0} d' "$crate/Cargo.toml" | grep -q '^koda-core'; then
        echo "::error::$crate must not depend on koda-core. Use a client protocol request or a shared crate (koda-protocol, koda-config, koda-state) instead."
        status=1
    fi

    uses=$(git ls-files "$crate/src/*.rs" "$crate/src/**/*.rs" "$crate/benches/*.rs" \
        | sort -u \
        | while read -r file; do
            # Code only: a comment may name a core module, such as a log
            # target, without using it.
            grep -nE 'koda_core::' "$file" 2>/dev/null \
                | grep -vE '^[0-9]+:[[:space:]]*//' \
                | sed "s|^|$file:|" || true
        done)
    if [ -n "$uses" ]; then
        echo "::error::$crate must not reach koda-core directly:"
        echo "$uses" | sed 's/^/  /'
        echo "Use a client protocol request or a shared crate (koda-protocol, koda-config, koda-state) instead."
        status=1
    fi
done

if [ "$status" -eq 0 ]; then
    echo "TUI core boundary: neither koda-cli nor koda-tui depends on koda-core."
fi
exit "$status"
