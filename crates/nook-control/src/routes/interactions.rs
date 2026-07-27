//! REST surface for durable interactions (MAIN-159). Thin wrappers over
//! `services::interactions`; the authorization lives there.
//!
//! Create is executor-scoped: a node (running a loop job) or a person may raise
//! one, and the service enforces the anti-spoof rule. Answering, listing, and
//! cancelling are person actions.

use axum::extract::{Path, State};
use axum::Json;
use nook_types::*;

use crate::auth::AuthCtx;
use crate::error::ApiResult;
use crate::services::interactions;
use crate::state::AppState;

#[utoipa::path(post, path = "/api/v1/interactions",
    operation_id = "interaction_create",
    request_body = CreateInteractionRequest,
    responses((status = 200, body = Interaction)))]
pub async fn create(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<CreateInteractionRequest>,
) -> ApiResult<Json<Interaction>> {
    // No `require_user`: an executor node raising an ask for the job it runs is
    // the primary path (AC-1). The service does the anti-spoof / visibility work.
    Ok(Json(
        interactions::create(&state, auth.tenant_id, &auth, req).await?,
    ))
}

#[utoipa::path(get, path = "/api/v1/interactions",
    operation_id = "interaction_list_pending",
    responses((status = 200, body = [Interaction])))]
pub async fn list_pending(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<Vec<Interaction>>> {
    auth.require_user()?;
    Ok(Json(
        interactions::list_pending(&state, auth.tenant_id, auth.user_id).await?,
    ))
}

#[utoipa::path(get, path = "/api/v1/interactions/{id}",
    operation_id = "interaction_get",
    params(("id" = String, Path,)),
    responses((status = 200, body = Interaction)))]
pub async fn get(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<InteractionId>,
) -> ApiResult<Json<Interaction>> {
    // Left open to a node token too: `nook interactions ask --wait` polls this to
    // pull the answer, and inside a job session it authenticates as the node —
    // which the service lets read its own interaction regardless of subject
    // visibility.
    Ok(Json(
        interactions::get(&state, auth.tenant_id, &auth, id).await?,
    ))
}

#[utoipa::path(post, path = "/api/v1/interactions/{id}/answer",
    operation_id = "interaction_answer",
    params(("id" = String, Path,)),
    request_body = AnswerInteractionRequest,
    responses((status = 200, body = Interaction)))]
pub async fn answer(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<InteractionId>,
    Json(req): Json<AnswerInteractionRequest>,
) -> ApiResult<Json<Interaction>> {
    auth.require_user()?;
    Ok(Json(
        interactions::answer(&state, auth.tenant_id, auth.user_id, id, req.response).await?,
    ))
}

#[utoipa::path(post, path = "/api/v1/interactions/{id}/cancel",
    operation_id = "interaction_cancel",
    params(("id" = String, Path,)),
    responses((status = 200, body = Interaction)))]
pub async fn cancel(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<InteractionId>,
) -> ApiResult<Json<Interaction>> {
    auth.require_user()?;
    Ok(Json(
        interactions::cancel(&state, auth.tenant_id, auth.user_id, id).await?,
    ))
}
