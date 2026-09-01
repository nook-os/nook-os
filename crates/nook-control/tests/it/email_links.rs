//! The email <-> ticket <-> job <-> PR chain and its read model (MAIN-330).
//!
//! The chain is written by C1's pipeline, so every link here comes from a real
//! delivery through the real receiver route rather than from a hand-inserted
//! row: what is under test is that the *pipeline* records the chain, and a test
//! that wrote its own link would pass with the pipeline's write deleted.
//!
//! The reads are driven as handlers, as the rest of this suite does.
//!
//! Nothing here is Postgres-shaped, so it runs on both engines.

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{Request, StatusCode};
use axum::Json;
use axum::Router;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::repo::admin::SettingWrite;
use nook_control::repo::tasks::{DbTaskRepository, TaskRepository};
use nook_control::routes::build_router;
use nook_control::routes::email_links::{list, lookup, ListFilter, LookupFilter};
use nook_control::services::{email_inbound as inbound, notify};
use nook_control::AppState;
use nook_db::{params, Db, FromDbRow};
use nook_testkit::TestBed;
use nook_types::*;
use tower::ServiceExt;
use uuid::Uuid;

const SECRET: &str = "inbound-secret-inbound-secret";
const STAFF: &str = "reporter@acme.example";

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

/// A private disk root per test, so one test's sealed objects are never
/// another's.
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

/// One tenant that receives support mail: an owner (so the investigate run is
/// requested by somebody), a board with the columns the pipeline and
/// `submit_pr` need, and the `email.inbound` setting naming the address.
struct Receiver {
    tenant: TenantId,
    owner: UserId,
    address: String,
}

async fn fixture(bed: &TestBed) -> Fixture {
    let scratch =
        Scratch(std::env::temp_dir().join(format!("nook-links-{}", Uuid::now_v7().simple())));
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
    // `submit_pr` parks the card here; without it the chain's PR test would be
    // exercising the fallback rather than the ordinary path.
    repo.create_column(board.id, "In Review", 1, "review")
        .await
        .expect("review column");

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

/// One provider inbound-parse body, threaded to `in_reply_to` when given.
fn payload(to: &str, message_id: &str, in_reply_to: Option<&str>) -> String {
    let mut headers = format!("Message-Id: {message_id}");
    if let Some(parent) = in_reply_to {
        headers.push_str(&format!("\nIn-Reply-To: {parent}"));
    }
    serde_json::json!({
        "envelope": { "from": STAFF, "to": [to] },
        "subject": "the login page 500s",
        "text": "it 500s when I submit the form",
        "spf": "pass",
        "headers": headers,
    })
    .to_string()
}

/// Deliver one message and assert it was filed.
async fn deliver(fx: &Fixture, to: &str, message_id: &str, in_reply_to: Option<&str>) {
    let body = payload(to, message_id, in_reply_to);
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
    let ack = axum::body::to_bytes(res.into_body(), 64 * 1024)
        .await
        .unwrap_or_default();
    let ack = String::from_utf8_lossy(&ack).to_string();
    assert!(ack.contains("\"status\":\"filed\""), "{ack}");
}

/// One link row as the table holds it — read straight from the table, so an
/// assertion about what the PIPELINE wrote does not go through the read model
/// it is meant to be independent of.
#[derive(Debug, FromDbRow)]
struct LinkRow {
    task_id: TaskId,
    loop_job_id: Option<JobId>,
    pr_ref: Option<String>,
    message_id: Option<String>,
    in_reply_to: Option<String>,
    storage_key: String,
}

async fn rows(fx: &Fixture, tenant: TenantId) -> Vec<LinkRow> {
    fx.state
        .db
        .query_all(
            "SELECT task_id, loop_job_id, pr_ref, message_id, in_reply_to, storage_key
               FROM email_links WHERE tenant_id = $1 ORDER BY created_at, id",
            params![tenant],
        )
        .await
        .expect("read the links")
}

async fn links_of(fx: &Fixture, who: AuthCtx, workspace: Option<Uuid>) -> Vec<EmailLink> {
    list(
        State(fx.state.clone()),
        who,
        Query(ListFilter {
            workspace_id: workspace,
        }),
    )
    .await
    .map(|Json(v)| v)
    .expect("list the chains")
}

async fn look_up(
    fx: &Fixture,
    who: AuthCtx,
    filter: LookupFilter,
) -> Result<EmailLink, nook_control::error::ApiError> {
    lookup(State(fx.state.clone()), who, Query(filter))
        .await
        .map(|Json(l)| l)
}

fn by_message(message_id: &str) -> LookupFilter {
    LookupFilter {
        message_id: Some(message_id.into()),
        task: None,
    }
}

fn by_task(task: TaskId) -> LookupFilter {
    LookupFilter {
        message_id: None,
        task: Some(task.0.to_string()),
    }
}

/// AC-1/AC-2: one delivery leaves one chain, and it names the card, the run and
/// the sealed object the pipeline actually produced.
#[tokio::test]
async fn an_accepted_delivery_chains_its_message_to_the_ticket_the_run_and_the_sealed_object() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let fx = fixture(&bed).await;
    let rx = receiver(&bed, &fx, "acme").await;

    deliver(
        &fx,
        &rx.address,
        "<m1@acme.example>",
        Some("<parent@acme.example>"),
    )
    .await;

    let rows = rows(&fx, rx.tenant).await;
    assert_eq!(rows.len(), 1, "one delivery, one chain: {rows:?}");
    let link = &rows[0];

    let card: TaskId = fx
        .state
        .db
        .query_scalar(
            "SELECT id FROM tasks WHERE tenant_id = $1",
            params![rx.tenant],
        )
        .await
        .expect("the filed card");
    assert_eq!(
        link.task_id, card,
        "the chain names the card the message became"
    );

    let run: JobId = fx
        .state
        .db
        .query_scalar(
            "SELECT id FROM loop_jobs WHERE tenant_id = $1",
            params![rx.tenant],
        )
        .await
        .expect("the seeded run");
    assert_eq!(
        link.loop_job_id,
        Some(run),
        "the chain names the investigate run seeded for the card"
    );

    assert_eq!(link.message_id.as_deref(), Some("<m1@acme.example>"));
    assert_eq!(link.in_reply_to.as_deref(), Some("<parent@acme.example>"));
    assert!(link.pr_ref.is_none(), "no PR has been opened yet");
    assert!(
        fx.state
            .user_content_store
            .get(&link.storage_key)
            .await
            .is_ok_and(|o| !o.is_empty()),
        "storage_key addresses the sealed original that is really there"
    );

    bed.teardown().await;
}

/// AC-3: the two questions the surfaces above ask — which chain is this
/// `Message-Id` (C4), and what has arrived (C7).
#[tokio::test]
async fn the_read_model_finds_a_chain_by_message_id_or_ticket_and_lists_them_newest_first() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let fx = fixture(&bed).await;
    let rx = receiver(&bed, &fx, "acme").await;
    let who = ctx(rx.owner, rx.tenant);

    deliver(&fx, &rx.address, "<first@acme.example>", None).await;
    deliver(&fx, &rx.address, "<second@acme.example>", None).await;

    let listed = links_of(&fx, who, None).await;
    assert_eq!(listed.len(), 2, "both chains: {listed:?}");
    assert_eq!(
        listed[0].message_id.as_deref(),
        Some("<second@acme.example>"),
        "newest first"
    );

    let one = look_up(&fx, who, by_message("<first@acme.example>"))
        .await
        .expect("found by message id");
    assert_eq!(one.message_id.as_deref(), Some("<first@acme.example>"));

    let same = look_up(&fx, who, by_task(one.task_id))
        .await
        .expect("found by ticket");
    assert_eq!(same.id, one.id, "the same chain, reached the other way");

    assert_eq!(
        status_of(
            look_up(&fx, who, by_message("<never-sent@acme.example>"))
                .await
                .map(|_| ())
                .expect_err("nothing was sent with that id")
        ),
        StatusCode::NOT_FOUND,
    );

    // Two selectors answer two questions, and a caller could not tell which one
    // it got back.
    assert_eq!(
        status_of(
            look_up(
                &fx,
                who,
                LookupFilter {
                    message_id: Some("<first@acme.example>".into()),
                    task: Some(one.task_id.0.to_string()),
                },
            )
            .await
            .map(|_| ())
            .expect_err("two selectors are one too many")
        ),
        StatusCode::BAD_REQUEST,
    );

    bed.teardown().await;
}

/// AC-4: a chain belongs to its tenant, by every door there is. A uuid or a
/// `Message-Id` from one tenant finds nothing in another.
#[tokio::test]
async fn a_chain_is_invisible_from_another_tenant() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let fx = fixture(&bed).await;
    let a = receiver(&bed, &fx, "acme").await;
    let b = receiver(&bed, &fx, "beta").await;

    deliver(&fx, &a.address, "<a1@acme.example>", None).await;
    deliver(&fx, &b.address, "<b1@beta.example>", None).await;

    let a_link = look_up(&fx, ctx(a.owner, a.tenant), by_message("<a1@acme.example>"))
        .await
        .expect("A sees its own chain");

    let intruder = ctx(b.owner, b.tenant);
    assert_eq!(
        status_of(
            look_up(&fx, intruder, by_message("<a1@acme.example>"))
                .await
                .map(|_| ())
                .expect_err("A's chain is not B's")
        ),
        StatusCode::NOT_FOUND,
        "A's Message-Id is not B's to look up"
    );
    assert_eq!(
        status_of(
            look_up(&fx, intruder, by_task(a_link.task_id))
                .await
                .map(|_| ())
                .expect_err("A's ticket is not B's")
        ),
        StatusCode::NOT_FOUND,
        "A's ticket uuid is not an authorisation in B"
    );

    let bs = links_of(&fx, intruder, None).await;
    assert_eq!(bs.len(), 1, "B lists only its own: {bs:?}");
    assert_eq!(bs[0].message_id.as_deref(), Some("<b1@beta.example>"));

    // And below the routes: the query itself is what refuses, so a future
    // caller that forgets a visibility check still cannot cross the boundary.
    assert!(fx
        .state
        .email_links
        .by_task(b.tenant, b.owner, a_link.task_id)
        .await
        .expect("query")
        .is_none());

    bed.teardown().await;
}

/// AC-2's later stage: submitting a PR against the card completes its chain.
#[tokio::test]
async fn submitting_a_pr_against_the_card_records_it_on_the_chain() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let fx = fixture(&bed).await;
    let rx = receiver(&bed, &fx, "acme").await;
    let who = ctx(rx.owner, rx.tenant);

    deliver(&fx, &rx.address, "<m1@acme.example>", None).await;
    let link = look_up(&fx, who, by_message("<m1@acme.example>"))
        .await
        .expect("the chain");
    assert!(link.pr_ref.is_none());

    // `submit_pr` refuses a card with no branch, and the pipeline files one
    // before any work starts.
    fx.state
        .db
        .exec(
            "UPDATE tasks SET branch = 'main-1-fix' WHERE id = $1",
            params![link.task_id],
        )
        .await
        .expect("branch the card");

    nook_control::services::taskwork::submit_pr(
        &fx.state,
        rx.tenant,
        link.task_id,
        Some("https://github.com/acme/app/pull/7".into()),
    )
    .await
    .expect("submit the PR");

    let after = look_up(&fx, who, by_message("<m1@acme.example>"))
        .await
        .expect("the chain");
    assert_eq!(
        after.pr_ref.as_deref(),
        Some("https://github.com/acme/app/pull/7"),
        "the chain reaches the fix"
    );
    assert_eq!(
        after.id, link.id,
        "the same chain, extended rather than replaced"
    );

    bed.teardown().await;
}
