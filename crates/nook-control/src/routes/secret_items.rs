//! Named secret items (MAIN-625): set one, list them, remove one, import a
//! `.env`.
//!
//! **No route here returns a value, and none can** — every read answers with
//! [`SecretItem`], which has nowhere to put one (AC-4). Delivery does not go
//! through HTTP at all: the control plane pushes values to a node it has
//! already authenticated, over the same socket a git key already rides.

use axum::extract::{Path, Query, State};
use axum::Json;
use nook_types::*;
use uuid::Uuid;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::services::secret_items;
use crate::state::AppState;

/// Narrow the listing to one scope, or one thing within it.
#[derive(Debug, serde::Deserialize, utoipa::IntoParams)]
pub struct ListSecretItemsQuery {
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub scope_id: Option<Uuid>,
}

/// Every item in the tenant — names, scopes and timestamps.
#[utoipa::path(get, path = "/api/v1/secrets",
    operation_id = "list_secret_items",
    params(ListSecretItemsQuery),
    responses((status = 200, body = [SecretItem])))]
pub async fn list(
    State(state): State<AppState>,
    auth: AuthCtx,
    Query(q): Query<ListSecretItemsQuery>,
) -> ApiResult<Json<Vec<SecretItem>>> {
    // Refused for machines, like the rest of the vault surface: a node token is
    // for doing that node's work, and enumerating the tenant's secrets — even
    // by name — is not it.
    auth.require_user()?;
    let scope = match q.scope.as_deref() {
        None => None,
        Some(raw) => Some(
            SecretScope::parse(raw)
                .ok_or_else(|| ApiError::BadRequest(format!("unknown scope '{raw}'")))?,
        ),
    };
    let items = state
        .secret_items
        .list(auth.tenant_id)
        .await?
        .iter()
        .map(|row| row.summary())
        .filter(|item| scope.is_none_or(|s| s == item.scope))
        .filter(|item| q.scope_id.is_none_or(|id| id == item.scope_id))
        .collect();
    Ok(Json(items))
}

/// Create an item, or replace the value of one that exists.
#[utoipa::path(put, path = "/api/v1/secrets",
    operation_id = "set_secret_item",
    request_body = SetSecretItemRequest,
    responses((status = 200, body = SecretItem), (status = 400), (status = 404)))]
pub async fn set(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<SetSecretItemRequest>,
) -> ApiResult<Json<SecretItem>> {
    auth.require_user()?;
    let scope_id =
        secret_items::resolve_scope_id(&state, auth.tenant_id, req.scope, req.scope_id).await?;
    Ok(Json(
        secret_items::set_item(
            &state,
            auth.tenant_id,
            auth.user_id,
            req.scope,
            scope_id,
            &req.name,
            &req.value,
        )
        .await?,
    ))
}

/// Remove an item. 404 when there was none, rather than a silent success —
/// "the secret is gone" and "you named the wrong one" must not read alike.
#[utoipa::path(delete, path = "/api/v1/secrets/{scope}/{scope_id}/{name}",
    operation_id = "delete_secret_item",
    params(("scope" = String, Path,), ("scope_id" = String, Path,), ("name" = String, Path,)),
    responses((status = 204), (status = 404)))]
pub async fn delete(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path((scope, scope_id, name)): Path<(String, Uuid, String)>,
) -> ApiResult<axum::http::StatusCode> {
    auth.require_user()?;
    let scope = SecretScope::parse(&scope)
        .ok_or_else(|| ApiError::BadRequest(format!("unknown scope '{scope}'")))?;
    if !state
        .secret_items
        .delete(auth.tenant_id, scope, scope_id, &name)
        .await?
    {
        return Err(ApiError::NotFound);
    }
    secret_items::record(
        &state,
        auth.tenant_id,
        auth.user_id,
        "secret.deleted",
        scope,
        scope_id,
        &name,
    )
    .await;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Import a `.env` body as one item per assignment (AC-8).
///
/// Partial by design: the assignments that parsed are stored and the lines that
/// did not are reported. Refusing the whole file for one bad line would make a
/// fifty-line import an exercise in binary search, and the report is what stops
/// a skipped line being silent.
#[utoipa::path(post, path = "/api/v1/secrets/import",
    operation_id = "import_secret_items",
    request_body = ImportSecretItemsRequest,
    responses((status = 200, body = ImportSecretItemsResult), (status = 400), (status = 404)))]
pub async fn import(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<ImportSecretItemsRequest>,
) -> ApiResult<Json<ImportSecretItemsResult>> {
    auth.require_user()?;
    let scope_id =
        secret_items::resolve_scope_id(&state, auth.tenant_id, req.scope, req.scope_id).await?;
    let parsed = secret_items::parse_dotenv(&req.content);
    let mut result = ImportSecretItemsResult {
        imported: Vec::with_capacity(parsed.items.len()),
        problems: parsed.problems,
    };
    for (name, value) in parsed.items {
        match secret_items::set_item(
            &state,
            auth.tenant_id,
            auth.user_id,
            req.scope,
            scope_id,
            &name,
            &value,
        )
        .await
        {
            Ok(item) => result.imported.push(item.name),
            // A store that failed is a problem like any other, and it is
            // reported by name so the operator knows which one to retry.
            Err(e) => result.problems.push(SecretImportProblem {
                line: None,
                reason: format!("could not store '{name}': {e}"),
            }),
        }
    }
    Ok(Json(result))
}
