//! Mission Control's aggregate read (MAIN-226): the whole fleet in one request,
//! scoped by the SAME node (own+shared) and session (MAIN-133) visibility rules
//! the dedicated list endpoints use — this page must never become a side-channel
//! around them.

use axum::extract::State;
use axum::Json;

use crate::auth::{AuthCtx, Principal};
use crate::error::ApiResult;
use crate::services::overview_queries;
use crate::state::AppState;

#[utoipa::path(get, path = "/api/v1/overview",
    operation_id = "get_overview",
    responses((status = 200, body = nook_types::Overview)))]
pub async fn overview(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<nook_types::Overview>> {
    // `sees_all` is the union the two list endpoints already grant: a node
    // credential and a tenant owner/admin get the unscoped fleet; a member is
    // scoped to their own + shared nodes and their own sessions. Computed once so
    // the node and session scopes can never diverge for the same caller.
    let sees_all = !matches!(auth.principal, Principal::User)
        || auth.is_tenant_admin(state.identity.as_ref()).await?;
    let node_owner = if sees_all {
        None
    } else {
        // Fails closed: an unresolvable person is scoped to one that owns nothing,
        // never shown the fleet.
        Some(
            crate::auth::person_id_of(&state, auth.user_id)
                .await
                .unwrap_or(uuid::Uuid::nil()),
        )
    };
    let session_creator = if sees_all { None } else { Some(auth.user_id) };
    // Card visibility is a per-task owner predicate, not a role (MAIN-76), so it
    // is scoped by the same `sees_all` union: an admin/node sees every ticket on
    // a visible checkout, a member only the ones they could open on the board.
    let task_viewer = if sees_all { None } else { Some(auth.user_id) };

    Ok(Json(
        overview_queries::overview(
            &state.db,
            auth.tenant_id,
            node_owner,
            session_creator,
            task_viewer,
        )
        .await?,
    ))
}
