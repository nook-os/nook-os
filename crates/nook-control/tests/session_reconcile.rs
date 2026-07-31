//! MAIN-316: the persistence half of the reconciler.
//!
//! The planner's rules are unit-tested beside the planner. What needs a real
//! database is the part that makes those rules SAFE: that "managed" is a stored
//! fact rather than an inference (AC-3), and that two replicas racing to start
//! the same session cannot both win (AC-4). The second is enforced by an index,
//! and an index is not something you can assert against a fake.

use nook_control::repo::sessions::NewSession;
use nook_testkit::TestBed;
use nook_types::*;

async fn a_session(
    bed: &TestBed,
    tenant: TenantId,
    workspace: Option<WorkspaceId>,
    node: NodeId,
    name: &str,
) -> SessionId {
    bed.app_state()
        .await
        .sessions
        .create(NewSession {
            tenant,
            workspace_id: workspace,
            node_id: node,
            name: name.to_string(),
            runtime: "claude".to_string(),
            created_by: None,
            checkout_id: None,
        })
        .await
        .expect("create session")
        .id
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

    let hand_started = a_session(&bed, tenant, Some(ws), node, "mine").await;
    let mine = a_session(&bed, tenant, Some(ws), node, "managed").await;
    state
        .sessions
        .mark_managed(tenant, mine)
        .await
        .expect("mark managed");

    let seen = state.sessions.live_managed(tenant, ws).await.expect("read");
    assert_eq!(seen.len(), 1, "only the marked one");
    assert_eq!(seen[0].0, mine);
    assert_ne!(seen[0].0, hand_started);

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

    let s = a_session(&bed, tenant, Some(ws), node, "managed").await;
    state.sessions.mark_managed(tenant, s).await.unwrap();
    assert_eq!(
        state.sessions.live_managed(tenant, ws).await.unwrap().len(),
        1
    );

    assert_eq!(
        state.sessions.mark_ended(tenant, s).await.unwrap(),
        1,
        "ending a live session reports one row"
    );
    assert!(state
        .sessions
        .live_managed(tenant, ws)
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

    let first = a_session(&bed, tenant, Some(ws), node, "managed").await;
    state.sessions.mark_managed(tenant, first).await.unwrap();

    let second = a_session(&bed, tenant, Some(ws), node, "managed-again").await;
    let race = state.sessions.mark_managed(tenant, second).await;
    assert!(
        race.is_err(),
        "the second live managed session on the same (workspace, node) must be refused"
    );

    assert_eq!(
        state.sessions.live_managed(tenant, ws).await.unwrap().len(),
        1,
        "exactly one survives"
    );

    // And once the first ends, the slot is free again — otherwise a crashed
    // session could never be replaced.
    state.sessions.mark_ended(tenant, first).await.unwrap();
    state
        .sessions
        .mark_managed(tenant, second)
        .await
        .expect("the slot reopens when the incumbent dies");

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

    let managed = a_session(&bed, tenant, Some(ws), node, "managed").await;
    state.sessions.mark_managed(tenant, managed).await.unwrap();

    a_session(&bed, tenant, Some(ws), node, "person-1").await;
    a_session(&bed, tenant, Some(ws), node, "person-2").await;

    assert_eq!(
        state.sessions.live_managed(tenant, ws).await.unwrap().len(),
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
        let s = a_session(&bed, t, Some(w), n, "managed").await;
        state.sessions.mark_managed(t, s).await.unwrap();
    }

    assert_eq!(
        state
            .sessions
            .live_managed(mine, my_ws)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        state
            .sessions
            .live_managed(mine, their_ws)
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
