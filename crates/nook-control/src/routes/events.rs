use axum::extract::{Query, State};
use axum::Json;
use chrono::{DateTime, Utc};
use nook_types::*;
use serde::Deserialize;

use crate::auth::AuthCtx;
use crate::error::ApiResult;
use crate::services::activity_queries;
use crate::state::AppState;

#[derive(Deserialize, utoipa::IntoParams)]
pub struct EventsQuery {
    pub workspace_id: Option<WorkspaceId>,
    /// Kind prefix filter, e.g. "session." matches session.started/exited.
    pub kind: Option<String>,
    pub before: Option<DateTime<Utc>>,
    pub limit: Option<i64>,
}

#[utoipa::path(get, path = "/api/v1/events",
    operation_id = "list_events",
    params(EventsQuery),
    responses((status = 200, body = EventsPage)))]
pub async fn list(
    State(state): State<AppState>,
    auth: AuthCtx,
    Query(q): Query<EventsQuery>,
) -> ApiResult<Json<EventsPage>> {
    // Members see their own activity; owner/admin get the full audit feed. The
    // same scope filters the live bus, so page and push agree (MAIN-134).
    let scope = activity_queries::ActivityScope::load(
        &state.db,
        auth.tenant_id,
        &auth,
        state.identity.as_ref(),
    )
    .await?;
    Ok(Json(
        activity_queries::events_page(
            &state.db,
            auth.tenant_id,
            q.workspace_id,
            q.kind,
            q.before,
            q.limit.unwrap_or(50),
            &scope,
        )
        .await?,
    ))
}
