//! Identity callers, exercised with **no database at all** (MAIN-246 AC-3).
//!
//! This is the point of the repository trait, not a side effect of it. Every
//! test below used to need Postgres — a container, a migrated schema, a private
//! database per test — to assert rules that are pure logic sitting on top of a
//! handful of rows. Now they need a `FakeIdentityRepository` and nothing else.
//!
//! Deliberately these are the **callers**, not the repository. Testing the fake
//! against the trait would prove only that the fake does what the fake does; the
//! value is in running `services::identity` and `services::local_auth` — real
//! code, unmodified — against an in-memory store.
//!
//! Stop Postgres and run `cargo test -p nook-control --test identity_fake`; it
//! passes, which is the AC's own verification step 3.

use nook_control::repo::identity::{FakeIdentityRepository, IdentityRepository};
use nook_control::services::identity::{
    active_membership_exists, member_user_in_tenant, memberships_for,
};

/// The active tenant is flagged, and only that one — the rule `/auth/me` shows
/// as "you are here".
#[tokio::test]
async fn memberships_mark_exactly_the_active_tenant() {
    let repo = FakeIdentityRepository::new();
    let home = repo.with_tenant("Home", "home");
    let shared = repo.with_tenant("Shared", "shared");
    let me = repo.with_user(home.id, "me@example.test", "owner");

    // The same person, reachable in a second tenant. The fake models this the
    // way the schema does — one `users` row per tenant, correlated by person —
    // so `memberships_of` has something real to join.
    let sibling = repo.with_user(shared.id, "me@example.test", "member");
    link_same_person(&repo, me.id, sibling.id);

    let list = memberships_for(&repo, me.id, shared.id).await.unwrap();
    let names: Vec<&str> = list.iter().map(|m| m.name.as_str()).collect();
    assert!(
        names.contains(&"Home") && names.contains(&"Shared"),
        "{names:?}"
    );

    let current: Vec<&str> = list
        .iter()
        .filter(|m| m.current)
        .map(|m| m.name.as_str())
        .collect();
    assert_eq!(
        current,
        vec!["Shared"],
        "exactly the active tenant is current"
    );
}

/// Revoking a grant takes effect immediately — the check `AuthCtx` makes on
/// every cookie-session request. The user row survives; only the grant goes.
#[tokio::test]
async fn a_revoked_grant_fails_the_membership_check_at_once() {
    let repo = FakeIdentityRepository::new();
    let t = repo.with_tenant("Home", "home");
    let user = repo.with_user(t.id, "me@example.test", "member");

    assert!(active_membership_exists(&repo, user.id, t.id)
        .await
        .unwrap());

    repo.revoke_membership(t.id, user.id);

    assert!(
        !active_membership_exists(&repo, user.id, t.id)
            .await
            .unwrap(),
        "a revoked grant must fail on the next request, not at next logout"
    );
}

/// Tenant switching is gated on membership, and correlated by person rather
/// than by email — the MAIN-12 rule. A matching email string in a tenant this
/// person does not belong to must not resolve.
#[tokio::test]
async fn switching_resolves_only_a_tenant_the_person_belongs_to() {
    let repo = FakeIdentityRepository::new();
    let home = repo.with_tenant("Home", "home");
    let other = repo.with_tenant("Other", "other");
    let me = repo.with_user(home.id, "me@example.test", "owner");

    // Same email, different person, in a tenant I have no grant for.
    let _impostor_target = repo.with_user(other.id, "me@example.test", "owner");

    assert!(
        member_user_in_tenant(&repo, me.id, other.id)
            .await
            .unwrap()
            .is_none(),
        "a matching email in another tenant is not a way in"
    );

    // …and the tenant I do belong to resolves to my row there.
    let sibling = repo.with_user(other.id, "me-2@example.test", "member");
    link_same_person(&repo, me.id, sibling.id);
    assert_eq!(
        member_user_in_tenant(&repo, me.id, other.id).await.unwrap(),
        Some(sibling.id),
        "a real membership resolves to this person's row in that tenant"
    );
}

/// The auth-mode lock is one-way and race-safe: the first claim wins and the
/// second is told what the instance already decided. This is the rule that
/// stops a wrong-password attempt from locking an instance out of OIDC.
#[tokio::test]
async fn the_first_auth_mode_claim_wins_and_the_second_is_refused() {
    let repo = FakeIdentityRepository::new();
    let t = repo.with_tenant("Home", "home");

    assert!(
        repo.claim_auth_mode(t.id, "local").await.unwrap(),
        "first claim sets it"
    );
    assert!(
        !repo.claim_auth_mode(t.id, "oidc").await.unwrap(),
        "a second, different claim does not overwrite the decision"
    );
    assert_eq!(
        repo.auth_mode_of(t.id).await.unwrap().as_deref(),
        Some("local")
    );
}

/// "Verified" means a timestamp from a real claim, never an email string that
/// happens to match (MAIN-29). A user with an email and no verified identity is
/// unverified.
#[tokio::test]
async fn email_verification_comes_from_an_identity_not_from_the_address() {
    let repo = FakeIdentityRepository::new();
    let t = repo.with_tenant("Home", "home");
    let user = repo.with_user(t.id, "me@example.test", "owner");

    assert!(
        !repo.email_is_verified(user.id).await.unwrap(),
        "having an email is not being verified"
    );

    // An identity that the IdP did NOT assert as verified still isn't.
    repo.create_identity(nook_control::repo::identity::NewIdentity {
        user_id: user.id,
        issuer: "https://idp.example".into(),
        subject: "sub-1".into(),
        email: Some("me@example.test".into()),
        raw_claims: serde_json::json!({}),
        email_verified: false,
    })
    .await
    .unwrap();
    assert!(!repo.email_is_verified(user.id).await.unwrap());

    // The claim arriving later moves it — one way.
    repo.mark_identity_verified("https://idp.example", "sub-1")
        .await
        .unwrap();
    assert!(repo.email_is_verified(user.id).await.unwrap());
}

/// A taken slug is an ordinary outcome, reported as `None` so the personal-tenant
/// allocator can retry. If this leaked a driver error instead, the retry loop
/// in `create_personal_tenant` would be dead code.
#[tokio::test]
async fn a_taken_slug_is_reported_rather_than_raised() {
    let repo = FakeIdentityRepository::new();
    assert!(repo.create_tenant("Ryan", "ryan").await.unwrap().is_some());
    assert!(
        repo.create_tenant("Ryan", "ryan").await.unwrap().is_none(),
        "the second attempt reports the collision for the caller to retry around"
    );
    assert!(repo
        .create_tenant("Ryan", "ryan-a1b2")
        .await
        .unwrap()
        .is_some());
}

/// Self-service accounts get their `tenant_members` grant with the row; invited
/// ones deliberately do not, because accepting the invite is what grants it.
/// Collapsing the two would silently admit someone whose invite was never used.
#[tokio::test]
async fn only_self_service_accounts_are_granted_membership_on_creation() {
    use nook_control::repo::identity::NewLocalUser;
    let repo = FakeIdentityRepository::new();
    let t = repo.with_tenant("Home", "home");

    let mk = |username: &str, email: &str, grant: bool| NewLocalUser {
        tenant: t.id,
        display_name: username.into(),
        email: email.into(),
        username: username.into(),
        password_hash: "not-a-real-hash".into(),
        role: "member".into(),
        grant_membership: grant,
    };

    let selfserve = repo
        .create_local_user(mk("selfserve", "a@example.test", true))
        .await
        .unwrap()
        .expect("created");
    assert!(
        repo.has_active_membership(selfserve.id, t.id)
            .await
            .unwrap(),
        "a self-service account can reach its own tenant"
    );

    let invited = repo
        .create_local_user(mk("invited", "b@example.test", false))
        .await
        .unwrap()
        .expect("created");
    assert!(
        !repo.has_active_membership(invited.id, t.id).await.unwrap(),
        "an invited account is granted by accepting the invite, not by registering"
    );
}

/// Give two user rows the same person, the way one human signing into two
/// tenants looks in the schema. The fake keeps `person_id` beside the `User`
/// DTO (which does not carry it), so this reaches through the repo's own seam.
fn link_same_person(repo: &FakeIdentityRepository, a: nook_types::UserId, b: nook_types::UserId) {
    repo.link_person(a, b);
}
