//! MAIN-447 AC-4: what the review loop is actually doing, for the surface that
//! sets its ceiling.
//!
//! The planner's own rules are unit-tested beside the planner. What needs a
//! database is the WIRING, and specifically the one mistake this endpoint
//! exists to avoid: `/reconcile-status` reports the workspace's `SessionSpec`
//! and filters this purpose out, so a UI that read it would show a repo's
//! reviewers as zero forever. Two declarations converge per workspace and
//! neither may describe the other.
//!
//! The gates are reported as two booleans rather than one `enabled` because the
//! panel's job is to name the switch that is off; a test that only checked
//! "some gate" would pass while the UI sent somebody to the wrong page.

use axum::extract::{Path, State};
use nook_control::auth::{AuthCtx, Principal};
use nook_control::repo::admin::SettingWrite;
use nook_control::repo::sessions::NewSession;
use nook_control::routes::workspaces::{review_loop_status, set_review_loop};
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

fn req(v: serde_json::Value) -> axum::Json<SetReviewLoopRequest> {
    axum::Json(SetReviewLoopRequest { max_replicas: v })
}

#[tokio::test]
async fn the_ceiling_is_what_the_status_reports_as_desired() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("rlstatus").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let auth = user_ctx(user, tenant);

    // Unset resolves to the build's default of one — the status has to RESOLVE
    // it, unlike the declaration endpoint, which reports the raw column so the
    // CLI can still say "unset (default 1)".
    let unset = review_loop_status(State(state.clone()), auth, Path(ws))
        .await
        .expect("status")
        .0;
    assert_eq!(unset.desired, 1, "unset means the default ceiling of one");

    let _ = set_review_loop(State(state.clone()), auth, Path(ws), req(0.into()))
        .await
        .expect("set 0");
    let off = review_loop_status(State(state.clone()), auth, Path(ws))
        .await
        .expect("status")
        .0;
    assert_eq!(off.desired, 0, "an explicit 0 wants no reviewer at all");

    let _ = set_review_loop(State(state.clone()), auth, Path(ws), req(3.into()))
        .await
        .expect("set 3");
    let three = review_loop_status(State(state.clone()), auth, Path(ws))
        .await
        .expect("status")
        .0;
    assert_eq!(three.desired, 3);

    bed.teardown().await;
}

#[tokio::test]
async fn both_tenant_gates_are_reported_separately() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("rlgates").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let auth = user_ctx(user, tenant);

    // Both default OFF (MAIN-239), which is exactly the state the panel must be
    // able to explain rather than silently show an idle loop.
    let fresh = review_loop_status(State(state.clone()), auth, Path(ws))
        .await
        .expect("status")
        .0;
    assert!(!fresh.reconcile_enabled);
    assert!(!fresh.loops_enabled);

    state
        .settings
        .put(SettingWrite {
            tenant,
            scope: "tenant".into(),
            user: None,
            key: nook_control::services::session_reconcile::KEY.into(),
            value: serde_json::json!(true),
        })
        .await
        .expect("enable reconcile");

    // One on, one off — the case a single `enabled` bool cannot express, and
    // the reason the DTO carries two.
    let half = review_loop_status(State(state.clone()), auth, Path(ws))
        .await
        .expect("status")
        .0;
    assert!(half.reconcile_enabled);
    assert!(!half.loops_enabled, "loops is its own switch");

    bed.teardown().await;
}

#[tokio::test]
async fn running_counts_review_loops_and_not_a_persons_terminal() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("rlcount").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let auth = user_ctx(user, tenant);

    // TWO clones, and a deliberately LOPSIDED population: a terminal on each,
    // one reviewer in total. Equal counts would let a filter on the wrong
    // purpose return the right number by luck, which is the shape of test that
    // passes while the endpoint reports the other declaration entirely.
    let mut checkouts = Vec::new();
    for path in ["/w/repo-a", "/w/repo-b"] {
        let node = bed.node(tenant, person).await;
        state
            .workspaces
            .associate_clone(tenant, node, ws, path, "repo.git", "repo")
            .await
            .expect("clone");
        let checkout = state
            .workspaces
            .checkout_id_at_path(node, path)
            .await
            .expect("read checkout")
            .expect("checkout present");
        checkouts.push((node, checkout));
    }

    let session = |purpose: ManagedPurpose, i: usize| {
        let (node, checkout) = checkouts[i];
        let state = state.clone();
        async move {
            state
                .sessions
                .create(NewSession {
                    tenant,
                    workspace_id: Some(ws),
                    node_id: node,
                    name: format!("{purpose}-{i}"),
                    runtime: "claude".to_string(),
                    created_by: None,
                    checkout_id: Some(checkout),
                    managed: true,
                    managed_purpose: purpose,
                })
                .await
                .expect("create session");
        }
    };
    session(ManagedPurpose::Access, 0).await;
    session(ManagedPurpose::Access, 1).await;
    session(ManagedPurpose::ReviewLoop, 0).await;

    let got = review_loop_status(State(state.clone()), auth, Path(ws))
        .await
        .expect("status")
        .0;
    // Three managed sessions live on this workspace; exactly one is a reviewer.
    assert_eq!(got.running, 1, "a person's terminal is not a review loop");

    bed.teardown().await;
}
