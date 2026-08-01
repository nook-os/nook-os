//! A grant belongs to the PERSON, not to one of their user rows.
//!
//! `users` is unique per `(tenant, email)`, so a human in two orgs is two rows
//! with two ids — and `role_bindings.subject_id` stores one of them. Every
//! grant therefore stopped applying the moment its holder acted in another
//! tenant, including a DEPLOYMENT-scoped one, whose entire meaning is
//! "everywhere".
//!
//! Found in production: an operator holding `operator` at deployment scope was
//! refused `node.manage` in a tenant they had just been added to, when trying to
//! authorize Claude on a node. It reads as a permissions bug in the new tenant
//! rather than as the grant not travelling with them.
//!
//! Ownership already learned this — `nodes.owner_person_id` is a PERSON because
//! a person outlives one membership (MAIN-119, MAIN-353). These are the same
//! rule applied to grants.

use nook_control::auth::perm::{Permission, Scope};
use nook_control::auth::{AuthCtx, Principal};
use nook_db::{params, Db};
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

/// A second user row for an EXISTING person — how one human holds membership of
/// two orgs, and the exact shape that broke.
async fn member(bed: &TestBed, tenant: TenantId, person: Uuid, role: &str) -> UserId {
    let user = UserId::new();
    bed.db()
        .exec(
            "INSERT INTO users (id, tenant_id, person_id, display_name, email, role)
             VALUES ($1, $2, $3, 'U', $4, $5)",
            params![
                user,
                tenant,
                person,
                format!("u-{}@example.test", user.0.simple()),
                role.to_string()
            ],
        )
        .await
        .expect("member");
    user
}

async fn bind(bed: &TestBed, user: UserId, role_key: &str, scope_type: &str, scope: Option<Uuid>) {
    bed.db()
        .exec(
            "INSERT INTO role_bindings (id, subject_type, subject_id, role_key, scope_type, scope_id)
             VALUES ($1, 'user', $2, $3, $4, $5)",
            params![Uuid::new_v4(), user.0, role_key.to_string(), scope_type.to_string(), scope],
        )
        .await
        .expect("bind");
}

#[tokio::test]
async fn a_deployment_grant_travels_to_every_tenant_its_holder_joins() {
    // THE production failure, reproduced: operator at deployment scope, granted
    // while acting in their own org, then used in somebody else's.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let mine = bed.tenant("mine").await;
    let theirs = bed.tenant("theirs").await;
    let (me_here, person) = bed.user(mine, "owner").await;
    let me_there = member(&bed, theirs, person, "admin").await;
    let state = bed.app_state().await;

    // Granted against the user row I had at the time — which is all a grant can
    // name, since that is what the operator page lists.
    bind(&bed, me_here, "operator", "deployment", None).await;

    ctx(me_here, mine)
        .require(&state, Permission::NodeManage, Scope::Tenant(mine))
        .await
        .expect("deployment scope covers my own tenant");

    ctx(me_there, theirs)
        .require(&state, Permission::NodeManage, Scope::Tenant(theirs))
        .await
        .expect("and every other tenant — that is what `deployment` means");

    bed.teardown().await;
}

#[tokio::test]
async fn a_tenant_grant_still_covers_only_its_own_tenant() {
    // The half that must NOT change. Matching by person widens WHO a grant
    // belongs to; it must not widen WHAT it covers, or every tenant admin would
    // quietly become an admin everywhere they were ever invited.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let mine = bed.tenant("mine").await;
    let theirs = bed.tenant("theirs").await;
    let (me_here, person) = bed.user(mine, "owner").await;
    let me_there = member(&bed, theirs, person, "admin").await;
    let state = bed.app_state().await;

    bind(&bed, me_here, "tenant_admin", "tenant", Some(mine.0)).await;

    ctx(me_here, mine)
        .require(&state, Permission::NodeManage, Scope::Tenant(mine))
        .await
        .expect("my own tenant, where the grant was scoped");

    assert!(
        ctx(me_there, theirs)
            .require(&state, Permission::NodeManage, Scope::Tenant(theirs))
            .await
            .is_err(),
        "a tenant-scoped grant must not leak into another tenant"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn somebody_elses_grant_is_not_mine() {
    // The person join must key on the person, not merely find *a* binding.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("shared").await;
    let (them, _their_person) = bed.user(tenant, "owner").await;
    let (me, _my_person) = bed.user(tenant, "member").await;
    let state = bed.app_state().await;

    bind(&bed, them, "operator", "deployment", None).await;

    assert!(
        ctx(me, tenant)
            .require(&state, Permission::NodeManage, Scope::Tenant(tenant))
            .await
            .is_err(),
        "another person's deployment grant is not mine"
    );

    bed.teardown().await;
}

// No test for a NULL `person_id`: the column is `NOT NULL` on both engine
// tracks, so the state is unreachable. The subject lookup relies on exactly
// that — matching on person alone always includes the caller's own row.
