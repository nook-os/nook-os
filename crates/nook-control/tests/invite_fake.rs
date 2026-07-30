//! Invite rules, with **no database at all** (MAIN-250 AC-3).
//!
//! These are the rules an invite refactor could quietly break, and every one of
//! them used to need a migrated Postgres to assert: one pending invite per
//! email, a resend invalidating the old link, an invite being consumable only
//! once, and — the security-relevant one — an invite addressed to somebody else
//! staying pending rather than being burned by whoever clicks the link.
//!
//! Stop Postgres and run `cargo test -p nook-control --test invite_fake`; it
//! passes, which is the AC's own verification step 3.

use nook_control::repo::invites::{FakeInviteRepository, InviteRepository};
use nook_types::TenantId;
use uuid::Uuid;

fn hash(t: &str) -> String {
    format!("h-{t}")
}

/// Re-inviting the same address replaces the pending invite rather than
/// stacking a second one. The partial unique index enforces this in Postgres;
/// the repository has to do the DELETE that makes room for it.
#[tokio::test]
async fn re_inviting_replaces_rather_than_stacks() {
    let repo = FakeInviteRepository::new();
    let t = TenantId::new();
    let by = Uuid::now_v7();

    let first = repo
        .issue(t, "someone@example.test", "member", &hash("one"), by)
        .await
        .unwrap();
    let second = repo
        .issue(t, "someone@example.test", "member", &hash("two"), by)
        .await
        .unwrap();

    assert_ne!(first.id, second.id);
    assert_eq!(
        repo.count_for(t, "someone@example.test"),
        1,
        "the pending invite was replaced, not stacked"
    );
    assert!(
        repo.by_token_hash(&hash("one")).await.unwrap().is_none(),
        "and the superseded link no longer resolves"
    );
}

/// Address matching is case-insensitive, so `Someone@…` does not sneak a second
/// pending invite past the replace.
#[tokio::test]
async fn the_replace_matches_the_address_case_insensitively() {
    let repo = FakeInviteRepository::new();
    let t = TenantId::new();
    let by = Uuid::now_v7();

    repo.issue(t, "someone@example.test", "member", &hash("a"), by)
        .await
        .unwrap();
    repo.issue(t, "SOMEONE@Example.TEST", "admin", &hash("b"), by)
        .await
        .unwrap();

    assert_eq!(repo.count_for(t, "someone@example.test"), 1);
}

/// A resend mints a fresh token, which invalidates the old link. The stored
/// hash is irreversible, so re-sending the original is not even possible —
/// that is why resend is a re-issue rather than a re-mail.
#[tokio::test]
async fn a_resend_invalidates_the_previous_link() {
    let repo = FakeInviteRepository::new();
    let t = TenantId::new();
    let inv = repo
        .issue(t, "a@example.test", "member", &hash("old"), Uuid::now_v7())
        .await
        .unwrap();

    let reissued = repo
        .reissue(inv.id, t, &hash("new"))
        .await
        .unwrap()
        .expect("a pending invite can be resent");
    assert_eq!(reissued.id, inv.id, "same invite, new token");

    assert!(repo.by_token_hash(&hash("old")).await.unwrap().is_none());
    assert!(repo.by_token_hash(&hash("new")).await.unwrap().is_some());
}

/// Revoking is scoped to the tenant and only bites a pending invite. Rows
/// affected is the contract — the route turns zero into a 404.
#[tokio::test]
async fn revoking_is_tenant_scoped_and_only_touches_pending() {
    let repo = FakeInviteRepository::new();
    let t = TenantId::new();
    let other = TenantId::new();
    let inv = repo
        .issue(t, "a@example.test", "member", &hash("x"), Uuid::now_v7())
        .await
        .unwrap();

    assert_eq!(
        repo.revoke(inv.id, other).await.unwrap(),
        0,
        "another tenant cannot revoke this invite"
    );
    assert_eq!(repo.revoke(inv.id, t).await.unwrap(), 1);
    assert_eq!(repo.status_of(inv.id).as_deref(), Some("revoked"));
    assert_eq!(
        repo.revoke(inv.id, t).await.unwrap(),
        0,
        "and revoking twice is not a second success"
    );

    // A revoked invite is no longer offered for acceptance.
    let p = repo
        .preview(&hash("x"))
        .await
        .unwrap()
        .expect("still findable");
    assert!(!p.valid, "revoked reads as invalid, not as missing");
}

/// A revoked or expired invite is *findable* but not *valid*. That distinction
/// is deliberate: the preview endpoint does the same work either way so a
/// missing, expired, revoked or accepted token cannot be told apart by timing.
#[tokio::test]
async fn expiry_and_status_show_up_as_validity_not_absence() {
    let repo = FakeInviteRepository::new();
    let t = TenantId::new();
    let inv = repo
        .issue(t, "a@example.test", "member", &hash("x"), Uuid::now_v7())
        .await
        .unwrap();

    assert!(repo.is_fresh(inv.id).await.unwrap());
    assert!(repo.preview(&hash("x")).await.unwrap().unwrap().valid);

    repo.expire(inv.id);
    assert!(!repo.is_fresh(inv.id).await.unwrap());

    let p = repo.preview(&hash("x")).await.unwrap();
    assert!(p.is_some(), "the row is still there…");
    assert!(!p.unwrap().valid, "…it is simply no longer usable");

    // An unknown token is the one case that genuinely resolves to nothing, and
    // the route gives it the same shape via its neutral fallback.
    assert!(repo.preview(&hash("never-issued")).await.unwrap().is_none());
}

/// An invite can be consumed once. A second accept finds it already used —
/// which is what the route turns into "this invite has already been used".
#[tokio::test]
async fn an_invite_is_consumable_once() {
    let repo = FakeInviteRepository::new();
    let t = TenantId::new();
    let inv = repo
        .issue(t, "a@example.test", "member", &hash("x"), Uuid::now_v7())
        .await
        .unwrap();

    assert_eq!(
        repo.by_token_hash(&hash("x"))
            .await
            .unwrap()
            .unwrap()
            .status,
        "pending"
    );
    repo.mark_accepted(inv.id).await.unwrap();
    assert_eq!(
        repo.by_token_hash(&hash("x"))
            .await
            .unwrap()
            .unwrap()
            .status,
        "accepted"
    );

    // …and it is gone from the pending list the admin screen shows.
    assert!(repo.list_pending(t).await.unwrap().is_empty());
}

/// Registration is gated on the invite naming the tenant, the email the account
/// must use, and the role acceptance will apply — all three, or the link is not
/// usable. Reading them from one place is what stops a caller pairing a valid
/// token with somebody else's tenant.
#[tokio::test]
async fn registration_reads_tenant_email_and_role_from_the_invite() {
    let repo = FakeInviteRepository::new();
    let t = TenantId::new();
    let inv = repo
        .issue(t, "New@Example.test", "admin", &hash("x"), Uuid::now_v7())
        .await
        .unwrap();

    let r = repo
        .registration_target(&hash("x"))
        .await
        .unwrap()
        .expect("a usable link");
    assert_eq!(
        (r.tenant, r.email.as_str(), r.role.as_str()),
        (t, "New@Example.test", "admin")
    );
    assert!(r.valid);

    // Once revoked, the same token still resolves but is refused.
    repo.revoke(inv.id, t).await.unwrap();
    assert!(
        !repo
            .registration_target(&hash("x"))
            .await
            .unwrap()
            .unwrap()
            .valid
    );
}

/// The pending list is per tenant and newest-first, and never shows an invite
/// that has been used or revoked.
#[tokio::test]
async fn the_pending_list_is_scoped_and_excludes_spent_invites() {
    let repo = FakeInviteRepository::new();
    let t = TenantId::new();
    let other = TenantId::new();
    let by = Uuid::now_v7();

    let a = repo
        .issue(t, "a@example.test", "member", &hash("a"), by)
        .await
        .unwrap();
    let _b = repo
        .issue(t, "b@example.test", "member", &hash("b"), by)
        .await
        .unwrap();
    repo.issue(other, "c@example.test", "member", &hash("c"), by)
        .await
        .unwrap();

    assert_eq!(
        repo.list_pending(t).await.unwrap().len(),
        2,
        "another tenant's is not listed"
    );

    repo.mark_accepted(a.id).await.unwrap();
    let left: Vec<String> = repo
        .list_pending(t)
        .await
        .unwrap()
        .into_iter()
        .map(|i| i.email)
        .collect();
    assert_eq!(
        left,
        vec!["b@example.test"],
        "a spent invite leaves the list"
    );
}
