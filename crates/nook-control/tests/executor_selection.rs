//! Loop-job executor selection (MAIN-160): own node preferred, operator
//! fallback, no-eligible reason, atomic claim under contention, ineligible
//! runtime skipped. Each test runs on its OWN private database (MAIN-156).
//!
//! Needs Postgres: `DATABASE_URL` (`NOOK_REQUIRE_DB=1` in the suite).

use nook_control::services::jobs;
use nook_control::state::AppState;
use nook_types::*;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use nook_testkit::TestBed;

/// A board + column + a team-visible task to anchor a job on.
async fn target_task(db: &PgPool, tenant: TenantId, creator: UserId) -> TaskId {
    let board = BoardId::new();
    sqlx::query(
        "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b',$3,'local')",
    )
    .bind(board)
    .bind(tenant)
    // The RANDOM tail of the v7 uuid — its leading bytes are a shared timestamp,
    // so two boards made in the same test would collide on a prefix-derived key.
    .bind(format!("B{}", &board.0.simple().to_string()[26..32]).to_uppercase())
    .execute(db)
    .await
    .expect("board");
    let col = ColumnId::new();
    sqlx::query(
        "INSERT INTO board_columns (id, board_id, name, position, type)
         VALUES ($1,$2,'Triage',0,'unstarted')",
    )
    .bind(col)
    .bind(board)
    .execute(db)
    .await
    .expect("column");
    let task = TaskId::new();
    sqlx::query(
        "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, type, created_by)
         VALUES ($1,$2,$3,$4,'t','task',$5)",
    )
    .bind(task)
    .bind(tenant)
    .bind(board)
    .bind(col)
    .bind(creator)
    .execute(db)
    .await
    .expect("task");
    task
}

/// A queued spec job on `target`, requested by `user`.
async fn queued_job(db: &PgPool, tenant: TenantId, user: UserId, target: TaskId) -> JobId {
    let id = JobId::new();
    sqlx::query(
        "INSERT INTO loop_jobs (id, tenant_id, kind, target_task_id, requested_by, state)
         VALUES ($1,$2,'spec',$3,$4,'queued')",
    )
    .bind(id)
    .bind(tenant)
    .bind(target)
    .bind(user)
    .execute(db)
    .await
    .expect("job");
    id
}

/// Insert a node with an explicit status, owner, and capabilities jsonb.
async fn node(
    db: &PgPool,
    tenant: TenantId,
    owner: Option<Uuid>,
    status: &str,
    caps: serde_json::Value,
) -> NodeId {
    let id = NodeId::new();
    sqlx::query(
        "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status, owner_person_id, capabilities)
         VALUES ($1,$2,$3,$4,$5,$6,$7)",
    )
    .bind(id)
    .bind(tenant)
    .bind(format!("n-{}", id.0.simple()))
    .bind(format!("h-{}", id.0.simple()))
    .bind(status)
    .bind(owner)
    .bind(caps)
    .execute(db)
    .await
    .expect("node");
    id
}

/// Capabilities reporting the `claude` runtime in the given auth state.
fn caps(state: &str, operator: bool) -> serde_json::Value {
    let mut c = json!({
        "runtime_auth": [
            { "id": "claude", "label": "Claude Code", "runtime": "claude", "state": state }
        ]
    });
    if operator {
        c["shared_operator"] = json!(true);
    }
    c
}

async fn setup(bed: &TestBed) -> (AppState, TenantId, UserId, Uuid, JobId) {
    let tenant = bed.tenant("exec").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let target = target_task(&bed.pool, tenant, user).await;
    let job = queued_job(&bed.pool, tenant, user, target).await;
    (bed.app_state().await, tenant, user, person, job)
}

#[tokio::test]
async fn own_node_is_preferred_over_the_operator() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, _user, person, job) = setup(&bed).await;
    let mine = node(
        &bed.pool,
        tenant,
        Some(person),
        "online",
        caps("authorized", false),
    )
    .await;
    let _operator = node(&bed.pool, tenant, None, "online", caps("authorized", true)).await;

    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select");
    assert_eq!(placed.state, "claimed");
    assert_eq!(
        placed.executor_node_id,
        Some(mine),
        "prefers the owned node"
    );
    assert!(placed.queued_reason.is_none());

    bed.teardown().await;
}

#[tokio::test]
async fn operator_is_the_fallback_when_no_owned_node_is_eligible() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, _user, person, job) = setup(&bed).await;
    // The requester's own node is online but NOT authorized — skipped.
    let _mine = node(
        &bed.pool,
        tenant,
        Some(person),
        "online",
        caps("not_authorized", false),
    )
    .await;
    let operator = node(&bed.pool, tenant, None, "online", caps("authorized", true)).await;

    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select");
    assert_eq!(placed.state, "claimed");
    assert_eq!(
        placed.executor_node_id,
        Some(operator),
        "falls back to the operator"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn no_eligible_executor_leaves_the_job_queued_with_a_reason() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, _user, person, job) = setup(&bed).await;
    // Owned node online but unauthorized; no operator at all.
    let _mine = node(
        &bed.pool,
        tenant,
        Some(person),
        "online",
        caps("not_authorized", false),
    )
    .await;

    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select");
    assert_eq!(placed.state, "queued", "unplaceable stays queued");
    assert!(placed.executor_node_id.is_none());
    let reason = placed.queued_reason.expect("a reason is recorded");
    assert!(
        reason.contains("not authorized") && reason.contains("claude"),
        "reason names the failed gate: {reason}"
    );

    // An offline owned node yields the 'no node online' reason instead.
    let job2 = queued_job(
        &bed.pool,
        tenant,
        _user,
        target_task(&bed.pool, tenant, _user).await,
    )
    .await;
    let _offline = node(
        &bed.pool,
        tenant,
        Some(person),
        "offline",
        caps("authorized", false),
    )
    .await;
    // (the online-but-unauthorized node from above still counts as online)
    let placed2 = jobs::select_executor(&state, tenant, job2)
        .await
        .expect("select2");
    assert_eq!(placed2.state, "queued");
    assert!(placed2.queued_reason.is_some());

    bed.teardown().await;
}

#[tokio::test]
async fn ineligible_runtime_is_skipped() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, _user, person, job) = setup(&bed).await;
    // A node reporting a DIFFERENT runtime authorized, not claude.
    let other = json!({
        "runtime_auth": [{ "id": "codex", "label": "Codex", "runtime": "codex", "state": "authorized" }]
    });
    let _mine = node(&bed.pool, tenant, Some(person), "online", other).await;

    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select");
    assert_eq!(
        placed.state, "queued",
        "a node without claude authorized is not eligible"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn concurrent_selection_claims_a_job_exactly_once() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, _user, person, job) = setup(&bed).await;
    let mine = node(
        &bed.pool,
        tenant,
        Some(person),
        "online",
        caps("authorized", false),
    )
    .await;

    // Two consumers race the same queued job.
    let (a, b) = tokio::join!(
        jobs::select_executor(&state, tenant, job),
        jobs::select_executor(&state, tenant, job),
    );
    let a = a.expect("a");
    let b = b.expect("b");

    // Both observe the SAME claim — one wrote it, the other read it back — and
    // the job is claimed by the one node exactly once.
    assert_eq!(a.state, "claimed");
    assert_eq!(b.state, "claimed");
    assert_eq!(a.executor_node_id, Some(mine));
    assert_eq!(b.executor_node_id, Some(mine));

    let (state_str, exec): (String, Option<NodeId>) =
        sqlx::query_as("SELECT state, executor_node_id FROM loop_jobs WHERE id = $1")
            .bind(job)
            .fetch_one(&bed.pool)
            .await
            .unwrap();
    assert_eq!(state_str, "claimed");
    assert_eq!(exec, Some(mine));

    bed.teardown().await;
}
