//! MAIN-367: the git-key endpoint is gated by PRINCIPAL, not by tenant equality.
//!
//! The bug this pins was found in review and is the second appearance of one
//! shape: scoping a NODE's request by the node's own tenant. MAIN-363 fixed it
//! for node-reported session updates; this endpoint reintroduced it, and the
//! symptom was identical — a cross-tenant session silently got no credential,
//! so git fell back to a key the repo does not authorize.
//!
//! Cross-tenant is not an edge case here. It is the placement MAIN-353 made
//! ordinary and the one this whole ticket exists to serve: an operator node
//! homed in one tenant running another tenant's workspace.
//!
//! Set `DATABASE_URL`.

use axum::extract::{Path, State};
use nook_control::auth::{AuthCtx, Principal};
use nook_control::routes::sessions::git_key;
use nook_db::{params, Db, DbPool};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

fn node_ctx(node: NodeId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: UserId(Uuid::nil()),
        // The node's HOME tenant, which is the whole point: it differs from the
        // tenant that owns the session below.
        tenant_id: tenant,
        principal: Principal::Node(node),
        cookie_session: false,
    }
}

async fn add_session(db: &DbPool, tenant: TenantId, node: NodeId) -> SessionId {
    let id = SessionId::new();
    db.exec(
        "INSERT INTO sessions (id, tenant_id, node_id, name, runtime, status)
         VALUES ($1, $2, $3, 'work', 'bash', 'running')",
        params![id, tenant, node],
    )
    .await
    .expect("session");
    id
}

/// The regression. A node fetching a key for a session on ITS OWN machine must
/// succeed even though that session belongs to a different tenant.
#[tokio::test]
async fn a_node_may_fetch_a_key_for_a_cross_tenant_session_on_itself() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;

    let home = bed.tenant("home").await;
    let other = bed.tenant("other").await;
    let (_user, person) = bed.user(home, "owner").await;
    let node = bed.node(home, person).await;

    // The session belongs to the OTHER tenant while running on this node —
    // exactly what cross-tenant placement produces.
    let session = add_session(&bed.db(), other, node).await;

    let res = git_key(State(state.clone()), node_ctx(node, home), Path(session)).await;
    assert!(
        res.is_ok(),
        "a node was refused a key for a session on its own machine because the \
         tenants differed — the MAIN-363 mistake, again"
    );

    bed.teardown().await;
}

/// The confinement that must survive the fix: a node speaks for its own machine
/// and nothing else. Widening the gate to make the case above work must not
/// turn one compromised box into every box.
#[tokio::test]
async fn a_node_may_not_fetch_a_key_for_another_machines_session() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;

    let tenant = bed.tenant("t").await;
    let (_user, person) = bed.user(tenant, "owner").await;
    let mine = bed.node(tenant, person).await;
    let theirs = bed.node(tenant, person).await;

    let session = add_session(&bed.db(), tenant, theirs).await;

    let err = git_key(State(state.clone()), node_ctx(mine, tenant), Path(session))
        .await
        .expect_err("another machine's session is refused");
    assert!(
        matches!(err, nook_control::error::ApiError::ForbiddenMsg(_)),
        "expected a forbidden refusal, got {err:?}"
    );

    bed.teardown().await;
}

/// An ad-hoc terminal has no workspace, so there is no repo whose key it could
/// want. 204 rather than an error: the shim reads that as "nothing pinned" and
/// falls through to plain ssh, which is correct for a terminal in `$HOME`.
#[tokio::test]
async fn a_session_with_no_workspace_yields_no_key() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;

    let tenant = bed.tenant("t").await;
    let (_user, person) = bed.user(tenant, "owner").await;
    let node = bed.node(tenant, person).await;
    let session = add_session(&bed.db(), tenant, node).await;

    let res = git_key(State(state.clone()), node_ctx(node, tenant), Path(session))
        .await
        .expect("no workspace is not an error");
    assert_eq!(res.status(), axum::http::StatusCode::NO_CONTENT);

    bed.teardown().await;
}
