//! SMTP client for sending emails via lettre.

use crate::config::EmailConfig;
use anyhow::{Context, Result};
use lettre::{
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    message::{Mailbox, MessageBuilder, header::ContentType},
    transport::smtp::authentication::Credentials,
};

// ── Testable trait seam ───────────────────────────────────────────────────────

/// Abstraction over an SMTP mailer so tests can inject a fake.
pub(crate) trait MailSender {
    async fn send_message(&self, email: Message) -> Result<String>;
}

/// Production implementation backed by lettre's async SMTP transport.
pub(crate) struct LettreMailer {
    inner: AsyncSmtpTransport<Tokio1Executor>,
    to: String,
}

impl MailSender for LettreMailer {
    async fn send_message(&self, email: Message) -> Result<String> {
        let response = self
            .inner
            .send(email)
            .await
            .context("Failed to send email via SMTP")?;
        Ok(format!(
            "Email sent to {} (status: {})",
            self.to,
            response.code()
        ))
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Build a lettre `Message` from raw parts.
///
/// Separated from transport so address parsing and message construction
/// can be unit-tested without opening a socket.
pub(crate) fn build_message(
    from: &str,
    to: &str,
    subject: &str,
    body: &str,
) -> Result<(Message, Mailbox)> {
    let from_addr: Mailbox = from
        .parse()
        .context("Invalid sender email address in KODA_EMAIL_USERNAME")?;
    let to_addr: Mailbox = to
        .parse()
        .context(format!("Invalid recipient email address: {to}"))?;

    let email = MessageBuilder::new()
        .from(from_addr)
        .to(to_addr.clone())
        .subject(subject)
        .header(ContentType::TEXT_PLAIN)
        .body(body.to_string())
        .context("Failed to build email message")?;

    Ok((email, to_addr))
}

/// Send an email via SMTP.
pub async fn send_email(
    config: &EmailConfig,
    to: &str,
    subject: &str,
    body: &str,
) -> Result<String> {
    let (email, to_addr) = build_message(&config.username, to, subject, body)?;

    let creds = Credentials::new(config.username.clone(), config.password.clone());
    let mailer = AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.smtp_host)
        .context("Failed to create SMTP transport")?
        .port(config.smtp_port)
        .credentials(creds)
        .build();

    let sender = LettreMailer {
        inner: mailer,
        to: to_addr.to_string(),
    };
    sender.send_message(email).await
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EmailConfig;

    fn fake_config() -> EmailConfig {
        EmailConfig {
            imap_host: "127.0.0.1".into(),
            imap_port: 993,
            smtp_host: "localhost".into(), // valid hostname → starttls_relay builds ok
            smtp_port: 1,                  // nothing listening — instant ECONNREFUSED on send
            username: "sender@example.com".into(),
            password: "pass".into(),
        }
    }

    struct FakeMailer {
        response: String,
    }

    impl MailSender for FakeMailer {
        async fn send_message(&self, _email: Message) -> Result<String> {
            Ok(self.response.clone())
        }
    }

    // ── build_message ──────────────────────────────────────────────────────

    #[test]
    fn test_build_message_valid() {
        let (msg, to) =
            build_message("sender@example.com", "recip@example.com", "Hello", "World").unwrap();
        assert!(to.to_string().contains("recip@example.com"));
        let _ = msg;
    }

    #[test]
    fn test_build_message_invalid_from() {
        let err = build_message("not-an-email", "to@example.com", "s", "b").unwrap_err();
        assert!(err.to_string().contains("Invalid sender"), "got: {err}");
    }

    #[test]
    fn test_build_message_invalid_to() {
        let err = build_message("from@example.com", "bad-addr", "s", "b").unwrap_err();
        assert!(err.to_string().contains("Invalid recipient"), "got: {err}");
    }

    // ── MailSender (fake) ──────────────────────────────────────────────────

    #[tokio::test]
    async fn test_fake_mailer_returns_configured_response() {
        let (email, _) = build_message("a@example.com", "b@example.com", "subj", "body").unwrap();
        let mailer = FakeMailer {
            response: "Email sent to b@example.com (status: 250)".into(),
        };
        let result = mailer.send_message(email).await.unwrap();
        assert!(result.contains("250"));
        assert!(result.contains("b@example.com"));
    }

    // ── send_email (connection-refused path covers transport-building lines) ─

    #[tokio::test]
    async fn test_send_email_bad_from_address_fails_before_network() {
        // username is not a valid email → fails in build_message, never touches SMTP
        let mut cfg = fake_config();
        cfg.username = "not-an-email".into();
        let err = send_email(&cfg, "to@example.com", "s", "b")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Invalid sender"));
    }

    #[tokio::test]
    async fn test_send_email_smtp_transport_built_then_refused() {
        // Covers: Credentials::new, starttls_relay build, LettreMailer construction,
        // and LettreMailer::send_message (which hits the network and gets ECONNREFUSED).
        let cfg = fake_config();
        let err = send_email(&cfg, "to@example.com", "subject", "body")
            .await
            .unwrap_err();
        let msg = err.to_string();
        // If starttls_relay itself fails the error says "Failed to create SMTP transport".
        // If we reach send_message and SMTP fails the error says "Failed to send email via SMTP".
        // Either way we just need the transport-building lines exercised.
        assert!(
            msg.contains("SMTP"),
            "Expected an SMTP-related error, got: {msg}"
        );
    }
}
