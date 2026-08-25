//! A terminal belongs to the person who owns the machine.
//!
//! Found in prod 2026-08-03: a second OWNER of a tenant attached to sessions
//! running on someone else's nodes. `require_session_access` gated on tenant
//! membership alone, so every member could read, type into and kill any terminal
//! in the tenant. The promise in `session_guard`'s docs — that operator and
//! administrative roles never reach terminal content — held against operators
//! and strangers, and not against the person at the next desk.
//!
//! Owners keep tenant-wide session METADATA (capacity and audit, MAIN-133).
//! These pin the other half: CONTENT is the node owner's, and no role is a way
//! in.
//!
//! Set `DATABASE_URL`.

use nook_control::auth::{AuthCtx, Principal};
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

fn user_ctx(tenant: TenantId, user: UserId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::new_v4()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: true,
    }
}

/// Make `user` an OWNER of `tenant` — the strongest role there is, and the one
/// the reporter actually held.
async fn make_owner(bed: &TestBed, tenant: TenantId, user: UserId) {
    bed.db()
        .exec(
            "INSERT INTO tenant_members (id, tenant_id, principal_type, principal_id, role)
             VALUES ($1, $2, 'user', $3, 'owner')",
            params![Uuid::now_v7(), tenant, user.0],
        )
        .await
        .expect("membership");
}

/// The regression. A tenant OWNER who does not own the machine is refused.
#[tokio::test]
async fn a_tenant_owner_cannot_reach_a_terminal_on_someone_elses_node() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;

    let tenant = bed.tenant("t").await;
    let (mine, my_person) = bed.user(tenant, "owner").await;
    let (theirs, _their_person) = bed.user(tenant, "owner").await;
    make_owner(&bed, tenant, theirs).await;

    // My machine.
    let node = bed.node(tenant, my_person).await;

    // I reach my own terminal.
    user_ctx(tenant, mine)
        .require_session_access(&state, tenant, node)
        .await
        .expect("the machine's owner reaches their own terminal");

    // My co-owner does not.
    let err = user_ctx(tenant, theirs)
        .require_session_access(&state, tenant, node)
        .await
        .expect_err("a tenant owner must not reach a terminal on a node they do not own");
    assert!(
        matches!(err, nook_control::error::ApiError::ForbiddenMsg(_)),
        "expected a forbidden refusal, got {err:?}"
    );

    bed.teardown().await;
}

/// Sharing a node lets the team RUN work on it. It does not hand them the
/// screens of work already running — the one place this rule deliberately
/// differs from `require_person_may_use_node`.
#[tokio::test]
async fn sharing_a_node_does_not_share_its_terminals() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;

    let tenant = bed.tenant("t").await;
    let (mine, my_person) = bed.user(tenant, "owner").await;
    let (theirs, _p) = bed.user(tenant, "owner").await;
    let node = bed.node(tenant, my_person).await;
    bed.db()
        .exec(
            "UPDATE nodes SET shared = true WHERE id = $1",
            params![node],
        )
        .await
        .expect("share the node");

    user_ctx(tenant, mine)
        .require_session_access(&state, tenant, node)
        .await
        .expect("the owner still reaches their own terminal on a shared machine");

    assert!(
        user_ctx(tenant, theirs)
            .require_session_access(&state, tenant, node)
            .await
            .is_err(),
        "sharing a machine handed over its terminals"
    );

    bed.teardown().await;
}
