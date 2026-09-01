//! MAIN-508: a node's loop-job capacity, settable centrally like its port range.
//!
//! The API half — the precedence between the central value and the machine's
//! own `NOOK_MAX_LOOP_JOBS`, the owner gate, and what the endpoint refuses.
//! Placement's half (that the new number is honoured with no restart) is in
//! `executor_selection.rs`, where the dispatcher lives.

use nook_control::routes::nodes::{get_capacity, set_capacity};
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;

use axum::extract::{Path, State};
use axum::Json;

use nook_control::auth::{AuthCtx, Principal};
use uuid::Uuid;

fn user_ctx(user: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: false,
    }
}

/// What the node reports about itself, as its agent would at register.
async fn reports(bed: &TestBed, node: NodeId, caps: serde_json::Value) {
    bed.db()
        .exec(
            "UPDATE nodes SET capabilities = $2 WHERE id = $1",
            params![node, caps],
        )
        .await
        .expect("report capabilities");
}

fn set(max: Option<i64>) -> Json<SetNodeCapacityRequest> {
    Json(SetNodeCapacityRequest { max_loop_jobs: max })
}

/// AC-6: nothing set centrally, and the machine's own number stands — an
/// upgrade changes no node's behaviour.
#[tokio::test]
async fn the_nodes_own_number_stands_until_someone_sets_one() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("capacity").await;
    let (owner, person) = bed.user(tenant, "member").await;
    let node = bed.node(tenant, person).await;
    reports(&bed, node, serde_json::json!({ "max_loop_jobs": 2 })).await;

    let got = get_capacity(
        State(bed.app_state().await),
        user_ctx(owner, tenant),
        Path(node),
    )
    .await
    .expect("read")
    .0;
    assert_eq!((got.effective, got.source.as_str()), (2, "node"));
    assert_eq!(got.operator, None);

    bed.teardown().await;
}

/// AC-1/AC-3: the central value beats the machine's env, and clearing it hands
/// the decision back. Both directions, because "set" that cannot be undone is
/// how an operator ends up unable to restore a machine's own sizing.
#[tokio::test]
async fn the_central_value_wins_and_clearing_gives_it_back() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("capacity").await;
    let (owner, person) = bed.user(tenant, "member").await;
    let node = bed.node(tenant, person).await;
    reports(&bed, node, serde_json::json!({ "max_loop_jobs": 2 })).await;
    let state = bed.app_state().await;
    let auth = user_ctx(owner, tenant);

    let got = set_capacity(State(state.clone()), auth, Path(node), set(Some(6)))
        .await
        .expect("set")
        .0;
    assert_eq!((got.effective, got.source.as_str()), (6, "operator"));
    // The machine's own number is still reported, so a UI can say what
    // clearing will fall back to instead of showing a number from nowhere.
    assert_eq!(got.advertised, Some(2));

    let got = set_capacity(State(state.clone()), auth, Path(node), set(None))
        .await
        .expect("clear")
        .0;
    assert_eq!((got.effective, got.source.as_str()), (2, "node"));
    assert_eq!(got.operator, None);

    bed.teardown().await;
}

/// AC-5: `0` is a cordon and survives every read as one — never rewritten to
/// "unset", never confused with a busy machine.
#[tokio::test]
async fn zero_is_stored_as_a_cordon_and_not_as_absent() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("capacity").await;
    let (owner, person) = bed.user(tenant, "member").await;
    let node = bed.node(tenant, person).await;
    reports(&bed, node, serde_json::json!({ "max_loop_jobs": 4 })).await;
    let state = bed.app_state().await;
    let auth = user_ctx(owner, tenant);

    let got = set_capacity(State(state.clone()), auth, Path(node), set(Some(0)))
        .await
        .expect("cordon")
        .0;
    assert_eq!((got.effective, got.source.as_str()), (0, "operator"));

    let got = get_capacity(State(state.clone()), auth, Path(node))
        .await
        .expect("read back")
        .0;
    assert_eq!(
        (got.effective, got.operator),
        (0, Some(0)),
        "a re-read must not resolve the cordon back to the node's 4"
    );

    bed.teardown().await;
}

/// AC-3's escape hatch. A pinned host keeps the last word, and the write is
/// REFUSED rather than stored and overruled — a setting that silently does
/// nothing is worse than one that says no.
#[tokio::test]
async fn a_pinned_host_refuses_the_central_write_and_says_why() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("capacity").await;
    let (owner, person) = bed.user(tenant, "member").await;
    let node = bed.node(tenant, person).await;
    reports(
        &bed,
        node,
        serde_json::json!({ "max_loop_jobs": 3, "max_loop_jobs_pinned": true }),
    )
    .await;
    let state = bed.app_state().await;
    let auth = user_ctx(owner, tenant);

    let err = set_capacity(State(state.clone()), auth, Path(node), set(Some(9)))
        .await
        .expect_err("a pinned host is not settable from here");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("NOOK_MAX_LOOP_JOBS_PINNED"),
        "the refusal names the variable to unset: {msg}"
    );

    let got = get_capacity(State(state.clone()), auth, Path(node))
        .await
        .expect("read")
        .0;
    assert_eq!((got.effective, got.source.as_str()), (3, "host"));
    assert!(got.pinned);

    bed.teardown().await;
}

/// A number nobody meant to type. Refused where it is still the number the
/// operator is looking at, rather than becoming a fleet handed thousands of
/// jobs.
#[tokio::test]
async fn a_negative_or_absurd_capacity_is_refused() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("capacity").await;
    let (owner, person) = bed.user(tenant, "member").await;
    let node = bed.node(tenant, person).await;
    let state = bed.app_state().await;
    let auth = user_ctx(owner, tenant);

    assert!(
        set_capacity(State(state.clone()), auth, Path(node), set(Some(-1)))
            .await
            .is_err(),
        "negative is not a capacity"
    );
    assert!(
        set_capacity(State(state.clone()), auth, Path(node), set(Some(4000)))
            .await
            .is_err(),
        "past the typo guard"
    );

    bed.teardown().await;
}

/// Owner-gated exactly as the port range is: sizing somebody's machine is the
/// machine owner's call, not any teammate's.
#[tokio::test]
async fn only_the_machines_owner_may_set_it() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("capacity").await;
    let (_owner, person) = bed.user(tenant, "member").await;
    let (other, _) = bed.user(tenant, "member").await;
    let node = bed.node(tenant, person).await;
    // Shared, so the teammate can SEE it — otherwise the refusal would be a 404
    // about visibility and would prove nothing about the owner gate.
    bed.db()
        .exec(
            "UPDATE nodes SET shared = true WHERE id = $1",
            params![node],
        )
        .await
        .expect("share");
    let state = bed.app_state().await;

    assert!(
        set_capacity(
            State(state.clone()),
            user_ctx(other, tenant),
            Path(node),
            set(Some(4))
        )
        .await
        .is_err(),
        "a teammate does not size someone else's machine"
    );
    // …and they can still read it, which is the same grade of fact as the
    // machine's port range.
    assert!(
        get_capacity(State(state.clone()), user_ctx(other, tenant), Path(node))
            .await
            .is_ok()
    );

    bed.teardown().await;
}
