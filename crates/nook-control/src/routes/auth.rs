use axum::extract::{Query, State};
use axum::response::{IntoResponse, Redirect};
use axum::Json;
use axum_extra::extract::cookie::{Cookie, CookieJar, PrivateCookieJar, SameSite};
use openidconnect::core::{CoreAuthenticationFlow, CoreClient};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, Nonce, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
};
use serde::Deserialize;
use utoipa::ToSchema;

use nook_db::{CiMatch, Postgres};
use nook_types::{DevAccountsResponse, MeResponse, PurgeTestTenantsResponse};

use crate::auth::{
    create_auth_session, removal_cookie, session_cookie, AuthCtx, FlowState, FLOW_COOKIE,
    SESSION_COOKIE,
};
use crate::error::{ApiError, ApiResult};
use crate::events::{self, EventDraft};
use crate::services::identity::{
    cached_memberships_for, invalidate_person_tenants, login_identity, member_user_in_tenant,
    memberships_for, IdentityClaims, DEV_ISSUER,
};
use crate::state::AppState;

/// Build the OIDC client from cached discovery metadata. Constructed per
/// request — pure, no network. Generic over any standards-compliant IdP.
macro_rules! oidc_client {
    ($state:expr, $oidc:expr) => {
        CoreClient::from_provider_metadata(
            $oidc.metadata.clone(),
            ClientId::new(
                $state
                    .cfg
                    .oidc_client_id
                    .clone()
                    .ok_or_else(|| ApiError::BadRequest("OIDC not configured".into()))?,
            ),
            $state.cfg.oidc_client_secret.clone().map(ClientSecret::new),
        )
        .set_redirect_uri(
            RedirectUrl::new(
                $state
                    .cfg
                    .oidc_redirect_url
                    .clone()
                    .ok_or_else(|| ApiError::BadRequest("OIDC not configured".into()))?,
            )
            .map_err(|e| ApiError::Internal(e.into()))?,
        )
        // client_secret_post: credentials in the token-request body. More
        // IdPs accept this than HTTP Basic, and it stays provider-generic.
        .set_auth_type(openidconnect::AuthType::RequestBody)
    };
}

#[derive(Deserialize)]
pub struct LoginParams {
    /// Path to return to after login; must be app-relative.
    next: Option<String>,
    /// An OIDC `prompt` hint to forward to the IdP. WHITELISTED: only the exact
    /// value `create` is passed through (so the invite landing can ask an IdP to
    /// open its sign-up screen); anything else is dropped, never reflected.
    prompt: Option<String>,
    /// An OIDC `login_hint` (an email to pre-fill at the IdP). Forwarded only
    /// when it looks like a plausible address; otherwise dropped, so this cannot
    /// become an open reflector for arbitrary strings.
    login_hint: Option<String>,
}

/// A `login_hint` we are willing to forward: a plausible email — one `@`, a dot
/// after it, no whitespace, and non-empty on both sides. Deliberately lax (real
/// validation is the IdP's job) but enough to refuse junk.
fn plausible_email(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.chars().any(char::is_whitespace) {
        return false;
    }
    match s.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
        }
        None => false,
    }
}

/// GET /api/v1/auth/login — redirect to the configured IdP.
pub async fn login(
    State(state): State<AppState>,
    Query(params): Query<LoginParams>,
    jar: PrivateCookieJar,
) -> ApiResult<impl IntoResponse> {
    // Configured-and-usable → go. Configured-but-degraded → attempt discovery
    // on the spot so a just-recovered IdP works in this very request, ahead of
    // the background retry; if it is still unreachable, an explicit 503 — never
    // a silent fall-through to another sign-in method (MAIN-169 AC-2).
    let oidc = match state.oidc.current() {
        Some(c) => c,
        None if state.oidc.configured() => match state.oidc.discover_now().await {
            Ok(Some(c)) => c,
            _ => {
                return Err(ApiError::ServiceUnavailable(
                    "the identity provider is unreachable — the server is retrying, \
                     please try again in a moment"
                        .into(),
                ))
            }
        },
        None => {
            return Err(ApiError::BadRequest(
                "OIDC is not configured on this instance".into(),
            ))
        }
    };
    let client = oidc_client!(state, oidc);

    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    let mut auth_req = client.authorize_url(
        CoreAuthenticationFlow::AuthorizationCode,
        CsrfToken::new_random,
        Nonce::new_random,
    );
    for scope in state.cfg.oidc_scopes.split_whitespace() {
        if scope != "openid" {
            auth_req = auth_req.add_scope(Scope::new(scope.to_string()));
        }
    }

    // Forward `prompt=create` so the invite landing's "Create account" action
    // lands the invitee on the IdP's sign-up screen. Whitelisted to that one
    // value; any other prompt is ignored. IdPs that don't understand it fall
    // back to their normal screen, so the flow still works.
    if params.prompt.as_deref() == Some("create") {
        auth_req = auth_req.add_extra_param("prompt", "create");
    }
    // Forward a pre-fill email hint, but only a plausible one — never an
    // arbitrary string reflected to the IdP.
    if let Some(hint) = params.login_hint.as_deref().map(str::trim) {
        if plausible_email(hint) {
            auth_req = auth_req.add_extra_param("login_hint", hint);
        }
    }

    let (auth_url, csrf, nonce) = auth_req.set_pkce_challenge(pkce_challenge).url();

    let next = params
        .next
        .filter(|n| n.starts_with('/') && !n.starts_with("//"))
        .unwrap_or_else(|| "/".to_string());
    let flow = FlowState {
        csrf: csrf.secret().clone(),
        nonce: nonce.secret().clone(),
        pkce_verifier: pkce_verifier.secret().clone(),
        next,
    };
    let flow_cookie = Cookie::build((
        FLOW_COOKIE,
        serde_json::to_string(&flow).map_err(|e| ApiError::Internal(e.into()))?,
    ))
    .path("/")
    .http_only(true)
    .same_site(SameSite::Lax)
    .secure(state.cfg.public_base_url.starts_with("https"))
    .max_age(cookie::time::Duration::minutes(5))
    .build();

    Ok((jar.add(flow_cookie), Redirect::to(auth_url.as_str())))
}

#[derive(Deserialize)]
pub struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// GET /api/v1/auth/callback — code exchange, identity upsert, session cookie.
pub async fn callback(
    State(state): State<AppState>,
    Query(params): Query<CallbackParams>,
    jar: CookieJar,
    private_jar: PrivateCookieJar,
) -> ApiResult<impl IntoResponse> {
    if let Some(err) = params.error {
        return Err(ApiError::BadRequest(format!(
            "IdP returned error: {} {}",
            err,
            params.error_description.unwrap_or_default()
        )));
    }
    let code = params
        .code
        .ok_or_else(|| ApiError::BadRequest("missing code".into()))?;
    let returned_state = params
        .state
        .ok_or_else(|| ApiError::BadRequest("missing state".into()))?;

    let flow: FlowState = private_jar
        .get(FLOW_COOKIE)
        .and_then(|c| serde_json::from_str(c.value()).ok())
        .ok_or_else(|| ApiError::BadRequest("login flow expired — try again".into()))?;
    if flow.csrf != returned_state {
        return Err(ApiError::BadRequest("state mismatch".into()));
    }

    let oidc = state
        .oidc
        .current()
        .ok_or_else(|| ApiError::BadRequest("OIDC is not configured on this instance".into()))?;
    let client = oidc_client!(state, oidc);

    let token_response = client
        .exchange_code(AuthorizationCode::new(code))
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("token endpoint missing: {e}")))?
        .set_pkce_verifier(PkceCodeVerifier::new(flow.pkce_verifier.clone()))
        .request_async(&oidc.http)
        .await
        .map_err(|e| ApiError::BadRequest(format!("code exchange failed: {e}")))?;

    let id_token = token_response
        .id_token()
        .ok_or_else(|| ApiError::BadRequest("IdP returned no id_token".into()))?;
    let expected_nonce = Nonce::new(flow.nonce.clone());
    let claims = id_token
        .claims(&client.id_token_verifier(), |nonce: Option<&Nonce>| {
            match nonce {
                Some(n) if n.secret() == expected_nonce.secret() => Ok(()),
                Some(_) => Err("nonce mismatch".to_string()),
                // Some IdPs omit the nonce claim (spec deviation). The flow
                // is still bound by state + PKCE, so accept with a warning
                // rather than locking those providers out.
                None => {
                    tracing::warn!("IdP omitted nonce claim from id_token (spec deviation)");
                    Ok(())
                }
            }
        })
        .map_err(|e| ApiError::BadRequest(format!("id_token validation failed: {e}")))?;

    let identity = IdentityClaims {
        issuer: claims.issuer().to_string(),
        subject: claims.subject().to_string(),
        email: claims.email().map(|e| e.to_string()),
        // Only the IdP's assertion verifies the address; absent/false is unverified.
        email_verified: claims.email_verified().unwrap_or(false),
        display_name: claims
            .name()
            .and_then(|n| n.get(None))
            .map(|n| n.to_string()),
        avatar_url: claims
            .picture()
            .and_then(|p| p.get(None))
            .map(|p| p.to_string()),
        raw_claims: serde_json::to_value(claims).unwrap_or_default(),
    };

    let (user, tenant) = login_identity(&state, identity).await?;
    let session_id = create_auth_session(&state, user.id, tenant.id).await?;

    events::record(
        &state,
        tenant.id,
        EventDraft::new("user.login")
            .actor("user", user.id.0)
            .payload(serde_json::json!({ "email": user.email, "via": "oidc" })),
    )
    .await;

    let dest = format!(
        "{}{}",
        state.cfg.web_origin.trim_end_matches('/'),
        flow.next
    );
    Ok((
        private_jar.remove(Cookie::from(FLOW_COOKIE)),
        jar.add(session_cookie(&state, session_id)),
        Redirect::to(&dest),
    ))
}

#[derive(Deserialize, ToSchema)]
pub struct DevLoginRequest {
    pub email: Option<String>,
    pub display_name: Option<String>,
}

/// POST /api/v1/auth/dev-login — dev/CI escape hatch. Compiled in, but
/// hard-refused unless AUTH_DEV_MODE=true (and never in production).
#[utoipa::path(
    post,
    path = "/api/v1/auth/dev-login",
    request_body = DevLoginRequest,
    responses((status = 200, body = MeResponse), (status = 403, description = "dev mode disabled"))
)]
pub async fn dev_login(
    State(state): State<AppState>,
    jar: CookieJar,
    Json(req): Json<DevLoginRequest>,
) -> ApiResult<impl IntoResponse> {
    if !state.cfg.auth_dev_mode || state.cfg.is_production() {
        return Err(ApiError::Forbidden);
    }
    let email = req.email.unwrap_or_else(|| "dev@nookos.local".into());

    // Become an EXISTING user when the email already belongs to one.
    //
    // Without this, "sign in as ryan@localhost" ran the new-identity path and
    // produced a SECOND ryan in a fresh personal tenant — which is correct
    // isolation (a dev identity is not a local account) and useless for the one
    // thing this endpoint is for. Testing an authorization model means being
    // the people who already exist; if switching to them silently creates
    // someone new, nobody tests roles at all.
    //
    // Dev-only, behind the same gate as the rest of this handler: matching by
    // email in production would let anybody who can reach an IdP become anybody
    // who shares their address.
    let existing = state.identity.user_and_tenant_by_email(&email).await?;

    if let Some((user_id, tenant_id)) = existing {
        let user = state
            .identity
            .get_user(user_id)
            .await?
            .ok_or(ApiError::NotFound)?;

        // Land an ACTUALLY-usable session. `resolve_session` 403s a session whose
        // user has no `tenant_members` grant on its tenant (a memberless session,
        // MAIN-98) — and legacy accounts seeded before the membership model, or by
        // the old shared-DB test path, have none. Without this, "click a name"
        // sets a cookie and then bounces off /auth/me with 403 instead of signing
        // you in (MAIN-221 AC-1). Dev-only (this whole handler is gated) and
        // idempotent; it grants the user's own role, never elevating.
        state
            .identity
            .grant_membership(tenant_id, user_id, &user.role)
            .await?;

        let session_id = create_auth_session(&state, user_id, tenant_id).await?;
        let tenant = state
            .identity
            .get_tenant(tenant_id)
            .await?
            .ok_or(ApiError::NotFound)?;
        events::record(
            &state,
            tenant.id,
            EventDraft::new("user.login")
                .actor("user", user.id.0)
                .payload(serde_json::json!({ "email": user.email, "via": "dev" })),
        )
        .await;
        return Ok((
            jar.add(session_cookie(&state, session_id)),
            Json(MeResponse {
                tenants: memberships_for(state.identity.as_ref(), user.id, tenant.id).await?,
                person_id: crate::auth::person_id_of(&state, user.id).await?,
                user,
                tenant,
                capability: Default::default(),
            }),
        ));
    }

    let identity = IdentityClaims {
        issuer: DEV_ISSUER.into(),
        subject: email.clone(),
        email: Some(email.clone()),
        // The dev login is not a real IdP asserting anything — never verified.
        email_verified: false,
        display_name: req.display_name.or_else(|| Some("Dev User".into())),
        avatar_url: None,
        raw_claims: serde_json::json!({ "dev": true }),
    };
    let (user, tenant) = login_identity(&state, identity).await?;
    let session_id = create_auth_session(&state, user.id, tenant.id).await?;

    events::record(
        &state,
        tenant.id,
        EventDraft::new("user.login")
            .actor("user", user.id.0)
            .payload(serde_json::json!({ "email": user.email, "via": "dev" })),
    )
    .await;

    Ok((
        jar.add(session_cookie(&state, session_id)),
        Json(MeResponse {
            tenants: memberships_for(state.identity.as_ref(), user.id, tenant.id).await?,
            person_id: crate::auth::person_id_of(&state, user.id).await?,
            user,
            tenant,
            capability: Default::default(),
        }),
    ))
}

/// GET /api/v1/auth/providers — unauthenticated: what sign-in methods exist,
/// so the login screen never offers a dead button.
#[utoipa::path(
    get,
    path = "/api/v1/auth/providers",
    operation_id = "auth_providers",
    responses((status = 200, body = nook_types::AuthProviders))
)]
pub async fn providers(State(state): State<AppState>) -> Json<nook_types::AuthProviders> {
    Json(nook_types::AuthProviders {
        // Three observable states (MAIN-169 AC-3): not configured (`oidc:false,
        // oidc_degraded:false`), configured-and-usable (`oidc:true`), and
        // configured-but-unreachable (`oidc:false, oidc_degraded:true`).
        oidc: state.oidc.current().is_some(),
        oidc_degraded: state.oidc.degraded(),
        dev_login: state.cfg.auth_dev_mode && !state.cfg.is_production(),
        // Always offered: an instance with no identity provider still needs a
        // way in, and `/auth/local/status` says whether it is usable here.
        local: true,
        oidc_issuer: state.cfg.oidc_issuer_url.clone(),
        device_authorization_endpoint: state.cfg.oidc_device_authorization_endpoint.clone(),
        device_client_id: state.cfg.oidc_device_client_id.clone(),
    })
}

/// GET /api/v1/auth/me
#[utoipa::path(
    get,
    path = "/api/v1/auth/me",
    responses((status = 200, body = MeResponse), (status = 401, description = "not signed in"))
)]
pub async fn me(State(state): State<AppState>, auth: AuthCtx) -> ApiResult<Json<MeResponse>> {
    let user = state
        .identity
        .get_user(auth.user_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let tenant = state
        .identity
        .get_tenant(auth.tenant_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(MeResponse {
        capability: capability_of(&state, &auth).await,
        tenants: cached_memberships_for(
            &*state.cache,
            state.identity.as_ref(),
            auth.user_id,
            auth.tenant_id,
        )
        .await?,
        person_id: crate::auth::person_id_of(&state, auth.user_id).await?,
        user,
        tenant,
    }))
}

/// GET /api/v1/me/tenants — every tenant the signed-in person belongs to, with
/// the active one marked. The switcher reads it; `me` also carries it inline so
/// the first render needs no second request.
#[utoipa::path(get, path = "/api/v1/me/tenants",
    operation_id = "my_tenants",
    responses((status = 200, body = [nook_types::TenantMembership]), (status = 401)))]
pub async fn my_tenants(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<Vec<nook_types::TenantMembership>>> {
    auth.require_user()?;
    Ok(Json(
        cached_memberships_for(
            &*state.cache,
            state.identity.as_ref(),
            auth.user_id,
            auth.tenant_id,
        )
        .await?,
    ))
}

/// POST /api/v1/me/tenant — switch the browser session's active tenant.
///
/// Membership is enforced here (403 for a tenant the person does not belong
/// to), and the switch is a single UPDATE of `sessions_auth`: because
/// `AuthCtx` resolves BOTH `user_id` and `tenant_id` from that row on every
/// request, moving the row re-scopes every tenant-scoped surface at once. The
/// per-tenant `user_id` changes too, so attribution and role follow the tenant.
///
/// Browser sessions only (NG-5): a `nook_user_` token has no `sessions_auth`
/// row, so the UPDATE affects nothing and the endpoint says so rather than
/// pretending to switch a credential that stays bound to its tenant.
#[utoipa::path(post, path = "/api/v1/me/tenant",
    operation_id = "switch_tenant",
    request_body = nook_types::SwitchTenantRequest,
    responses((status = 200, body = MeResponse), (status = 403), (status = 400)))]
pub async fn switch_tenant(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<nook_types::SwitchTenantRequest>,
) -> ApiResult<Json<MeResponse>> {
    auth.require_user()?;

    // A user token (also `Principal::User`) has no `sessions_auth` row to move,
    // so it structurally cannot switch — refuse it up front (AC-3), rather than
    // inferring "token" from a zero-row UPDATE, which cannot tell it apart from
    // a browser session whose row vanished mid-request.
    if !auth.cookie_session {
        return Err(ApiError::BadRequest(
            "tenant switching is available on a browser session only — a user \
             token stays bound to the tenant it was minted for"
                .into(),
        ));
    }
    let source_tenant = auth.tenant_id;

    // Membership is checked on EVERY switch, including re-selecting the tenant
    // you are already in. The earlier shortcut (`req.tenant_id == auth.tenant_id
    // => auth.user_id`) skipped the check, so switching into your current tenant
    // returned 200 even after your grant there was revoked — AC-7 requires the
    // switch endpoint to 403 for a membership that is gone, current tenant or
    // not. One lookup on the hot-but-rare switch path is a fine price.
    let target_user = member_user_in_tenant(state.identity.as_ref(), auth.user_id, req.tenant_id)
        .await?
        .ok_or_else(|| ApiError::ForbiddenMsg("you are not a member of that tenant".into()))?;

    let res = state
        .identity
        .switch_session(auth.session_id, target_user, req.tenant_id)
        .await?;
    if res == 0 {
        // The caller IS a cookie session (checked above), so a zero-row update
        // means its `sessions_auth` row is gone — a concurrent logout or expiry
        // between authentication and here. The session is gone, not a token (AC-3).
        return Err(ApiError::Unauthorized);
    }

    // The active tenant just changed, so the `current` marker on this person's
    // cached list is now wrong for both the row they left and the one they
    // moved to — drop it so `/auth/me` reflects the switch immediately (AC-4).
    invalidate_person_tenants(&*state.cache, state.identity.as_ref(), target_user).await;

    // Arrival, recorded in the destination tenant. The payload names BOTH
    // tenants and its direction, so a consumer reads the whole switch from this
    // one row without inferring anything from which key happens to be present
    // (MAIN-46 AC-1). Still the single `user.tenant_switched` kind (NG-1).
    events::record(
        &state,
        req.tenant_id,
        EventDraft::new("user.tenant_switched")
            .actor("user", target_user.0)
            .payload(serde_json::json!({
                "direction": "in",
                "from_tenant": source_tenant,
                "to_tenant": req.tenant_id,
            })),
    )
    .await;

    // Departure, recorded in the tenant left behind, so a switch is auditable
    // from BOTH sides (AC-2) — same self-describing payload, direction "out",
    // actor = the source-tenant user. Skipped when re-selecting the current
    // tenant (no crossing to record).
    if source_tenant != req.tenant_id {
        events::record(
            &state,
            source_tenant,
            EventDraft::new("user.tenant_switched")
                .actor("user", auth.user_id.0)
                .payload(serde_json::json!({
                    "direction": "out",
                    "from_tenant": source_tenant,
                    "to_tenant": req.tenant_id,
                })),
        )
        .await;
    }

    // Rebuild the caller against the tenant they just moved to, so the client
    // updates in one round trip.
    let switched = AuthCtx {
        user_id: target_user,
        tenant_id: req.tenant_id,
        ..auth
    };
    let user = state
        .identity
        .get_user(switched.user_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let tenant = state
        .identity
        .get_tenant(switched.tenant_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(MeResponse {
        capability: capability_of(&state, &switched).await,
        tenants: cached_memberships_for(
            &*state.cache,
            state.identity.as_ref(),
            switched.user_id,
            switched.tenant_id,
        )
        .await?,
        person_id: crate::auth::person_id_of(&state, switched.user_id).await?,
        user,
        tenant,
    }))
}

/// What this caller holds, for the UI.
///
/// Advisory only — every route re-checks. This exists so the operator section
/// can be ABSENT rather than present-and-forbidden: a greyed-out door still
/// tells you there is a room.
async fn capability_of(state: &AppState, auth: &AuthCtx) -> nook_types::Capability {
    use crate::auth::perm::{Permission, Scope};

    let mut held = Vec::new();
    for p in Permission::ALL {
        if auth.can(state, p, Scope::Deployment).await {
            held.push(p.key().to_string());
        }
    }
    let org_id = state.identity.org_of(auth.tenant_id).await.ok().flatten();

    nook_types::Capability {
        // "Operator" means holding anything at the deployment scope. Derived
        // rather than stored, so a role rename cannot desynchronise the UI.
        operator: !held.is_empty(),
        deployment: held,
        org_id,
    }
}

/// The account count returned before a search must be applied — beyond this the
/// picker shows a "N more — refine" hint rather than an unbounded, stuck list.
const DEV_ACCOUNTS_CAP: i64 = 50;

#[derive(Deserialize, utoipa::IntoParams)]
pub struct DevAccountsQuery {
    /// Optional case-insensitive substring over email / display name / tenant
    /// slug. Absent or blank returns the newest accounts up to the cap.
    pub q: Option<String>,
}

/// GET /api/v1/auth/dev-accounts — who you can sign in as, in dev mode.
///
/// Dev only, and unauthenticated by necessity: it is what the login screen
/// reads before anybody is signed in. It returns emails and display names of
/// accounts that already exist — no credentials, no tokens — and it is refused
/// outright unless `AUTH_DEV_MODE` is on and this is not production, which is
/// the same gate `dev_login` itself uses.
///
/// It exists because testing authorization requires BEING different people, and
/// a model you cannot switch between users to exercise is a model nobody
/// exercises.
#[utoipa::path(get, path = "/api/v1/auth/dev-accounts",
    operation_id = "dev_accounts",
    params(DevAccountsQuery),
    responses((status = 200, body = DevAccountsResponse), (status = 403)))]
pub async fn dev_accounts(
    State(state): State<AppState>,
    Query(query): Query<DevAccountsQuery>,
) -> ApiResult<Json<DevAccountsResponse>> {
    if !state.cfg.auth_dev_mode || state.cfg.is_production() {
        return Err(ApiError::Forbidden);
    }

    // A blank search is no search — bind SQL NULL so the filter short-circuits
    // to "match everything" and the same query serves both cases.
    let pattern: Option<String> = query
        .q
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| format!("%{s}%"));

    // `$1` matches any of the three columns; a NULL `$1` matches all rows.
    let _filter = format!(
        "($1::text IS NULL OR {} OR {} OR {})",
        Postgres.ci_match("u.email", "$1"),
        Postgres.ci_match("u.display_name", "$1"),
        Postgres.ci_match("t.slug", "$1"),
    );

    // One call: the page and its total share a filter whose construction
    // belongs with the SQL, not here.
    let (accounts, total) = state
        .identity
        .dev_accounts_page(pattern, DEV_ACCOUNTS_CAP)
        .await?;

    Ok(Json(DevAccountsResponse { accounts, total }))
}

/// POST /api/v1/auth/purge-test-tenants — dev-only cleanup of the legacy
/// pre-testkit pollution: tenants named/slugged `test-<uuid>` from the old
/// shared-DB `test_config` path (MAIN-221 AC-3). Cascades (every `tenant_id` FK
/// is `ON DELETE CASCADE`), is idempotent (a second run deletes 0), and is
/// refused unless dev mode is on and this is not production — the same gate the
/// rest of the dev hatch uses. Keyed strictly to the `test-%` marker (NG-3).
#[utoipa::path(post, path = "/api/v1/auth/purge-test-tenants",
    operation_id = "purge_test_tenants",
    responses((status = 200, body = PurgeTestTenantsResponse), (status = 403)))]
pub async fn purge_test_tenants(
    State(state): State<AppState>,
) -> ApiResult<Json<PurgeTestTenantsResponse>> {
    if !state.cfg.auth_dev_mode || state.cfg.is_production() {
        return Err(ApiError::Forbidden);
    }
    let deleted = state.identity.purge_test_tenants().await?;
    Ok(Json(PurgeTestTenantsResponse {
        deleted: deleted as i64,
    }))
}

/// POST /api/v1/auth/logout
#[utoipa::path(post, path = "/api/v1/auth/logout", responses((status = 204)))]
pub async fn logout(State(state): State<AppState>, jar: CookieJar) -> ApiResult<impl IntoResponse> {
    if let Some(sid) = jar
        .get(SESSION_COOKIE)
        .and_then(|c| c.value().parse::<uuid::Uuid>().ok())
    {
        state.identity.delete_auth_session(sid).await?;
    }
    Ok((
        jar.add(removal_cookie(SESSION_COOKIE)),
        axum::http::StatusCode::NO_CONTENT,
    ))
}

#[cfg(test)]
mod tests {
    /// Switching tenant must be gated on membership and confined to browser
    /// sessions. Asserted at the source because both failures are silent in a
    /// happy-path test: a missing membership check would let anyone name any
    /// tenant id, and a missing browser-session guard would appear to switch a
    /// `nook_user_` token that actually stays bound to its tenant (NG-5).
    fn switch_handler() -> &'static str {
        include_str!("auth.rs")
            .split("pub async fn switch_tenant(")
            .nth(1)
            .expect("switch_tenant handler")
            .split("\npub async fn ")
            .next()
            .expect("handler body")
    }

    #[test]
    fn login_hint_is_only_forwarded_for_a_plausible_email() {
        use super::plausible_email;
        assert!(plausible_email("ryan@example.com"));
        assert!(plausible_email("a.b+c@sub.example.co"));
        // Junk and reflector bait are refused.
        assert!(!plausible_email("not-an-email"));
        assert!(!plausible_email("no-domain@"));
        assert!(!plausible_email("@no-local.com"));
        assert!(!plausible_email("spaces in@it.com"));
        assert!(!plausible_email("trailing@dot."));
        assert!(!plausible_email("nodot@localhost"));
        assert!(!plausible_email(""));
    }

    /// The login handler must whitelist `prompt` to exactly `create` and only
    /// forward a validated `login_hint` — asserted at the source, because a
    /// reflected arbitrary prompt/hint is invisible in a happy-path test.
    #[test]
    fn login_forwards_only_whitelisted_prompt_and_validated_hint() {
        let src = include_str!("auth.rs");
        let body = src
            .split("pub async fn login(")
            .nth(1)
            .expect("login handler")
            .split("\npub async fn ")
            .next()
            .expect("handler body");
        assert!(
            body.contains("Some(\"create\")"),
            "prompt must be whitelisted to exactly \"create\""
        );
        assert!(
            body.contains("plausible_email(hint)"),
            "login_hint must be validated before it is forwarded"
        );
        assert!(
            body.contains("add_extra_param"),
            "the params must reach the authorize request"
        );
    }

    #[test]
    fn switch_refuses_a_non_member() {
        let body = switch_handler();
        assert!(
            body.contains("member_user_in_tenant"),
            "switch must resolve the target through the membership guard"
        );
        assert!(
            body.contains("ForbiddenMsg"),
            "a tenant the caller does not belong to must 403, not switch"
        );
    }

    #[test]
    fn switch_is_browser_session_only() {
        let body = switch_handler();
        // Looked for the literal `UPDATE sessions_auth` until MAIN-247 moved the
        // statement behind `IdentityRepository`. Same intent — switching must be
        // a move of the browser session's active tenant, not a token reissue —
        // so it now names the call that does it.
        assert!(
            body.contains("switch_session"),
            "switching is a move of the browser session's active tenant"
        );
        // The guard is on the affected-row count of the UPDATE: `exec` returns
        // rows-affected (MAIN-205's dispatch API), so a zero-row update — a
        // credential with no `sessions_auth` row (a user token) — is caught by
        // `res == 0` rather than being a silent no-op.
        assert!(
            body.contains("res == 0"),
            "a credential with no sessions_auth row (a user token) must be told \
             switching is browser-only, not silently no-op"
        );
    }
}
