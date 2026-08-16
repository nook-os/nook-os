//! The IMAP poller: a second [`EmailSource`] feeding C1's pipeline (MAIN-333).
//!
//! ## What is new here, and what is not
//!
//! New: an acquisition loop, a normalizer for whole RFC 5322 messages, and a
//! ledger of what has already been ingested. Not new — and not touched —
//! everything a message meets after the allow-list: the card, the sealed
//! original, the investigate run. `receive` is called with the same
//! [`InboundEmail`] the webhook produces, and files exactly the same way.
//!
//! ## The trust gate, for a source with no signature (AC-2)
//!
//! A webhook delivery is authenticated by an HMAC because anybody on the
//! internet can post to that route. Nothing can post here: the messages come
//! from a mailbox the deployment held the password to, over TLS, and the only
//! party who could have put one there is whoever can send mail to the tenant's
//! support address. So the gate is the allow-list, applied to a sender the
//! DELIVERING server vouched for — and the whole question becomes which
//! address that is.
//!
//! It is `Return-Path`, and it is the topmost one. RFC 5321 has the final
//! delivery MTA write the SMTP envelope sender into that header as it files the
//! message, which makes it the same fact the webhook source reads out of
//! `envelope.from` — provider-verified, not author-supplied. `From:` is free
//! text and is never consulted, exactly as in C1. A sender can of course
//! include their own `Return-Path` in what they send; it lands BELOW the one
//! the delivering server prepends, which is why "topmost" is load-bearing and
//! why this module reads headers in document order rather than through
//! mail-parser's accessors (which return the LAST occurrence).
//!
//! A message with no `Return-Path` at all is refused rather than falling back:
//! there is then no address the allow-list can be applied to, and C1's rule —
//! fail closed, never widen the gate — is the same rule here.
//!
//! ## Idempotency (AC-3)
//!
//! Two mechanisms, and both are needed:
//!
//! - The **UID watermark** means an ordinary poll asks the server only for what
//!   arrived since the last one. It is an efficiency, not a guarantee: a server
//!   may renumber a mailbox (`UIDVALIDITY` changes) and the watermark is then
//!   worthless by design.
//! - The **message-id ledger** is the guarantee. Every message is claimed by
//!   its own identity before it is processed, and a claim that conflicts is a
//!   message somebody has already decided about. It survives renumbering,
//!   reconfiguration, a mailbox restored from backup, and two replicas polling
//!   at once.

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use mail_parser::{MessageParser, MimeHeaders};
use nook_types::*;
use sha2::{Digest, Sha256};

use crate::error::{ApiError, ApiResult};
use crate::repo::email_pollers::EmailPoller;
use crate::services::email_inbound::{
    self as inbound, Disposition, EmailSource, InboundAttachment, InboundEmail,
};
use crate::services::imap::{ImapAccount, ImapFetcher, TlsImapFetcher, Watermark};
use crate::state::AppState;

/// How often the sweep looks for a poller that is due. Each poller has its own
/// interval; this is only the resolution that interval is honoured at.
const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

/// The narrowest interval a tenant may ask for. A poll is a login and a search
/// against somebody else's server, and providers rate-limit; ten seconds is
/// already far below any support workflow's needs.
pub const MIN_POLL_INTERVAL_SECS: i32 = 10;

/// What a tenant gets when it names no interval.
pub const DEFAULT_POLL_INTERVAL_SECS: i32 = 60;

pub const DEFAULT_MAILBOX: &str = "INBOX";
pub const DEFAULT_PORT: i32 = 993;

/// This source's name — in the log, and in the object key each sealed message
/// is stored under.
pub const SOURCE_ID: &str = "imap";

/// A polled mailbox's messages, normalized (AC-1).
pub struct ImapSource;

#[async_trait]
impl EmailSource for ImapSource {
    fn id(&self) -> &'static str {
        SOURCE_ID
    }

    /// Async by the trait's shape, but this does no I/O: the poller fetched
    /// `BODY[]` whole, so there is nothing left to go back for.
    async fn normalize(&self, raw: &[u8]) -> ApiResult<InboundEmail> {
        let message = MessageParser::default()
            .parse(raw)
            .ok_or_else(|| ApiError::BadRequest("that is not an RFC 5322 message".into()))?;

        let headers: Vec<(String, String)> = message
            .headers_raw()
            .map(|(name, value)| (name.to_ascii_lowercase(), unfold(value)))
            .collect();
        let topmost = |name: &str| {
            headers
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.as_str())
        };

        // See the module docs: the delivering server's own `Return-Path`, never
        // the author's `From:`, and never a second copy further down.
        let from = topmost("return-path")
            .map(bare_address)
            .filter(|a| !a.is_empty())
            .ok_or_else(|| {
                ApiError::BadRequest(
                    "the message carries no Return-Path naming a sender — nothing \
                     recorded who sent it, so the allow-list cannot be applied to it"
                        .into(),
                )
            })?;

        // SPF is what authenticates a `Return-Path`, so a server that checked
        // and said no has refuted the one fact the gate rests on. Absence is
        // NOT a refusal the way it is for the webhook: there the field is the
        // only authentication a public route has, while here the mailbox
        // already is, and plenty of ordinary servers file mail without an
        // `Authentication-Results` header at all.
        //
        // Only SPF. DMARC is about `From:` aligning with the envelope, and
        // `From:` is never read here — so a `dmarc=fail`, which is the ordinary
        // state of forwarded and mailing-list mail, would drop reports this
        // mailbox exists to receive while refuting nothing the gate relies on.
        if let Some(results) = topmost("authentication-results") {
            if spf_failed(results) {
                return Err(ApiError::BadRequest(
                    "the delivering server's own SPF check failed, so nothing vouches for \
                     the Return-Path the allow-list would be applied to"
                        .into(),
                ));
            }
        }

        let mut to: Vec<String> = Vec::new();
        // `Delivered-To`/`X-Original-To` first: an alias, a list or a Bcc puts
        // the address the message actually reached nowhere else.
        for (name, value) in &headers {
            if matches!(name.as_str(), "delivered-to" | "x-original-to") {
                to.push(bare_address(value));
            }
        }
        if let Some(addresses) = message.to().and_then(|a| a.as_list().map(<[_]>::to_vec)) {
            to.extend(
                addresses
                    .iter()
                    .filter_map(|a| a.address.as_deref())
                    .map(bare_address),
            );
        }
        to.retain(|a| !a.is_empty());

        let attachments = message
            .attachments()
            .enumerate()
            .map(|(i, part)| InboundAttachment {
                filename: part
                    .attachment_name()
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("attachment-{i}")),
                content_type: part
                    .content_type()
                    .map(|ct| match ct.subtype() {
                        Some(sub) => format!("{}/{sub}", ct.ctype()),
                        None => ct.ctype().to_string(),
                    })
                    .unwrap_or_else(|| "application/octet-stream".into()),
                bytes: part.contents().to_vec(),
            })
            .collect();

        Ok(InboundEmail {
            from,
            to,
            subject: message.subject().unwrap_or_default().to_string(),
            body_text: message.body_text(0).map(|b| b.into_owned()),
            body_html: message.body_html(0).map(|b| b.into_owned()),
            attachments,
            message_id: message.message_id().map(str::to_string),
            in_reply_to: message.in_reply_to().as_text().map(str::to_string),
            received_at: message
                .date()
                .and_then(|d| Utc.timestamp_opt(d.to_timestamp(), 0).single())
                .unwrap_or_else(Utc::now),
            raw: raw.to_vec(),
        })
    }
}

/// Start the sweep. One task for the deployment; every replica may run it,
/// because a poller is claimed by the conditional update that marks it polled.
pub fn start(state: AppState) {
    tokio::spawn(async move {
        let fetcher = TlsImapFetcher;
        loop {
            tokio::time::sleep(SWEEP_INTERVAL).await;
            if let Err(e) = sweep(&state, &fetcher).await {
                tracing::warn!(error = %e, "the inbound-email poll sweep failed");
            }
        }
    });
}

/// One pass: poll every mailbox that is due.
pub async fn sweep(state: &AppState, fetcher: &dyn ImapFetcher) -> ApiResult<()> {
    for poller in state.email_pollers.claim_due().await? {
        let tenant = poller.tenant_id;
        // One tenant's unreachable server must not stop the next tenant's poll,
        // so the failure is recorded on the row and the sweep continues.
        if let Err(e) = poll_one(state, fetcher, &poller).await {
            tracing::warn!(%tenant, error = %e, "polling a tenant's mailbox failed");
            state
                .email_pollers
                .record_poll(tenant, None, Some(&e.to_string()))
                .await?;
        }
    }
    Ok(())
}

/// Poll one mailbox and run everything it yields through the shared pipeline.
pub async fn poll_one(
    state: &AppState,
    fetcher: &dyn ImapFetcher,
    poller: &EmailPoller,
) -> ApiResult<()> {
    let tenant = poller.tenant_id;
    let account = account(state, poller)?;

    // The remembered position travels WHOLE — which namespace, and how far into
    // it — so the fetcher can drop it the moment the server reports a different
    // `UIDVALIDITY`, rather than this function noticing one poll too late.
    let polled = fetcher
        .poll(
            &account,
            Watermark {
                uid_validity: poller.uid_validity.and_then(|v| u32::try_from(v).ok()),
                last_uid: u32::try_from(poller.last_uid).unwrap_or(0),
            },
        )
        .await?;

    let renumbered = poller
        .uid_validity
        .is_some_and(|v| v != i64::from(polled.uid_validity));
    if renumbered {
        tracing::info!(
            %tenant,
            "the polled mailbox was renumbered (UIDVALIDITY changed) — reading it from the \
             start again; the message-id ledger is what stops that re-filing anything"
        );
    }

    // Zero, not the stored number, when the mailbox was renumbered: the stored
    // one counts in a namespace that no longer exists, and keeping it would put
    // the new namespace's low UIDs permanently out of reach.
    let mut highest = if renumbered { 0 } else { poller.last_uid };
    for message in &polled.messages {
        // The watermark may only ever cover a CONTIGUOUS PREFIX of messages
        // this deployment has actually decided about. Advancing past an
        // undecided one would put it beyond every future `UID SEARCH`, which
        // loses it silently and forever — so an undecided message stops the
        // advance rather than being stepped over, and the poll simply ends
        // there. Messages are ingested oldest-first, which is what makes
        // "stop here" the same thing as "keep the prefix".
        match ingest(state, tenant, &message.raw).await? {
            Ingested::Decided => highest = highest.max(i64::from(message.uid)),
            Ingested::Undecided => break,
        }
    }

    state
        .email_pollers
        .record_poll(
            tenant,
            Some((i64::from(polled.uid_validity), highest)),
            None,
        )
        .await?;
    Ok(())
}

/// Was anything actually settled about a message?
///
/// The distinction the UID watermark turns on: a message this deployment has
/// decided about — filed, or deliberately dropped — is behind us, while one it
/// could not yet decide must stay reachable by a later poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ingested {
    Decided,
    Undecided,
}

/// Claim one message, then hand it to the shared pipeline (AC-2, AC-3).
async fn ingest(state: &AppState, tenant: TenantId, raw: &[u8]) -> ApiResult<Ingested> {
    let key = dedupe_key(raw);
    if !state
        .email_pollers
        .claim_message(tenant, SOURCE_ID, &key)
        .await?
    {
        tracing::debug!(%tenant, message_id = %key, "polled message already ingested — skipped");
        // Somebody else decided about it, which is still decided.
        return Ok(Ingested::Decided);
    }

    // `receive_authenticated`, not `receive`: the connection WAS the
    // authentication — the poller held the mailbox's credentials over TLS, and
    // nothing reaches here the server did not hand over. There is no signature
    // to check and no deployment secret to require, so an IMAP-only deployment
    // sets no `EMAIL_INBOUND_SECRET` and still receives its mail.
    let outcome = inbound::receive_authenticated(
        state,
        &ImapSource,
        raw,
        inbound::Routing::ToMailboxOwner(tenant),
    )
    .await;

    match outcome {
        Ok(Disposition::Filed { task, .. }) => {
            state
                .email_pollers
                .record_filed(tenant, SOURCE_ID, &key, task)
                .await?;
            Ok(Ingested::Decided)
        }
        // The one drop that is about the deployment and not the message: there
        // is no allow-list yet, so nothing was decided ABOUT this mail and the
        // claim goes back. Otherwise configuring `email.inbound` after the
        // poller would leave every message already in the mailbox permanently
        // unreadable — the route refuses that ordering, and this is what makes
        // deleting the setting afterwards recoverable too.
        Ok(Disposition::Dropped(reason)) if reason == inbound::DROPPED_UNCONFIGURED => {
            state
                .email_pollers
                .release_message(tenant, SOURCE_ID, &key)
                .await?;
            // Releasing the ledger row is only half of staying reachable: the
            // watermark must not move past it either, or the release buys
            // nothing and the message is lost anyway.
            Ok(Ingested::Undecided)
        }
        // Every other drop KEEPS its claim. The gate has decided about this
        // message, and deciding again on the next poll would be the same
        // refusal plus a second log line — forever, for every message the
        // mailbox holds that support staff did not send.
        Ok(Disposition::Dropped(reason)) => {
            tracing::info!(%tenant, reason, "polled message dropped by the trust gate");
            Ok(Ingested::Decided)
        }
        Err(e) if permanent(&e) => {
            // Also keeps its claim, for the same reason: no future poll will
            // parse these bytes any differently.
            tracing::warn!(%tenant, error = %e, "polled message could not be normalized");
            Ok(Ingested::Decided)
        }
        Err(e) => {
            // Everything else is about the moment, not the message — the store
            // is down, the tenant has no board yet. Give the claim back so a
            // later poll gets to try again, and let the sweep record the
            // failure against the poller.
            state
                .email_pollers
                .release_message(tenant, SOURCE_ID, &key)
                .await?;
            Err(e)
        }
    }
}

/// Is this failure a property of the message rather than of the moment?
///
/// A `BadRequest` here means normalization refused the bytes — no
/// `Return-Path`, a failed sender check, not a message at all. Retrying cannot
/// change any of those. Anything else (the object store, the board, the
/// database) can be true again later.
fn permanent(e: &ApiError) -> bool {
    matches!(e, ApiError::BadRequest(_))
}

/// What AC-3 dedupes on.
///
/// The `Message-Id` when the message has one, which is the identity the card
/// asks for and the one that survives a mailbox being renumbered or restored.
/// A digest of the raw bytes when it does not: `Message-Id` is a SHOULD, not a
/// MUST, and a source that skipped dedupe for messages lacking one would file
/// exactly those twice — a guarantee with a hole in it is not one.
fn dedupe_key(raw: &[u8]) -> String {
    match MessageParser::default()
        .parse(raw)
        .and_then(|m| m.message_id().map(str::to_string))
        .filter(|id| !id.trim().is_empty())
    {
        Some(id) => id,
        None => format!("sha256:{:x}", Sha256::digest(raw)),
    }
}

/// Unseal the credential for the length of one poll.
fn account(state: &AppState, poller: &EmailPoller) -> ApiResult<ImapAccount> {
    let password = state
        .vault
        .decrypt_string(&poller.password_enc)
        .map_err(|e| {
            ApiError::Internal(anyhow::anyhow!(
                "the stored IMAP password could not be unsealed (wrong SECRETS_KEY?): {e}"
            ))
        })?;
    Ok(ImapAccount {
        host: poller.host.clone(),
        port: u16::try_from(poller.port)
            .map_err(|_| ApiError::BadRequest(format!("{} is not a port", poller.port)))?,
        username: poller.username.clone(),
        password,
        mailbox: poller.mailbox.clone(),
    })
}

/// A header value as one line: RFC 5322 folding is CRLF plus whitespace, and
/// the value is what the fold hid.
fn unfold(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `<a@b.example>` or `A Person <a@b.example>` → `a@b.example`.
///
/// `<>` — the null return path a bounce carries — comes back empty, which is
/// what makes a bounce message refused rather than attributed to nobody.
fn bare_address(raw: &str) -> String {
    let raw = raw.trim();
    match (raw.rfind('<'), raw.rfind('>')) {
        (Some(open), Some(close)) if close > open + 1 => raw[open + 1..close].trim().to_string(),
        (Some(_), Some(_)) => String::new(),
        _ => raw.to_string(),
    }
}

/// Did the delivering server's own SPF check say `fail`?
///
/// `fail` only. `softfail`, `neutral`, `none` and `temperror` all mean the
/// check did not conclude, and treating an inconclusive result as a refusal
/// would drop the forwarded and relayed mail a support mailbox routinely
/// receives.
fn spf_failed(results: &str) -> bool {
    results
        .to_ascii_lowercase()
        .split(';')
        .filter_map(|part| part.trim().strip_prefix("spf="))
        .any(|rest| rest.trim_start().starts_with("fail"))
}
