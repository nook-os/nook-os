//! Discovery attributes a checkout by the folder it was found in, not by whose
//! machine reported it.
//!
//! The bug, seen in prod on 2026-08-03: cross-tenant placement (MAIN-353) puts
//! tenant B's workspace on a node homed in tenant A, and MAIN-363 correctly
//! cloned it into B's own folder. But the scan report carried only a path — an
//! opaque string — so every identity lookup ran scoped to A, missed, and the
//! fallback minted a duplicate workspace in A which then STOLE the real one's
//! checkout. The duplicate had no git credential, so the next clone failed with
//! an authentication error pointing nowhere near the cause.
//!
//! Getting the folder right never helped, because nothing read the folder. The
//! node now reports which root it found each checkout under, and these tests
//! pin the three answers that matter: a foreign folder resolves to its owner, an
//! unrecognised or absent one behaves exactly as before (the legacy tree that
//! cannot be moved), and a foreign folder may never CREATE a workspace in a
//! tenant the node has no claim on.
//!
//! Set `DATABASE_URL`.

use nook_control::services::discovery;
use nook_db::{params, Db};
use nook_proto::DiscoveredWorkspace;
use nook_testkit::TestBed;
use nook_types::*;

fn discovered(path: &str, remote: &str, root_segment: Option<&str>) -> DiscoveredWorkspace {
    DiscoveredWorkspace {
        path: path.into(),
        name: "acme/api".into(),
        git_remote_url: Some(remote.into()),
        branch: Some("main".into()),
        dirty: false,
        worktree: false,
        root_segment: root_segment.map(str::to_string),
    }
}

async fn slug_of(bed: &TestBed, tenant: TenantId) -> String {
    bed.db()
        .query_scalar_opt("SELECT slug FROM tenants WHERE id = $1", params![tenant])
        .await
        .expect("slug query")
        .expect("tenant exists")
}

/// Give a workspace the normalized remote discovery matches on.
async fn set_remote(bed: &TestBed, workspace: WorkspaceId, remote: &str) {
    let normalized = discovery::normalize_remote(remote);
    bed.db()
        .exec(
            "UPDATE workspaces SET git_remote_url = $2, git_remote_normalized = $3 WHERE id = $1",
            params![workspace, remote, normalized],
        )
        .await
        .expect("set remote");
}

async fn workspace_count(bed: &TestBed, tenant: TenantId) -> i64 {
    bed.db()
        .query_scalar_opt(
            "SELECT count(*) FROM workspaces WHERE tenant_id = $1",
            params![tenant],
        )
        .await
        .expect("count")
        .unwrap_or(0)
}

/// The regression. A checkout found under ANOTHER tenant's folder attaches to
/// that tenant's existing workspace, and mints nothing here.
#[tokio::test]
async fn a_checkout_in_another_tenants_folder_attaches_to_that_tenant() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;

    let node_home = bed.tenant("nodehome").await;
    let owner_tenant = bed.tenant("ownerteam").await;
    let (_u, person) = bed.user(node_home, "owner").await;
    let node = bed.node(node_home, person).await;

    let remote = "git@github.com:acme/api.git";
    let theirs = bed.workspace(owner_tenant).await;
    set_remote(&bed, theirs, remote).await;

    let seg = slug_of(&bed, owner_tenant).await;
    let path = format!("/root/.nook/workspace/{seg}/acme/api");
    let before = workspace_count(&bed, node_home).await;

    discovery::reconcile(
        &state,
        node_home,
        node,
        vec![discovered(&path, remote, Some(&seg))],
    )
    .await
    .expect("reconcile");

    assert_eq!(
        workspace_count(&bed, node_home).await,
        before,
        "a duplicate was minted in the NODE's tenant — the folder named its \
         owner and discovery still guessed"
    );

    let (ws, owner): (WorkspaceId, TenantId) = bed
        .db()
        .query_opt(
            "SELECT workspace_id, tenant_id FROM node_workspaces WHERE node_id = $1 AND path = $2",
            params![node, path.clone()],
        )
        .await
        .expect("checkout query")
        .expect("the checkout was recorded");
    assert_eq!(
        ws, theirs,
        "the checkout was attached to the wrong workspace"
    );
    assert_eq!(
        owner, owner_tenant,
        "the checkout row was recorded under the node's tenant — that IS the \
         mis-attribution, arriving by a different door"
    );

    bed.teardown().await;
}

/// The legacy guarantee. A root that is not tenant-scoped — the flat pre-347
/// tree, a control-plane-slug root, or an older node that sends no field at all
/// — behaves exactly as it did before. This is what lets a legacy checkout that
/// cannot be moved stay where it is.
#[tokio::test]
async fn an_unrecognised_or_absent_root_keeps_todays_behaviour() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;

    let node_home = bed.tenant("nodehome").await;
    let (_u, person) = bed.user(node_home, "owner").await;
    let node = bed.node(node_home, person).await;

    let before = workspace_count(&bed, node_home).await;
    discovery::reconcile(
        &state,
        node_home,
        node,
        vec![
            // An older node: no field on the wire at all.
            discovered(
                "/root/.nook/workspace/acme/legacy-a",
                "git@github.com:acme/legacy-a.git",
                None,
            ),
            // A root whose segment matches no tenant — a control-plane slug.
            discovered(
                "/root/.nook/workspace/nook.example.com/acme/legacy-b",
                "git@github.com:acme/legacy-b.git",
                Some("nook.example.com"),
            ),
        ],
    )
    .await
    .expect("reconcile");

    assert_eq!(
        workspace_count(&bed, node_home).await,
        before + 2,
        "a legacy checkout must still be adopted by the node's own tenant"
    );

    bed.teardown().await;
}

/// The security line. A node naming somebody else's folder may MATCH a workspace
/// there, never CREATE one — otherwise a node credential could plant workspaces
/// in any tenant by inventing a directory name. With no workspace to match, the
/// new one lands in the node's own tenant, exactly as an unrecognised root does.
#[tokio::test]
async fn a_foreign_folder_cannot_mint_a_workspace_in_that_tenant() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;

    let node_home = bed.tenant("nodehome").await;
    let victim = bed.tenant("victim").await;
    let (_u, person) = bed.user(node_home, "owner").await;
    let node = bed.node(node_home, person).await;

    let seg = slug_of(&bed, victim).await;
    let victim_before = workspace_count(&bed, victim).await;
    let home_before = workspace_count(&bed, node_home).await;

    // Nothing in `victim` carries this remote, so there is nothing to match.
    discovery::reconcile(
        &state,
        node_home,
        node,
        vec![discovered(
            &format!("/root/.nook/workspace/{seg}/acme/planted"),
            "git@github.com:acme/planted.git",
            Some(&seg),
        )],
    )
    .await
    .expect("reconcile");

    assert_eq!(
        workspace_count(&bed, victim).await,
        victim_before,
        "a node planted a workspace in a tenant it has no claim on, just by \
         naming a folder"
    );
    assert_eq!(
        workspace_count(&bed, node_home).await,
        home_before + 1,
        "the unmatched checkout should still be adopted by the node's own tenant"
    );

    bed.teardown().await;
}
