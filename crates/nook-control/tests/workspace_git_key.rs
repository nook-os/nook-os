//! MAIN-367: the git-key endpoint is scoped by NODE and WORKSPACE.
//!
//! Two earlier shapes of this endpoint were wrong in the same direction, and
//! both are pinned here. It first asked the session's tenant, which refused a
//! node running another tenant's workspace — the mistake MAIN-363 fixed for
//! node-reported session updates. Fixing that by widening
//! `session_for_content` then let a node reach session CONTENT cross-tenant on
//! all nine session routes, far more than fetching a key needs.
//!
//! The shape here removes the pressure rather than relieving it: a git
//! credential is workspace data, so the question is "does this node hold this
//! checkout", which `require_node_may_use` plus the checkout row answer without
//! consulting tenants at all.
//!
//! Set `DATABASE_URL`.

use axum::extract::{Path, State};
use nook_control::auth::{AuthCtx, Principal};
use nook_control::routes::workspaces::git_key;
use nook_db::{params, Db, DbPool};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

fn node_ctx(node: NodeId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: UserId(Uuid::nil()),
        // The node's HOME tenant, which differs from the workspace's below.
        tenant_id: tenant,
        principal: Principal::Node(node),
        cookie_session: false,
    }
}

async fn add_checkout(db: &DbPool, tenant: TenantId, node: NodeId, workspace: WorkspaceId) {
    db.exec(
        "INSERT INTO node_workspaces (id, tenant_id, node_id, workspace_id, path)
         VALUES ($1, $2, $3, $4, '/w/repo')",
        params![NodeWorkspaceId::new(), tenant, node, workspace],
    )
    .await
    .expect("checkout");
}

/// The regression. A node holding a checkout of ANOTHER tenant's workspace must
/// be able to fetch its key — that is cross-tenant placement, and it is the case
/// this whole ticket exists to serve.
#[tokio::test]
async fn a_node_may_fetch_a_key_for_a_cross_tenant_workspace_it_holds() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;

    let home = bed.tenant("home").await;
    let other = bed.tenant("other").await;
    let (_u, person) = bed.user(home, "owner").await;
    let node = bed.node(home, person).await;
    let workspace = bed.workspace(other).await;
    add_checkout(&bed.db(), other, node, workspace).await;

    let res = git_key(
        State(state.clone()),
        node_ctx(node, home),
        Path((node, workspace)),
    )
    .await;
    assert!(
        res.is_ok(),
        "a node was refused a key for a workspace it holds because the tenants \
         differed — the MAIN-363 mistake, again"
    );

    bed.teardown().await;
}

/// A node may not name a workspace it does not hold. Without this the node id
/// alone would be enough to ask for any tenant's key.
#[tokio::test]
async fn a_node_may_not_name_a_workspace_it_does_not_hold() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;

    let tenant = bed.tenant("t").await;
    let (_u, person) = bed.user(tenant, "owner").await;
    let node = bed.node(tenant, person).await;
    // A real workspace, but no checkout of it on this node.
    let workspace = bed.workspace(tenant).await;

    let err = git_key(
        State(state.clone()),
        node_ctx(node, tenant),
        Path((node, workspace)),
    )
    .await
    .expect_err("a workspace this node does not hold is not found");
    assert!(matches!(err, nook_control::error::ApiError::NotFound));

    bed.teardown().await;
}

/// The confinement that must survive: a node speaks for itself and nothing else.
#[tokio::test]
async fn a_node_may_not_ask_on_behalf_of_another_machine() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;

    let tenant = bed.tenant("t").await;
    let (_u, person) = bed.user(tenant, "owner").await;
    let mine = bed.node(tenant, person).await;
    let theirs = bed.node(tenant, person).await;
    let workspace = bed.workspace(tenant).await;
    add_checkout(&bed.db(), tenant, theirs, workspace).await;

    let err = git_key(
        State(state.clone()),
        node_ctx(mine, tenant),
        Path((theirs, workspace)),
    )
    .await
    .expect_err("another machine's checkout is refused");
    assert!(matches!(
        err,
        nook_control::error::ApiError::ForbiddenMsg(_)
    ));

    bed.teardown().await;
}

/// Discovery's liveness opinion must not gate the key. A checkout flagged
/// missing has still been held by this node, and discovery gets that wrong:
/// a clone landing outside the scan roots was flagged missing while sitting on
/// disk (MAIN-363). Refusing here would turn that into an authentication
/// failure two layers away, and buys nothing — a checkout that really is gone
/// gives git no repo to run in.
#[tokio::test]
async fn a_checkout_flagged_missing_still_yields_its_key() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;

    let tenant = bed.tenant("t").await;
    let (_u, person) = bed.user(tenant, "owner").await;
    let node = bed.node(tenant, person).await;
    let workspace = bed.workspace(tenant).await;
    add_checkout(&bed.db(), tenant, node, workspace).await;
    bed.db()
        .exec(
            "UPDATE node_workspaces SET missing_at = now()
             WHERE node_id = $1 AND workspace_id = $2",
            params![node, workspace],
        )
        .await
        .expect("flag missing");

    let res = git_key(
        State(state.clone()),
        node_ctx(node, tenant),
        Path((node, workspace)),
    )
    .await;
    assert!(
        res.is_ok(),
        "a checkout flagged missing was refused its key — discovery's opinion \
         must not become an authentication failure"
    );

    bed.teardown().await;
}

/// A workspace pinning nothing yields 204, so the shim falls through to plain
/// ssh and public repos keep working untouched (AC-6).
#[tokio::test]
async fn an_unpinned_workspace_yields_no_key() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;

    let tenant = bed.tenant("t").await;
    let (_u, person) = bed.user(tenant, "owner").await;
    let node = bed.node(tenant, person).await;
    let workspace = bed.workspace(tenant).await;
    add_checkout(&bed.db(), tenant, node, workspace).await;

    let res = git_key(
        State(state.clone()),
        node_ctx(node, tenant),
        Path((node, workspace)),
    )
    .await
    .expect("unpinned is not an error");
    assert_eq!(res.status(), axum::http::StatusCode::NO_CONTENT);

    bed.teardown().await;
}
