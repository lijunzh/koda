# TUI Architecture Comparison — koda design study

> **Status**: Adopted. Tracked in epic #1146.
> **Decision**: Plan C — bundled inline-first migration including custom textarea (Phase C of epic).
>
> See `Recommendation for koda` section below.

**Question:** koda moved from inline to alt-screen because (the recollection is) inline couldn't do rich markdown / syntax highlighting / formatting. Was that the right call?

**Short answer:** No. All three reference CLI agents (codex, claude-code, gemini-cli) ship rich markdown + syntax highlighting **inline-first**. Inline is the dominant paradigm for agent CLIs in 2026. The premise that motivated koda's switch was a tooling problem, not a fundamental limitation — it's solvable, codex literally shows the solution.

---

## TL;DR matrix

| Project | Default mode | Alt-screen role | Markdown / syntax | Mouse capture | Stack |
|---|---|---|---|---|---|
| **codex** (Rust) | **Inline** (DECSTBM scroll regions) | On-demand overlays only (pager, image picker, image preview) | Full markdown via `pulldown-cmark` → `Vec<Line>`; syntect syntax highlighting | **None** — uses `?1007h` alternate scroll instead | ratatui + custom `insert_history.rs` |
| **claude-code** (Node) | **Inline** | Opt-in via `CLAUDE_CODE_FULLSCREEN` env var; auto-disabled in tmux -CC | Full markdown (`Markdown.tsx` 27KB), tables (`MarkdownTable.tsx` 46KB), syntax (`HighlightedCode.tsx` 17KB) | Yes (SGR 1000/1002/1006), with documented caveats | Custom Ink fork |
| **gemini-cli** (Node) | **Inline** | Configurable via `getUseAlternateBuffer()`; replays history into scrollback on exit | `MarkdownDisplay.tsx` + `InlineMarkdownRenderer` → ANSI; `CodeColorizer.tsx` | Conditional, off by default | Stock Ink |
| **koda** (Rust) | **Alt-screen** | Always | Custom highlighter via `tui_render.rs` | Yes (full SGR mouse capture) | ratatui |
| **zed** (Rust) | **Desktop GPU app** — not a TUI | N/A | N/A (GPUI text renderer) | N/A | GPUI custom |

**Three of three reference TUIs are inline-first.** Koda is the outlier.

---

## What "inline" actually means in 2026

Common architecture across codex / claude-code / gemini-cli:

```
┌─ terminal scrollback (native, OS-managed) ──────────────────────┐
│ [user] write a function that …                                  │
│ ⏵ Read foo.rs                                                   │
│ ⏵ Edit foo.rs                                                   │
│ ```rust                                                         │
│ pub fn foo() { … }                                              │  ← finalized history
│ ```                                                             │     lives in scrollback
│ Done. Tests pass.                                               │
│                                                                 │
├─ live ratatui/Ink viewport (small, bottom-anchored) ────────────┤
│ Thinking… ⠋ tokens 1.2k/200k                                    │  ← actively re-rendered
│ > █                                                             │     each frame
└─────────────────────────────────────────────────────────────────┘
```

Two regions:

1. **Finalized region**: pre-rendered `Vec<Line>` (markdown + syntax-highlighted) **inserted into terminal scrollback** above the live viewport. Once written, the terminal owns it — selection, copy, search, scroll all native, zero in-app state.
2. **Live region**: a small bottom-anchored viewport for the prompt + status indicator + spinner + ephemeral UI. This is the *only* part that re-renders each frame.

The tricky bit is step 1: how do you insert `Vec<Line>` into scrollback **above** a live viewport without flickering or scrambling layout? Answer: **DECSTBM scroll regions** (`ESC [ top ; bot r`) + Reverse Index (`ESC M`). You temporarily restrict the scroll region to *just the live viewport rows*, emit reverse-index newlines to slide existing scrollback down, write your new lines into the freed space at the top of the region, then restore the scroll region. Codex's `insert_history.rs` is **32KB of code** doing exactly this, including a Zellij fallback (Zellij silently drops scroll-region escapes).

This **is** the hard part. It is also a solved problem with a 700-line reference implementation sitting in `../codex/codex-rs/tui/src/insert_history.rs`. Apache 2.0 licensed. We can absolutely port it.

---

## Why the original premise ("inline can't do rich rendering") was wrong

Codex is a counter-example to every part of the claim:

- **Markdown**: `markdown_render.rs` (40KB) parses CommonMark via `pulldown-cmark` → `Vec<Line<'static>>` with full styling (bold, italic, headings, lists, blockquotes, links).
- **Syntax highlighting**: code blocks call `highlight_code_to_lines(&code, &lang)` which uses syntect (the same crate koda already depends on).
- **Tables**: rendered as ratatui `Lines` with column alignment.
- **Links**: clickable in iTerm2/kitty/Ghostty via OSC 8 escapes.
- **Wrapping & resize reflow**: handled by `wrapping.rs` (47KB) + `resize_reflow_cap.rs` — when the terminal resizes, the scrollback is reflowed in place.

All of this works **inline**. The output ends up in native scrollback where the user can select / copy / search it with their terminal's native UI.

The most likely reason koda hit a wall on inline was **ratatui's default `Terminal::draw()` API doesn't expose insert-into-scrollback**. It only knows how to render a `Frame` into a fixed viewport. To do inline-with-scrollback you either:

- (a) Bypass `Terminal::draw()` for finalized content and write escape sequences directly via `crossterm::queue!` — what codex does in `insert_history.rs` + their custom `Terminal` wrapper (`custom_terminal.rs`, 29KB).
- (b) Use ratatui's experimental `Viewport::Inline(N)` mode — which has rough edges around scroll regions and resize.

The path (a) is non-trivial but well-trodden; we have a working reference 30 feet away.

---

## What koda gains by going inline

| Win | Source |
|---|---|
| Native scrollback, search, selection, copy | Terminal owns finalized history |
| Eliminates the `EnableMouseCapture` bug class entirely (#540, #1137 ancestors) | No mouse capture needed |
| ~440 LOC of `mouse_select.rs` deleted | Native selection replaces it |
| ~30% less work per frame (we only re-render the live viewport, not the whole history panel) | Tiny live area |
| #1140 disappears | No mouse capture means nothing to drop |
| OS-native shell integration (iTerm2 marks, terminal scroll, etc.) | Inline output is just regular terminal output |
| Better tmux/screen/ssh behavior | Alt-screen has a bunch of quirks in these envs |
| Smaller TUI surface area to maintain | Less custom scroll/clipboard code |

## What koda loses

| Loss | Severity | Mitigation |
|---|---|---|
| Custom click-and-drag selection feature | Medium-low | Native terminal selection works in iTerm2/Ghostty/kitty/Windows Terminal/WezTerm/Alacritty (the actual user base). `Shift+drag` workaround for tmux. |
| Always-visible status bar at the screen edge | Medium | Replaced by always-visible *bottom-anchored* status above the prompt, like codex. Visually equivalent for most UX intent. |
| Always-visible scrollback indicator | Low | Terminals already show their own scrollbars |
| One screen-cleared view per session | Low (this was a feature, not a bug, for some users) | Optional opt-in alt-screen via env var, like claude-code/gemini do |
| Custom mouse-driven scroll | Low | Terminal native scroll handles it |

## What about the alt-screen overlays codex uses?

Codex still calls `EnterAlternateScreen` for **specific transient surfaces**:
- Pager overlay (full-screen log/transcript viewer)
- Image picker
- Resume picker
- OSS model selection wizard

These are modal screens that *want* to obliterate the chat history temporarily, then return. Same pattern works for koda's `/help`, MCP browser, model picker, etc. — they can be alt-screen overlays inside an otherwise-inline app.

This is exactly the **hybrid** model. Inline by default, alt-screen on demand for modal experiences. Codex calls this `enter_alt_screen()` / `leave_alt_screen()` (lines 641 and 663 of `tui.rs`).

---

## Cross-cutting: what every reference does that koda doesn't (yet)

Patterns that are universal across codex / claude-code / gemini-cli:

1. **Frame coalescing scheduler** at ~120 FPS — koda just shipped this in #1143 ✅
2. **Bounded event drain** in the main loop — koda shipped in #1142 ✅
3. **No mouse capture by default** — codex, off in claude-code's inline mode, conditional in gemini-cli
4. **Inline rendering as default** — all three
5. **Modal alt-screen overlays** — all three for special-purpose surfaces
6. **Custom Terminal wrapper** that exposes scroll-region writes — codex's `custom_terminal.rs`, similar abstractions in Ink fork
7. **Markdown pre-rendered to Lines/spans then inserted as scrollback** — codex's `markdown_render.rs` + `insert_history.rs`
8. **Status indicator widget that lives in the live viewport** — codex's `status_indicator_widget.rs` (16KB), claude-code's `StatusLine.tsx`, gemini's footer

Items 3–8 are all things koda's current architecture makes harder than necessary.

---

## Zed for context (not directly applicable)

Zed is not a TUI; it's a GPU-rendered desktop editor in Rust using GPUI. Their `agent_ui` crate (1.7MB) shows the most polished agent UX in the industry: streaming responses with live syntax highlighting, in-buffer code edits, diff previews, MCP integration, multi-agent orchestration. But the rendering paradigm (GPU-accelerated, retained-mode, pixel-perfect text) is a different universe from terminal output. The value for us is **UX patterns**, not architecture:

- Diff-based tool result rendering (we already do this)
- Bottom-anchored composer with above-line history (matches inline-first)
- Slash command palette with rich previews
- Mention/`@`-completion with file & symbol search

Worth studying for v0.4.x feature parity. Not relevant to the inline-vs-alt-screen decision.

---

## Recommendation for koda

**Go inline.** Specifically: pursue **codex-style hybrid** = inline-first + on-demand alt-screen overlays.

This is a **multi-week refactor** (≈ #1116 territory) but it pays off massively:
- Closes #1140 and the entire mouse-leak bug class structurally
- Closes the scroll/selection UX gap with codex
- Deletes ~440 LOC (`mouse_select.rs`) and reduces ~30% of per-frame work
- Aligns us with the dominant agent-CLI architecture
- Unlocks codex parity on resume, pager, and other modal flows

The order I'd suggest:

### Phase 2 (revised)
1. **File a new umbrella issue**: "Migrate to inline-first rendering with alt-screen overlays"
2. **Close #1140 with a comment** linking the umbrella — Phase 1's defenses already prevent the leak in practice; the structural fix is via inline migration, not via swapping `?1049h` for `?1007h` in our current alt-screen architecture (which doesn't make sense for our event-routing model).
3. **Spike**: stand up a minimal proof-of-concept that ports `insert_history.rs` from codex into a `koda-cli/src/insert_history.rs` file (Apache-2.0 attribution). Verify it works against ratatui's `CrosstermBackend` and renders one finalized line into scrollback above a tiny inline viewport.
4. **Iterate**: migrate one history-cell type at a time (assistant messages first, then tool calls, then commands), keeping the alt-screen mode functional in parallel behind a feature flag (`KODA_RENDER=inline|altscreen`). Lets us bisect regressions.
5. **Cutover**: flip default to inline once parity is reached, deprecate alt-screen path, remove mouse capture, delete `mouse_select.rs`.

### Issues to file

- **Umbrella**: "Migrate koda from alt-screen to inline-first rendering (codex parity)"
- **Spike**: "POC: port codex's `insert_history.rs` to koda"
- **Migration**: one issue per history-cell category to track parity

### Issues to close as obsolete

- **#1140**: replaced by the umbrella above
- **#1116** (textarea): re-frame as part of the inline migration since codex's textarea works inline

---

## Sanity check on the original recollection

You said koda moved from inline to alt-screen because "inline couldn't do rich syntax highlighting/formatting." Three possibilities for what actually happened:

1. **Ratatui `Viewport::Inline(N)` had bugs** at the time (it's still listed as experimental in some versions). The escape into `Viewport::Fullscreen` + alt-screen sidesteps those bugs. **This is fixable** — codex bypasses ratatui's viewport API for finalized content and only uses it for the live region.
2. **The koda implementation tried to keep state in ratatui's Buffer for the entire history** instead of writing finalized lines through to scrollback. That doesn't scale and would have looked broken under wrapping/resize. **Also fixable** — separate finalized vs live state.
3. **Resize reflow was hard** to get right with inline. Codex's `wrapping.rs` (47KB) + `resize_reflow_cap.rs` (6KB) is the proof that it's doable; just not trivial. **Fixable, but it's the hardest part.**

None of these are "rich rendering doesn't work inline." They're all "ratatui's default API doesn't expose what you need; you have to build the missing pieces." Codex built them. We can borrow them.

---

## My honest opinion

Koda's alt-screen choice was a **local optimum** — it solved an immediate "I can't get rich rendering working with ratatui's default `Viewport::Inline`" problem but locked in:
- Mouse capture as mandatory (which caused #540, #1137)
- A custom in-app scroll buffer (~440 LOC + ongoing maintenance)
- A custom click-and-drag selection (~440 LOC of `mouse_select.rs`)
- Worse tmux/screen/ssh behavior than competitors
- A divergence from where the agent-CLI ecosystem is converging

The **global optimum** is inline-first hybrid, demonstrated by all three serious agent-CLI competitors. We have a working reference implementation 30 feet away. The migration is multi-week work, but it deletes more code than it adds in the long run.

I'd vote for **doing the migration as v0.4.x flagship work**, with #1140 and #1116 folded into it.

— code 🐶
