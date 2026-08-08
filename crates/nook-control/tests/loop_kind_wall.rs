//! The build-loop wall on the task-claim route (MAIN-142 AC-4).
//!
//! A build that runs as a person or an agent typing `nook claim` inside a
//! session never becomes a job, so the executor wall cannot catch it (the
//! `build` job KIND has its own wall — `kind_wall_refusal`, tested in
//! executor_selection). So here the wall is applied at the claim, against the
//! node the claiming session actually runs on.
//!
//! The third case is the one worth having: a claim with NO session context is
//! deliberately out of reach. The control plane cannot tell where it came from,
//! and refusing every context-less claim would break every human on the board.
//!
//! Needs Postgres: `DATABASE_URL` (`NOOK_REQUIRE_DB=1` in the suite).

use axum::extract::{Path, State};
use axum::Json;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::error::ApiError;
use nook_control::routes::task_query::claim;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

fn user_ctx(user: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: false,
    }
}

/// A board with a Todo column and one claimable task on it.
async fn claimable(bed: &TestBed, tenant: TenantId, creator: UserId) -> TaskId {
    let board = BoardId::new();
    bed.db()
        .exec(
            "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b',$3,'local')",
            params![
                board,
                tenant,
                format!("B{}", &board.0.simple().to_string()[26..32]).to_uppercase()
            ],
        )
        .await
        .expect("board");
    for (name, kind, pos) in [("Todo", "unstarted", 0), ("In Progress", "started", 1)] {
        bed.db()
            .exec(
                "INSERT INTO board_columns (id, board_id, name, position, type)
                 VALUES ($1,$2,$3,$4,$5)",
                params![
                    ColumnId::new(),
                    board,
                    name.to_string(),
                    pos,
                    kind.to_string()
                ],
            )
            .await
            .expect("column");
    }
    let col: ColumnId = bed
        .db()
        .query_scalar(
            "SELECT id FROM board_columns WHERE board_id = $1 AND type = 'unstarted'",
            params![board],
        )
        .await
        .expect("todo column");
    let task = TaskId::new();
    bed.db()
        .exec(
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, type, created_by)
             VALUES ($1,$2,$3,$4,'t','task',$5)",
            params![task, tenant, board, col, creator],
        )
        .await
        .expect("task");
    task
}

/// A session on a node, where the node is or is not a shared operator.
async fn session_on(bed: &TestBed, tenant: TenantId, operator: bool) -> SessionId {
    let node = NodeId::new();
    bed.db()
        .exec(
            "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status, capabilities)
             VALUES ($1,$2,$3,$4,'online',$5)",
            params![
                node,
                tenant,
                format!("n-{}", node.0.simple()),
                format!("h-{}", node.0.simple()),
                serde_json::json!({ "shared_operator": operator })
            ],
        )
        .await
        .expect("node");
    let session = SessionId::new();
    bed.db()
        .exec(
            "INSERT INTO sessions (id, tenant_id, node_id, runtime, status)
             VALUES ($1,$2,$3,'claude','running')",
            params![session, tenant, node],
        )
        .await
        .expect("session");
    session
}

fn req(session: Option<SessionId>) -> ClaimTaskRequest {
    ClaimTaskRequest {
        column_type: Some("started".into()),
        assignee_user_id: None,
        session_id: session,
    }
}

#[tokio::test]
async fn a_claim_from_a_shared_operator_session_is_refused() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("wall").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let task = claimable(&bed, tenant, user).await;
    let session = session_on(&bed, tenant, true).await;
    let state = bed.app_state().await;

    let err = claim(
        State(state),
        user_ctx(user, tenant),
        Path(task.to_string()),
        Json(req(Some(session))),
    )
    .await
    .expect_err("the operator may not run the build loop");
    match err {
        ApiError::ForbiddenMsg(m) => assert!(
            m.contains("shared operator nodes do not run the build loop"),
            "the message says which rule bit: {m}"
        ),
        other => panic!("expected a forbidden refusal, got {other:?}"),
    }

    bed.teardown().await;
}

#[tokio::test]
async fn the_same_claim_from_a_personal_node_session_succeeds() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("wall").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let task = claimable(&bed, tenant, user).await;
    let session = session_on(&bed, tenant, false).await;
    let state = bed.app_state().await;

    let got = claim(
        State(state),
        user_ctx(user, tenant),
        Path(task.to_string()),
        Json(req(Some(session))),
    )
    .await
    .expect("a personal node runs the build loop");
    assert_eq!(got.0.assignee_user_id, Some(user));

    bed.teardown().await;
}

/// The documented hole, pinned so it stays deliberate: with no session context
/// the control plane cannot know where the claim came from, and it claims.
#[tokio::test]
async fn a_claim_with_no_session_context_is_out_of_the_walls_reach() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("wall").await;
    let (user, _p) = bed.user(tenant, "owner").await;
    let task = claimable(&bed, tenant, user).await;
    // A session on an operator exists — it is simply not named by the request.
    let _ = session_on(&bed, tenant, true).await;
    let state = bed.app_state().await;

    let got = claim(
        State(state),
        user_ctx(user, tenant),
        Path(task.to_string()),
        Json(req(None)),
    )
    .await
    .expect("no context, no refusal");
    assert_eq!(got.0.assignee_user_id, Some(user));

    bed.teardown().await;
}
