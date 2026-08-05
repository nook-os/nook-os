//! Local-auth registration through an invite (MAIN-98), against a live Postgres.
//! Set `DATABASE_URL`.
//!
//! The security-critical shape: a local invitee registers → verifies → signs in
//! WITHOUT tenant membership → accepts → gains membership. Identity is decoupled
//! from membership. Setup + teardown run through `nook_testkit::TestBed`
//! (MAIN-156).

use axum::extract::{ConnectInfo, State};
use axum::http::HeaderMap;
use axum::Json;
use nook_control::auth::IdentityCtx;
use nook_control::error::ApiResult;
use nook_control::routes::invites;
use nook_control::services::{identity, local_auth};
use nook_control::state::AppState;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use uuid::Uuid;

/// A pending invite for `email` with `role`, returning the plaintext token.
async fn add_invite(bed: &TestBed, tenant: TenantId, email: &str, role: &str) -> String {
    let token = format!("inv-{}", Uuid::now_v7().simple());
    bed.db()
        .exec(
            "INSERT INTO invites (id, tenant_id, email, role, token_hash, status, expires_at)
         VALUES ($1, $2, $3, $4, $5, 'pending', now() + interval '14 days')",
            params![
                Uuid::now_v7(),
                tenant,
                email,
                role,
                nook_auth::hash_token(&token)
            ],
        )
        .await
        .unwrap();
    token
}

/// Call the register handler with a distinct client IP each time so the shared
/// per-IP rate limiter never trips across a test's several calls.
async fn register(state: &AppState, req: RegisterInviteRequest, ip: u8) -> ApiResult<()> {
    let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, ip)), 5555);
    invites::register(
        State(state.clone()),
        ConnectInfo(peer),
        HeaderMap::new(),
        Json(req),
    )
    .await
    .map(|_| ())
}

async fn member_count(bed: &TestBed, tenant: TenantId, user: Uuid) -> i64 {
    bed.db()
        .query_scalar(
            "SELECT count(*) FROM tenant_members
         WHERE tenant_id = $1 AND principal_type = 'user' AND principal_id = $2",
            params![tenant, user],
        )
        .await
        .unwrap()
}

async fn user_by_email(
    bed: &TestBed,
    tenant: TenantId,
    email: &str,
) -> Option<(Uuid, String, bool)> {
    bed.db()
        .query_opt::<(Uuid, Option<String>, bool)>(
            "SELECT id, username, password_hash IS NOT NULL
         FROM users WHERE tenant_id = $1 AND lower(email) = lower($2)",
            params![tenant, email],
        )
        .await
        .unwrap()
        .map(|(id, u, has_pw)| (id, u.unwrap_or_default(), has_pw))
}

#[tokio::test]
async fn register_makes_an_unverified_memberless_user_and_leaves_the_invite_pending() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping invite-registration test — no DATABASE_URL");
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("t").await;
    let email = format!("pm-{}@example.test", Uuid::now_v7().simple());
    let token = add_invite(&bed, tenant, &email, "admin").await;

    register(
        &state,
        RegisterInviteRequest {
            token: token.clone(),
            name: "Pat M".into(),
            username: format!("pat{}", tenant.0.simple()),
            // never client-supplied email — the account must use the invite's.
            password: "correct horse battery".into(),
        },
        11,
    )
    .await
    .expect("registration succeeds");

    let (user, username, has_pw) = user_by_email(&bed, tenant, &email)
        .await
        .expect("the account was created with the invite email");
    assert!(has_pw, "a local account has a password");
    assert!(username.starts_with("pat"), "the chosen username stuck");
    // No membership yet (AC-5 / NG-3): acceptance is separate.
    assert_eq!(member_count(&bed, tenant, user).await, 0, "no membership");
    // Unverified — no verified identity row.
    assert!(
        !identity::email_is_verified(
            &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
            UserId(user)
        )
        .await
        .unwrap(),
        "the account starts unverified"
    );
    // The invite is untouched (AC-5).
    let status: String = bed
        .db()
        .query_scalar(
            "SELECT status FROM invites WHERE token_hash = $1",
            params![nook_auth::hash_token(&token)],
        )
        .await
        .unwrap();
    assert_eq!(
        status, "pending",
        "registration must not consume the invite"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn login_by_username_or_email_works_for_a_memberless_user() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("t").await;
    let email = format!("m-{}@example.test", Uuid::now_v7().simple());
    let username = format!("mem{}", tenant.0.simple());
    let user = local_auth::register_invited(
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        tenant,
        &username,
        &email,
        "Mem",
        "s3cret-passphrase",
        "member",
    )
    .await
    .expect("register_invited");
    assert_eq!(
        member_count(&bed, tenant, user.id.0).await,
        0,
        "no membership"
    );

    // Zero memberships, yet login succeeds — by username AND by email.
    assert!(
        local_auth::login(
            &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
            tenant,
            &username,
            "s3cret-passphrase"
        )
        .await
        .is_ok(),
        "login by username"
    );
    assert!(
        local_auth::login(
            &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
            tenant,
            &email,
            "s3cret-passphrase"
        )
        .await
        .is_ok(),
        "login by email"
    );
    // Case-insensitive, and a wrong password still fails.
    assert!(
        local_auth::login(
            &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
            tenant,
            &email.to_uppercase(),
            "s3cret-passphrase"
        )
        .await
        .is_ok(),
        "identifier match is case-insensitive"
    );
    assert!(
        local_auth::login(
            &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
            tenant,
            &username,
            "wrong"
        )
        .await
        .is_err(),
        "a wrong password is refused"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn identity_context_resolves_the_session_but_tenant_scoped_rejects_it() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("t").await;
    let email = format!("id-{}@example.test", Uuid::now_v7().simple());
    let user = local_auth::register_invited(
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        tenant,
        &format!("id{}", tenant.0.simple()),
        &email,
        "Id",
        "passphrase-here",
        "member",
    )
    .await
    .unwrap();

    // A live session for the memberless user.
    let sid = Uuid::now_v7();
    bed.db()
        .exec(
            "INSERT INTO sessions_auth (id, user_id, tenant_id, expires_at)
         VALUES ($1, $2, $3, now() + interval '1 hour')",
            params![sid, user.id.0, tenant],
        )
        .await
        .unwrap();

    // Identity-only resolution succeeds and reports non-membership...
    let (resolved, is_member) = nook_auth::resolve_session_identity(&bed.db(), sid)
        .await
        .unwrap();
    assert_eq!(resolved.user_id, user.id.0);
    assert!(!is_member, "the invitee is not a member yet");
    // ...while the membership-requiring resolution refuses it (a 403, not a 401).
    assert!(
        matches!(
            nook_auth::resolve_session(&bed.db(), sid).await,
            Err(nook_auth::AuthError::Forbidden)
        ),
        "tenant-scoped resolution must reject a memberless session with Forbidden"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn acceptance_needs_a_verified_email_then_creates_membership_with_the_invited_role() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("t").await;
    let email = format!("acc-{}@example.test", Uuid::now_v7().simple());
    let token = add_invite(&bed, tenant, &email, "admin").await;
    let user = local_auth::register_invited(
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        tenant,
        &format!("acc{}", tenant.0.simple()),
        &email,
        "Acc",
        "a-good-passphrase",
        "admin",
    )
    .await
    .unwrap();

    // Unverified: acceptance is declined and NO membership is created (AC-2).
    let declined = invites::accept_core(
        &nook_control::repo::invites::DbInviteRepository::new(bed.db()),
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        user.id.0,
        tenant,
        &token,
    )
    .await
    .unwrap();
    assert!(!declined.accepted, "unverified acceptance is declined");
    assert_eq!(
        member_count(&bed, tenant, user.id.0).await,
        0,
        "still no membership"
    );

    // Verify, then accept: membership is created carrying the INVITED role.
    identity::mark_local_email_verified(
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        user.id,
        &email,
    )
    .await
    .unwrap();
    let accepted = invites::accept_core(
        &nook_control::repo::invites::DbInviteRepository::new(bed.db()),
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        user.id.0,
        tenant,
        &token,
    )
    .await
    .unwrap();
    assert!(accepted.accepted, "a verified account accepts");
    assert_eq!(accepted.tenant_id, tenant);
    let role: Option<String> = bed
        .db()
        .query_scalar_opt(
            "SELECT role FROM tenant_members
         WHERE tenant_id = $1 AND principal_type = 'user' AND principal_id = $2",
            params![tenant, user.id.0],
        )
        .await
        .unwrap();
    assert_eq!(
        role.as_deref(),
        Some("admin"),
        "membership carries the invited role"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn a_mismatched_or_invalid_invite_creates_no_membership() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("t").await;
    // The invite was addressed to someone else.
    let token = add_invite(&bed, tenant, "someone-else@example.test", "member").await;
    let email = format!("me-{}@example.test", Uuid::now_v7().simple());
    let user = local_auth::register_invited(
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        tenant,
        &format!("me{}", tenant.0.simple()),
        &email,
        "Me",
        "passphrase-1234",
        "member",
    )
    .await
    .unwrap();
    identity::mark_local_email_verified(
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        user.id,
        &email,
    )
    .await
    .unwrap();

    // Verified, but the invite's email is not this account's — declined.
    let declined = invites::accept_core(
        &nook_control::repo::invites::DbInviteRepository::new(bed.db()),
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        user.id.0,
        tenant,
        &token,
    )
    .await
    .unwrap();
    assert!(!declined.accepted, "an email mismatch is declined");
    assert_eq!(member_count(&bed, tenant, user.id.0).await, 0);

    // An unknown token is declined too.
    let bad = invites::accept_core(
        &nook_control::repo::invites::DbInviteRepository::new(bed.db()),
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        user.id.0,
        tenant,
        "nonsense-token",
    )
    .await
    .unwrap();
    assert!(!bad.accepted);
    assert_eq!(member_count(&bed, tenant, user.id.0).await, 0);

    bed.teardown().await;
}

#[tokio::test]
async fn accept_moves_the_memberless_session_onto_the_accepted_tenant() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("t").await;
    let email = format!("sw-{}@example.test", Uuid::now_v7().simple());
    let token = add_invite(&bed, tenant, &email, "member").await;
    let user = local_auth::register_invited(
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        tenant,
        &format!("sw{}", tenant.0.simple()),
        &email,
        "Sw",
        "passphrase-xyz",
        "member",
    )
    .await
    .unwrap();
    identity::mark_local_email_verified(
        &nook_control::repo::identity::DbIdentityRepository::new(bed.db()),
        user.id,
        &email,
    )
    .await
    .unwrap();

    let sid = Uuid::now_v7();
    bed.db()
        .exec(
            "INSERT INTO sessions_auth (id, user_id, tenant_id, expires_at)
         VALUES ($1, $2, $3, now() + interval '1 hour')",
            params![sid, user.id.0, tenant],
        )
        .await
        .unwrap();

    // Accept through the handler as an identity-only, non-member caller.
    let id = IdentityCtx {
        session_id: AuthSessionId(sid),
        user_id: user.id,
        tenant_id: tenant,
        cookie_session: true,
        is_member: false,
    };
    let out = invites::accept(
        State(state.clone()),
        id,
        Json(AcceptInviteRequest { token }),
    )
    .await
    .expect("accept handler")
    .0;
    assert!(out.accepted);

    // The session's active tenant is now the accepted one (AC-2 "becomes active")
    // — here the same single tenant, and the memberless→member transition holds.
    let active: Uuid = bed
        .db()
        .query_scalar(
            "SELECT tenant_id FROM sessions_auth WHERE id = $1",
            params![sid],
        )
        .await
        .unwrap();
    assert_eq!(
        active, tenant.0,
        "the accepted tenant is set active on the session"
    );
    assert_eq!(
        member_count(&bed, tenant, user.id.0).await,
        1,
        "now a member"
    );

    // And that session now passes the membership-requiring resolution.
    assert!(
        nook_auth::resolve_session(&bed.db(), sid).await.is_ok(),
        "after accept, the session is member-scoped"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn duplicate_email_registration_is_indistinguishable_and_harmless() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("t").await;
    let email = format!("dup-{}@example.test", Uuid::now_v7().simple());
    let token = add_invite(&bed, tenant, &email, "member").await;

    let first = RegisterInviteRequest {
        token: token.clone(),
        name: "First".into(),
        username: format!("first{}", tenant.0.simple()),
        password: "passphrase-one".into(),
    };
    register(&state, first, 21)
        .await
        .expect("first registration");
    let (existing_id, existing_username, _) = user_by_email(&bed, tenant, &email)
        .await
        .expect("account exists");

    // A second registration for the same invite email returns the SAME generic
    // Ok (no error that would confirm the address is taken), and does NOT create
    // a second account or change the first.
    let second = RegisterInviteRequest {
        token,
        name: "Second".into(),
        username: format!("second{}", tenant.0.simple()),
        password: "passphrase-two".into(),
    };
    register(&state, second, 22)
        .await
        .expect("duplicate registration returns the generic success shape, not an error");

    let count: i64 = bed
        .db()
        .query_scalar(
            "SELECT count(*) FROM users WHERE tenant_id = $1 AND lower(email) = lower($2)",
            params![tenant, email.clone()],
        )
        .await
        .unwrap();
    assert_eq!(count, 1, "no second account for the same email");
    let (still_id, still_username, _) = user_by_email(&bed, tenant, &email).await.unwrap();
    assert_eq!(still_id, existing_id, "the existing account is unchanged");
    assert_eq!(
        still_username, existing_username,
        "and its username is untouched"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn registration_is_refused_on_an_oidc_tenant() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("t").await;
    bed.db()
        .exec(
            "UPDATE tenants SET auth_mode = 'oidc' WHERE id = $1",
            params![tenant],
        )
        .await
        .unwrap();
    let email = format!("oidc-{}@example.test", Uuid::now_v7().simple());
    let token = add_invite(&bed, tenant, &email, "member").await;

    let refused = register(
        &state,
        RegisterInviteRequest {
            token,
            name: "No".into(),
            username: format!("no{}", tenant.0.simple()),
            password: "passphrase-nope".into(),
        },
        23,
    )
    .await;
    assert!(
        refused.is_err(),
        "local registration is refused on an OIDC tenant"
    );
    assert!(
        user_by_email(&bed, tenant, &email).await.is_none(),
        "nothing was created"
    );

    bed.teardown().await;
}
