use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

use crate::error::ApiResult;
use crate::state::AppState;

/// Readiness: 200 only when the database is reachable. Kubernetes pulls a pod
/// that fails this from Service endpoints but does NOT restart it — a DB blip
/// should stop routing traffic, not recycle the process.
#[utoipa::path(get, path = "/healthz", responses((status = 200, description = "Service ready")))]
pub async fn healthz(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    sqlx::query("SELECT 1").execute(&state.db).await?;
    Ok(Json(json!({ "status": "ok" })))
}

/// Liveness: 200 whenever the process is up, deliberately WITHOUT touching the
/// database. Used as a Kubernetes liveness probe, this must not depend on any
/// external dependency — otherwise a brief DB outage would make the cluster
/// kill and restart otherwise-healthy pods, turning a hiccup into an outage.
/// Takes no `State` at all, so it cannot grow a database call by accident.
#[utoipa::path(get, path = "/livez", responses((status = 200, description = "Process alive")))]
pub async fn livez() -> Json<Value> {
    Json(json!({ "status": "alive" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    // `livez` takes no `State`, so it structurally cannot reach the database —
    // this both pins its body and guards against someone later giving it a
    // `State` and a query (which would reintroduce the DB dependency AC-1
    // forbids: the signature would have to change and this test would need
    // editing to match).
    #[tokio::test]
    async fn livez_reports_alive_without_touching_the_database() {
        let Json(body) = livez().await;
        assert_eq!(body["status"], "alive");
    }
}
