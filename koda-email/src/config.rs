//! Email server configuration loaded from environment variables.
//!
//! Credentials are expected as:
//! - KODA_EMAIL_IMAP_HOST, KODA_EMAIL_IMAP_PORT
//! - KODA_EMAIL_SMTP_HOST, KODA_EMAIL_SMTP_PORT
//! - KODA_EMAIL_USERNAME, KODA_EMAIL_PASSWORD

use anyhow::{Context, Result};

/// IMAP + SMTP connection settings.
#[derive(Clone)]
pub struct EmailConfig {
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for EmailConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmailConfig")
            .field("imap_host", &self.imap_host)
            .field("imap_port", &self.imap_port)
            .field("smtp_host", &self.smtp_host)
            .field("smtp_port", &self.smtp_port)
            .field("username", &self.username)
            .field("password", &"[REDACTED]")
            .finish()
    }
}

impl EmailConfig {
    /// Load config from environment variables.
    ///
    /// Required: KODA_EMAIL_IMAP_HOST, KODA_EMAIL_USERNAME, KODA_EMAIL_PASSWORD
    /// Optional: KODA_EMAIL_IMAP_PORT (default 993), KODA_EMAIL_SMTP_HOST,
    ///           KODA_EMAIL_SMTP_PORT (default 587)
    pub fn from_env() -> Result<Self> {
        let imap_host =
            std::env::var("KODA_EMAIL_IMAP_HOST").context("KODA_EMAIL_IMAP_HOST not set")?;
        let imap_port = std::env::var("KODA_EMAIL_IMAP_PORT")
            .unwrap_or_else(|_| "993".to_string())
            .parse::<u16>()
            .context("KODA_EMAIL_IMAP_PORT must be a valid port number")?;

        // Default SMTP host: derive from IMAP host (imap.example.com → smtp.example.com)
        let default_smtp_host = imap_host.replacen("imap", "smtp", 1);
        let smtp_host = std::env::var("KODA_EMAIL_SMTP_HOST").unwrap_or(default_smtp_host);
        let smtp_port = std::env::var("KODA_EMAIL_SMTP_PORT")
            .unwrap_or_else(|_| "587".to_string())
            .parse::<u16>()
            .context("KODA_EMAIL_SMTP_PORT must be a valid port number")?;

        let username =
            std::env::var("KODA_EMAIL_USERNAME").context("KODA_EMAIL_USERNAME not set")?;
        let password =
            std::env::var("KODA_EMAIL_PASSWORD").context("KODA_EMAIL_PASSWORD not set")?;

        Ok(Self {
            imap_host,
            imap_port,
            smtp_host,
            smtp_port,
            username,
            password,
        })
    }

    /// Human-readable missing-config error with setup instructions.
    pub fn setup_instructions() -> String {
        "koda-email requires email credentials via environment variables:\n\n\
         Required:\n  \
         KODA_EMAIL_IMAP_HOST=imap.gmail.com\n  \
         KODA_EMAIL_USERNAME=you@gmail.com\n  \
         KODA_EMAIL_PASSWORD=your-app-password\n\n\
         Optional:\n  \
         KODA_EMAIL_IMAP_PORT=993 (default)\n  \
         KODA_EMAIL_SMTP_HOST=smtp.gmail.com (derived from IMAP host)\n  \
         KODA_EMAIL_SMTP_PORT=587 (default)\n\n\
         For Gmail: use an App Password (Settings → Security → App Passwords).\n\
         For Outlook: use an App Password or IMAP must be enabled."
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_setup_instructions_not_empty() {
        let msg = EmailConfig::setup_instructions();
        assert!(msg.contains("KODA_EMAIL_IMAP_HOST"));
        assert!(msg.contains("KODA_EMAIL_USERNAME"));
    }
}
