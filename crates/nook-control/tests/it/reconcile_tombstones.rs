//! MAIN-220: reconcile hardening — tombstones instead of deletes.
//!
//! Discovery used to DELETE any checkout a node stopped reporting, so an empty
//! report (unmounted root, a scan that panicked into `unwrap_or_default()`)
//! erased every checkout a node had while the files sat on disk, and a moved
//! checkout became delete+insert — a new id and a broken task reference.
//!
//! These tests assert the new contract: a vanished checkout is MARKED missing
//! (never deleted), a re-report HEALS the same row id (and does not re-announce
//! the checkout), an empty report marks all and deletes none, hard deletion is
//! the retention sweep's job and reclaims only aged-out rows, and reconcile no
//! longer rewrites `workspaces.slug` — so a rename collision no longer freezes
//! the scan.
//!
//! Setup + teardown run through `nook_testkit::TestBed` (MAIN-156); every row is
//! test-created and scoped to its own tenant/node.

use chrono::{DateTime, Utc};
use nook_control::services::{discovery, workspace_reaper};
use nook_db::{params, Db, EnginePool};
use nook_proto::DiscoveredWorkspace;
use nook_testkit::TestBed;
use nook_types::{NodeId, NodeWorkspaceId, TenantId, WorkspaceId};
use uuid::Uuid;

struct Fixture {
    tenant: TenantId,
    node: NodeId,
    workspace: WorkspaceId,
    remote: String,
}

/// A tenant, an online node, and a workspace with a recorded normalized remote.
async fn seed(bed: &TestBed) -> Fixture {
    let tenant = TenantId::new();
    let node = NodeId::new();
    let workspace = WorkspaceId::new();
    let remote = format!("git@github.com:acme/m220-{}.git", Uuid::now_v7().simple());
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
         VALUES ($1, $2, $3, $3, 'online')",
            params![node, tenant, format!("n-{}", Uuid::now_v7().simple())],
        )
        .await
        .expect("node");
    bed.db()
        .exec(
            "INSERT INTO workspaces (id, tenant_id, name, slug, git_remote_normalized)
         VALUES ($1, $2, $3, $4, $5)",
            params![
                workspace,
                tenant,
                format!("acme/w-{}", Uuid::now_v7().simple()),
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

fn discovered(path: &str, remote: &str) -> DiscoveredWorkspace {
    DiscoveredWorkspace {
        path: path.into(),
        name: "worktree".into(),
        git_remote_url: Some(remote.into()),
        branch: Some("main".into()),
        dirty: false,
        worktree: false,
        root_segment: None,
    }
}

/// (id, missing_at) for every checkout of a node, path-ordered.
async fn rows(bed: &TestBed, node: NodeId) -> Vec<(NodeWorkspaceId, Option<DateTime<Utc>>)> {
    bed.db()
        .query_all(
            "SELECT id, missing_at FROM node_workspaces WHERE node_id = $1 ORDER BY path",
            params![node],
        )
        .await
        .expect("rows")
}

#[tokio::test]
async fn empty_report_marks_all_missing_and_deletes_none() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = seed(&bed).await;
    let state = bed.app_state().await;

    // First scan: two checkouts present.
    discovery::reconcile(
        &state,
        f.tenant,
        f.node,
        vec![discovered("/w/a", &f.remote), discovered("/w/b", &f.remote)],
    )
    .await
    .expect("first reconcile");
    let before = rows(&bed, f.node).await;
    assert_eq!(before.len(), 2, "both checkouts recorded");
    assert!(
        before.iter().all(|(_, m)| m.is_none()),
        "present checkouts are not tombstoned"
    );

    // The unmount: the node now reports ZERO checkouts. The old code deleted
    // every row; the new code marks them missing and deletes nothing.
    discovery::reconcile(&state, f.tenant, f.node, vec![])
        .await
        .expect("empty reconcile must not error");

    let after = rows(&bed, f.node).await;
    assert_eq!(after.len(), 2, "an empty report deletes NOTHING");
    assert!(
        after.iter().all(|(_, m)| m.is_some()),
        "an empty report marks every checkout missing"
    );
    // Same ids — the rows were marked in place, not churned.
    assert_eq!(
        before.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        after.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        "row identity survives the empty report"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn heal_preserves_row_id_and_does_not_reannounce() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = seed(&bed).await;
    // A secret makes `announce_new_checkout` record a `workspace.checkout_added`
    // event, so re-announcement is observable by counting those events.
    bed.db()
        .exec(
            "INSERT INTO workspace_secrets (id, tenant_id, workspace_id, content_enc)
         VALUES ($1, $2, $3, $4)",
            params![Uuid::now_v7(), f.tenant, f.workspace, vec![1u8, 2, 3]],
        )
        .await
        .expect("secret");
    let state = bed.app_state().await;

    let announces = |db: EnginePool, ws: WorkspaceId| async move {
        let (n,): (i64,) = db
            .query_one(
                "SELECT count(*) FROM events
             WHERE kind = 'workspace.checkout_added' AND workspace_id = $1",
                params![ws],
            )
            .await
            .expect("count");
        n
    };

    // Discover the checkout → row created, one announcement.
    discovery::reconcile(
        &state,
        f.tenant,
        f.node,
        vec![discovered("/w/a", &f.remote)],
    )
    .await
    .expect("discover");
    let created = rows(&bed, f.node).await;
    assert_eq!(created.len(), 1);
    let id0 = created[0].0;
    assert_eq!(
        announces(bed.db(), f.workspace).await,
        1,
        "a brand-new checkout announces once"
    );

    // It vanishes → tombstoned, same id.
    discovery::reconcile(&state, f.tenant, f.node, vec![])
        .await
        .expect("vanish");
    let gone = rows(&bed, f.node).await;
    assert_eq!(gone.len(), 1, "not deleted");
    assert_eq!(gone[0].0, id0, "same row");
    assert!(gone[0].1.is_some(), "marked missing");

    // It comes back → healed: same id, missing_at cleared, and NO second
    // announcement (the row was never new — it healed in place).
    discovery::reconcile(
        &state,
        f.tenant,
        f.node,
        vec![discovered("/w/a", &f.remote)],
    )
    .await
    .expect("heal");
    let healed = rows(&bed, f.node).await;
    assert_eq!(healed.len(), 1);
    assert_eq!(healed[0].0, id0, "heal preserves the row id");
    assert!(healed[0].1.is_none(), "heal clears missing_at");
    assert_eq!(
        announces(bed.db(), f.workspace).await,
        1,
        "healing a checkout does NOT re-announce it"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn retention_sweep_removes_only_expired_rows() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = seed(&bed).await;

    // Three checkouts: one present, one freshly missing, one long-missing.
    let insert = |id: NodeWorkspaceId, path: &'static str, missing: Option<&'static str>| {
        let (tenant, node, workspace, remote) = (f.tenant, f.node, f.workspace, f.remote.clone());
        let db = bed.db();
        async move {
            let normalized = discovery::normalize_remote(&remote);
            db.exec(
                "INSERT INTO node_workspaces
                   (id, tenant_id, node_id, workspace_id, path, git_remote_url,
                    git_remote_normalized, git_branch, git_status, missing_at)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,'main','{}',
                         CASE WHEN $8::text IS NULL THEN NULL ELSE now() - ($8::bigint * interval '1 second') END)",
                params![
                    id,
                    tenant,
                    node,
                    workspace,
                    path,
                    remote.clone(),
                    normalized.clone(),
                    missing.map(str::to_string)
                ],
            )
            .await
            .expect("insert nw");
        }
    };
    let present = NodeWorkspaceId::new();
    let fresh = NodeWorkspaceId::new();
    let stale = NodeWorkspaceId::new();
    insert(present, "/w/present", None).await;
    insert(fresh, "/w/fresh", Some("3600")).await; // missing 1 hour
    insert(stale, "/w/stale", Some("864000")).await; // missing 10 days

    let state = bed.app_state().await;
    // Retention = 7 days. Only the 10-day-missing row is past it.
    let reaped = workspace_reaper::reap_missing_checkouts(&state, 604_800)
        .await
        .expect("sweep");
    assert_eq!(reaped, 1, "exactly one row was past retention");

    let remaining: Vec<NodeWorkspaceId> = rows(&bed, f.node)
        .await
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert!(
        remaining.contains(&present),
        "a present checkout is never reaped"
    );
    assert!(
        remaining.contains(&fresh),
        "a freshly-missing checkout is within retention"
    );
    assert!(
        !remaining.contains(&stale),
        "a long-missing checkout is reclaimed"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn slug_is_stable_and_a_rename_collision_does_not_freeze_the_scan() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = TenantId::new();
    let node = NodeId::new();
    let remote = format!("git@github.com:acme/m220s-{}.git", Uuid::now_v7().simple());
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
         VALUES ($1, $2, $3, $3, 'online')",
            params![node, tenant, format!("n-{}", Uuid::now_v7().simple())],
        )
        .await
        .expect("node");

    // The workspace under test: a BARE name "repo" (slug "repo"), matched by
    // remote. Discovery of "acme/repo" would qualify the name to "acme/repo".
    let ws = WorkspaceId::new();
    bed.db()
        .exec(
            "INSERT INTO workspaces (id, tenant_id, name, slug, git_remote_normalized)
         VALUES ($1, $2, 'repo', 'repo', $3)",
            params![ws, tenant, normalized.clone()],
        )
        .await
        .expect("ws");

    // A SECOND workspace already owns the slug the old code would rewrite to
    // (slugify("acme/repo") == "acme-repo"). The old `SET slug = ...` would hit
    // the unique-slug constraint here and abort the whole reconcile.
    let squatter = WorkspaceId::new();
    bed.db()
        .exec(
            "INSERT INTO workspaces (id, tenant_id, name, slug) VALUES ($1, $2, 'other', 'acme-repo')",
            params![squatter, tenant],
        )
        .await
        .expect("squatter");

    let state = bed.app_state().await;
    let mut d = discovered("/w/repo", &remote);
    d.name = "acme/repo".into(); // owner/repo → triggers the name-qualify path

    // The rename path fires and must NOT freeze on the slug collision.
    discovery::reconcile(&state, tenant, node, vec![d])
        .await
        .expect("reconcile must not abort on a slug that is already taken");

    let (name, slug): (String, String) = bed
        .db()
        .query_one(
            "SELECT name, slug FROM workspaces WHERE id = $1",
            params![ws],
        )
        .await
        .expect("ws row");
    assert_eq!(name, "acme/repo", "the display name may be qualified");
    assert_eq!(slug, "repo", "the slug is stable after creation");

    // The squatter's slug was never touched.
    let (sslug,): (String,) = bed
        .db()
        .query_one(
            "SELECT slug FROM workspaces WHERE id = $1",
            params![squatter],
        )
        .await
        .expect("squatter row");
    assert_eq!(sslug, "acme-repo");

    bed.teardown().await;
}

/// The `missing_at IS NULL` guard, which nothing covered until MAIN-251 moved
/// the statement and went looking.
///
/// Every scan re-runs the tombstone sweep, so without the guard a row missing
/// for a month would have its `missing_at` re-stamped to "just now" on each
/// pass — the retention clock would restart forever and the reaper would never
/// reclaim anything. Dropping `AND missing_at IS NULL` from the UPDATE is a
/// one-token change that no other test in this file notices.
#[tokio::test]
async fn a_repeated_empty_report_does_not_restart_the_retention_clock() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = seed(&bed).await;
    let state = bed.app_state().await;

    discovery::reconcile(
        &state,
        f.tenant,
        f.node,
        vec![discovered("/w/a", &f.remote)],
    )
    .await
    .expect("first reconcile");
    discovery::reconcile(&state, f.tenant, f.node, vec![])
        .await
        .expect("the checkout vanishes");

    let first_stamp = rows(&bed, f.node).await[0]
        .1
        .expect("tombstoned by the empty report");

    // Age the tombstone: this is the row that must age out.
    bed.db()
        .exec(
            "UPDATE node_workspaces SET missing_at = $2 WHERE node_id = $1",
            params![f.node, first_stamp - chrono::Duration::days(30)],
        )
        .await
        .expect("backdate");

    // Another scan, still not reporting it.
    discovery::reconcile(&state, f.tenant, f.node, vec![])
        .await
        .expect("second empty report");

    let now = rows(&bed, f.node).await[0].1.expect("still tombstoned");
    assert!(
        now < Utc::now() - chrono::Duration::days(29),
        "a second scan re-stamped missing_at ({now}) — the retention clock \
         restarted, so this checkout would never be reclaimed"
    );

    // And the consequence that actually matters: it is past a 7-day retention.
    let reaped = workspace_reaper::reap_missing_checkouts(&state, 7 * 24 * 3600)
        .await
        .expect("sweep");
    assert_eq!(
        reaped, 1,
        "a row missing for 30 days is past a 7-day window"
    );

    bed.teardown().await;
}
