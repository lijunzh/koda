#!/usr/bin/env bash
# Publish every workspace crate whose version isn't on crates.io yet, and
# try again while some are missing.
#
# `cargo publish --workspace` publishes in dependency order and waits for each
# crate to reach the index before the crates that need it, but only for 60
# seconds; stable cargo can't wait longer (`publish.timeout` is unstable).
# On 0.7.6 crates.io's index was slower than that: 9 of 16 crates went up and
# the rest were never sent. A plain rerun then fails at once, because cargo
# refuses a version that already exists. This script asks the index which
# versions are there, publishes only the rest, and after a timeout waits and
# goes again, so a run (or a rerun) finishes what an earlier one started.
#
# Usage: scripts/publish-crates.sh            publish what's missing
#        scripts/publish-crates.sh --list     print what's missing, publish nothing
# Needs CARGO_REGISTRY_TOKEN to publish, plus jq and curl.
# Exit codes: 0 every crate is on crates.io (or --list), 1 some still aren't.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

ATTEMPTS="${PUBLISH_ATTEMPTS:-5}"
WAIT_SECS="${PUBLISH_WAIT_SECS:-60}"

# The sparse index's path for a crate name (crates.io's layout).
index_path() {
    local name
    name=$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')
    case ${#name} in
        1) echo "1/$name" ;;
        2) echo "2/$name" ;;
        3) echo "3/${name:0:1}/$name" ;;
        *) echo "${name:0:2}/${name:2:2}/$name" ;;
    esac
}

# Publishable workspace crates whose current version isn't on the index.
missing() {
    cargo metadata --no-deps --format-version 1 \
        | jq -r '.packages[] | select(.publish == null) | "\(.name) \(.version)"' \
        | while read -r name version; do
            if ! curl -fsS -A "koda-release" "https://index.crates.io/$(index_path "$name")" 2>/dev/null \
                | jq -e --arg v "$version" 'select(.vers == $v)' >/dev/null; then
                echo "$name"
            fi
        done
}

if [ "${1:-}" = "--list" ]; then
    missing
    exit 0
fi

for attempt in $(seq 1 "$ATTEMPTS"); do
    crates=$(missing)
    if [ -z "$crates" ]; then
        echo "Every crate is on crates.io."
        exit 0
    fi
    echo "Attempt $attempt of $ATTEMPTS: publishing $(echo "$crates" | tr '\n' ' ')"
    args=()
    for crate in $crates; do
        args+=(-p "$crate")
    done
    if cargo publish "${args[@]}"; then
        continue
    fi
    echo "cargo publish stopped; waiting ${WAIT_SECS}s for the index before trying again."
    sleep "$WAIT_SECS"
done

crates=$(missing)
if [ -z "$crates" ]; then
    echo "Every crate is on crates.io."
    exit 0
fi
echo "Still not on crates.io after $ATTEMPTS attempts: $(echo "$crates" | tr '\n' ' ')" >&2
exit 1
