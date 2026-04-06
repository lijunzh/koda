//! IMAP client for reading and searching emails.
//!
//! Uses the synchronous `imap` crate with `tokio::task::spawn_blocking`
//! for async compatibility. Simpler and more battle-tested than async-imap.
//!
//! ## Testability seam
//!
//! Network I/O is isolated behind the `ImapOps` trait.  Production code uses
//! `RealImapSession`; tests inject `FakeImapSession` without opening a socket.

use crate::config::EmailConfig;
use anyhow::{Context, Result};

// ── Public types ──────────────────────────────────────────────────────────────

/// A parsed email summary (subject, sender, date, snippet).
#[derive(Debug, Clone, serde::Serialize)]
pub struct EmailSummary {
    pub uid: u32,
    pub subject: String,
    pub from: String,
    pub date: String,
    pub snippet: String,
}

// ── Testability trait seam ────────────────────────────────────────────────────

/// Raw, owned message data returned by the IMAP layer.
///
/// Decouples message parsing from the `imap::types::Fetch` lifetime maze
/// so `parse_messages` can be unit-tested without a live session.
pub(crate) struct RawMessage {
    pub uid: u32,
    pub headers: String,
    pub body: String,
}

/// Minimal IMAP operations needed by this module.
///
/// `RealImapSession` wraps the live `imap::Session`;
/// `FakeImapSession` is used in unit tests.
pub(crate) trait ImapOps {
    /// Select INBOX and return the total message count.
    fn select_inbox(&mut self) -> Result<u32>;
    /// Fetch messages by sequence-set range (e.g. `"1:5"` or `"3,7,9"`).
    fn fetch_range(&mut self, range: &str) -> Result<Vec<RawMessage>>;
    /// Execute an IMAP SEARCH command and return matching sequence numbers.
    fn search_uids(&mut self, query: &str) -> Result<Vec<u32>>;
    /// Gracefully log out (errors silently ignored by callers).
    fn logout_session(&mut self);
}

// ── Production implementation ─────────────────────────────────────────────────

pub(crate) struct RealImapSession {
    session: imap::Session<Box<dyn imap::ImapConnection>>,
}

impl ImapOps for RealImapSession {
    fn select_inbox(&mut self) -> Result<u32> {
        let mb = self
            .session
            .select("INBOX")
            .context("Failed to select INBOX")?;
        Ok(mb.exists)
    }

    fn fetch_range(&mut self, range: &str) -> Result<Vec<RawMessage>> {
        let fetches = self
            .session
            .fetch(
                range,
                "(UID BODY.PEEK[HEADER.FIELDS (FROM SUBJECT DATE)] BODY.PEEK[TEXT]<0.200>)",
            )
            .context("Failed to fetch messages")?;

        let mut raw = Vec::new();
        for fetch in fetches.iter() {
            let uid = fetch.uid.unwrap_or(0);
            let headers = String::from_utf8_lossy(fetch.header().unwrap_or_default()).into_owned();
            let body = String::from_utf8_lossy(fetch.text().unwrap_or_default()).into_owned();
            raw.push(RawMessage { uid, headers, body });
        }
        Ok(raw)
    }

    fn search_uids(&mut self, query: &str) -> Result<Vec<u32>> {
        let set = self.session.search(query).context("IMAP search failed")?;
        Ok(set.into_iter().collect())
    }

    fn logout_session(&mut self) {
        self.session.logout().ok();
    }
}

// ── Pure message parsing ──────────────────────────────────────────────────────

/// Convert raw IMAP fetch data into `EmailSummary` values.
///
/// Pure function — no I/O, fully unit-testable.
pub(crate) fn parse_messages(raw: Vec<RawMessage>) -> Vec<EmailSummary> {
    let mut summaries: Vec<EmailSummary> = raw
        .into_iter()
        .map(|m| {
            let subject = extract_header(&m.headers, "Subject");
            let from = extract_header(&m.headers, "From");
            let date = extract_header(&m.headers, "Date");
            let snippet = clean_snippet(&m.body, 200);
            EmailSummary {
                uid: m.uid,
                subject,
                from,
                date,
                snippet,
            }
        })
        .collect();
    summaries.reverse(); // newest first
    summaries
}

// ── Synchronous implementations (generic over ImapOps) ────────────────────────

fn connect(config: &EmailConfig) -> Result<RealImapSession> {
    let client = imap::ClientBuilder::new(&config.imap_host, config.imap_port)
        .connect()
        .context("Failed to connect to IMAP server")?;
    let session = client
        .login(&config.username, &config.password)
        .map_err(|e| anyhow::anyhow!("IMAP login failed: {}", e.0))?;
    Ok(RealImapSession { session })
}

fn read_with_session<I: ImapOps>(session: &mut I, count: u32) -> Result<Vec<EmailSummary>> {
    let total = session.select_inbox()?;
    if total == 0 {
        session.logout_session();
        return Ok(Vec::new());
    }
    let start = total.saturating_sub(count) + 1;
    let range = format!("{start}:{total}");
    let raw = session.fetch_range(&range)?;
    session.logout_session();
    Ok(parse_messages(raw))
}

fn search_with_session<I: ImapOps>(
    session: &mut I,
    query: &str,
    max_results: u32,
) -> Result<Vec<EmailSummary>> {
    session.select_inbox()?;
    let search_cmd = build_search_query(query);
    let mut uids = session.search_uids(&search_cmd)?;
    if uids.is_empty() {
        session.logout_session();
        return Ok(Vec::new());
    }
    uids.sort();
    let take = uids.len().min(max_results as usize);
    let selected = &uids[uids.len() - take..];
    let range = selected
        .iter()
        .map(|u| u.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let raw = session.fetch_range(&range)?;
    session.logout_session();
    Ok(parse_messages(raw))
}

// ── Public async API ──────────────────────────────────────────────────────────

/// Fetch the last N emails from INBOX.
pub async fn read_emails(config: &EmailConfig, count: u32) -> Result<Vec<EmailSummary>> {
    let config = config.clone();
    tokio::task::spawn_blocking(move || {
        let mut session = connect(&config)?;
        read_with_session(&mut session, count)
    })
    .await
    .context("IMAP task panicked")?
}

/// Search emails by query.
pub async fn search_emails(
    config: &EmailConfig,
    query: &str,
    max_results: u32,
) -> Result<Vec<EmailSummary>> {
    let config = config.clone();
    let query = query.to_string();
    tokio::task::spawn_blocking(move || {
        let mut session = connect(&config)?;
        search_with_session(&mut session, &query, max_results)
    })
    .await
    .context("IMAP task panicked")?
}

// ── Pure helpers ──────────────────────────────────────────────────────────────

/// Build an IMAP SEARCH command from a user query.
///
/// Supports:
/// - Plain text → searches subject + body via OR
/// - `"from:user@example.com"` → FROM filter
/// - `"subject:keyword"` → SUBJECT filter
///
/// Input is sanitized to prevent IMAP search injection (#529).
fn build_search_query(query: &str) -> String {
    let q = sanitize_imap_string(query.trim());

    if let Some(addr) = q.strip_prefix("from:") {
        return format!("FROM \"{}\"", addr.trim());
    }
    if let Some(subj) = q.strip_prefix("subject:") {
        return format!("SUBJECT \"{}\"", subj.trim());
    }
    format!("OR SUBJECT \"{}\" BODY \"{}\"", q, q)
}

/// Escape backslashes and double-quotes for safe IMAP string interpolation.
fn sanitize_imap_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Extract a header value from raw header text.
fn extract_header(headers: &str, name: &str) -> String {
    let prefix = format!("{name}: ");
    headers
        .lines()
        .find(|line| line.starts_with(&prefix))
        .map(|line| line[prefix.len()..].trim().to_string())
        .unwrap_or_else(|| "(unknown)".to_string())
}

/// Clean a body snippet: strip HTML, collapse whitespace, truncate.
fn clean_snippet(raw: &str, max_len: usize) -> String {
    let mut result = String::new();
    let mut in_tag = false;
    for ch in raw.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(ch),
            _ => {}
        }
    }
    let collapsed: String = result.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.len() > max_len {
        format!("{}...", &collapsed[..max_len])
    } else {
        collapsed
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── FakeImapSession ────────────────────────────────────────────────────

    struct FakeImapSession {
        /// Messages returned by `fetch_range` (in insertion order).
        messages: Vec<RawMessage>,
        /// UIDs returned by `search_uids`.
        search_results: Vec<u32>,
        /// Total reported by `select_inbox`.
        total: u32,
    }

    impl FakeImapSession {
        fn with_messages(msgs: Vec<RawMessage>) -> Self {
            let total = msgs.len() as u32;
            Self {
                messages: msgs,
                search_results: vec![],
                total,
            }
        }
        fn empty() -> Self {
            Self {
                messages: vec![],
                search_results: vec![],
                total: 0,
            }
        }
        fn with_search(msgs: Vec<RawMessage>, uids: Vec<u32>) -> Self {
            let total = msgs.len() as u32;
            Self {
                messages: msgs,
                search_results: uids,
                total,
            }
        }
    }

    impl ImapOps for FakeImapSession {
        fn select_inbox(&mut self) -> Result<u32> {
            Ok(self.total)
        }
        fn fetch_range(&mut self, _range: &str) -> Result<Vec<RawMessage>> {
            Ok(std::mem::take(&mut self.messages))
        }
        fn search_uids(&mut self, _query: &str) -> Result<Vec<u32>> {
            Ok(self.search_results.clone())
        }
        fn logout_session(&mut self) {}
    }

    fn make_raw(uid: u32, subject: &str, from: &str) -> RawMessage {
        RawMessage {
            uid,
            headers: format!("Subject: {subject}\r\nFrom: {from}\r\nDate: Mon, 1 Jan 2024\r\n"),
            body: format!("Body of message {uid}"),
        }
    }

    fn fake_config() -> crate::config::EmailConfig {
        crate::config::EmailConfig {
            imap_host: "127.0.0.1".into(),
            imap_port: 1, // nothing listening — instant ECONNREFUSED
            smtp_host: "127.0.0.1".into(),
            smtp_port: 1,
            username: "user@example.com".into(),
            password: "pass".into(),
        }
    }

    // ── parse_messages ─────────────────────────────────────────────────────

    #[test]
    fn test_parse_messages_empty() {
        let result = parse_messages(vec![]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_messages_single() {
        let raw = vec![make_raw(1, "Hello", "alice@example.com")];
        let summaries = parse_messages(raw);
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].uid, 1);
        assert_eq!(summaries[0].subject, "Hello");
        assert_eq!(summaries[0].from, "alice@example.com");
        assert_eq!(summaries[0].date, "Mon, 1 Jan 2024");
        assert!(summaries[0].snippet.contains("Body of message 1"));
    }

    #[test]
    fn test_parse_messages_reversed_newest_first() {
        let raw = vec![
            make_raw(1, "Older", "a@b.com"),
            make_raw(2, "Newer", "a@b.com"),
        ];
        let summaries = parse_messages(raw);
        // reversed → uid 2 first
        assert_eq!(summaries[0].uid, 2);
        assert_eq!(summaries[1].uid, 1);
    }

    #[test]
    fn test_parse_messages_missing_header_falls_back() {
        let raw = vec![RawMessage {
            uid: 99,
            headers: String::new(),
            body: String::new(),
        }];
        let summaries = parse_messages(raw);
        assert_eq!(summaries[0].subject, "(unknown)");
        assert_eq!(summaries[0].from, "(unknown)");
    }

    // ── read_with_session ──────────────────────────────────────────────────

    #[test]
    fn test_read_with_session_empty_inbox() {
        let mut fake = FakeImapSession::empty();
        let result = read_with_session(&mut fake, 5).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_read_with_session_returns_messages() {
        let mut fake = FakeImapSession::with_messages(vec![
            make_raw(1, "First", "a@example.com"),
            make_raw(2, "Second", "b@example.com"),
        ]);
        let result = read_with_session(&mut fake, 10).unwrap();
        assert_eq!(result.len(), 2);
        // newest first
        assert_eq!(result[0].subject, "Second");
    }

    // ── search_with_session ────────────────────────────────────────────────

    #[test]
    fn test_search_with_session_no_results() {
        let mut fake = FakeImapSession::with_search(vec![], vec![]);
        let result = search_with_session(&mut fake, "missing", 10).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_search_with_session_returns_matches() {
        let msg = make_raw(3, "Meeting notes", "boss@example.com");
        let mut fake = FakeImapSession::with_search(vec![msg], vec![3]);
        let result = search_with_session(&mut fake, "meeting", 10).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].subject, "Meeting notes");
    }

    #[test]
    fn test_search_with_session_respects_max_results() {
        // 5 messages, max_results=2 → only last 2 UIDs fetched
        let msgs: Vec<RawMessage> = (1..=2).map(|i| make_raw(i, "msg", "x@y.com")).collect();
        let mut fake = FakeImapSession::with_search(msgs, vec![1, 2, 3, 4, 5]);
        let result = search_with_session(&mut fake, "msg", 2).unwrap();
        assert_eq!(result.len(), 2);
    }

    // ── build_search_query ─────────────────────────────────────────────────

    #[test]
    fn test_build_search_query_plain() {
        assert_eq!(
            build_search_query("meeting notes"),
            "OR SUBJECT \"meeting notes\" BODY \"meeting notes\""
        );
    }

    #[test]
    fn test_build_search_query_from() {
        assert_eq!(
            build_search_query("from:alice@example.com"),
            "FROM \"alice@example.com\""
        );
    }

    #[test]
    fn test_build_search_query_subject() {
        assert_eq!(
            build_search_query("subject:quarterly review"),
            "SUBJECT \"quarterly review\""
        );
    }

    #[test]
    fn test_build_search_query_escapes_quotes() {
        let result = build_search_query("hello\" BODY \"injected");
        assert!(
            result.contains(r#"\""#),
            "Should contain escaped quotes: {result}"
        );
        assert!(
            result.starts_with("OR SUBJECT \""),
            "Should start with OR SUBJECT: {result}"
        );
    }

    // ── sanitize_imap_string ───────────────────────────────────────────────

    #[test]
    fn test_sanitize_imap_string() {
        assert_eq!(sanitize_imap_string("hello"), "hello");
        assert_eq!(sanitize_imap_string("say \"hi\""), "say \\\"hi\\\"");
        assert_eq!(sanitize_imap_string("back\\slash"), "back\\\\slash");
    }

    // ── extract_header ─────────────────────────────────────────────────────

    #[test]
    fn test_extract_header() {
        let headers = "From: alice@example.com\r\nSubject: Hello\r\nDate: Mon, 1 Jan 2024\r\n";
        assert_eq!(extract_header(headers, "From"), "alice@example.com");
        assert_eq!(extract_header(headers, "Subject"), "Hello");
        assert_eq!(extract_header(headers, "Date"), "Mon, 1 Jan 2024");
        assert_eq!(extract_header(headers, "Missing"), "(unknown)");
    }

    // ── clean_snippet ──────────────────────────────────────────────────────

    #[test]
    fn test_clean_snippet_strips_html() {
        let raw = "<html><body><p>Hello <b>world</b></p></body></html>";
        assert_eq!(clean_snippet(raw, 100), "Hello world");
    }

    #[test]
    fn test_clean_snippet_truncates() {
        let raw = "a ".repeat(200);
        let result = clean_snippet(&raw, 20);
        assert!(result.len() <= 24); // 20 + "..."
        assert!(result.ends_with("..."));
    }

    #[test]
    fn test_clean_snippet_collapses_whitespace() {
        let raw = "hello    world\n\n  foo";
        assert_eq!(clean_snippet(raw, 100), "hello world foo");
    }

    // ── connect / async wrappers (ECONNREFUSED path) ─────────────────────────
    //
    // These tests point at 127.0.0.1:1 so they get instant ECONNREFUSED
    // without needing a real mail server.  The goal is line coverage of the
    // transport-building and async-wrapper code, not functional mail delivery.

    #[tokio::test]
    async fn test_read_emails_propagates_connection_error() {
        let cfg = fake_config();
        let result = read_emails(&cfg, 5).await;
        assert!(result.is_err(), "expected connection error, got ok");
    }

    #[tokio::test]
    async fn test_search_emails_propagates_connection_error() {
        let cfg = fake_config();
        let result = search_emails(&cfg, "subject:test", 10).await;
        assert!(result.is_err(), "expected connection error, got ok");
    }

    // ── connect() post-accept path (stub server accepts then drops) ──────────
    //
    // We spin a real TcpListener that accepts the socket and immediately
    // closes it.  The IMAP crate connects successfully, then fails reading
    // the server greeting — covering the lines *after* ClientBuilder::connect().

    #[tokio::test]
    async fn test_connect_fails_after_tcp_accept_no_greeting() {
        use tokio::net::TcpListener;

        // Bind to a random port on loopback.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Accept one connection and immediately drop it (no IMAP greeting).
        tokio::spawn(async move {
            let _ = listener.accept().await; // accept, then drop
        });

        let mut cfg = fake_config();
        cfg.imap_host = "127.0.0.1".into();
        cfg.imap_port = port;

        // The IMAP client will connect OK then fail at the greeting read.
        let result = read_emails(&cfg, 5).await;
        assert!(result.is_err(), "expected greeting error after TCP accept");
    }
}
