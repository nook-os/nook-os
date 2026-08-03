//! A checkout's tenant follows its workspace, including when a row is healed in
//! place.
//!
//! The prod failure this pins, 2026-08-03: `upsert_checkout` and
//! `associate_clone` both heal a conflicting row with
//! `ON CONFLICT (node_id, path) DO UPDATE SET workspace_id = EXCLUDED…` and did
//! not list `tenant_id`. A row left from an earlier owner was re-pointed at a
//! workspace in another tenant and kept its old scope. Every read of
//! `node_workspaces` is tenant-scoped, so `present_checkouts` stopped seeing it,
//! the reconciler decided the node held no checkout, and it re-cloned every 60
//! seconds forever without ever placing a session.
//!
//! The clones SUCCEEDED every time, which is why it presented as "azul just
//! isn't cloning" rather than as an error anywhere.
//!
//! Set `DATABASE_URL`.

use nook_control::repo::workspaces::CheckoutUpsert;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;

/// The tenant recorded on the single checkout at `path`, and the workspace it
/// points at.
async fn row(bed: &TestBed, node: NodeId, path: &str) -> (TenantId, WorkspaceId) {
    bed.db()
        .query_opt(
            "SELECT tenant_id, workspace_id FROM node_workspaces
              WHERE node_id = $1 AND path = $2",
            params![node, path],
        )
        .await
        .expect("checkout query")
        .expect("the checkout row exists")
}

/// Re-pointing a checkout at a workspace in another tenant must move its tenant
/// too, or the row contradicts itself and every tenant-scoped read loses it.
#[tokio::test]
async fn healing_a_checkout_onto_another_tenants_workspace_moves_its_tenant() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;

    let first = bed.tenant("first").await;
    let second = bed.tenant("second").await;
    let (_u, person) = bed.user(first, "owner").await;
    let node = bed.node(first, person).await;

    let old = bed.workspace(first).await;
    let new = bed.workspace(second).await;
    let path = "/root/.nook/workspace/second/acme/api";

    let upsert = |tenant: TenantId, workspace: WorkspaceId| CheckoutUpsert {
        tenant,
        node_id: node,
        workspace_id: workspace,
        path: path.to_string(),
        git_remote_url: Some("git@github.com:acme/api.git".into()),
        git_remote_normalized: Some("github.com/acme/api".into()),
        branch: Some("main".into()),
        git_status: serde_json::json!({ "dirty": false, "worktree": false }),
        kind: "clone".to_string(),
    };

    // The row as an earlier owner left it.
    state
        .workspaces
        .upsert_checkout(upsert(first, old))
        .await
        .expect("seed the stale row");
    assert_eq!(row(&bed, node, path).await, (first, old));

    // The same path, now belonging to the other tenant's workspace. This is the
    // heal-in-place that used to move only the pointer.
    state
        .workspaces
        .upsert_checkout(upsert(second, new))
        .await
        .expect("heal in place");

    let (tenant, workspace) = row(&bed, node, path).await;
    assert_eq!(workspace, new, "the workspace pointer did not move");
    assert_eq!(
        tenant, second,
        "the checkout kept its OLD tenant while pointing at the new workspace — \
         that row is invisible to present_checkouts, which is the re-clone loop"
    );

    // The proof that matters: the read the reconciler actually uses can see it.
    let present = state
        .workspaces
        .present_checkouts(second, new)
        .await
        .expect("present_checkouts");
    assert!(
        present.iter().any(|c| c.node_id == node),
        "present_checkouts(second) cannot see the healed checkout — the \
         reconciler would call this node checkout-less and re-clone forever"
    );

    bed.teardown().await;
}

/// `associate_clone` is the reconciler's own completion path and carries the
/// same rule: a clone landing on a path some stale row already claims must
/// correct the scope, not just the pointer.
#[tokio::test]
async fn associate_clone_also_moves_a_stale_rows_tenant() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;

    let first = bed.tenant("first").await;
    let second = bed.tenant("second").await;
    let (_u, person) = bed.user(first, "owner").await;
    let node = bed.node(first, person).await;

    let old = bed.workspace(first).await;
    let new = bed.workspace(second).await;
    let path = "/root/.nook/workspace/second/acme/api";
    let url = "git@github.com:acme/api.git";

    state
        .workspaces
        .upsert_checkout(CheckoutUpsert {
            tenant: first,
            node_id: node,
            workspace_id: old,
            path: path.to_string(),
            git_remote_url: Some(url.into()),
            git_remote_normalized: Some("github.com/acme/api".into()),
            branch: None,
            git_status: serde_json::json!({}),
            kind: "clone".to_string(),
        })
        .await
        .expect("seed the stale row");

    state
        .workspaces
        .associate_clone(second, node, new, path, url, "github.com/acme/api")
        .await
        .expect("associate the completed clone");

    let (tenant, workspace) = row(&bed, node, path).await;
    assert_eq!(workspace, new);
    assert_eq!(
        tenant, second,
        "associate_clone healed the pointer and left the scope stale"
    );

    bed.teardown().await;
}
