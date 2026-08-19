//! The `capture` mail provider: records (and logs) what would be sent instead
//! of sending it.
//!
//! Two jobs: it is what tests assert against, and it is the default provider —
//! so a dev without a mail server, and a fresh instance before mail is set up,
//! both boot and run rather than erroring on the first send.

use std::sync::Mutex;

use anyhow::Result;
use async_trait::async_trait;

use super::{Category, Mailer, SendOutcome, Threading};

/// One message that would have been sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedEmail {
    pub to: String,
    pub subject: String,
    pub text_body: String,
    pub html_body: Option<String>,
    /// The thread it would have joined. Captured rather than dropped because
    /// "the reply was threaded" is a claim a test has to be able to check, and
    /// this is the transport every test runs against.
    pub threading: Threading,
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
    async fn send_threaded(
        &self,
        to: &str,
        subject: &str,
        text_body: &str,
        html_body: Option<&str>,
        category: Category,
        threading: &Threading,
    ) -> Result<SendOutcome> {
        tracing::info!(
            to,
            subject,
            category = category.as_str(),
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
            threading: threading.clone(),
        });
        // `Delivered` even though nothing left the process: this IS the
        // transport when it is selected, and the gate that decides a message
        // was held is the guard's, not a provider's.
        Ok(SendOutcome::Delivered)
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
        m.send(
            "her@example.com",
            "Hi",
            "plain",
            Some("<b>rich</b>"),
            Category::Transactional,
        )
        .await
        .expect("capture send is always Ok");
        m.send(
            "them@example.com",
            "Second",
            "body2",
            None,
            Category::Notification,
        )
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
                threading: Threading::default(),
            }
        );
        assert_eq!(sent[1].to, "them@example.com");
        assert_eq!(sent[1].html_body, None);
    }

    #[tokio::test]
    async fn keeps_memory_bounded() {
        let m = CaptureMailer::new();
        for i in 0..(MAX_KEPT + 10) {
            m.send(
                "x@example.com",
                &format!("n{i}"),
                "b",
                None,
                Category::Transactional,
            )
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

    /// What a threaded send records, so a test asserting "the reply joined the
    /// customer's thread" has something to assert against.
    #[tokio::test]
    async fn records_the_thread_a_message_would_have_joined() {
        let m = CaptureMailer::new();
        let thread = Threading {
            in_reply_to: Some("<a@b>".into()),
            references: vec!["<root@b>".into(), "<a@b>".into()],
        };
        m.send_threaded(
            "her@example.com",
            "Re: it 500s",
            "we reproduced it",
            None,
            Category::Transactional,
            &thread,
        )
        .await
        .unwrap();
        assert_eq!(m.sent()[0].threading, thread);
        assert_eq!(
            thread.references_header().as_deref(),
            Some("<root@b> <a@b>")
        );
    }
}
