//! Just enough IMAP to poll a mailbox (MAIN-333).
//!
//! ## Why this is not a client library
//!
//! The poller issues four commands — `LOGIN`, `SELECT`, `UID SEARCH`,
//! `UID FETCH` — and reads two kinds of response. A general IMAP client brings
//! a second async runtime shim, a second TLS stack and a second `nom`/
//! `thiserror` major into a tree that already talks to rustls directly for the
//! agent listener and the node's pinned dial. What it would NOT bring is test
//! coverage: a fake server is needed either way, because no test may reach a
//! real mailbox. So the protocol lives here, small enough to read in one
//! sitting, and [`Session`] is generic over the stream so the tests below drive
//! the same parser a real connection does over an in-memory pipe.
//!
//! ## Implicit TLS only
//!
//! Port 993, TLS before the greeting. There is deliberately no STARTTLS arm: it
//! is the variant where a downgrade is possible, the credential is what would
//! be lost, and no deployment needs it — 993 has been the ordinary spelling of
//! "IMAP" for two decades.
//!
//! ## `BODY.PEEK[]`, never `BODY[]`
//!
//! Fetching with `BODY[]` sets `\Seen`, so polling a shared support mailbox
//! would mark every message read under the humans also watching it. `PEEK` is
//! the same fetch without the side effect; the poller's own position is the UID
//! watermark, not a flag on somebody else's mail.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::error::{ApiError, ApiResult};

/// How long one whole poll may take — connect, log in, search, fetch, log out.
/// A mailbox that stops responding mid-fetch must not hold the sweep's task
/// forever, and the next tick is a better recovery than a hung one.
pub const POLL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The most messages one poll will fetch.
///
/// The watermark advances past exactly what was fetched, so a mailbox with a
/// backlog drains over successive polls rather than pulling thousands of
/// messages — and their attachments — into memory in one tick.
pub const MAX_MESSAGES_PER_POLL: usize = 50;

/// A refusal to even try: the credential carries something that would end the
/// line the command is written on. See [`quoted`].
const CONTROL_IN_CREDENTIAL: &str =
    "an IMAP host, mailbox or credential may not contain a control character";

/// Where to poll, and as whom. The password is plaintext here and nowhere else
/// — it is unsealed for the length of one poll.
#[derive(Clone)]
pub struct ImapAccount {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub mailbox: String,
}

impl std::fmt::Debug for ImapAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImapAccount")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("mailbox", &self.mailbox)
            .finish()
    }
}

/// One message, exactly as the server holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedMessage {
    pub uid: u32,
    pub raw: Vec<u8>,
}

/// What one poll saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Polled {
    /// The mailbox's UID namespace. A change means every UID the poller
    /// remembered names a different message — or nothing — so the caller must
    /// forget its watermark rather than skip past it.
    pub uid_validity: u32,
    pub messages: Vec<FetchedMessage>,
}

/// How far a previous poll got.
///
/// The two fields are ONE fact and are useless apart: a UID names a message
/// only within the namespace `uid_validity` identifies, so a `last_uid` carried
/// across a change of that number points at a different message or at none.
/// Passing them together is what lets the fetcher drop the watermark at the
/// moment `SELECT` reports the change — before it searches, rather than after
/// it has already asked the wrong question.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Watermark {
    /// `None` on the first poll, and after a reconfiguration.
    pub uid_validity: Option<u32>,
    pub last_uid: u32,
}

/// Fetching a mailbox, behind a trait so the poller can be driven without a
/// server — the fake in `email_imap`'s tests is the only other implementor.
#[async_trait]
pub trait ImapFetcher: Send + Sync {
    /// Every message the mailbox holds beyond `since`, oldest first, capped at
    /// [`MAX_MESSAGES_PER_POLL`]. A mailbox whose `UIDVALIDITY` no longer
    /// matches `since` is read from the start.
    async fn poll(&self, account: &ImapAccount, since: Watermark) -> ApiResult<Polled>;
}

/// The real one: implicit TLS to the configured host.
pub struct TlsImapFetcher;

#[async_trait]
impl ImapFetcher for TlsImapFetcher {
    async fn poll(&self, account: &ImapAccount, since: Watermark) -> ApiResult<Polled> {
        tokio::time::timeout(POLL_TIMEOUT, async {
            let stream = connect(&account.host, account.port).await?;
            Session::new(stream).poll(account, since).await
        })
        .await
        .map_err(|_| {
            ApiError::ServiceUnavailable(format!(
                "the IMAP server at {} did not finish a poll within {}s",
                account.host,
                POLL_TIMEOUT.as_secs()
            ))
        })?
    }
}

async fn connect(host: &str, port: u16) -> ApiResult<tokio_rustls::client::TlsStream<TcpStream>> {
    // rustls refuses to guess between compiled-in providers and panics where a
    // config is built rather than at startup. `main` installs one; this is the
    // belt for a path that reaches TLS without going through it (a test, a
    // future binary), and a second install is an error we ignore.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();

    let name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|_| ApiError::BadRequest(format!("{host} is not a valid host name")))?;
    let tcp = TcpStream::connect((host, port)).await.map_err(|e| {
        ApiError::ServiceUnavailable(format!(
            "could not reach the IMAP server at {host}:{port}: {e}"
        ))
    })?;
    tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await
        .map_err(|e| {
            ApiError::ServiceUnavailable(format!("TLS to the IMAP server at {host} failed: {e}"))
        })
}

/// One connection's worth of conversation.
pub struct Session<S> {
    io: BufReader<S>,
    tag: u32,
}

/// One logical server response: its text with the literals removed, and those
/// literals in the order they appeared.
///
/// Splitting them is what makes the parser trivial. A literal is arbitrary
/// bytes — a whole email, `)`s and `{`s and CRLFs included — so any parser that
/// left it inline would have to re-derive where it ended, which is the one
/// thing the `{n}` prefix already said.
#[derive(Debug, Default)]
struct Response {
    text: String,
    literals: Vec<Vec<u8>>,
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> Session<S> {
    pub fn new(stream: S) -> Self {
        Self {
            io: BufReader::new(stream),
            tag: 0,
        }
    }

    /// Log in, select the mailbox, and fetch what arrived after `since`.
    pub async fn poll(&mut self, account: &ImapAccount, since: Watermark) -> ApiResult<Polled> {
        let greeting = self.read_response().await?;
        if greeting.text.contains("* BYE") {
            return Err(ApiError::ServiceUnavailable(
                "the IMAP server closed the connection on greeting".into(),
            ));
        }
        // `* PREAUTH` means the transport already authenticated us and `LOGIN`
        // would be an error, not a formality.
        if !greeting.text.contains("* PREAUTH") {
            self.command(&format!(
                "LOGIN {} {}",
                quoted(&account.username)?,
                quoted(&account.password)?
            ))
            .await
            // The server's own words are dropped here and ONLY here. Every
            // other refusal is quoted into `email_pollers.last_error`, which a
            // tenant admin reads back — and `LOGIN` is the one command whose
            // arguments include the password, so a server that echoes what it
            // refused would put the credential there.
            .map_err(|_| {
                ApiError::BadRequest("the IMAP server rejected the poller's credentials".into())
            })?;
        }

        let selected = self
            .command(&format!("SELECT {}", quoted(&account.mailbox)?))
            .await?;
        let uid_validity = uid_validity(&selected.text).ok_or_else(|| {
            ApiError::ServiceUnavailable(format!(
                "the IMAP server reported no UIDVALIDITY for {}",
                account.mailbox
            ))
        })?;

        // The watermark is only meaningful inside the namespace it was taken
        // from. `SELECT` has just reported which namespace this is, so a
        // mismatch is answered HERE — before the search — and the mailbox is
        // read from the start. Noticing afterwards would mean one whole poll
        // asking a question about UIDs that no longer name anything.
        let since_uid = match since.uid_validity {
            Some(remembered) if remembered == uid_validity => since.last_uid,
            Some(_) => 0,
            None => 0,
        };

        let found = self
            .command(&format!("UID SEARCH UID {}:*", since_uid.saturating_add(1)))
            .await?;
        // `SEARCH UID n:*` is not the filter it reads as. RFC 3501's range
        // takes `*` to be the highest UID in the mailbox, and a range whose
        // start is above its end is normalised rather than empty — so a server
        // with nothing new answers with its LAST message instead of nothing.
        // The filter has to happen here.
        let mut uids: Vec<u32> = search_results(&found.text)
            .into_iter()
            .filter(|uid| *uid > since_uid)
            .collect();
        uids.sort_unstable();
        if uids.len() > MAX_MESSAGES_PER_POLL {
            // Said out loud, because a cap nobody is told about reads as "the
            // mailbox is up to date". The rest arrive on the next poll: the
            // watermark advances only past what was actually ingested.
            tracing::info!(
                mailbox = %account.mailbox,
                found = uids.len(),
                taking = MAX_MESSAGES_PER_POLL,
                "the mailbox holds more than one poll fetches — the remainder follows next poll"
            );
            uids.truncate(MAX_MESSAGES_PER_POLL);
        }

        let mut messages = Vec::new();
        if !uids.is_empty() {
            let set = uids
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",");
            let fetched = self
                .command(&format!("UID FETCH {set} (BODY.PEEK[])"))
                .await?;
            messages = fetched.messages();
            // Oldest first, so a run that is cut short leaves the watermark
            // behind a contiguous prefix rather than a hole.
            messages.sort_by_key(|m| m.uid);
        }

        // Best effort: the mailbox has been read, and a server that will not say
        // goodbye has not undone that.
        let _ = self.command("LOGOUT").await;

        Ok(Polled {
            uid_validity,
            messages,
        })
    }

    /// Send one command and read to its tagged completion.
    async fn command(&mut self, command: &str) -> ApiResult<Response> {
        self.tag += 1;
        let tag = format!("n{:04}", self.tag);
        // The command is NOT logged. `LOGIN` carries the password, and a rule
        // that holds for every command cannot be forgotten for one of them.
        self.io
            .write_all(format!("{tag} {command}\r\n").as_bytes())
            .await
            .map_err(io_err)?;
        self.io.flush().await.map_err(io_err)?;

        let mut collected = Response::default();
        loop {
            let response = self.read_response().await?;
            let Some(rest) = response.text.strip_prefix(&format!("{tag} ")) else {
                collected.text.push_str(&response.text);
                collected.text.push('\n');
                collected.literals.extend(response.literals);
                continue;
            };
            let rest = rest.trim();
            return match rest.split_whitespace().next() {
                Some("OK") => Ok(collected),
                _ => Err(ApiError::ServiceUnavailable(format!(
                    "the IMAP server refused a command: {rest}"
                ))),
            };
        }
    }

    /// Read one logical response: a line, plus every literal it announces and
    /// the text that follows each.
    async fn read_response(&mut self) -> ApiResult<Response> {
        let mut out = Response::default();
        loop {
            let line = self.read_line().await?;
            out.text.push_str(&String::from_utf8_lossy(&line));
            let Some(len) = trailing_literal_len(&line) else {
                return Ok(out);
            };
            let mut buf = vec![0u8; len];
            self.io.read_exact(&mut buf).await.map_err(io_err)?;
            out.literals.push(buf);
        }
    }

    /// One CRLF-terminated line, without its terminator.
    async fn read_line(&mut self) -> ApiResult<Vec<u8>> {
        let mut line = Vec::new();
        let read = self.io.read_until(b'\n', &mut line).await.map_err(io_err)?;
        if read == 0 {
            return Err(ApiError::ServiceUnavailable(
                "the IMAP server closed the connection mid-response".into(),
            ));
        }
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        Ok(line)
    }
}

impl Response {
    /// The `FETCH` responses in this reply, paired with their bodies.
    ///
    /// The UID and the literal are matched by ORDER rather than by position in
    /// the text: servers put `UID` before or after `BODY[]` as they please, and
    /// both spellings are ordinary. What is fixed is that the nth `FETCH` line
    /// **that announced a body** carries the nth literal.
    ///
    /// That qualifier is load-bearing, and `" FETCH "` alone is not enough. A
    /// server may interleave an UNSOLICITED `FETCH` into this reply — RFC 3501
    /// §7.4.2, typically `* 3 FETCH (UID 103 FLAGS (\Seen))` when another
    /// client touches a flag, which on a shared support mailbox with humans
    /// reading it is an ordinary Tuesday. It carries a UID and NO literal, so
    /// counting it would shift every body onto the wrong UID and let
    /// `poll_one`'s watermark jump past messages that were never fetched —
    /// which loses them permanently, because the watermark has passed them.
    ///
    /// Requiring `BODY[` is what excludes it. It also excludes the
    /// lower-probability case of a server returning a short body as a quoted
    /// string rather than a literal: that line announces no literal either, and
    /// dropping it loses one message from one poll (the watermark does not
    /// advance past it) instead of misattributing every message after it.
    fn messages(&self) -> Vec<FetchedMessage> {
        self.text
            .lines()
            .filter(|line| line.contains(" FETCH ") && line.contains("BODY["))
            .filter_map(uid_of)
            .zip(self.literals.iter())
            .map(|(uid, raw)| FetchedMessage {
                uid,
                raw: raw.clone(),
            })
            .collect()
    }
}

fn io_err(e: std::io::Error) -> ApiError {
    ApiError::ServiceUnavailable(format!("the IMAP connection failed: {e}"))
}

/// An IMAP quoted string, or a refusal.
///
/// The refusal is the point. A quoted string cannot carry CR or LF, so a
/// password holding one would end the `LOGIN` line early and everything after
/// it would be read as a command — the classic protocol injection, reachable
/// here by anyone who can write the poller's configuration. Escaping is not an
/// option because there is no escape for those two bytes; refusing is.
fn quoted(value: &str) -> ApiResult<String> {
    if value.chars().any(|c| c.is_control()) {
        return Err(ApiError::BadRequest(CONTROL_IN_CREDENTIAL.into()));
    }
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    Ok(format!("\"{escaped}\""))
}

/// The `{123}` a line ends with, meaning "123 bytes follow".
fn trailing_literal_len(line: &[u8]) -> Option<usize> {
    let line = std::str::from_utf8(line).ok()?;
    let inner = line.strip_suffix('}')?.rsplit_once('{')?.1;
    // `{123+}` is the non-synchronising form; a server may use it in a
    // response and the trailing `+` is not part of the count.
    inner.trim_end_matches('+').parse().ok()
}

/// `* OK [UIDVALIDITY 3857529045] …` → the number.
fn uid_validity(text: &str) -> Option<u32> {
    let after = text.split("UIDVALIDITY").nth(1)?;
    after
        .trim_start()
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

/// `* SEARCH 101 102 103` → the numbers, from however many untagged lines.
fn search_results(text: &str) -> Vec<u32> {
    text.lines()
        .filter_map(|line| line.trim().strip_prefix("* SEARCH"))
        .flat_map(|rest| rest.split_whitespace().filter_map(|n| n.parse().ok()))
        .collect()
}

/// The `UID 101` in a FETCH line.
///
/// Both halves are trimmed of punctuation before they are read, because a
/// FETCH response is parenthesised and whitespace-splitting keeps the parens
/// on the words: the very first spelling this has to handle is
/// `(UID 101 BODY[] …`, whose first word is `(UID` and not `UID`.
fn uid_of(line: &str) -> Option<u32> {
    let bare = |word: &str| {
        word.trim_matches(|c: char| !c.is_ascii_alphanumeric())
            .to_string()
    };
    let mut parts = line.split_whitespace();
    while let Some(word) = parts.next() {
        if bare(word).eq_ignore_ascii_case("UID") {
            return bare(parts.next()?).parse().ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scripted server on an in-memory pipe: it answers each command with the
    /// next canned reply, so the real [`Session`] runs its real parser against
    /// real bytes.
    async fn scripted(
        script: Vec<String>,
    ) -> (
        Session<tokio::io::DuplexStream>,
        tokio::task::JoinHandle<Vec<String>>,
    ) {
        let (client, server) = tokio::io::duplex(1 << 20);
        let handle = tokio::spawn(async move {
            let mut io = BufReader::new(server);
            let mut seen = Vec::new();
            let mut script = script.into_iter();
            // The greeting is unsolicited.
            if let Some(first) = script.next() {
                let _ = io.write_all(first.as_bytes()).await;
            }
            for reply in script {
                let mut line = String::new();
                if io.read_line(&mut line).await.unwrap_or(0) == 0 {
                    break;
                }
                seen.push(line.trim_end().to_string());
                let _ = io.write_all(reply.as_bytes()).await;
            }
            seen
        });
        (Session::new(client), handle)
    }

    /// A watermark inside the mailbox the scripted `SELECT` reports.
    fn at(last_uid: u32) -> Watermark {
        Watermark {
            uid_validity: Some(3857529045),
            last_uid,
        }
    }

    fn account() -> ImapAccount {
        ImapAccount {
            host: "imap.example".into(),
            port: 993,
            username: "support@acme.example".into(),
            password: "hunter2".into(),
            mailbox: "INBOX".into(),
        }
    }

    fn s(v: &str) -> String {
        v.to_string()
    }

    const GREETING: &str = "* OK [CAPABILITY IMAP4rev1] ready\r\n";
    const LOGGED_IN: &str = "n0001 OK LOGIN completed\r\n";
    const SELECTED: &str = "* 2 EXISTS\r\n* OK [UIDVALIDITY 3857529045] UIDs valid\r\n\
                            n0002 OK [READ-WRITE] SELECT completed\r\n";

    #[tokio::test]
    async fn a_poll_logs_in_selects_searches_and_fetches() {
        let (mut session, server) = scripted(vec![
            s(GREETING),
            s(LOGGED_IN),
            s(SELECTED),
            s("* SEARCH 101 102\r\nn0003 OK SEARCH completed\r\n"),
            s("* 1 FETCH (UID 101 BODY[] {5}\r\nfirst)\r\n\
               * 2 FETCH (UID 102 BODY[] {6}\r\nsecond)\r\n\
               n0004 OK FETCH completed\r\n"),
            s("* BYE\r\nn0005 OK LOGOUT completed\r\n"),
        ])
        .await;

        let polled = session.poll(&account(), at(100)).await.expect("poll");
        assert_eq!(polled.uid_validity, 3857529045);
        assert_eq!(
            polled.messages,
            vec![
                FetchedMessage {
                    uid: 101,
                    raw: b"first".to_vec()
                },
                FetchedMessage {
                    uid: 102,
                    raw: b"second".to_vec()
                },
            ]
        );

        let sent = server.await.expect("server");
        assert_eq!(sent[0], r#"n0001 LOGIN "support@acme.example" "hunter2""#);
        assert_eq!(sent[1], r#"n0002 SELECT "INBOX""#);
        assert_eq!(sent[2], "n0003 UID SEARCH UID 101:*");
        assert_eq!(
            sent[3], "n0004 UID FETCH 101,102 (BODY.PEEK[])",
            "PEEK, or polling would mark a shared mailbox read"
        );
    }

    /// A literal is arbitrary bytes: a whole message carries the CRLFs, braces
    /// and parens that would end the response if the reader were line-based.
    #[tokio::test]
    async fn a_message_body_may_contain_anything() {
        let body = "Subject: hi\r\n\r\nline one\r\n) {9} not a literal\r\n";
        let fetch = format!(
            "* 1 FETCH (UID 7 BODY[] {{{}}}\r\n{body})\r\nn0004 OK FETCH completed\r\n",
            body.len()
        );
        let (mut session, _server) = scripted(vec![
            s(GREETING),
            s(LOGGED_IN),
            s(SELECTED),
            s("* SEARCH 7\r\nn0003 OK SEARCH completed\r\n"),
            fetch,
            s("n0005 OK LOGOUT completed\r\n"),
        ])
        .await;

        let polled = session.poll(&account(), at(0)).await.expect("poll");
        assert_eq!(polled.messages.len(), 1);
        assert_eq!(polled.messages[0].raw, body.as_bytes());
    }

    /// `UID SEARCH UID n:*` answers with the mailbox's last message when
    /// nothing is above `n` — a range is normalised, not emptied. Without the
    /// client-side filter every poll would re-fetch the newest message.
    #[tokio::test]
    async fn a_search_that_answers_with_an_old_uid_fetches_nothing() {
        let (mut session, server) = scripted(vec![
            s(GREETING),
            s(LOGGED_IN),
            s(SELECTED),
            s("* SEARCH 100\r\nn0003 OK SEARCH completed\r\n"),
            s("n0004 OK LOGOUT completed\r\n"),
        ])
        .await;

        let polled = session.poll(&account(), at(100)).await.expect("poll");
        assert!(polled.messages.is_empty());
        let sent = server.await.expect("server");
        assert_eq!(sent[3], "n0004 LOGOUT", "no FETCH was issued at all");
    }

    /// A server may interleave an unsolicited flag-only `FETCH` into the reply
    /// when another client touches a message — which on a SHARED support
    /// mailbox is ordinary. It carries a UID and no literal, so counting it
    /// would slide every body onto the wrong UID and let the watermark jump
    /// past messages that were never fetched.
    #[tokio::test]
    async fn an_interleaved_flag_update_does_not_steal_a_body() {
        let (mut session, _server) = scripted(vec![
            s(GREETING),
            s(LOGGED_IN),
            s(SELECTED),
            s("* SEARCH 101 102\r\nn0003 OK SEARCH completed\r\n"),
            // The flag-only response sits BETWEEN the two bodies, which is the
            // placement that misattributes both if it is counted.
            s("* 1 FETCH (UID 101 BODY[] {5}\r\nfirst)\r\n\
               * 3 FETCH (UID 103 FLAGS (\\Seen))\r\n\
               * 2 FETCH (UID 102 BODY[] {6}\r\nsecond)\r\n\
               n0004 OK FETCH completed\r\n"),
            s("n0005 OK LOGOUT completed\r\n"),
        ])
        .await;

        let polled = session.poll(&account(), at(100)).await.expect("poll");
        assert_eq!(
            polled.messages,
            vec![
                FetchedMessage {
                    uid: 101,
                    raw: b"first".to_vec()
                },
                FetchedMessage {
                    uid: 102,
                    raw: b"second".to_vec()
                },
            ],
            "the flag-only FETCH must not consume a literal slot"
        );
    }

    #[tokio::test]
    async fn a_refused_login_is_unauthorized() {
        let (mut session, _server) = scripted(vec![
            s(GREETING),
            s("n0001 NO [AUTHENTICATIONFAILED] Invalid credentials\r\n"),
        ])
        .await;
        let err = session.poll(&account(), at(0)).await.expect_err("refused");
        assert!(
            matches!(&err, ApiError::BadRequest(m) if m.contains("rejected the poller's credentials")),
            "{err}"
        );
        assert!(
            !err.to_string().contains("Invalid credentials"),
            "LOGIN's own refusal is never quoted onward: {err}"
        );
    }

    /// The whole reason [`quoted`] returns a Result: a newline in the password
    /// would end the LOGIN line and turn everything after it into commands.
    #[test]
    fn a_credential_carrying_a_newline_is_refused_rather_than_escaped() {
        assert!(quoted("hunter2\r\nn0001 DELETE \"INBOX\"").is_err());
        assert!(quoted("a\tb").is_err());
        assert_eq!(quoted(r#"pa"ss\word"#).unwrap(), r#""pa\"ss\\word""#);
    }

    #[test]
    fn a_literal_length_is_read_off_the_end_of_the_line() {
        assert_eq!(
            trailing_literal_len(b"* 1 FETCH (UID 1 BODY[] {42}"),
            Some(42)
        );
        assert_eq!(
            trailing_literal_len(b"* 1 FETCH (UID 1 BODY[] {42+}"),
            Some(42)
        );
        assert_eq!(trailing_literal_len(b"n0001 OK done"), None);
    }

    /// A FETCH response is parenthesised, so the word carrying the key is
    /// `(UID` as often as it is `UID`, and the value may be `9)`.
    #[test]
    fn the_uid_is_found_on_either_side_of_the_body() {
        assert_eq!(uid_of("* 1 FETCH (UID 101 BODY[] {5}"), Some(101));
        assert_eq!(uid_of("* 1 FETCH (BODY[] {5}"), None);
        assert_eq!(uid_of("* 1 FETCH (FLAGS (\\Seen) UID 9)"), Some(9));
        assert_eq!(uid_of("* 1 FETCH (BODY[] {5} UID 12)"), Some(12));
    }
}
