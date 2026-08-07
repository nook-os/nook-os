//! MAIN-445: how many review loops a workspace is owed.
//!
//! The whole card rests on THREE states where the old code had one, so what is
//! worth pinning here is that they stay three all the way through the API:
//! unset (use the build's default), an explicit 0 (off), and n. A read that
//! resolved unset to `1` would be indistinguishable from someone having set 1,
//! and the CLI could no longer tell a person whether anyone ever touched it.

use axum::extract::{Path, State};
use axum::Json;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::error::ApiError;
use nook_control::routes::workspaces::{get_review_loop, set_review_loop};
use nook_testkit::TestBed;
use nook_types::*;
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

fn req(v: serde_json::Value) -> Json<SetReviewLoopRequest> {
    Json(SetReviewLoopRequest { replicas: v })
}

#[tokio::test]
async fn a_workspace_starts_unset_and_a_count_round_trips() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("reviewloop").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let auth = user_ctx(user, tenant);

    let fresh = get_review_loop(State(state.clone()), auth, Path(ws))
        .await
        .expect("read")
        .0;
    assert_eq!(fresh.replicas, None, "a new workspace is unset, not 1");

    let set = set_review_loop(State(state.clone()), auth, Path(ws), req(3.into()))
        .await
        .expect("set")
        .0;
    assert_eq!(set.replicas, Some(3));

    let reread = get_review_loop(State(state.clone()), auth, Path(ws))
        .await
        .expect("re-read")
        .0;
    assert_eq!(reread.replicas, Some(3), "the write path persisted");

    bed.teardown().await;
}

/// The distinction the nullable column exists for. `0` and unset are both
/// falsy, both "not a positive number", and mean opposite things: one turns
/// reviewing OFF for this repo, the other asks for the build's default of one.
#[tokio::test]
async fn zero_is_stored_and_is_not_the_same_as_unset() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("reviewloop").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let auth = user_ctx(user, tenant);

    let off = set_review_loop(State(state.clone()), auth, Path(ws), req(0.into()))
        .await
        .expect("set 0")
        .0;
    assert_eq!(off.replicas, Some(0), "0 is a value, not an absence");

    // And it survives the round trip as 0 rather than decaying to null.
    let reread = get_review_loop(State(state.clone()), auth, Path(ws))
        .await
        .expect("re-read")
        .0;
    assert_eq!(reread.replicas, Some(0));

    // null is reachable again, which is what lets someone undo an 0 without
    // having to know what the default happens to be.
    let cleared = set_review_loop(
        State(state.clone()),
        auth,
        Path(ws),
        req(serde_json::Value::Null),
    )
    .await
    .expect("clear")
    .0;
    assert_eq!(cleared.replicas, None);

    bed.teardown().await;
}

/// AC-2's rejection rule. Each of these is a different way to not be a
/// non-negative integer, and every one must name the field — a 400 that does
/// not say `replicas` costs the caller a round trip to find out which key.
#[tokio::test]
async fn anything_but_a_non_negative_integer_is_a_400_naming_the_field() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("reviewloop").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let auth = user_ctx(user, tenant);

    for bad in [
        serde_json::json!(-1),
        serde_json::json!("3"),
        serde_json::json!(1.5),
        serde_json::json!(true),
        serde_json::json!([1]),
        serde_json::json!(i64::from(i32::MAX) + 1),
    ] {
        let label = bad.to_string();
        let err = set_review_loop(State(state.clone()), auth, Path(ws), req(bad))
            .await
            .expect_err(&format!("{label} must be refused"));
        match err {
            ApiError::BadRequest(m) => assert!(
                m.contains("replicas"),
                "{label}: the message must name the field, got {m:?}"
            ),
            other => panic!("{label}: expected a 400, got {other:?}"),
        }
    }

    // A refused write leaves the stored value alone rather than half-applying.
    let after = get_review_loop(State(state.clone()), auth, Path(ws))
        .await
        .expect("read")
        .0;
    assert_eq!(after.replicas, None);

    bed.teardown().await;
}

/// Tenant scoping, the same rule every other workspace read follows: another
/// tenant's workspace is NOT FOUND, not forbidden — the existence of a repo in
/// someone else's fleet is not ours to confirm.
#[tokio::test]
async fn another_tenants_workspace_is_not_found() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let mine = bed.tenant("mine").await;
    let theirs = bed.tenant("theirs").await;
    let (user, _) = bed.user(mine, "member").await;
    let their_ws = bed.workspace(theirs).await;
    let state = bed.app_state().await;
    let auth = user_ctx(user, mine);

    assert!(matches!(
        get_review_loop(State(state.clone()), auth, Path(their_ws))
            .await
            .expect_err("read"),
        ApiError::NotFound
    ));
    assert!(matches!(
        set_review_loop(State(state.clone()), auth, Path(their_ws), req(2.into()))
            .await
            .expect_err("write"),
        ApiError::NotFound
    ));

    bed.teardown().await;
}
