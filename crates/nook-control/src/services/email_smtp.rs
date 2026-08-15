//! The direct SMTP receiver: an inbound-email source that is a socket
//! (MAIN-334).
//!
//! This file is transport and nothing else. It speaks enough ESMTP to take one
//! message off the wire, and then hands the raw bytes to
//! [`email_inbound::receive_authenticated`] — the same gate, the same encryption
//! and the same card the provider webhook goes through. Nothing about what
//! happens to a message lives here.
//!
//! ## It is OFF unless an operator names an address
//!
//! Every other source is reached through a port the control plane was already
//! listening on. This one opens a new one, so it is the only part of the system
//! whose attack surface an operator has not already accepted. `EMAIL_SMTP_LISTEN`
//! unset — the shipped default — means [`bind`] returns `None` and no socket is
//! created (AC-3).
//!
//! ## Why it demands `AUTH`, and refuses to boot without a credential
//!
//! The gate downstream decides whether to act on a message by comparing the
//! **envelope sender** against a tenant's allow-list. Over plain SMTP that
//! address is whatever the peer typed. So an unauthenticated listener would let
//! anyone who can reach the port file a sealed card as a tenant's support staff
//! — the allow-list would be checking a string the attacker chose. ESMTP `AUTH`
//! against the deployment's relay credential is the SMTP-shaped equivalent of
//! the webhook's HMAC: it authenticates *the relay*, not the sender, and it is
//! what everything after it is entitled to assume. Enabling the listener with no
//! credential configured is therefore a boot failure, not a warning — the same
//! judgement the agent listener makes about plaintext in production.
//!
//! The sender is a second, independent question, and the answer must come from
//! something that can actually check: the front MTA's
//! `Authentication-Results: … spf=pass`. [`email_inbound::SmtpSource`] enforces
//! that and explains why the topmost header is the only one it reads.
//!
//! ## TLS
//!
//! `EMAIL_SMTP_TLS_CERT` + `EMAIL_SMTP_TLS_KEY` wrap the socket before the
//! greeting — implicit TLS, the shape port 465 uses — because the credential
//! above travels in the clear otherwise. STARTTLS is deliberately not offered:
//! it is an extra state machine whose whole purpose is being *optional*, and an
//! optional upgrade is one a stripping proxy removes. Unset, the listener is
//! plaintext and belongs only on a trusted network behind an MTA; it says so
//! loudly at boot, and refuses outright in production on a non-loopback address.
//!
//! ## Abuse
//!
//! Everything an unauthenticated peer can make this process spend is bounded:
//! concurrent connections, the length of a command line, how long a read may
//! block, **how long the TLS handshake may take**, how long the whole session
//! may last, how many transactions and how many bad commands one connection
//! gets, recipients per transaction, and the message size (advertised as ESMTP
//! `SIZE` and enforced as `DATA` is read, so an oversized message is refused
//! while it arrives rather than after). The constants below are the whole list.
//!
//! The handshake is called out because it is the one step that happens with a
//! connection slot already held and none of `session`'s own timeouts running
//! yet — so it needs a deadline of its own or the claim above is false exactly
//! where it matters most.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine as _;
use hmac::{Hmac, Mac};
use nook_infra::Config;
use sha2::Sha256;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use crate::error::ApiError;
use crate::services::email_inbound::{self as inbound, Disposition, SmtpSource};
use crate::state::AppState;

/// What the greeting and the `EHLO` reply call this server. A constant rather
/// than a setting: no peer routes on it, and the operator's own relay is the
/// only thing that ever reads it.
const SERVER_NAME: &str = "nook";

/// Sessions served at once. A relay opens one connection, or a handful; this is
/// sized to be generous for that and stingy for anything else.
const MAX_CONNECTIONS: usize = 32;
/// Longest command line accepted. RFC 5321 caps a command at 512 octets; four
/// times that tolerates a long `AUTH PLAIN` initial response without giving a
/// peer an unbounded buffer.
const MAX_COMMAND_BYTES: usize = 2048;
/// Recipients per transaction. RFC 5321 requires at least 100 to be accepted;
/// this is a receiver for one tenant's support address, and a delivery naming
/// more than a handful is not that.
const MAX_RECIPIENTS: usize = 16;
/// Messages one connection may deliver before it is asked to reconnect.
/// `pub` so the test that proves a refused message still spends from this
/// budget counts to the same number this does.
pub const MAX_TRANSACTIONS: usize = 16;
/// Syntax errors one connection may make before it is hung up on.
const MAX_BAD_COMMANDS: usize = 8;
/// How long a single command may take to arrive.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the whole of `DATA` may take.
const DATA_TIMEOUT: Duration = Duration::from_secs(300);
/// How long a TLS handshake may take. Its own constant because it is the ONE
/// thing a peer holds before `session` — and therefore before any of the
/// timeouts inside it — starts running. Without it, thirty-two peers that open
/// TCP and then send nothing hold every slot for ever, no credential required.
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long one connection may live, whatever it is doing. The per-command
/// timeout is not enough on its own: a peer sending `NOOP` every fifty seconds
/// resets it forever and holds a slot, and thirty-two such peers are the whole
/// receiver.
const SESSION_TIMEOUT: Duration = Duration::from_secs(1800);

/// A bound, configured receiver. Held between [`bind`] and [`serve`] for the
/// reason the control plane binds both its HTTP doors before serving either: a
/// boot that is going to fail should fail with every door still shut.
pub struct Receiver {
    listener: TcpListener,
    tls: Option<TlsAcceptor>,
    username: String,
    password: String,
    /// Which MTA's verdict counts, when the operator has named one.
    authserv_id: Option<Arc<str>>,
    max_bytes: usize,
}

impl Receiver {
    /// Where it actually landed — the point of asking is `:0`, which is how a
    /// test gets a port nothing else on the machine is using.
    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }
}

/// Bind the receiver, or `Ok(None)` when this deployment does not run one.
///
/// Every misconfiguration is fatal here rather than at the first delivery: an
/// operator finds out at boot, which is when they are looking.
pub async fn bind(cfg: &Config) -> Result<Option<Receiver>> {
    let Some(addr) = cfg
        .email_smtp_listen
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
    else {
        return Ok(None);
    };

    let (username, password) = match (
        cfg.email_smtp_username.as_deref().filter(|s| !s.is_empty()),
        cfg.email_smtp_password.as_deref().filter(|s| !s.is_empty()),
    ) {
        (Some(u), Some(p)) => (u.to_string(), p.to_string()),
        _ => anyhow::bail!(
            "the inbound SMTP receiver on {addr} would accept unauthenticated \
             transactions: set EMAIL_SMTP_USERNAME and EMAIL_SMTP_PASSWORD, which is \
             what the support-staff allow-list is entitled to assume happened"
        ),
    };

    let tls = match (
        cfg.email_smtp_tls_cert.as_deref().filter(|s| !s.is_empty()),
        cfg.email_smtp_tls_key.as_deref().filter(|s| !s.is_empty()),
    ) {
        (Some(cert), Some(key)) => Some(acceptor(cert, key)?),
        (None, None) => {
            // The credential above is sent as base64 — encoding, not
            // encryption. In production that must not cross anything but a
            // loopback interface, and "it is only on the internal network" is
            // exactly the assumption this refuses to take on the operator's
            // behalf.
            if cfg.is_production() && !is_loopback(addr) {
                anyhow::bail!(
                    "the inbound SMTP receiver on {addr} would be PLAINTEXT and is not \
                     bound to loopback: set EMAIL_SMTP_TLS_CERT and EMAIL_SMTP_TLS_KEY, \
                     or bind it to 127.0.0.1 behind an MTA on the same host"
                );
            }
            tracing::warn!(
                bind = %addr,
                "inbound SMTP receiver is PLAINTEXT — its AUTH credential crosses the \
                 wire in the clear; set EMAIL_SMTP_TLS_CERT and EMAIL_SMTP_TLS_KEY"
            );
            None
        }
        _ => anyhow::bail!("EMAIL_SMTP_TLS_CERT and EMAIL_SMTP_TLS_KEY must be set together"),
    };

    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot bind the inbound SMTP port {addr}"))?;
    tracing::info!(
        bind = %addr,
        tls = tls.is_some(),
        max_bytes = cfg.email_smtp_max_bytes,
        "inbound SMTP receiver listening"
    );
    Ok(Some(Receiver {
        listener,
        tls,
        username,
        password,
        authserv_id: cfg
            .email_smtp_authserv_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(Arc::from),
        max_bytes: cfg.email_smtp_max_bytes,
    }))
}

/// Accept until `shutdown` resolves. One task per connection, capped.
pub async fn serve(receiver: Receiver, state: AppState, shutdown: impl Future<Output = ()>) {
    let Receiver {
        listener,
        tls,
        username,
        password,
        authserv_id,
        max_bytes,
    } = receiver;
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    let credential = Arc::new(Credential { username, password });
    tokio::pin!(shutdown);

    loop {
        let accepted = tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => accepted,
        };
        let (stream, peer) = match accepted {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(error = %e, "inbound SMTP accept failed");
                continue;
            }
        };
        // `try_acquire_owned`, not `acquire`: a receiver at its connection cap
        // should refuse the next peer immediately with the code that means
        // "come back later", not hold the accept loop and let a flood queue up
        // behind it.
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            tracing::warn!(%peer, "inbound SMTP connection refused — at the connection cap");
            let mut stream = stream;
            let _ = stream
                .write_all(b"421 4.7.0 Too many connections, try again later\r\n")
                .await;
            continue;
        };

        let state = state.clone();
        let tls = tls.clone();
        let credential = credential.clone();
        let sender_check = authserv_id.clone();
        tokio::spawn(async move {
            let _slot = slot;
            let outcome = match tls {
                // The slot is already held at this point, so the handshake is
                // inside the budget and must have a deadline of its own.
                Some(tls) => {
                    match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, tls.accept(stream)).await {
                        Ok(Ok(stream)) => {
                            session(
                                stream,
                                &state,
                                &credential,
                                sender_check.as_deref(),
                                max_bytes,
                            )
                            .await
                        }
                        Ok(Err(e)) => {
                            tracing::debug!(%peer, error = %e, "inbound SMTP TLS handshake failed");
                            return;
                        }
                        Err(_) => {
                            tracing::warn!(
                                %peer,
                                "inbound SMTP TLS handshake timed out — the peer opened a \
                                 connection and never negotiated"
                            );
                            return;
                        }
                    }
                }
                None => {
                    session(
                        stream,
                        &state,
                        &credential,
                        sender_check.as_deref(),
                        max_bytes,
                    )
                    .await
                }
            };
            if let Err(e) = outcome {
                tracing::debug!(%peer, error = %e, "inbound SMTP session ended");
            }
        });
    }
    tracing::info!("inbound SMTP receiver stopped");
}

/// The relay credential, and the only comparison anything makes against it.
struct Credential {
    username: String,
    password: String,
}

impl Credential {
    fn matches(&self, username: &str, password: &str) -> bool {
        // Both halves compared, and neither short-circuits: a byte-by-byte
        // `==` on a secret a peer may retry indefinitely is a timing oracle,
        // which is the reasoning `tunnels::open` records for using HMAC
        // verification instead of comparing tags. `&`, not `&&`.
        secret_eq(&self.username, username) & secret_eq(&self.password, password)
    }
}

/// Constant-time string equality, built from the HMAC already in the tree.
///
/// Both sides are tagged under their own value as the key and the tags are
/// compared by `verify_slice`, which is constant-time. It leaks nothing about
/// either input, including its length.
fn secret_eq(expected: &str, presented: &str) -> bool {
    let tag = |s: &str| {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(s.as_bytes()).expect("hmac accepts any key length");
        mac.update(b"nook-smtp-credential");
        mac
    };
    tag(presented)
        .verify_slice(&tag(expected).finalize().into_bytes())
        .is_ok()
}

/// Is this bind address one only the local host can reach?
fn is_loopback(addr: &str) -> bool {
    let host = match addr.rsplit_once(':') {
        Some((host, _)) => host.trim_matches(['[', ']']),
        None => addr,
    };
    host.parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
        || host.eq_ignore_ascii_case("localhost")
}

/// Where one connection is in the SMTP conversation.
#[derive(Default)]
struct Transaction {
    from: Option<String>,
    to: Vec<String>,
}

/// One connection, from the greeting to the disconnect.
async fn session<S>(
    stream: S,
    state: &AppState,
    credential: &Credential,
    authserv_id: Option<&str>,
    max_bytes: usize,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);

    say(
        &mut writer,
        &format!("220 {SERVER_NAME} NookOS inbound SMTP"),
    )
    .await?;

    let mut greeted = false;
    let mut authenticated = false;
    let mut tx = Transaction::default();
    let mut delivered = 0usize;
    let mut bad = 0usize;
    let deadline = tokio::time::Instant::now() + SESSION_TIMEOUT;

    loop {
        if tokio::time::Instant::now() >= deadline {
            say(&mut writer, "421 4.4.2 Session too long").await?;
            return Ok(());
        }
        let Some(line) = read_command(&mut reader).await? else {
            return Ok(());
        };
        let line = line.trim_end_matches(['\r', '\n']).to_string();
        let (verb, rest) = match line.split_once(' ') {
            Some((v, r)) => (v.to_ascii_uppercase(), r.trim()),
            None => (line.to_ascii_uppercase(), ""),
        };

        match verb.as_str() {
            "QUIT" => {
                say(&mut writer, "221 2.0.0 Bye").await?;
                return Ok(());
            }
            "NOOP" => say(&mut writer, "250 2.0.0 Ok").await?,
            "RSET" => {
                tx = Transaction::default();
                say(&mut writer, "250 2.0.0 Ok").await?;
            }
            // 252 is "cannot verify, but will attempt delivery": the honest
            // answer, and the one that does not turn the receiver into a way to
            // ask which addresses a deployment serves.
            "VRFY" | "EXPN" => say(&mut writer, "252 2.5.2 Cannot verify").await?,
            "HELO" => {
                greeted = true;
                tx = Transaction::default();
                say(&mut writer, &format!("250 {SERVER_NAME}")).await?;
            }
            "EHLO" => {
                greeted = true;
                tx = Transaction::default();
                say(&mut writer, &format!("250-{SERVER_NAME}")).await?;
                say(&mut writer, &format!("250-SIZE {max_bytes}")).await?;
                say(&mut writer, "250-8BITMIME").await?;
                say(&mut writer, "250-SMTPUTF8").await?;
                say(&mut writer, "250 AUTH PLAIN LOGIN").await?;
            }
            // Not offered, so a peer asking for it is told plainly rather than
            // left to assume the session was upgraded.
            "STARTTLS" => {
                say(
                    &mut writer,
                    "502 5.5.1 STARTTLS not available — this receiver uses implicit TLS",
                )
                .await?
            }
            "AUTH" if !greeted => say(&mut writer, "503 5.5.1 EHLO first").await?,
            "AUTH" if authenticated => say(&mut writer, "503 5.5.1 Already authenticated").await?,
            "AUTH" => {
                match authenticate(&mut reader, &mut writer, rest, credential).await? {
                    Some(true) => {
                        authenticated = true;
                        say(&mut writer, "235 2.7.0 Authentication successful").await?;
                    }
                    Some(false) => {
                        // The count is shared with syntax errors so a peer
                        // guessing the password is disconnected on the same
                        // budget as one talking nonsense.
                        bad += 1;
                        say(&mut writer, "535 5.7.8 Authentication credentials invalid").await?;
                    }
                    None => {
                        bad += 1;
                        say(&mut writer, "501 5.5.4 Cannot decode AUTH exchange").await?;
                    }
                }
            }
            "MAIL" if !authenticated => {
                say(&mut writer, "530 5.7.0 Authentication required").await?
            }
            // Refused rather than silently reset: a relay that sends this has
            // lost track of where it is, and quietly discarding the recipients
            // it already gave would deliver the next message to nobody.
            "MAIL" if tx.from.is_some() => {
                say(&mut writer, "503 5.5.1 Nested MAIL command").await?
            }
            "MAIL" => match address_of("FROM", rest) {
                Some(from) => {
                    tx = Transaction {
                        from: Some(from),
                        to: Vec::new(),
                    };
                    say(&mut writer, "250 2.1.0 Ok").await?;
                }
                None => {
                    bad += 1;
                    say(&mut writer, "501 5.5.4 Syntax: MAIL FROM:<address>").await?;
                }
            },
            "RCPT" if !authenticated => {
                say(&mut writer, "530 5.7.0 Authentication required").await?
            }
            "RCPT" if tx.from.is_none() => say(&mut writer, "503 5.5.1 MAIL first").await?,
            "RCPT" if tx.to.len() >= MAX_RECIPIENTS => {
                say(&mut writer, "452 4.5.3 Too many recipients").await?
            }
            "RCPT" => match address_of("TO", rest) {
                Some(to) => {
                    tx.to.push(to);
                    say(&mut writer, "250 2.1.5 Ok").await?;
                }
                None => {
                    bad += 1;
                    say(&mut writer, "501 5.5.4 Syntax: RCPT TO:<address>").await?;
                }
            },
            "DATA" if !authenticated => {
                say(&mut writer, "530 5.7.0 Authentication required").await?
            }
            "DATA" if tx.from.is_none() || tx.to.is_empty() => {
                say(&mut writer, "503 5.5.1 MAIL and RCPT first").await?
            }
            "DATA" => {
                say(&mut writer, "354 End data with <CR><LF>.<CR><LF>").await?;
                let read =
                    tokio::time::timeout(DATA_TIMEOUT, read_message(&mut reader, max_bytes)).await;
                let taken = std::mem::take(&mut tx);
                let reply = match read {
                    Ok(Ok(Some(raw))) => deliver(state, taken, raw, authserv_id).await,
                    // `read_message` drains to the terminating dot even when it
                    // refuses the content, so the peer is waiting for this
                    // reply and the session goes on.
                    Ok(Ok(None)) => "552 5.3.4 Message too large".to_string(),
                    Ok(Err(e)) => return Err(e),
                    Err(_) => {
                        say(&mut writer, "421 4.4.2 Timed out reading the message").await?;
                        return Ok(());
                    }
                };
                say(&mut writer, &reply).await?;
                // Counted whatever the verdict was. A refused 25 MiB upload
                // cost exactly as much to read as an accepted one, so it must
                // spend from the same budget — otherwise `MAX_TRANSACTIONS`
                // bounds only the messages a peer got right.
                delivered += 1;
                if delivered >= MAX_TRANSACTIONS {
                    say(&mut writer, "421 4.7.0 Too many messages on one connection").await?;
                    return Ok(());
                }
            }
            _ => {
                bad += 1;
                say(&mut writer, "500 5.5.2 Command not recognized").await?;
            }
        }

        if bad >= MAX_BAD_COMMANDS {
            say(&mut writer, "421 4.7.0 Too many errors").await?;
            return Ok(());
        }
    }
}

/// Run one accepted transaction through the shared pipeline and turn the
/// verdict into an SMTP reply.
///
/// **A drop answers what an accept answers, byte for byte** — the same rule the
/// webhook's 202 follows, for the same reason: a distinguishable reply turns
/// the receiver into a way to ask "does this deployment serve that address" and
/// "is that person support staff", and a relay must not retry a message we have
/// decided not to act on. Byte for byte and not merely the same code, because
/// the reply text reaches the relay's mail log, which is a far more widely read
/// place than this process's own. Only a *malformed* message earns a permanent
/// refusal, and anything internal earns a temporary one so the relay retries.
async fn deliver(
    state: &AppState,
    tx: Transaction,
    raw: Vec<u8>,
    authserv_id: Option<&str>,
) -> String {
    let source = SmtpSource {
        envelope_from: tx.from.unwrap_or_default(),
        envelope_to: tx.to,
        authserv_id: authserv_id.map(str::to_string),
    };
    match inbound::receive_authenticated(state, &source, &raw, inbound::Routing::ByRecipient).await
    {
        // Byte-identical, not merely the same code. The key used to be named
        // here, which made the reply itself the oracle the paragraph above says
        // does not exist — "filed as NOOK-42" separates routed-and-allow-listed
        // from everything else, in a line that ends up in the relay's mail log.
        // What was filed is in this process's own log, at INFO, where an
        // operator can read it and a peer cannot.
        Ok(Disposition::Filed { .. }) | Ok(Disposition::Dropped(_)) => "250 2.0.0 Ok".into(),
        Err(ApiError::BadRequest(e)) => format!("550 5.6.0 {}", one_line(&e)),
        Err(e) => {
            tracing::error!(error = %e, "inbound SMTP delivery could not be filed");
            "451 4.3.0 Cannot file this message right now, try again later".into()
        }
    }
}

/// An SMTP reply is one line, so anything interpolated into one must be.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `AUTH` in both spellings a relay uses. `Some(bool)` is a verdict,
/// `None` an exchange that did not decode.
async fn authenticate<R, W>(
    reader: &mut R,
    writer: &mut W,
    rest: &str,
    credential: &Credential,
) -> std::io::Result<Option<bool>>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (mechanism, initial) = match rest.split_once(' ') {
        Some((m, i)) => (m.to_ascii_uppercase(), i.trim().to_string()),
        None => (rest.to_ascii_uppercase(), String::new()),
    };
    match mechanism.as_str() {
        "PLAIN" => {
            let blob = if initial.is_empty() {
                say(writer, "334 ").await?;
                match read_command(reader).await? {
                    Some(l) => l.trim().to_string(),
                    None => return Ok(None),
                }
            } else {
                initial
            };
            // `authzid \0 authcid \0 password` (RFC 4616). The authorization
            // identity is empty in every relay's client and is not read here:
            // there is exactly one identity this receiver knows.
            let Some(decoded) = decode_b64(&blob) else {
                return Ok(None);
            };
            let mut fields = decoded.split(|b| *b == 0);
            let (Some(_), Some(user), Some(pass), None) =
                (fields.next(), fields.next(), fields.next(), fields.next())
            else {
                return Ok(None);
            };
            let (Ok(user), Ok(pass)) = (std::str::from_utf8(user), std::str::from_utf8(pass))
            else {
                return Ok(None);
            };
            Ok(Some(credential.matches(user, pass)))
        }
        "LOGIN" => {
            let user = if initial.is_empty() {
                say(writer, "334 VXNlcm5hbWU6").await?;
                match read_command(reader).await? {
                    Some(l) => l.trim().to_string(),
                    None => return Ok(None),
                }
            } else {
                initial
            };
            say(writer, "334 UGFzc3dvcmQ6").await?;
            let Some(pass) = read_command(reader).await? else {
                return Ok(None);
            };
            let (Some(user), Some(pass)) = (decode_b64(&user), decode_b64(pass.trim())) else {
                return Ok(None);
            };
            let (Ok(user), Ok(pass)) = (std::str::from_utf8(&user), std::str::from_utf8(&pass))
            else {
                return Ok(None);
            };
            Ok(Some(credential.matches(user, pass)))
        }
        _ => Ok(None),
    }
}

fn decode_b64(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .ok()
}

/// `FROM:<a@b> SIZE=42` → `a@b`. The angle brackets are required by RFC 5321
/// and the bare form is accepted anyway, because relays send it.
///
/// `MAIL FROM:<>` — the null sender a bounce carries — parses to an empty
/// address, which the pipeline then refuses: this receiver files reports from
/// people, and a bounce is not one.
fn address_of(keyword: &str, rest: &str) -> Option<String> {
    let (name, value) = rest.split_once(':')?;
    if !name.trim().eq_ignore_ascii_case(keyword) {
        return None;
    }
    let value = value.trim();
    let address = match value.split_once('>') {
        Some((head, _)) => head.trim_start().strip_prefix('<')?.to_string(),
        // No brackets: the address runs to the first ESMTP parameter.
        None => value
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string(),
    };
    Some(address.trim().to_string())
}

/// One command line, or `None` at end of stream. Bounded in both length and
/// time, because the peer chooses both.
async fn read_command<R: AsyncBufRead + Unpin>(reader: &mut R) -> std::io::Result<Option<String>> {
    match tokio::time::timeout(COMMAND_TIMEOUT, read_line(reader, MAX_COMMAND_BYTES)).await {
        Ok(line) => Ok(line?.map(|l| String::from_utf8_lossy(&l).into_owned())),
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "the peer stopped sending commands",
        )),
    }
}

/// The `DATA` payload, dot-unstuffed, up to `max_bytes`.
///
/// `Ok(None)` means the message was too large. The stream is drained to the
/// terminating dot first — a peer that is told "too large" mid-`DATA` cannot
/// read the reply, so the connection is closed by the caller either way, but
/// draining keeps the refusal from being reported as a broken pipe.
async fn read_message<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut out = Vec::new();
    let mut over = false;
    loop {
        // Bounded by the message cap and not by the command cap: a mailer that
        // does not wrap — an HTML body sent 8-bit, most obviously — puts the
        // whole of it on one line, and refusing that would refuse ordinary
        // mail. The buffer is still bounded, by the same number the message is.
        let Some(line) = read_line(reader, max_bytes.max(MAX_COMMAND_BYTES)).await? else {
            // End of stream before the dot: an incomplete message, which is
            // never filed.
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the peer disconnected mid-message",
            ));
        };
        let body = line
            .strip_suffix(b"\r\n".as_slice())
            .unwrap_or_else(|| line.strip_suffix(b"\n".as_slice()).unwrap_or(&line));
        if body == b"." {
            return Ok((!over).then_some(out));
        }
        // Transparency (RFC 5321 §4.5.2): a line the sender began with a dot
        // arrives with a second one in front of it.
        let body = body.strip_prefix(b".".as_slice()).unwrap_or(body);
        if out.len() + body.len() + 2 > max_bytes {
            over = true;
            out.clear();
            continue;
        }
        if !over {
            out.extend_from_slice(body);
            out.extend_from_slice(b"\r\n");
        }
    }
}

/// Read up to and including the next `\n`, refusing a line longer than `limit`.
///
/// `read_until` would do this but has no cap, and the length of a line is one
/// of the things an unauthenticated peer must not get to choose.
async fn read_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut out = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok((!out.is_empty()).then_some(out));
        }
        match available.iter().position(|b| *b == b'\n') {
            Some(i) => {
                out.extend_from_slice(&available[..=i]);
                reader.consume(i + 1);
                return Ok(Some(out));
            }
            None => {
                let n = available.len();
                out.extend_from_slice(available);
                reader.consume(n);
                if out.len() > limit {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "the peer sent a line longer than this receiver accepts",
                    ));
                }
            }
        }
    }
}

async fn say<W: AsyncWrite + Unpin>(writer: &mut W, line: &str) -> std::io::Result<()> {
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\r\n").await
}

/// The TLS acceptor for the receiver's own certificate.
///
/// Not `agent_tls::acceptor`: that one requests a client certificate, because
/// the fleet authenticates with mTLS. A relay has no client certificate and
/// asking for one would make every well-behaved MTA log a warning about it.
fn acceptor(cert_path: &str, key_path: &str) -> Result<TlsAcceptor> {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};

    let certs: Vec<CertificateDer<'static>> = {
        let pem = std::fs::read(cert_path)
            .with_context(|| format!("cannot read the inbound SMTP certificate at {cert_path}"))?;
        rustls_pemfile::certs(&mut pem.as_slice()).collect::<std::result::Result<_, _>>()?
    };
    if certs.is_empty() {
        anyhow::bail!("{cert_path} contains no certificate");
    }
    let key: PrivateKeyDer<'static> = {
        let pem = std::fs::read(key_path)
            .with_context(|| format!("cannot read the inbound SMTP key at {key_path}"))?;
        rustls_pemfile::private_key(&mut pem.as_slice())?
            .with_context(|| format!("{key_path} contains no private key"))?
    };
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("the inbound SMTP certificate and key do not match")?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_envelope_address_survives_every_spelling_a_relay_uses() {
        assert_eq!(
            address_of("FROM", "FROM:<reporter@example.com>").as_deref(),
            Some("reporter@example.com")
        );
        assert_eq!(
            address_of(
                "FROM",
                "FROM: <reporter@example.com> SIZE=4096 BODY=8BITMIME"
            )
            .as_deref(),
            Some("reporter@example.com")
        );
        assert_eq!(
            address_of("TO", "to:<support@acme.example>").as_deref(),
            Some("support@acme.example")
        );
        assert_eq!(
            address_of("FROM", "FROM:reporter@example.com").as_deref(),
            Some("reporter@example.com")
        );
        // The null sender a bounce carries. Parsed, then refused downstream.
        assert_eq!(address_of("FROM", "FROM:<>").as_deref(), Some(""));
        // The wrong keyword is not a `MAIL FROM` however it is punctuated.
        assert_eq!(address_of("FROM", "TO:<a@b>"), None);
        assert_eq!(address_of("FROM", "reporter@example.com"), None);
    }

    /// RFC 5321 §4.5.2: the sender doubles a leading dot, and the receiver must
    /// undo it — otherwise a message quoting SMTP is corrupted, and a body line
    /// of `.` ends the message early.
    #[tokio::test]
    async fn data_is_dot_unstuffed_and_ends_at_the_lone_dot() {
        let wire = b"Subject: hi\r\n\r\n..hidden\r\nplain\r\n.\r\nQUIT\r\n";
        let mut reader = BufReader::new(&wire[..]);
        let message = read_message(&mut reader, 1024)
            .await
            .expect("reads")
            .expect("inside the cap");
        assert_eq!(message, b"Subject: hi\r\n\r\n.hidden\r\nplain\r\n");
        // The reader is left exactly after the dot, so the session goes on.
        let next = read_command(&mut reader).await.expect("reads");
        assert_eq!(next.as_deref().map(str::trim), Some("QUIT"));
    }

    #[tokio::test]
    async fn a_message_over_the_cap_is_refused_but_still_drained() {
        let wire = b"Subject: hi\r\n\r\naaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n.\r\nQUIT\r\n";
        let mut reader = BufReader::new(&wire[..]);
        assert!(
            read_message(&mut reader, 16)
                .await
                .expect("reads")
                .is_none(),
            "over the cap"
        );
        let next = read_command(&mut reader).await.expect("reads");
        assert_eq!(
            next.as_deref().map(str::trim),
            Some("QUIT"),
            "the stream is left at the command that follows the message"
        );
    }

    #[tokio::test]
    async fn a_line_longer_than_the_cap_is_an_error_rather_than_a_buffer() {
        let wire = vec![b'x'; MAX_COMMAND_BYTES * 4];
        let mut reader = BufReader::new(&wire[..]);
        assert!(read_line(&mut reader, MAX_COMMAND_BYTES).await.is_err());
    }

    #[test]
    fn the_credential_matches_only_itself() {
        let credential = Credential {
            username: "relay".into(),
            password: "s3cret".into(),
        };
        assert!(credential.matches("relay", "s3cret"));
        assert!(!credential.matches("relay", "s3cre"));
        assert!(!credential.matches("relay", "s3crett"));
        assert!(!credential.matches("Relay", "s3cret"));
        assert!(!credential.matches("", ""));
    }

    #[test]
    fn loopback_is_recognized_in_the_forms_an_operator_writes() {
        assert!(is_loopback("127.0.0.1:2525"));
        assert!(is_loopback("localhost:2525"));
        assert!(is_loopback("[::1]:2525"));
        assert!(!is_loopback("0.0.0.0:2525"));
        assert!(!is_loopback("10.0.0.4:2525"));
    }

    /// An SMTP reply is one line. A pipeline error carrying a newline would
    /// otherwise let the peer's own message write the next reply code.
    #[test]
    fn a_refusal_reason_cannot_break_out_of_its_reply_line() {
        assert_eq!(
            one_line("that is not\r\n250 Ok\r\na MIME message"),
            "that is not 250 Ok a MIME message"
        );
    }

    /// The listener is the only source that opens a port, so "no address
    /// configured" must mean "no socket", not "a socket on a default".
    #[tokio::test]
    async fn an_unconfigured_deployment_binds_nothing() {
        let cfg = Config::for_test();
        assert!(bind(&cfg).await.expect("no error").is_none());
    }

    /// Enabling the receiver without a credential would let anyone who reaches
    /// the port choose the envelope sender the allow-list is applied to.
    #[tokio::test]
    async fn an_enabled_receiver_with_no_credential_refuses_to_bind() {
        let mut cfg = Config::for_test();
        cfg.email_smtp_listen = Some("127.0.0.1:0".into());
        assert!(bind(&cfg).await.is_err());
        cfg.email_smtp_username = Some("relay".into());
        assert!(bind(&cfg).await.is_err(), "half a credential is none");
    }

    /// Half-configured TLS is a mistake worth failing on rather than quietly
    /// serving the credential in the clear.
    #[tokio::test]
    async fn half_configured_tls_refuses_to_bind() {
        let mut cfg = Config::for_test();
        cfg.email_smtp_listen = Some("127.0.0.1:0".into());
        cfg.email_smtp_username = Some("relay".into());
        cfg.email_smtp_password = Some("s3cret".into());
        cfg.email_smtp_tls_cert = Some("/nonexistent.crt".into());
        assert!(bind(&cfg).await.is_err());
    }

    /// A production deployment reachable from off the host must not send its
    /// relay credential as base64 over a cleartext socket.
    #[tokio::test]
    async fn production_refuses_a_plaintext_listener_that_is_not_loopback() {
        let mut cfg = Config::for_test();
        cfg.app_env = "production".into();
        cfg.email_smtp_listen = Some("0.0.0.0:0".into());
        cfg.email_smtp_username = Some("relay".into());
        cfg.email_smtp_password = Some("s3cret".into());
        assert!(bind(&cfg).await.is_err());

        cfg.email_smtp_listen = Some("127.0.0.1:0".into());
        assert!(
            bind(&cfg).await.expect("loopback is allowed").is_some(),
            "behind an MTA on the same host is the supported plaintext shape"
        );
    }
}
