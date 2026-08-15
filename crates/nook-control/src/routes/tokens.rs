//! Personal access tokens: how a script, a CLI or an agent acts as *you*.
//!
//! A node token authenticates a machine and the control plane confines it to
//! that machine — which is right, and which is exactly why it can't be the
//! credential for `nook start --node other-box`. This is the other half: a
//! credential that stands in for a person, so tooling can drive the whole
//! fleet the way that person could from a browser.
//!
//! The plaintext is shown once, at creation, and never stored — only its
//! SHA-256. Losing one means minting another, not reading it back.

use axum::extract::{Path, State};
use axum::Json;
use chrono::{Duration, Utc};
use nook_types::*;
use rand::distr::Alphanumeric;
use rand::Rng;

use crate::auth::scopes::{ScopeSet, TokenGrant};
use crate::auth::{AuthCtx, USER_TOKEN_PREFIX};
use crate::error::{ApiError, ApiResult};
use crate::events::{self, EventDraft};
use crate::seed::hash_token;
use crate::state::AppState;

/// Mint a token for the signed-in user, optionally narrowed (MAIN-602).
///
/// Requires a *user* — a node token minting user tokens would be a machine
/// promoting itself to its owner, which is the one thing node confinement
/// exists to prevent.
///
/// ## Why a token can never exceed its minter (AC-3)
///
/// Two rules, and between them there is no way up:
///
/// 1. **A scoped token cannot mint at all.** Minting is not in the closed scope
///    set and never will be (NG-5), so `required_scope` refuses `POST /tokens`
///    for every narrowed credential before this handler runs. The check below is
///    the belt to that braces: it reads the minter's own row **unconditionally**
///    and refuses a scoped minter outright — including the empty-scope request,
///    which asks for a FULL token and is therefore the widest thing anyone could
///    ask for, not the narrowest. So the property survives anyone widening the
///    scoped surface later.
/// 2. **A workspace is resolved in the MINTER'S tenant.** `resolve_by_key` is
///    tenant-scoped, so naming someone else's workspace is a refusal, not a
///    grant — and the resulting narrowing can only ever point at something the
///    minter could already reach.
#[utoipa::path(post, path = "/api/v1/tokens",
    operation_id = "create_user_token",
    request_body = CreateUserTokenRequest,
    responses((status = 200, body = CreateUserTokenResponse), (status = 400), (status = 403)))]
pub async fn create(
    State(state): State<AppState>,
    auth: AuthCtx,
    body: Option<Json<CreateUserTokenRequest>>,
) -> ApiResult<Json<CreateUserTokenResponse>> {
    auth.require_user()?;
    let req = body.map(|Json(r)| r).unwrap_or_default();

    let name = req.name.unwrap_or_default();
    let name = name.trim();
    if name.chars().count() > 80 {
        return Err(ApiError::BadRequest(
            "token name must be 80 characters or fewer".into(),
        ));
    }
    let expires_at = req.expires_in_days.map(|d| Utc::now() + Duration::days(d));

    // An unknown scope is a 400 NAMING it (AC-2). Storing it and ignoring it is
    // the failure this forbids: the caller would believe it holds a permission
    // that nothing grants and nothing checks.
    let mut asked = ScopeSet::default();
    for raw in req.scopes.iter().flatten() {
        let scope = nook_types::TokenScope::parse(raw).ok_or_else(|| {
            ApiError::BadRequest(format!(
                "{raw:?} is not a scope — the set is: {}",
                nook_types::TokenScope::all_names()
            ))
        })?;
        asked.insert(scope);
    }

    let workspace = match req
        .workspace
        .as_deref()
        .map(str::trim)
        .filter(|w| !w.is_empty())
    {
        None => None,
        Some(key) => {
            if asked.is_empty() {
                return Err(ApiError::BadRequest(
                    "a workspace narrowing needs scopes — an unscoped token is already \
                     everything you can do"
                        .into(),
                ));
            }
            Some(
                crate::services::workspace_queries::resolve_by_key(
                    state.workspaces.as_ref(),
                    auth.tenant_id,
                    key,
                )
                .await
                .map_err(|e| ApiError::BadRequest(e.to_string()))?,
            )
        }
    };

    // `mcp` and a workspace cannot both mean something. `scopes::authorize`
    // refuses a narrowed token at `/mcp` — the MCP surface is tenant-wide and its
    // `McpCaller` carries no narrowing — so storing the pair would mint a scope
    // that is silently inert, which is the same failure an unknown scope is
    // refused for (AC-2). Refused here, where the caller can still fix it.
    if workspace.is_some() && asked.contains(nook_types::TokenScope::Mcp) {
        return Err(ApiError::BadRequest(
            "'mcp' cannot be narrowed to a workspace — the MCP surface is tenant-wide. \
             Drop the workspace, or mint two tokens: one tenant-wide for 'mcp' and one \
             narrowed for the rest"
                .into(),
        ));
    }

    // The minter's OWN grant, read BEFORE anything branches on what was asked
    // for. Guarding this on `!asked.is_empty()` is what made the check skip the
    // one case it exists for: an empty scope list mints a FULL token, so a scoped
    // credential asking for nothing would have escalated to everything.
    let minter = minter_grant(&state, &auth).await?;
    if let Some(held) = minter.scopes() {
        if asked.is_empty() {
            return Err(ApiError::ForbiddenMsg(format!(
                "this credential holds {} and cannot mint an unscoped token",
                held.to_stored()
            )));
        }
        if !asked.subset_of(held) {
            return Err(ApiError::ForbiddenMsg(format!(
                "this credential holds {} and cannot mint {}",
                held.to_stored(),
                asked.to_stored()
            )));
        }
        if let Some(narrowed) = minter.workspace() {
            if workspace != Some(narrowed) {
                return Err(ApiError::ForbiddenMsg(format!(
                    "this credential is narrowed to workspace {narrowed} and cannot mint \
                     beyond it"
                )));
            }
        }
    }
    let stored_scopes = (!asked.is_empty()).then(|| asked.to_stored());

    let body_chars: String = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(40)
        .map(char::from)
        .collect();
    let token = format!("{USER_TOKEN_PREFIX}{body_chars}");

    let id = uuid::Uuid::now_v7();
    state
        .identity
        .create_user_token(crate::repo::identity::NewUserToken {
            id,
            tenant: auth.tenant_id,
            user_id: auth.user_id,
            token_hash: hash_token(&token),
            name: name.to_string(),
            expires_at,
            scopes: stored_scopes,
            workspace_id: workspace,
        })
        .await?;

    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("user.token_created")
            .actor("user", auth.user_id.0)
            .payload(serde_json::json!({
                "name": name,
                "scopes": asked.to_vec(),
                "workspace_id": workspace.map(|w| w.to_string()),
            })),
    )
    .await;

    Ok(Json(CreateUserTokenResponse {
        // The only time this value exists anywhere but the caller's hands.
        token,
        id: id.to_string(),
        name: name.to_string(),
        expires_at,
        scopes: asked.to_vec(),
        workspace_id: workspace.map(|w| w.to_string()),
    }))
}

/// What the CREDENTIAL making this request may do — not what its owner may do.
///
/// A cookie session is its person entire. A bearer token carries its own row's
/// id in `session_id`, which is what the narrowing is read by: keyed on the
/// credential, so a tenant switch — which moves `user_id` to the person's
/// sibling row — cannot make a narrowed token look unnarrowed.
async fn minter_grant(state: &AppState, auth: &AuthCtx) -> ApiResult<TokenGrant> {
    if auth.cookie_session {
        return Ok(TokenGrant::Full);
    }
    // A non-cookie user caller IS a `user_tokens` row, so a missing one means it
    // was revoked between authenticating and here. Refused rather than defaulted
    // to unscoped: the default would be the WIDE answer, and a credential that
    // no longer exists must not mint its successor.
    let row = state
        .identity
        .user_token_narrowing(auth.session_id.0)
        .await?
        .ok_or(ApiError::Unauthorized)?;
    Ok(TokenGrant::from_stored(
        row.scopes.as_deref(),
        row.workspace_id,
    ))
}

/// List this user's tokens: what each may do, where it may do it, and when it
/// was last used. Never the tokens themselves — the point of the list is
/// deciding which one to revoke.
#[utoipa::path(get, path = "/api/v1/tokens",
    operation_id = "list_user_tokens",
    responses((status = 200, body = [UserToken])))]
pub async fn list(State(state): State<AppState>, auth: AuthCtx) -> ApiResult<Json<Vec<UserToken>>> {
    auth.require_user()?;
    Ok(Json(state.identity.list_user_tokens(auth.user_id).await?))
}

/// Revoke one. Immediate: the next request carrying it is unauthorized.
#[utoipa::path(delete, path = "/api/v1/tokens/{id}",
    operation_id = "revoke_user_token",
    params(("id" = String, Path,)),
    responses((status = 204), (status = 404)))]
pub async fn revoke(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<String>,
) -> ApiResult<axum::http::StatusCode> {
    auth.require_user()?;
    let uuid: uuid::Uuid = id
        .parse()
        .map_err(|_| ApiError::BadRequest("not a token id".into()))?;

    // Scoped to the caller: one user revoking another's credential is an
    // administrative act, not a self-service one.
    let done = state.identity.revoke_user_token(uuid, auth.user_id).await?;
    if done == 0 {
        return Err(ApiError::NotFound);
    }

    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("user.token_revoked").actor("user", auth.user_id.0),
    )
    .await;
    Ok(axum::http::StatusCode::NO_CONTENT)
}
