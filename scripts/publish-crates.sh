#!/usr/bin/env bash
# Publish every workspace crate whose version isn't on crates.io yet, and
# try again while some are missing.
#
# `cargo publish --workspace` publishes in dependency order and waits for each
# crate to reach the index before the crates that need it, but only for 60
# seconds; stable cargo can't wait longer (`publish.timeout` is unstable).
# On 0.7.6 crates.io's index was slower than that: 9 of 16 crates went up and
# the rest were never sent. A plain rerun then fails at once, because cargo
# refuses a version that already exists. This script asks crates.io which
# versions are there, publishes only the rest, and after a timeout waits and
# goes again, so a run (or a rerun) finishes what an earlier one started.
#
# It asks the crates.io API, not the sparse index: on 0.7.13 the runner read
# a cached index file without koda-cli for four minutes after the upload, and
# every retry was refused as "already uploaded". The API answers for the
# upload itself, the same thing `cargo publish` checks; cargo still waits for
# the index before publishing the crates that depend on one.
#
# Usage: scripts/publish-crates.sh            publish what's missing
#        scripts/publish-crates.sh --list     print what's missing, publish nothing
# Needs CARGO_REGISTRY_TOKEN to publish, plus jq and curl.
# Exit codes: 0 every crate is on crates.io (or --list), 1 some still aren't.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

ATTEMPTS="${PUBLISH_ATTEMPTS:-5}"
WAIT_SECS="${PUBLISH_WAIT_SECS:-60}"

# Publishable workspace crates whose current version crates.io doesn't have.
# One request a second, as crates.io's crawler policy asks; anything but a
# 200 counts as missing, so a failed lookup costs a refused publish and a
# retry, never a skipped crate.
missing() {
    cargo metadata --no-deps --format-version 1 \
        | jq -r '.packages[] | select(.publish == null) | "\(.name) \(.version)"' \
        | while read -r name version; do
            status=$(curl -sS -o /dev/null -w '%{http_code}' -A "koda-release (github.com/lijunzh/koda)" \
                "https://crates.io/api/v1/crates/$name/$version" 2>/dev/null || true)
            if [ "$status" != "200" ]; then
                echo "$name"
            fi
            sleep 1
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
