//! MAIN-436 AC-3: `has_permission` still REFUSES after the UNION rewrite.
//!
//! `has_permission` is a permission gate, so the two ways a rewrite can be
//! wrong are not symmetrical. Returning rows it should not is a security
//! regression. Returning an error reads as "denied" and looks perfectly fine —
//! which is how this chain has already been bitten once: on SQLite,
//! `somebody_elses_grant_is_not_mine` asserted a refusal and passed green
//! against a database that never answered the question, because a decode error
//! is also not an `Ok`.
//!
//! So the refusal here is asserted on its REASON, not on failure. A syntax
//! error, a decode error or a dead connection all fail `is_err()`; only a real
//! verdict names the permission.
//!
//! Everything below gates at `Scope::Deployment` deliberately. That path maps
//! to `(None, None)` and never reads `tenants.org_id`, so these run on the
//! SQLite arm with this card's fix alone — the uuid decode is the sibling
//! card's (MAIN-437) and this must not depend on it to prove its own claim.

use nook_control::auth::perm::{Permission, Scope};
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

/// A grant that must be honoured — the first UNION branch, reached through a
/// real `role_bindings` row.
#[tokio::test]
async fn a_deployment_grant_is_honoured() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("granted").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let state = bed.app_state().await;
    state
        .operator
        .grant_deployment_role(user.0, "operator", user.0)
        .await
        .expect("grant");

    ctx(user, tenant)
        .require(&state, Permission::NodeManage, Scope::Deployment)
        .await
        .expect("a deployment operator holds node.manage everywhere");

    bed.teardown().await;
}

/// …and one that must NOT be, **failing for the right reason**.
///
/// The assertion is on the message. `is_err()` alone would pass against a
/// database that never executed the predicate, which is exactly the false-green
/// this file exists to rule out.
#[tokio::test]
async fn an_ungranted_user_is_refused_and_the_refusal_is_a_verdict() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ungranted").await;
    // A MEMBER, so neither UNION branch matches: no binding, and the second
    // branch wants an owner/admin seat.
    let (user, _) = bed.user(tenant, "member").await;
    let state = bed.app_state().await;

    let refused = ctx(user, tenant)
        .require(&state, Permission::NodeManage, Scope::Deployment)
        .await
        .expect_err("a member holds nothing at deployment scope");

    let msg = refused.to_string();
    assert!(
        msg.contains("node.manage"),
        "the refusal must be a VERDICT naming the permission, not a query that \
         never ran — a syntax or decode error also fails is_err(): {msg}"
    );

    bed.teardown().await;
}

/// The predicate answers rather than erroring, on whichever engine this bed is.
///
/// `can()` is `require().is_ok()`, so it cannot distinguish "no" from "broken" —
/// which makes it the sharpest possible statement of the thing the rewrite had
/// to preserve: both answers are reachable, in one process, on one engine.
#[tokio::test]
async fn the_predicate_returns_both_answers_on_this_engine() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("both").await;
    let (granted, _) = bed.user(tenant, "owner").await;
    let (plain, _) = bed.user(tenant, "member").await;
    let state = bed.app_state().await;
    state
        .operator
        .grant_deployment_role(granted.0, "operator", granted.0)
        .await
        .expect("grant");

    assert!(
        ctx(granted, tenant)
            .can(&state, Permission::NodeManage, Scope::Deployment)
            .await,
        "the granted user is allowed"
    );
    assert!(
        !ctx(plain, tenant)
            .can(&state, Permission::NodeManage, Scope::Deployment)
            .await,
        "the ungranted user is not"
    );

    bed.teardown().await;
}
