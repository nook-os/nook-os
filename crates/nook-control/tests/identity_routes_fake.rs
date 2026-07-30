//! The routes-side identity rules, with **no database at all** (MAIN-247 AC-3).
//!
//! MAIN-246 put the identity *services* behind the trait; this card put the
//! routes behind it too, and these are the rules that migration could quietly
//! break. Each one used to need a migrated Postgres to assert — a token's owner
//! scope, the one-live-token invariant, `tenant_members`/`users.role` moving
//! together — and now needs none.
//!
//! Stop Postgres and run `cargo test -p nook-control --test identity_routes_fake`;
//! it passes, which is the AC's own verification step 3.

use nook_control::repo::identity::{
    FakeIdentityRepository, IdentityRepository, NewLocalUser, NewUserToken,
};
use nook_types::{TenantId, UserId};
use uuid::Uuid;

/// Revoking a token is scoped to its owner: one user revoking another's
/// credential is an administrative act, not a self-service one. The route
/// reports `NotFound` off the zero-row count, so the count is the contract.
#[tokio::test]
async fn a_token_can_only_be_revoked_by_the_user_who_owns_it() {
    let repo = FakeIdentityRepository::new();
    let t = repo.with_tenant("Home", "home");
    let mine = repo.with_user(t.id, "me@example.test", "member");
    let theirs = repo.with_user(t.id, "them@example.test", "member");

    let id = Uuid::now_v7();
    repo.create_user_token(NewUserToken {
        id,
        tenant: t.id,
        user_id: mine.id,
        token_hash: "hash".into(),
        name: "laptop".into(),
        expires_at: None,
    })
    .await
    .unwrap();

    assert_eq!(
        repo.revoke_user_token(id, theirs.id).await.unwrap(),
        0,
        "another user's revoke matches no row — the route turns this into 404"
    );
    assert_eq!(
        repo.list_user_tokens(mine.id).await.unwrap().len(),
        1,
        "and the token is still there"
    );

    assert_eq!(repo.revoke_user_token(id, mine.id).await.unwrap(), 1);
    assert!(repo.list_user_tokens(mine.id).await.unwrap().is_empty());
}

/// Tokens are listed only to the user who owns them.
#[tokio::test]
async fn tokens_are_listed_only_to_their_owner() {
    let repo = FakeIdentityRepository::new();
    let t = repo.with_tenant("Home", "home");
    let a = repo.with_user(t.id, "a@example.test", "member");
    let b = repo.with_user(t.id, "b@example.test", "member");

    for (user, name) in [(a.id, "a-token"), (b.id, "b-token")] {
        repo.create_user_token(NewUserToken {
            id: Uuid::now_v7(),
            tenant: t.id,
            user_id: user,
            token_hash: format!("h-{name}"),
            name: name.into(),
            expires_at: None,
        })
        .await
        .unwrap();
    }

    let names: Vec<String> = repo
        .list_user_tokens(a.id)
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert_eq!(names, vec!["a-token"], "b's credential is not a's business");
}

/// One live verification token per user: issuing a second drops the first, so a
/// stale link in an old email cannot still be redeemed.
#[tokio::test]
async fn issuing_a_verification_token_invalidates_the_previous_one() {
    let repo = FakeIdentityRepository::new();
    let t = repo.with_tenant("Home", "home");
    let user = repo.with_user(t.id, "me@example.test", "member");

    repo.issue_verification_token(user.id, "me@example.test", "hash-one")
        .await
        .unwrap();
    repo.issue_verification_token(user.id, "me@example.test", "hash-two")
        .await
        .unwrap();

    assert!(
        repo.verification_token("hash-one").await.unwrap().is_none(),
        "the superseded link is gone, not merely older"
    );
    assert!(repo.verification_token("hash-two").await.unwrap().is_some());
}

/// Consume-then-verify: a replayed link finds the token already spent. The
/// route reads `consumed_at` to decline, so it has to be set by consuming.
#[tokio::test]
async fn a_consumed_verification_token_stays_consumed() {
    let repo = FakeIdentityRepository::new();
    let t = repo.with_tenant("Home", "home");
    let user = repo.with_user(t.id, "me@example.test", "member");

    repo.issue_verification_token(user.id, "me@example.test", "hash")
        .await
        .unwrap();
    let tok = repo.verification_token("hash").await.unwrap().unwrap();
    assert!(tok.consumed_at.is_none(), "freshly issued, unspent");

    repo.consume_verification_token(tok.id).await.unwrap();

    let replayed = repo.verification_token("hash").await.unwrap().unwrap();
    assert!(
        replayed.consumed_at.is_some(),
        "a replay finds it spent rather than usable"
    );
}

/// `tenant_members.role` and `users.role` move together. The two disagreeing is
/// the bug the single method exists to prevent — authorization reads the table,
/// the UI reads the column.
#[tokio::test]
async fn changing_a_role_moves_the_membership_and_the_user_row_together() {
    let repo = FakeIdentityRepository::new();
    let t = repo.with_tenant("Home", "home");
    let user = repo.with_user(t.id, "me@example.test", "member");

    repo.change_member_role(t.id, user.id.0, "admin")
        .await
        .unwrap();

    assert_eq!(
        repo.membership_role(t.id, user.id.0)
            .await
            .unwrap()
            .as_deref(),
        Some("admin"),
        "the grant that actually decides access"
    );
    assert_eq!(
        repo.role_in_tenant(user.id, t.id).await.unwrap().as_deref(),
        Some("admin"),
        "…and the users row the UI renders, in step"
    );
}

/// The last-owner guard counts owners from `tenant_members`, so a tenant cannot
/// be left ownerless by a demotion or a removal.
#[tokio::test]
async fn owner_count_tracks_promotions_and_removals() {
    let repo = FakeIdentityRepository::new();
    let t = repo.with_tenant("Home", "home");
    let owner = repo.with_user(t.id, "owner@example.test", "owner");
    let member = repo.with_user(t.id, "member@example.test", "member");

    assert_eq!(repo.owner_count(t.id).await.unwrap(), 1);

    repo.change_member_role(t.id, member.id.0, "owner")
        .await
        .unwrap();
    assert_eq!(repo.owner_count(t.id).await.unwrap(), 2);

    assert_eq!(repo.remove_membership(t.id, owner.id.0).await.unwrap(), 1);
    assert_eq!(repo.owner_count(t.id).await.unwrap(), 1);

    // Removing someone who is not a member reports zero rather than pretending.
    assert_eq!(
        repo.remove_membership(t.id, Uuid::now_v7()).await.unwrap(),
        0
    );
}

/// Tenant *grants* are correlated by the user row, not by person — this is the
/// management list, where the question is which grants this row holds. (The
/// person-correlated list is `memberships_of`, and MAIN-246 covers it.)
#[tokio::test]
async fn the_grant_list_follows_the_membership_table() {
    let repo = FakeIdentityRepository::new();
    let home = repo.with_tenant("Home", "home");
    let shared = repo.with_tenant("Shared", "shared");
    let user = repo.with_user(home.id, "me@example.test", "owner");

    let only_home: Vec<String> = repo
        .tenant_grants_of(user.id.0)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.slug)
        .collect();
    assert_eq!(only_home, vec!["home"]);

    repo.grant_membership(shared.id, user.id, "member")
        .await
        .unwrap();
    let both: Vec<String> = repo
        .tenant_grants_of(user.id.0)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.slug)
        .collect();
    assert!(both.contains(&"home".to_string()) && both.contains(&"shared".to_string()));
}

/// The break-glass signal: how many local credentials a tenant has, so an OIDC
/// outage can be told apart from a lock-out. An OIDC account has no password
/// and must not count.
#[tokio::test]
async fn only_password_accounts_count_as_local_credentials() {
    let repo = FakeIdentityRepository::new();
    let t = repo.with_tenant("Home", "home");
    repo.with_user(t.id, "oidc@example.test", "member");

    assert_eq!(
        repo.count_local_credentials(t.id).await.unwrap(),
        0,
        "an OIDC account is not a way back in"
    );

    repo.create_local_user(NewLocalUser {
        tenant: t.id,
        display_name: "local".into(),
        email: "local@example.test".into(),
        username: "local".into(),
        password_hash: "not-a-real-hash".into(),
        role: "member".into(),
        grant_membership: true,
    })
    .await
    .unwrap()
    .expect("created");

    assert_eq!(repo.count_local_credentials(t.id).await.unwrap(), 1);
}

/// A session switch reports how many rows it moved: zero means the session
/// vanished between authentication and the update, which the route turns into
/// `Unauthorized` rather than a silent success.
#[tokio::test]
async fn switching_a_vanished_session_reports_zero_rows() {
    let repo = FakeIdentityRepository::new();
    let t = repo.with_tenant("Home", "home");
    let user = repo.with_user(t.id, "me@example.test", "owner");
    let session = nook_types::AuthSessionId::new();

    assert_eq!(
        repo.switch_session(session, user.id, t.id).await.unwrap(),
        0,
        "no such session yet"
    );

    repo.create_auth_session(session, user.id, t.id, 24)
        .await
        .unwrap();
    assert_eq!(
        repo.switch_session(session, user.id, t.id).await.unwrap(),
        1
    );

    repo.delete_auth_session(session.0).await.unwrap();
    assert_eq!(
        repo.switch_session(session, user.id, t.id).await.unwrap(),
        0,
        "a logged-out session cannot be switched"
    );
}

/// The dev-hatch purge is keyed strictly to the `test-%` marker and cascades.
/// Deleting anything else would be the accident this test exists to catch.
#[tokio::test]
async fn purging_test_tenants_takes_only_test_tenants() {
    let repo = FakeIdentityRepository::new();
    let real = repo.with_tenant("Home", "home");
    let junk = repo.with_tenant("test-abc", "test-abc");
    repo.with_user(real.id, "keep@example.test", "owner");
    repo.with_user(junk.id, "drop@example.test", "owner");

    assert_eq!(repo.purge_test_tenants().await.unwrap(), 1);
    assert!(repo.get_tenant(real.id).await.unwrap().is_some());
    assert!(repo.get_tenant(junk.id).await.unwrap().is_none());
    assert!(
        repo.user_and_tenant_by_email("drop@example.test")
            .await
            .unwrap()
            .is_none(),
        "the cascade took the tenant's users with it"
    );
    assert!(repo
        .user_and_tenant_by_email("keep@example.test")
        .await
        .unwrap()
        .is_some());

    // Idempotent: a second run deletes nothing.
    assert_eq!(repo.purge_test_tenants().await.unwrap(), 0);
}

/// Email lookup is case-insensitive — the dev-login path matches on it, and
/// `Me@Example.test` is the same person as `me@example.test`.
#[tokio::test]
async fn email_lookup_ignores_case() {
    let repo = FakeIdentityRepository::new();
    let t = repo.with_tenant("Home", "home");
    let user = repo.with_user(t.id, "me@example.test", "owner");

    assert_eq!(
        repo.user_and_tenant_by_email("ME@Example.TEST")
            .await
            .unwrap(),
        Some((user.id, t.id))
    );
    assert_eq!(
        repo.user_and_tenant_by_email("nobody@example.test")
            .await
            .unwrap(),
        None
    );
}

/// The dev-account browser filters on any of the three columns and caps the
/// page while reporting the true total — a cap that also truncated the count
/// would make the UI say there is nothing more to see.
#[tokio::test]
async fn dev_accounts_filter_on_any_column_and_report_the_uncapped_total() {
    let repo = FakeIdentityRepository::new();
    let t = repo.with_tenant("Acme", "acme");
    repo.with_user(t.id, "alice@example.test", "owner");
    repo.with_user(t.id, "bob@example.test", "member");
    repo.with_user(t.id, "carol@example.test", "member");

    let (page, total) = repo.dev_accounts_page(None, 2).await.unwrap();
    assert_eq!(
        (page.len(), total),
        (2, 3),
        "the cap limits the page, not the count"
    );

    // By email…
    let (page, _) = repo
        .dev_accounts_page(Some("%alice%".into()), 50)
        .await
        .unwrap();
    assert_eq!(page.len(), 1);

    // …and by tenant slug, which is the other reason to search.
    let (page, total) = repo
        .dev_accounts_page(Some("%acme%".into()), 50)
        .await
        .unwrap();
    assert_eq!((page.len(), total), (3, 3));
}

/// A tenant's members render with the role from the membership table.
#[tokio::test]
async fn a_member_item_carries_the_membership_role() {
    let repo = FakeIdentityRepository::new();
    let t = repo.with_tenant("Home", "home");
    let user = repo.with_user(t.id, "me@example.test", "member");

    let item = repo
        .member_item(t.id, user.id.0)
        .await
        .unwrap()
        .expect("a member");
    assert_eq!(item.email, "me@example.test");
    assert_eq!(item.role, "member");

    repo.change_member_role(t.id, user.id.0, "admin")
        .await
        .unwrap();
    assert_eq!(
        repo.member_item(t.id, user.id.0)
            .await
            .unwrap()
            .unwrap()
            .role,
        "admin"
    );

    // Someone with no grant is not a member of this tenant.
    assert!(repo
        .member_item(TenantId::new(), user.id.0)
        .await
        .unwrap()
        .is_none());
}

/// `tenant_of_user` addresses the outbound verification mail; an unknown user
/// resolves to nothing rather than to some tenant.
#[tokio::test]
async fn a_users_tenant_resolves_for_addressing_mail() {
    let repo = FakeIdentityRepository::new();
    let t = repo.with_tenant("Home", "home");
    let user = repo.with_user(t.id, "me@example.test", "owner");

    assert_eq!(repo.tenant_of_user(user.id).await.unwrap(), Some(t.id.0));
    assert_eq!(repo.tenant_of_user(UserId::new()).await.unwrap(), None);
}
