//! Wire-level SMTP send, exercised against a real server (Mailpit in dev).
//!
//! Gated on `NOOK_MAILPIT_SMTP=host:port`, and skips otherwise — the same idiom
//! the database tests use, so CI and a laptop without a mail server both stay
//! green while the check is still runnable on demand:
//!
//! ```sh
//! docker run -d --rm -p 1025:1025 -p 8025:8025 axllent/mailpit
//! NOOK_MAILPIT_SMTP=localhost:1025 ./test.sh --host rust sends_through_smtp
//! # then see it at http://localhost:8025
//! ```

use nook_control::mailer::smtp::SmtpMailer;
use nook_control::mailer::Mailer;

#[tokio::test]
async fn sends_through_smtp_to_mailpit() {
    let Ok(addr) = std::env::var("NOOK_MAILPIT_SMTP") else {
        return; // no mail server configured — nothing to exercise
    };
    let (host, port) = addr
        .split_once(':')
        .expect("NOOK_MAILPIT_SMTP must be host:port");
    let port: u16 = port.parse().expect("port must be a number");

    let mailer = SmtpMailer::build(
        host,
        port,
        "none", // Mailpit speaks plaintext
        "NookOS <no-reply@localhost>",
        None,
        None,
    )
    .expect("Mailpit transport builds");

    mailer
        .send(
            "recipient@example.com",
            "NookOS SMTP wire check",
            "This message proves the SMTP Mailer puts bytes on the wire.",
            Some("<p>This message proves the <b>SMTP Mailer</b> puts bytes on the wire.</p>"),
        )
        .await
        .expect("the send should reach Mailpit");
}
