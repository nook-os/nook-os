//! What the read-only investigate run produces, and where it lands (MAIN-331).
//!
//! Every chain here comes from a real delivery through the real receiver route,
//! for the reason `email_links.rs` gives: what is under test is the pipeline's
//! own record, and a test that inserted its own link would pass with the
//! pipeline's write deleted.
//!
//! The two writes worth being paranoid about are asserted against the TABLE and
//! against the object store, not against the read model: "stored encrypted" is
//! a claim about the bytes at rest, and a read model that decrypts on the way
//! out would report it true either way.
//!
//! Nothing here is Postgres-shaped, so it runs on both engines.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{Request, StatusCode};
use axum::Json;
use axum::Router;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::repo::admin::SettingWrite;
use nook_control::repo::tasks::{DbTaskRepository, TaskRepository};
use nook_control::routes::build_router;
use nook_control::routes::jobs::{email_message, investigation};
use nook_control::services::{email_inbound as inbound, notify};
use nook_control::AppState;
use nook_db::{params, Db, FromDbRow};
use nook_testkit::TestBed;
use nook_types::*;
use tower::ServiceExt;
use uuid::Uuid;

const SECRET: &str = "inbound-secret-inbound-secret";
const STAFF: &str = "reporter@acme.example";
/// A distinctive line only the RAW message carries — the card quotes a
/// truncated excerpt, so finding this in what a run reads proves the run got
/// the original rather than the summary.
const RAW_MARKER: &str = "X-Reporter-Trace: 8f21c";

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
    _scratch: Scratch,
}

struct Receiver {
    tenant: TenantId,
    owner: UserId,
    address: String,
}

async fn fixture(bed: &TestBed) -> Fixture {
    let scratch =
        Scratch(std::env::temp_dir().join(format!("nook-investig-{}", Uuid::now_v7().simple())));
    let mut cfg = bed.config();
    cfg.user_content_dir = scratch.0.to_string_lossy().into_owned();
    cfg.email_inbound_secret = Some(SECRET.to_string());
    let state = AppState::new(bed.db(), cfg, None).await;
    Fixture {
        app: build_router(state.clone()),
        state,
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

/// Deliver one message and assert it was filed.
async fn deliver(fx: &Fixture, to: &str, message_id: &str) {
    let body = serde_json::json!({
        "envelope": { "from": STAFF, "to": [to] },
        "subject": "the login page 500s",
        "text": "it 500s when I submit the form",
        "spf": "pass",
        "headers": format!("Message-Id: {message_id}\n{RAW_MARKER}"),
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

/// The chain as the TABLE holds it, ciphertext included — the only honest place
/// to assert what is at rest.
#[derive(Debug, FromDbRow)]
struct LinkRow {
    id: Uuid,
    task_id: TaskId,
    loop_job_id: Option<JobId>,
    pr_ref: Option<String>,
    findings: Option<String>,
    draft_reply_enc: Option<Vec<u8>>,
}

async fn row(fx: &Fixture, tenant: TenantId) -> LinkRow {
    fx.state
        .db
        .query_one(
            "SELECT id, task_id, loop_job_id, pr_ref, findings, draft_reply_enc
               FROM email_links WHERE tenant_id = $1",
            params![tenant],
        )
        .await
        .expect("the one chain")
}

/// The run seeded for the delivery. Every test here acts AS the run, which is
/// the only caller these two routes have.
async fn seeded_run(fx: &Fixture, tenant: TenantId) -> JobId {
    row(fx, tenant).await.loop_job_id.expect("a run was seeded")
}

async fn report(
    fx: &Fixture,
    who: AuthCtx,
    job: JobId,
    findings: &str,
    draft: &str,
) -> Result<EmailLink, nook_control::error::ApiError> {
    investigation(
        State(fx.state.clone()),
        who,
        Path(job),
        Json(InvestigationReport {
            findings: findings.into(),
            draft_reply: draft.into(),
        }),
    )
    .await
    .map(|Json(l)| l)
}

async fn read_message(
    fx: &Fixture,
    who: AuthCtx,
    job: JobId,
) -> Result<DecryptedMessage, nook_control::error::ApiError> {
    email_message(State(fx.state.clone()), who, Path(job))
        .await
        .map(|Json(m)| m)
}

const FINDINGS: &str = "Reproduced: yes. auth.rs:212 unwraps before the empty-field check.";
const DRAFT: &str = "Hi — we reproduced the 500 you hit on submit. A fix is being scoped.";

/// AC-2: the run's two halves land on the chain, and the draft is ciphertext at
/// rest.
#[tokio::test]
async fn an_investigation_records_findings_and_a_sealed_draft_reply() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let fx = fixture(&bed).await;
    let rx = receiver(&bed, &fx, "acme").await;
    deliver(&fx, &rx.address, "<m1@acme.example>").await;
    let job = seeded_run(&fx, rx.tenant).await;

    let link = report(&fx, ctx(rx.owner, rx.tenant), job, FINDINGS, DRAFT)
        .await
        .expect("the run reports");
    assert_eq!(link.findings.as_deref(), Some(FINDINGS));
    assert!(
        link.has_draft_reply,
        "the read model says a draft exists without carrying it"
    );

    let stored = row(&fx, rx.tenant).await;
    assert_eq!(stored.findings.as_deref(), Some(FINDINGS));
    let sealed = stored.draft_reply_enc.expect("a draft was stored");
    assert!(
        !String::from_utf8_lossy(&sealed).contains("we reproduced"),
        "the draft must not be readable in the table"
    );
    assert_ne!(
        sealed,
        DRAFT.as_bytes(),
        "stored bytes are the ciphertext, not the reply"
    );
    assert_eq!(
        fx.state
            .vault
            .decrypt_string(&sealed)
            .expect("the vault unseals what it sealed"),
        DRAFT,
        "and it is genuinely this draft, sealed"
    );

    bed.teardown().await;
}

/// AC-4: the run reads the original by decryption, and the object store holds
/// only ciphertext.
#[tokio::test]
async fn the_run_reads_the_original_message_decrypted() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let fx = fixture(&bed).await;
    let rx = receiver(&bed, &fx, "acme").await;
    deliver(&fx, &rx.address, "<m1@acme.example>").await;
    let job = seeded_run(&fx, rx.tenant).await;

    let msg = read_message(&fx, ctx(rx.owner, rx.tenant), job)
        .await
        .expect("the run reads its message");
    assert!(
        msg.message.contains(RAW_MARKER),
        "the whole original, not the card's excerpt: {}",
        msg.message
    );
    assert!(msg.message.contains("it 500s when I submit the form"));

    // What the decryption was FOR: the bytes on disk say none of that.
    let key = fx
        .state
        .db
        .query_scalar::<String>(
            "SELECT storage_key FROM email_links WHERE tenant_id = $1",
            params![rx.tenant],
        )
        .await
        .expect("the sealed object's key");
    let at_rest = fx
        .state
        .user_content_store
        .get(&key)
        .await
        .expect("the object is there");
    assert!(
        !String::from_utf8_lossy(&at_rest).contains(RAW_MARKER),
        "the stored original is sealed, not plaintext (HC-4)"
    );

    bed.teardown().await;
}

/// AC-3: the run writes no PR, and no other kind may use this surface at all.
///
/// The kind check is what keeps the one plaintext route in the deployment
/// confined: a `spec` or `build` run holds a token of exactly the same shape,
/// so without it any loop job could ask for any of its tenant's support mail.
#[tokio::test]
async fn only_an_investigate_run_may_read_or_report_and_it_records_no_pr() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let fx = fixture(&bed).await;
    let rx = receiver(&bed, &fx, "acme").await;
    let who = ctx(rx.owner, rx.tenant);
    deliver(&fx, &rx.address, "<m1@acme.example>").await;
    let chain = row(&fx, rx.tenant).await;
    let job = chain.loop_job_id.expect("a run was seeded");

    report(&fx, who, job, FINDINGS, DRAFT)
        .await
        .expect("the run reports");
    assert!(
        row(&fx, rx.tenant).await.pr_ref.is_none(),
        "an investigation opens no PR, so the chain still names none"
    );

    // And it cannot report one either: the outcome call belongs to the kind
    // that writes code.
    let refused = nook_control::services::jobs::record_build_outcome(
        &fx.state,
        rx.tenant,
        job,
        &BuildOutcomeRequest {
            outcome: "pr_opened".into(),
            url: Some("https://github.com/acme/app/pull/7".into()),
            question: None,
        },
    )
    .await
    .map(|_| ())
    .expect_err("an investigate run has no build outcome to report");
    assert_eq!(status_of(refused), StatusCode::BAD_REQUEST);

    // A run of another kind, in the same tenant, holding the same shape of
    // token: refused by kind, at both doors.
    let other = fx
        .state
        .jobs
        .create(nook_control::repo::jobs::NewLoopJob {
            id: JobId::new(),
            tenant: rx.tenant,
            kind: "spec".into(),
            target_task_id: Some(chain.task_id),
            workspace_id: None,
            requested_by: rx.owner,
            seed: None,
            predecessor_job_id: None,
            review_forced: false,
            review_pr_number: None,
            review_head_sha: None,
            build_fingerprint: None,
        })
        .await
        .expect("a spec run");
    assert_eq!(
        status_of(
            read_message(&fx, who, other.id)
                .await
                .map(|_| ())
                .expect_err("a spec run has no support mail to read")
        ),
        StatusCode::BAD_REQUEST,
    );
    assert_eq!(
        status_of(
            report(&fx, who, other.id, FINDINGS, DRAFT)
                .await
                .map(|_| ())
                .expect_err("nor an investigation to report")
        ),
        StatusCode::BAD_REQUEST,
    );

    bed.teardown().await;
}

/// A report is both halves or neither: the staffer is owed a "why" AND
/// something to send, and half a report on the record reads like a whole one.
#[tokio::test]
async fn a_report_missing_either_half_is_refused_and_records_nothing() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let fx = fixture(&bed).await;
    let rx = receiver(&bed, &fx, "acme").await;
    let who = ctx(rx.owner, rx.tenant);
    deliver(&fx, &rx.address, "<m1@acme.example>").await;
    let job = seeded_run(&fx, rx.tenant).await;

    for (findings, draft) in [(FINDINGS, "   "), ("", DRAFT)] {
        assert_eq!(
            status_of(
                report(&fx, who, job, findings, draft)
                    .await
                    .map(|_| ())
                    .expect_err("half a report is not a report")
            ),
            StatusCode::BAD_REQUEST,
        );
    }
    let stored = row(&fx, rx.tenant).await;
    assert!(stored.findings.is_none(), "and nothing was recorded");
    assert!(stored.draft_reply_enc.is_none());

    bed.teardown().await;
}

/// A run reaches its OWN chain and nothing else: the job id is the whole
/// address, so another tenant's run — and another delivery's — is out of reach.
#[tokio::test]
async fn a_run_reaches_only_the_chain_it_was_seeded_from() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let fx = fixture(&bed).await;
    let a = receiver(&bed, &fx, "acme").await;
    let b = receiver(&bed, &fx, "beta").await;
    deliver(&fx, &a.address, "<a1@acme.example>").await;
    deliver(&fx, &b.address, "<b1@beta.example>").await;

    let a_job = seeded_run(&fx, a.tenant).await;
    let intruder = ctx(b.owner, b.tenant);

    assert_eq!(
        status_of(
            read_message(&fx, intruder, a_job)
                .await
                .map(|_| ())
                .expect_err("A's run is not B's")
        ),
        StatusCode::NOT_FOUND,
        "a job id from another tenant is not an authorisation",
    );
    assert_eq!(
        status_of(
            report(&fx, intruder, a_job, FINDINGS, DRAFT)
                .await
                .map(|_| ())
                .expect_err("nor may B write onto A's chain")
        ),
        StatusCode::NOT_FOUND,
    );
    assert!(
        row(&fx, a.tenant).await.findings.is_none(),
        "A's chain is untouched"
    );

    // B's own run reports, and only B's chain moves.
    let b_job = seeded_run(&fx, b.tenant).await;
    report(&fx, intruder, b_job, FINDINGS, DRAFT)
        .await
        .expect("B's run reports onto B's chain");
    assert!(row(&fx, b.tenant).await.findings.is_some());
    assert!(row(&fx, a.tenant).await.findings.is_none());

    bed.teardown().await;
}

/// The report replaces itself rather than accumulating: a repair pass over the
/// same card is a second reading of one message, not a second message.
#[tokio::test]
async fn a_second_report_replaces_the_first() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let fx = fixture(&bed).await;
    let rx = receiver(&bed, &fx, "acme").await;
    let who = ctx(rx.owner, rx.tenant);
    deliver(&fx, &rx.address, "<m1@acme.example>").await;
    let job = seeded_run(&fx, rx.tenant).await;
    let first = row(&fx, rx.tenant).await.id;

    report(&fx, who, job, FINDINGS, DRAFT).await.expect("first");
    let link = report(
        &fx,
        who,
        job,
        "Reproduced: no.",
        "Hi — we could not reproduce it.",
    )
    .await
    .expect("second");

    assert_eq!(
        link.id, first,
        "the same chain, rewritten rather than added"
    );
    let stored = row(&fx, rx.tenant).await;
    assert_eq!(stored.findings.as_deref(), Some("Reproduced: no."));
    assert_eq!(
        fx.state
            .vault
            .decrypt_string(&stored.draft_reply_enc.expect("a draft"))
            .expect("unseal"),
        "Hi — we could not reproduce it.",
    );

    bed.teardown().await;
}
