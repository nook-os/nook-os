//! MAIN-133: `GET /sessions` scopes its metadata by role. A member sees only
//! sessions they created (never `created_by NULL` rows); a tenant owner/admin
//! sees every session, including NULL-creator ones, for capacity/audit. Content
//! access (`session_guard`) is untouched. Set `DATABASE_URL`.

use axum::extract::{Query, State};
use nook_control::auth::{AuthCtx, Principal};
use nook_control::routes::sessions::SessionsQuery;
use nook_db::{params, Db, DbPool};
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

/// An ownerless node (no `owner_person_id`) — `TestBed::node` always sets an
/// owner, so this session suite keeps its own helper.
async fn add_node(db: &DbPool, tenant: TenantId) -> NodeId {
    let id = NodeId::new();
    db.exec(
        "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status)
         VALUES ($1, $2, $3, $4, 'offline')",
        params![
            id,
            tenant,
            format!("n-{}", id.0.simple()),
            format!("h-{}", id.0.simple())
        ],
    )
    .await
    .expect("node");
    id
}

/// A session on `node` created by `creator` (None = a legacy/MCP row).
async fn add_session(
    db: &DbPool,
    tenant: TenantId,
    node: NodeId,
    creator: Option<UserId>,
) -> SessionId {
    let id = SessionId::new();
    db.exec(
        "INSERT INTO sessions (id, tenant_id, node_id, runtime, status, created_by)
         VALUES ($1, $2, $3, 'bash', 'running', $4)",
        params![id, tenant, node, creator.map(|v| v.0)],
    )
    .await
    .expect("session");
    id
}

fn all(active: Option<bool>) -> Query<SessionsQuery> {
    Query(SessionsQuery {
        workspace_id: None,
        active,
    })
}

#[tokio::test]
async fn members_see_only_their_own_admins_see_all_including_null_creators() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping session-visibility test — no DATABASE_URL");
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("sv").await;
    let (owner, _) = bed.user(tenant, "owner").await;
    let (member, _) = bed.user(tenant, "member").await;
    let node = add_node(&bed.db(), tenant).await;

    let my_session = add_session(&bed.db(), tenant, node, Some(member)).await;
    let owner_session = add_session(&bed.db(), tenant, node, Some(owner)).await;
    let legacy_session = add_session(&bed.db(), tenant, node, None).await; // created_by NULL

    // The member sees ONLY their own — not the owner's, not the NULL-creator row.
    let mine = nook_control::routes::sessions::list(
        State(state.clone()),
        user_ctx(member, tenant),
        all(None),
    )
    .await
    .expect("list")
    .0;
    let mine_ids: Vec<SessionId> = mine.iter().map(|s| s.id).collect();
    assert!(
        mine_ids.contains(&my_session),
        "member sees their own session"
    );
    assert!(
        !mine_ids.contains(&owner_session),
        "member must NOT see a teammate's session"
    );
    assert!(
        !mine_ids.contains(&legacy_session),
        "member must NOT see a created_by NULL session"
    );

    // The owner (admin role) sees all three, including the NULL-creator row.
    let all_seen = nook_control::routes::sessions::list(
        State(state.clone()),
        user_ctx(owner, tenant),
        all(None),
    )
    .await
    .expect("list")
    .0;
    let all_ids: Vec<SessionId> = all_seen.iter().map(|s| s.id).collect();
    assert!(
        all_ids.contains(&my_session)
            && all_ids.contains(&owner_session)
            && all_ids.contains(&legacy_session),
        "an owner/admin sees every session incl. NULL-creator rows"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn a_node_credential_sees_all_sessions_unchanged() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("sv").await;
    let (member, _) = bed.user(tenant, "member").await;
    let node = add_node(&bed.db(), tenant).await;
    let s1 = add_session(&bed.db(), tenant, node, Some(member)).await;
    let s2 = add_session(&bed.db(), tenant, node, None).await;

    // A node token's listing is unchanged (whole tenant).
    let node_ctx = AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: UserId(Uuid::nil()),
        tenant_id: tenant,
        principal: Principal::Node(node),
        cookie_session: false,
    };
    let listed = nook_control::routes::sessions::list(State(state.clone()), node_ctx, all(None))
        .await
        .expect("list")
        .0;
    let ids: Vec<SessionId> = listed.iter().map(|s| s.id).collect();
    assert!(
        ids.contains(&s1) && ids.contains(&s2),
        "a node token still sees every session in its tenant"
    );

    bed.teardown().await;
}

/// The `status IN ('starting','running','detached')` guard on the status write,
/// which nothing covered until MAIN-253 moved the statement and went looking.
///
/// Terminal sockets close after the process they were watching has already
/// gone, so a `detached` write routinely races an `exited` one. Without the
/// guard the late write wins and a dead session reads `detached` — which is a
/// LIVE status, so it never leaves the active list, keeps its slot in the
/// capacity view, and looks resumable in the UI. Dropping the `AND status IN
/// (…)` is a one-line edit no other test notices.
#[tokio::test]
async fn a_late_status_write_cannot_resurrect_a_finished_session() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("late").await;
    let node = add_node(&bed.db(), tenant).await;
    let id = add_session(&bed.db(), tenant, node, None).await;

    // The node reported it started, so it is `running` — the state a departing
    // viewer may move.
    bed.db()
        .exec(
            "UPDATE sessions SET status = 'running' WHERE id = $1",
            params![id],
        )
        .await
        .expect("running");

    // While it is live, the write lands.
    assert_eq!(
        state
            .sessions
            .mark_viewer_presence(id, false)
            .await
            .expect("live write"),
        1
    );
    assert_eq!(
        state.sessions.status_of(id).await.unwrap().as_deref(),
        Some("detached")
    );

    // The process exits.
    bed.db()
        .exec(
            "UPDATE sessions SET status = 'exited' WHERE id = $1",
            params![id],
        )
        .await
        .expect("exit");

    // The socket's detach handler fires afterwards. It must match no row.
    assert_eq!(
        state
            .sessions
            .mark_viewer_presence(id, false)
            .await
            .expect("late write"),
        0,
        "a finished session is final"
    );
    assert_eq!(
        state.sessions.status_of(id).await.unwrap().as_deref(),
        Some("exited"),
        "without the guard this reads 'detached' and the session never leaves \
         the active list"
    );

    bed.teardown().await;
}
