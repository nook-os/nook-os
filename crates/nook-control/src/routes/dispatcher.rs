use axum::extract::State;
use axum::Json;
use nook_dispatcher::{DispatchContext, DispatchError};
use nook_types::*;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::services::session_queries;
use crate::state::AppState;

/// A dispatcher failure as an HTTP error.
///
/// A free function rather than `impl From<DispatchError> for ApiError`: since
/// MAIN-274 moved `ApiError` to `nook-errors`, both types are foreign here and
/// the orphan rule forbids the impl. Putting it in `nook-errors` instead would
/// drag the dispatcher into the error crate for one mapping, so it stays beside
/// the one route that needs it. The mapping itself is unchanged.
fn dispatch_error(e: DispatchError) -> ApiError {
    match e {
        DispatchError::NotConfigured(id) => {
            ApiError::BadRequest(format!("dispatcher backend '{id}' is not configured"))
        }
        DispatchError::Internal(m) => ApiError::Internal(anyhow::anyhow!(m)),
    }
}

#[utoipa::path(post, path = "/api/v1/dispatcher/suggest",
    operation_id = "dispatcher_suggest",
    responses((status = 200, body = DispatchSuggestion)))]
pub async fn suggest(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<DispatchSuggestion>> {
    // Assemble the dispatcher's (deliberately limited) view of the world.
    let boards = state.kanban.all_boards(auth.tenant_id).await?;
    let mut tasks = Vec::new();
    let mut columns = Vec::new();
    for board in &boards {
        if let Some(provider) = state.kanban.get(&board.provider) {
            if let Ok(detail) = provider.board_detail(auth.tenant_id, board.id).await {
                tasks.extend(detail.tasks);
                columns.extend(detail.columns);
            }
        }
    }
    // The dispatcher's counts are tenant-wide capacity signals, unchanged by the
    // per-member listing scopes (MAIN-132/133: no dispatch changes).
    // main's session form (#204) + this branch's node form (MAIN-252).
    let sessions = session_queries::list_sessions(
        &*state.sessions,
        &*state.workspaces,
        auth.tenant_id,
        None,
        true,
        None,
    )
    .await?;
    let nodes = state.nodes.list(auth.tenant_id, None).await?;

    let suggestion = state
        .dispatcher
        .suggest(DispatchContext {
            tasks,
            columns,
            active_sessions: sessions.len(),
            online_nodes: nodes.iter().filter(|n| n.status == "online").count(),
        })
        .await
        .map_err(dispatch_error)?;
    Ok(Json(suggestion))
}
