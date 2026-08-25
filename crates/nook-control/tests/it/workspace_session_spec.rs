//! MAIN-315: a workspace's declared desired session state.
//!
//! No reconciler (NG-1) — nothing acts on these. What is worth pinning is the
//! distinction the column exists for: **unmanaged is not the same as wanting
//! zero sessions**, and a spec that cannot mean anything is refused rather than
//! stored for a reconciler to trip over later.

use axum::extract::{Path, State};
use axum::Json;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::routes::workspaces::{get_session_spec, set_session_spec};
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

fn spec(runtime: &str, replicas: Replicas) -> SessionSpec {
    SessionSpec {
        runtime: runtime.to_string(),
        node_selector: Default::default(),
        tolerations: vec![],
        replicas,
    }
}

#[tokio::test]
async fn a_workspace_starts_unmanaged_and_a_spec_round_trips() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("spec").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let auth = user_ctx(user, tenant);

    // Unmanaged: the column is NULL, which is NOT "wants zero sessions".
    let got = get_session_spec(State(state.clone()), auth, Path(ws))
        .await
        .expect("read")
        .0;
    assert!(got.is_none(), "a fresh workspace is unmanaged");

    let want = SessionSpec {
        runtime: "claude".into(),
        node_selector: [("os".to_string(), "linux".to_string())].into(),
        tolerations: vec![Toleration {
            key: "no-linux".into(),
            effect: "NoSchedule".into(),
        }],
        replicas: Replicas::Count { count: 2 },
    };
    let stored = set_session_spec(
        State(state.clone()),
        auth,
        Path(ws),
        Json(SetSessionSpecRequest {
            spec: Some(want.clone()),
        }),
    )
    .await
    .expect("set")
    .0
    .expect("a spec was set");
    assert_eq!(stored.runtime, "claude");
    assert_eq!(stored.replicas, Replicas::Count { count: 2 });
    assert_eq!(
        stored.node_selector.get("os").map(String::as_str),
        Some("linux")
    );
    assert_eq!(stored.tolerations, want.tolerations);

    // It persists: the read path sees what the write path stored.
    let reread = get_session_spec(State(state.clone()), auth, Path(ws))
        .await
        .expect("re-read")
        .0
        .expect("still set");
    assert_eq!(reread.replicas, Replicas::Count { count: 2 });

    bed.teardown().await;
}

#[tokio::test]
async fn clearing_returns_the_workspace_to_unmanaged() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("spec").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let auth = user_ctx(user, tenant);

    let _ = set_session_spec(
        State(state.clone()),
        auth,
        Path(ws),
        Json(SetSessionSpecRequest {
            spec: Some(spec("bash", Replicas::Single)),
        }),
    )
    .await
    .expect("set");

    // `spec: null` is a real instruction, not an omission — it un-enrols the
    // workspace, which zero replicas could not express.
    let cleared = set_session_spec(
        State(state.clone()),
        auth,
        Path(ws),
        Json(SetSessionSpecRequest { spec: None }),
    )
    .await
    .expect("clear")
    .0;
    assert!(cleared.is_none());

    let reread = get_session_spec(State(state.clone()), auth, Path(ws))
        .await
        .expect("re-read")
        .0;
    assert!(
        reread.is_none(),
        "cleared means unmanaged, not zero replicas"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn zero_replicas_is_managed_and_distinct_from_unmanaged() {
    // The reason the column is nullable at all: a reconciler must be able to
    // tell "I am responsible for this workspace and it wants none" from "not
    // mine". Both are legal; they are not the same.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("spec").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let auth = user_ctx(user, tenant);

    let stored = set_session_spec(
        State(state.clone()),
        auth,
        Path(ws),
        Json(SetSessionSpecRequest {
            spec: Some(spec("bash", Replicas::Count { count: 0 })),
        }),
    )
    .await
    .expect("zero replicas is a legal declaration")
    .0
    .expect("and it is SET, not absent");
    assert_eq!(stored.replicas, Replicas::Count { count: 0 });

    bed.teardown().await;
}

#[tokio::test]
async fn a_spec_that_cannot_mean_anything_is_refused() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("spec").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let auth = user_ctx(user, tenant);

    let mut empty_selector_key = spec("bash", Replicas::Single);
    empty_selector_key
        .node_selector
        .insert("  ".to_string(), "linux".to_string());

    let mut empty_selector_value = spec("bash", Replicas::Single);
    empty_selector_value
        .node_selector
        .insert("os".to_string(), String::new());

    let mut bad_toleration = spec("bash", Replicas::Single);
    bad_toleration.tolerations.push(Toleration {
        key: "no-linux".into(),
        effect: "Evict".into(),
    });

    for bad in [
        spec("", Replicas::Single),
        empty_selector_key,
        empty_selector_value,
        bad_toleration,
    ] {
        let r = set_session_spec(
            State(state.clone()),
            auth,
            Path(ws),
            Json(SetSessionSpecRequest { spec: Some(bad) }),
        )
        .await;
        assert!(r.is_err(), "a meaningless spec must be refused, not stored");
    }

    // …and nothing was stored on the way through.
    let after = get_session_spec(State(state.clone()), auth, Path(ws))
        .await
        .expect("read")
        .0;
    assert!(after.is_none(), "a refused write must not have stored");

    bed.teardown().await;
}

#[tokio::test]
async fn another_tenants_workspace_is_not_found() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let mine = bed.tenant("spec-mine").await;
    let theirs = bed.tenant("spec-theirs").await;
    let (user, _) = bed.user(mine, "member").await;
    let their_ws = bed.workspace(theirs).await;
    let state = bed.app_state().await;
    let auth = user_ctx(user, mine);

    assert!(
        get_session_spec(State(state.clone()), auth, Path(their_ws))
            .await
            .is_err(),
        "a workspace in another tenant is not readable"
    );
    assert!(
        set_session_spec(
            State(state.clone()),
            auth,
            Path(their_ws),
            Json(SetSessionSpecRequest {
                spec: Some(spec("bash", Replicas::Single))
            }),
        )
        .await
        .is_err(),
        "…nor writable"
    );

    bed.teardown().await;
}
