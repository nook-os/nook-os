//! `GET /api/v1/tenants/{id}/export` — a tenant's data, leaving (MAIN-659).
//!
//! The archive is built and sent as it goes: the handler resolves who is
//! asking, answers with headers, and hands the writing to a task that pushes
//! gzip chunks into the body. Nothing is staged on disk and no part of the
//! archive is held whole (AC-1).
//!
//! **A failure after the headers breaks the body rather than shortening it.**
//! The status has already gone; the only honest signal left is a stream that
//! does not decompress. A truncated archive that looks complete is the one
//! outcome worth avoiding, because it is the one a restore would trust.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::Response;
use chrono::Utc;
use nook_types::TenantId;
use tokio::sync::mpsc;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::services::tenant_archive::{self, ArchiveSink, Chunk};
use crate::state::AppState;

/// How many gzip chunks may be queued ahead of the client. Backpressure, not a
/// buffer: a slow reader stops the exporter rather than growing this.
const CHANNEL_DEPTH: usize = 4;

/// The whole tenant, as a gzip tarball.
///
/// Owner-only (AC-2). Not admin: an export is every card, every workspace and
/// every uploaded file in one file that leaves the deployment, which is a
/// decision about the tenant itself rather than about the work inside it. A
/// node token is refused outright — a machine has no reason to pull one.
#[utoipa::path(get, path = "/api/v1/tenants/{id}/export",
    operation_id = "export_tenant",
    params(("id" = String, Path,)),
    responses(
        (status = 200, description = "a gzip tarball of the tenant", content_type = "application/gzip"),
        (status = 403, description = "the caller does not own this tenant"),
        (status = 404)))]
pub async fn export(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(tenant): Path<TenantId>,
) -> ApiResult<Response> {
    auth.require_user()?;
    if tenant != auth.tenant_id {
        return Err(ApiError::ForbiddenMsg(
            "you can only export the tenant you are switched into".into(),
        ));
    }
    let role = state
        .identity
        .membership_role(tenant, auth.user_id.0)
        .await?
        .ok_or_else(|| ApiError::ForbiddenMsg("you are not a member of this tenant".into()))?;
    if role != "owner" {
        return Err(ApiError::ForbiddenMsg(
            "exporting a tenant needs the owner role".into(),
        ));
    }
    crate::repo::tenant_export::require_postgres(&state.db)?;

    // Opened here rather than in the task so "no such tenant" is a 404 with the
    // ordinary body, not a broken download.
    let mut ex = crate::repo::tenant_export::begin(&state.db).await?;
    let identity = match ex.tenant(tenant).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            ex.finish().await;
            return Err(ApiError::NotFound);
        }
        Err(e) => {
            ex.finish().await;
            return Err(e);
        }
    };
    // The snapshot cannot cross into the spawned task: it borrows the pool. The
    // task opens its own, which is the same database a moment later — and the
    // only thing read from this one is the tenant's own name, which is what the
    // filename needs before a single byte is sent.
    ex.finish().await;

    let at = Utc::now();
    let filename = tenant_archive::archive_filename(&identity.slug, at);

    let (tx, rx) = mpsc::channel::<Chunk>(CHANNEL_DEPTH);
    let db = state.db.clone();
    let store = state.user_content_store.clone();
    let version = env!("CARGO_PKG_VERSION");
    tokio::spawn(async move {
        let mut ex = match crate::repo::tenant_export::begin(&db).await {
            Ok(ex) => ex,
            Err(e) => {
                ArchiveSink::new(tx, at).fail(&e.to_string()).await;
                return;
            }
        };
        let sink = ArchiveSink::new(tx, at);
        if let Err(e) = tenant_archive::write_archive(
            &mut ex,
            store.as_ref(),
            tenant,
            identity,
            version,
            at,
            sink,
        )
        .await
        {
            tracing::warn!(%tenant, error = %e, "tenant export ended early");
        }
        ex.finish().await;
    });

    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|chunk| (chunk, rx))
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/gzip")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        // Nothing about an archive is cacheable, and an intermediary holding
        // one is a copy of a tenant nobody knows about.
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(stream))
        .map_err(|e| ApiError::Internal(e.into()))
}

#[cfg(test)]
mod tests {
    /// The gate is the whole point of this route and its happy path works
    /// without it, so assert it at the source too — the integration suite
    /// exercises the decision, this catches a deletion.
    fn body() -> &'static str {
        include_str!("tenant_export.rs")
            .split("pub async fn export(")
            .nth(1)
            .expect("the handler")
            .split("\n#[cfg(test)]")
            .next()
            .expect("its body")
    }

    #[test]
    fn export_is_owner_only_and_never_a_machine() {
        let b = body();
        assert!(
            b.contains("auth.require_user()?"),
            "a node token cannot export a tenant (AC-2)"
        );
        assert!(
            b.contains("membership_role") && b.contains("!= \"owner\""),
            "the owner role is read from tenant_members and required (AC-2)"
        );
        assert!(
            b.contains("tenant != auth.tenant_id"),
            "you export the tenant you are switched into, never another"
        );
    }
}
