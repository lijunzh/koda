#!/usr/bin/env bash
# scripts/preflight.sh — comprehensive pre-PR validation.
#
# Mirrors what CI runs on both ubuntu-latest and macos-latest. Use this
# before opening a PR if you want full confidence — the pre-push hook
# only runs fmt + lib clippy for speed.
#
# Usage:
#   ./scripts/preflight.sh

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

ok()   { echo -e "${GREEN}✓${NC} $*"; }
fail() { echo -e "${RED}✗${NC} $*"; }
info() { echo -e "${YELLOW}→${NC} $*"; }

START=$(date +%s)

info "fmt"
cargo fmt --all --check && ok "fmt"

info "clippy (full)"
cargo clippy --workspace --all-targets --features koda-core/test-support -- -D warnings && ok "clippy"

info "check (no features — catches feature-gating)"
cargo check --workspace --all-targets && ok "check"

info "unit tests"
cargo test --workspace --features koda-core/test-support --lib && ok "unit"

info "integration tests (policy-critical, fast)"
cargo test -p koda-core --features koda-core/test-support \
    --test bash_safety_test \
    --test e2e_safety_test \
    --test e2e_tools_test \
    --test e2e_test \
    --test e2e_agent_test \
    --test e2e_skills_test \
    --test file_tools_test \
    --test golden_test \
    --test guarantee_matrix_test \
    --test new_tools_test \
    --test tool_normalize_test \
    --test tool_wiring_test && ok "integration"

ELAPSED=$(($(date +%s) - START))
echo
ok "Preflight passed in ${ELAPSED}s — safe to open PR."
