//! Outbound email, behind a provider trait — the same shape as `storage`.
//!
//! The control plane needs to send mail for two features that don't exist yet:
//! local-account email verification, and invite emails (MAIN-7). Rather than
//! grow either of those first (and make one depend on the other), this is the
//! transport alone: a `Mailer` that knows how to put a message on the wire and
//! nothing about what the message says.
//!
//! Two backends, chosen from config at boot exactly as `ArtifactStore` is:
//!
//! - **smtp** — a real SMTP relay (dev points at Mailpit, prod at the mail
//!   host). Selected when `SMTP_HOST` is set.
//! - **capture** — records and logs what would be sent instead of sending it.
//!   The fallback when no SMTP is configured, so the stack still boots and a dev
//!   is never blocked, and the impl tests assert against.
//!
//! Sending is best-effort and one-shot: no queue, no retry, no bounce handling
//! (those are a later concern). `send` returns `Result`, so a caller can react
//! to a failure — but nothing here forces a request path to block on delivery,
//! and a failed send is logged, never a panic.

use anyhow::Result;
use async_trait::async_trait;

pub mod capture;
pub mod smtp;

#[async_trait]
pub trait Mailer: Send + Sync {
    /// Send one message. `html_body`, when present, makes the message
    /// multipart/alternative with `text_body` as the plain-text fallback.
    /// Returns `Err` on a delivery failure.
    async fn send(
        &self,
        to: &str,
        subject: &str,
        text_body: &str,
        html_body: Option<&str>,
    ) -> Result<()>;

    /// For logs and the health page: which backend, pointed where.
    fn describe(&self) -> String;
}

/// Build the mailer this instance is configured for.
///
/// Falls back to capture rather than failing to boot: a control plane that
/// won't start because SMTP is misconfigured is worse than one that starts and
/// logs the mail it couldn't send — the second can still be fixed while running.
pub fn from_config(cfg: &crate::config::Config) -> Box<dyn Mailer> {
    match cfg.smtp_host.as_deref().filter(|h| !h.is_empty()) {
        Some(_) => match smtp::SmtpMailer::from_config(cfg) {
            Ok(m) => {
                tracing::info!(mailer = %m.describe(), "email transport");
                Box::new(m)
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "SMTP is configured but unusable — capturing mail instead; nothing will be delivered"
                );
                Box::new(capture::CaptureMailer::new())
            }
        },
        None => {
            let m = capture::CaptureMailer::new();
            tracing::info!(mailer = %m.describe(), "email transport");
            Box::new(m)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn selects_capture_without_smtp_and_smtp_when_configured() {
        let mut cfg = Config::for_test();
        // No SMTP host → the capture fallback, so the stack still boots (AC-2/3).
        assert!(from_config(&cfg).describe().contains("capture"));

        // A host present → the SMTP transport (AC-2).
        cfg.smtp_host = Some("mail.example.com".into());
        cfg.smtp_tls = "none".into();
        assert!(from_config(&cfg).describe().starts_with("smtp"));

        // An empty host is treated as unset, not an error.
        cfg.smtp_host = Some(String::new());
        assert!(from_config(&cfg).describe().contains("capture"));
    }
}
