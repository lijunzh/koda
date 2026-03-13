//! koda-email: MCP server for email read/send/search via IMAP/SMTP.
//!
//! Thin MCP wrapper around the `koda_email` library crate.
//! Part of the koda ecosystem — auto-provisioned on first use.

use koda_email::config::EmailConfig;
use koda_email::imap_client;
use koda_email::smtp_client;

use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ── Tool parameter types ───────────────────────────────────────

/// Parameters for EmailRead tool.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct EmailReadParams {
    /// Number of recent emails to fetch (default: 5, max: 20)
    #[serde(default = "default_count")]
    pub count: u32,
}

/// Parameters for EmailSend tool.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct EmailSendParams {
    /// Recipient email address
    pub to: String,
    /// Email subject line
    pub subject: String,
    /// Email body text
    pub body: String,
}

/// Parameters for EmailSearch tool.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct EmailSearchParams {
    /// Search query. Plain text searches subject+body.
    /// Prefix with "from:" or "subject:" for targeted search.
    pub query: String,
    /// Maximum results to return (default: 10)
    #[serde(default = "default_max_results")]
    pub max_results: u32,
}

fn default_count() -> u32 {
    5
}
fn default_max_results() -> u32 {
    10
}

// ── MCP Server ───────────────────────────────────────────
//
// NOTE: The #[tool(description = "...")] attributes below must stay in sync
// with `koda_email::tool_definitions()` in lib.rs (the authoritative source).

#[derive(Debug, Clone)]
struct EmailServer {
    config: Option<EmailConfig>,
    config_error: Option<String>,
    tool_router: ToolRouter<Self>,
}

impl EmailServer {
    fn new() -> Self {
        let (config, config_error) = match EmailConfig::from_env() {
            Ok(c) => (Some(c), None),
            Err(e) => (None, Some(format!("{e:#}"))),
        };
        Self {
            config,
            config_error,
            tool_router: Self::tool_router(),
        }
    }

    /// Get config or return a setup-instructions error.
    fn require_config(&self) -> Result<&EmailConfig, rmcp::ErrorData> {
        self.config.as_ref().ok_or_else(|| {
            let msg = format!(
                "Email not configured: {}\n\n{}",
                self.config_error.as_deref().unwrap_or("unknown error"),
                EmailConfig::setup_instructions()
            );
            rmcp::ErrorData::internal_error(msg, None)
        })
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for EmailServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = InitializeResult::new(ServerCapabilities::builder().enable_tools().build());
        info.server_info = Implementation::new("koda-email", env!("CARGO_PKG_VERSION"));
        info.instructions = Some(
            "Email server for reading, sending, and searching emails via IMAP/SMTP. \
             Configure with KODA_EMAIL_* environment variables."
                .to_string(),
        );
        info
    }
}

#[tool_router]
impl EmailServer {
    /// Read recent emails from your inbox.
    #[tool(
        name = "EmailRead",
        description = "Read recent emails from INBOX. Returns subject, sender, date, and a text snippet for each email. Use 'count' to control how many (default 5, max 20)."
    )]
    async fn email_read(
        &self,
        params: Parameters<EmailReadParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let config = self.require_config()?;
        let count = params.0.count.clamp(1, 20);

        match imap_client::read_emails(config, count).await {
            Ok(emails) if emails.is_empty() => Ok(CallToolResult::success(vec![Content::text(
                "No emails found in INBOX.",
            )])),
            Ok(emails) => {
                let output = format_email_list(&emails);
                Ok(CallToolResult::success(vec![Content::text(output)]))
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Error reading emails: {e:#}"
            ))])),
        }
    }

    /// Send an email.
    #[tool(
        name = "EmailSend",
        description = "Send an email via SMTP. Requires 'to' (recipient), 'subject', and 'body'."
    )]
    async fn email_send(
        &self,
        params: Parameters<EmailSendParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let config = self.require_config()?;
        let p = &params.0;

        match smtp_client::send_email(config, &p.to, &p.subject, &p.body).await {
            Ok(msg) => Ok(CallToolResult::success(vec![Content::text(msg)])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Error sending email: {e:#}"
            ))])),
        }
    }

    /// Search emails by query.
    #[tool(
        name = "EmailSearch",
        description = "Search emails in INBOX. Plain text searches subject and body. Use 'from:addr' to search by sender, 'subject:text' to search by subject line."
    )]
    async fn email_search(
        &self,
        params: Parameters<EmailSearchParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let config = self.require_config()?;
        let p = &params.0;
        let max = p.max_results.clamp(1, 50);

        match imap_client::search_emails(config, &p.query, max).await {
            Ok(emails) if emails.is_empty() => Ok(CallToolResult::success(vec![Content::text(
                format!("No emails found matching: {}", p.query),
            )])),
            Ok(emails) => {
                let output = format!(
                    "Found {} result(s) for \"{}\":\n\n{}",
                    emails.len(),
                    p.query,
                    format_email_list(&emails)
                );
                Ok(CallToolResult::success(vec![Content::text(output)]))
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Error searching emails: {e:#}"
            ))])),
        }
    }
}

/// Format email summaries for LLM-friendly output.
fn format_email_list(emails: &[imap_client::EmailSummary]) -> String {
    emails
        .iter()
        .enumerate()
        .map(|(i, e)| {
            format!(
                "{}. [{}] {}\n   From: {}\n   Date: {}\n   {}\n",
                i + 1,
                e.uid,
                e.subject,
                e.from,
                e.date,
                if e.snippet.is_empty() {
                    "(no preview)".to_string()
                } else {
                    e.snippet.clone()
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Handle --version flag
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("koda-email {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .with_writer(std::io::stderr)
        .init();

    tracing::info!("koda-email MCP server starting...");

    let server = EmailServer::new();
    if server.config.is_none() {
        tracing::warn!(
            "Email credentials not configured. Tools will return setup instructions.\n{}",
            EmailConfig::setup_instructions()
        );
    }

    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
