#!/usr/bin/env bash
# Compare Koda's protocol with Codex's app-server protocol, field by field
# (koda-protocol/tests/codex_parity.rs). A difference must be renamed to
# Codex's name or recorded, with its reason, in
# koda-protocol/codex-deviations.toml.
#
# Codex's schema comes from `codex app-server generate-json-schema
# --experimental`, which includes the methods Codex marks experimental. It
# uses the `codex` on PATH, or the one CODEX names:
#
#   scripts/check_codex_parity.sh
#   CODEX=~/repo/agent-reference/codex/codex-rs/target/debug/codex scripts/check_codex_parity.sh
#
# Run it when Codex is updated and before a release. It needs Codex, so CI
# doesn't run it.
set -euo pipefail

codex="${CODEX:-codex}"
if ! command -v "$codex" >/dev/null 2>&1; then
    echo "error: no codex to generate Codex's schema with; install it or set CODEX" >&2
    exit 2
fi

schema="$(mktemp -d)"
trap 'rm -rf "$schema"' EXIT
"$codex" app-server generate-json-schema --experimental --out "$schema" >/dev/null
echo "Codex: $("$codex" --version)"

cd "$(dirname "$0")/.."
CODEX_SCHEMA_DIR="$schema" cargo test -q -p koda-protocol --test codex_parity
