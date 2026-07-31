//! MAIN-314: a node's labels and taints — the inputs placement will read.
//!
//! No scheduling here (NG-1): these prove the attributes exist, are derived
//! where they should be derived, and are gated to the machine's owner.

use nook_control::routes::nodes::{get_placement, set_placement};
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;

use axum::extract::{Path, State};
use axum::Json;

use nook_control::auth::{AuthCtx, Principal};
use uuid::Uuid;

/// The same shape the other route-level tests build (there is no shared helper
/// module in `tests/`, so each file constructs its own).
fn user_ctx(user: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: false,
    }
}

#[tokio::test]
async fn os_and_arch_are_derived_and_cannot_be_set() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("placement").await;
    let (owner, person) = bed.user(tenant, "member").await;
    let node = bed.node(tenant, person).await;

    // The agent reports both: platform is its own column, arch rides in
    // capabilities.
    bed.db()
        .exec(
            "UPDATE nodes SET platform = 'linux', capabilities = $2 WHERE id = $1",
            params![node, serde_json::json!({"architecture": "aarch64"})],
        )
        .await
        .expect("report platform");

    let state = bed.app_state().await;
    let auth = user_ctx(owner, tenant);

    let got = get_placement(State(state.clone()), auth, Path(node))
        .await
        .expect("read")
        .0;
    assert_eq!(got.labels.get("os").map(String::as_str), Some("linux"));
    assert_eq!(got.labels.get("arch").map(String::as_str), Some("aarch64"));
    // Derived, so not among the operator's own.
    assert!(got.custom_labels.is_empty());

    // …and an operator cannot store one, because a stored copy could drift
    // from what the node actually reports.
    let refused = set_placement(
        State(state.clone()),
        auth,
        Path(node),
        Json(SetNodePlacementRequest {
            labels: [("os".to_string(), "windows".to_string())].into(),
            taints: vec![],
        }),
    )
    .await;
    assert!(refused.is_err(), "`os` must not be settable");

    bed.teardown().await;
}

#[tokio::test]
async fn custom_labels_and_taints_round_trip_and_replace_wholesale() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("placement").await;
    let (owner, person) = bed.user(tenant, "member").await;
    let node = bed.node(tenant, person).await;
    let state = bed.app_state().await;
    let auth = user_ctx(owner, tenant);

    let set = |labels: Vec<(&'static str, &'static str)>, taints: Vec<NodeTaint>| {
        let (state, auth) = (state.clone(), auth);
        async move {
            set_placement(
                State(state),
                auth,
                Path(node),
                Json(SetNodePlacementRequest {
                    labels: labels
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                    taints,
                }),
            )
            .await
        }
    };

    let got = set(
        vec![("gpu", "true"), ("zone", "eu")],
        vec![NodeTaint {
            key: "no-linux".into(),
            effect: "NoSchedule".into(),
        }],
    )
    .await
    .expect("set")
    .0;
    assert_eq!(
        got.custom_labels.get("gpu").map(String::as_str),
        Some("true")
    );
    assert_eq!(got.taints.len(), 1);
    assert_eq!(got.taints[0].key, "no-linux");

    // A write REPLACES: sending one label drops the other, which is the whole
    // reason this is a PUT and not a PATCH.
    let got = set(vec![("zone", "us")], vec![]).await.expect("replace").0;
    assert_eq!(got.custom_labels.len(), 1);
    assert_eq!(
        got.custom_labels.get("zone").map(String::as_str),
        Some("us")
    );
    assert!(got.taints.is_empty());

    // It persists — the read path sees what the write path stored.
    let reread = get_placement(State(state.clone()), auth, Path(node))
        .await
        .expect("re-read")
        .0;
    assert_eq!(reread.custom_labels, got.custom_labels);

    bed.teardown().await;
}

#[tokio::test]
async fn only_the_owner_may_set_placement() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("placement").await;
    let (_owner, person) = bed.user(tenant, "member").await;
    let (stranger, _) = bed.user(tenant, "member").await;
    let node = bed.node(tenant, person).await;
    // Shared, so the stranger can SEE it — otherwise this would prove a 404,
    // not a refusal.
    bed.db()
        .exec(
            "UPDATE nodes SET shared = true WHERE id = $1",
            params![node],
        )
        .await
        .expect("share");

    let state = bed.app_state().await;
    let theirs = user_ctx(stranger, tenant);

    // Seeing it is fine…
    let _ = get_placement(State(state.clone()), theirs, Path(node))
        .await
        .expect("a shared node's placement is readable");

    // …setting it is not. Steering where work lands is the owner's call.
    let refused = set_placement(
        State(state.clone()),
        theirs,
        Path(node),
        Json(SetNodePlacementRequest {
            labels: [("zone".to_string(), "us".to_string())].into(),
            taints: vec![],
        }),
    )
    .await;
    assert!(refused.is_err(), "a non-owner must not set placement");

    bed.teardown().await;
}

#[tokio::test]
async fn a_taint_needs_a_key_and_a_known_effect() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("placement").await;
    let (owner, person) = bed.user(tenant, "member").await;
    let node = bed.node(tenant, person).await;
    let state = bed.app_state().await;
    let auth = user_ctx(owner, tenant);

    for taint in [
        NodeTaint {
            key: "  ".into(),
            effect: "NoSchedule".into(),
        },
        NodeTaint {
            key: "no-linux".into(),
            effect: "Evict".into(),
        },
    ] {
        let r = set_placement(
            State(state.clone()),
            auth,
            Path(node),
            Json(SetNodePlacementRequest {
                labels: Default::default(),
                taints: vec![taint],
            }),
        )
        .await;
        assert!(r.is_err(), "a malformed taint must be refused");
    }

    bed.teardown().await;
}
