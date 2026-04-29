//! Inline-mode history insertion for koda's TUI.
//!
//! In inline mode (the default starting from epic #1146 — see
//! `docs/architecture/inline-vs-altscreen.md`), finalized chat history lives
//! in the terminal's *native scrollback* rather than in an in-app scroll
//! buffer. The bottom 3-10 rows of the screen are an inline ratatui
//! viewport (status + composer + popups); everything above is real
//! scrollback owned by the terminal emulator (which means native
//! selection, search, and scroll all work for free).
//!
//! ## How it works under the hood
//!
//! `ratatui::Terminal::insert_before(height, draw_fn)` (added in 0.30)
//! does the heavy lifting: when the `scrolling-regions` cargo feature is
//! enabled (we have it on — see `koda-cli/Cargo.toml`), it uses the
//! `\x1b[top;bot r` DECSTBM escape to slide existing scrollback down and
//! writes the new lines into the freed space above the inline viewport.
//! Codex's `insert_history.rs` does the same thing manually in 843 lines;
//! ratatui exposes it as a one-liner because we're already on 0.30.
//!
//! ## Cell-by-cell behavior worth knowing
//!
//! The inline viewport starts at `y=0` of the screen and grows downward
//! as `insert_before` is called. Each insert reserves N rows above the
//! current viewport position and shifts the viewport down by N rows
//! (until the screen bottom is reached, at which point further inserts
//! roll the topmost rows off into native scrollback). `TestBackend`
//! tests in this module cover both regimes.
//!
//! ## Why a wrapper module
//!
//! The raw `insert_before` API takes a closure that fills a `Buffer` with
//! a known height. Callers always want to insert "this `Vec<Line>`",
//! which means computing the wrapped height and rendering via
//! `Paragraph::wrap`. Centralising that calculation here keeps every
//! call-site terse and consistent.
//!
//! ## What this module is NOT yet
//!
//! - URL-aware wrapping (clickable-link preservation across line wraps —
//!   codex's `wrapping.rs` adds this; we'll port it when needed).
//! - Zellij fallback (Zellij ignores DECSTBM scroll regions — we'll add a
//!   detection + fallback path in a later commit if Zellij users complain).
//!
//! ## License attribution
//!
//! The high-level idea (compute wrapped height, then call into the
//! terminal's scroll-region machinery) is borrowed from
//! `codex-rs/tui/src/insert_history.rs` (Apache-2.0). The implementation
//! itself is a thin shim over ratatui 0.30's upstream `insert_before` and
//! is original (MIT, like the rest of koda).

use ratatui::{
    Terminal,
    backend::Backend,
    text::Line,
    widgets::{Paragraph, Widget, Wrap},
};
use std::io;

/// Insert finalized history lines into the terminal's scrollback, above
/// the inline viewport.
///
/// Computes the wrapped height of `lines` at the current terminal width,
/// then delegates to `Terminal::insert_before` to slide existing
/// scrollback down and write the new content into the freed rows.
///
/// Cursor-position-neutral: the inline viewport (composer/status) is
/// re-rendered in-place by ratatui after the insert, so callers don't
/// need to redraw afterward.
///
/// # Behavior
///
/// - **No-op** if `lines` is empty or terminal width is 0.
/// - **No-op** if the terminal is in `Viewport::Fullscreen` mode
///   (insert-into-scrollback only makes sense for `Viewport::Inline(_)`).
/// - **Wraps** lines at the current terminal width using
///   `Paragraph::wrap(Wrap { trim: false })`. Long lines flow onto the
///   next scrollback row.
///
/// # Errors
///
/// Returns whatever the underlying backend returns (typically I/O errors
/// from writing escape sequences to the terminal).
///
/// # Example (conceptual)
///
/// ```ignore
/// use ratatui::text::Line;
/// // After a tool finishes, push its rendered output into scrollback:
/// koda_cli::inline_history::push_history(
///     &mut terminal,
///     vec![Line::from("✓ Read foo.rs (245 lines)")],
/// )?;
/// ```
#[allow(dead_code)] // wired up in a subsequent commit on the same branch.
pub fn push_history<B>(
    terminal: &mut Terminal<B>,
    lines: Vec<Line<'static>>,
) -> io::Result<()>
where
    B: Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    if lines.is_empty() {
        return Ok(());
    }

    // `terminal.size()` is fast (no syscall — cached from last poll).
    let size = terminal.size().map_err(io::Error::other)?;
    let width = size.width;
    if width == 0 {
        return Ok(());
    }

    // Compute the wrapped row count. `Paragraph::line_count` accounts for
    // both hard line breaks in the input and soft wrapping at the
    // terminal edge, so a single 200-char `Line` at width 80 reports 3.
    //
    // We clone here because `insert_before` needs the lines again inside
    // the closure (FnOnce). The clone is cheap — `Line<'static>` is
    // mostly `Cow<'static, str>` spans which clone by Arc-bumping the
    // refcount on owned content.
    let para = Paragraph::new(lines.clone()).wrap(Wrap { trim: false });
    let height = para.line_count(width) as u16;
    if height == 0 {
        return Ok(());
    }

    terminal
        .insert_before(height, |buf| {
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .render(buf.area, buf);
        })
        .map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    //! Tests verify the *observable* behavior of `push_history`:
    //! finalized lines actually land above the inline viewport on the
    //! screen, and overflow into native scrollback once the on-screen
    //! rows are exhausted.
    //!
    //! `TestBackend::assert_buffer_lines` shows the currently-visible
    //! screen; `assert_scrollback_lines` shows lines that have scrolled
    //! off the top. Together they're the upstream-recommended way to
    //! verify `insert_before` semantics (see
    //! `ratatui-0.30/tests/terminal.rs`).

    use super::*;
    use ratatui::{
        Terminal, TerminalOptions, Viewport,
        backend::TestBackend,
        prelude::Stylize,
        widgets::Paragraph,
    };

    fn inline_terminal(width: u16, height: u16, viewport: u16) -> Terminal<TestBackend> {
        let backend = TestBackend::new(width, height);
        Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(viewport),
            },
        )
        .expect("test terminal should construct")
    }

    /// Draw a known string into the inline viewport so we can later
    /// assert it survives history insertion intact. The label is
    /// right-padded with spaces to the full terminal width so every
    /// cell in the viewport row has an explicit symbol — otherwise
    /// `assert_buffer_lines` mismatches against `None` cells.
    fn paint_viewport(terminal: &mut Terminal<TestBackend>, label: &str) {
        let width = terminal
            .backend()
            .size()
            .map(|s| s.width as usize)
            .unwrap_or(0);
        let padded = format!("{label:<width$}");
        terminal
            .draw(|f| {
                let p = Paragraph::new(padded);
                f.render_widget(p, f.area());
            })
            .expect("viewport draw should succeed");
    }

    /// Helper: assert scrollback is empty regardless of width by
    /// inspecting the backend's scrollback buffer area directly
    /// (`assert_scrollback_lines(Vec::<&str>::new())` constructs a
    /// zero-width expected buffer which spuriously fails width
    /// assertions).
    fn assert_scrollback_empty(t: &Terminal<TestBackend>) {
        // The backend exposes `scrollback()` which returns &Buffer.
        // A truly-empty scrollback has height 0.
        let area = t.backend().scrollback().area;
        assert_eq!(
            area.height, 0,
            "expected empty scrollback, found {} rows",
            area.height
        );
    }

    /// Dump the on-screen buffer as `Vec<String>`, treating cells with
    /// `symbol: None` (i.e. cleared by scroll-region operations but
    /// never explicitly written) as spaces. This is needed because
    /// `assert_buffer_lines` does *exact* cell equality, so a freshly-
    /// scrolled empty cell (`None`) won't match an expected explicit
    /// space. For our purposes a blank visual cell is a blank visual
    /// cell regardless of how it got that way.
    fn dump_buffer(t: &Terminal<TestBackend>) -> Vec<String> {
        let buf = t.backend().buffer();
        let area = buf.area;
        let mut rows = Vec::with_capacity(area.height as usize);
        for y in area.top()..area.bottom() {
            let mut row = String::with_capacity(area.width as usize);
            for x in area.left()..area.right() {
                let cell = &buf[(x, y)];
                let sym = cell.symbol();
                if sym.is_empty() {
                    row.push(' ');
                } else {
                    row.push_str(sym);
                }
            }
            rows.push(row);
        }
        rows
    }

    /// Dump scrollback the same way `dump_buffer` does for the screen.
    fn dump_scrollback(t: &Terminal<TestBackend>) -> Vec<String> {
        let buf = t.backend().scrollback();
        let area = buf.area;
        let mut rows = Vec::with_capacity(area.height as usize);
        for y in area.top()..area.bottom() {
            let mut row = String::with_capacity(area.width as usize);
            for x in area.left()..area.right() {
                let cell = &buf[(x, y)];
                let sym = cell.symbol();
                if sym.is_empty() {
                    row.push(' ');
                } else {
                    row.push_str(sym);
                }
            }
            rows.push(row);
        }
        rows
    }

    #[test]
    fn empty_lines_is_a_noop() {
        // Most basic guarantee: callers can fire-and-forget without
        // worrying about an empty `insert_before(0, ..)` call panicking
        // or scrambling the viewport.
        let mut t = inline_terminal(20, 5, 1);
        paint_viewport(&mut t, "[viewport]");
        push_history(&mut t, vec![]).expect("noop must succeed");
        assert_scrollback_empty(&t);
        assert_eq!(
            dump_buffer(&t),
            vec![
                "[viewport]          ",
                "                    ",
                "                    ",
                "                    ",
                "                    ",
            ]
        );
    }

    #[test]
    fn one_short_line_lands_above_viewport_on_screen() {
        // The fundamental contract: pushing one line places it
        // *above* the inline viewport on the visible screen, not
        // *inside* the viewport. Scrollback stays empty because the
        // line still fits on screen.
        let mut t = inline_terminal(20, 5, 1);
        paint_viewport(&mut t, "[viewport]");
        push_history(&mut t, vec![Line::from("hello world")]).expect("push must succeed");
        assert_scrollback_empty(&t);
        // Inline viewport starts at y=0 and grows downward as content
        // is inserted: the new line lands at row 0, the viewport row
        // shifts to row 1.
        assert_eq!(
            dump_buffer(&t),
            vec![
                "hello world         ",
                "[viewport]          ",
                "                    ",
                "                    ",
                "                    ",
            ]
        );
    }

    #[test]
    fn overflow_pushes_oldest_lines_into_scrollback() {
        // When more lines are pushed than fit above the viewport, the
        // oldest must roll off the top into native scrollback. This
        // is what unlocks native terminal selection / search of
        // historical content.
        //
        // Screen: 5 tall, viewport: 1 tall — so 4 rows above viewport
        // can hold history before overflow. Pushing 6 lines means
        // lines 0 and 1 should end up in scrollback; lines 2-5 stay
        // on-screen above the viewport.
        let mut t = inline_terminal(20, 5, 1);
        paint_viewport(&mut t, "[viewport]");
        for i in 0..6 {
            push_history(&mut t, vec![Line::from(format!("line {i}"))]).unwrap();
        }
        assert_eq!(
            dump_scrollback(&t),
            vec![
                "line 0              ",
                "line 1              ",
            ]
        );
        assert_eq!(
            dump_buffer(&t),
            vec![
                "line 2              ",
                "line 3              ",
                "line 4              ",
                "line 5              ",
                "[viewport]          ",
            ]
        );
    }

    #[test]
    fn long_line_wraps_into_multiple_screen_rows() {
        // A 30-character line at terminal width 20 should wrap to 2
        // physical rows. This is the wrap-height calculation under
        // test: if it returned 1, the second half would be lost; if
        // it returned 3, we'd waste a blank row.
        let mut t = inline_terminal(20, 8, 1);
        paint_viewport(&mut t, "[viewport]");
        let long: String = std::iter::repeat_n('x', 30).collect();
        push_history(&mut t, vec![Line::from(long)]).expect("long-line push must succeed");
        assert_scrollback_empty(&t);
        assert_eq!(
            dump_buffer(&t),
            vec![
                "xxxxxxxxxxxxxxxxxxxx",
                "xxxxxxxxxx          ",
                "[viewport]          ",
                "                    ",
                "                    ",
                "                    ",
                "                    ",
                "                    ",
            ]
        );
    }

    #[test]
    fn styled_spans_render_into_history_area() {
        // Smoke test: styled content (bold, colored) must round-trip
        // through `Vec<Line>` -> `Buffer` -> screen without being
        // dropped. We verify the *symbol* content lands correctly;
        // ratatui's own tests cover style attribute rendering.
        let mut t = inline_terminal(20, 5, 1);
        paint_viewport(&mut t, "[viewport]");
        let line = Line::from(vec!["ERR: ".bold().red(), "boom".into()]);
        push_history(&mut t, vec![line]).expect("styled push must succeed");
        assert_eq!(
            dump_buffer(&t),
            vec![
                "ERR: boom           ",
                "[viewport]          ",
                "                    ",
                "                    ",
                "                    ",
            ]
        );
    }

    #[test]
    fn fullscreen_viewport_is_a_noop() {
        // Inserting into a fullscreen viewport doesn't make sense
        // (there's no scrollback gap). The underlying ratatui call
        // is a documented no-op; we want to confirm we don't panic
        // or write garbage in that mode.
        let backend = TestBackend::new(20, 5);
        let mut t = Terminal::new(backend).expect("fullscreen terminal");
        push_history(&mut t, vec![Line::from("ignored")])
            .expect("fullscreen no-op must succeed");
        assert_scrollback_empty(&t);
    }

    #[test]
    fn boundary_width_line_is_one_row() {
        // Off-by-one trap: a line of *exactly* `width` chars should
        // occupy exactly 1 row (not 2). Many wrap algos get this
        // wrong by writing the boundary char then immediately wrapping
        // for a phantom "next char".
        let mut t = inline_terminal(10, 5, 1);
        paint_viewport(&mut t, "[v]");
        push_history(&mut t, vec![Line::from("0123456789")]) // exactly 10 chars
            .expect("boundary push must succeed");
        assert_eq!(
            dump_buffer(&t),
            vec![
                "0123456789",
                "[v]       ",
                "          ",
                "          ",
                "          ",
            ]
        );
    }

    #[test]
    fn multiline_call_preserves_line_boundaries() {
        // A single `push_history` with multiple lines must not
        // collapse them into one wrapped paragraph. Each `Line`
        // boundary should produce a new row.
        let mut t = inline_terminal(20, 8, 1);
        paint_viewport(&mut t, "[v]");
        push_history(
            &mut t,
            vec![
                Line::from("alpha"),
                Line::from("beta"),
                Line::from("gamma"),
            ],
        )
        .expect("multi-line push must succeed");
        assert_eq!(
            dump_buffer(&t),
            vec![
                "alpha               ",
                "beta                ",
                "gamma               ",
                "[v]                 ",
                "                    ",
                "                    ",
                "                    ",
                "                    ",
            ]
        );
    }

    #[test]
    fn order_preserved_under_overflow() {
        // When content rolls into scrollback it must do so in the
        // order it was pushed — prove there's no reversal or
        // duplication at the overflow boundary.
        let mut t = inline_terminal(20, 5, 1);
        paint_viewport(&mut t, "[v]");
        push_history(&mut t, vec![Line::from("first")]).unwrap();
        push_history(&mut t, vec![Line::from("second")]).unwrap();
        push_history(&mut t, vec![Line::from("third")]).unwrap();
        push_history(&mut t, vec![Line::from("fourth")]).unwrap();
        push_history(&mut t, vec![Line::from("fifth")]).unwrap();
        push_history(&mut t, vec![Line::from("sixth")]).unwrap();
        assert_eq!(
            dump_scrollback(&t),
            vec![
                "first               ",
                "second              ",
            ]
        );
    }
}
