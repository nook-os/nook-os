//! MAIN-326: the persistence half of the control plane's own review loop.
//!
//! The planner's rules are unit-tested beside the planner. What needs a real
//! database is the thing that makes two declarations SAFE to run over one
//! workspace: `sessions_one_managed_per_checkout_purpose`. A fake cannot prove
//! an index, and the whole design rests on this one being right — if the purpose
//! were not part of the key, the access session and the review loop would each
//! read as the other's duplicate and stop each other forever.

use nook_control::repo::sessions::NewSession;
use nook_testkit::TestBed;
use nook_types::*;

/// A present clone of `workspace` on `node`, returning its row id — the key the
/// managed-session index arbitrates on.
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

/// A managed session for one declaration, in one checkout. Returns the repo's
/// answer rather than unwrapping, because half these cases are about the INSERT
/// being refused.
async fn declared(
    bed: &TestBed,
    tenant: TenantId,
    workspace: WorkspaceId,
    node: NodeId,
    checkout: NodeWorkspaceId,
    purpose: ManagedPurpose,
) -> nook_control::error::ApiResult<SessionId> {
    Ok(bed
        .app_state()
        .await
        .sessions
        .create(NewSession {
            tenant,
            workspace_id: Some(workspace),
            node_id: node,
            name: format!("{purpose}"),
            runtime: "claude".to_string(),
            created_by: None,
            checkout_id: Some(checkout),
            managed: true,
            managed_purpose: purpose,
        })
        .await?
        .id)
}

/// The load-bearing case. One clone on the loop node holds BOTH declarations:
/// the person's terminal and the always-on review loop. Before the purpose was
/// part of the index this insert was refused, and whichever declaration lost
/// simply never converged.
#[tokio::test]
async fn one_checkout_holds_both_declarations() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("recon").await;
    let (_, person) = bed.user(tenant, "owner").await;
    let node = bed.node(tenant, person).await;
    let ws = bed.workspace(tenant).await;
    let co = a_checkout(&bed, tenant, node, ws, "/w/clone").await;

    let access = declared(&bed, tenant, ws, node, co, ManagedPurpose::Access)
        .await
        .expect("the access session");
    let review = declared(&bed, tenant, ws, node, co, ManagedPurpose::ReviewLoop)
        .await
        .expect("the review loop is not a duplicate of the access session");
    assert_ne!(access, review);

    bed.teardown().await;
}

/// …and the guarantee it must not have cost. Uniqueness still holds WITHIN a
/// purpose, which is what arbitrates the multi-replica race: two replicas both
/// seeing a missing review loop must not both start one.
#[tokio::test]
async fn two_replicas_cannot_both_start_the_review_loop() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("recon").await;
    let (_, person) = bed.user(tenant, "owner").await;
    let node = bed.node(tenant, person).await;
    let ws = bed.workspace(tenant).await;
    let co = a_checkout(&bed, tenant, node, ws, "/w/clone").await;

    declared(&bed, tenant, ws, node, co, ManagedPurpose::ReviewLoop)
        .await
        .expect("first wins");
    let second = declared(&bed, tenant, ws, node, co, ManagedPurpose::ReviewLoop).await;
    assert!(
        second.is_err(),
        "the index must refuse a second live review loop on one checkout"
    );

    bed.teardown().await;
}

/// The planner's `actual` set is also its STOP list, so a declaration that
/// could see the other's sessions would stop them as strays. `None` is the
/// unfiltered read workspace deletion needs — a review loop is equally
/// reconciler-owned, and counting it as a session "somebody started" would make
/// the workspace undeletable.
#[tokio::test]
async fn live_managed_narrows_by_purpose_and_none_takes_both() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("recon").await;
    let (_, person) = bed.user(tenant, "owner").await;
    let node = bed.node(tenant, person).await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let co = a_checkout(&bed, tenant, node, ws, "/w/clone").await;

    let access = declared(&bed, tenant, ws, node, co, ManagedPurpose::Access)
        .await
        .unwrap();
    let review = declared(&bed, tenant, ws, node, co, ManagedPurpose::ReviewLoop)
        .await
        .unwrap();

    let seen = |p| {
        let state = state.clone();
        async move {
            state
                .sessions
                .live_managed(tenant, ws, p)
                .await
                .expect("read")
                .into_iter()
                .map(|(id, _, _)| id)
                .collect::<Vec<_>>()
        }
    };

    assert_eq!(seen(Some(ManagedPurpose::Access)).await, vec![access]);
    assert_eq!(seen(Some(ManagedPurpose::ReviewLoop)).await, vec![review]);
    let both = seen(None).await;
    assert_eq!(both.len(), 2, "unfiltered takes every declaration");
    assert!(both.contains(&access) && both.contains(&review));

    bed.teardown().await;
}

/// Self-healing, per declaration. A crashed review loop is a gap the next pass
/// fills; it must not be blocked by the index it just vacated, and it must not
/// take the access session down with it.
#[tokio::test]
async fn an_ended_review_loop_is_replaceable_and_leaves_the_terminal_alone() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("recon").await;
    let (_, person) = bed.user(tenant, "owner").await;
    let node = bed.node(tenant, person).await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let co = a_checkout(&bed, tenant, node, ws, "/w/clone").await;

    let access = declared(&bed, tenant, ws, node, co, ManagedPurpose::Access)
        .await
        .unwrap();
    let dead = declared(&bed, tenant, ws, node, co, ManagedPurpose::ReviewLoop)
        .await
        .unwrap();
    assert_eq!(state.sessions.mark_ended(tenant, dead).await.unwrap(), 1);

    declared(&bed, tenant, ws, node, co, ManagedPurpose::ReviewLoop)
        .await
        .expect("the gap is fillable");
    assert_eq!(
        state
            .sessions
            .live_managed(tenant, ws, Some(ManagedPurpose::Access))
            .await
            .unwrap()
            .into_iter()
            .map(|(id, _, _)| id)
            .collect::<Vec<_>>(),
        vec![access],
        "the person's terminal was never involved"
    );

    bed.teardown().await;
}
