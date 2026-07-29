//! Mission Control's aggregate read must compose the existing visibility rules
//! exactly, never becoming a side-channel around them (MAIN-226 AC-6): checkouts
//! are node-scoped (own + shared), sessions are creator-scoped (MAIN-133).
//!
//! Runs against a private `nook_testkit::TestBed`. Set `DATABASE_URL`.

use nook_control::services::core::overview;
use nook_testkit::TestBed;
use nook_types::*;
use sqlx::PgPool;

async fn checkout(
    db: &PgPool,
    tenant: TenantId,
    node: NodeId,
    ws: WorkspaceId,
    path: &str,
    kind: &str,
) -> NodeWorkspaceId {
    let id = NodeWorkspaceId::new();
    sqlx::query(
        "INSERT INTO node_workspaces (id, tenant_id, node_id, workspace_id, path, kind)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(tenant)
    .bind(node)
    .bind(ws)
    .bind(path)
    .bind(kind)
    .execute(db)
    .await
    .expect("checkout");
    id
}

async fn session(
    db: &PgPool,
    tenant: TenantId,
    ws: WorkspaceId,
    node: NodeId,
    creator: UserId,
    checkout: NodeWorkspaceId,
) -> SessionId {
    let id = SessionId::new();
    sqlx::query(
        "INSERT INTO sessions
             (id, tenant_id, workspace_id, node_id, name, runtime, status, created_by, checkout_id)
         VALUES ($1, $2, $3, $4, 's', 'bash', 'running', $5, $6)",
    )
    .bind(id)
    .bind(tenant)
    .bind(ws)
    .bind(node)
    .bind(creator)
    .bind(checkout)
    .execute(db)
    .await
    .expect("session");
    id
}

/// Every checkout id across the payload's workspaces.
fn all_checkout_ids(ov: &Overview) -> Vec<NodeWorkspaceId> {
    ov.workspaces
        .iter()
        .flat_map(|w| w.checkouts.iter().map(|c| c.id))
        .collect()
}

/// Every session id bound under any checkout.
fn all_session_ids(ov: &Overview) -> Vec<SessionId> {
    ov.workspaces
        .iter()
        .flat_map(|w| {
            w.checkouts
                .iter()
                .flat_map(|c| c.sessions.iter().map(|s| s.id))
        })
        .collect()
}

#[tokio::test]
async fn overview_scopes_checkouts_by_node_and_sessions_by_creator() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ov").await;
    let (user_a, person_a) = bed.user(tenant, "member").await;
    let (user_b, person_b) = bed.user(tenant, "member").await;
    let node_a = bed.node(tenant, person_a).await;
    let node_b = bed.node(tenant, person_b).await;
    let ws = bed.workspace(tenant).await;

    let co_a = checkout(&bed.pool, tenant, node_a, ws, "/srv/a", "clone").await;
    let co_b = checkout(&bed.pool, tenant, node_b, ws, "/srv/b", "clone").await;
    let sess_a = session(&bed.pool, tenant, ws, node_a, user_a, co_a).await;
    let sess_b = session(&bed.pool, tenant, ws, node_b, user_b, co_b).await;

    // Admin scope (both None): the whole fleet — both checkouts, both sessions.
    let admin = overview(&bed.db(), tenant, None, None).await.unwrap();
    let admin_cos = all_checkout_ids(&admin);
    assert!(
        admin_cos.contains(&co_a) && admin_cos.contains(&co_b),
        "admin sees both checkouts"
    );
    let admin_sessions = all_session_ids(&admin);
    assert!(
        admin_sessions.contains(&sess_a) && admin_sessions.contains(&sess_b),
        "admin sees both sessions"
    );

    // Member A: only their own node's checkout, only their own session.
    let a = overview(&bed.db(), tenant, Some(person_a), Some(user_a))
        .await
        .unwrap();
    let a_cos = all_checkout_ids(&a);
    assert!(a_cos.contains(&co_a), "member A sees their own checkout");
    assert!(
        !a_cos.contains(&co_b),
        "member A does NOT see B's node checkout — no side-channel"
    );
    let a_sessions = all_session_ids(&a);
    assert_eq!(
        a_sessions,
        vec![sess_a],
        "member A sees only their own session"
    );

    // Share node B with the team: A now sees B's checkout (own+shared), but STILL
    // not B's session — the two axes are independent.
    sqlx::query("UPDATE nodes SET shared = true WHERE id = $1")
        .bind(node_b)
        .execute(&bed.pool)
        .await
        .expect("share node B");
    let shared = overview(&bed.db(), tenant, Some(person_a), Some(user_a))
        .await
        .unwrap();
    let shared_cos = all_checkout_ids(&shared);
    assert!(
        shared_cos.contains(&co_a) && shared_cos.contains(&co_b),
        "a shared node's checkout becomes visible"
    );
    assert!(
        !all_session_ids(&shared).contains(&sess_b),
        "sharing a node does NOT expose its owner's sessions"
    );

    let _ = person_b;
    bed.teardown().await;
}

#[tokio::test]
async fn overview_groups_the_hierarchy_and_omits_empty_workspaces() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ov").await;
    let (user, person) = bed.user(tenant, "member").await;
    let node = bed.node(tenant, person).await;
    let ws = bed.workspace(tenant).await;
    // A second workspace with NO checkouts/sessions → must be omitted.
    let _empty = bed.workspace(tenant).await;

    let clone = checkout(&bed.pool, tenant, node, ws, "/srv/clone", "clone").await;
    let wt = checkout(&bed.pool, tenant, node, ws, "/srv/wt", "worktree").await;
    let sess = session(&bed.pool, tenant, ws, node, user, clone).await;

    let ov = overview(&bed.db(), tenant, Some(person), Some(user))
        .await
        .unwrap();
    assert_eq!(ov.workspaces.len(), 1, "the empty workspace is omitted");
    let w = &ov.workspaces[0];
    assert_eq!(w.id, ws);
    assert_eq!(w.checkouts.len(), 2, "both checkouts appear under the repo");

    // Kind badges carry through; the session sits under its clone, the worktree is bare.
    let clone_row = w.checkouts.iter().find(|c| c.id == clone).unwrap();
    let wt_row = w.checkouts.iter().find(|c| c.id == wt).unwrap();
    assert_eq!(clone_row.kind, "clone");
    assert_eq!(wt_row.kind, "worktree");
    assert_eq!(
        clone_row.sessions.iter().map(|s| s.id).collect::<Vec<_>>(),
        vec![sess]
    );
    assert!(wt_row.sessions.is_empty());

    bed.teardown().await;
}

// ── MAIN-226 review fix: the dev seed populates Mission Control ──────────────

#[tokio::test]
async fn dev_seed_populates_mission_control() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    // TestBed's template runs `seed::run` (Config::for_test, tenant slug "test"),
    // which now seeds the Mission Control demo: a repo with a remote, a clone + a
    // worktree, a tombstoned checkout, a bound session, and a loose terminal.
    let tenant: TenantId = sqlx::query_scalar("SELECT id FROM tenants WHERE slug = 'test'")
        .fetch_one(&bed.pool)
        .await
        .expect("the seeded dev tenant");

    let ov = overview(&bed.db(), tenant, None, None).await.unwrap();
    let demo = ov
        .workspaces
        .iter()
        .find(|w| w.slug == "mission-demo")
        .expect("the demo workspace is seeded");

    assert!(
        demo.git_remote_url.is_some(),
        "the demo repo shows its remote"
    );
    assert_eq!(demo.checkouts.len(), 3, "clone + worktree + tombstoned");
    assert!(demo.checkouts.iter().any(|c| c.kind == "clone"));
    assert!(demo.checkouts.iter().any(|c| c.kind == "worktree"));
    assert!(
        demo.checkouts.iter().any(|c| c.missing_at.is_some()),
        "a tombstoned checkout for the ghosting"
    );
    assert!(
        demo.checkouts.iter().any(|c| !c.sessions.is_empty()),
        "a session bound to a checkout"
    );
    assert!(
        !ov.loose_sessions.is_empty(),
        "a loose $HOME terminal with no workspace"
    );

    bed.teardown().await;
}
