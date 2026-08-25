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
use nook_db::dialect::{json, time_math, type_mapping};
use nook_db::{params, Db, EnginePool};
use nook_proto::DiscoveredWorkspace;
use nook_testkit::TestBed;
use nook_types::{NodeId, NodeWorkspaceId, SessionId, TenantId, WorkspaceId};
use uuid::Uuid;

struct Fixture {
    tenant: TenantId,
    node: NodeId,
    workspace: WorkspaceId,
    remote: String,
}

async fn seed(bed: &TestBed) -> Fixture {
    let tenant = TenantId::new();
    let node = NodeId::new();
    let workspace = WorkspaceId::new();
    let remote = format!("git@github.com:acme/m222-{}.git", Uuid::now_v7().simple());
    let normalized = discovery::normalize_remote(&remote);
    bed.db()
        .exec(
            "INSERT INTO tenants (id, slug, name) VALUES ($1, $2, $2)",
            params![tenant, format!("t-{}", Uuid::now_v7().simple())],
        )
        .await
        .expect("tenant");
    bed.db()
        .exec(
            "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status)
         VALUES ($1, $2, 'dev-box', $3, 'online')",
            params![node, tenant, format!("h-{}", Uuid::now_v7().simple())],
        )
        .await
        .expect("node");
    bed.db()
        .exec(
            "INSERT INTO workspaces (id, tenant_id, name, slug, git_remote_normalized)
         VALUES ($1, $2, 'acme/repo', $3, $4)",
            params![
                workspace,
                tenant,
                format!("w-{}", Uuid::now_v7().simple()),
                normalized.clone()
            ],
        )
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
    bed: &TestBed,
    f: &Fixture,
    id: NodeWorkspaceId,
    path: &str,
    kind: &str,
    branch: &str,
    discovered_secs_ago: i64,
    missing: bool,
) {
    let discovered = time_math(bed.engine()).now_minus_scaled("$8", "1 second");
    let now = type_mapping(bed.engine()).now();
    bed.db()
        .exec(
            &format!(
                "INSERT INTO node_workspaces
           (id, tenant_id, node_id, workspace_id, path, git_branch, git_status, kind,
            discovered_at, missing_at)
         VALUES ($1,$2,$3,$4,$5,$6,'{{}}',$7,
                 {discovered},
                 CASE WHEN $9 THEN {now} ELSE NULL END)"
            ),
            params![
                id,
                f.tenant,
                f.node,
                f.workspace,
                path,
                branch,
                kind,
                discovered_secs_ago,
                missing
            ],
        )
        .await
        .expect("checkout");
}

async fn kind_of(bed: &TestBed, path: &str) -> String {
    bed.db()
        .query_scalar(
            "SELECT kind FROM node_workspaces WHERE path = $1",
            params![path],
        )
        .await
        .expect("kind")
}

#[tokio::test]
async fn discovery_writes_kind_from_the_report() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = seed(&bed).await;
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

    assert_eq!(kind_of(&bed, "/w/primary").await, "clone");
    assert_eq!(
        kind_of(&bed, "/w/feature").await,
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
    let f = seed(&bed).await;
    // Legacy rows: kind left at the 'clone' default, the truth only in the jsonb.
    let mk = |path: &'static str, worktree: bool| {
        let db = bed.db();
        let (tenant, node, workspace) = (f.tenant, f.node, f.workspace);
        async move {
            db.exec(
                "INSERT INTO node_workspaces
                   (id, tenant_id, node_id, workspace_id, path, git_status, kind)
                 VALUES ($1,$2,$3,$4,$5,$6,'clone')",
                params![
                    NodeWorkspaceId::new(),
                    tenant,
                    node,
                    workspace,
                    path,
                    serde_json::json!({ "worktree": worktree })
                ],
            )
            .await
            .expect("legacy row");
        }
    };
    mk("/legacy/primary", false).await;
    mk("/legacy/feature", true).await;

    // The migration's backfill statement, with its `->>`-and-cast routed through
    // the seams — Postgres still renders the original `(git_status ->>
    // 'worktree')::boolean` character for character.
    let e = bed.engine();
    let is_worktree = type_mapping(e).cast(
        &format!("({})", json(e).get_text("git_status", "worktree")),
        "boolean",
    );
    bed.db()
        .exec(
            &format!(
                "UPDATE node_workspaces SET kind = 'worktree'
         WHERE kind = 'clone' AND {is_worktree} IS TRUE"
            ),
            params![],
        )
        .await
        .expect("backfill");

    assert_eq!(kind_of(&bed, "/legacy/feature").await, "worktree");
    assert_eq!(
        kind_of(&bed, "/legacy/primary").await,
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
    let f = seed(&bed).await;
    let clone = NodeWorkspaceId::new();
    let worktree = NodeWorkspaceId::new();
    let missing_clone = NodeWorkspaceId::new();
    // The worktree is the NEWEST (would win a bare discovered_at order); a second
    // clone is present but MISSING; the real clone is the oldest.
    seed_checkout(&bed, &f, clone, "/w/clone", "clone", "main", 300, false).await;
    seed_checkout(&bed, &f, worktree, "/w/wt", "worktree", "feat", 1, false).await;
    seed_checkout(
        &bed,
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
    let picked: Option<NodeWorkspaceId> = bed
        .db()
        .query_opt::<(NodeWorkspaceId,)>(
            "SELECT id FROM node_workspaces
         WHERE tenant_id = $1 AND workspace_id = $2 AND node_id = $3
           AND kind = 'clone' AND missing_at IS NULL
         ORDER BY discovered_at LIMIT 1",
            params![f.tenant, f.workspace, f.node],
        )
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
    let f = seed(&bed).await;
    let clone = NodeWorkspaceId::new();
    let wt = NodeWorkspaceId::new();
    let gone = NodeWorkspaceId::new();
    seed_checkout(&bed, &f, clone, "/w/clone", "clone", "main", 100, false).await;
    seed_checkout(&bed, &f, wt, "/w/wt", "worktree", "feat", 50, false).await;
    seed_checkout(&bed, &f, gone, "/w/gone", "clone", "main", 10, true).await;

    // The exact resolution create_session_at runs to bind `checkout_id`.
    let resolve = |path: &'static str, db: EnginePool, node: NodeId| async move {
        db.query_opt::<(NodeWorkspaceId,)>(
            "SELECT id FROM node_workspaces WHERE node_id = $1 AND path = $2 AND missing_at IS NULL",
            params![node, path],
        )
        .await
        .expect("resolve")
        .map(|(id,)| id)
    };

    assert_eq!(
        resolve("/w/clone", bed.db(), f.node).await,
        Some(clone),
        "an explicit clone path binds that exact row"
    );
    assert_eq!(
        resolve("/w/wt", bed.db(), f.node).await,
        Some(wt),
        "an explicit worktree path binds that exact row — kind is not filtered when a path is named"
    );
    assert_eq!(
        resolve("/w/nope", bed.db(), f.node).await,
        None,
        "an unknown path binds nothing (NULL checkout_id)"
    );
    assert_eq!(
        resolve("/w/gone", bed.db(), f.node).await,
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
    let f = seed(&bed).await;
    let wt = NodeWorkspaceId::new();
    seed_checkout(&bed, &f, wt, "/w/wt", "worktree", "feature/x", 10, false).await;

    // A session bound to that worktree, and an ad-hoc session bound to nothing.
    let bound = SessionId::new();
    let adhoc = SessionId::new();
    bed.db()
        .exec(
            "INSERT INTO sessions (id, tenant_id, workspace_id, node_id, name, runtime, status, checkout_id)
         VALUES ($1,$2,$3,$4,'s','bash','running',$5)",
            params![bound, f.tenant, f.workspace, f.node, wt],
        )
        .await
        .expect("bound session");
    bed.db()
        .exec(
            "INSERT INTO sessions (id, tenant_id, node_id, name, runtime, status)
         VALUES ($1,$2,$3,'term','bash','running')",
            params![adhoc, f.tenant, f.node],
        )
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
    let f = seed(&bed).await;
    let clone = NodeWorkspaceId::new();
    let wt = NodeWorkspaceId::new();
    seed_checkout(&bed, &f, clone, "/w/clone", "clone", "main", 300, false).await;
    seed_checkout(&bed, &f, wt, "/w/wt", "worktree", "feat", 1, false).await;

    // The restart binding query (AC-4): the session started in the worktree.
    let bound_path = |cid: NodeWorkspaceId, db: EnginePool| async move {
        db.query_opt::<(String,)>(
            "SELECT path FROM node_workspaces WHERE id = $1 AND missing_at IS NULL",
            params![cid],
        )
        .await
        .expect("bound")
        .map(|(p,)| p)
    };
    assert_eq!(
        bound_path(wt, bed.db()).await.as_deref(),
        Some("/w/wt"),
        "restart reuses the exact checkout the session started in"
    );

    // Prune the worktree → the binding resolves to nothing → fall back to the
    // deterministic clone pick (AC-4's NULL/missing fallback).
    let now = type_mapping(bed.engine()).now();
    bed.db()
        .exec(
            &format!("UPDATE node_workspaces SET missing_at = {now} WHERE id = $1"),
            params![wt],
        )
        .await
        .unwrap();
    assert_eq!(
        bound_path(wt, bed.db()).await,
        None,
        "a pruned binding no longer resolves"
    );
    let fallback: Option<String> = bed
        .db()
        .query_opt::<(String,)>(
            "SELECT path FROM node_workspaces
         WHERE workspace_id = $1 AND node_id = $2
           AND kind = 'clone' AND missing_at IS NULL
         ORDER BY discovered_at LIMIT 1",
            params![f.workspace, f.node],
        )
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
    bed.db()
        .exec(
            "INSERT INTO sessions (id, tenant_id, workspace_id, node_id, name, runtime, status, checkout_id)
         VALUES ($1,$2,$3,$4,'s','bash','running',$5)",
            // started in the (now pruned) worktree
            params![s, f.tenant, f.workspace, f.node, wt],
        )
        .await
        .expect("session");
    // The exact rebind the restart handler performs on fallback.
    bed.db()
        .exec(
            "UPDATE sessions SET checkout_id = $2 WHERE id = $1",
            params![s, clone],
        )
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
