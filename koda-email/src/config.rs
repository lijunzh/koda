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
            .field("username", &"[REDACTED]")
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

    /// Serialize env-var mutations so parallel tests don't stomp each other.
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn set_required_vars() {
        unsafe {
            std::env::set_var("KODA_EMAIL_IMAP_HOST", "imap.example.com");
            std::env::set_var("KODA_EMAIL_USERNAME", "user@example.com");
            std::env::set_var("KODA_EMAIL_PASSWORD", "secret");
        }
    }

    fn clear_all_vars() {
        unsafe {
            for k in [
                "KODA_EMAIL_IMAP_HOST",
                "KODA_EMAIL_IMAP_PORT",
                "KODA_EMAIL_SMTP_HOST",
                "KODA_EMAIL_SMTP_PORT",
                "KODA_EMAIL_USERNAME",
                "KODA_EMAIL_PASSWORD",
            ] {
                std::env::remove_var(k);
            }
        }
    }

    #[test]
    fn test_setup_instructions_not_empty() {
        let msg = EmailConfig::setup_instructions();
        assert!(msg.contains("KODA_EMAIL_IMAP_HOST"));
        assert!(msg.contains("KODA_EMAIL_USERNAME"));
    }

    #[test]
    fn test_from_env_happy_path() {
        let _g = ENV_MUTEX.lock().unwrap();
        clear_all_vars();
        set_required_vars();
        let cfg = EmailConfig::from_env().unwrap();
        assert_eq!(cfg.imap_host, "imap.example.com");
        assert_eq!(cfg.imap_port, 993); // default
        assert_eq!(cfg.smtp_host, "smtp.example.com"); // derived
        assert_eq!(cfg.smtp_port, 587); // default
        assert_eq!(cfg.username, "user@example.com");
        assert_eq!(cfg.password, "secret");
    }

    #[test]
    fn test_from_env_custom_ports_and_smtp_host() {
        let _g = ENV_MUTEX.lock().unwrap();
        clear_all_vars();
        set_required_vars();
        unsafe {
            std::env::set_var("KODA_EMAIL_IMAP_PORT", "143");
            std::env::set_var("KODA_EMAIL_SMTP_HOST", "relay.example.com");
            std::env::set_var("KODA_EMAIL_SMTP_PORT", "465");
        }
        let cfg = EmailConfig::from_env().unwrap();
        assert_eq!(cfg.imap_port, 143);
        assert_eq!(cfg.smtp_host, "relay.example.com");
        assert_eq!(cfg.smtp_port, 465);
    }

    #[test]
    fn test_from_env_missing_imap_host() {
        let _g = ENV_MUTEX.lock().unwrap();
        clear_all_vars();
        let err = EmailConfig::from_env().unwrap_err();
        assert!(err.to_string().contains("KODA_EMAIL_IMAP_HOST"));
    }

    #[test]
    fn test_from_env_missing_username() {
        let _g = ENV_MUTEX.lock().unwrap();
        clear_all_vars();
        unsafe { std::env::set_var("KODA_EMAIL_IMAP_HOST", "imap.example.com") };
        let err = EmailConfig::from_env().unwrap_err();
        assert!(err.to_string().contains("KODA_EMAIL_USERNAME"));
    }

    #[test]
    fn test_from_env_missing_password() {
        let _g = ENV_MUTEX.lock().unwrap();
        clear_all_vars();
        unsafe {
            std::env::set_var("KODA_EMAIL_IMAP_HOST", "imap.example.com");
            std::env::set_var("KODA_EMAIL_USERNAME", "u@example.com");
        }
        let err = EmailConfig::from_env().unwrap_err();
        assert!(err.to_string().contains("KODA_EMAIL_PASSWORD"));
    }

    #[test]
    fn test_from_env_invalid_imap_port() {
        let _g = ENV_MUTEX.lock().unwrap();
        clear_all_vars();
        set_required_vars();
        unsafe { std::env::set_var("KODA_EMAIL_IMAP_PORT", "notaport") };
        let err = EmailConfig::from_env().unwrap_err();
        assert!(err.to_string().contains("KODA_EMAIL_IMAP_PORT"));
    }

    #[test]
    fn test_from_env_invalid_smtp_port() {
        let _g = ENV_MUTEX.lock().unwrap();
        clear_all_vars();
        set_required_vars();
        unsafe { std::env::set_var("KODA_EMAIL_SMTP_PORT", "99999") };
        let err = EmailConfig::from_env().unwrap_err();
        assert!(err.to_string().contains("KODA_EMAIL_SMTP_PORT"));
    }

    #[test]
    fn test_debug_redacts_credentials() {
        let _g = ENV_MUTEX.lock().unwrap();
        clear_all_vars();
        set_required_vars();
        let cfg = EmailConfig::from_env().unwrap();
        let debug = format!("{cfg:?}");
        assert!(
            !debug.contains("secret"),
            "password must be redacted: {debug}"
        );
        assert!(
            debug.contains("[REDACTED]"),
            "should show [REDACTED]: {debug}"
        );
        assert!(
            debug.contains("imap.example.com"),
            "host should be visible: {debug}"
        );
    }
}
