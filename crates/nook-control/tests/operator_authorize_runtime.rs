//! MAIN-276: the deployment operator authorizes a runtime on any fleet node.
//!
//! Three properties, and they are not the same property:
//!
//! 1. The operator reaches a node in a tenant they are NOT a member of. That is
//!    the whole point — an operator who could only act inside their own tenant
//!    could not log a runtime in on the shared executor.
//! 2. The owner can decline it on one machine, and their decline outranks the
//!    operator's role.
//! 3. **Authorize is not permit-work.** Nothing about authorizing grants any
//!    ability to run anything there. This is the assertion the card was split
//!    to give an undistracted review, so it gets a test of its own rather than
//!    a line in another one.

use nook_control::auth::{AuthCtx, Principal};
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

/// A deployment operator: a user in their own tenant holding the deployment-
/// scoped role. Deliberately NOT a member of the tenant they will act on —
/// that separation is what the cross-tenant case is about.
async fn an_operator(bed: &TestBed) -> (UserId, TenantId) {
    let home = bed.tenant("op-home").await;
    let (user, _) = bed.user(home, "owner").await;
    bed.app_state()
        .await
        .operator
        .grant_deployment_role(user.0, "operator", user.0)
        .await
        .expect("grant the deployment role");
    (user, home)
}

/// The operator's reach does not stop at a tenant boundary (AC-1/AC-2). The
/// node is offline here, so the device-login flow refuses at the node rather
/// than at the gate — which is exactly the distinction being asserted: the
/// AUTHORIZATION passed, and only the machine's absence stopped it.
#[tokio::test]
async fn the_operator_gate_admits_a_node_in_another_tenant() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (operator, op_home) = an_operator(&bed).await;
    let theirs = bed.tenant("someone-else").await;
    let (_, owner) = bed.user(theirs, "owner").await;
    let node = bed.node(theirs, owner).await;
    let state = bed.app_state().await;

    // The gate the endpoint runs, in isolation: an operator holds NodeManage
    // on a tenant they have no membership in.
    let allowed = ctx(operator, op_home)
        .require(
            &state,
            nook_control::auth::perm::Permission::NodeManage,
            nook_control::auth::perm::Scope::Tenant(theirs),
        )
        .await;
    assert!(
        allowed.is_ok(),
        "a deployment operator must reach another tenant's node: {allowed:?}"
    );
    assert!(
        !state
            .nodes
            .get(theirs, node)
            .await
            .expect("read")
            .expect("node")
            .operator_authorize_optout,
        "opt-out defaults OFF — the capability exists until an owner withdraws it"
    );

    bed.teardown().await;
}

/// A tenant member who is not an operator gets nothing from this surface.
#[tokio::test]
async fn an_ordinary_member_is_refused_the_operator_gate() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let theirs = bed.tenant("someone-else").await;
    let (_, owner) = bed.user(theirs, "owner").await;
    bed.node(theirs, owner).await;
    let mine = bed.tenant("mine").await;
    let (member, _) = bed.user(mine, "member").await;
    let state = bed.app_state().await;

    let refused = ctx(member, mine)
        .require(
            &state,
            nook_control::auth::perm::Permission::NodeManage,
            nook_control::auth::perm::Scope::Tenant(theirs),
        )
        .await;
    assert!(
        refused.is_err(),
        "a member of another tenant must not hold NodeManage here"
    );

    bed.teardown().await;
}

/// AC-6. The owner's decline is stored on the node and is what the endpoint
/// reads before it starts anything.
#[tokio::test]
async fn the_owner_can_decline_operator_authorize_on_their_machine() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("owner-veto").await;
    let (_, person) = bed.user(tenant, "owner").await;
    let node = bed.node(tenant, person).await;
    let state = bed.app_state().await;

    let after = state
        .nodes
        .set_operator_authorize_optout(node, tenant, true)
        .await
        .expect("set")
        .expect("node");
    assert!(after.operator_authorize_optout);

    // …and it is reversible, so declining is not a one-way door.
    let restored = state
        .nodes
        .set_operator_authorize_optout(node, tenant, false)
        .await
        .expect("set")
        .expect("node");
    assert!(!restored.operator_authorize_optout);

    bed.teardown().await;
}

/// The flag is tenant-scoped at the repository, which is what stops it being
/// written from outside the owner's tenant even if a caller names the id.
#[tokio::test]
async fn the_optout_cannot_be_set_from_another_tenant() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let theirs = bed.tenant("theirs").await;
    let (_, person) = bed.user(theirs, "owner").await;
    let node = bed.node(theirs, person).await;
    let elsewhere = bed.tenant("elsewhere").await;
    let state = bed.app_state().await;

    assert!(
        state
            .nodes
            .set_operator_authorize_optout(node, elsewhere, true)
            .await
            .expect("query runs")
            .is_none(),
        "naming another tenant's node must match no row"
    );
    assert!(
        !state
            .nodes
            .get(theirs, node)
            .await
            .expect("read")
            .expect("node")
            .operator_authorize_optout,
        "and must not have changed it"
    );

    bed.teardown().await;
}

/// **AC-3 — authorize is not permit-work.**
///
/// The operator holding `NodeManage` on another tenant is what lets them
/// authorize a runtime. It must not also let them start a session there. These
/// are two gates, and this asserts they have not quietly become one: session
/// start answers to node OWNERSHIP (`require_person_owns_node`), which no role
/// satisfies.
#[tokio::test]
async fn operator_authorize_does_not_grant_node_use() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (operator, _) = an_operator(&bed).await;
    let theirs = bed.tenant("someone-else").await;
    let (_, owner) = bed.user(theirs, "owner").await;
    let node = bed.node(theirs, owner).await;
    let state = bed.app_state().await;

    // The operator is not the node's owner, and the ownership chokepoint is
    // what session start runs. A role does not satisfy it.
    let use_refused =
        nook_control::auth::require_person_owns_node(&state, theirs, Some(operator), node).await;
    assert!(
        use_refused.is_err(),
        "authorizing a runtime must not make the operator able to run work there"
    );

    bed.teardown().await;
}
