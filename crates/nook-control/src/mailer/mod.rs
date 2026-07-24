//! Outbound email behind a **mail provider** trait — the same shape as
//! `storage`. The provider is a transport: SMTP is the first one, and a hosted
//! service (SES, Sendgrid, Postmark, …) would be another `impl Mailer` with no
//! change to this trait.
//!
//! The control plane needs to send mail for features that build on this:
//! local-account email verification, and invite emails (MAIN-7). Rather than
//! grow either first (and make one depend on the other), this is the transport
//! alone: a `Mailer` that knows how to put a message on the wire and nothing
//! about what the message says.
//!
//! The provider is chosen by an EXPLICIT name — `MAIL_PROVIDER` — not inferred
//! from whether some transport's config happens to be present. That mirrors
//! `NOOK_ARTIFACT_STORE`: which transport carries the mail is a deployment
//! decision, stated once. Providers today:
//!
//! - **capture** (default) — records and logs what would be sent instead of
//!   sending it. So the stack boots with no mail server, a dev is never blocked,
//!   and tests have something to assert against.
//! - **smtp** — a real SMTP relay (dev points at Mailpit, prod at the mail host).
//!
//! Sending is best-effort and one-shot: no queue, no retry, no bounce handling
//! (those are a later concern). `send` returns `Result`, so a caller can react
//! to a failure — but nothing here forces a request path to block on delivery,
//! and a failed send is logged, never a panic.

use anyhow::Result;
use async_trait::async_trait;

pub mod capture;
pub mod smtp;

/// The mail-provider names this build understands. `MAIL_PROVIDER` must be one
/// of these; the first is the default. Adding a transport adds a name here and
/// an arm in [`from_config`] — never a change to the [`Mailer`] trait.
pub const PROVIDERS: &[&str] = &["capture", "smtp"];

/// Whether `name` is a provider this build understands (config validates it at
/// boot so an unknown value fails loudly rather than silently dropping mail).
pub fn is_known_provider(name: &str) -> bool {
    PROVIDERS.contains(&name)
}

#[async_trait]
pub trait Mailer: Send + Sync {
    /// Send one message. `html_body`, when present, makes the message
    /// multipart/alternative with `text_body` as the plain-text fallback.
    /// Returns `Err` on a delivery failure.
    ///
    /// Nothing in this signature is SMTP-specific: a hosted-API transport
    /// implements the same method by POSTing the same fields (AC-7).
    async fn send(
        &self,
        to: &str,
        subject: &str,
        text_body: &str,
        html_body: Option<&str>,
    ) -> Result<()>;

    /// For logs and the health page: which provider, pointed where.
    fn describe(&self) -> String;
}

/// Build the mail provider this instance is configured for, chosen by name.
///
/// `capture` and an unset/validated config always succeed. `smtp` that fails to
/// build (a bad address or TLS mode) degrades to capture with a loud error
/// rather than refusing to boot: a control plane that won't start because the
/// relay is misconfigured is worse than one that starts and logs the mail it
/// couldn't send — the second can still be fixed while running. An *unknown*
/// name never reaches here; `Config::from_env` rejects it at boot.
pub fn from_config(cfg: &crate::config::Config) -> Box<dyn Mailer> {
    match cfg.mail_provider.as_str() {
        "smtp" => match smtp::SmtpMailer::from_config(cfg) {
            Ok(m) => {
                tracing::info!(provider = %m.describe(), "mail provider");
                Box::new(m)
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "MAIL_PROVIDER=smtp but the transport is unusable — capturing mail instead; nothing will be delivered"
                );
                Box::new(capture::CaptureMailer::new())
            }
        },
        // "capture", and defensively anything else (validated at boot).
        _ => {
            let m = capture::CaptureMailer::new();
            tracing::info!(provider = %m.describe(), "mail provider");
            Box::new(m)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn selects_the_provider_by_explicit_name() {
        let mut cfg = Config::for_test();
        // Default (capture) sends nothing, so the stack boots without a relay.
        assert!(from_config(&cfg).describe().contains("capture"));

        // MAIL_PROVIDER=smtp selects the SMTP transport — regardless of nothing
        // being inferred from smtp_host presence alone.
        cfg.mail_provider = "smtp".into();
        cfg.smtp_host = Some("mail.example.com".into());
        cfg.smtp_tls = "none".into();
        assert!(from_config(&cfg).describe().starts_with("smtp"));

        // A host set but the provider left at capture must NOT start sending —
        // selection is by name, not by inference (AC-8).
        cfg.mail_provider = "capture".into();
        assert!(from_config(&cfg).describe().contains("capture"));
    }

    #[test]
    fn only_known_provider_names_are_accepted() {
        assert!(is_known_provider("capture"));
        assert!(is_known_provider("smtp"));
        assert!(!is_known_provider("ses"));
        assert!(!is_known_provider(""));
        // The default is a real provider.
        assert!(is_known_provider(PROVIDERS[0]));
    }
}
