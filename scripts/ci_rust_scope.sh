#!/usr/bin/env bash
# Whether a CI job that builds or tests the Rust workspace has anything to do:
# writes run=true or run=false to $GITHUB_OUTPUT.
#
# On a pull request whose every change is in files no Rust build or test
# reads (the desktop app, the manual's Markdown pages, the changelog and the
# README), the Rust jobs have nothing to check, and skip their steps. They
# still run and report, as `audit` does, so `CI required` sees them pass.
# Every other event (main, nightly, a release) always runs them.
#
# docs/src/protocol-schema.json is read by koda-protocol's tests, so it
# isn't in the list; nor is any other file under docs/src that isn't .md.
set -euo pipefail

out=${GITHUB_OUTPUT:-/dev/stdout}
if [ "${EVENT:-}" != "pull_request" ]; then
  echo "run=true" >> "$out"
  exit 0
fi
# Fail closed: without the test-merge commit, HEAD^1 would be the PR's own
# previous commit and the diff would shrink.
if ! git rev-parse --verify --quiet 'HEAD^2' > /dev/null; then
  echo "::error::Expected the pull_request test-merge commit at HEAD (two parents)."
  exit 1
fi
changed=$(git diff --name-only 'HEAD^1' HEAD)
rust=$(printf '%s\n' "$changed" | grep -vE '^(apps/desktop/.*|docs/src/[^/]+\.md|CHANGELOG\.md|README\.md)$' || true)
if [ -n "$changed" ] && [ -z "$rust" ]; then
  echo "::notice title=Rust skipped::Only the desktop app and Markdown changed; main and the nightly run test the workspace."
  echo "run=false" >> "$out"
else
  echo "run=true" >> "$out"
fi
