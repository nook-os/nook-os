//! Reading the chain from a support email to the work it caused (MAIN-330).
//!
//! Two reads, and they are the two questions the surfaces above ask. The inbox
//! (C7) asks *what has arrived for this workspace*; reply threading (C4) asks
//! *which chain is this `Message-Id`*, or, from an open ticket, *which message
//! made this card*.
//!
//! **A `Message-Id` is a query parameter, not a path segment.** It is a
//! sender-authored string carrying `<`, `>`, `@` and sometimes `/`, so putting
//! it in the path would mean every caller getting the same escaping right — and
//! a caller that got it wrong would silently address a different route.
//!
//! Nothing here decrypts anything. A link names the sealed object; fetching its
//! bytes is the user-content route's job, behind its own authentication (C7
//! AC-4).

use axum::extract::{Query, State};
use axum::Json;
use nook_types::*;
use serde::Deserialize;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

#[derive(Debug, Clone, Default, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListFilter {
    /// Only this workspace's chains. Omit for the whole tenant's, which is what
    /// a tenant that scopes its support mail to no repository has.
    ///
    /// A bare `Uuid` rather than the newtype: a query string is parsed by
    /// `serde_urlencoded`, whose value deserializer handles a string and not a
    /// newtype wrapper around one.
    pub workspace_id: Option<uuid::Uuid>,
}

#[derive(Debug, Clone, Default, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct LookupFilter {
    /// The delivery's `Message-Id`, exactly as it arrived — angle brackets
    /// included.
    pub message_id: Option<String>,
    /// A task uuid or key (`NOOK-42`).
    pub task: Option<String>,
}

/// The inbox listing (AC-3): this workspace's chains, newest first.
#[utoipa::path(get, path = "/api/v1/email-links",
    operation_id = "list_email_links", params(ListFilter),
    responses((status = 200, body = [EmailLink])))]
pub async fn list(
    State(state): State<AppState>,
    auth: AuthCtx,
    Query(filter): Query<ListFilter>,
) -> ApiResult<Json<Vec<EmailLink>>> {
    Ok(Json(
        state
            .email_links
            .list(
                auth.tenant_id,
                auth.user_id,
                filter.workspace_id.map(WorkspaceId),
            )
            .await?,
    ))
}

/// One chain, by the message that started it or by the card it became (AC-3).
///
/// Exactly one of the two selectors: asking by both would be two questions with
/// one answer, and the caller could not tell which one was answered.
#[utoipa::path(get, path = "/api/v1/email-links/lookup",
    operation_id = "lookup_email_link", params(LookupFilter),
    responses((status = 200, body = EmailLink), (status = 400), (status = 404)))]
pub async fn lookup(
    State(state): State<AppState>,
    auth: AuthCtx,
    Query(filter): Query<LookupFilter>,
) -> ApiResult<Json<EmailLink>> {
    let message_id = filter.message_id.as_deref().filter(|m| !m.is_empty());
    let task = filter.task.as_deref().filter(|t| !t.is_empty());
    let link = match (message_id, task) {
        (Some(_), Some(_)) | (None, None) => {
            return Err(ApiError::BadRequest(
                "look a chain up by exactly one of message_id or task".into(),
            ))
        }
        (Some(message_id), None) => {
            state
                .email_links
                .by_message_id(auth.tenant_id, auth.user_id, message_id)
                .await?
        }
        (None, Some(ident)) => {
            // Resolved through `readable_task` so a card this viewer may not
            // open is a 404 here too — the same refusal the detail route makes,
            // in the same place (MAIN-76).
            let task =
                crate::services::tasks::readable_task(&state, auth.tenant_id, auth.user_id, ident)
                    .await?;
            state
                .email_links
                .by_task(auth.tenant_id, auth.user_id, task)
                .await?
        }
    };
    Ok(Json(link.ok_or(ApiError::NotFound)?))
}
