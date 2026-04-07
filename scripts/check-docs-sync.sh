#!/usr/bin/env bash
# scripts/check-docs-sync.sh
#
# Checks that the docs/ chapter files haven't drifted from the source
# sections they document. Exits 1 if stale hashes are detected.
#
# Usage:
#   ./scripts/check-docs-sync.sh          # check mode (CI)
#   ./scripts/check-docs-sync.sh --update # update stored hashes (after editing)

set -euo pipefail

SYNC_FILE="$(dirname "$0")/../docs/sync.toml"
UPDATE=false
[[ "${1:-}" == "--update" ]] && UPDATE=true

fail=0

check() {
    local label="$1"
    local file="$2"
    local stored_key="$3"

    if [[ ! -f "$file" ]]; then
        echo "SKIP  $label (file not found: $file)"
        return
    fi

    local actual
    actual=$(git log -1 --format="%H" -- "$file" 2>/dev/null || echo "UNTRACKED")

    if $UPDATE; then
        # Replace the stored hash in sync.toml
        sed -i.bak "s|^${stored_key} = \"[^\"]*\"|${stored_key} = \"${actual}\"|" "$SYNC_FILE"
        echo "UPDATED $label → $actual"
        return
    fi

    local stored
    stored=$(grep "^${stored_key}" "$SYNC_FILE" | sed 's/.*= "\(.*\)"/\1/' || true)

    if [[ -z "$stored" ]]; then
        echo "WARN  $label — no entry in sync.toml (run --update)"
        return
    fi

    if [[ "$actual" != "$stored" ]]; then
        echo "STALE $label"
        echo "      stored : $stored"
        echo "      current: $actual"
        echo "      → edit docs/src/ to reflect the change, then run:"
        echo "        ./scripts/check-docs-sync.sh --update"
        fail=1
    else
        echo "OK    $label"
    fi
}

check "app.rs (CLI + commands)"       "koda-cli/src/app.rs"       "app_rs"
check "repl.rs (slash commands)"      "koda-cli/src/repl.rs"      "repl_rs"
check "headless.rs"                   "koda-cli/src/headless.rs"  "headless_rs"
check "tui_types.rs (keybindings)"    "koda-cli/src/tui_types.rs" "tui_types_rs"
check "startup.rs (onboarding)"       "koda-cli/src/startup.rs"   "startup_rs"

if $UPDATE; then
    rm -f "$SYNC_FILE.bak"
    echo ""
    echo "sync.toml updated."
    exit 0
fi

if [[ $fail -ne 0 ]]; then
    echo ""
    echo "One or more doc sources have changed since the docs were last synced."
    exit 1
fi

echo ""
echo "All docs are in sync."
