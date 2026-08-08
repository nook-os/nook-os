//! MAIN-316: the persistence half of the reconciler.
//!
//! The planner's rules are unit-tested beside the planner. What needs a real
//! database is the part that makes those rules SAFE: that "managed" is a stored
//! fact rather than an inference (AC-3), and that two replicas racing to start
//! the same session cannot both win (AC-4). The second is enforced by an index,
//! and an index is not something you can assert against a fake.

use nook_control::auth::{AuthCtx, Principal};
use nook_control::repo::sessions::NewSession;
use nook_db::Db;
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

fn ctx(user: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: false,
    }
}

/// A present checkout of `workspace` on `node`, returning its row id — the key
/// the managed-session index now arbitrates on. Idempotent per `path`, so the
/// same call twice yields the SAME checkout (which is how two replicas race for
/// one slot).
async fn a_checkout(
    bed: &TestBed,
    tenant: TenantId,
    node: NodeId,
    workspace: WorkspaceId,
    path: &str,
) -> NodeWorkspaceId {
    let state = bed.app_state().await;
    state
        .workspaces
        .associate_clone(tenant, node, workspace, path, "repo.git", "repo")
        .await
        .expect("associate checkout");
    state
        .workspaces
        .checkout_id_at_path(node, path)
        .await
        .expect("read checkout")
        .expect("checkout present")
}

/// Create a session, ad-hoc or reconciler-owned. `managed` rides the INSERT,
/// which is the point: it is what the unique index arbitrates on, before
/// anything reaches a node. A managed session must carry a `checkout` — the
/// index keys on it — so the helper takes one.
async fn a_session(
    bed: &TestBed,
    tenant: TenantId,
    workspace: Option<WorkspaceId>,
    node: NodeId,
    checkout: Option<NodeWorkspaceId>,
    name: &str,
    managed: bool,
) -> nook_control::error::ApiResult<SessionId> {
    Ok(bed
        .app_state()
        .await
        .sessions
        .create(NewSession {
            tenant,
            workspace_id: workspace,
            node_id: node,
            name: name.to_string(),
            runtime: "claude".to_string(),
            created_by: None,
            checkout_id: checkout,
            managed,
            managed_purpose: ManagedPurpose::Access,
            managed_shard: 0,
            managed_shards: 1,
        })
        .await?
        .id)
}

/// The common case: it must succeed.
async fn made(
    bed: &TestBed,
    tenant: TenantId,
    workspace: Option<WorkspaceId>,
    node: NodeId,
    checkout: Option<NodeWorkspaceId>,
    name: &str,
    managed: bool,
) -> SessionId {
    a_session(bed, tenant, workspace, node, checkout, name, managed)
        .await
        .expect("create session")
}

#[tokio::test]
async fn only_sessions_the_reconciler_marked_are_visible_to_it() {
    // AC-3, and the reason `managed` is a column: an ad-hoc session in a
    // managed workspace, on an eligible node, with the SAME runtime, is
    // indistinguishable from a replica by inspection. It must not be counted —
    // because what follows counting is being killed on scale-down.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("recon").await;
    let (_, person) = bed.user(tenant, "owner").await;
    let node = bed.node(tenant, person).await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    let co = a_checkout(&bed, tenant, node, ws, "/w/mine").await;
    let hand_started = made(&bed, tenant, Some(ws), node, Some(co), "mine", false).await;
    let mine = made(&bed, tenant, Some(ws), node, Some(co), "managed", true).await;

    let seen = state
        .sessions
        .live_managed(tenant, ws, None)
        .await
        .expect("read");
    assert_eq!(seen.len(), 1, "only the marked one");
    assert_eq!(seen[0].id, mine);
    assert_ne!(seen[0].id, hand_started);

    bed.teardown().await;
}

#[tokio::test]
async fn an_ended_managed_session_is_a_gap_not_a_replica() {
    // "Restart a crashed managed session" (AC-2) is entirely this: the dead one
    // stops counting, so the next plan sees a hole and fills it.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("recon").await;
    let (_, person) = bed.user(tenant, "owner").await;
    let node = bed.node(tenant, person).await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    let co = a_checkout(&bed, tenant, node, ws, "/w/managed").await;
    let s = made(&bed, tenant, Some(ws), node, Some(co), "managed", true).await;
    assert_eq!(
        state
            .sessions
            .live_managed(tenant, ws, None)
            .await
            .unwrap()
            .len(),
        1
    );

    assert_eq!(
        state.sessions.mark_ended(tenant, s).await.unwrap(),
        1,
        "ending a live session reports one row"
    );
    assert!(state
        .sessions
        .live_managed(tenant, ws, None)
        .await
        .unwrap()
        .is_empty());

    // Idempotent: a second replica running the same plan changes nothing.
    assert_eq!(
        state.sessions.mark_ended(tenant, s).await.unwrap(),
        0,
        "ending an already-ended session is a no-op, not a second end"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn two_replicas_cannot_both_start_the_same_managed_session() {
    // AC-4's "one action wins", and AC-4's "no doubling per node" — the same
    // index does both. This is the test that would have needed a distributed
    // lease to pass any other way.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("recon").await;
    let (_, person) = bed.user(tenant, "owner").await;
    let node = bed.node(tenant, person).await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    // Both replicas race for the SAME checkout — that is the slot the index
    // arbitrates now.
    let co = a_checkout(&bed, tenant, node, ws, "/w/managed").await;
    let first = made(&bed, tenant, Some(ws), node, Some(co), "managed", true).await;

    // The CREATE must fail, not some later marking step. That ordering is the
    // whole fix: `create_session_at` inserts and then tells the node to start,
    // so a race arbitrated after the insert would already have launched a real
    // tmux session that nothing could ever reconcile.
    let race = a_session(
        &bed,
        tenant,
        Some(ws),
        node,
        Some(co),
        "managed-again",
        true,
    )
    .await;
    assert!(
        race.is_err(),
        "a second live managed session on the same checkout must be refused at INSERT"
    );

    assert_eq!(
        state
            .sessions
            .live_managed(tenant, ws, None)
            .await
            .unwrap()
            .len(),
        1,
        "exactly one survives"
    );

    // And once the first ends, the slot is free again — otherwise a crashed
    // session could never be replaced.
    state.sessions.mark_ended(tenant, first).await.unwrap();
    made(&bed, tenant, Some(ws), node, Some(co), "replacement", true).await;

    bed.teardown().await;
}

#[tokio::test]
async fn an_ad_hoc_session_is_not_blocked_by_the_managed_index() {
    // The index is partial on `managed`. If it were not, a person could not
    // open a terminal on a node the reconciler is already using — which would
    // make the whole feature hostile.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("recon").await;
    let (_, person) = bed.user(tenant, "owner").await;
    let node = bed.node(tenant, person).await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    let co = a_checkout(&bed, tenant, node, ws, "/w/managed").await;
    made(&bed, tenant, Some(ws), node, Some(co), "managed", true).await;
    made(&bed, tenant, Some(ws), node, None, "person-1", false).await;
    made(&bed, tenant, Some(ws), node, None, "person-2", false).await;

    assert_eq!(
        state
            .sessions
            .live_managed(tenant, ws, None)
            .await
            .unwrap()
            .len(),
        1,
        "the ad-hoc ones neither block nor appear"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn live_managed_is_scoped_to_its_tenant_and_workspace() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let mine = bed.tenant("recon-mine").await;
    let theirs = bed.tenant("recon-theirs").await;
    let (_, p1) = bed.user(mine, "owner").await;
    let (_, p2) = bed.user(theirs, "owner").await;
    let my_node = bed.node(mine, p1).await;
    let their_node = bed.node(theirs, p2).await;
    let my_ws = bed.workspace(mine).await;
    let my_other_ws = bed.workspace(mine).await;
    let their_ws = bed.workspace(theirs).await;
    let state = bed.app_state().await;

    for (t, w, n) in [
        (mine, my_ws, my_node),
        (mine, my_other_ws, my_node),
        (theirs, their_ws, their_node),
    ] {
        // A distinct checkout per workspace (path unique per node).
        let co = a_checkout(&bed, t, n, w, &format!("/w/{}", w.0.simple())).await;
        made(&bed, t, Some(w), n, Some(co), "managed", true).await;
    }

    assert_eq!(
        state
            .sessions
            .live_managed(mine, my_ws, None)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        state
            .sessions
            .live_managed(mine, their_ws, None)
            .await
            .unwrap()
            .is_empty(),
        "another tenant's workspace is not readable even by id"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn only_workspaces_that_declare_a_spec_are_listed() {
    // The reconciler's outer loop. An unmanaged workspace is not its business,
    // and listing one would make every workspace a candidate for reconciling.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("recon").await;
    let declared = bed.workspace(tenant).await;
    let _unmanaged = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    state
        .workspaces
        .set_session_spec(
            tenant,
            declared,
            Some(serde_json::json!({
                "runtime": "claude",
                "replicas": {"kind": "single"}
            })),
        )
        .await
        .expect("declare");

    let listed = state.workspaces.all_session_specs().await.expect("list");
    let ours: Vec<_> = listed.iter().filter(|(t, _, _)| *t == tenant).collect();
    assert_eq!(ours.len(), 1, "only the declared one");
    assert_eq!(ours[0].1, declared);

    // Clearing the spec removes it from the loop's view entirely.
    state
        .workspaces
        .set_session_spec(tenant, declared, None)
        .await
        .unwrap();
    let listed = state.workspaces.all_session_specs().await.unwrap();
    assert!(listed.iter().all(|(t, _, _)| *t != tenant));

    bed.teardown().await;
}

// ── MAIN-319: the status read ───────────────────────────────────────────────

#[tokio::test]
async fn an_unmanaged_workspace_reports_unmanaged_rather_than_zero_of_zero() {
    // "No policy" and "a policy wanting none" are different answers, and a UI
    // that renders both as 0/0 tells you nothing about which you are looking at.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("recon").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    let got = nook_control::routes::workspaces::reconcile_status(
        axum::extract::State(state.clone()),
        ctx(user, tenant),
        axum::extract::Path(ws),
    )
    .await
    .expect("status")
    .0;
    assert!(!got.managed);
    assert_eq!(got.desired, 0);

    bed.teardown().await;
}

#[tokio::test]
async fn reconcile_on_makes_an_unspecced_workspace_managed_by_auto_derive() {
    // The auto-derive: flipping the tenant switch on makes EVERY workspace
    // managed toward the default spec, with no per-workspace `session_spec`. The
    // status endpoint must derive it the same way the loop does — reporting
    // "unmanaged" here while the loop is about to place a default session is the
    // exact drift the endpoint exists to prevent.
    use nook_control::repo::admin::SettingWrite;

    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("recon").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    // No spec on the workspace — only the tenant switch.
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

    let got = nook_control::routes::workspaces::reconcile_status(
        axum::extract::State(state.clone()),
        ctx(user, tenant),
        axum::extract::Path(ws),
    )
    .await
    .expect("status")
    .0;

    assert!(got.enabled, "the tenant switch is on");
    assert!(
        got.managed,
        "an on tenant auto-derives a default spec, so the workspace is managed with no session_spec of its own"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn a_declared_workspace_reports_desired_and_the_shortfall() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("recon").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    state
        .workspaces
        .set_session_spec(
            tenant,
            ws,
            Some(serde_json::json!({
                "runtime": "claude",
                "replicas": {"kind": "count", "count": 3}
            })),
        )
        .await
        .expect("declare");

    let got = nook_control::routes::workspaces::reconcile_status(
        axum::extract::State(state.clone()),
        ctx(user, tenant),
        axum::extract::Path(ws),
    )
    .await
    .expect("status")
    .0;

    assert!(got.managed);
    assert_eq!(got.desired, 3);
    assert_eq!(got.running, 0);
    // No online node in the bed, so all three are owed. The number matters less
    // than that it comes from the reconciler's own planner rather than a second
    // opinion about it.
    assert_eq!(got.shortfall, 3);
    // Reconciling is off by default, and the UI has to be able to say so —
    // otherwise a declared-but-never-converging workspace looks broken.
    assert!(!got.enabled);

    bed.teardown().await;
}

#[tokio::test]
async fn another_tenants_workspace_has_no_status() {
    // AC-4: the read is tenant-scoped like every other workspace read, so this
    // cannot be used to inspect somebody else's fleet.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let mine = bed.tenant("recon-mine").await;
    let theirs = bed.tenant("recon-theirs").await;
    let (user, _) = bed.user(mine, "owner").await;
    let their_ws = bed.workspace(theirs).await;
    let state = bed.app_state().await;

    assert!(nook_control::routes::workspaces::reconcile_status(
        axum::extract::State(state.clone()),
        ctx(user, mine),
        axum::extract::Path(their_ws),
    )
    .await
    .is_err());

    bed.teardown().await;
}

/// A terminal you opened is YOURS. The reconciler has no opinion about how many
/// there should be, and one pass must not create any.
///
/// This is the guard for the change that removed access reconciliation. It was
/// `ManagedPurpose::Access` over `Slots::EveryCheckout`, run for every workspace
/// in every reconcile-on tenant, so every checkout was owed a session forever —
/// which meant Stop undid itself within a poll interval, and production sat at
/// `desired=9 actual=12` retrying stops it could not perform every ten seconds.
///
/// Asserting an ABSENCE, deliberately. The planner tests all pass whether or not
/// anything calls the planner with `Access`, because they exercise
/// `plan_workspace` directly — so nothing below this level can notice the
/// regression. If a later change reinstates access reconciliation, this is the
/// test that has to be deleted to do it.
#[tokio::test]
async fn a_reconcile_pass_creates_no_access_sessions() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;

    let tenant = bed.tenant("noaccess").await;
    let (_user, person) = bed.user(tenant, "owner").await;
    let node = bed.node(tenant, person).await;
    let workspace = bed.workspace(tenant).await;
    // A present clone, which is exactly the slot the old behaviour filled.
    a_checkout(&bed, tenant, node, workspace, "/w/repo").await;

    // Reconcile ON for this tenant — without it the pass returns early and the
    // assertion below would hold for the wrong reason.
    state
        .settings
        .put(nook_control::repo::admin::SettingWrite {
            tenant,
            scope: "tenant".to_string(),
            user: None,
            key: nook_control::services::session_reconcile::KEY.to_string(),
            value: serde_json::Value::Bool(true),
        })
        .await
        .expect("switch on");

    // The node must be ONLINE, or this test proves nothing: placement skips an
    // offline node, so the pass would create no session whether or not access
    // reconciliation exists. Verified by reinstating the old call and watching
    // this test stay green — a false guard caught before it shipped.
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    state.registry.register_node(
        node,
        nook_control::ws::registry::NodeHandle {
            tenant_id: tenant,
            tx,
        },
    );

    let throttle = nook_control::services::session_reconcile::CloneThrottle::default();
    nook_control::services::session_reconcile::pass(&state, &throttle)
        .await
        .expect("one pass");

    let sessions: i64 = bed
        .db()
        .query_scalar(
            "SELECT count(*) FROM sessions WHERE tenant_id = $1 AND managed_purpose = 'access'",
            nook_db::params![tenant],
        )
        .await
        .expect("count");
    assert_eq!(
        sessions, 0,
        "the reconciler created an access session — a human's terminal is not \
         drift to be corrected"
    );

    bed.teardown().await;
}
