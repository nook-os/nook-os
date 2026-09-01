//! Where a drafted reply actually goes, under each of the three policies
//! (MAIN-332).
//!
//! Every chain here comes from a real delivery through the real receiver route,
//! for the reason `email_links.rs` gives: what is under test is the pipeline's
//! own record, and a test that inserted its own link would pass with the
//! pipeline's write deleted.
//!
//! **The mailer is a bare `CaptureMailer`, not the state's guarded one.** The
//! guard's job is to hold everything back when `MAIL_SEND_ENABLED` is off, which
//! is the shipped default and what `Config::for_test` carries — so through it
//! every case here would assert "nothing sent" and pass whatever the routing
//! did. Swapping it out is what makes the recipient the thing under test.
//!
//! Nothing here is Postgres-shaped, so it runs on both engines.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{Request, StatusCode};
use axum::Json;
use axum::Router;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::mailer::capture::{CaptureMailer, CapturedEmail};
use nook_control::repo::admin::SettingWrite;
use nook_control::repo::tasks::{DbTaskRepository, TaskRepository};
use nook_control::routes::build_router;
use nook_control::routes::email_links::reply;
use nook_control::routes::jobs::investigation;
use nook_control::services::email_links::reply::{Policy, SETTING_KEY as POLICY_KEY};
use nook_control::services::{email_inbound as inbound, notify};
use nook_control::AppState;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use tower::ServiceExt;
use uuid::Uuid;

const SECRET: &str = "inbound-secret-inbound-secret";
/// The support staffer who forwards the report — the allow-listed sender.
const STAFF: &str = "staffer@acme.example";
/// The person who reported the problem, named only by the forward's `Reply-To`.
const CUSTOMER: &str = "customer@example.net";
const MESSAGE_ID: &str = "<m1@acme.example>";
/// What the forwarded message was itself a reply to — the rest of the thread
/// this record kept, and so the front of the outbound `References`.
const PARENT_ID: &str = "<m0@example.net>";
const SUBJECT: &str = "the login page 500s";

const FINDINGS: &str = "Reproduced: yes. auth.rs:212 unwraps before the empty-field check.";
const DRAFT: &str = "Hi — we reproduced the 500 you hit on submit. A fix is being scoped.";

fn ctx(user: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: true,
    }
}

fn status_of(err: nook_control::error::ApiError) -> StatusCode {
    axum::response::IntoResponse::into_response(err).status()
}

struct Scratch(std::path::PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    state: AppState,
    app: Router,
    mail: Arc<CaptureMailer>,
    _scratch: Scratch,
}

impl Fixture {
    fn sent(&self) -> Vec<CapturedEmail> {
        self.mail.sent()
    }
}

struct Receiver {
    tenant: TenantId,
    owner: UserId,
    address: String,
}

async fn fixture(bed: &TestBed) -> Fixture {
    let scratch =
        Scratch(std::env::temp_dir().join(format!("nook-reply-{}", Uuid::now_v7().simple())));
    let mut cfg = bed.config();
    cfg.user_content_dir = scratch.0.to_string_lossy().into_owned();
    cfg.email_inbound_secret = Some(SECRET.to_string());
    let mut state = AppState::new(bed.db(), cfg, None).await;
    let mail = Arc::new(CaptureMailer::new());
    state.mailer = mail.clone();
    Fixture {
        app: build_router(state.clone()),
        state,
        mail,
        _scratch: scratch,
    }
}

async fn receiver(bed: &TestBed, fx: &Fixture, slug: &str) -> Receiver {
    let tenant = bed.tenant(slug).await;
    let (owner, _) = bed.user(tenant, "owner").await;

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

    let address = format!("support@{slug}.example");
    fx.state
        .settings
        .put(SettingWrite {
            tenant,
            scope: "tenant".into(),
            user: None,
            key: inbound::SETTING_KEY.into(),
            value: serde_json::json!({
                "address": address,
                "allow_from": [STAFF],
            }),
        })
        .await
        .expect("configure inbound email");

    Receiver {
        tenant,
        owner,
        address,
    }
}

/// Say what this tenant does with a drafted reply. Absent is the default, which
/// several cases here deliberately never write.
async fn set_policy(fx: &Fixture, tenant: TenantId, policy: Policy) {
    fx.state
        .settings
        .put(SettingWrite {
            tenant,
            scope: "tenant".into(),
            user: None,
            key: POLICY_KEY.into(),
            value: serde_json::json!(policy.as_str()),
        })
        .await
        .expect("set the reply policy");
}

/// One forwarded support report. `reply_to` is what names the customer — a
/// message without it records no customer address at all.
async fn deliver(fx: &Fixture, to: &str, reply_to: Option<&str>) {
    let mut headers = format!("Message-Id: {MESSAGE_ID}\nIn-Reply-To: {PARENT_ID}");
    if let Some(reply_to) = reply_to {
        headers.push_str(&format!("\nReply-To: A Customer <{reply_to}>"));
    }
    let body = serde_json::json!({
        "envelope": { "from": STAFF, "to": [to] },
        "subject": SUBJECT,
        "text": "it 500s when I submit the form",
        "spf": "pass",
        "headers": headers,
    })
    .to_string();
    let signature = notify::sign(SECRET, &body, chrono::Utc::now().timestamp());
    let res = fx
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/email/inbound")
                .header("content-type", "application/json")
                .header(inbound::SIGNATURE_HEADER, signature)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .expect("the receiver answers");
    assert_eq!(res.status(), StatusCode::ACCEPTED);
}

async fn link_id(fx: &Fixture, tenant: TenantId) -> Uuid {
    fx.state
        .db
        .query_scalar::<Uuid>(
            "SELECT id FROM email_links WHERE tenant_id = $1",
            params![tenant],
        )
        .await
        .expect("the one chain")
}

async fn seeded_run(fx: &Fixture, tenant: TenantId) -> JobId {
    fx.state
        .db
        .query_scalar::<Option<JobId>>(
            "SELECT loop_job_id FROM email_links WHERE tenant_id = $1",
            params![tenant],
        )
        .await
        .expect("the one chain")
        .expect("a run was seeded")
}

/// Run the investigation, which is what puts a draft on the chain — and, for a
/// tenant on `auto_send`, what sends it.
async fn investigate(fx: &Fixture, who: AuthCtx, job: JobId) -> EmailLink {
    investigation(
        State(fx.state.clone()),
        who,
        Path(job),
        Json(InvestigationReport {
            findings: FINDINGS.into(),
            draft_reply: DRAFT.into(),
        }),
    )
    .await
    .map(|Json(l)| l)
    .expect("the run reports")
}

async fn approve(
    fx: &Fixture,
    who: AuthCtx,
    id: Uuid,
    edited: Option<&str>,
) -> Result<EmailLink, nook_control::error::ApiError> {
    reply(
        State(fx.state.clone()),
        who,
        Path(id),
        Json(SendReplyRequest {
            reply: edited.map(str::to_string),
        }),
    )
    .await
    .map(|Json(l)| l)
}

/// The whole chain, ready to approve: a delivery, its run, and its draft.
async fn drafted(bed: &TestBed, fx: &Fixture, slug: &str, reply_to: Option<&str>) -> Receiver {
    let rx = receiver(bed, fx, slug).await;
    deliver(fx, &rx.address, reply_to).await;
    let job = seeded_run(fx, rx.tenant).await;
    investigate(fx, ctx(rx.owner, rx.tenant), job).await;
    rx
}

/// AC-1 (the default) + AC-2 + AC-6: a tenant that has said nothing gets
/// `to_staffer`, and the customer is not emailed — even though the chain knows
/// perfectly well how to reach them.
#[tokio::test]
async fn the_default_policy_delivers_the_draft_to_the_staffer_and_never_the_customer() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let fx = fixture(&bed).await;
    let rx = drafted(&bed, &fx, "acme", Some(CUSTOMER)).await;

    // Nothing has been written to `email.reply_policy` at any point.
    let link = approve(
        &fx,
        ctx(rx.owner, rx.tenant),
        link_id(&fx, rx.tenant).await,
        None,
    )
    .await
    .expect("the staffer approves");

    let sent = fx.sent();
    assert_eq!(sent.len(), 1, "one reply, and only one");
    assert_eq!(sent[0].to, STAFF);
    assert_ne!(
        sent[0].to, CUSTOMER,
        "the default emails no customer (NG-1)"
    );
    assert_eq!(sent[0].text_body, DRAFT);
    assert_eq!(link.reply_recipient.as_deref(), Some(STAFF));
    assert!(link.reply_sent_at.is_some(), "the chain records the send");

    bed.teardown().await;
}

/// AC-3 + AC-5: the opted-in mode reaches the customer, threaded onto the
/// message that started the chain, and the chain records where it went.
#[tokio::test]
async fn approve_then_send_emails_the_customer_threaded_onto_the_original() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let fx = fixture(&bed).await;
    let rx = drafted(&bed, &fx, "acme", Some(CUSTOMER)).await;
    set_policy(&fx, rx.tenant, Policy::ApproveThenSend).await;

    // The draft alone sends nothing: this mode waits for a human.
    assert!(fx.sent().is_empty(), "nothing goes out before the approve");

    let link = approve(
        &fx,
        ctx(rx.owner, rx.tenant),
        link_id(&fx, rx.tenant).await,
        None,
    )
    .await
    .expect("the staffer approves");

    let sent = fx.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, CUSTOMER);
    assert_eq!(sent[0].subject, format!("Re: {SUBJECT}"));
    assert_eq!(
        sent[0].threading.in_reply_to.as_deref(),
        Some(MESSAGE_ID),
        "the reply answers the message that started the chain"
    );
    assert_eq!(
        sent[0].threading.references,
        vec![PARENT_ID.to_string(), MESSAGE_ID.to_string()],
        "References is the parent's chain, then the parent"
    );
    assert_eq!(link.reply_recipient.as_deref(), Some(CUSTOMER));

    bed.teardown().await;
}

/// AC-4 + AC-6: `auto_send` sends as the draft lands, with no approve call —
/// and it is the ONLY thing that does, which is what "explicit opt-in" means.
#[tokio::test]
async fn auto_send_replies_without_a_human_and_only_when_the_tenant_asked() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let fx = fixture(&bed).await;

    // The same flow, in a tenant that opted into nothing: reporting the
    // investigation sends no mail at all.
    let quiet = receiver(&bed, &fx, "quiet").await;
    deliver(&fx, &quiet.address, Some(CUSTOMER)).await;
    let job = seeded_run(&fx, quiet.tenant).await;
    investigate(&fx, ctx(quiet.owner, quiet.tenant), job).await;
    assert!(
        fx.sent().is_empty(),
        "a draft is not a send unless the tenant said auto_send"
    );

    let rx = receiver(&bed, &fx, "acme").await;
    set_policy(&fx, rx.tenant, Policy::AutoSend).await;
    deliver(&fx, &rx.address, Some(CUSTOMER)).await;
    let job = seeded_run(&fx, rx.tenant).await;
    let link = investigate(&fx, ctx(rx.owner, rx.tenant), job).await;

    let sent = fx.sent();
    assert_eq!(sent.len(), 1, "exactly the opted-in tenant's reply");
    assert_eq!(sent[0].to, CUSTOMER);
    assert_eq!(sent[0].text_body, DRAFT);
    assert_eq!(
        sent[0].threading.in_reply_to.as_deref(),
        Some(MESSAGE_ID),
        "an automatic reply is threaded like any other"
    );
    assert_eq!(
        link.reply_recipient.as_deref(),
        Some(CUSTOMER),
        "the investigation's own answer already carries the receipt"
    );

    // A re-reported investigation is a second reading of one message, not a
    // second message: the customer is not answered twice.
    investigate(&fx, ctx(rx.owner, rx.tenant), job).await;
    assert_eq!(fx.sent().len(), 1, "the chain still sent exactly one reply");

    bed.teardown().await;
}

/// A chain whose forward carried no `Reply-To` has no customer to answer, and
/// the customer-facing mode says so rather than falling back to the staffer —
/// which would send a reply meant for a customer to somebody else.
#[tokio::test]
async fn a_chain_with_no_reply_address_refuses_the_customer_facing_mode() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let fx = fixture(&bed).await;
    let rx = drafted(&bed, &fx, "acme", None).await;
    set_policy(&fx, rx.tenant, Policy::ApproveThenSend).await;
    let id = link_id(&fx, rx.tenant).await;

    let refused = approve(&fx, ctx(rx.owner, rx.tenant), id, None)
        .await
        .map(|_| ())
        .expect_err("there is nobody to answer");
    assert_eq!(status_of(refused), StatusCode::CONFLICT);
    assert!(fx.sent().is_empty(), "and nothing was sent to anybody");

    // The same chain under the default policy still works: the staffer is
    // recorded whether or not the forward named a customer.
    set_policy(&fx, rx.tenant, Policy::ToStaffer).await;
    let link = approve(&fx, ctx(rx.owner, rx.tenant), id, None)
        .await
        .expect("the staffer can still be answered");
    assert_eq!(link.reply_recipient.as_deref(), Some(STAFF));

    bed.teardown().await;
}

/// One reply per chain: a second approve is refused, so a customer is never
/// sent the same answer twice.
#[tokio::test]
async fn a_chain_sends_its_reply_exactly_once() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let fx = fixture(&bed).await;
    let rx = drafted(&bed, &fx, "acme", Some(CUSTOMER)).await;
    set_policy(&fx, rx.tenant, Policy::ApproveThenSend).await;
    let id = link_id(&fx, rx.tenant).await;
    let who = ctx(rx.owner, rx.tenant);

    approve(&fx, who, id, None).await.expect("the first send");
    let refused = approve(&fx, who, id, None)
        .await
        .map(|_| ())
        .expect_err("the second is refused");
    assert_eq!(status_of(refused), StatusCode::CONFLICT);
    assert_eq!(fx.sent().len(), 1, "one message reached the customer");

    bed.teardown().await;
}

/// A human who edits the draft before approving sends — and records — the text
/// they actually approved.
#[tokio::test]
async fn an_edited_draft_is_what_gets_sent_and_what_the_chain_keeps() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let fx = fixture(&bed).await;
    let rx = drafted(&bed, &fx, "acme", Some(CUSTOMER)).await;
    let id = link_id(&fx, rx.tenant).await;
    const EDITED: &str = "Hi — we found the cause and a fix ships this week.";

    approve(&fx, ctx(rx.owner, rx.tenant), id, Some(EDITED))
        .await
        .expect("the staffer approves their own wording");
    assert_eq!(fx.sent()[0].text_body, EDITED);

    // And it is the edit that is sealed on the chain, not the run's original.
    let sealed = fx
        .state
        .db
        .query_scalar::<Vec<u8>>(
            "SELECT draft_reply_enc FROM email_links WHERE tenant_id = $1",
            params![rx.tenant],
        )
        .await
        .expect("a draft is stored");
    assert_eq!(
        fx.state.vault.decrypt_string(&sealed).expect("unseal"),
        EDITED
    );

    bed.teardown().await;
}

/// HC-3's second gate is a PERSON. A machine token is refused, so the run that
/// drafted the reply cannot approve it.
#[tokio::test]
async fn a_machine_token_cannot_approve_a_reply() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let fx = fixture(&bed).await;
    let rx = drafted(&bed, &fx, "acme", Some(CUSTOMER)).await;
    let id = link_id(&fx, rx.tenant).await;

    let machine = AuthCtx {
        principal: Principal::Node(NodeId(Uuid::now_v7())),
        cookie_session: false,
        ..ctx(rx.owner, rx.tenant)
    };
    let refused = approve(&fx, machine, id, None)
        .await
        .map(|_| ())
        .expect_err("a node token does not approve customer mail");
    assert_eq!(status_of(refused), StatusCode::FORBIDDEN);
    assert!(fx.sent().is_empty());

    bed.teardown().await;
}

/// A chain reaches only its own tenant: a link id from elsewhere is not an
/// authorisation to answer somebody else's customer.
#[tokio::test]
async fn a_reply_cannot_be_sent_on_another_tenants_chain() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let fx = fixture(&bed).await;
    let a = drafted(&bed, &fx, "acme", Some(CUSTOMER)).await;
    let b = receiver(&bed, &fx, "beta").await;
    let a_link = link_id(&fx, a.tenant).await;

    let refused = approve(&fx, ctx(b.owner, b.tenant), a_link, None)
        .await
        .map(|_| ())
        .expect_err("A's chain is not B's");
    assert_eq!(status_of(refused), StatusCode::NOT_FOUND);
    assert!(fx.sent().is_empty());

    bed.teardown().await;
}
