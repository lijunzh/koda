#!/usr/bin/env bash
# CI guard for crates.io's keyword rules, which `cargo publish --dry-run`
# doesn't check: the registry rejects a crate at upload time, after the
# crates it depends on are already published. koda-mcp's
# "model-context-protocol" (22 characters) stopped the 0.7.1 publish halfway.
# crates.io allows at most five keywords per crate, each at most 20
# characters, ASCII, starting with a letter or digit, and made of letters,
# digits, `_`, `-` and `+`.
#
# Usage: scripts/check_crate_keywords.sh
# Exit codes: 0 all clear, 1 a violation.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

status=0
for manifest in $(git ls-files 'Cargo.toml' '*/Cargo.toml'); do
    line=$(grep -E '^keywords[[:space:]]*=' "$manifest" || true)
    [ -z "$line" ] && continue
    keywords=$(printf '%s\n' "$line" | grep -oE '"[^"]*"' | tr -d '"')
    count=$(printf '%s\n' "$keywords" | grep -c . || true)
    if [ "$count" -gt 5 ]; then
        echo "::error file=$manifest::$count keywords; crates.io allows at most 5."
        status=1
    fi
    while read -r keyword; do
        [ -z "$keyword" ] && continue
        if [ "${#keyword}" -gt 20 ]; then
            echo "::error file=$manifest::keyword \"$keyword\" has ${#keyword} characters; crates.io allows at most 20."
            status=1
        fi
        if ! printf '%s' "$keyword" | LC_ALL=C grep -qE '^[A-Za-z0-9][A-Za-z0-9_+-]*$'; then
            echo "::error file=$manifest::keyword \"$keyword\" must start with a letter or digit and use only letters, digits, _, - and +."
            status=1
        fi
    done <<< "$keywords"
done

if [ "$status" -eq 0 ]; then
    echo "Crate keywords: every crate meets crates.io's keyword rules."
fi
exit "$status"
