//! IMAP client for reading and searching emails.
//!
//! Uses the synchronous `imap` crate with `tokio::task::spawn_blocking`
//! for async compatibility. Simpler and more battle-tested than async-imap.

use crate::config::EmailConfig;
use anyhow::{Context, Result};

/// A parsed email summary (subject, sender, date, snippet).
#[derive(Debug, Clone, serde::Serialize)]
pub struct EmailSummary {
    pub uid: u32,
    pub subject: String,
    pub from: String,
    pub date: String,
    pub snippet: String,
}

/// Fetch the last N emails from INBOX.
pub async fn read_emails(config: &EmailConfig, count: u32) -> Result<Vec<EmailSummary>> {
    let config = config.clone();
    tokio::task::spawn_blocking(move || read_emails_sync(&config, count))
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
    tokio::task::spawn_blocking(move || search_emails_sync(&config, &query, max_results))
        .await
        .context("IMAP task panicked")?
}

// ── Synchronous implementations ───────────────────────────────

fn connect(config: &EmailConfig) -> Result<imap::Session<Box<dyn imap::ImapConnection>>> {
    let client = imap::ClientBuilder::new(&config.imap_host, config.imap_port)
        .connect()
        .context("Failed to connect to IMAP server")?;
    let session = client
        .login(&config.username, &config.password)
        .map_err(|e| anyhow::anyhow!("IMAP login failed: {}", e.0))?;
    Ok(session)
}

fn read_emails_sync(config: &EmailConfig, count: u32) -> Result<Vec<EmailSummary>> {
    let mut session = connect(config)?;
    let mailbox = session.select("INBOX").context("Failed to select INBOX")?;

    let total = mailbox.exists;
    if total == 0 {
        session.logout().ok();
        return Ok(Vec::new());
    }

    let start = total.saturating_sub(count) + 1;
    let range = format!("{start}:{total}");
    let summaries = fetch_messages(&mut session, &range)?;

    session.logout().ok();
    Ok(summaries)
}

fn search_emails_sync(
    config: &EmailConfig,
    query: &str,
    max_results: u32,
) -> Result<Vec<EmailSummary>> {
    let mut session = connect(config)?;
    session.select("INBOX").context("Failed to select INBOX")?;

    let search_cmd = build_search_query(query);
    let uids = session.search(&search_cmd).context("IMAP search failed")?;

    if uids.is_empty() {
        session.logout().ok();
        return Ok(Vec::new());
    }

    // Take last N results (most recent)
    let mut uid_list: Vec<u32> = uids.into_iter().collect();
    uid_list.sort();
    let take = uid_list.len().min(max_results as usize);
    let selected = &uid_list[uid_list.len() - take..];
    let range = selected
        .iter()
        .map(|u| u.to_string())
        .collect::<Vec<_>>()
        .join(",");

    let summaries = fetch_messages(&mut session, &range)?;
    session.logout().ok();
    Ok(summaries)
}

/// Build an IMAP SEARCH command from a user query.
///
/// Supports:
/// - Plain text → searches subject + body via OR
/// - "from:user@example.com" → FROM filter
/// - "subject:keyword" → SUBJECT filter
fn build_search_query(query: &str) -> String {
    let q = query.trim();

    if let Some(addr) = q.strip_prefix("from:") {
        return format!("FROM \"{}\"", addr.trim());
    }
    if let Some(subj) = q.strip_prefix("subject:") {
        return format!("SUBJECT \"{}\"", subj.trim());
    }

    // Default: search subject OR body
    format!("OR SUBJECT \"{}\" BODY \"{}\"", q, q)
}

/// Fetch messages by sequence range and parse into summaries.
fn fetch_messages(
    session: &mut imap::Session<Box<dyn imap::ImapConnection>>,
    range: &str,
) -> Result<Vec<EmailSummary>> {
    let fetches = session
        .fetch(
            range,
            "(UID BODY.PEEK[HEADER.FIELDS (FROM SUBJECT DATE)] BODY.PEEK[TEXT]<0.200>)",
        )
        .context("Failed to fetch messages")?;

    let mut summaries = Vec::new();

    for fetch in fetches.iter() {
        let uid = fetch.uid.unwrap_or(0);

        // Parse headers
        let header_bytes = fetch.header().unwrap_or_default();
        let header_str = String::from_utf8_lossy(header_bytes);

        let subject = extract_header(&header_str, "Subject");
        let from = extract_header(&header_str, "From");
        let date = extract_header(&header_str, "Date");

        // Parse body snippet
        let body_bytes = fetch.text().unwrap_or_default();
        let body_raw = String::from_utf8_lossy(body_bytes);
        let snippet = clean_snippet(&body_raw, 200);

        summaries.push(EmailSummary {
            uid,
            subject,
            from,
            date,
            snippet,
        });
    }

    // Newest first
    summaries.reverse();
    Ok(summaries)
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
    // Strip HTML tags (simple state-machine approach)
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

    // Collapse whitespace
    let collapsed: String = result.split_whitespace().collect::<Vec<_>>().join(" ");

    if collapsed.len() > max_len {
        format!("{}...", &collapsed[..max_len])
    } else {
        collapsed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_extract_header() {
        let headers = "From: alice@example.com\r\nSubject: Hello\r\nDate: Mon, 1 Jan 2024\r\n";
        assert_eq!(extract_header(headers, "From"), "alice@example.com");
        assert_eq!(extract_header(headers, "Subject"), "Hello");
        assert_eq!(extract_header(headers, "Date"), "Mon, 1 Jan 2024");
        assert_eq!(extract_header(headers, "Missing"), "(unknown)");
    }

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
}
