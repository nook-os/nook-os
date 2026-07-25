//! SMTP transport via `lettre`, over rustls (no OpenSSL, matching the tree).
//!
//! Built once from config and reused; a `send` composes a message and hands it
//! to the transport. Best-effort and one-shot — the transport does no pooling,
//! queueing, or retry.

use anyhow::{Context, Result};
use async_trait::async_trait;
use lettre::message::{header::ContentType, Mailbox, MultiPart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use super::{Category, Mailer};

pub struct SmtpMailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
    describe: String,
}

impl SmtpMailer {
    /// Build the transport from config. Fails (so `from_config` falls back to
    /// capture) if the host, address, or TLS mode is unusable — never at send
    /// time for a reason that was knowable at boot.
    pub fn from_config(cfg: &crate::config::Config) -> Result<Self> {
        let host = cfg
            .smtp_host
            .as_deref()
            .filter(|h| !h.is_empty())
            .context("SMTP_HOST is required for the smtp mailer")?;
        Self::build(
            host,
            cfg.smtp_port,
            &cfg.smtp_tls,
            &cfg.smtp_from,
            cfg.smtp_username.as_deref(),
            cfg.smtp_password.as_deref(),
        )
    }

    /// The transport-building core, taking plain arguments so it can be tested
    /// without a whole `Config`. No network happens here — it only assembles and
    /// validates the transport, which is why a bad address or TLS mode is caught
    /// at boot rather than on the first send.
    pub fn build(
        host: &str,
        port: u16,
        tls: &str,
        from: &str,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<Self> {
        // `starttls` (587) is the default; `implicit` is TLS from the first byte
        // (465); `none` is plaintext, which is what Mailpit speaks in dev.
        let builder = match tls {
            "none" => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host),
            "implicit" => AsyncSmtpTransport::<Tokio1Executor>::relay(host)
                .context("building an implicit-TLS SMTP relay")?,
            "starttls" => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host)
                .context("building a STARTTLS SMTP relay")?,
            other => anyhow::bail!("SMTP_TLS must be starttls, implicit, or none — got {other:?}"),
        }
        .port(port);

        // Mailpit and other open relays need no auth; only send credentials when
        // both are present.
        let builder = match (username, password) {
            (Some(user), Some(pass)) => {
                builder.credentials(Credentials::new(user.to_string(), pass.to_string()))
            }
            _ => builder,
        };

        let from: Mailbox = from
            .parse()
            .with_context(|| format!("SMTP_FROM is not a valid address: {from:?}"))?;

        let describe = format!("smtp {host}:{port} tls={tls} from={from}");
        Ok(Self {
            transport: builder.build(),
            from,
            describe,
        })
    }
}

#[async_trait]
impl Mailer for SmtpMailer {
    async fn send(
        &self,
        to: &str,
        subject: &str,
        text_body: &str,
        html_body: Option<&str>,
        // A transport just delivers; the category is the guard's concern.
        _category: Category,
    ) -> Result<()> {
        let recipient: Mailbox = to
            .parse()
            .with_context(|| format!("invalid recipient address: {to:?}"))?;
        let message = Message::builder()
            .from(self.from.clone())
            .to(recipient)
            .subject(subject);
        let message = match html_body {
            Some(html) => message
                .multipart(MultiPart::alternative_plain_html(
                    text_body.to_string(),
                    html.to_string(),
                ))
                .context("composing the multipart message")?,
            None => message
                .header(ContentType::TEXT_PLAIN)
                .body(text_body.to_string())
                .context("composing the message")?,
        };
        self.transport
            .send(message)
            .await
            .with_context(|| format!("SMTP send to {to} failed"))?;
        Ok(())
    }

    fn describe(&self) -> String {
        self.describe.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_mailpit_transport_and_describes_it() {
        let m = SmtpMailer::build(
            "localhost",
            1025,
            "none",
            "NookOS <no-reply@localhost>",
            None,
            None,
        )
        .expect("plaintext Mailpit config builds");
        assert!(m.describe().starts_with("smtp localhost:1025 tls=none"));
    }

    #[test]
    fn accepts_each_valid_tls_mode() {
        for tls in ["none", "starttls", "implicit"] {
            assert!(
                SmtpMailer::build("mail.example.com", 587, tls, "a@example.com", None, None)
                    .is_ok(),
                "tls={tls} should build"
            );
        }
    }

    #[test]
    fn rejects_a_bad_tls_mode_and_a_bad_from_address() {
        assert!(
            SmtpMailer::build("localhost", 587, "ssl-maybe", "a@example.com", None, None).is_err(),
            "unknown TLS mode must be refused"
        );
        assert!(
            SmtpMailer::build("localhost", 587, "none", "not an address", None, None).is_err(),
            "an unparseable from-address must be refused"
        );
    }
}
