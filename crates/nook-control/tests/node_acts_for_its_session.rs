//! A node acting in the tenant of a session it is running.
//!
//! Cross-tenant placement (MAIN-353) runs one org's checkout on another org's
//! machine. That worked for PLACEMENT and not for IDENTITY: the session's only
//! credential is the node token, which names the machine's own org — so every
//! `nook` command inside a session belonging to a DIFFERENT org was answered
//! about the machine's. `NOOK_TENANT_ID` was exported into the session, sent on
//! every request, and dropped by the server, silently.
//!
//! Refusing was right; refusing CATEGORICALLY was not. A node token must not be
//! able to NAME a tenant — that would make one shared box a skeleton key for
//! every org its owner belongs to. But it can safely inherit the tenant of work
//! the control plane itself placed on it, for as long as that work is live.
//!
//! So the scope comes from the session: the token proves which machine, the
//! session id proves which job, and the server checks that job is live and
//! HERE. These pin every part of that check, because each one is what stops the
//! rule widening back into "any tenant it likes".

use nook_control::auth::{AuthCtx, Principal};
use nook_control::repo::sessions::NewSession;
use nook_control::state::AppState;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

async fn node_in(bed: &TestBed, tenant: TenantId) -> NodeId {
    let id = NodeId::new();
    bed.db()
        .exec(
            "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status)
             VALUES ($1,$2,$3,$4,'online')",
            params![
                id,
                tenant,
                format!("n-{}", id.0.simple()),
                format!("h-{}", id.0.simple())
            ],
        )
        .await
        .expect("node");
    id
}

async fn session_on(
    bed: &TestBed,
    tenant: TenantId,
    node: NodeId,
    workspace: WorkspaceId,
) -> SessionId {
    bed.app_state()
        .await
        .sessions
        .create(NewSession {
            tenant,
            workspace_id: Some(workspace),
            node_id: node,
            name: "claude (managed)".into(),
            runtime: "claude".into(),
            created_by: None,
            checkout_id: None,
            managed: true,
            managed_purpose: ManagedPurpose::Access,
            managed_shard: 0,
            managed_shards: 1,
            interface: SessionInterface::Terminal,
        })
        .await
        .expect("session")
        .id
}

/// Resolve `AuthCtx` for a NODE token claiming `session`, exactly as a `nook`
/// command inside that session does.
async fn as_node(
    state: &AppState,
    node: NodeId,
    home: TenantId,
    session: Option<SessionId>,
) -> Result<AuthCtx, nook_control::error::ApiError> {
    let mut req = axum::http::Request::builder();
    if let Some(s) = session {
        req = req.header("x-nook-session", s.0.to_string());
    }
    let (mut parts, _) = req.body(axum::body::Body::empty()).unwrap().into_parts();
    // The credential itself is not under test — the scoping decision after it
    // is — so the node principal is constructed directly.
    let ctx = AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: UserId(Uuid::nil()),
        tenant_id: home,
        principal: Principal::Node(node),
        cookie_session: false,
    };
    nook_control::auth::retenant_for_test(state, ctx, &mut parts).await
}

#[tokio::test]
async fn a_node_takes_the_tenant_of_the_session_it_is_running() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let machines_org = bed.tenant("machines-org").await;
    let works_org = bed.tenant("works-org").await;
    let state = bed.app_state().await;

    // The shape in production: the machine is homed in one org, the workspace
    // and its session belong to another.
    // A real tenant has an owner, and the scoping resolves a user THERE — a
    // fixture without one would decline the swap and pass this test for a
    // reason production never has.
    let _ = bed.user(works_org, "owner").await;
    let node = node_in(&bed, machines_org).await;
    let ws = bed.workspace(works_org).await;
    let session = session_on(&bed, works_org, node, ws).await;

    let ctx = as_node(&state, node, machines_org, Some(session))
        .await
        .expect("scoped");
    assert_eq!(
        ctx.tenant_id, works_org,
        "the session's tenant, not the machine's"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn a_node_claiming_someone_elses_session_is_refused() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let a = bed.tenant("machines-org").await;
    let b = bed.tenant("works-org").await;
    let state = bed.app_state().await;

    // THE attack this rule has to stop: a machine naming a session it is not
    // running, to inherit a tenant nothing ever placed on it.
    let mine = node_in(&bed, a).await;
    let theirs = node_in(&bed, b).await;
    let ws = bed.workspace(b).await;
    let not_mine = session_on(&bed, b, theirs, ws).await;

    let err = as_node(&state, mine, a, Some(not_mine))
        .await
        .expect_err("must refuse");
    assert!(
        err.to_string().contains("not running on this machine"),
        "{err}"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn a_finished_session_is_not_a_licence() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let machines_org = bed.tenant("machines-org").await;
    let works_org = bed.tenant("works-org").await;
    let state = bed.app_state().await;

    // Owned, so what stops the swap below is the session's STATUS and nothing
    // else — without this the test would pass on an unresolvable owner instead.
    let _ = bed.user(works_org, "owner").await;
    let node = node_in(&bed, machines_org).await;
    let ws = bed.workspace(works_org).await;
    let session = session_on(&bed, works_org, node, ws).await;
    bed.db()
        .exec(
            "UPDATE sessions SET status = 'exited' WHERE id = $1",
            params![session],
        )
        .await
        .expect("end it");

    // The row keeps its tenant forever; honouring it would let a machine act
    // for work that finished months ago.
    let ctx = as_node(&state, node, machines_org, Some(session))
        .await
        .expect("falls back");
    assert_eq!(
        ctx.tenant_id, machines_org,
        "an ended session grants nothing — back to the machine's own tenant"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn a_node_claiming_no_session_keeps_its_own_tenant() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let home = bed.tenant("machines-org").await;
    let state = bed.app_state().await;
    let node = node_in(&bed, home).await;

    // The heartbeat path, and every other call a machine makes about itself.
    let ctx = as_node(&state, node, home, None).await.expect("unchanged");
    assert_eq!(ctx.tenant_id, home);

    bed.teardown().await;
}

#[tokio::test]
async fn the_user_identity_moves_with_the_tenant() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let machines_org = bed.tenant("machines-org").await;
    let works_org = bed.tenant("works-org").await;
    let state = bed.app_state().await;

    let (mine, _) = bed.user(machines_org, "owner").await;
    let (theirs, _) = bed.user(works_org, "owner").await;
    assert_ne!(mine, theirs, "a user row is per tenant");

    let node = node_in(&bed, machines_org).await;
    let ws = bed.workspace(works_org).await;
    let session = session_on(&bed, works_org, node, ws).await;

    let ctx = as_node(&state, node, machines_org, Some(session))
        .await
        .expect("scoped");
    assert_eq!(ctx.tenant_id, works_org);
    assert_ne!(
        ctx.user_id, mine,
        "must not carry the machine org's user into another tenant"
    );

    bed.teardown().await;
}
