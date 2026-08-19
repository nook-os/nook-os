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

use super::{Category, Mailer, SendOutcome, Threading};

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
    ///
    /// Threading rides in `Headers`, Postmark's `[{Name, Value}]` escape hatch
    /// for the RFC 5322 fields its own schema does not name — and it is omitted
    /// entirely for a message that is not a reply, so an unthreaded send is the
    /// byte-identical request it was before threading existed.
    pub fn payload(
        from: &str,
        to: &str,
        subject: &str,
        text: &str,
        html: Option<&str>,
        threading: &Threading,
    ) -> Value {
        let mut body = json!({
            "From": from,
            "To": to,
            "Subject": subject,
            "TextBody": text,
        });
        if let Some(html) = html {
            body["HtmlBody"] = json!(html);
        }
        let headers: Vec<Value> = threading
            .in_reply_to
            .iter()
            .map(|id| json!({ "Name": "In-Reply-To", "Value": id }))
            .chain(
                threading
                    .references_header()
                    .map(|refs| json!({ "Name": "References", "Value": refs })),
            )
            .collect();
        if !headers.is_empty() {
            body["Headers"] = json!(headers);
        }
        body
    }
}

/// The header carrying the server token (a Postmark constant).
pub const TOKEN_HEADER: &str = "X-Postmark-Server-Token";

#[async_trait]
impl Mailer for PostmarkMailer {
    async fn send_threaded(
        &self,
        to: &str,
        subject: &str,
        text_body: &str,
        html_body: Option<&str>,
        // A transport just delivers; the guard has already decided category.
        _category: Category,
        threading: &Threading,
    ) -> Result<SendOutcome> {
        let body = Self::payload(&self.from, to, subject, text_body, html_body, threading);
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
        Ok(SendOutcome::Delivered)
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
        let plain = Threading::default();
        let text_only = PostmarkMailer::payload(
            "NookOS <no-reply@hein.network>",
            "her@example.com",
            "Hello",
            "plain body",
            None,
            &plain,
        );
        assert_eq!(text_only["From"], "NookOS <no-reply@hein.network>");
        assert_eq!(text_only["To"], "her@example.com");
        assert_eq!(text_only["Subject"], "Hello");
        assert_eq!(text_only["TextBody"], "plain body");
        assert!(
            text_only.get("HtmlBody").is_none(),
            "no HtmlBody for a text-only message"
        );
        assert!(
            text_only.get("Headers").is_none(),
            "a message that is not a reply carries no threading headers"
        );

        let with_html =
            PostmarkMailer::payload("a@b.com", "c@d.com", "S", "t", Some("<b>rich</b>"), &plain);
        assert_eq!(with_html["HtmlBody"], "<b>rich</b>");
        assert_eq!(with_html["TextBody"], "t");
    }

    /// A reply carries both threading headers, in Postmark's `Headers` form.
    #[test]
    fn payload_carries_the_thread_as_custom_headers() {
        let body = PostmarkMailer::payload(
            "a@b.com",
            "c@d.com",
            "Re: it 500s",
            "we reproduced it",
            None,
            &Threading {
                in_reply_to: Some("<m1@acme.example>".into()),
                references: vec!["<root@acme.example>".into(), "<m1@acme.example>".into()],
            },
        );
        assert_eq!(
            body["Headers"],
            json!([
                { "Name": "In-Reply-To", "Value": "<m1@acme.example>" },
                { "Name": "References", "Value": "<root@acme.example> <m1@acme.example>" },
            ])
        );
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
