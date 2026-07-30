//! The one HTTP error both NookOS services return (MAIN-274).
//!
//! There used to be two hand-mirrored types — `ApiError` here and nook-chat's
//! `ChatError` — each with its own `IntoResponse`. Two mappings of the same
//! concept drift: a status code corrected on one side, a body key renamed on
//! the other, and the API a client sees stops being one API. This is that type,
//! moved out of nook-control so both services land on it by construction.
//!
//! It lives beside the panic net (MAIN-273) deliberately. The net's entire
//! contract is emitting the *same* body this mapping emits; agreeing on a body
//! shape across two crates is how they come apart.
//!
//! `sqlx` appears here in `ApiError::Db`. That is not an oversight: this IS the
//! boundary where the driver's error becomes an HTTP status, and concentrating
//! it in one crate is what lets the rest of the tree stop naming it (MAIN-269).

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

pub type ApiResult<T> = Result<T, ApiError>;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden")]
    Forbidden,
    /// Forbidden, with a reason worth reading — "a node token cannot do this"
    /// is the difference between a confusing 403 and an obvious one.
    #[error("{0}")]
    ForbiddenMsg(String),
    #[error("not found")]
    NotFound,
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Conflict(String),
    /// Rate limited. A 429 rather than a 400: the request was fine, there were
    /// just too many of them, and a client that retries later will succeed.
    #[error("{0}")]
    TooManyRequests(String),
    /// The caller has to set something up before this can work — today, an app
    /// password before any secret can be stored. 428 rather than 400 so the UI
    /// can tell "you must do X first" apart from "you sent nonsense".
    #[error("{0}")]
    SetupRequired(String),
    /// A dependency the request needs is temporarily unreachable and the server
    /// is already retrying — a 503 rather than a 400 so a client knows the
    /// request was fine and trying again shortly will work. Today: the IdP is
    /// down and OIDC discovery has not yet succeeded (MAIN-169 AC-2).
    #[error("{0}")]
    ServiceUnavailable(String),
    #[error(transparent)]
    Db(#[from] nook_db::DbError),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, self.to_string()),
            ApiError::Forbidden => (StatusCode::FORBIDDEN, self.to_string()),
            ApiError::ForbiddenMsg(m) => (StatusCode::FORBIDDEN, m.clone()),
            ApiError::NotFound => (StatusCode::NOT_FOUND, self.to_string()),
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m.clone()),
            ApiError::Conflict(m) => (StatusCode::CONFLICT, m.clone()),
            ApiError::TooManyRequests(m) => (StatusCode::TOO_MANY_REQUESTS, m.clone()),
            ApiError::SetupRequired(m) => (StatusCode::PRECONDITION_REQUIRED, m.clone()),
            ApiError::ServiceUnavailable(m) => (StatusCode::SERVICE_UNAVAILABLE, m.clone()),
            ApiError::Db(e) if e.is_row_not_found() => (StatusCode::NOT_FOUND, "not found".into()),
            ApiError::Db(e) => {
                tracing::error!(error = %e, "database error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
            ApiError::Internal(e) => {
                tracing::error!(error = %e, "internal error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

/// The auth crate's failure, as an HTTP error.
///
/// Lives here rather than in either service because the orphan rule puts it
/// here: `AuthError` and `ApiError` are both foreign to nook-control and
/// nook-chat, so neither could write this impl. nook-chat had a copy of it
/// against `ChatError`; this is that copy's one surviving home.
///
/// `AuthError::Db` becomes `Internal` and its detail is preserved in the
/// message, so the failure is logged rather than swallowed — chat's version
/// mapped it to a bare unit variant that logged nothing.
impl From<nook_auth::AuthError> for ApiError {
    fn from(e: nook_auth::AuthError) -> Self {
        match e {
            nook_auth::AuthError::Unauthorized => ApiError::Unauthorized,
            nook_auth::AuthError::Forbidden => ApiError::Forbidden,
            nook_auth::AuthError::Db(e) => ApiError::Db(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn parts(e: ApiError) -> (StatusCode, String) {
        let res = e.into_response();
        let status = res.status();
        let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .expect("a readable body");
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    /// MAIN-274 AC-4: ONE body shape. Every variant, both services — because
    /// both now land on this impl — renders `{"error": <msg>}` and nothing else.
    #[tokio::test]
    async fn every_variant_renders_the_one_body_shape() {
        let cases = [
            (
                ApiError::Unauthorized,
                StatusCode::UNAUTHORIZED,
                "unauthorized",
            ),
            (ApiError::Forbidden, StatusCode::FORBIDDEN, "forbidden"),
            (
                ApiError::ForbiddenMsg("nope".into()),
                StatusCode::FORBIDDEN,
                "nope",
            ),
            (ApiError::NotFound, StatusCode::NOT_FOUND, "not found"),
            (
                ApiError::BadRequest("bad".into()),
                StatusCode::BAD_REQUEST,
                "bad",
            ),
            (
                ApiError::Conflict("dupe".into()),
                StatusCode::CONFLICT,
                "dupe",
            ),
            (
                ApiError::TooManyRequests("slow".into()),
                StatusCode::TOO_MANY_REQUESTS,
                "slow",
            ),
            (
                ApiError::SetupRequired("set up".into()),
                StatusCode::PRECONDITION_REQUIRED,
                "set up",
            ),
            (
                ApiError::ServiceUnavailable("idp".into()),
                StatusCode::SERVICE_UNAVAILABLE,
                "idp",
            ),
            (
                ApiError::Internal(anyhow::anyhow!("boom")),
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error",
            ),
        ];
        for (err, want_status, want_msg) in cases {
            let (status, body) = parts(err).await;
            assert_eq!(status, want_status, "{body}");
            assert_eq!(
                body,
                serde_json::json!({ "error": want_msg }).to_string(),
                "the body is `{{\"error\": …}}` and only that"
            );
        }
    }

    /// The statuses nook-chat used to produce from its own mapping, now served
    /// by this one. Pinned so retiring `ChatError` cannot have moved a code.
    #[tokio::test]
    async fn the_retired_chat_mapping_is_reproduced_exactly() {
        for (err, want) in [
            (ApiError::Unauthorized, StatusCode::UNAUTHORIZED),
            (ApiError::Forbidden, StatusCode::FORBIDDEN),
            (ApiError::NotFound, StatusCode::NOT_FOUND),
            (ApiError::BadRequest(String::new()), StatusCode::BAD_REQUEST),
            (ApiError::Conflict(String::new()), StatusCode::CONFLICT),
            (
                ApiError::Internal(anyhow::anyhow!("x")),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ] {
            assert_eq!(parts(err).await.0, want);
        }
    }

    /// A database "no rows" is a 404, not a 500 — the one variant whose status
    /// depends on the error inside it.
    #[tokio::test]
    async fn a_missing_row_is_not_found_rather_than_internal() {
        let (status, body) = parts(ApiError::Db(nook_db::DbError::Query(
            sqlx::Error::RowNotFound,
        )))
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            body,
            serde_json::json!({ "error": "not found" }).to_string()
        );
    }
}
