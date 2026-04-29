# Postmortem: inline-rendering exploration (April 2026)

> **Status**: Closed. Decided NOT to migrate.
> **Original epic**: #1146 (closed as won't-do)
> **Original PR**: #1147 (closed unmerged)
> **Decision date**: 2026-04-29

## Why this doc exists

Between April 28 – April 29, 2026 the koda project ran a focused exploration of whether to migrate the TUI from alt-screen rendering to inline-first rendering, matching what `codex`, `claude-code`, and `gemini-cli` ship. We built a working Phase A POC (PR #1147 — opt-in `KODA_RENDER=inline` mode), then stepped back and asked the strategic question that should have come first.

The answer was: **don't migrate.** This doc captures the analysis so the same exploration doesn't get re-run in 6 months.

## TL;DR

- **Three-of-three reference TUIs (codex, claude-code, gemini-cli) are inline-first.** koda is the outlier, and on the surface this looked like a gap to close.
- **Inline rendering is technically achievable in ratatui 0.30.** PR #1147 demonstrated this — `Terminal::insert_before()` writes pre-rendered lines into native terminal scrollback above an inline viewport.
- **But the surface-level POC is the easy part.** Reaching codex-level polish requires roughly **~10K LOC of platform code**: a custom `Terminal` fork (no DSR cursor queries), a `desired_height(width)` rollup propagated through every widget, a resize-reflow rebuild path, custom popup positioning, and likely a hand-rolled TextArea (codex's is 2,660 lines).
- **For a single-person project, that maintenance cost has no return.** The cosmetic wins (scrollback preservation, mouse-capture leak elimination on opt-in) don't justify becoming codex-shaped.
- **Strategic direction is different anyway.** koda's long-term shape is a `koda-core` daemon with multiple thin frontends (TUI now, GUI later, mobile/automation eventually). Heavy in-process TUI investment competes with that direction for engineering attention.

## What inline-first actually looks like in the reference projects

Common architecture across codex / claude-code / gemini-cli:

```
┌─ terminal scrollback (native, OS-managed) ──────────────────────┐
│ [user] write a function that …                                  │
│ ⏵ Read foo.rs                                                   │  ← finalized history
│ ```rust                                                         │     lives in scrollback,
│ pub fn foo() { … }                                              │     selection/copy/scroll
│ ```                                                             │     all OS-native
│ Done. Tests pass.                                               │
│                                                                 │
├─ live ratatui/Ink viewport (small, bottom-anchored) ────────────┤
│ Thinking… ⠋ tokens 1.2k/200k                                    │  ← actively re-rendered
│ > █                                                             │     each frame
└─────────────────────────────────────────────────────────────────┘
```

Two regions:

1. **Finalized region**: pre-rendered `Vec<Line>` (markdown + syntax-highlighted) inserted into terminal scrollback above the live viewport. Once written, the terminal owns it.
2. **Live viewport**: small bottom-anchored region for composer + status + transient UI (spinner, token counter, slash menu).

| Project | Default | Markdown | Syntax | Mouse capture | Stack |
|---|---|---|---|---|---|
| **codex** (Rust) | Inline (DECSTBM) | `pulldown-cmark` → `Vec<Line>` | syntect | None (`?1007h` alt scroll) | ratatui + custom Terminal |
| **claude-code** (Node) | Inline | `Markdown.tsx` | `HighlightedCode.tsx` | Yes, leak-aware | Ink |
| **gemini-cli** (Node) | Inline | `MarkdownDisplay.tsx` | `CodeColorizer.tsx` | Conditional | Stock Ink |
| **koda** (Rust) | Alt-screen | Custom `tui_render.rs` | Custom | Yes (full SGR) | ratatui |
| **zed** (Rust) | GPU desktop app | N/A | N/A | N/A | GPUI |

## What we'd gain from migrating

- **Scrollback preservation**: alt-screen swap destroys terminal scrollback on entry; inline preserves it.
- **OS-native selection/copy** in the finalized region (no in-app mouse capture needed for it).
- **Mouse-capture leak class** (#540, regressed in #1137) becomes architecturally impossible if mouse capture is dropped (#1140 closed).
- **Native scroll** instead of in-app scroll buffer code.
- **Architectural alignment** with codex/claude-code/gemini-cli (lower friction for users moving between them).

## What we'd lose / cost

- **Multi-viewport overlays** (modal pickers, image previews, fullscreen approval flows). Codex handles these via on-demand alt-screen *overlays*, not by being alt-screen-default — but every overlay is its own render path to maintain.
- **Resize handling becomes app-owned**. The terminal no longer reflows for us; we have to detect width changes and rebuild from a transcript-of-truth (which we have via SQLite, fortunately).
- **Layout state lives in the app**. With alt-screen, ratatui's standard `Frame` rect math suffices. Inline requires `desired_height(width)` to be threadable through every widget so the viewport can be sized correctly each frame.
- **Several unfixable-without-fork issues** (the actual blockers below).

## Why "polish parity with codex" requires owning the terminal abstraction

This is the load-bearing finding. ratatui's stock `Viewport::Inline` has well-known failure modes that we previously fought from v0.1.7 → v0.1.13 (issues #415, #418, #419, #420, #463, #470). The reasons are architectural:

- **DSR cursor position queries** (`\x1B[6n`) on every `init_terminal()` and `autoresize()` — unreliable in tmux/screen/SSH/after sleep, race conditions with EventStream stdin reader, **caused random crashes** until we abandoned inline (#470).
- **Resize ghost fragments** (#418): after column resize, terminal reflows content making `viewport_area.y` stale, with no terminal API to query post-reflow position.
- **Scrollback destruction on width change** (#415): ANSI erase doesn't account for line-wrap reflow.

Codex sidesteps these by **not using ratatui's `Viewport::Inline` at all**. Their `CustomTerminal` (~700 LOC fork) and surrounding infrastructure:

- Zero DSR queries in steady state — `set_viewport_area` directly mutates an internal buffer rect.
- App-owned viewport coordinates — never stale because nothing else can change them.
- DECSTBM scroll regions to push viewport content INTO scrollback before drawing the new viewport (no ANSI erase, no width-reflow problem).
- Dedicated 50-line `update_inline_viewport_for_resize_reflow` path that rebuilds rows from the transcript on width change.

To match codex's polish, koda would need to build the equivalent. Sizing estimate from reading their code:

| Component | LOC |
|---|---|
| `custom_terminal.rs` fork | ~700 |
| `bottom_pane/textarea.rs` (hand-rolled TextArea) | 2,660 |
| `bottom_pane/mod.rs` | ~93 KB (~3,000 LOC) |
| `markdown_render.rs` | ~40 KB |
| `wrapping.rs` | ~47 KB |
| `insert_history.rs` | 843 |
| **Total platform code** | **~10K LOC** |

This is platform code that exists because ratatui isn't enough. Every kernel update, every new terminal emulator, every tmux release is potential whack-a-mole. Codex has full-time engineers maintaining it. koda does not.

## Why we're not doing it

Three reasons, in order of weight:

### 1. Strategic direction is daemon-first, not TUI-polish-first

koda's long-term shape is a `koda-core` daemon (handles engine, sessions, tools, providers, sub-agents, MCP, sandbox, persistence) that multiple thin frontends connect to over a protocol:

```
┌──────────────┐    ┌──────────────┐    ┌──────────────┐
│  TUI client  │    │  GUI client  │    │ phone client │
└──────┬───────┘    └──────┬───────┘    └──────┬───────┘
       │                   │                   │
       └────────────────── ▼ ──────────────────┘
                  ClientCommand / ServerEvent
                           │
                  ┌────────▼────────┐
                  │   koda-core     │
                  │   (daemon)      │
                  └─────────────────┘
```

The architectural foundation is already in place: `koda-core::EngineSink` is the daemon-shaped API, `koda-cli::CliSink` is a 60-line shim that today forwards over an mpsc channel and tomorrow could forward over a socket.

Heavy in-process TUI investment (10K LOC of custom terminal abstraction) competes with this direction for engineering attention. The smarter move is to harden the boundary now (audit + `koda-protocol` crate + protocol-first discipline) so when daemon time comes, it's a 2-week refactor, not a 2-month rewrite.

### 2. Single-person project economics

For a project with one maintainer, every line of code is a maintenance liability. Maintaining an "experimental" UI mode alongside a canonical one means every TUI bug has 2× the surface area to triage, every dependency upgrade has to be tested in both, and every refactor has to consider both code paths. The opt-in users would be a tiny minority paying for a UX that's worse than the canonical alt-screen mode.

### 3. The cosmetic gap isn't actually large

koda's alt-screen TUI today renders markdown, syntax highlighting, slash menus, popups, sub-agent fan-out, MCP status, and more — things users actually feel. The inline-vs-alt-screen difference is mostly:

- Scrollback preservation across sessions (real but minor)
- Native OS selection in the history region (nice but workable)
- Mouse-capture leak elimination (already addressable via #1140 patterns)

None of these are blocking adoption. They're polish, and the polish cost is wildly disproportionate to the polish benefit.

## What survives this decision

- **TextArea ownership** (#1116, re-opened): independent of inline-vs-alt-screen. Any future frontend (TUI now, GUI later, attached to daemon) needs an input composer; the case for owning ours is unchanged.
- **Mouse-capture leak addressing**: track per-issue if it bites alt-screen users; #1140 closed but pattern is documented.
- **Code-quality refactors** (#1145, #1144): help the eventual `koda-core` ↔ `koda-cli` boundary cleanup.
- **Frame coalescing, drain bounding, round-robin fairness** (#1137-#1140): all already merged via epic #1141.

## When this decision should be revisited

Pick this back up only if **at least two** of the following become true:

- [ ] koda gains a second maintainer (or commercial sponsorship) such that 10K LOC of platform code is no longer the dominant cost
- [ ] ratatui ships a `Viewport::Inline` rewrite that solves DSR/reflow without app-owned terminal abstraction
- [ ] alt-screen mode hits a UX cliff that inline would architecturally resolve (e.g. an image-rendering protocol that requires kitty graphics inline)
- [ ] daemon-first migration completes and the TUI client is the canonical frontend (not just one of several), making "TUI polish" a higher-leverage investment

Until then: **stay on alt-screen.** Document this decision in any future epic that proposes a TUI rendering rewrite.

## Lesson for project decision-making

This exploration shouldn't have been launched before the strategic question ("what is koda's UX shape long-term?") was answered. The answer turned out to be incompatible with the exploration's premise. **Going forward: strategic clarity before epic-sized commitments.** Especially for a single-person project where every line is a maintenance liability.

## References

- Original advocacy doc (since deleted): `docs/architecture/inline-vs-altscreen.md` on branch `feat/inline-rendering-migration` (deleted; recoverable from git history)
- Closed epic: #1146
- Closed PR: #1147
- Re-opened TextArea issue: #1116
- Completed responsiveness epic: #1141
- Historical inline-mode failures: #415, #418, #419, #420, #463, #470
