//! Native TUI wizard handlers — multi-step interactive flows.
//!
//! ## Wizards
//!
//! - **`/provider`** — provider selection menu (Anthropic, Gemini, etc.)
//! - **`/trust`** — set approval mode for the session
//! - **`/key`** — API key entry (masked input)
//!
//! All output flows through the [`crate::scroll_buffer::ScrollBuffer`] render cache.

use crate::scroll_buffer::ScrollBuffer;
use crate::tui_output;

use ratatui::text::{Line, Span};

use tui_output::{BOLD, DIM};

// ── Diff (native TUI) ───────────────────────────────

/// `git` in the TUI's directory, hardened by [`koda_git`]: the repository's
/// config can't make `/diff` run a program.
fn git(operation: koda_git::Operation, args: &[&str]) -> std::io::Result<std::process::Output> {
    koda_git::command(std::path::Path::new("."), operation)?
        .args(args)
        .args(koda_git::DIFF_ARGS)
        .output()
}

pub(crate) fn handle_diff(buffer: &mut ScrollBuffer) {
    let output = git(koda_git::Operation::WorkingTree, &["diff", "--stat"]);

    let diff_stat = match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            tui_output::err_msg(buffer, format!("Git error: {err}"));
            return;
        }
        Err(e) => {
            tui_output::err_msg(buffer, format!("Failed to run git: {e}"));
            return;
        }
    };

    let has_unstaged = !diff_stat.trim().is_empty();
    let staged_stat = if !has_unstaged {
        git(
            koda_git::Operation::Metadata,
            &["diff", "--cached", "--stat"],
        )
        .ok()
        .and_then(|o| {
            if o.status.success() {
                let s = String::from_utf8_lossy(&o.stdout).to_string();
                if s.trim().is_empty() { None } else { Some(s) }
            } else {
                None
            }
        })
    } else {
        None
    };

    let stat = if has_unstaged {
        diff_stat
    } else if let Some(s) = staged_stat {
        s
    } else {
        tui_output::dim_msg(buffer, "No uncommitted changes.".into());
        return;
    };

    tui_output::blank(buffer);
    tui_output::emit_line(
        buffer,
        Line::styled("  \u{1f43b} Uncommitted Changes", BOLD),
    );
    tui_output::blank(buffer);
    for line in stat.lines() {
        tui_output::dim_msg(buffer, line.to_string());
    }
    tui_output::blank(buffer);
    tui_output::dim_msg(
        buffer,
        "/diff review   \u{2014} ask Koda to review the changes".into(),
    );
    tui_output::dim_msg(
        buffer,
        "/diff commit   \u{2014} generate a commit message".into(),
    );
}

// ── API key management (/key) ─────────────────────────

/// `/key`'s table: each provider that takes a key, its variable, and
/// whether the host has a key for it (`modelProvider/list`).
pub(crate) fn handle_keys(buffer: &mut ScrollBuffer, providers: &crate::providers::Providers) {
    tui_output::emit_line(buffer, Line::styled("  \u{1f511} API Keys", BOLD));
    tui_output::blank(buffer);

    for provider in providers.all().iter().filter(|p| p.requires_api_key) {
        let (name, env_var) = (&provider.name, &provider.env_key);
        let is_set = provider.has_api_key;
        let status = if is_set {
            Span::styled("\u{2714} set", tui_output::GREEN)
        } else {
            Span::styled("  not set", DIM)
        };
        tui_output::emit_line(
            buffer,
            Line::from(vec![
                Span::raw(format!("  {name:<16}")),
                Span::styled(format!("{env_var:<24}"), DIM),
                status,
            ]),
        );
    }

    tui_output::blank(buffer);
    tui_output::dim_msg(
        buffer,
        "Pick a provider below to set or update its key.".into(),
    );
}
