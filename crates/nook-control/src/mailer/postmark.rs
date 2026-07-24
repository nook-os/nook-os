//! The Postmark HTTP mail provider.
//!
//! Prod's Postmark server has SMTP disabled — only its HTTP API works — so this
//! POSTs to the send endpoint with the server token in a header, mapping a
//! [`Mailer`] send to Postmark's `{ From, To, Subject, TextBody, HtmlBody? }`
//! JSON (MAIN-52 AC-1).
//!
//! Same shape as `smtp.rs`: built once from config (a missing token fails the
//! build so `from_config` falls back to capture), and each send composes and
//! POSTs a message. Best-effort and one-shot — no queue, no retry. A non-2xx
//! response, or a 200 carrying a non-zero Postmark `ErrorCode`, is surfaced as a
//! send error and logged; never a panic, and never at startup.

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use super::{Category, Mailer};

pub struct PostmarkMailer {
    http: reqwest::Client,
    api_url: String,
    token: String,
    from: String,
    describe: String,
}

impl PostmarkMailer {
    /// Build from config. Fails (→ `from_config` falls back to capture) when the
    /// server token is absent — a knowable-at-boot reason, not a per-send one.
    pub fn from_config(cfg: &crate::config::Config) -> Result<Self> {
        let token = cfg
            .postmark_token
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .context("POSTMARK_TOKEN is required for the postmark mailer")?;
        Ok(Self::build(&cfg.postmark_api_url, token, &cfg.mail_from))
    }

    /// The build core, taking plain arguments so it is constructible without a
    /// whole `Config` (mirrors `SmtpMailer::build`). No network happens here.
    pub fn build(api_url: &str, token: &str, from: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_url: api_url.to_string(),
            token: token.to_string(),
            from: from.to_string(),
            describe: format!("postmark {api_url} from={from}"),
        }
    }

    /// Compose the Postmark request body. Pure, so the field mapping is testable
    /// without HTTP: `HtmlBody` is present only for a multipart message.
    pub fn payload(from: &str, to: &str, subject: &str, text: &str, html: Option<&str>) -> Value {
        let mut body = json!({
            "From": from,
            "To": to,
            "Subject": subject,
            "TextBody": text,
        });
        if let Some(html) = html {
            body["HtmlBody"] = json!(html);
        }
        body
    }
}

/// The header carrying the server token (a Postmark constant).
pub const TOKEN_HEADER: &str = "X-Postmark-Server-Token";

#[async_trait]
impl Mailer for PostmarkMailer {
    async fn send(
        &self,
        to: &str,
        subject: &str,
        text_body: &str,
        html_body: Option<&str>,
        // A transport just delivers; the guard has already decided category.
        _category: Category,
    ) -> Result<()> {
        let body = Self::payload(&self.from, to, subject, text_body, html_body);
        let resp = self
            .http
            .post(&self.api_url)
            .header(TOKEN_HEADER, &self.token)
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST to Postmark ({}) failed", self.api_url))?;

        let status = resp.status();
        let payload: Value = resp.json().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!(
                "Postmark returned {status}: {}",
                payload
                    .get("Message")
                    .and_then(Value::as_str)
                    .unwrap_or("no message")
            );
        }
        // Postmark can answer 200 with a non-zero ErrorCode (e.g. inactive
        // recipient); that is a failed send, not a success.
        if let Some(code) = payload.get("ErrorCode").and_then(Value::as_i64) {
            if code != 0 {
                anyhow::bail!(
                    "Postmark ErrorCode {code}: {}",
                    payload
                        .get("Message")
                        .and_then(Value::as_str)
                        .unwrap_or("no message")
                );
            }
        }
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
    fn payload_maps_the_fields_and_omits_html_when_absent() {
        let text_only = PostmarkMailer::payload(
            "NookOS <no-reply@hein.network>",
            "her@example.com",
            "Hello",
            "plain body",
            None,
        );
        assert_eq!(text_only["From"], "NookOS <no-reply@hein.network>");
        assert_eq!(text_only["To"], "her@example.com");
        assert_eq!(text_only["Subject"], "Hello");
        assert_eq!(text_only["TextBody"], "plain body");
        assert!(
            text_only.get("HtmlBody").is_none(),
            "no HtmlBody for a text-only message"
        );

        let with_html =
            PostmarkMailer::payload("a@b.com", "c@d.com", "S", "t", Some("<b>rich</b>"));
        assert_eq!(with_html["HtmlBody"], "<b>rich</b>");
        assert_eq!(with_html["TextBody"], "t");
    }

    #[test]
    fn build_requires_a_token_and_describes_where_it_points() {
        let mut cfg = crate::config::Config::for_test();
        cfg.mail_provider = "postmark".into();
        // No token → build fails so from_config falls back to capture.
        assert!(PostmarkMailer::from_config(&cfg).is_err());

        cfg.postmark_token = Some("tok-123".into());
        cfg.mail_from = "NookOS <no-reply@hein.network>".into();
        let m = PostmarkMailer::from_config(&cfg).expect("a token builds the provider");
        assert!(m
            .describe()
            .starts_with("postmark https://api.postmarkapp.com/email"));
        assert!(m.describe().contains("from=NookOS <no-reply@hein.network>"));
    }
}
