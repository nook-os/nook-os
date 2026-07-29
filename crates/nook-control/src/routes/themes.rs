use axum::extract::{Path, State};
use axum::Json;
use nook_db::{params, Db};
use nook_types::*;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

#[utoipa::path(get, path = "/api/v1/themes",
    operation_id = "list_themes", responses((status = 200, body = [Theme])))]
pub async fn list(State(state): State<AppState>, auth: AuthCtx) -> ApiResult<Json<Vec<Theme>>> {
    // Built-ins (tenant NULL) plus the tenant's own themes.
    let themes: Vec<Theme> = state
        .db
        .query_all(
            "SELECT * FROM themes WHERE tenant_id IS NULL OR tenant_id = $1 ORDER BY name",
            params![auth.tenant_id],
        )
        .await?;
    Ok(Json(themes))
}

#[utoipa::path(get, path = "/api/v1/themes/{slug}",
    operation_id = "get_theme",
    params(("slug" = String, Path,)),
    responses((status = 200, body = Theme), (status = 404)))]
pub async fn get_one(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(slug): Path<String>,
) -> ApiResult<Json<Theme>> {
    let theme: Option<Theme> = state
        .db
        .query_opt(
            "SELECT * FROM themes WHERE slug = $1 AND (tenant_id IS NULL OR tenant_id = $2)",
            params![&slug, auth.tenant_id],
        )
        .await?;
    theme.map(Json).ok_or(ApiError::NotFound)
}
