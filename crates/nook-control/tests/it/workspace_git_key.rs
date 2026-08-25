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
//! checkout", which the machine-only guard plus the checkout row answer without
//! consulting tenants at all.
//!
//! A third wrong shape is pinned too. `require_node_may_use` looked like the
//! right question but admits a USER principal through
//! `require_person_may_use_node`, which returns `Ok` for any member of a tenant
//! the node is SHARED with — so a session cookie could read a decrypted private
//! key off a shared operator node. The route is now machine-only (owner's ruling
//! on MAIN-367, 2026-08-03) and `a_user_credential_is_refused_outright` holds
//! that line.
//!
//! Set `DATABASE_URL`.

use axum::extract::{Path, State};
use nook_control::auth::{AuthCtx, Principal};
use nook_control::routes::workspaces::git_key;
use nook_db::dialect::{time_math, type_mapping};
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

/// A signed-in human — the principal that must NOT reach this route. `user_id`
/// and `tenant_id` are real rather than nil so the refusal cannot be an accident
/// of an empty context: this is a caller who would pass every other node guard.
fn user_ctx(tenant: TenantId, user: UserId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::new_v4()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: true,
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

/// A signed-in human must not be able to pull a private key out of the control
/// plane, however privileged they are on the node.
///
/// The gap this closes: the route used `require_node_may_use`, whose user leg is
/// `require_person_may_use_node`, which returns `Ok` for the node's owner AND for
/// any member of a tenant the node is SHARED with. Shared operator nodes running
/// other tenants' workspaces are the design, not an edge case, so a session
/// cookie was enough to read any workspace's key off one.
///
/// The caller here is the node's own OWNER — the strongest human claim there is.
/// If even they are refused, no weaker one gets through.
#[tokio::test]
async fn a_user_credential_is_refused_outright() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;

    let tenant = bed.tenant("t").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let node = bed.node(tenant, person).await;
    let workspace = bed.workspace(tenant).await;
    add_checkout(&bed.db(), tenant, node, workspace).await;

    let err = git_key(
        State(state.clone()),
        user_ctx(tenant, user),
        Path((node, workspace)),
    )
    .await
    .expect_err("a user credential must not fetch key material");
    assert!(
        matches!(err, nook_control::error::ApiError::ForbiddenMsg(_)),
        "expected a forbidden refusal, got {err:?} — a user reaching this route \
         at all means the machine-only guard is gone"
    );

    bed.teardown().await;
}

/// Discovery's liveness opinion must not gate the key while it is FRESH. A
/// checkout flagged missing moments ago is very likely still on disk: a clone
/// landing outside the scan roots was flagged missing while sitting there
/// perfectly (MAIN-363), and refusing turns that into an authentication failure
/// two layers away.
#[tokio::test]
async fn a_recently_missing_checkout_still_yields_its_key() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;

    let tenant = bed.tenant("t").await;
    let (_u, person) = bed.user(tenant, "owner").await;
    let node = bed.node(tenant, person).await;
    let workspace = bed.workspace(tenant).await;
    add_checkout(&bed.db(), tenant, node, workspace).await;
    // Through the SEAM, like its long-dead twin below. `CURRENT_TIMESTAMP` was
    // the engine-independent spelling before MAIN-442; it still runs on both,
    // but on SQLite it writes a second-resolution form that nothing else in
    // that database uses, and this row is compared against `now()`.
    let db = bed.db();
    db.exec(
        &format!(
            "UPDATE node_workspaces SET missing_at = {}
             WHERE node_id = $1 AND workspace_id = $2",
            type_mapping(db.engine()).now()
        ),
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
        "a checkout flagged missing seconds ago was refused its key — discovery's \
         momentary opinion must not become an authentication failure"
    );

    bed.teardown().await;
}

/// The other half, and the one that makes "prune" mean something.
///
/// Tolerating EVERY tombstone — which this route did before — meant removing a
/// workspace from a node never withdrew its key access. The row survives until
/// `reap_tombstoned`, which runs at `workspace_missing_retention_secs` (default
/// 7 days) behind `loops.enabled`, and loops ship OFF; so in practice the access
/// never ended, while pruning reads to an operator as a revocation gesture.
///
/// An hour is far past the 15-minute grace and far short of the 7-day retention,
/// so this fails if the window is dropped OR widened to the retention constant.
#[tokio::test]
async fn a_long_dead_checkout_is_refused_its_key() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;

    let tenant = bed.tenant("t").await;
    let (_u, person) = bed.user(tenant, "owner").await;
    let node = bed.node(tenant, person).await;
    let workspace = bed.workspace(tenant).await;
    add_checkout(&bed.db(), tenant, node, workspace).await;
    // Through the SEAM, which is how production writes it. Before MAIN-442 a
    // bound `chrono` value wrote an RFC3339 string
    // (`2026-08-03T09:00:00+00:00`) while `type_mapping(engine).now()` wrote
    // `2026-08-03 09:00:00`; the repo compares them as TEXT on SQLite, and `T`
    // sorts above the space, so an hour-old tombstone read as NEWER than the
    // cutoff — green on Postgres, red on SQLite. Both halves render one form
    // now, but writing the way production writes is still what makes this
    // assert the production comparison.
    let db = bed.db();
    db.exec(
        &format!(
            "UPDATE node_workspaces SET missing_at = {}
             WHERE node_id = $1 AND workspace_id = $2",
            time_math(db.engine()).now_minus_scaled("$3", "1 second")
        ),
        params![node, workspace, 3600_i64],
    )
    .await
    .expect("flag long-missing");

    let err = git_key(
        State(state.clone()),
        node_ctx(node, tenant),
        Path((node, workspace)),
    )
    .await
    .expect_err("a checkout pruned an hour ago must not still yield its key");
    assert!(
        matches!(err, nook_control::error::ApiError::NotFound),
        "expected NotFound for a long-dead checkout, got {err:?}"
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
