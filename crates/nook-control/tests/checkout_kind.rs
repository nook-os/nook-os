//! MAIN-222: checkouts have a `kind`, sessions know their checkout, and every
//! "the checkout" default is deterministic and clone-only.
//!
//! The load-bearing pair (the slice's whole reason to exist) is here: the
//! deterministic pick NEVER returns a worktree or a missing row, and a session's
//! stored `checkout_id` round-trips to its summary (what restart reuses and the
//! UI shows). `create_session`/`restart` themselves require a live node
//! connection the harness can't fake, so the picks and the restart binding are
//! asserted against the exact SQL the handlers run; `kind` writing and summary
//! hydration go through the real service functions.
//!
//! Every row is test-created and scoped to its own uniquely-named DB via
//! `nook_testkit::TestBed`.

use nook_control::repo::sessions::DbSessionRepository;
use nook_control::repo::workspaces::DbWorkspaceRepository;
use nook_control::services::{discovery, session_queries};
use nook_proto::DiscoveredWorkspace;
use nook_testkit::TestBed;
use nook_types::{NodeId, NodeWorkspaceId, SessionId, TenantId, WorkspaceId};
use sqlx::PgPool;
use uuid::Uuid;

struct Fixture {
    tenant: TenantId,
    node: NodeId,
    workspace: WorkspaceId,
    remote: String,
}

async fn seed(db: &PgPool) -> Fixture {
    let tenant = TenantId::new();
    let node = NodeId::new();
    let workspace = WorkspaceId::new();
    let remote = format!("git@github.com:acme/m222-{}.git", Uuid::now_v7().simple());
    let normalized = discovery::normalize_remote(&remote);
    sqlx::query("INSERT INTO tenants (id, slug, name) VALUES ($1, $2, $2)")
        .bind(tenant)
        .bind(format!("t-{}", Uuid::now_v7().simple()))
        .execute(db)
        .await
        .expect("tenant");
    sqlx::query(
        "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status)
         VALUES ($1, $2, 'dev-box', $3, 'online')",
    )
    .bind(node)
    .bind(tenant)
    .bind(format!("h-{}", Uuid::now_v7().simple()))
    .execute(db)
    .await
    .expect("node");
    sqlx::query(
        "INSERT INTO workspaces (id, tenant_id, name, slug, git_remote_normalized)
         VALUES ($1, $2, 'acme/repo', $3, $4)",
    )
    .bind(workspace)
    .bind(tenant)
    .bind(format!("w-{}", Uuid::now_v7().simple()))
    .bind(&normalized)
    .execute(db)
    .await
    .expect("workspace");
    Fixture {
        tenant,
        node,
        workspace,
        remote,
    }
}

/// Insert a checkout row directly, controlling kind, age, branch, and presence.
#[allow(clippy::too_many_arguments)]
async fn seed_checkout(
    db: &PgPool,
    f: &Fixture,
    id: NodeWorkspaceId,
    path: &str,
    kind: &str,
    branch: &str,
    discovered_secs_ago: i64,
    missing: bool,
) {
    sqlx::query(
        "INSERT INTO node_workspaces
           (id, tenant_id, node_id, workspace_id, path, git_branch, git_status, kind,
            discovered_at, missing_at)
         VALUES ($1,$2,$3,$4,$5,$6,'{}',$7,
                 now() - ($8 * interval '1 second'),
                 CASE WHEN $9 THEN now() ELSE NULL END)",
    )
    .bind(id)
    .bind(f.tenant)
    .bind(f.node)
    .bind(f.workspace)
    .bind(path)
    .bind(branch)
    .bind(kind)
    .bind(discovered_secs_ago)
    .bind(missing)
    .execute(db)
    .await
    .expect("checkout");
}

async fn kind_of(db: &PgPool, path: &str) -> String {
    sqlx::query_as::<_, (String,)>("SELECT kind FROM node_workspaces WHERE path = $1")
        .bind(path)
        .fetch_one(db)
        .await
        .expect("kind")
        .0
}

#[tokio::test]
async fn discovery_writes_kind_from_the_report() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = seed(&bed.pool).await;
    let state = bed.app_state().await;

    let d = |path: &str, worktree: bool| DiscoveredWorkspace {
        path: path.into(),
        name: "acme/repo".into(),
        git_remote_url: Some(f.remote.clone()),
        branch: Some("main".into()),
        dirty: false,
        worktree,
        root_segment: None,
    };
    discovery::reconcile(
        &state,
        f.tenant,
        f.node,
        vec![d("/w/primary", false), d("/w/feature", true)],
    )
    .await
    .expect("reconcile");

    assert_eq!(kind_of(&bed.pool, "/w/primary").await, "clone");
    assert_eq!(
        kind_of(&bed.pool, "/w/feature").await,
        "worktree",
        "the node's worktree report drives kind directly"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn backfill_converts_worktree_jsonb_to_kind() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = seed(&bed.pool).await;
    // Legacy rows: kind left at the 'clone' default, the truth only in the jsonb.
    let mk = |path: &'static str, worktree: bool| {
        let pool = bed.pool.clone();
        let (tenant, node, workspace) = (f.tenant, f.node, f.workspace);
        async move {
            sqlx::query(
                "INSERT INTO node_workspaces
                   (id, tenant_id, node_id, workspace_id, path, git_status, kind)
                 VALUES ($1,$2,$3,$4,$5,$6,'clone')",
            )
            .bind(NodeWorkspaceId::new())
            .bind(tenant)
            .bind(node)
            .bind(workspace)
            .bind(path)
            .bind(serde_json::json!({ "worktree": worktree }))
            .execute(&pool)
            .await
            .expect("legacy row");
        }
    };
    mk("/legacy/primary", false).await;
    mk("/legacy/feature", true).await;

    // The migration's backfill statement, verbatim.
    sqlx::query(
        "UPDATE node_workspaces SET kind = 'worktree'
         WHERE kind = 'clone' AND (git_status ->> 'worktree')::boolean IS TRUE",
    )
    .execute(&bed.pool)
    .await
    .expect("backfill");

    assert_eq!(kind_of(&bed.pool, "/legacy/feature").await, "worktree");
    assert_eq!(
        kind_of(&bed.pool, "/legacy/primary").await,
        "clone",
        "a non-worktree row is untouched"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn the_deterministic_pick_is_clone_only_and_present_only() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = seed(&bed.pool).await;
    let clone = NodeWorkspaceId::new();
    let worktree = NodeWorkspaceId::new();
    let missing_clone = NodeWorkspaceId::new();
    // The worktree is the NEWEST (would win a bare discovered_at order); a second
    // clone is present but MISSING; the real clone is the oldest.
    seed_checkout(
        &bed.pool, &f, clone, "/w/clone", "clone", "main", 300, false,
    )
    .await;
    seed_checkout(
        &bed.pool, &f, worktree, "/w/wt", "worktree", "feat", 1, false,
    )
    .await;
    seed_checkout(
        &bed.pool,
        &f,
        missing_clone,
        "/w/gone",
        "clone",
        "main",
        200,
        true,
    )
    .await;

    // The exact pick the four audited sites run (AC-3).
    let picked: Option<NodeWorkspaceId> = sqlx::query_as::<_, (NodeWorkspaceId,)>(
        "SELECT id FROM node_workspaces
         WHERE tenant_id = $1 AND workspace_id = $2 AND node_id = $3
           AND kind = 'clone' AND missing_at IS NULL
         ORDER BY discovered_at LIMIT 1",
    )
    .bind(f.tenant)
    .bind(f.workspace)
    .bind(f.node)
    .fetch_optional(&bed.pool)
    .await
    .expect("pick")
    .map(|(id,)| id);

    assert_eq!(
        picked,
        Some(clone),
        "the pick is the present clone — never the newer worktree, never the missing clone"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn create_binds_checkout_by_path() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = seed(&bed.pool).await;
    let clone = NodeWorkspaceId::new();
    let wt = NodeWorkspaceId::new();
    let gone = NodeWorkspaceId::new();
    seed_checkout(
        &bed.pool, &f, clone, "/w/clone", "clone", "main", 100, false,
    )
    .await;
    seed_checkout(&bed.pool, &f, wt, "/w/wt", "worktree", "feat", 50, false).await;
    seed_checkout(&bed.pool, &f, gone, "/w/gone", "clone", "main", 10, true).await;

    // The exact resolution create_session_at runs to bind `checkout_id`.
    let resolve = |path: &'static str, pool: PgPool, node: NodeId| async move {
        sqlx::query_as::<_, (NodeWorkspaceId,)>(
            "SELECT id FROM node_workspaces WHERE node_id = $1 AND path = $2 AND missing_at IS NULL",
        )
        .bind(node)
        .bind(path)
        .fetch_optional(&pool)
        .await
        .expect("resolve")
        .map(|(id,)| id)
    };

    assert_eq!(
        resolve("/w/clone", bed.pool.clone(), f.node).await,
        Some(clone),
        "an explicit clone path binds that exact row"
    );
    assert_eq!(
        resolve("/w/wt", bed.pool.clone(), f.node).await,
        Some(wt),
        "an explicit worktree path binds that exact row — kind is not filtered when a path is named"
    );
    assert_eq!(
        resolve("/w/nope", bed.pool.clone(), f.node).await,
        None,
        "an unknown path binds nothing (NULL checkout_id)"
    );
    assert_eq!(
        resolve("/w/gone", bed.pool.clone(), f.node).await,
        None,
        "a tombstoned checkout at that path is never bound"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn hydrate_fills_the_checkout_summary_and_leaves_ad_hoc_null() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = seed(&bed.pool).await;
    let wt = NodeWorkspaceId::new();
    seed_checkout(
        &bed.pool,
        &f,
        wt,
        "/w/wt",
        "worktree",
        "feature/x",
        10,
        false,
    )
    .await;

    // A session bound to that worktree, and an ad-hoc session bound to nothing.
    let bound = SessionId::new();
    let adhoc = SessionId::new();
    sqlx::query(
        "INSERT INTO sessions (id, tenant_id, workspace_id, node_id, name, runtime, status, checkout_id)
         VALUES ($1,$2,$3,$4,'s','bash','running',$5)",
    )
    .bind(bound)
    .bind(f.tenant)
    .bind(f.workspace)
    .bind(f.node)
    .bind(wt)
    .execute(&bed.pool)
    .await
    .expect("bound session");
    sqlx::query(
        "INSERT INTO sessions (id, tenant_id, node_id, name, runtime, status)
         VALUES ($1,$2,$3,'term','bash','running')",
    )
    .bind(adhoc)
    .bind(f.tenant)
    .bind(f.node)
    .execute(&bed.pool)
    .await
    .expect("adhoc session");

    let sessions = session_queries::list_sessions(
        &DbSessionRepository::new(bed.db()),
        &DbWorkspaceRepository::new(bed.db()),
        f.tenant,
        None,
        false,
        None,
    )
    .await
    .expect("list");
    // list_sessions hydrates; find our two.
    let bound_row = sessions.iter().find(|s| s.id == bound).expect("bound");
    let adhoc_row = sessions.iter().find(|s| s.id == adhoc).expect("adhoc");

    let summary = bound_row
        .checkout
        .as_ref()
        .expect("bound session must carry its checkout summary");
    assert_eq!(summary.id, wt);
    assert_eq!(summary.path, "/w/wt");
    assert_eq!(summary.kind, "worktree");
    assert_eq!(summary.branch.as_deref(), Some("feature/x"));
    assert_eq!(summary.node_name, "dev-box");
    assert!(
        adhoc_row.checkout.is_none(),
        "an ad-hoc session has no checkout binding"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn restart_reuses_the_bound_checkout_then_falls_back_when_it_is_gone() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = seed(&bed.pool).await;
    let clone = NodeWorkspaceId::new();
    let wt = NodeWorkspaceId::new();
    seed_checkout(
        &bed.pool, &f, clone, "/w/clone", "clone", "main", 300, false,
    )
    .await;
    seed_checkout(&bed.pool, &f, wt, "/w/wt", "worktree", "feat", 1, false).await;

    // The restart binding query (AC-4): the session started in the worktree.
    let bound_path = |cid: NodeWorkspaceId, pool: PgPool| async move {
        sqlx::query_as::<_, (String,)>(
            "SELECT path FROM node_workspaces WHERE id = $1 AND missing_at IS NULL",
        )
        .bind(cid)
        .fetch_optional(&pool)
        .await
        .expect("bound")
        .map(|(p,)| p)
    };
    assert_eq!(
        bound_path(wt, bed.pool.clone()).await.as_deref(),
        Some("/w/wt"),
        "restart reuses the exact checkout the session started in"
    );

    // Prune the worktree → the binding resolves to nothing → fall back to the
    // deterministic clone pick (AC-4's NULL/missing fallback).
    sqlx::query("UPDATE node_workspaces SET missing_at = now() WHERE id = $1")
        .bind(wt)
        .execute(&bed.pool)
        .await
        .unwrap();
    assert_eq!(
        bound_path(wt, bed.pool.clone()).await,
        None,
        "a pruned binding no longer resolves"
    );
    let fallback: Option<String> = sqlx::query_as::<_, (String,)>(
        "SELECT path FROM node_workspaces
         WHERE workspace_id = $1 AND node_id = $2
           AND kind = 'clone' AND missing_at IS NULL
         ORDER BY discovered_at LIMIT 1",
    )
    .bind(f.workspace)
    .bind(f.node)
    .fetch_optional(&bed.pool)
    .await
    .expect("fallback")
    .map(|(p,)| p);
    assert_eq!(
        fallback.as_deref(),
        Some("/w/clone"),
        "the fallback is the primary clone, not a worktree"
    );

    // The should-fix: on fallback, restart re-binds the session's checkout_id to
    // the clone, so its summary chip names where it now runs — not the pruned
    // worktree it started in.
    let s = SessionId::new();
    sqlx::query(
        "INSERT INTO sessions (id, tenant_id, workspace_id, node_id, name, runtime, status, checkout_id)
         VALUES ($1,$2,$3,$4,'s','bash','running',$5)",
    )
    .bind(s)
    .bind(f.tenant)
    .bind(f.workspace)
    .bind(f.node)
    .bind(wt) // started in the (now pruned) worktree
    .execute(&bed.pool)
    .await
    .expect("session");
    // The exact rebind the restart handler performs on fallback.
    sqlx::query("UPDATE sessions SET checkout_id = $2 WHERE id = $1")
        .bind(s)
        .bind(clone)
        .execute(&bed.pool)
        .await
        .unwrap();
    let sessions = session_queries::list_sessions(
        &DbSessionRepository::new(bed.db()),
        &DbWorkspaceRepository::new(bed.db()),
        f.tenant,
        None,
        false,
        None,
    )
    .await
    .expect("list");
    let summary = sessions
        .iter()
        .find(|x| x.id == s)
        .expect("session")
        .checkout
        .as_ref()
        .expect("summary");
    assert_eq!(
        summary.id, clone,
        "after fallback the chip names the clone, not the pruned worktree"
    );
    assert_eq!(summary.kind, "clone");

    bed.teardown().await;
}
