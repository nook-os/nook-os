use axum::extract::{Path, State};
use axum::Json;
use nook_types::*;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::repo::admin::SettingWrite;
use crate::state::AppState;

#[utoipa::path(get, path = "/api/v1/settings",
    operation_id = "list_settings", responses((status = 200, body = [Setting])))]
pub async fn list(State(state): State<AppState>, auth: AuthCtx) -> ApiResult<Json<Vec<Setting>>> {
    // Tenant-scoped settings plus the caller's user-scoped ones.
    let settings = state
        .settings
        .visible_to(auth.tenant_id, auth.user_id)
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
    let setting = state
        .settings
        .put(SettingWrite {
            tenant: auth.tenant_id,
            scope,
            user: user_id,
            key,
            value: req.value,
        })
        .await?;
    Ok(Json(setting))
}
