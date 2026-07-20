use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

use crate::error::ApiResult;
use crate::state::AppState;

#[utoipa::path(get, path = "/healthz", responses((status = 200, description = "Service healthy")))]
pub async fn healthz(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    sqlx::query("SELECT 1").execute(&state.db).await?;
    Ok(Json(json!({ "status": "ok" })))
}
