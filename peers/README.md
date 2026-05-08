# `peers/` — cross-reference repos for koda development

This directory holds **symlinks to sibling clones of peer projects** we
study while building koda — Claude Code, Codex, Gemini CLI, and Zed.

## Why this exists, in one paragraph

When we design a feature (mailbox plumbing, sub-agent dispatch, TUI
event loop, etc.) we constantly pattern-match against how the same
problem is solved in those four projects. Having their source one
`rg peers/codex/codex-rs/ ...` away — without leaving the repo, without
GitHub round-trips, with full IDE jump-to-definition — turns hours of
context-switching into seconds.

## Why **not** git submodules

Submodules **pin to specific commits**. We want **always-at-HEAD**
reference material. A submodule-based design means either:

- Every peer bump becomes a noisy "chore: bump peers" commit in koda's
  history, OR
- Submodules drift stale and the whole point is defeated.

Submodules also force-clone gigabytes onto every contributor (zed alone
is huge) and publish a `.gitmodules` file that misleadingly declares
codex/zed as koda dependencies. They aren't — they're reference
material for koda's authors, with zero runtime relationship to koda.

The sync-script approach below sidesteps all of that.

## Layout

```
peers/
├── README.md          ← checked in (this file)
├── peers.txt          ← checked in (manifest: name + git URL)
├── sync.sh            ← checked in (clone-or-update + symlink)
├── .gitkeep           ← checked in (keeps the dir present)
└── <name>/            ← gitignored (symlink to ../../<name>)
```

Each entry in `peers.txt` becomes:

1. A clone at `../<name>` (sibling of the koda repo)
2. A symlink `peers/<name>` → `../../<name>`

The clones live OUTSIDE the koda working tree so they never count
against koda's clone size, never trigger `cargo`/`npm`/IDE indexers
that walk the workspace, and can be `git pull`-ed independently.

## Daily usage

```sh
./peers/sync.sh
```

That's it. The script is **idempotent and serves as both first-time
setup and ongoing sync** — same as `npm install`, `brew install`,
`cargo update`, and `terraform apply`. One verb, one command, always
correct:

- Missing clone → `git clone` it into `../<name>`
- Missing symlink → create `peers/<name>` → `../../<name>`
- Existing clone, clean tree → fast-forward to upstream HEAD
- Existing clone, dirty tree → skip the update (never touches your WIP)
- Diverged from upstream → skip the update (your problem to resolve)

Run it on a fresh laptop to set everything up. Run it weekly to
refresh. Same command, same mental model, no second script to
remember.

## Adding or removing a peer

**Add:** append a line to `peers.txt`, run `./peers/sync.sh`.

**Remove:** delete the line from `peers.txt`, then `rm peers/<name>` to
drop the symlink. The clone at `../<name>` is left in place — it's
your local checkout, the script doesn't presume to delete it.

## When this design might break down

- If we ever need a peer **at a known-good commit** (e.g. for a
  reproducible benchmark comparison), submodules become the right
  call for that *specific* peer. Add it as a real submodule under a
  different path (e.g. `bench/refs/`); leave `peers/` for the
  always-at-HEAD reference workflow.
- If a peer changes hosting (GitHub → GitLab, rename, etc.), update
  the URL in `peers.txt` and rerun sync. Existing clones won't
  auto-rewrite their `origin` remote — you'd need to `git remote
  set-url origin <new>` in `../<name>` manually, or just delete the
  clone and let sync re-clone fresh.
