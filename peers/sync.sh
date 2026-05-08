#!/usr/bin/env bash
# Sync peer reference repos for koda development.
#
# Reads peers/peers.txt — each entry becomes:
#   1. A clone at ../<name>          (sibling of this repo)
#   2. A symlink peers/<name>        (-> ../../<name>)
#
# Idempotent — safe to run on every checkout, every week, every time.
# Doubles as first-time setup on a new machine: missing clones get
# created, existing clones get fast-forwarded. One verb (sync), one
# command, always correct — same shape as `npm install`, `brew install`,
# `cargo update`, `terraform apply`.
#
# Why we fight this hard with `--tags --force --prune`:
#   Several peers (codex's rusty-v8, zed's release tags) retag in place.
#   `git pull --ff-only` aborts the merge silently when fetch rejects
#   tag rewrites — HEAD never moves and you don't notice. `--force`
#   accepts the rewrites; these are read-only mirrors so local tag
#   state isn't precious.
#
# Why we never auto-stash dirty trees:
#   These clones are YOUR working copies. If you've made local changes
#   in ../codex while patching a bug, we will not silently move HEAD
#   out from under you. We skip and warn.
#
# Why we use --ff-only and bail on divergence:
#   Same reason. Diverged history means you've done something
#   intentional in the peer; auto-merging it would be presumptuous.

set -euo pipefail

# ── Locate ourselves ─────────────────────────────────────────────────
script_dir="$(cd "$(dirname "$0")" && pwd)"
repo_parent="$(cd "$script_dir/.." && pwd)"
parent_dir="$(cd "$repo_parent/.." && pwd)"
manifest="$script_dir/peers.txt"

if [[ ! -f "$manifest" ]]; then
  echo "❌ Missing manifest: $manifest" >&2
  exit 1
fi

# ── Per-peer state for the summary table ────────────────────────────
summary=()
record() { summary+=("$1"); }

# ── Worker: sync one peer (clone if missing, fast-forward if present) ──────
process_peer() {
  local name="$1" url="$2"
  local clone_path="$parent_dir/$name"
  local link_path="$script_dir/$name"

  printf '\n━━ %s ━━\n' "$name"

  # Step 1: ensure the clone exists.
  if [[ ! -d "$clone_path/.git" ]]; then
    if [[ -e "$clone_path" ]]; then
      echo "  ❌ $clone_path exists but isn't a git repo — investigate manually"
      record "$(printf '%-20s  ERROR (path collision)' "$name")"
      return
    fi
    echo "  📥 cloning $url → $clone_path"
    if ! git clone "$url" "$clone_path"; then
      record "$(printf '%-20s  ERROR (clone failed)' "$name")"
      return
    fi
  else
    # Step 2a: existing clone — fast-forward to upstream HEAD.
    update_existing_clone "$name" "$clone_path" || {
      # update_existing_clone records its own summary line on failure.
      ensure_symlink "$name" "$link_path" "$clone_path"
      return
    }
  fi

  # Step 2b: ensure the symlink exists and points where it should.
  ensure_symlink "$name" "$link_path" "$clone_path"

  # Step 3: emit summary line for the up-to-date case.
  record_status "$name" "$clone_path"
}

# Fast-forward an existing clone to its upstream HEAD. Returns non-zero
# on any condition that left HEAD unmoved (dirty, diverged, fetch
# error) — caller still creates the symlink but skips the OK-summary.
update_existing_clone() {
  local name="$1" clone_path="$2"
  local prev_head; prev_head=$(git -C "$clone_path" rev-parse HEAD)

  # Refuse to touch a dirty tree.
  if ! git -C "$clone_path" diff --quiet \
     || ! git -C "$clone_path" diff --cached --quiet; then
    echo "  ⚠️  dirty working tree in $clone_path — leaving alone"
    record "$(printf '%-20s  SKIPPED (dirty)' "$name")"
    return 1
  fi

  # `--tags --force --prune` accepts retagged remotes; mirrors aren't
  # precious about local tag state.
  if ! git -C "$clone_path" fetch --tags --force --prune origin 2>&1 | tail -3; then
    record "$(printf '%-20s  ERROR (fetch failed)' "$name")"
    return 1
  fi

  # Bail if no upstream is set on the current branch (detached HEAD,
  # custom branch, etc.) — leave HEAD where the user put it.
  if ! git -C "$clone_path" rev-parse --abbrev-ref '@{u}' >/dev/null 2>&1; then
    echo "  ⚠️  no upstream tracking branch — leaving alone"
    record "$(printf '%-20s  SKIPPED (no upstream)' "$name")"
    return 1
  fi

  # Fast-forward only. Diverged history is the user's problem.
  if ! git -C "$clone_path" merge --ff-only '@{u}' 2>&1 | tail -3; then
    record "$(printf '%-20s  SKIPPED (non-ff)' "$name")"
    return 1
  fi

  local new_head; new_head=$(git -C "$clone_path" rev-parse HEAD)
  if [[ "$prev_head" == "$new_head" ]]; then
    echo "  ✓ already up to date"
  else
    local n; n=$(git -C "$clone_path" rev-list --count "$prev_head..$new_head")
    echo "  ⬆️  fast-forwarded $n commit(s)"
  fi
}

# Create or repair the symlink at peers/<name>. Idempotent.
# Uses a relative target so the symlink survives moves of the koda repo.
ensure_symlink() {
  local name="$1" link_path="$2" clone_path="$3"
  local rel_target="../../$name"

  if [[ -L "$link_path" ]]; then
    local current; current=$(readlink "$link_path")
    if [[ "$current" == "$rel_target" ]]; then
      return 0
    fi
    echo "  🔗 fixing symlink ($current → $rel_target)"
  elif [[ -e "$link_path" ]]; then
    echo "  ❌ $link_path exists but isn't a symlink — investigate manually"
    record "$(printf '%-20s  ERROR (link path occupied)' "$name")"
    return 1
  else
    echo "  🔗 creating symlink $link_path → $rel_target"
  fi

  ln -snf "$rel_target" "$link_path"
}

record_status() {
  local name="$1" clone_path="$2"
  local branch; branch=$(git -C "$clone_path" rev-parse --abbrev-ref HEAD 2>/dev/null || echo '?')
  local ab; ab=$(git -C "$clone_path" rev-list --left-right --count HEAD...@\{u\} 2>/dev/null | tr '\t' '/' || echo '?/?')
  local last; last=$(git -C "$clone_path" log -1 --format='%cr  %s' 2>/dev/null | cut -c1-60)
  record "$(printf '%-20s  %-10s  ahead/behind=%-6s  %s' "$name" "$branch" "$ab" "$last")"
}

# ── Main loop ────────────────────────────────────────────────────────
while IFS=$' \t' read -r name url _rest; do
  # Skip blanks and comments.
  [[ -z "${name:-}" ]] && continue
  [[ "${name:0:1}" == "#" ]] && continue
  if [[ -z "${url:-}" ]]; then
    echo "⚠️  manifest line missing URL: $name" >&2
    continue
  fi
  process_peer "$name" "$url"
done < "$manifest"

# ── Summary ──────────────────────────────────────────────────────────
printf '\n━━ summary ━━\n'
for line in "${summary[@]}"; do
  printf '%s\n' "$line"
done
