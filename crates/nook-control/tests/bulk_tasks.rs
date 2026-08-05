//! Backlog bulk actions (MAIN-154): every action's happy path, epic skip
//! reasons, cross-tenant/visibility skips, and the load-bearing mixed batch
//! (one ok + two distinct skip reasons) — through the real handler against a
//! live Postgres. Set `DATABASE_URL`. Assertions scope to test-created rows.

use axum::extract::State;
use axum::Json;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::routes::bulk::bulk_tasks;
use nook_control::services::identity::{login_identity, IdentityClaims};
use nook_control::state::AppState;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

fn claims(subject: &str, name: &str) -> IdentityClaims {
    IdentityClaims {
        issuer: "test-idp".into(),
        subject: subject.into(),
        email: Some(format!("{subject}@example.test")),
        email_verified: false,
        display_name: Some(name.into()),
        avatar_url: None,
        raw_claims: serde_json::json!({}),
    }
}

fn auth(user: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: false,
    }
}

async fn add_member(bed: &TestBed, tenant: TenantId, name: &str) -> UserId {
    let id = UserId::new();
    bed.db()
        .exec(
            "INSERT INTO users (id, tenant_id, person_id, display_name, email, role)
         VALUES ($1, $2, gen_random_uuid(), $3, $4, 'member')",
            params![
                id,
                tenant,
                name,
                format!("{}-{}@example.test", name, id.0.simple())
            ],
        )
        .await
        .expect("member");
    id
}

async fn make_board(bed: &TestBed, tenant: TenantId) -> BoardId {
    let board: BoardId = bed
        .db()
        .query_scalar(
            "INSERT INTO boards (id, tenant_id, name, key, provider)
         VALUES ($1, $2, 'b', $3, 'local') RETURNING id",
            params![
                BoardId::new(),
                tenant,
                format!("BK{}", &Uuid::now_v7().simple().to_string()[..6]).to_uppercase()
            ],
        )
        .await
        .expect("board");
    for (name, pos, ty) in [("Todo", 0, "unstarted"), ("Done", 1, "completed")] {
        bed.db()
            .exec(
                "INSERT INTO board_columns (id, board_id, name, position, type)
             VALUES ($1, $2, $3, $4, $5)",
                params![Uuid::now_v7(), board, name, pos, ty],
            )
            .await
            .expect("column");
    }
    board
}

async fn make_task(
    state: &AppState,
    tenant: TenantId,
    board: BoardId,
    creator: UserId,
    title: &str,
    type_: Option<&str>,
) -> TaskItem {
    state
        .kanban
        .get("local")
        .expect("local provider")
        .create_task(
            tenant,
            board,
            Some(creator),
            CreateTaskRequest {
                title: title.into(),
                description: None,
                column_id: None,
                column_type: None,
                workspace_id: None,
                priority: None,
                type_: type_.map(Into::into),
                visibility: Some("team".into()),
                parent: None,
                labels: vec![],
            },
        )
        .await
        .expect("create task")
}

fn req(ids: &[TaskId], action: &str, value: Option<&str>) -> BulkTaskRequest {
    BulkTaskRequest {
        task_ids: ids.iter().map(|i| i.0.to_string()).collect(),
        action: action.into(),
        value: value.map(Into::into),
    }
}

async fn call(state: &AppState, a: AuthCtx, r: BulkTaskRequest) -> BulkTaskResponse {
    bulk_tasks(State(state.clone()), a, Json(r))
        .await
        .expect("bulk ok")
        .0
}

fn result_for(resp: &BulkTaskResponse, id: TaskId) -> &BulkTaskItemResult {
    let key = id.0.to_string();
    resp.results
        .iter()
        .find(|r| r.id == key)
        .expect("a result for the id")
}

#[tokio::test]
async fn bulk_actions_apply_skip_epics_and_respect_visibility() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping bulk_tasks test — no DATABASE_URL");
        return;
    };
    let state = bed.app_state().await;

    let sub = format!("bulk-owner-{}", Uuid::now_v7().simple());
    let (owner, tenant) = login_identity(&state, claims(&sub, "Owner"))
        .await
        .expect("owner in");
    let a = auth(owner.id, tenant.id);
    let member = add_member(&bed, tenant.id, "mia").await;
    let board = make_board(&bed, tenant.id).await;

    let t1 = make_task(&state, tenant.id, board, owner.id, "one", None).await;
    let t2 = make_task(&state, tenant.id, board, owner.id, "two", None).await;
    let epic = make_task(&state, tenant.id, board, owner.id, "an epic", Some("epic")).await;

    // A task in a DIFFERENT tenant — invisible to `a`, must be skipped.
    let osub = format!("bulk-other-{}", Uuid::now_v7().simple());
    let (other_owner, other_tenant) = login_identity(&state, claims(&osub, "Other"))
        .await
        .expect("other in");
    let other_board = make_board(&bed, other_tenant.id).await;
    let foreign = make_task(
        &state,
        other_tenant.id,
        other_board,
        other_owner.id,
        "foreign",
        None,
    )
    .await;

    // ── The load-bearing case: a mixed batch → exactly one ok + two distinct
    //    reasons (AC-2) ──────────────────────────────────────────────────────
    let resp = call(
        &state,
        a,
        req(&[t1.id, epic.id, foreign.id], "type", Some("chore")),
    )
    .await;
    assert_eq!(resp.updated, 1, "only the plain task takes the type change");
    assert_eq!(resp.skipped, 2);
    assert_eq!(result_for(&resp, t1.id).status, "ok");
    assert_eq!(
        result_for(&resp, epic.id).reason.as_deref(),
        Some("epics are containers")
    );
    assert_eq!(
        result_for(&resp, foreign.id).reason.as_deref(),
        Some("not found in your tenant")
    );
    // t1 really changed; the epic and foreign task did not.
    assert_eq!(task_type(&bed, t1.id).await, "chore");
    assert_eq!(task_type(&bed, epic.id).await, "epic");
    assert_eq!(
        task_type(&bed, foreign.id).await,
        "task",
        "foreign untouched"
    );

    // ── priority (happy path) ───────────────────────────────────────────────
    let resp = call(&state, a, req(&[t1.id, t2.id], "priority", Some("2"))).await;
    assert_eq!(resp.updated, 2);
    assert_eq!(task_priority(&bed, t1.id).await, 2);
    assert_eq!(task_priority(&bed, t2.id).await, 2);

    // ── agent_ready on/off, epic skipped with its own reason ────────────────
    let resp = call(&state, a, req(&[t1.id, epic.id], "agent_ready", Some("on"))).await;
    assert_eq!(resp.updated, 1);
    assert_eq!(
        result_for(&resp, epic.id).reason.as_deref(),
        Some("epics are never agent-ready")
    );
    assert!(
        has_label(&bed, t1.id, "agent-ready").await,
        "label attached"
    );
    let _ = call(&state, a, req(&[t1.id], "agent_ready", Some("off"))).await;
    assert!(
        !has_label(&bed, t1.id, "agent-ready").await,
        "label removed"
    );

    // ── assignee: set to a member, then unassign ────────────────────────────
    let resp = call(
        &state,
        a,
        req(&[t1.id], "assignee", Some(&member.0.to_string())),
    )
    .await;
    assert_eq!(resp.updated, 1);
    assert_eq!(task_assignee(&bed, t1.id).await, Some(member.0));
    let _ = call(&state, a, req(&[t1.id], "assignee", None)).await;
    assert_eq!(task_assignee(&bed, t1.id).await, None, "unassigned");

    // ── move to a column by type ────────────────────────────────────────────
    let resp = call(&state, a, req(&[t1.id], "move_column", Some("completed"))).await;
    assert_eq!(resp.updated, 1);
    assert_eq!(column_type(&bed, t1.id).await, "completed");

    // ── archive ─────────────────────────────────────────────────────────────
    let resp = call(&state, a, req(&[t2.id], "archive", None)).await;
    assert_eq!(resp.updated, 1);
    assert!(is_archived(&bed, t2.id).await, "archived_at set");

    bed.teardown().await;
}

async fn task_type(bed: &TestBed, id: TaskId) -> String {
    bed.db()
        .query_scalar("SELECT type FROM tasks WHERE id = $1", params![id])
        .await
        .unwrap()
}
async fn task_priority(bed: &TestBed, id: TaskId) -> i32 {
    bed.db()
        .query_scalar("SELECT priority FROM tasks WHERE id = $1", params![id])
        .await
        .unwrap()
}
async fn task_assignee(bed: &TestBed, id: TaskId) -> Option<Uuid> {
    bed.db()
        .query_scalar::<Option<Uuid>>(
            "SELECT assignee_user_id FROM tasks WHERE id = $1",
            params![id],
        )
        .await
        .unwrap()
}
async fn is_archived(bed: &TestBed, id: TaskId) -> bool {
    bed.db()
        .query_scalar::<Option<chrono::DateTime<chrono::Utc>>>(
            "SELECT archived_at FROM tasks WHERE id = $1",
            params![id],
        )
        .await
        .unwrap()
        .is_some()
}
async fn column_type(bed: &TestBed, id: TaskId) -> String {
    bed.db()
        .query_scalar(
            "SELECT c.type FROM tasks t JOIN board_columns c ON c.id = t.column_id WHERE t.id = $1",
            params![id],
        )
        .await
        .unwrap()
}
async fn has_label(bed: &TestBed, id: TaskId, name: &str) -> bool {
    bed.db()
        .query_scalar::<i64>(
            "SELECT count(*) FROM task_labels tl JOIN labels l ON l.id = tl.label_id
         WHERE tl.task_id = $1 AND l.name = $2",
            params![id, name],
        )
        .await
        .unwrap()
        > 0
}
