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

use nook_types::{DevAccount, MeResponse, TenantId, UserId};

use crate::auth::{
    create_auth_session, removal_cookie, session_cookie, AuthCtx, FlowState, FLOW_COOKIE,
    SESSION_COOKIE,
};
use crate::error::{ApiError, ApiResult};
use crate::events::{self, EventDraft};
use crate::services::identity::{login_identity, IdentityClaims};
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
}

/// GET /api/v1/auth/login — redirect to the configured IdP.
pub async fn login(
    State(state): State<AppState>,
    Query(params): Query<LoginParams>,
    jar: PrivateCookieJar,
) -> ApiResult<impl IntoResponse> {
    let oidc = state
        .oidc
        .clone()
        .ok_or_else(|| ApiError::BadRequest("OIDC is not configured on this instance".into()))?;
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
        .clone()
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
    let existing: Option<(UserId, TenantId)> =
        sqlx::query_as("SELECT id, tenant_id FROM users WHERE lower(email) = lower($1) LIMIT 1")
            .bind(&email)
            .fetch_optional(&state.db)
            .await?;

    if let Some((user_id, tenant_id)) = existing {
        let session_id = create_auth_session(&state, user_id, tenant_id).await?;
        let user: nook_types::User = sqlx::query_as("SELECT * FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&state.db)
            .await?;
        let tenant: nook_types::Tenant = sqlx::query_as("SELECT * FROM tenants WHERE id = $1")
            .bind(tenant_id)
            .fetch_one(&state.db)
            .await?;
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
                user,
                tenant,
                capability: Default::default(),
            }),
        ));
    }

    let identity = IdentityClaims {
        issuer: "nookos-dev".into(),
        subject: email.clone(),
        email: Some(email.clone()),
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
        oidc: state.oidc.is_some(),
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
    let user: nook_types::User = sqlx::query_as("SELECT * FROM users WHERE id = $1")
        .bind(auth.user_id)
        .fetch_one(&state.db)
        .await?;
    let tenant: nook_types::Tenant = sqlx::query_as("SELECT * FROM tenants WHERE id = $1")
        .bind(auth.tenant_id)
        .fetch_one(&state.db)
        .await?;
    Ok(Json(MeResponse {
        capability: capability_of(&state, &auth).await,
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
    let org_id: Option<uuid::Uuid> =
        sqlx::query_as::<_, (Option<uuid::Uuid>,)>("SELECT org_id FROM tenants WHERE id = $1")
            .bind(auth.tenant_id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten()
            .and_then(|(o,)| o);

    nook_types::Capability {
        // "Operator" means holding anything at the deployment scope. Derived
        // rather than stored, so a role rename cannot desynchronise the UI.
        operator: !held.is_empty(),
        deployment: held,
        org_id,
    }
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
    responses((status = 200, body = [DevAccount]), (status = 403)))]
pub async fn dev_accounts(State(state): State<AppState>) -> ApiResult<Json<Vec<DevAccount>>> {
    if !state.cfg.auth_dev_mode || state.cfg.is_production() {
        return Err(ApiError::Forbidden);
    }
    let rows: Vec<DevAccount> = sqlx::query_as(
        "SELECT u.email, u.display_name, t.slug AS tenant_slug,
                COALESCE(
                    (SELECT array_agg(b.role_key ORDER BY b.role_key)
                     FROM role_bindings b
                     WHERE b.subject_id = u.id AND b.scope_type = 'deployment'),
                    '{}'
                ) AS deployment_roles
         FROM users u JOIN tenants t ON t.id = u.tenant_id
         ORDER BY u.created_at",
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

/// POST /api/v1/auth/logout
#[utoipa::path(post, path = "/api/v1/auth/logout", responses((status = 204)))]
pub async fn logout(State(state): State<AppState>, jar: CookieJar) -> ApiResult<impl IntoResponse> {
    if let Some(sid) = jar
        .get(SESSION_COOKIE)
        .and_then(|c| c.value().parse::<uuid::Uuid>().ok())
    {
        sqlx::query("DELETE FROM sessions_auth WHERE id = $1")
            .bind(sid)
            .execute(&state.db)
            .await?;
    }
    Ok((
        jar.add(removal_cookie(SESSION_COOKIE)),
        axum::http::StatusCode::NO_CONTENT,
    ))
}
