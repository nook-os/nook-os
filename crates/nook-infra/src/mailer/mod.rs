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
//! # What is queued, and what is not (MAIN-149, MAIN-410)
//!
//! This trait is the TRANSPORT: `send` puts one message on the wire and returns
//! what happened. It has no queue, no retry and no bounce handling of its own,
//! and it is not going to grow them — durability belongs to the caller.
//!
//! Callers split in two, and the split is deliberate rather than half-finished:
//!
//! - **Transactional mail — verification and invites — goes through the durable
//!   queue** as an `email.send` job (MAIN-149). A user is waiting on it, the
//!   request path must not block on SMTP, and a failure has to be retried and
//!   then dead-lettered with the real SMTP error rather than lost.
//! - **The email NOTIFICATION channel sends inline**, from
//!   `nook_control::services::notify`. `Channel::deliver`'s contract is "I
//!   attempted delivery, here is what happened" — its result is recorded onto
//!   the channel row and returned by the test button, which answers 400 with
//!   the provider's own message. A queued send could only ever answer
//!   "accepted", which is not what either caller asked. The full reasoning is
//!   on `Email` in that module.
//!
//! So "all mail goes through the queue" is NOT true, and the exception is one
//! place with one reason. `send` returning `Result` is what lets both callers
//! work: the queue handler retries on it, and the notification channel reports
//! it.

use anyhow::Result;
use async_trait::async_trait;

pub mod capture;
pub mod guard;
pub mod postmark;
pub mod smtp;

pub use guard::GuardedMailer;

/// The mail-provider names this build understands. `MAIL_PROVIDER` must be one
/// of these; the first is the default. Adding a transport adds a name here and
/// an arm in [`from_config`] — never a change to the [`Mailer`] trait.
pub const PROVIDERS: &[&str] = &["capture", "smtp", "postmark"];

/// Why a message is being sent — the axis the send guards gate on. A transport
/// ignores it (a transport just delivers); the [`GuardedMailer`] uses it to
/// decide whether a send is allowed (MAIN-52 AC-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// Verification and invite mail — a user is waiting on it. Sends whenever
    /// sending is enabled.
    Transactional,
    /// The email notification channel. Sends only when sending is enabled AND
    /// notifications are separately enabled — so turning mail on does not, by
    /// itself, start emailing notifications.
    Notification,
}

impl Category {
    pub fn as_str(self) -> &'static str {
        match self {
            Category::Transactional => "transactional",
            Category::Notification => "notification",
        }
    }

    /// Parse the wire form (see [`as_str`](Category::as_str)). Anything
    /// unrecognized is treated as `Transactional` — the safe default, since a
    /// transactional message is one a user is actively waiting on.
    pub fn parse(s: &str) -> Category {
        match s {
            "notification" => Category::Notification,
            _ => Category::Transactional,
        }
    }
}

/// The queue `work_type` for an email send (MAIN-149).
pub const EMAIL_WORK_TYPE: &str = "email.send";

/// A rendered email queued for delivery (MAIN-149). The control plane renders
/// the message and enqueues this as an `email.send` job; the worker deserializes
/// it and drives the configured mail provider. The queue treats it as opaque
/// bytes (JSON by convention). `category` is the wire form of [`Category`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EmailJob {
    pub to: String,
    pub subject: String,
    pub text_body: String,
    #[serde(default)]
    pub html_body: Option<String>,
    pub category: String,
}

impl EmailJob {
    pub fn new(
        to: impl Into<String>,
        subject: impl Into<String>,
        text_body: impl Into<String>,
        html_body: Option<String>,
        category: Category,
    ) -> Self {
        Self {
            to: to.into(),
            subject: subject.into(),
            text_body: text_body.into(),
            html_body,
            category: category.as_str().into(),
        }
    }
}

/// Whether `name` is a provider this build understands (config validates it at
/// boot so an unknown value fails loudly rather than silently dropping mail).
pub fn is_known_provider(name: &str) -> bool {
    PROVIDERS.contains(&name)
}

/// Whether a message actually left toward the recipient, or was held back —
/// with a short reason, for logs, when it was held.
///
/// A bare `Result<()>` from [`Mailer::send`] cannot tell these apart: the
/// [`GuardedMailer`] returns `Ok(())` whether it delivered or captured, so a
/// caller that logged "sent" on `Ok` was lying every time a gate (sending
/// disabled, notifications off, quota reached) held the message. `send_reporting`
/// returns this instead, so callers can log honestly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendOutcome {
    /// The message reached the transport.
    Delivered,
    /// Nothing was sent; the string is why (a static reason, for the log).
    Held(&'static str),
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
        category: Category,
    ) -> Result<()>;

    /// Like [`send`](Mailer::send), but reports whether the message was actually
    /// delivered or held back by a gate. The default assumes a plain transport
    /// delivers whenever `send` succeeds — true for capture/smtp/postmark, which
    /// have no gates of their own. [`GuardedMailer`] overrides it so its gates
    /// report [`SendOutcome::Held`] instead of a misleading "delivered".
    async fn send_reporting(
        &self,
        to: &str,
        subject: &str,
        text_body: &str,
        html_body: Option<&str>,
        category: Category,
    ) -> Result<SendOutcome> {
        self.send(to, subject, text_body, html_body, category)
            .await
            .map(|()| SendOutcome::Delivered)
    }

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
        "postmark" => match postmark::PostmarkMailer::from_config(cfg) {
            Ok(m) => {
                tracing::info!(provider = %m.describe(), "mail provider");
                Box::new(m)
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "MAIL_PROVIDER=postmark but the transport is unusable — capturing mail instead; nothing will be delivered"
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
        assert!(is_known_provider("postmark"));
        assert!(!is_known_provider("ses"));
        assert!(!is_known_provider(""));
        // The default is a real provider.
        assert!(is_known_provider(PROVIDERS[0]));
    }
}
