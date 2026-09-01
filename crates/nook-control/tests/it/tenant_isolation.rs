//! Tenant provisioning: a new person gets their own tenant, and cannot see
//! anyone else's.
//!
//! This is the rule that, when it breaks, breaks quietly — everything still
//! works, there is just one more machine in your list than there should be.
//! So it is tested against a real database through the real login path rather
//! than by reading the code.
//!
//! Needs a running Postgres (the dev stack's works): set `DATABASE_URL`. The
//! tests skip cleanly when it's absent. Setup + teardown run through
//! `nook_testkit::TestBed` (MAIN-156).

use nook_control::services::identity::{email_is_verified, login_identity, IdentityClaims};
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::TenantId;
use uuid::Uuid;

fn claims(subject: &str, name: &str) -> IdentityClaims {
    claims_verified(subject, name, false)
}

fn claims_verified(subject: &str, name: &str, email_verified: bool) -> IdentityClaims {
    IdentityClaims {
        issuer: "test-idp".into(),
        subject: subject.into(),
        email: Some(format!("{subject}@example.test")),
        email_verified,
        display_name: Some(name.into()),
        avatar_url: None,
        raw_claims: serde_json::json!({}),
    }
}

/// The whole point: two people signing in do not end up in one tenant.
#[tokio::test]
async fn each_new_user_gets_their_own_tenant() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping: DATABASE_URL not set / postgres unreachable");
        return;
    };
    let state = bed.app_state().await;

    // Unique subjects so the test is re-runnable against a live dev database.
    let a_sub = format!("alice-{}", Uuid::now_v7().simple());
    let b_sub = format!("bob-{}", Uuid::now_v7().simple());

    let (a_user, a_tenant) = login_identity(&state, claims(&a_sub, "Alice"))
        .await
        .expect("alice signs in");
    let (b_user, b_tenant) = login_identity(&state, claims(&b_sub, "Bob"))
        .await
        .expect("bob signs in");

    assert_ne!(
        a_tenant.id, b_tenant.id,
        "two new users landed in the same tenant — they would see each other's nodes"
    );
    assert_eq!(a_user.tenant_id, a_tenant.id);
    assert_eq!(b_user.tenant_id, b_tenant.id);
    assert_eq!(a_user.role, "owner", "you own the tenant made for you");
    assert_eq!(b_user.role, "owner");

    // Neither can see the other: scoping is by tenant, and the tenants differ.
    let (shared,): (i64,) = bed
        .db()
        .query_one(
            "SELECT count(*) FROM tenant_members
         WHERE principal_id IN ($1, $2) AND tenant_id IN ($3, $4)",
            params![a_user.id.0, b_user.id.0, a_tenant.id, b_tenant.id],
        )
        .await
        .unwrap();
    assert_eq!(
        shared, 2,
        "each user belongs to exactly one of the two tenants"
    );

    bed.teardown().await;
}

/// Signing in again is not a new tenant — the identity is already known.
#[tokio::test]
async fn returning_user_keeps_their_tenant() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping: DATABASE_URL not set / postgres unreachable");
        return;
    };
    let state = bed.app_state().await;
    let sub = format!("carol-{}", Uuid::now_v7().simple());

    let (first_user, first_tenant) = login_identity(&state, claims(&sub, "Carol"))
        .await
        .expect("first sign-in");
    let (again_user, again_tenant) = login_identity(&state, claims(&sub, "Carol"))
        .await
        .expect("second sign-in");

    assert_eq!(first_tenant.id, again_tenant.id);
    assert_eq!(first_user.id, again_user.id);

    let (tenant_count,): (i64,) = bed
        .db()
        .query_one(
            "SELECT count(*) FROM tenants WHERE id = $1",
            params![first_tenant.id],
        )
        .await
        .unwrap();
    assert_eq!(tenant_count, 1);

    bed.teardown().await;
}

/// Membership is written alongside the user, because that table is what teams
/// will read — if provisioning skips it, a user belongs to a tenant by one
/// rule and not by the other.
#[tokio::test]
async fn membership_row_mirrors_the_personal_tenant() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping: DATABASE_URL not set / postgres unreachable");
        return;
    };
    let state = bed.app_state().await;
    let sub = format!("dave-{}", Uuid::now_v7().simple());

    let (user, tenant) = login_identity(&state, claims(&sub, "Dave"))
        .await
        .expect("dave signs in");

    let row: Option<(String, String)> = bed
        .db()
        .query_opt(
            "SELECT principal_type, role FROM tenant_members
         WHERE tenant_id = $1 AND principal_id = $2",
            params![tenant.id, user.id.0],
        )
        .await
        .unwrap();

    let (principal_type, role) = row.expect("membership row was not written");
    assert_eq!(principal_type, "user");
    assert_eq!(role, "owner");

    bed.teardown().await;
}

/// There is no way to make two people share a tenant by signing in. The flag
/// that used to do it is gone, so this asserts the property directly: two new
/// identities, two tenants, no configuration involved.
#[tokio::test]
async fn every_new_identity_gets_its_own_tenant() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping: DATABASE_URL not set / postgres unreachable");
        return;
    };
    let state = bed.app_state().await;

    let a = format!("erin-{}", Uuid::now_v7().simple());
    let b = format!("frank-{}", Uuid::now_v7().simple());
    let (a_user, a_tenant) = login_identity(&state, claims(&a, "Erin")).await.unwrap();
    let (b_user, b_tenant) = login_identity(&state, claims(&b, "Frank")).await.unwrap();

    assert_ne!(
        a_tenant.id, b_tenant.id,
        "signing in must never drop a new person into someone else's tenant"
    );
    assert_eq!(
        a_user.role, "owner",
        "a personal tenant is owned by its person"
    );
    assert_eq!(b_user.role, "owner");

    bed.teardown().await;
}

/// A node token is a service credential, not the owner's password.
///
/// It authenticates every machine that joined, and it sits in a plain file on
/// a box whose job is running other people's code. So it may do a node's work
/// — read the tenant, drive sessions — but not the things that hand over
/// lasting control: the vault, enrolling machines, evicting other nodes.
#[tokio::test]
async fn node_tokens_cannot_escalate() {
    use nook_control::auth::{AuthCtx, Principal};
    use nook_types::{AuthSessionId, NodeId, UserId};

    let node = AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: UserId(Uuid::nil()),
        tenant_id: TenantId(Uuid::nil()),
        principal: Principal::Node(NodeId(Uuid::now_v7())),
        cookie_session: false,
    };
    let human = AuthCtx {
        principal: Principal::User,
        ..node
    };

    assert!(
        node.require_user().is_err(),
        "a node token must be refused for owner-only operations"
    );
    assert!(human.require_user().is_ok(), "a signed-in user must not be");
}

/// A node token is confined to its own machine.
///
/// This is the lateral-movement boundary: starting a session, cloning, or
/// attaching a terminal all execute code on the node they name. One
/// compromised machine must not become every machine.
#[tokio::test]
async fn node_tokens_are_confined_to_their_own_machine() {
    use nook_control::auth::{AuthCtx, Principal};
    use nook_types::{AuthSessionId, NodeId, UserId};

    let self_id = NodeId(Uuid::now_v7());
    let other_id = NodeId(Uuid::now_v7());

    let node = AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: UserId(Uuid::nil()),
        tenant_id: TenantId(Uuid::nil()),
        principal: Principal::Node(self_id),
        cookie_session: false,
    };
    let human = AuthCtx {
        principal: Principal::User,
        ..node
    };

    assert!(
        node.require_node_self(self_id).is_ok(),
        "a node must still be able to act on itself — that is the CLI"
    );
    assert!(
        node.require_node_self(other_id).is_err(),
        "a node token reached another machine: lateral movement is open"
    );
    // `require_node_self` still waves a human through — it is the MANAGEMENT
    // confinement (kill/rescan/update/delete), and driving other nodes there is
    // the whole point of the control plane. It is no longer the SPAWN gate.
    assert!(
        human.require_node_self(other_id).is_ok(),
        "management ops still let a human act on any node in their tenant",
    );

    // MAIN-130: the inverted expectation. `require_node_self` used to be the
    // authority for STARTING A SESSION too, so any human sailed onto any node —
    // the spawn vuln. Spawning now goes through `require_node_owner`, which
    // refuses a human on a node they do not own. Asserted here against a real
    // node, with the comprehensive owner/member/admin/ownerless/MCP matrix in
    // tests/node_owner.rs.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;

    let tenant = bed.tenant("iso").await;
    // A member whose person owns NO node, and a node owned by someone else.
    let (member, _member_person) = bed.user(tenant, "member").await;
    let their_node = bed.node(tenant, Uuid::now_v7()).await; // owned by a different person

    let member_ctx = AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: member,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: false,
    };
    assert!(
        member_ctx
            .require_node_owner(&state, their_node)
            .await
            .is_err(),
        "MAIN-130: a human must be refused starting a session on a node they do not own",
    );

    bed.teardown().await;
}

/// MAIN-29: an OIDC login whose IdP asserts `email_verified=true` stamps the
/// identity, and the predicate reports it. An unverified claim leaves it null.
#[tokio::test]
async fn oidc_email_verified_claim_sets_the_timestamp_and_predicate() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping: DATABASE_URL not set / postgres unreachable");
        return;
    };
    let state = bed.app_state().await;

    let v_sub = format!("verified-{}", Uuid::now_v7().simple());
    let (v_user, _v_tenant) = login_identity(&state, claims_verified(&v_sub, "Vera", true))
        .await
        .expect("verified user signs in");
    let u_sub = format!("unverified-{}", Uuid::now_v7().simple());
    let (u_user, _u_tenant) = login_identity(&state, claims_verified(&u_sub, "Uri", false))
        .await
        .expect("unverified user signs in");

    // The column reflects the claim…
    let (v_at,): (Option<chrono::DateTime<chrono::Utc>>,) = bed
        .db()
        .query_one(
            "SELECT email_verified_at FROM identities WHERE subject = $1",
            params![v_sub.clone()],
        )
        .await
        .unwrap();
    let (u_at,): (Option<chrono::DateTime<chrono::Utc>>,) = bed
        .db()
        .query_one(
            "SELECT email_verified_at FROM identities WHERE subject = $1",
            params![u_sub.clone()],
        )
        .await
        .unwrap();

    // …and so does the predicate.
    let v_pred = email_is_verified(
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        v_user.id,
    )
    .await
    .unwrap();
    let u_pred = email_is_verified(
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        u_user.id,
    )
    .await
    .unwrap();

    bed.teardown().await;

    assert!(
        v_at.is_some(),
        "email_verified=true must stamp the timestamp"
    );
    assert!(
        u_at.is_none(),
        "an unverified claim must leave the timestamp null"
    );
    assert!(v_pred, "predicate is true for a verified identity");
    assert!(!u_pred, "predicate is false when the timestamp is null");
}

/// MAIN-29: a returning identity that was unverified becomes verified the first
/// time the IdP asserts it — verification only moves one way.
#[tokio::test]
async fn returning_login_records_a_newly_verified_email() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping: DATABASE_URL not set / postgres unreachable");
        return;
    };
    let state = bed.app_state().await;
    let sub = format!("laterverify-{}", Uuid::now_v7().simple());

    let (user, _tenant) = login_identity(&state, claims_verified(&sub, "Lee", false))
        .await
        .expect("first sign-in, unverified");
    assert!(
        !email_is_verified(
            &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
            user.id
        )
        .await
        .unwrap(),
        "starts unverified"
    );

    // The IdP now confirms the address.
    login_identity(&state, claims_verified(&sub, "Lee", true))
        .await
        .expect("second sign-in, now verified");
    let verified = email_is_verified(
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        user.id,
    )
    .await
    .unwrap();

    bed.teardown().await;
    assert!(verified, "a later verified login records the verification");
}
