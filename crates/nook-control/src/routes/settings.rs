use axum::extract::{Path, State};
use axum::Json;
use nook_db::{params, Db};
use nook_types::*;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

#[utoipa::path(get, path = "/api/v1/settings",
    operation_id = "list_settings", responses((status = 200, body = [Setting])))]
pub async fn list(State(state): State<AppState>, auth: AuthCtx) -> ApiResult<Json<Vec<Setting>>> {
    // Tenant-scoped settings plus the caller's user-scoped ones.
    let settings: Vec<Setting> = state
        .db
        .query_all(
            "SELECT * FROM settings
         WHERE tenant_id = $1 AND (scope = 'tenant' OR (scope = 'user' AND user_id = $2))
         ORDER BY key",
            params![auth.tenant_id, auth.user_id],
        )
        .await?;
    Ok(Json(settings))
}

#[utoipa::path(put, path = "/api/v1/settings/{key}",
    operation_id = "put_setting",
    params(("key" = String, Path,)),
    request_body = UpdateSettingRequest,
    responses((status = 200, body = Setting)))]
pub async fn put(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(key): Path<String>,
    Json(req): Json<UpdateSettingRequest>,
) -> ApiResult<Json<Setting>> {
    let scope = req.scope.unwrap_or_else(|| "user".into());
    if scope != "tenant" && scope != "user" {
        return Err(ApiError::BadRequest(
            "scope must be 'tenant' or 'user'".into(),
        ));
    }
    let user_id = (scope == "user").then_some(auth.user_id);
    let setting: Setting = state
        .db
        .query_one(
            "INSERT INTO settings (id, tenant_id, scope, user_id, key, value)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (tenant_id, scope, user_id, key)
         DO UPDATE SET value = EXCLUDED.value
         RETURNING *",
            params![
                SettingId::new(),
                auth.tenant_id,
                &scope,
                user_id.map(|x| x.0),
                &key,
                &req.value
            ],
        )
        .await?;
    Ok(Json(setting))
}
