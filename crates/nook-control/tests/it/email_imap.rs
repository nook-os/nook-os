//! The IMAP poller (MAIN-333).
//!
//! The mailbox is a fake [`ImapFetcher`], not a server: what these tests are
//! about is what happens to a message AFTER it has been fetched, and the
//! protocol that fetches it has its own tests beside the parser it exercises
//! (`services::imap`). The fake also lets a test say "the same message, twice"
//! exactly, which is what AC-3 is a claim about.
//!
//! Every message here is real RFC 5322 with the headers a delivering server
//! adds, because those headers ARE the trust gate for this source.

use async_trait::async_trait;
use nook_control::error::ApiResult;
use nook_control::repo::admin::SettingWrite;
use nook_control::repo::email_pollers::NewEmailPoller;
use nook_control::repo::tasks::{DbTaskRepository, TaskRepository};
use nook_control::services::email_imap;
use nook_control::services::email_inbound as inbound;
use nook_control::services::imap::{FetchedMessage, ImapAccount, ImapFetcher, Polled, Watermark};
use nook_control::AppState;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use std::sync::Mutex;
use uuid::Uuid;

const SUPPORT: &str = "support@acme.example";
const STAFF: &str = "reporter@acme.example";

/// A private disk root per test, so one test's sealed objects are never
/// another's.
struct Scratch(std::path::PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    tenant: TenantId,
    state: AppState,
    _scratch: Scratch,
}

/// A mailbox that answers with whatever the test put in it.
///
/// `uid_validity` is settable because a server renumbering a mailbox is the one
/// event the UID watermark cannot survive, and the ledger's whole job is to
/// make that survivable anyway.
struct FakeMailbox {
    uid_validity: Mutex<u32>,
    messages: Mutex<Vec<FetchedMessage>>,
    polls: Mutex<Vec<u32>>,
}

impl FakeMailbox {
    fn holding(messages: Vec<(u32, String)>) -> Self {
        Self {
            uid_validity: Mutex::new(1),
            messages: Mutex::new(
                messages
                    .into_iter()
                    .map(|(uid, raw)| FetchedMessage {
                        uid,
                        raw: raw.into_bytes(),
                    })
                    .collect(),
            ),
            polls: Mutex::new(Vec::new()),
        }
    }

    /// The EFFECTIVE `since_uid` each poll searched from — the watermark as
    /// the server actually applied it.
    fn asked_from(&self) -> Vec<u32> {
        self.polls.lock().expect("polls").clone()
    }

    fn renumber(&self) {
        *self.uid_validity.lock().expect("validity") += 1;
    }
}

#[async_trait]
impl ImapFetcher for FakeMailbox {
    async fn poll(&self, _account: &ImapAccount, since: Watermark) -> ApiResult<Polled> {
        let uid_validity = *self.uid_validity.lock().expect("validity");
        // The same rule a real server's `SELECT` makes the client apply: a
        // watermark from another namespace names nothing here.
        let since_uid = match since.uid_validity {
            Some(remembered) if remembered == uid_validity => since.last_uid,
            _ => 0,
        };
        self.polls.lock().expect("polls").push(since_uid);
        Ok(Polled {
            uid_validity,
            messages: self
                .messages
                .lock()
                .expect("messages")
                .iter()
                .filter(|m| m.uid > since_uid)
                .cloned()
                .collect(),
        })
    }
}

/// A tenant that polls a mailbox: a board to file on, an owner for the seeded
/// run to be requested by, the `email.inbound` allow-list the gate reads, and
/// the poller row itself.
async fn fixture(bed: &TestBed) -> Fixture {
    let scratch =
        Scratch(std::env::temp_dir().join(format!("nook-imap-{}", Uuid::now_v7().simple())));
    let mut cfg = bed.config();
    cfg.user_content_dir = scratch.0.to_string_lossy().into_owned();
    // Deliberately none: a deployment that receives mail ONLY by polling
    // configures no webhook secret, and this source must work without one.
    cfg.email_inbound_secret = None;
    let state = AppState::new(bed.db(), cfg, None).await;

    let tenant = bed.tenant("imap").await;
    bed.user(tenant, "owner").await;

    let repo = DbTaskRepository::new(bed.db());
    let board = repo
        .create_board(
            tenant,
            None,
            "Support",
            &format!("S{}", &tenant.0.simple().to_string()[..4]).to_uppercase(),
        )
        .await
        .expect("board");
    repo.create_column(board.id, "Backlog", 0, "backlog")
        .await
        .expect("backlog column");

    state
        .settings
        .put(SettingWrite {
            tenant,
            scope: "tenant".into(),
            user: None,
            key: inbound::SETTING_KEY.into(),
            value: serde_json::json!({
                "address": SUPPORT,
                "allow_from": [format!("A Reporter <{STAFF}>")],
            }),
        })
        .await
        .expect("configure inbound email");

    state
        .email_pollers
        .put(NewEmailPoller {
            tenant,
            host: "imap.example".into(),
            port: 993,
            username: SUPPORT.into(),
            password_enc: state.vault.encrypt(b"hunter2").expect("seal"),
            mailbox: "INBOX".into(),
            // Zero, so every `sweep` here actually polls. What these tests are
            // about is what a fetched message becomes; whether a poller is DUE
            // has one test of its own below, and pacing every other test on a
            // real interval would mean sleeping through it.
            poll_interval_secs: 0,
            enabled: true,
        })
        .await
        .expect("configure the poller");

    Fixture {
        tenant,
        state,
        _scratch: scratch,
    }
}

/// One message as a delivering server files it: the envelope sender it recorded
/// in `Return-Path`, then the author's own headers.
fn message(return_path: &str, message_id: &str, subject: &str, body: &str) -> String {
    format!(
        "Return-Path: <{return_path}>\r\n\
         Delivered-To: {SUPPORT}\r\n\
         Authentication-Results: mx.example; spf=pass smtp.mailfrom={return_path}\r\n\
         From: A Reporter <{STAFF}>\r\n\
         To: {SUPPORT}\r\n\
         Subject: {subject}\r\n\
         Message-Id: <{message_id}>\r\n\
         Date: Thu, 14 Aug 2026 09:15:00 +0000\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         \r\n\
         {body}\r\n"
    )
}

fn staff_message(message_id: &str, body: &str) -> String {
    message(STAFF, message_id, "the login page 500s", body)
}

/// Every card on the tenant's board, as `(type, title, description, column type)`.
async fn cards(fx: &Fixture) -> Vec<(String, String, String, String)> {
    fx.state
        .db
        .query_all(
            "SELECT t.type, t.title, COALESCE(t.description, ''), c.type
             FROM tasks t JOIN board_columns c ON c.id = t.column_id
             WHERE t.tenant_id = $1 ORDER BY t.created_at",
            params![fx.tenant],
        )
        .await
        .expect("read the board")
}

/// Every loop job raised for the tenant, as `(kind, state)`.
async fn jobs(fx: &Fixture) -> Vec<(String, String)> {
    fx.state
        .db
        .query_all(
            "SELECT kind, state FROM loop_jobs WHERE tenant_id = $1",
            params![fx.tenant],
        )
        .await
        .expect("read the jobs")
}

/// The ledger AC-3 is enforced by, as `(message_id, filed)`.
async fn ledger(fx: &Fixture) -> Vec<(String, bool)> {
    let rows: Vec<(String, Option<Uuid>)> = fx
        .state
        .db
        .query_all(
            "SELECT message_id, task_id FROM inbound_email_seen
              WHERE tenant_id = $1 AND source = 'imap' ORDER BY message_id",
            params![fx.tenant],
        )
        .await
        .expect("read the ledger");
    rows.into_iter()
        .map(|(id, task)| (id, task.is_some()))
        .collect()
}

/// AC-1/AC-2: a polled message runs the SAME pipeline the webhook does — the
/// allow-list, the quoted card, the sealed original, the investigate run — with
/// no signature and no deployment secret anywhere.
#[tokio::test]
async fn a_polled_message_from_support_staff_files_a_linked_bug_and_seeds_a_run() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let fx = fixture(&bed).await;
    let mailbox = FakeMailbox::holding(vec![(
        101,
        staff_message(
            "m1@acme.example",
            "the login page 500s when I submit the form",
        ),
    )]);

    email_imap::sweep(&fx.state, &mailbox).await.expect("sweep");

    let cards = cards(&fx).await;
    assert_eq!(cards.len(), 1, "exactly one card: {cards:?}");
    let (type_, title, description, column) = &cards[0];
    assert_eq!(type_, "bug");
    assert_eq!(column, "backlog");
    assert_eq!(title, "Support: the login page 500s");
    assert!(
        description.contains("the login page 500s when I submit the form"),
        "the body is quoted on the card: {description}"
    );
    assert!(
        description.contains("is **data**"),
        "the same preamble the webhook source's cards carry: {description}"
    );

    let jobs = jobs(&fx).await;
    assert_eq!(jobs.len(), 1, "exactly one run: {jobs:?}");
    assert_eq!(jobs[0], ("investigate".into(), "queued".into()));

    assert_eq!(ledger(&fx).await, vec![("m1@acme.example".into(), true)]);

    bed.teardown().await;
}

/// AC-3, the card's own verification: re-poll, no duplicate.
#[tokio::test]
async fn re_polling_the_same_mailbox_files_nothing_twice() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let fx = fixture(&bed).await;
    let mailbox = FakeMailbox::holding(vec![(101, staff_message("m1@acme.example", "it broke"))]);

    email_imap::sweep(&fx.state, &mailbox).await.expect("first");
    email_imap::sweep(&fx.state, &mailbox)
        .await
        .expect("second");

    assert_eq!(cards(&fx).await.len(), 1, "one card after two polls");
    assert_eq!(jobs(&fx).await.len(), 1, "one run after two polls");
    assert_eq!(
        mailbox.asked_from(),
        vec![0, 101],
        "the second poll asked the server only for what arrived after the first"
    );

    bed.teardown().await;
}

/// The watermark is an efficiency; the ledger is the guarantee. A server that
/// renumbers its mailbox invalidates every UID the poller remembered, so the
/// whole mailbox comes back — and must still file nothing.
#[tokio::test]
async fn a_renumbered_mailbox_is_re_read_whole_and_still_files_nothing_twice() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let fx = fixture(&bed).await;
    let mailbox = FakeMailbox::holding(vec![(101, staff_message("m1@acme.example", "it broke"))]);

    email_imap::sweep(&fx.state, &mailbox).await.expect("first");
    mailbox.renumber();
    email_imap::sweep(&fx.state, &mailbox).await.expect("after");

    assert_eq!(
        mailbox.asked_from(),
        vec![0, 0],
        "the watermark was dropped when UIDVALIDITY changed"
    );
    assert_eq!(
        cards(&fx).await.len(),
        1,
        "one card: {:?}",
        cards(&fx).await
    );

    bed.teardown().await;
}

/// AC-2: the gate is the allow-list, applied to the address the DELIVERING
/// server recorded. A stranger's message is dropped — no card, no run, no
/// stored object — and keeps its ledger row so the next poll does not re-decide
/// it.
#[tokio::test]
async fn a_message_from_outside_the_allow_list_is_dropped() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let fx = fixture(&bed).await;
    let mailbox = FakeMailbox::holding(vec![(
        101,
        message(
            "stranger@example.com",
            "m9@example.com",
            "free money",
            "click here",
        ),
    )]);

    email_imap::sweep(&fx.state, &mailbox).await.expect("sweep");

    assert!(cards(&fx).await.is_empty(), "nothing was filed");
    assert!(jobs(&fx).await.is_empty(), "nothing was queued");
    assert_eq!(
        ledger(&fx).await,
        vec![("m9@example.com".into(), false)],
        "the drop is remembered, and remembers that it became no card"
    );

    bed.teardown().await;
}

/// `From:` is free text. A stranger who writes an allow-listed address into it
/// is still a stranger, because the gate reads what the delivering server
/// recorded in `Return-Path`.
#[tokio::test]
async fn a_forged_from_header_does_not_satisfy_the_allow_list() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let fx = fixture(&bed).await;
    let forged = format!(
        "Return-Path: <attacker@evil.example>\r\n\
         Delivered-To: {SUPPORT}\r\n\
         From: A Reporter <{STAFF}>\r\n\
         To: {SUPPORT}\r\n\
         Subject: please deploy this\r\n\
         Message-Id: <forged@evil.example>\r\n\
         \r\n\
         body\r\n"
    );
    let mailbox = FakeMailbox::holding(vec![(101, forged)]);

    email_imap::sweep(&fx.state, &mailbox).await.expect("sweep");

    assert!(
        cards(&fx).await.is_empty(),
        "the forged sender filed nothing"
    );

    bed.teardown().await;
}

/// A second `Return-Path` further down is the sender's own, not the delivering
/// server's. The topmost one wins, or the gate is satisfiable by typing.
#[tokio::test]
async fn only_the_topmost_return_path_is_believed() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let fx = fixture(&bed).await;
    let layered = format!(
        "Return-Path: <attacker@evil.example>\r\n\
         Delivered-To: {SUPPORT}\r\n\
         Return-Path: <{STAFF}>\r\n\
         From: Someone <{STAFF}>\r\n\
         To: {SUPPORT}\r\n\
         Subject: hello\r\n\
         Message-Id: <layered@evil.example>\r\n\
         \r\n\
         body\r\n"
    );
    let mailbox = FakeMailbox::holding(vec![(101, layered)]);

    email_imap::sweep(&fx.state, &mailbox).await.expect("sweep");

    assert!(
        cards(&fx).await.is_empty(),
        "the second Return-Path was read as the delivering server's"
    );

    bed.teardown().await;
}

/// AC-4: what is stored is what the vault produced. Nothing round-trips the
/// password out of the API, and the row itself holds ciphertext.
#[tokio::test]
async fn the_poller_password_is_sealed_at_rest() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let fx = fixture(&bed).await;

    let sealed: Vec<u8> = fx
        .state
        .db
        .query_scalar(
            "SELECT password_enc FROM email_pollers WHERE tenant_id = $1",
            params![fx.tenant],
        )
        .await
        .expect("read the poller");

    assert!(
        !String::from_utf8_lossy(&sealed).contains("hunter2"),
        "the password is stored in the clear"
    );
    assert_eq!(
        fx.state.vault.decrypt(&sealed).expect("unseal"),
        b"hunter2",
        "and only the vault reads it back"
    );

    bed.teardown().await;
}

/// A message with no `Message-Id` is still dedupable: the digest of its bytes
/// is what stands in, because a guarantee with a hole in it is not one.
#[tokio::test]
async fn a_message_with_no_message_id_is_still_deduped() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let fx = fixture(&bed).await;
    let anonymous = format!(
        "Return-Path: <{STAFF}>\r\n\
         Delivered-To: {SUPPORT}\r\n\
         From: A Reporter <{STAFF}>\r\n\
         To: {SUPPORT}\r\n\
         Subject: no id here\r\n\
         \r\n\
         body\r\n"
    );
    let mailbox = FakeMailbox::holding(vec![(101, anonymous.clone())]);

    email_imap::sweep(&fx.state, &mailbox).await.expect("first");
    mailbox.renumber(); // forces the whole mailbox to be re-read
    email_imap::sweep(&fx.state, &mailbox).await.expect("again");

    assert_eq!(cards(&fx).await.len(), 1, "one card from two reads");
    let ledger = ledger(&fx).await;
    assert_eq!(ledger.len(), 1);
    assert!(
        ledger[0].0.starts_with("sha256:"),
        "the digest stands in for the absent Message-Id: {ledger:?}"
    );

    bed.teardown().await;
}

/// A disabled poller is configuration that is not running. The sweep must not
/// claim it at all.
#[tokio::test]
async fn a_disabled_poller_is_not_swept() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let fx = fixture(&bed).await;
    fx.state
        .email_pollers
        .put(NewEmailPoller {
            tenant: fx.tenant,
            host: "imap.example".into(),
            port: 993,
            username: SUPPORT.into(),
            password_enc: fx.state.vault.encrypt(b"hunter2").expect("seal"),
            mailbox: "INBOX".into(),
            poll_interval_secs: 0,
            enabled: false,
        })
        .await
        .expect("disable");

    let mailbox = FakeMailbox::holding(vec![(101, staff_message("m1@acme.example", "it broke"))]);
    email_imap::sweep(&fx.state, &mailbox).await.expect("sweep");

    assert!(
        mailbox.asked_from().is_empty(),
        "the mailbox was never asked"
    );
    assert!(cards(&fx).await.is_empty());

    bed.teardown().await;
}

/// The interval is honoured: a poller polled a moment ago is not claimed again
/// until it has elapsed.
#[tokio::test]
async fn a_poller_inside_its_interval_is_not_claimed_again() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let fx = fixture(&bed).await;
    fx.state
        .email_pollers
        .put(NewEmailPoller {
            tenant: fx.tenant,
            host: "imap.example".into(),
            port: 993,
            username: SUPPORT.into(),
            password_enc: fx.state.vault.encrypt(b"hunter2").expect("seal"),
            mailbox: "INBOX".into(),
            poll_interval_secs: 3600,
            enabled: true,
        })
        .await
        .expect("re-configure");

    let mailbox = FakeMailbox::holding(vec![(101, staff_message("m1@acme.example", "it broke"))]);
    email_imap::sweep(&fx.state, &mailbox).await.expect("first");
    email_imap::sweep(&fx.state, &mailbox)
        .await
        .expect("second");

    assert_eq!(
        mailbox.asked_from().len(),
        1,
        "the second sweep found nothing due"
    );

    bed.teardown().await;
}

/// The one drop that is about the deployment rather than the message. A tenant
/// whose `email.inbound` has gone has no allow-list, so nothing was decided
/// ABOUT the mail — and the claim must go back, or restoring the setting would
/// leave everything already in the mailbox permanently unreadable.
#[tokio::test]
async fn a_drop_for_a_missing_allow_list_is_recoverable() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let fx = fixture(&bed).await;
    let allow_list = fx
        .state
        .settings
        .tenant_value(fx.tenant, inbound::SETTING_KEY)
        .await
        .expect("read")
        .expect("configured");
    // Straight through the database: `SettingRepository` has no delete, and the
    // state under test is a setting that is genuinely ABSENT — an empty
    // `allow_from` is a different thing, and drops as `sender-not-allowed`.
    fx.state
        .db
        .exec(
            "DELETE FROM settings WHERE tenant_id = $1 AND key = $2",
            params![fx.tenant, inbound::SETTING_KEY.to_string()],
        )
        .await
        .expect("unconfigure");

    let mailbox = FakeMailbox::holding(vec![(101, staff_message("m1@acme.example", "it broke"))]);
    email_imap::sweep(&fx.state, &mailbox).await.expect("sweep");

    assert!(cards(&fx).await.is_empty(), "nothing was filed");
    assert!(
        ledger(&fx).await.is_empty(),
        "and nothing was remembered, because nothing was decided"
    );

    // The watermark must not have moved past it either — releasing the ledger
    // row alone would recover nothing, because `UID SEARCH UID highest+1:*`
    // would never return the message again. This is the assertion that proves
    // the release is real; an earlier version of this test called
    // `mailbox.renumber()` here, and so passed because UIDVALIDITY changed
    // rather than because anything was released.
    assert_eq!(
        mailbox.asked_from(),
        vec![0],
        "the undecided message did not advance the watermark"
    );

    // Configure it and poll again — no renumbering, nothing else forced.
    fx.state
        .settings
        .put(SettingWrite {
            tenant: fx.tenant,
            scope: "tenant".into(),
            user: None,
            key: inbound::SETTING_KEY.into(),
            value: allow_list,
        })
        .await
        .expect("reconfigure");
    email_imap::sweep(&fx.state, &mailbox).await.expect("again");

    assert_eq!(
        mailbox.asked_from(),
        vec![0, 0],
        "the second poll still starts from the beginning, so the message is reachable"
    );
    assert_eq!(cards(&fx).await.len(), 1, "the held message was filed once");

    bed.teardown().await;
}
