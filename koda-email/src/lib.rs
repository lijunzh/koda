//! koda-email: Email read/send/search library via IMAP/SMTP.
//!
//! Provides email operations for the koda ecosystem.
//! This is the library crate. For the MCP server binary, see `main.rs`.

pub mod config;
pub mod imap_client;
pub mod smtp_client;

/// Tool definition metadata for consumers (koda-core ToolRegistry).
///
/// This is the single source of truth for email tool schemas.
/// Both the MCP wrapper (`main.rs`) and direct integrations use this.
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters_json: &'static str,
}

/// Returns tool definitions exported by this crate.
pub fn tool_definitions() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "EmailRead",
            description: "Read recent emails from INBOX. Returns subject, sender, date, \
                 and a text snippet for each email. Use 'count' to control how many \
                 (default 5, max 20).",
            parameters_json: r#"{"type":"object","properties":{"count":{"type":"integer","description":"Number of recent emails to fetch (default 5, max 20)"}},"required":[]}"#,
        },
        ToolDef {
            name: "EmailSend",
            description: "Send an email via SMTP. Requires 'to' (recipient), 'subject', \
                 and 'body'.",
            parameters_json: r#"{"type":"object","properties":{"to":{"type":"string","description":"Recipient email address"},"subject":{"type":"string","description":"Email subject line"},"body":{"type":"string","description":"Email body text"}},"required":["to","subject","body"]}"#,
        },
        ToolDef {
            name: "EmailSearch",
            description: "Search emails in INBOX. Plain text searches subject and body. \
                 Use 'from:addr' to search by sender, 'subject:text' to search by subject line.",
            parameters_json: r#"{"type":"object","properties":{"query":{"type":"string","description":"Search query. Use 'from:' or 'subject:' prefixes for targeted search."},"max_results":{"type":"integer","description":"Max results (default 10, max 50)"}},"required":["query"]}"#,
        },
    ]
}
