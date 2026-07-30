//! The sessionless authorize endpoint (MAIN-290).
//!
//! The point of the card is what does NOT happen: no session row, no terminal,
//! no `StartAuthSession`. That is asserted against the database rather than by
//! reading the code, because "we did not create a session" is exactly the kind
//! of claim that quietly stops being true.

use axum::extract::State;
use axum::Json;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::routes::runtime_auth::{start, RuntimeAuthRequest};
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;

fn auth(user: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId::new(),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: false,
    }
}

/// Point the `claude` descriptor at a dead port.
///
/// Process-global, so it is set by every test that needs it rather than left to
/// whichever ran first — these run in parallel in one binary. Nothing is ever
/// dialled successfully: a flow that does spawn fails transport immediately,
/// which is all these tests need it to do.
fn configure_descriptor() {
    std::env::set_var(
        "NOOK_CLAUDE_DEVICE_AUTH_ENDPOINT",
        "http://127.0.0.1:1/device",
    );
    std::env::set_var("NOOK_CLAUDE_TOKEN_ENDPOINT", "http://127.0.0.1:1/token");
    std::env::set_var("NOOK_CLAUDE_CLIENT_ID", "test-client");
}

async fn sessions_in(bed: &TestBed, tenant: TenantId) -> i64 {
    bed.db()
        .query_scalar(
            "SELECT count(*) FROM sessions WHERE tenant_id = $1",
            params![tenant],
        )
        .await
        .expect("count sessions")
}

/// AC-1: an unknown runtime is refused by name, before anything starts.
#[tokio::test]
async fn an_unknown_runtime_is_refused_naming_it() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ra-unknown").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let node = bed.node(tenant, person).await;
    let state = bed.app_state().await;

    let res = start(
        State(state),
        auth(user, tenant),
        Json(RuntimeAuthRequest {
            runtime: "nonesuch".into(),
            node_ids: vec![node],
        }),
    )
    .await;

    let err = format!("{:?}", res.expect_err("refused"));
    assert!(
        err.contains("nonesuch"),
        "the refusal names the runtime: {err}"
    );
    assert_eq!(
        sessions_in(&bed, tenant).await,
        0,
        "a refused request creates nothing"
    );
    bed.teardown().await;
}

/// AC-1: delivering to nobody is a client error, not a no-op that reports
/// success — an authorize with no target has approved a credential for nothing.
#[tokio::test]
async fn an_empty_node_list_is_refused() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ra-empty").await;
    let (user, _person) = bed.user(tenant, "owner").await;
    let state = bed.app_state().await;

    let res = start(
        State(state),
        auth(user, tenant),
        Json(RuntimeAuthRequest {
            runtime: "claude".into(),
            node_ids: vec![],
        }),
    )
    .await;
    assert!(res.is_err(), "an empty node list must be refused");
    bed.teardown().await;
}

/// AC-7, the card's whole point: whatever this endpoint does, it does not make
/// a session.
///
/// Both paths are exercised: `claude` is configured, so that request is
/// ACCEPTED and spawns a real flow (which then fails transport against a dead
/// port); `nonesuch` is refused. Neither may touch `sessions`, and the count is
/// read from the database rather than inferred from the code.
#[tokio::test]
async fn the_sessionless_endpoint_never_creates_a_session() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ra-nosession").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let node = bed.node(tenant, person).await;
    let state = bed.app_state().await;
    configure_descriptor();

    let before = sessions_in(&bed, tenant).await;

    let accepted = start(
        State(state.clone()),
        auth(user, tenant),
        Json(RuntimeAuthRequest {
            runtime: "claude".into(),
            node_ids: vec![node],
        }),
    )
    .await
    .expect("a configured runtime on the caller's own node is accepted");
    assert_eq!(
        accepted.0,
        axum::http::StatusCode::ACCEPTED,
        "202, not a result"
    );

    for runtime in ["hermes", "nonesuch"] {
        let _ = start(
            State(state.clone()),
            auth(user, tenant),
            Json(RuntimeAuthRequest {
                runtime: runtime.into(),
                node_ids: vec![node],
            }),
        )
        .await;
    }

    assert_eq!(
        sessions_in(&bed, tenant).await,
        before,
        "the sessionless path created a session row — that is the one thing it must never do"
    );
    bed.teardown().await;
}

/// A node in another tenant is a 404, not a 403 — the same no-existence-oracle
/// rule the session-based endpoint follows, applied before any flow starts.
///
/// Needs a resolvable descriptor to reach the node check at all, because the
/// endpoint validates the REQUEST before it authorizes the nodes (see
/// `the_request_is_validated_before_any_node_is_named`). The endpoints are
/// never dialled: the 404 returns before the flow is spawned.
#[tokio::test]
async fn another_tenants_node_is_not_found() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    configure_descriptor();
    let mine = bed.tenant("ra-mine").await;
    let theirs = bed.tenant("ra-theirs").await;
    let (user, _p) = bed.user(mine, "owner").await;
    let (_u2, person2) = bed.user(theirs, "owner").await;
    let their_node = bed.node(theirs, person2).await;
    let state = bed.app_state().await;

    let res = start(
        State(state),
        auth(user, mine),
        Json(RuntimeAuthRequest {
            runtime: "claude".into(),
            node_ids: vec![their_node],
        }),
    )
    .await;

    let err = format!("{:?}", res.expect_err("refused"));
    assert!(
        err.contains("NotFound"),
        "another tenant's node must be 404, not 403: {err}"
    );
    assert_eq!(sessions_in(&bed, mine).await, 0);
    bed.teardown().await;
}

/// The precedence, pinned because it is a decision rather than an accident: the
/// REQUEST is validated before any node is authorized.
///
/// A 400 about the runtime or an empty list leaks nothing, whereas the node
/// check reveals whether a node exists — so the harmless refusal goes first and
/// the answer does not depend on which runtimes a deployment has configured.
#[tokio::test]
async fn the_request_is_validated_before_any_node_is_named() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let mine = bed.tenant("ra-order").await;
    let theirs = bed.tenant("ra-order-other").await;
    let (user, _p) = bed.user(mine, "owner").await;
    let (_u2, person2) = bed.user(theirs, "owner").await;
    let their_node = bed.node(theirs, person2).await;
    let state = bed.app_state().await;

    // A bad runtime AND a node the caller cannot see: the runtime wins, so the
    // caller learns nothing about the node.
    let res = start(
        State(state),
        auth(user, mine),
        Json(RuntimeAuthRequest {
            runtime: "nonesuch".into(),
            node_ids: vec![their_node],
        }),
    )
    .await;
    let err = format!("{:?}", res.expect_err("refused"));
    assert!(
        err.contains("nonesuch") && !err.contains("NotFound"),
        "the request-shape refusal must come first: {err}"
    );
    bed.teardown().await;
}
