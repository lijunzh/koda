//! SMTP client for sending emails via lettre.

use crate::config::EmailConfig;
use anyhow::{Context, Result};
use lettre::{
    AsyncSmtpTransport, AsyncTransport, Tokio1Executor,
    message::{Mailbox, MessageBuilder, header::ContentType},
    transport::smtp::authentication::Credentials,
};

/// Send an email via SMTP.
pub async fn send_email(
    config: &EmailConfig,
    to: &str,
    subject: &str,
    body: &str,
) -> Result<String> {
    let from: Mailbox = config
        .username
        .parse()
        .context("Invalid sender email address in KODA_EMAIL_USERNAME")?;
    let to_addr: Mailbox = to
        .parse()
        .context(format!("Invalid recipient email address: {to}"))?;

    let email = MessageBuilder::new()
        .from(from)
        .to(to_addr)
        .subject(subject)
        .header(ContentType::TEXT_PLAIN)
        .body(body.to_string())
        .context("Failed to build email message")?;

    let creds = Credentials::new(config.username.clone(), config.password.clone());

    let mailer = AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.smtp_host)
        .context("Failed to create SMTP transport")?
        .port(config.smtp_port)
        .credentials(creds)
        .build();

    let response = mailer
        .send(email)
        .await
        .context("Failed to send email via SMTP")?;

    Ok(format!("Email sent to {to} (status: {})", response.code()))
}
