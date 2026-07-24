//! The `capture` mail provider: records (and logs) what would be sent instead
//! of sending it.
//!
//! Two jobs: it is what tests assert against, and it is the default provider —
//! so a dev without a mail server, and a fresh instance before mail is set up,
//! both boot and run rather than erroring on the first send.

use std::sync::Mutex;

use anyhow::Result;
use async_trait::async_trait;

use super::Mailer;

/// One message that would have been sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedEmail {
    pub to: String,
    pub subject: String,
    pub text_body: String,
    pub html_body: Option<String>,
}

#[derive(Default)]
pub struct CaptureMailer {
    sent: Mutex<Vec<CapturedEmail>>,
}

/// Keep memory bounded in the no-SMTP production case: this is not a mailbox,
/// just the tail of what would have gone out, enough for a test or a glance.
const MAX_KEPT: usize = 256;

impl CaptureMailer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything captured so far, oldest first. Tests read this to assert what
    /// a flow tried to send.
    pub fn sent(&self) -> Vec<CapturedEmail> {
        self.sent.lock().expect("capture lock").clone()
    }
}

#[async_trait]
impl Mailer for CaptureMailer {
    async fn send(
        &self,
        to: &str,
        subject: &str,
        text_body: &str,
        html_body: Option<&str>,
    ) -> Result<()> {
        tracing::info!(
            to,
            subject,
            "email captured by the capture provider — not delivered"
        );
        let mut sent = self.sent.lock().expect("capture lock");
        if sent.len() >= MAX_KEPT {
            sent.remove(0);
        }
        sent.push(CapturedEmail {
            to: to.to_string(),
            subject: subject.to_string(),
            text_body: text_body.to_string(),
            html_body: html_body.map(str::to_string),
        });
        Ok(())
    }

    fn describe(&self) -> String {
        "capture (mail is logged, not sent)".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn records_the_message_and_returns_ok() {
        let m = CaptureMailer::new();
        m.send("her@example.com", "Hi", "plain", Some("<b>rich</b>"))
            .await
            .expect("capture send is always Ok");
        m.send("them@example.com", "Second", "body2", None)
            .await
            .unwrap();

        let sent = m.sent();
        assert_eq!(sent.len(), 2);
        assert_eq!(
            sent[0],
            CapturedEmail {
                to: "her@example.com".into(),
                subject: "Hi".into(),
                text_body: "plain".into(),
                html_body: Some("<b>rich</b>".into()),
            }
        );
        assert_eq!(sent[1].to, "them@example.com");
        assert_eq!(sent[1].html_body, None);
    }

    #[tokio::test]
    async fn keeps_memory_bounded() {
        let m = CaptureMailer::new();
        for i in 0..(MAX_KEPT + 10) {
            m.send("x@example.com", &format!("n{i}"), "b", None)
                .await
                .unwrap();
        }
        let sent = m.sent();
        assert_eq!(
            sent.len(),
            MAX_KEPT,
            "old messages are dropped, not accumulated"
        );
        // The oldest kept is the (10)th message, not the very first.
        assert_eq!(sent.first().unwrap().subject, "n10");
    }
}
