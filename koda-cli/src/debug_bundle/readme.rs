//! Generate the bundle README.md, opened first by humans cracking it open.

use super::metadata::Metadata;

/// Build the README that ships at the top level of every debug bundle.
///
/// Per RFC #1167: "what's in the bundle, opened first by humans" — so
/// this is intentionally orientation material, not a polished doc.
/// Optimised for "I just unzipped this in 10 seconds, now what?"
pub(super) fn render(meta: &Metadata) -> String {
    let mut out = String::new();
    out.push_str("# Koda debug bundle\n\n");
    out.push_str(&format!("Captured at: {}\n", meta.captured_at));
    out.push_str(&format!("Session ID:  {}\n", meta.session_id));
    if let Some(title) = &meta.session_title {
        out.push_str(&format!("Session:     {title}\n"));
    }
    out.push_str(&format!("Koda version: {}\n", meta.koda_version));
    if let Some(sha) = &meta.git_sha {
        out.push_str(&format!("Git SHA:      {sha}\n"));
    }
    if let Some(model) = &meta.model {
        out.push_str(&format!("Model:        {model}\n"));
    }
    if let Some(provider) = &meta.provider {
        out.push_str(&format!("Provider:     {provider}\n"));
    }
    out.push_str(&format!(
        "Platform:     {}/{}",
        meta.platform.os, meta.platform.arch
    ));
    if let Some(term) = &meta.platform.term {
        out.push_str(&format!(" ({term})"));
    }
    out.push('\n');

    out.push_str(
        r#"
## What's in this bundle

Read in this order if you're a human debugging:

1. **`README.md`** — you are here.
2. **`metadata.json`** — session/runtime context at a glance. Open this
   first to know what you're looking at.
3. **`conversation.md`** — the conversation as it appeared on the user's
   screen. Faithful to the live TUI render — same per-tool formatting,
   same status icons, same preview lines. Strip-styled to plain text so
   any text editor can read it.
4. **`messages.json`** — the raw DB dump of every message row. The
   model's-eye view of the session. Consult when `conversation.md`
   leaves a question unanswered ("did the model actually see X?").
5. **`logs/koda-<PID>.log`** — the full per-process tracing log for the
   koda invocation that wrote this bundle. Look here for engine events
   (sub-agent spawn, tool dispatch, retry, cancel) and warnings.
6. **`logs/panic.log`** (if present) — forensic record of any panics
   captured during the lifetime of THIS koda config dir. Not just this
   session — the panic log is shared across all koda processes that
   share `~/.config/koda/`. Filter by timestamp if you need to scope to
   one session. Absent file means no panics ever recorded.
7. **`env.txt`** — environment variables filtered through a hardcoded
   allowlist. Credentials are length-redacted (the question "is
   $OPENAI_API_KEY set?" is debuggable; the value is not in the bundle).
   Most user/path env vars are omitted entirely to avoid corp-ID leakage.

## What's NOT in this bundle

- The koda config file (`~/.config/koda/config.toml`) — may contain
  provider URLs or other deployment-sensitive info. Re-run with
  `--config` if reproduction needs a config tweak.
- The session database file itself — `messages.json` is the relevant
  slice. The DB has every other session you've ever run.
- Files referenced via `@file` attachments — those were inlined into
  the messages at the time and appear in `conversation.md` /
  `messages.json` already.

## Sharing this bundle

Bundles are designed to be shareable with colleagues OR pasted into
another LLM for second-opinion debugging. The redaction policy assumes
either consumer is acceptable. **Re-check `env.txt` before sharing
externally** — some allowlisted vars (e.g. `KODA_*` deploy hints) might
still be sensitive in your specific deployment.
"#,
    );
    out
}

#[cfg(test)]
mod tests {
    use super::super::metadata::{Platform, Totals};
    use super::*;

    fn fixture_meta() -> Metadata {
        Metadata {
            session_id: "abc-123".to_string(),
            session_title: Some("test session".to_string()),
            model: Some("claude-sonnet-4-6".to_string()),
            provider: Some("anthropic".to_string()),
            context_window: Some(200_000),
            totals: Totals::default(),
            koda_version: "0.2.25".to_string(),
            git_sha: Some("deadbeef".to_string()),
            started_at: Some("2026-04-30T10:00:00Z".to_string()),
            captured_at: "2026-04-30T11:00:00Z".to_string(),
            platform: Platform {
                os: "macos".to_string(),
                arch: "aarch64".to_string(),
                term: Some("xterm-256color".to_string()),
            },
        }
    }

    #[test]
    fn header_includes_session_and_version() {
        let readme = render(&fixture_meta());
        assert!(readme.contains("# Koda debug bundle"));
        assert!(readme.contains("abc-123"));
        assert!(readme.contains("0.2.25"));
        assert!(readme.contains("test session"));
        assert!(readme.contains("claude-sonnet-4-6"));
    }

    #[test]
    fn optional_fields_omitted_when_absent() {
        let mut meta = fixture_meta();
        meta.session_title = None;
        meta.git_sha = None;
        meta.model = None;
        meta.provider = None;

        let readme = render(&meta);
        assert!(!readme.contains("Session:    "), "session title leaked");
        assert!(!readme.contains("Git SHA:    "), "git sha leaked");
        assert!(!readme.contains("Model:      "), "model leaked");
        assert!(!readme.contains("Provider:   "), "provider leaked");
        // Required fields still present.
        assert!(readme.contains("abc-123"));
        assert!(readme.contains("0.2.25"));
    }

    #[test]
    fn lists_all_documented_files() {
        let readme = render(&fixture_meta());
        // Every file in the RFC bundle spec must be mentioned in the
        // readme so consumers know what they're looking at.
        for expected in [
            "README.md",
            "metadata.json",
            "conversation.md",
            "messages.json",
            "logs/koda-",
            "panic.log",
            "env.txt",
        ] {
            assert!(
                readme.contains(expected),
                "README missing reference to {expected}"
            );
        }
    }

    #[test]
    fn warns_about_external_sharing() {
        let readme = render(&fixture_meta());
        // Guardrail: a future refactor that strips the sharing warning
        // would be a real privacy regression. We pin two non-adjacent
        // phrases so the test fails loudly if the section is removed,
        // but tolerates re-flowing the line breaks for readability.
        assert!(
            readme.contains("Re-check"),
            "external-sharing warning trigger phrase removed"
        );
        assert!(
            readme.contains("externally"),
            "external-sharing warning recipient phrase removed"
        );
    }
}
