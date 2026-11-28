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

/// The untracked files as `/diff` lists them: one line each, at most
/// [`UNTRACKED_SHOWN`], then how many more.
const UNTRACKED_SHOWN: usize = 20;

fn untracked_summary(files: &[String]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let mut text = format!(" {} untracked:\n", files.len());
    for file in files.iter().take(UNTRACKED_SHOWN) {
        text.push_str(&format!("   {file}\n"));
    }
    if files.len() > UNTRACKED_SHOWN {
        text.push_str(&format!(
            "   … and {} more\n",
            files.len() - UNTRACKED_SHOWN
        ));
    }
    text
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

    // New files git doesn't track yet, as Codex's `/diff` includes them.
    let untracked = koda_git::untracked_files(std::path::Path::new("."));
    let stat = if has_unstaged {
        diff_stat
    } else if let Some(s) = staged_stat {
        s
    } else if !untracked.is_empty() {
        String::new()
    } else {
        tui_output::dim_msg(buffer, "No uncommitted changes.".into());
        return;
    };
    let stat = format!("{stat}{}", untracked_summary(&untracked));

    tui_output::blank(buffer);
    tui_output::emit_line(buffer, Line::styled("  Uncommitted Changes", BOLD));
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
/// whether the host has a key for it (`modelProvider/list`), then Brave
/// Search's for `WebSearch` (`webSearch/apiKey/read`).
pub(crate) fn handle_keys(
    buffer: &mut ScrollBuffer,
    providers: &crate::providers::Providers,
    web_search: Option<&koda_client::protocol::config::WebSearchApiKeyReadResponse>,
) {
    tui_output::emit_line(buffer, Line::styled("  \u{1f511} API Keys", BOLD));
    tui_output::blank(buffer);

    let providers = providers
        .all()
        .iter()
        .filter(|p| p.requires_api_key)
        .map(|p| (&p.name, &p.env_key, p.has_api_key));
    let web_search = web_search.map(|k| (&k.name, &k.env_key, k.has_api_key));
    for (name, env_var, is_set) in providers.chain(web_search) {
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
        "Pick one below to set or update its key. Brave Search is for WebSearch on models \
         other than Claude."
            .into(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untracked_files_are_listed_up_to_the_cap() {
        assert_eq!(untracked_summary(&[]), "");
        let two = untracked_summary(&["a.rs".into(), "b.rs".into()]);
        assert_eq!(two, " 2 untracked:\n   a.rs\n   b.rs\n");
        let many: Vec<String> = (0..25).map(|i| format!("f{i}")).collect();
        let shown = untracked_summary(&many);
        assert!(shown.starts_with(" 25 untracked:\n"), "{shown}");
        assert!(
            shown.contains("   f19\n") && !shown.contains("   f20\n"),
            "{shown}"
        );
        assert!(shown.ends_with("   … and 5 more\n"), "{shown}");
    }
}
