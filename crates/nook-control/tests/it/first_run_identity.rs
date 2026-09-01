//! MAIN-397: first run on an instance with no identity provider.
//!
//! `needs_bootstrap` was written as break-glass for an OIDC outage. On a local
//! install it is the only way in that exists — `OIDC_*` is empty by
//! construction — so the properties the login screen relies on there are
//! asserted here rather than inferred from the outage card's tests.
//!
//! The config models a local install exactly: no OIDC, and **no dev-login
//! hatch** (`AUTH_DEV_MODE` is unset in the bundled control plane's
//! environment). That matters — with the hatch on, "the only way in" would be
//! false and every assertion below would pass for the wrong reason.

use axum::extract::{FromRequestParts, State};
use axum::response::IntoResponse;
use axum::Json;
use axum_extra::extract::CookieJar;
use nook_control::auth::{AuthCtx, SESSION_COOKIE};
use nook_control::error::ApiError;
use nook_control::routes::{auth, local_auth};
use nook_control::services::local_auth::{self as local_auth_svc, AuthMode};
use nook_control::AppState;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::{LocalAuthStatus, LocalRegisterRequest, MeResponse};

const PASSWORD: &str = "a genuinely long first-run password";

/// A control plane configured the way the desktop bundle configures it: no
/// identity provider, no dev hatch.
async fn local_install(bed: &TestBed) -> AppState {
    let mut cfg = bed.config();
    cfg.auth_dev_mode = false;
    AppState::new(bed.db(), cfg, None).await
}

async fn status(state: &AppState) -> LocalAuthStatus {
    local_auth::status(State(state.clone()))
        .await
        .expect("local status")
        .0
}

/// Claim the instance, returning the response body and the session the
/// `Set-Cookie` carried — the two halves of "you are now signed in as its
/// owner".
async fn claim(state: &AppState, username: &str) -> Result<(MeResponse, uuid::Uuid), ApiError> {
    let response = local_auth::bootstrap(
        State(state.clone()),
        CookieJar::new(),
        Json(LocalRegisterRequest {
            username: username.into(),
            password: PASSWORD.into(),
            email: None,
            display_name: None,
        }),
    )
    .await?
    .into_response();

    let cookie = response
        .headers()
        .get(axum::http::header::SET_COOKIE)
        .expect("bootstrap must set a session cookie")
        .to_str()
        .expect("cookie is ascii")
        .to_string();
    let session = cookie
        .split(';')
        .next()
        .and_then(|kv| kv.strip_prefix(&format!("{SESSION_COOKIE}=")))
        .and_then(|v| uuid::Uuid::parse_str(v).ok())
        .unwrap_or_else(|| panic!("no {SESSION_COOKIE} in {cookie:?}"));

    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read the body");
    Ok((serde_json::from_slice(&bytes).expect("MeResponse"), session))
}

/// The caller the cookie makes, through the real extractor.
async fn caller(state: &AppState, session: uuid::Uuid) -> AuthCtx {
    let req = axum::http::Request::builder()
        .header(
            axum::http::header::COOKIE,
            format!("{SESSION_COOKIE}={session}"),
        )
        .body(axum::body::Body::empty())
        .unwrap();
    let (mut parts, _) = req.into_parts();
    AuthCtx::from_request_parts(&mut parts, state)
        .await
        .expect("the bootstrap cookie must resolve to a caller")
}

/// AC-1 and AC-3. What a virgin local database offers is account creation, and
/// the absent identity provider is reported as absent — never as degraded,
/// which is the state the login screen renders as an outage warning.
#[tokio::test]
async fn a_virgin_local_instance_offers_account_creation_and_says_nothing_about_oidc() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = local_install(&bed).await;

    let providers = auth::providers(State(state.clone())).await.0;
    assert!(!providers.oidc, "nothing is configured to sign in with");
    assert!(
        !providers.oidc_degraded,
        "absent configuration is not an outage: `oidc_degraded` is what the \
         login screen turns into an 'identity provider unreachable' warning, \
         and there is no provider here to be unreachable (AC-3)"
    );
    assert!(!providers.dev_login, "no dev hatch on a local install");
    assert!(providers.local, "local sign-in is always offered");

    let s = status(&state).await;
    assert!(s.needs_bootstrap, "nobody has claimed this instance (AC-1)");
    assert!(s.available, "and local sign-in is usable here");
    assert_eq!(s.mode, None, "the method has not been chosen yet");
    assert!(
        !s.has_local_credentials,
        "there is no existing credential to fall back to — this is a first \
         run, not the break-glass case the flag was added for"
    );

    // Together these are what the screen needs to present creation rather than
    // an error: something IS available, so it never reaches "no sign-in method
    // is configured — set OIDC_*" (AC-3).
    assert!(
        s.available && !providers.oidc && !providers.oidc_degraded,
        "a local first run must be an offer, not a misconfiguration"
    );

    bed.teardown().await;
}

/// AC-2. The account created here owns the instance through the existing
/// bootstrap claim — the same path an OIDC-less server has always used — and
/// is signed in as itself, not merely written to the database.
#[tokio::test]
async fn the_first_account_owns_the_instance_through_the_existing_claim() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = local_install(&bed).await;

    let (me, session) = claim(&state, "ryan").await.expect("the first claim wins");
    assert_eq!(me.user.role, "owner", "the claimant owns the instance");

    // Signed in, through the real extractor: a bootstrap that wrote a user and
    // handed back a cookie nothing accepts is not a first run that worked.
    let ctx = caller(&state, session).await;
    assert_eq!(ctx.user_id, me.user.id);
    assert_eq!(ctx.tenant_id, me.tenant.id);

    let signed_in = auth::me(State(state.clone()), ctx)
        .await
        .expect("/auth/me must answer for the account just created")
        .0;
    assert_eq!(signed_in.user.id, me.user.id);
    assert!(
        !signed_in.tenants.is_empty(),
        "the owner is a member of the tenant it just claimed — a session with \
         no membership resolves and then 403s everywhere (MAIN-98)"
    );

    // The existing mechanism, not a second one: the tenant is committed to
    // local sign-in, and the deployment operator grant is the one `bootstrap`
    // has always made.
    assert_eq!(
        local_auth_svc::mode_of(state.identity.as_ref(), me.tenant.id)
            .await
            .unwrap(),
        Some(AuthMode::Local)
    );
    let (operators,): (i64,) = bed
        .db()
        .query_one(
            "SELECT count(*) FROM role_bindings
             WHERE scope_type = 'deployment' AND role_key = 'operator'
               AND subject_type = 'user' AND subject_id = $1",
            params![me.user.id],
        )
        .await
        .unwrap();
    assert_eq!(operators, 1, "the claimant is the deployment operator");

    bed.teardown().await;
}

/// AC-4. Existing behaviour, asserted rather than assumed: the window closes on
/// the first account, so a second person reaching the same local instance
/// cannot quietly claim it out from under the owner.
#[tokio::test]
async fn the_bootstrap_window_closes_on_the_first_account() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = local_install(&bed).await;

    claim(&state, "ryan").await.expect("the first claim wins");

    let s = status(&state).await;
    assert!(
        !s.needs_bootstrap,
        "the second visitor is offered sign-in, not the create-owner form"
    );
    assert!(
        s.has_local_credentials,
        "and the credential that now exists is the owner's"
    );

    // And the offer is not what enforces it. A caller who ignores the status
    // and posts anyway is refused by the endpoint itself.
    let err = claim(&state, "someone-else")
        .await
        .expect_err("a second bootstrap must be refused");
    assert!(
        matches!(err, ApiError::ForbiddenMsg(_)),
        "a closed window is 403, got {err:?}"
    );

    let (users,): (i64,) = bed
        .db()
        .query_one("SELECT count(*) FROM users", params![])
        .await
        .unwrap();
    assert_eq!(users, 1, "the refused claim must leave no account behind");

    bed.teardown().await;
}
