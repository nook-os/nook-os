//! MAIN-318: who owns a session, and what "remove it" means.
//!
//! MAIN-316 made `managed` a stored fact so the reconciler could tell its own
//! sessions from everybody else's. This is the half that faces outward: the
//! session API says which kind a session is, and the two removals mean
//! different things — killing a managed session is not removing it, lowering
//! the workspace's replicas is.
//!
//! The planner assertions here are deliberately fed from a REAL repository read
//! rather than hand-built `Actual`s. `live_managed` is the seam where ownership
//! stops being a column and starts being a decision, and a test that skipped it
//! would prove the planner right about a list nothing produces.

use nook_control::repo::sessions::{NewSession, SessionFilter};
use nook_control::repo::workspaces::CheckoutUpsert;
use nook_control::services::session_reconcile::{
    plan, Action, Actual, CheckoutSlot, NodeFacts, PortSafety,
};
use nook_testkit::TestBed;
use nook_types::*;

/// A present checkout, landed the way a node's discovery scan lands one.
///
/// `associate_clone` is the other way in, and this file deliberately does not
/// use it: it emits `'{}'::jsonb`, which is why `session_reconcile`'s binary is
/// on the SQLite allowlist under MAIN-289. The upsert says the same thing in
/// dialect-neutral SQL, so these tests are covered on both engines instead of
/// adding a line to a list that is supposed to shrink.
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
        .upsert_checkout(CheckoutUpsert {
            tenant,
            node_id: node,
            workspace_id: workspace,
            path: path.to_string(),
            git_remote_url: Some("repo.git".into()),
            git_remote_normalized: Some("repo".into()),
            branch: None,
            git_status: serde_json::json!({}),
            kind: "clone".into(),
        })
        .await
        .expect("land checkout");
    state
        .workspaces
        .checkout_id_at_path(node, path)
        .await
        .expect("read checkout")
        .expect("checkout present")
}

async fn made(
    bed: &TestBed,
    tenant: TenantId,
    workspace: WorkspaceId,
    node: NodeId,
    checkout: NodeWorkspaceId,
    name: &str,
    managed: bool,
) -> SessionId {
    bed.app_state()
        .await
        .sessions
        .create(NewSession {
            tenant,
            workspace_id: Some(workspace),
            node_id: node,
            name: name.to_string(),
            runtime: "bash".to_string(),
            created_by: None,
            checkout_id: Some(checkout),
            managed,
            managed_purpose: ManagedPurpose::Access,
        })
        .await
        .expect("create session")
        .id
}

fn a_spec(replicas: Replicas) -> SessionSpec {
    SessionSpec {
        runtime: "bash".into(),
        node_selector: Default::default(),
        tolerations: vec![],
        replicas,
    }
}

fn facts(node: NodeId) -> NodeFacts {
    NodeFacts {
        id: node,
        online: true,
        labels: Default::default(),
        taints: vec![],
        runtimes: vec!["bash".into()],
    }
}

/// The reconciler's own view, read from the database exactly as a pass reads it.
async fn actual(bed: &TestBed, tenant: TenantId, workspace: WorkspaceId) -> Vec<Actual> {
    bed.app_state()
        .await
        .sessions
        .live_managed(tenant, workspace, None)
        .await
        .expect("live managed")
        .into_iter()
        .map(|(session_id, checkout_id, node_id)| Actual {
            session_id,
            checkout_id,
            node_id,
        })
        .collect()
}

#[tokio::test]
async fn the_session_api_says_which_kind_of_session_this_is() {
    // AC-1/AC-4. Both list shapes, because the UI decides which removal to
    // offer from the LIST — by the time it has fetched the one session it
    // already drew the wrong button.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("own").await;
    let (_, person) = bed.user(tenant, "owner").await;
    let node = bed.node(tenant, person).await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    let co_mine = a_checkout(&bed, tenant, node, ws, "/w/mine").await;
    let co_theirs = a_checkout(&bed, tenant, node, ws, "/w/theirs").await;
    let hand_started = made(&bed, tenant, ws, node, co_mine, "mine", false).await;
    let reconciled = made(&bed, tenant, ws, node, co_theirs, "replica", true).await;

    let one = state
        .sessions
        .get(tenant, reconciled)
        .await
        .expect("get")
        .expect("present");
    assert!(one.managed, "a reconciler session reports managed");

    let listed = state
        .sessions
        .list(tenant, SessionFilter::default())
        .await
        .expect("list");
    let flag = |id: SessionId| listed.iter().find(|s| s.id == id).expect("listed").managed;
    assert!(flag(reconciled));
    assert!(
        !flag(hand_started),
        "a hand-started session reports ad-hoc — the default the column ships with"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn killing_a_managed_session_is_not_removing_it() {
    // AC-2's first half. A kill ends the row (the node reports the exit); the
    // very next plan sees a checkout with no live managed session and starts
    // another one. This is the behaviour the card wants — and the reason the UI
    // needs `managed`: offering "kill" as removal would be a lie.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("own").await;
    let (_, person) = bed.user(tenant, "owner").await;
    let node = bed.node(tenant, person).await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    let co = a_checkout(&bed, tenant, node, ws, "/w/managed").await;
    let session = made(&bed, tenant, ws, node, co, "replica", true).await;
    let slots = vec![CheckoutSlot {
        checkout_id: co,
        node_id: node,
        path: "/w/managed".into(),
    }];
    let spec = a_spec(Replicas::All);

    let converged = plan(
        &spec,
        &[facts(node)],
        &slots,
        &actual(&bed, tenant, ws).await,
        PortSafety::Declared,
    );
    assert!(
        converged.actions.is_empty(),
        "nothing to do while it is running: {:?}",
        converged.actions
    );

    state
        .sessions
        .mark_ended(tenant, session)
        .await
        .expect("kill lands as an ended row");

    let after = plan(
        &spec,
        &[facts(node)],
        &slots,
        &actual(&bed, tenant, ws).await,
        PortSafety::Declared,
    );
    assert_eq!(
        after.actions,
        vec![Action::Start {
            checkout: co,
            node,
            path: "/w/managed".into(),
        }],
        "the declaration is unchanged, so the session comes back"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn lowering_the_replicas_is_what_removes_one() {
    // AC-2's second half, and the contrast that makes the first half safe:
    // editing the declaration is the removal that sticks. Zero replicas stops
    // what is running and — unlike a kill — starts nothing back.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("own").await;
    let (_, person) = bed.user(tenant, "owner").await;
    let node = bed.node(tenant, person).await;
    let ws = bed.workspace(tenant).await;

    let co = a_checkout(&bed, tenant, node, ws, "/w/managed").await;
    let session = made(&bed, tenant, ws, node, co, "replica", true).await;
    let slots = vec![CheckoutSlot {
        checkout_id: co,
        node_id: node,
        path: "/w/managed".into(),
    }];

    let p = plan(
        &a_spec(Replicas::Count { count: 0 }),
        &[facts(node)],
        &slots,
        &actual(&bed, tenant, ws).await,
        PortSafety::Declared,
    );
    assert_eq!(p.actions, vec![Action::Stop { session, node }]);

    bed.teardown().await;
}

#[tokio::test]
async fn an_ad_hoc_session_is_neither_a_replica_nor_a_victim() {
    // AC-3, end to end rather than at the query alone. A person's terminal, in
    // a managed workspace, on the reconciler's own checkout, with the spec's
    // runtime — the case that is indistinguishable by inspection. It must not
    // satisfy the slot (or the workspace silently never gets its session), and
    // it must not be stopped when the declaration drops to zero.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("own").await;
    let (_, person) = bed.user(tenant, "owner").await;
    let node = bed.node(tenant, person).await;
    let ws = bed.workspace(tenant).await;

    let co = a_checkout(&bed, tenant, node, ws, "/w/shared").await;
    made(&bed, tenant, ws, node, co, "someone's terminal", false).await;
    let slots = vec![CheckoutSlot {
        checkout_id: co,
        node_id: node,
        path: "/w/shared".into(),
    }];

    let wanted = plan(
        &a_spec(Replicas::All),
        &[facts(node)],
        &slots,
        &actual(&bed, tenant, ws).await,
        PortSafety::Declared,
    );
    assert_eq!(
        wanted.actions,
        vec![Action::Start {
            checkout: co,
            node,
            path: "/w/shared".into(),
        }],
        "an ad-hoc session does not fill a managed slot"
    );

    let dropped = plan(
        &a_spec(Replicas::Count { count: 0 }),
        &[facts(node)],
        &slots,
        &actual(&bed, tenant, ws).await,
        PortSafety::Declared,
    );
    assert!(
        dropped.actions.is_empty(),
        "scaling to zero touches nothing it does not own: {:?}",
        dropped.actions
    );

    bed.teardown().await;
}
