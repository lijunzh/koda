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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_definitions_returns_three_tools() {
        let defs = tool_definitions();
        assert_eq!(defs.len(), 3);
        let names: Vec<_> = defs.iter().map(|d| d.name).collect();
        assert!(names.contains(&"EmailRead"));
        assert!(names.contains(&"EmailSend"));
        assert!(names.contains(&"EmailSearch"));
    }

    #[test]
    fn test_tool_definitions_schemas_are_valid_json() {
        for def in tool_definitions() {
            let v: serde_json::Value = serde_json::from_str(def.parameters_json)
                .unwrap_or_else(|e| panic!("{} has invalid JSON schema: {e}", def.name));
            assert_eq!(v["type"], "object", "{} schema must be an object", def.name);
        }
    }

    #[test]
    fn test_tool_definitions_descriptions_not_empty() {
        for def in tool_definitions() {
            assert!(
                !def.description.is_empty(),
                "{} has empty description",
                def.name
            );
        }
    }
}
