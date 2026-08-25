//! A tenant owner holds their own tenant's permissions (QOL, after MAIN-353).
//!
//! `0001` backfilled `users.role IN ('owner','admin')` into a `tenant_admin`
//! binding ONCE, at migration time, and nothing has maintained it since. Every
//! tenant created afterwards, and every member promoted afterwards, has the role
//! and no binding — so an owner held no permissions at all in their own tenant
//! and the two mechanisms silently stopped meeting.
//!
//! It is derived on read now rather than written at each of the eight-plus
//! places a role is set, because a ninth insert site cannot forget something
//! nothing writes. What the role GRANTS still lives in `role_permissions` as
//! data; only the binding is implied.
//!
//! The edge that matters is scope: administering one tenant must never reach the
//! deployment, another tenant, or anything above.

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

/// A membership with an explicit tenant role and NO role binding — exactly what
/// every tenant created since `0001` looks like.
async fn seat(bed: &TestBed, tenant: TenantId, person: Uuid, role: &str) -> UserId {
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
        .expect("seat");
    user
}

#[tokio::test]
async fn an_owner_administers_their_own_tenant_without_a_binding() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("mine").await;
    let (owner, _p) = bed.user(tenant, "owner").await;
    let state = bed.app_state().await;

    for p in [
        Permission::TenantManage,
        Permission::TenantView,
        Permission::NodeManage,
        Permission::AuditView,
    ] {
        ctx(owner, tenant)
            .require(&state, p, Scope::Tenant(tenant))
            .await
            .unwrap_or_else(|e| panic!("owner should hold {}: {e:?}", p.key()));
    }

    bed.teardown().await;
}

#[tokio::test]
async fn an_admin_does_too_and_a_member_does_not() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("mine").await;
    let (_o, person) = bed.user(tenant, "owner").await;
    let admin = seat(&bed, bed.tenant("other").await, person, "member").await;
    let _ = admin;
    let (member, _p2) = bed.user(tenant, "member").await;
    let (adm, _p3) = bed.user(tenant, "admin").await;
    let state = bed.app_state().await;

    ctx(adm, tenant)
        .require(&state, Permission::TenantManage, Scope::Tenant(tenant))
        .await
        .expect("an admin administers the tenant");

    assert!(
        ctx(member, tenant)
            .require(&state, Permission::TenantManage, Scope::Tenant(tenant))
            .await
            .is_err(),
        "a plain member does not — this is the gate that stops anybody \
         turning the fleet on"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn administering_one_tenant_reaches_no_other_tenant() {
    // The whole risk of deriving this. An owner is an owner of THEIR tenant.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let mine = bed.tenant("mine").await;
    let theirs = bed.tenant("theirs").await;
    let (owner, _p) = bed.user(mine, "owner").await;
    let state = bed.app_state().await;

    assert!(
        ctx(owner, mine)
            .require(&state, Permission::TenantManage, Scope::Tenant(theirs))
            .await
            .is_err(),
        "owning one tenant is not owning the next one"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn administering_a_tenant_reaches_neither_the_deployment_nor_its_org() {
    // `$4` is NULL for a deployment- or org-scoped ask, so the implicit arm
    // matches nothing. Running one tenant is not running the deployment, and
    // this is the assertion that keeps it that way.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("mine").await;
    let (owner, _p) = bed.user(tenant, "owner").await;
    let state = bed.app_state().await;

    assert!(
        ctx(owner, tenant)
            .require(&state, Permission::TenantManage, Scope::Deployment)
            .await
            .is_err(),
        "a tenant owner does not administer the deployment"
    );
    assert!(
        ctx(owner, tenant)
            .require(&state, Permission::RbacGrant, Scope::Tenant(tenant))
            .await
            .is_err(),
        "and never gains rbac.grant — `tenant_admin` does not hold it, so \
         deriving the binding cannot hand out the power to appoint"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn a_second_seat_does_not_carry_the_first_seats_role() {
    // Bindings travel by person (MAIN-353's rule); this implicit one must NOT.
    // Being the owner of your own tenant cannot make you an admin of a team you
    // merely joined — the seat's OWN role is what counts.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let mine = bed.tenant("mine").await;
    let theirs = bed.tenant("theirs").await;
    let (_me, person) = bed.user(mine, "owner").await;
    let joined = seat(&bed, theirs, person, "member").await;
    let state = bed.app_state().await;

    assert!(
        ctx(joined, theirs)
            .require(&state, Permission::TenantManage, Scope::Tenant(theirs))
            .await
            .is_err(),
        "owner over there, member over here — the seat decides"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn an_explicit_binding_still_works_on_its_own() {
    // The derived arm is additive. A `tenant_admin` binding granted to somebody
    // whose seat says `member` keeps working, which is what makes the operator
    // page's tenant-scoped grant meaningful.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("mine").await;
    let (member, _p) = bed.user(tenant, "member").await;
    let state = bed.app_state().await;

    bed.db()
        .exec(
            "INSERT INTO role_bindings (id, subject_type, subject_id, role_key, scope_type, scope_id)
             VALUES ($1, 'user', $2, 'tenant_admin', 'tenant', $3)",
            params![Uuid::new_v4(), member.0, tenant],
        )
        .await
        .expect("bind");

    ctx(member, tenant)
        .require(&state, Permission::TenantManage, Scope::Tenant(tenant))
        .await
        .expect("an explicit grant still stands on its own");

    bed.teardown().await;
}
