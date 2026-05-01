//! `/debug-bundle` slash command implementation (RFC #1167).
//!
//! Produces a self-contained `.zip` artifact a debugger (human or LLM)
//! can use to reason about a koda session without needing access to the
//! original machine.
//!
//! ## Bundle layout (per RFC #1167)
//!
//! ```text
//! koda-debug-{YYYYMMDD-HHMMSS}-{session-slug}.zip
//! ├── README.md            ← what's in the bundle, opened first by humans
//! ├── conversation.md      ← history_render → lines_to_text serializer
//! ├── messages.json        ← raw DB dump (every Message row)
//! ├── metadata.json        ← session/runtime context
//! ├── env.txt              ← allowlist-filtered env vars (D5)
//! └── logs/
//!     ├── koda-<PID>.log   ← FULL per-process log
//!     └── panic.log        ← if exists, full file (else absent)
//! ```
//!
//! ## Why `.zip` and not `.tar.gz`
//!
//! Both formats produce roughly the same compressed size for our content
//! (text-heavy, sub-MB total). The difference is **random access**: a zip
//! has a central directory at the tail, so reading one entry is
//! O(entry size). `tar.gz` is a stream, so reading one entry requires
//! decompressing the whole archive up to that point.
//!
//! For the consumer profile of a debug bundle (LLM agents poking at one
//! file at a time during a debugging session, claude.ai/chatgpt UI
//! attachment uploads, Windows colleagues browsing in Explorer), random
//! access wins decisively. The compression delta (a few percent on text)
//! is irrelevant at our archive sizes.
//!
//! ## Design choices (and why they're not options)
//!
//! - **No runtime knobs.** The env-var allowlist is hardcoded; bundle
//!   structure is fixed. To extend, edit source. Per RFC §D5: minimal
//!   runtime config = less surface area, no foot-guns.
//! - **`history_render` is the single source of truth.** `conversation.md`
//!   is produced by `history_render::render_history_messages` →
//!   `lines_to_text`. Same pipeline as the live TUI render and the
//!   resumed-session display. Per RFC §D1.
//! - **Logs copied verbatim, not tailed.** Per RFC §D4: each koda
//!   invocation gets its own log file; bundle takes the whole thing.
//! - **`panic.log` stays separate from `koda-{PID}.log` on disk.** Per
//!   RFC §D3. The bundle is the merge point, not the file system.

mod env_filter;
mod metadata;
mod readme;

use std::io::Write;
use std::path::{Path, PathBuf};

use koda_core::persistence::Message;
use ratatui::text::Line;
use serde_json::{Value, json};
use zip::CompressionMethod;
use zip::write::{SimpleFileOptions, ZipWriter};

use self::metadata::{Metadata, Totals};

/// Inputs the orchestrator needs to assemble a bundle.
///
/// Decoupled from `KodaSession` so unit tests can construct `BundleInput`
/// directly without standing up a database. The handler in
/// `tui_commands.rs` is the (only) place that translates a live session
/// into this shape.
pub struct BundleInput<'a> {
    pub session_id: &'a str,
    pub session_title: Option<&'a str>,
    pub session_started_at: Option<&'a str>,
    pub model: Option<&'a str>,
    pub provider: Option<&'a str>,
    pub context_window: Option<u64>,
    pub messages: &'a [Message],
    /// Used for log discovery (`<config_dir>/logs/koda-<PID>.log`).
    /// Passed as a parameter rather than read from the live process so
    /// tests can point at a temp dir.
    pub config_dir: &'a Path,
    pub current_pid: u32,
    /// Captured-at ISO 8601 timestamp. Caller-supplied (rather than
    /// `now()`-internal) so tests are deterministic.
    pub captured_at: &'a str,
    /// Where to write the resulting `.zip`. The directory is
    /// created if missing.
    pub output_dir: &'a Path,
}

/// Write a debug bundle. Returns the absolute path of the resulting
/// `.zip` on success.
///
/// The function is fully synchronous: a debug bundle is small (typically
/// <1 MB) and the user is staring at the TUI waiting for it. Async would
/// add complexity without latency benefit.
pub fn write_bundle(input: &BundleInput<'_>) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(input.output_dir)?;

    let bundle_path = input.output_dir.join(bundle_filename(input));
    let file = std::fs::File::create(&bundle_path)?;
    let mut zip = ZipWriter::new(file);

    let meta = build_metadata(input);

    // Each `add_file_to_zip` call produces a single entry. Order is the
    // order we want a human unzipping + `ls`-ing to see — README first.
    add_file_to_zip(&mut zip, "README.md", readme::render(&meta).as_bytes())?;
    add_file_to_zip(
        &mut zip,
        "metadata.json",
        serde_json::to_string_pretty(&meta.to_json())
            .unwrap_or_default()
            .as_bytes(),
    )?;
    add_file_to_zip(
        &mut zip,
        "conversation.md",
        render_conversation(input.messages).as_bytes(),
    )?;
    add_file_to_zip(
        &mut zip,
        "messages.json",
        serde_json::to_string_pretty(&messages_to_json(input.messages))
            .unwrap_or_default()
            .as_bytes(),
    )?;
    add_file_to_zip(&mut zip, "env.txt", render_env().as_bytes())?;

    // Logs: best-effort copy. Missing log file (e.g. headless test
    // harness without tracing init) is not an error — the bundle just
    // has no logs/ directory entry.
    add_logs_to_zip(&mut zip, input)?;

    // ZipWriter::finish writes the central directory at the tail.
    // Without this, the file is truncated and unreadable. Drop alone is
    // not sufficient — the writer needs to know we're done adding.
    zip.finish()?;

    Ok(bundle_path)
}

/// Build the bundle filename: `koda-debug-{YYYYMMDD-HHMMSS}-{slug}.zip`.
///
/// Slug comes from the session title, falling back to "session" if the
/// title is empty/missing. Slug is sanitized: lowercase, alphanumerics +
/// dashes only, max 32 chars.
fn bundle_filename(input: &BundleInput<'_>) -> String {
    let timestamp = compact_timestamp(input.captured_at);
    let slug = input
        .session_title
        .map(slugify)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "session".to_string());
    format!("koda-debug-{timestamp}-{slug}.zip")
}

/// Convert ISO 8601 `2026-04-30T22:48:43Z` into `20260430-224843` for use
/// in filenames. Implementation: extract all digits, slot the first 8 into
/// the date component and the next 6 into the time component. Anything
/// shorter degrades gracefully (still a valid filename).
fn compact_timestamp(iso: &str) -> String {
    let digits: String = iso.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() >= 14 {
        format!("{}-{}", &digits[..8], &digits[8..14])
    } else {
        digits
    }
}

/// Sanitize a string into a filesystem-safe slug.
fn slugify(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .chars()
        .take(32)
        .collect()
}

/// Add a single in-memory file to the zip stream with deflate compression
/// + Unix 0o644 permissions (so unzippers on Unix produce sane file modes).
fn add_file_to_zip<W: Write + std::io::Seek>(
    zip: &mut ZipWriter<W>,
    path: &str,
    contents: &[u8],
) -> std::io::Result<()> {
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o644);
    zip.start_file(path, options)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    zip.write_all(contents)
}

/// Copy log files into the zip. Best-effort: missing files are silently
/// skipped (a debug bundle from a headless test harness without tracing
/// init is still useful).
fn add_logs_to_zip<W: Write + std::io::Seek>(
    zip: &mut ZipWriter<W>,
    input: &BundleInput<'_>,
) -> std::io::Result<()> {
    let logs_dir = input.config_dir.join("logs");
    let process_log = logs_dir.join(format!("koda-{}.log", input.current_pid));
    if let Ok(contents) = std::fs::read(&process_log) {
        add_file_to_zip(
            zip,
            &format!("logs/koda-{}.log", input.current_pid),
            &contents,
        )?;
    }
    let panic_log = logs_dir.join("panic.log");
    if let Ok(contents) = std::fs::read(&panic_log) {
        add_file_to_zip(zip, "logs/panic.log", &contents)?;
    }
    Ok(())
}

/// Build the [`Metadata`] snapshot from inputs + runtime sniffing.
fn build_metadata(input: &BundleInput<'_>) -> Metadata {
    Metadata {
        session_id: input.session_id.to_string(),
        session_title: input.session_title.map(str::to_string),
        model: input.model.map(str::to_string),
        provider: input.provider.map(str::to_string),
        context_window: input.context_window,
        totals: compute_totals(input.messages),
        koda_version: metadata::current_koda_version(),
        git_sha: metadata::current_git_sha(),
        started_at: input.session_started_at.map(str::to_string),
        captured_at: input.captured_at.to_string(),
        platform: metadata::current_platform(),
    }
}

/// One pass over the message slice to compute the at-a-glance counts the
/// metadata block exposes.
///
/// `tool_calls` count: each row's `tool_calls` is a serialized JSON array.
/// We parse it once per row to count entries (not just "row has any").
/// This keeps the totals semantically meaningful when an assistant turn
/// fans out to multiple parallel tool calls.
fn compute_totals(messages: &[Message]) -> Totals {
    let mut t = Totals::default();
    for m in messages {
        match m.role.as_str() {
            "user" => t.user_msgs += 1,
            "assistant" => t.assistant_msgs += 1,
            _ => {}
        }
        if let Some(json_str) = m.tool_calls.as_deref()
            && let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(json_str)
        {
            t.tool_calls += arr.len() as u64;
        }
        t.tokens_in += m.prompt_tokens.unwrap_or(0).max(0) as u64;
        t.tokens_out += m.completion_tokens.unwrap_or(0).max(0) as u64;
    }
    t
}

/// Render the conversation by routing through `history_render` (RFC §D1
/// — single renderer) and flattening to plain text via `lines_to_text`.
///
/// This is the function that makes the bundle's `conversation.md` match
/// the user's screen byte-for-byte (modulo styling).
fn render_conversation(messages: &[Message]) -> String {
    let lines = crate::history_render::render_history_messages(messages);
    lines_to_text(&lines)
}

/// Flatten styled `ratatui::Line`s to plain text.
///
/// Per RFC §D1, this is THE serializer that lets `history_render`'s
/// output reach non-TUI surfaces without re-implementing per-tool
/// formatting. Strips ratatui::Style (color, bold, underline) and joins
/// every span's content with no separator (spans inside a Line are
/// already byte-adjacent in the visual flow).
pub(crate) fn lines_to_text(lines: &[Line<'_>]) -> String {
    let mut out = String::new();
    for line in lines {
        for span in &line.spans {
            out.push_str(&span.content);
        }
        out.push('\n');
    }
    out
}

/// Dump messages to a `serde_json::Value` for the `messages.json` file.
///
/// Format mirrors the DB row shape — the bundle reader's job is to reason
/// about what koda saw, so flatten as little as possible. `role` is
/// rendered via `Role::as_str()` since the enum doesn't impl `Serialize`
/// directly (and lowercase string is the on-disk format anyway).
/// `tool_calls` is the raw serialized JSON string from the DB — callers
/// can `JSON.parse` it themselves to inspect.
fn messages_to_json(messages: &[Message]) -> Value {
    let array: Vec<Value> = messages
        .iter()
        .map(|m| {
            json!({
                "id": m.id,
                "session_id": m.session_id,
                "role": m.role.as_str(),
                "content": m.content,
                "full_content": m.full_content,
                "tool_calls": m.tool_calls,
                "tool_call_id": m.tool_call_id,
                "created_at": m.created_at,
                "prompt_tokens": m.prompt_tokens,
                "completion_tokens": m.completion_tokens,
                "cache_read_tokens": m.cache_read_tokens,
                "cache_creation_tokens": m.cache_creation_tokens,
                "thinking_tokens": m.thinking_tokens,
                "thinking_content": m.thinking_content,
            })
        })
        .collect();
    json!({ "messages": array })
}

/// Filter and render the current process's env vars per RFC §D5.
fn render_env() -> String {
    let filtered = env_filter::filter(std::env::vars());
    env_filter::render(&filtered)
}

#[cfg(test)]
#[allow(unused_imports)]
pub(crate) use lines_to_text as _ensure_pub_visible;

#[cfg(test)]
mod tests {
    use super::*;
    use koda_core::persistence::Message;
    use ratatui::text::Span;

    fn temp_dir(label: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "koda-debug-bundle-test-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn fixture_message(id: i64, role: koda_core::persistence::Role, content: &str) -> Message {
        Message {
            id,
            session_id: "sess-1".to_string(),
            role,
            content: Some(content.to_string()),
            full_content: None,
            tool_calls: None,
            tool_call_id: None,
            prompt_tokens: Some(100),
            completion_tokens: Some(50),
            cache_read_tokens: None,
            cache_creation_tokens: None,
            thinking_tokens: None,
            thinking_content: None,
            created_at: Some("2026-04-30T10:00:00Z".to_string()),
        }
    }

    #[test]
    fn lines_to_text_strips_styling_preserves_content() {
        let line1 = Line::from(vec![
            Span::styled(
                "hello ",
                ratatui::style::Style::default().fg(ratatui::style::Color::Red),
            ),
            Span::raw("world"),
        ]);
        let line2 = Line::from("second line");
        let text = lines_to_text(&[line1, line2]);
        assert_eq!(text, "hello world\nsecond line\n");
    }

    #[test]
    fn lines_to_text_empty_input() {
        let text = lines_to_text(&[]);
        assert_eq!(text, "");
    }

    #[test]
    fn slugify_sanitizes_aggressively() {
        assert_eq!(slugify("Hello World"), "hello-world");
        assert_eq!(slugify("foo!@#bar"), "foo-bar");
        assert_eq!(slugify("---leading-trailing---"), "leading-trailing");
        assert_eq!(slugify(""), "");
        assert_eq!(slugify("正常 unicode"), "unicode"); // non-ASCII drops to dashes, then trims
        // Length cap.
        let long = "a".repeat(100);
        assert_eq!(slugify(&long).len(), 32);
    }

    #[test]
    fn compact_timestamp_strips_iso_separators() {
        assert_eq!(compact_timestamp("2026-04-30T22:48:43Z"), "20260430-224843");
        // Subsecond fractions: extra digits flow into the time slot beyond 6,
        // which we discard since the function format is exactly DATE-HHMMSS.
        // Trailing fraction digits are dropped silently.
        assert_eq!(
            compact_timestamp("2026-04-30T22:48:43.123Z"),
            "20260430-224843"
        );
        // Degraded input — just collect what digits there are.
        assert_eq!(compact_timestamp("2026-04"), "202604");
    }

    #[test]
    fn bundle_filename_uses_slug_and_timestamp() {
        let messages: Vec<Message> = vec![];
        let config_dir = temp_dir("filename");
        let output_dir = temp_dir("filename-out");
        let input = BundleInput {
            session_id: "abc",
            session_title: Some("Fix the WaitTask bug"),
            session_started_at: None,
            model: None,
            provider: None,
            context_window: None,
            messages: &messages,
            config_dir: &config_dir,
            current_pid: 1234,
            captured_at: "2026-04-30T22:48:43Z",
            output_dir: &output_dir,
        };
        let name = bundle_filename(&input);
        assert!(name.starts_with("koda-debug-20260430-224843-"));
        assert!(name.contains("fix-the-waittask-bug"));
        assert!(name.ends_with(".zip"));
    }

    #[test]
    fn bundle_filename_falls_back_when_title_missing() {
        let messages: Vec<Message> = vec![];
        let config_dir = temp_dir("noslug");
        let output_dir = temp_dir("noslug-out");
        let input = BundleInput {
            session_id: "abc",
            session_title: None,
            session_started_at: None,
            model: None,
            provider: None,
            context_window: None,
            messages: &messages,
            config_dir: &config_dir,
            current_pid: 1234,
            captured_at: "2026-04-30T22:48:43Z",
            output_dir: &output_dir,
        };
        let name = bundle_filename(&input);
        assert!(name.contains("-session.zip"));
    }

    #[test]
    fn compute_totals_walks_messages_once() {
        use koda_core::persistence::Role;
        let messages = vec![
            fixture_message(1, Role::User, "hello"),
            fixture_message(2, Role::Assistant, "hi back"),
            fixture_message(3, Role::User, "another"),
            fixture_message(4, Role::System, "ignored in counts"),
        ];
        let totals = compute_totals(&messages);
        assert_eq!(totals.user_msgs, 2);
        assert_eq!(totals.assistant_msgs, 1);
        assert_eq!(totals.tool_calls, 0);
        assert_eq!(totals.tokens_in, 400); // 4 * 100
        assert_eq!(totals.tokens_out, 200); // 4 * 50
    }

    #[test]
    fn messages_to_json_round_trips_required_fields() {
        use koda_core::persistence::Role;
        let messages = vec![fixture_message(1, Role::User, "hello")];
        let value = messages_to_json(&messages);
        let arr = value["messages"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], 1);
        assert_eq!(arr[0]["role"], "user");
        assert_eq!(arr[0]["content"], "hello");
        assert_eq!(arr[0]["prompt_tokens"], 100);
        assert_eq!(arr[0]["completion_tokens"], 50);
    }
    /// Integration test: produce a real bundle, unzip it (in memory),
    /// verify every documented file is present and well-formed.
    #[test]
    fn write_bundle_produces_complete_zip() {
        use std::io::Read;

        let messages = vec![
            fixture_message(1, koda_core::persistence::Role::User, "Hello koda"),
            fixture_message(
                2,
                koda_core::persistence::Role::Assistant,
                "Hi! What can I help with?",
            ),
        ];
        let config_dir = temp_dir("integration-cfg");
        let output_dir = temp_dir("integration-out");

        // Seed a fake log file so we exercise the log-copy path.
        std::fs::create_dir_all(config_dir.join("logs")).unwrap();
        std::fs::write(
            config_dir.join("logs").join(format!("koda-{}.log", 9999)),
            b"INFO koda starting up\nINFO session created\n",
        )
        .unwrap();

        let input = BundleInput {
            session_id: "test-session-id",
            session_title: Some("Integration test bundle"),
            session_started_at: Some("2026-04-30T10:00:00Z"),
            model: Some("test-model"),
            provider: Some("test-provider"),
            context_window: Some(200_000),
            messages: &messages,
            config_dir: &config_dir,
            current_pid: 9999,
            captured_at: "2026-04-30T11:00:00Z",
            output_dir: &output_dir,
        };

        let bundle_path = write_bundle(&input).expect("bundle write should succeed");
        assert!(bundle_path.exists(), "bundle file not created");
        assert!(bundle_path.extension().map(|e| e == "zip").unwrap_or(false));

        // Unzip in memory and collect every entry.
        let file = std::fs::File::open(&bundle_path).unwrap();
        let mut archive = zip::ZipArchive::new(file).expect("valid zip");
        let mut found: std::collections::HashMap<String, Vec<u8>> = Default::default();
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).unwrap();
            let path = entry.name().to_string();
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).unwrap();
            found.insert(path, buf);
        }

        // Every documented file must be present.
        for required in [
            "README.md",
            "metadata.json",
            "conversation.md",
            "messages.json",
            "env.txt",
            "logs/koda-9999.log",
        ] {
            assert!(
                found.contains_key(required),
                "bundle missing required file: {required} (have: {:?})",
                found.keys().collect::<Vec<_>>()
            );
        }

        // panic.log should be ABSENT (we didn't seed one).
        assert!(
            !found.contains_key("logs/panic.log"),
            "panic.log appeared but no source file was seeded"
        );

        // Random-access sanity check — one of the headline reasons we
        // chose zip over tar.gz. Read just `metadata.json` by name
        // without iterating the archive.
        let file2 = std::fs::File::open(&bundle_path).unwrap();
        let mut archive2 = zip::ZipArchive::new(file2).expect("valid zip");
        let mut entry = archive2
            .by_name("metadata.json")
            .expect("random-access by name");
        let mut buf = String::new();
        entry.read_to_string(&mut buf).unwrap();
        let direct: Value = serde_json::from_str(&buf).unwrap();
        assert_eq!(direct["session_id"], "test-session-id");

        // Spot-check content well-formedness.
        let metadata: Value =
            serde_json::from_slice(&found["metadata.json"]).expect("metadata.json valid JSON");
        assert_eq!(metadata["session_id"], "test-session-id");
        assert_eq!(metadata["totals"]["user_msgs"], 1);
        assert_eq!(metadata["totals"]["assistant_msgs"], 1);

        let messages_dump: Value =
            serde_json::from_slice(&found["messages.json"]).expect("messages.json valid JSON");
        assert_eq!(messages_dump["messages"].as_array().unwrap().len(), 2);

        let conversation = String::from_utf8(found["conversation.md"].clone()).unwrap();
        assert!(
            !conversation.is_empty(),
            "conversation.md should not be empty"
        );

        let log = String::from_utf8(found["logs/koda-9999.log"].clone()).unwrap();
        assert!(log.contains("koda starting up"));

        // Bundle filename must include slug + timestamp.
        let name = bundle_path.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("koda-debug-20260430-110000-"));
        assert!(name.contains("integration-test-bundle"));
    }

    #[test]
    fn write_bundle_with_panic_log_includes_it() {
        use std::io::Read;

        let messages: Vec<Message> = vec![];
        let config_dir = temp_dir("panic-cfg");
        let output_dir = temp_dir("panic-out");
        std::fs::create_dir_all(config_dir.join("logs")).unwrap();
        std::fs::write(
            config_dir.join("logs").join("panic.log"),
            b"--- panic at 2026-04-30T11:00:00Z ---\nthread 'main' panicked\n",
        )
        .unwrap();

        let input = BundleInput {
            session_id: "p",
            session_title: None,
            session_started_at: None,
            model: None,
            provider: None,
            context_window: None,
            messages: &messages,
            config_dir: &config_dir,
            current_pid: 1,
            captured_at: "2026-04-30T11:00:00Z",
            output_dir: &output_dir,
        };
        let path = write_bundle(&input).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let mut archive = zip::ZipArchive::new(file).expect("valid zip");
        let mut entry = archive
            .by_name("logs/panic.log")
            .expect("panic.log seeded but missing from bundle");
        let mut buf = String::new();
        entry.read_to_string(&mut buf).unwrap();
        assert!(buf.contains("panicked"));
    }
    #[test]
    fn write_bundle_succeeds_with_no_logs_dir() {
        // Headless / fresh-config-dir case: log copying is best-effort,
        // bundle still ships.
        let messages: Vec<Message> = vec![];
        let config_dir = temp_dir("nologs-cfg"); // empty
        let output_dir = temp_dir("nologs-out");
        let input = BundleInput {
            session_id: "n",
            session_title: None,
            session_started_at: None,
            model: None,
            provider: None,
            context_window: None,
            messages: &messages,
            config_dir: &config_dir,
            current_pid: 1,
            captured_at: "2026-04-30T11:00:00Z",
            output_dir: &output_dir,
        };
        let path = write_bundle(&input).expect("bundle should write even with no logs");
        assert!(path.exists());
    }
}
