//! MAIN-134: the activity feed scopes by role. A member sees only events they
//! caused, events on a node they own, or events on a session they created; a
//! tenant owner/admin (and a node token) sees the whole tenant's activity. The
//! same `ActivityScope` filters the REST list and the live bus, so this tests
//! the shared predicate and the list. Set `DATABASE_URL`.

use nook_control::auth::{AuthCtx, Principal};
use nook_control::services::activity_queries::{self, ActivityScope};
use nook_db::{params, Db, DbPool};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

// ── The pure predicate (the same one the live bus applies per connection) ────

fn event(actor: Option<Uuid>, node: Option<NodeId>, session: Option<SessionId>) -> Event {
    Event {
        id: EventId(Uuid::now_v7()),
        tenant_id: TenantId(Uuid::nil()),
        occurred_at: chrono::Utc::now(),
        kind: "session.started".into(),
        actor_type: actor.map(|_| "user".into()),
        actor_id: actor,
        workspace_id: None,
        node_id: node,
        session_id: session,
        payload: serde_json::json!({}),
    }
}

#[test]
fn all_scope_allows_every_event() {
    let e = event(Some(Uuid::now_v7()), None, None);
    assert!(ActivityScope::All.allows(&e));
    assert!(ActivityScope::All.allows(&event(None, None, None)));
}

#[test]
fn member_scope_allows_only_own_actor_node_or_session() {
    let me = Uuid::now_v7();
    let my_node = NodeId::new();
    let my_session = SessionId::new();
    let scope = ActivityScope::Member {
        user_ids: vec![me],
        node_ids: vec![my_node.0],
        session_ids: vec![my_session.0],
    };

    assert!(
        scope.allows(&event(Some(me), None, None)),
        "an event I caused reaches me"
    );
    assert!(
        scope.allows(&event(None, Some(my_node), None)),
        "an event on my node reaches me"
    );
    assert!(
        scope.allows(&event(None, None, Some(my_session))),
        "an event on my session reaches me"
    );
    assert!(
        !scope.allows(&event(Some(Uuid::now_v7()), None, None)),
        "a teammate's action does not reach me"
    );
    assert!(
        !scope.allows(&event(None, Some(NodeId::new()), Some(SessionId::new()))),
        "an event on a node/session that isn't mine does not reach me"
    );
    assert!(
        !scope.allows(&event(None, None, None)),
        "an unattributed event does not reach a member"
    );
}

// ── The list, and the loader that resolves a caller's scope, against a DB ────

async fn new_tenant(db: &DbPool) -> TenantId {
    let id = TenantId::new();
    db.exec(
        "INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $2)",
        params![id, format!("av-{}", id.0.simple())],
    )
    .await
    .expect("tenant");
    id
}

async fn add_user(db: &DbPool, tenant: TenantId, role: &str) -> (UserId, Uuid) {
    let user = UserId::new();
    let person = Uuid::now_v7();
    db.exec(
        "INSERT INTO users (id, tenant_id, person_id, display_name, email, role)
         VALUES ($1, $2, $3, 'U', $4, $5)",
        params![
            user,
            tenant,
            person,
            format!("u-{}@example.test", user.0.simple()),
            role
        ],
    )
    .await
    .expect("user");
    (user, person)
}

async fn add_node(db: &DbPool, tenant: TenantId, owner: Uuid) -> NodeId {
    let id = NodeId::new();
    db.exec(
        "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status, owner_person_id)
         VALUES ($1, $2, $3, $4, 'offline', $5)",
        params![
            id,
            tenant,
            format!("n-{}", id.0.simple()),
            format!("h-{}", id.0.simple()),
            owner
        ],
    )
    .await
    .expect("node");
    id
}

async fn add_session(db: &DbPool, tenant: TenantId, node: NodeId, creator: UserId) -> SessionId {
    let id = SessionId::new();
    db.exec(
        "INSERT INTO sessions (id, tenant_id, node_id, runtime, status, created_by)
         VALUES ($1, $2, $3, 'bash', 'running', $4)",
        params![id, tenant, node, creator],
    )
    .await
    .expect("session");
    id
}

async fn add_event(
    db: &DbPool,
    tenant: TenantId,
    actor: Option<UserId>,
    node: Option<NodeId>,
    session: Option<SessionId>,
) -> EventId {
    let id = EventId(Uuid::now_v7());
    db.exec(
        "INSERT INTO events (id, tenant_id, kind, actor_type, actor_id, node_id, session_id)
         VALUES ($1, $2, 'test.event', $3, $4, $5, $6)",
        params![
            id,
            tenant,
            actor.map(|_| "user"),
            actor.map(|u| u.0),
            node.map(|v| v.0),
            session.map(|v| v.0)
        ],
    )
    .await
    .expect("event");
    id
}

fn user_ctx(user: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: false,
    }
}

#[tokio::test]
async fn events_list_scopes_members_to_their_own_activity() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping activity-visibility test — no DATABASE_URL");
        return;
    };
    let tenant = new_tenant(&bed.db()).await;
    let (owner, owner_person) = add_user(&bed.db(), tenant, "owner").await;
    let (member, member_person) = add_user(&bed.db(), tenant, "member").await;

    let my_node = add_node(&bed.db(), tenant, member_person).await;
    let my_session = add_session(&bed.db(), tenant, my_node, member).await;
    let owner_node = add_node(&bed.db(), tenant, owner_person).await;

    // Five events: mine by action / on my node / on my session, plus a
    // teammate's action and an event on the teammate's node.
    let e_my_action = add_event(&bed.db(), tenant, Some(member), None, None).await;
    let e_my_node = add_event(&bed.db(), tenant, None, Some(my_node), None).await;
    let e_my_session = add_event(&bed.db(), tenant, None, None, Some(my_session)).await;
    let e_owner_action = add_event(&bed.db(), tenant, Some(owner), None, None).await;
    let e_owner_node = add_event(&bed.db(), tenant, None, Some(owner_node), None).await;

    // Member: sees the three that are theirs, none of the teammate's.
    let scope = ActivityScope::load(
        &bed.db(),
        tenant,
        &user_ctx(member, tenant),
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
    )
    .await
    .expect("scope");
    assert!(
        matches!(scope, ActivityScope::Member { .. }),
        "a member resolves to a scoped view"
    );
    let seen = activity_queries::events_page(&bed.db(), tenant, None, None, None, 200, &scope)
        .await
        .expect("list")
        .events;
    let ids: Vec<EventId> = seen.iter().map(|e| e.id).collect();
    assert!(ids.contains(&e_my_action), "sees their own action");
    assert!(ids.contains(&e_my_node), "sees events on their node");
    assert!(ids.contains(&e_my_session), "sees events on their session");
    assert!(
        !ids.contains(&e_owner_action),
        "does NOT see a teammate's action"
    );
    assert!(
        !ids.contains(&e_owner_node),
        "does NOT see events on a teammate's node"
    );

    // Owner: the full audit feed sees all five.
    let admin_scope = ActivityScope::load(
        &bed.db(),
        tenant,
        &user_ctx(owner, tenant),
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
    )
    .await
    .expect("scope");
    assert!(
        matches!(admin_scope, ActivityScope::All),
        "an owner resolves to the unfiltered feed"
    );
    let all_seen =
        activity_queries::events_page(&bed.db(), tenant, None, None, None, 200, &admin_scope)
            .await
            .expect("list")
            .events;
    let all_ids: Vec<EventId> = all_seen.iter().map(|e| e.id).collect();
    for id in [
        e_my_action,
        e_my_node,
        e_my_session,
        e_owner_action,
        e_owner_node,
    ] {
        assert!(all_ids.contains(&id), "owner sees every event");
    }

    bed.teardown().await;
}

#[tokio::test]
async fn a_node_credential_resolves_to_the_unfiltered_feed() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = new_tenant(&bed.db()).await;

    let node_ctx = AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: UserId(Uuid::nil()),
        tenant_id: tenant,
        principal: Principal::Node(NodeId::new()),
        cookie_session: false,
    };
    let scope = ActivityScope::load(
        &bed.db(),
        tenant,
        &node_ctx,
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
    )
    .await
    .expect("scope");
    assert!(
        matches!(scope, ActivityScope::All),
        "a node token keeps the unfiltered tenant feed"
    );

    bed.teardown().await;
}
