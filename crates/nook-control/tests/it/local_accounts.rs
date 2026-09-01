//! Local accounts: the parts that are only true against a real database.
//!
//! Two things here are load-bearing beyond "does it work". A password hash
//! must never reach a response, and the choice between OIDC and local sign-in
//! must be genuinely one-way — both are the kind of property that stays
//! correct right up until someone adds a field or a code path, which is what
//! makes them worth asserting rather than reviewing.

use nook_control::services::local_auth::{self, AuthMode};
use nook_db::{params, Db, DbPool};
use nook_testkit::TestBed;
use nook_types::TenantId;
use uuid::Uuid;

async fn seed_tenant(pool: &DbPool) -> TenantId {
    let id = TenantId::new();
    pool.exec(
        "INSERT INTO tenants (id, slug, name) VALUES ($1, $2, $2)",
        params![id, format!("local-{}", Uuid::now_v7().simple())],
    )
    .await
    .expect("tenant");
    id
}

const GOOD: &str = "correct horse battery staple";

#[tokio::test]
async fn an_account_signs_in_with_its_password_and_not_with_another() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let t = seed_tenant(&bed.db()).await;

    local_auth::create(
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        t,
        "ryan",
        "ryan@example.com",
        "Ryan",
        GOOD,
        true,
    )
    .await
    .expect("create");

    let (user, _) = local_auth::login(
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        t,
        "ryan",
        GOOD,
    )
    .await
    .expect("the right password must work");
    assert_eq!(user.role, "owner", "the first account owns the instance");

    assert!(local_auth::login(
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        t,
        "ryan",
        "wrong password entirely"
    )
    .await
    .is_err());
    // Usernames are case-insensitive: Alice and alice must be one person.
    assert!(local_auth::login(
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        t,
        "RYAN",
        GOOD
    )
    .await
    .is_ok());

    bed.teardown().await;
}

/// The whole point of the mode field. Once a tenant is OIDC, local sign-in is
/// closed — otherwise the same human can exist twice, with two ids and two
/// sets of grants, and RBAC has no way to say which one a permission meant.
#[tokio::test]
async fn choosing_a_sign_in_method_is_one_way() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let t = seed_tenant(&bed.db()).await;

    assert_eq!(
        local_auth::mode_of(
            &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
            t
        )
        .await
        .unwrap(),
        None,
        "a fresh tenant has not chosen yet"
    );

    local_auth::claim_mode(
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        t,
        AuthMode::Oidc,
    )
    .await
    .expect("first claim wins");
    assert_eq!(
        local_auth::mode_of(
            &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
            t
        )
        .await
        .unwrap(),
        Some(AuthMode::Oidc)
    );

    // Claiming the same mode again is fine — every later sign-in does it.
    local_auth::claim_mode(
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        t,
        AuthMode::Oidc,
    )
    .await
    .expect("re-claiming the same mode must be idempotent");

    let err = local_auth::claim_mode(
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        t,
        AuthMode::Local,
    )
    .await
    .expect_err("switching must be refused");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("oidc") && msg.contains("local"),
        "the error should name both modes so the operator knows what happened: {msg}"
    );

    bed.teardown().await;
}

/// A wrong password must not commit the tenant to local sign-in. Otherwise an
/// anonymous caller could lock an instance out of OIDC by guessing badly once.
#[tokio::test]
async fn a_failed_login_does_not_claim_the_mode() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let t = seed_tenant(&bed.db()).await;

    assert!(local_auth::login(
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        t,
        "nobody",
        "wrong"
    )
    .await
    .is_err());
    assert_eq!(
        local_auth::mode_of(
            &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
            t
        )
        .await
        .unwrap(),
        None,
        "a failed sign-in must leave the choice open"
    );

    bed.teardown().await;
}

/// `SELECT * FROM users` is used all over the codebase and the result is
/// serialised straight into responses. The hash lives in that table, so the
/// only thing keeping it out of the API is `User` not having the field.
#[tokio::test]
async fn the_password_hash_never_leaves_the_database() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let t = seed_tenant(&bed.db()).await;

    let user = local_auth::create(
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        t,
        "ryan",
        "r@example.com",
        "Ryan",
        GOOD,
        true,
    )
    .await
    .expect("create");

    // It really is stored…
    let (stored,): (Option<String>,) = bed
        .db()
        .query_one(
            "SELECT password_hash FROM users WHERE id = $1",
            params![user.id],
        )
        .await
        .unwrap();
    assert!(stored.unwrap().starts_with("$argon2id$"));

    // …and it really is absent from everything we hand out.
    let json = serde_json::to_string(&user).unwrap();
    for forbidden in ["argon2", "password", "hash", GOOD] {
        assert!(
            !json.to_lowercase().contains(forbidden),
            "serialised User leaked {forbidden:?}: {json}"
        );
    }

    // And from the shape the rest of the API fetches, which is the one that
    // would regress if someone added the field to the struct.
    let refetched: nook_types::User = bed
        .db()
        .query_one("SELECT * FROM users WHERE id = $1", params![user.id])
        .await
        .unwrap();
    let json = serde_json::to_string(&refetched).unwrap();
    assert!(!json.to_lowercase().contains("argon2"), "{json}");

    bed.teardown().await;
}

/// An OIDC account has no password here. Giving it one would mean two ways to
/// become the same person, and only one of them revocable at the provider.
#[tokio::test]
async fn an_oidc_account_cannot_be_given_a_password() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let t = seed_tenant(&bed.db()).await;

    let id = nook_types::UserId::new();
    bed.db()
        .exec(
            "INSERT INTO users (id, tenant_id, display_name, email, role)
         VALUES ($1, $2, 'Fed', 'fed@example.com', 'member')",
            params![id, t],
        )
        .await
        .unwrap();

    assert!(
        local_auth::change_password(
            &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
            id,
            "anything",
            "a-new-long-password"
        )
        .await
        .is_err(),
        "an account with no local password must not acquire one this way"
    );
    // And it cannot be signed into locally either.
    assert!(local_auth::login(
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        t,
        "fed",
        GOOD
    )
    .await
    .is_err());

    bed.teardown().await;
}

#[tokio::test]
async fn changing_a_password_requires_the_current_one() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let t = seed_tenant(&bed.db()).await;

    let user = local_auth::create(
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        t,
        "ryan",
        "r@example.com",
        "Ryan",
        GOOD,
        true,
    )
    .await
    .unwrap();

    assert!(local_auth::change_password(
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        user.id,
        "not the current one",
        "another-long-one"
    )
    .await
    .is_err());
    local_auth::change_password(
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        user.id,
        GOOD,
        "another-long-password",
    )
    .await
    .expect("the right current password must work");

    assert!(local_auth::login(
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        t,
        "ryan",
        GOOD
    )
    .await
    .is_err());
    assert!(local_auth::login(
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        t,
        "ryan",
        "another-long-password"
    )
    .await
    .is_ok());

    bed.teardown().await;
}

#[tokio::test]
async fn usernames_are_unique_within_a_tenant_case_insensitively() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let t = seed_tenant(&bed.db()).await;

    local_auth::create(
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        t,
        "ryan",
        "a@example.com",
        "A",
        GOOD,
        true,
    )
    .await
    .unwrap();
    let err = local_auth::create(
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        t,
        "RYAN",
        "b@example.com",
        "B",
        GOOD,
        false,
    )
    .await
    .expect_err("differing only by case must collide");
    assert!(format!("{err:?}").contains("already taken"), "{err:?}");

    // A different tenant is a different namespace.
    let other = seed_tenant(&bed.db()).await;
    local_auth::create(
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        other,
        "ryan",
        "c@example.com",
        "C",
        GOOD,
        true,
    )
    .await
    .expect("the same name in another tenant is fine");

    bed.teardown().await;
}

/// A password too short to store must be refused before a user row exists,
/// not after — a half-made account with no credential is a support ticket.
#[tokio::test]
async fn a_weak_password_creates_nothing() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let t = seed_tenant(&bed.db()).await;

    assert!(local_auth::create(
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        t,
        "ryan",
        "r@e.com",
        "R",
        "short",
        true
    )
    .await
    .is_err());

    let (n,): (i64,) = bed
        .db()
        .query_one(
            "SELECT count(*) FROM users WHERE tenant_id = $1",
            params![t],
        )
        .await
        .unwrap();
    assert_eq!(n, 0, "a rejected password must leave no user behind");

    bed.teardown().await;
}
