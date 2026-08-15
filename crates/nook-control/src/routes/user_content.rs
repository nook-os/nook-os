//! Somewhere to put a file a person uploads (MAIN-532).
//!
//! A **general** user-content store: upload bytes, get them back, delete them.
//! Nothing here knows what a ticket or a message is, and nothing here will —
//! a consumer that wants attachments records the join on its own side and
//! reaches these three routes. That is what lets the second consumer cost
//! nothing.
//!
//! Three decisions are worth reading before changing anything.
//!
//! **Stream by default, redirect only when asked.** The bytes go out through
//! the control plane, exactly as the artifact download does and for the same
//! reason: the object store is commonly on a private network the browser
//! cannot reach, and a presigned URL to an unreachable host fails in a way
//! that looks like the app is broken. `NOOK_USER_CONTENT_REDIRECT` turns
//! redirection on where the store is genuinely public, and it is its own
//! variable rather than `NOOK_ARTIFACT_REDIRECT` because a deployment can
//! reasonably want one and not the other — an installer runs on a machine, a
//! download runs in a browser.
//!
//! **What is stored is not what is served.** The content type on the way in is
//! whatever the uploader's client claimed; the one on the way out is this
//! module's decision. Anything but an image or a PDF leaves as
//! `application/octet-stream` with `Content-Disposition: attachment`, so an
//! uploaded `.html` is downloaded rather than executed in this origin — which
//! is the whole reason a "no allowlist" upload route can be safe (AC-5, AC-7).
//!
//! **The cap is enforced as the body arrives.** Reading the multipart field
//! chunk by chunk and stopping at the first byte over the limit means an
//! oversized upload is refused having buffered a chunk, not 30 MB, and — since
//! the store is written only after the whole field is read — having stored
//! nothing at all.

use std::time::Duration;

use axum::body::Body;
use axum::extract::{Multipart, Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use nook_types::*;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::repo::user_content::{NewContent, StoredContent};
use crate::state::AppState;
use crate::storage::user_content_key;

/// How long a presigned URL is good for. Short because it is handed to a
/// browser that is about to follow it immediately — the redirect is a
/// hand-off, not a shareable link.
const PRESIGN_TTL: Duration = Duration::from_secs(300);

/// What a caller is told when the store cannot take the bytes (MAIN-598).
///
/// Operational, not diagnostic. The path, the bucket, the credential and the
/// OS error are all in the boot log, where an operator can act on them and a
/// caller cannot read them — MAIN-273's rule, applied to a failure that is the
/// server's rather than the request's.
fn storage_unavailable() -> ApiError {
    ApiError::ServiceUnavailable("file storage is not configured".into())
}

/// Refuse before reading a byte of the body when the boot probe already found
/// the store unusable — buffering 25 MiB to fail at the last step wastes the
/// caller's upload and this server's memory.
fn unusable(state: &AppState) -> Result<(), ApiError> {
    match &state.user_content_store_error {
        Some(_) => Err(storage_unavailable()),
        None => Ok(()),
    }
}

/// Content types served inline. Everything else is an attachment, whatever the
/// uploader called it (AC-5).
///
/// The test is deliberately crude — a prefix and one exact string — because
/// the interesting question is not "which types are safe to render" but
/// "which types are safe to render *in this origin*", and the honest answer
/// for anything scriptable is none. Adding to this list is a security decision.
fn served_as(stored: &str) -> (&'static str, String) {
    let base = stored
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if base == "application/pdf" {
        return ("inline", base);
    }
    // An image subtype is echoed back only when it is plainly a subtype, so
    // neither a stored `image/png, text/html` nor a header-splitting attempt
    // can reach a response header through this.
    if let Some(subtype) = base.strip_prefix("image/") {
        let plain = !subtype.is_empty()
            && subtype
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'));
        if plain {
            return ("inline", base);
        }
    }
    ("attachment", "application/octet-stream".to_string())
}

/// `Content-Disposition`, with the filename in both the plain and the RFC 5987
/// form so a non-ASCII name survives without a raw byte reaching a header.
fn disposition(kind: &str, filename: &str) -> String {
    let ascii: String = filename
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let ascii = if ascii.trim().is_empty() {
        "download".to_string()
    } else {
        ascii
    };
    let encoded: String = filename
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    format!("{kind}; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}

/// Accept an upload and record it.
///
/// The bytes are written to the store first and the row second, because the
/// failure that leaves an object with no row is recoverable by a human with a
/// bucket listing, while a row pointing at bytes that were never written is a
/// 404 the UI cannot explain. When the row does fail the object is removed
/// again, best effort.
#[utoipa::path(post, path = "/api/v1/user-content",
    operation_id = "upload_user_content",
    responses(
        (status = 201, body = UserContent),
        (status = 413, description = "larger than the configured cap"),
        (status = 503, description = "the file store is not usable (MAIN-598)")))]
pub async fn upload(
    State(state): State<AppState>,
    auth: AuthCtx,
    mut multipart: Multipart,
) -> ApiResult<Response> {
    auth.require_user()?;
    unusable(&state)?;
    let cap = state.cfg.user_content_max_bytes;

    let mut file: Option<(String, String, Vec<u8>)> = None;
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::BadRequest(format!("malformed upload: {e}")))?
    {
        // A field with no filename is an ordinary form value — a caption, a
        // CSRF token — and skipping it is what lets a client post the file
        // alongside whatever else its form carries.
        let Some(filename) = field.file_name().map(str::to_string) else {
            continue;
        };
        if file.is_some() {
            return Err(ApiError::BadRequest(
                "one file per upload — send the rest as separate requests".into(),
            ));
        }
        let content_type = field
            .content_type()
            .unwrap_or("application/octet-stream")
            .to_string();

        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|e| ApiError::BadRequest(format!("upload ended early: {e}")))?
        {
            if bytes.len() as u64 + chunk.len() as u64 > cap {
                // Returning here drops the multipart reader, so the rest of the
                // body is never read and never buffered (AC-6).
                return Err(ApiError::PayloadTooLarge(format!(
                    "that file is larger than the {} MiB upload limit",
                    cap / (1024 * 1024)
                )));
            }
            bytes.extend_from_slice(&chunk);
        }
        file = Some((filename, content_type, bytes));
    }

    let Some((filename, content_type, bytes)) = file else {
        return Err(ApiError::BadRequest(
            "no file in the upload — send it as a multipart field with a filename".into(),
        ));
    };

    let id = Uuid::now_v7();
    let key = user_content_key(
        &state.cfg.user_content_prefix,
        &auth.tenant_id.0.to_string(),
        &id.to_string(),
    );
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    let size_bytes = bytes.len() as i64;
    state
        .user_content_store
        .put(&key, bytes)
        .await
        .map_err(|e| {
            tracing::error!(%key, error = %e, "the user-content store refused a write");
            storage_unavailable()
        })?;

    let row = match state
        .user_content
        .insert(NewContent {
            id,
            tenant: auth.tenant_id,
            uploaded_by: auth.user_id,
            filename,
            content_type,
            size_bytes,
            sha256,
            storage_key: key.clone(),
        })
        .await
    {
        Ok(row) => row,
        Err(e) => {
            if let Err(cleanup) = state.user_content_store.delete(&key).await {
                tracing::warn!(%key, error = %cleanup, "orphaned upload: the row failed and the object could not be removed");
            }
            return Err(e);
        }
    };

    Ok((StatusCode::CREATED, Json(row.record())).into_response())
}

/// Serve the bytes — streamed, or a 302 to the store when the deployment has
/// opted in and the store can sign.
#[utoipa::path(get, path = "/api/v1/user-content/{id}",
    operation_id = "get_user_content",
    params(("id" = String, Path)),
    responses(
        (status = 200, description = "the stored bytes"),
        (status = 302, description = "a short-lived presigned URL"),
        (status = 404, description = "no such content in this tenant")))]
pub async fn serve(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<Uuid>,
) -> ApiResult<Response> {
    let row = load(&state, id, auth.tenant_id).await?;

    if state.cfg.user_content_redirect {
        // `presign` answering `None` is not a failure — it is a disk store
        // saying it has no URL of its own — so the switch being on simply
        // falls through to streaming (AC-4).
        if let Some(url) = state
            .user_content_store
            .presign(&row.storage_key, PRESIGN_TTL)
            .await
            .map_err(ApiError::Internal)?
        {
            return Ok(
                (StatusCode::FOUND, [(header::LOCATION, url)], headers(&row)).into_response(),
            );
        }
    }

    let bytes = state
        .user_content_store
        .get(&row.storage_key)
        .await
        .map_err(|e| {
            tracing::error!(key = %row.storage_key, error = %e, "user content row has no object behind it");
            ApiError::NotFound
        })?;

    Ok((headers(&row), Body::from(bytes)).into_response())
}

/// Remove the row and the bytes.
///
/// The row goes first: a delete that removed the object and then failed would
/// leave a record pointing at nothing, which serves as a 500 rather than the
/// 404 the caller asked for. An object left behind by a failure after this
/// point is invisible and harmless.
#[utoipa::path(delete, path = "/api/v1/user-content/{id}",
    operation_id = "delete_user_content",
    params(("id" = String, Path)),
    responses((status = 204), (status = 403), (status = 404)))]
pub async fn delete(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    auth.require_user()?;
    let row = load(&state, id, auth.tenant_id).await?;

    let is_uploader = row.uploaded_by == auth.user_id;
    if !is_uploader && !auth.is_tenant_admin(state.identity.as_ref()).await? {
        return Err(ApiError::ForbiddenMsg(
            "only the uploader, or a tenant owner or admin, can delete this".into(),
        ));
    }

    if state.user_content.delete(id, auth.tenant_id).await? == 0 {
        return Err(ApiError::NotFound);
    }
    state
        .user_content_store
        .delete(&row.storage_key)
        .await
        .map_err(ApiError::Internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// The row, or a 404 — **never** a 403. Another tenant's id and an id that was
/// never issued are the same answer on purpose: a 403 would confirm the id
/// exists, which is exactly the probe AC-3 forbids.
async fn load(state: &AppState, id: Uuid, tenant: TenantId) -> ApiResult<StoredContent> {
    state
        .user_content
        .get(id, tenant)
        .await?
        .ok_or(ApiError::NotFound)
}

/// The headers every answer carries, redirect included — a client that follows
/// a 302 to a store which echoes its own content type still learns from these
/// what this server intends the file to be.
fn headers(row: &StoredContent) -> [(header::HeaderName, String); 4] {
    let (kind, content_type) = served_as(&row.content_type);
    [
        (header::CONTENT_TYPE, content_type),
        (
            header::CONTENT_DISPOSITION,
            disposition(kind, &row.filename),
        ),
        // Without this a browser is free to sniff the bytes and decide the
        // octet-stream above was HTML after all, which would undo the whole
        // rule (AC-5).
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
        // Belt to the braces: an SVG is an image and is served inline, and an
        // SVG can carry script. A sandboxed, source-less document renders the
        // picture and runs nothing in this origin.
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; sandbox".to_string(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC-5, at the level the integration tests cannot reach cheaply: the
    /// adversarial spellings of a content type.
    #[test]
    fn only_a_plain_image_or_pdf_is_served_inline() {
        for stored in ["image/png", "IMAGE/PNG", "image/jpeg; charset=binary"] {
            let (kind, ct) = served_as(stored);
            assert_eq!(kind, "inline", "{stored}");
            assert!(ct.starts_with("image/"), "{stored} -> {ct}");
        }
        assert_eq!(
            served_as("application/pdf"),
            ("inline", "application/pdf".to_string())
        );

        for stored in [
            "text/html",
            "application/xhtml+xml",
            "text/plain",
            "",
            // A subtype carrying a second type, or anything a header could be
            // split on, is not a subtype — it downloads.
            "image/png, text/html",
            "image/png\r\nX-Evil: 1",
        ] {
            let (kind, ct) = served_as(stored);
            assert_eq!(kind, "attachment", "{stored}");
            assert_eq!(ct, "application/octet-stream", "{stored}");
        }
    }

    /// A filename reaches a header, so it is the one field an uploader
    /// controls that could inject one. Nothing but graphic ASCII survives into
    /// the quoted form; the real name rides in `filename*`, percent-encoded.
    #[test]
    fn a_filename_cannot_inject_a_header() {
        let d = disposition("attachment", "eviln\r\nX-Evil: 1\"; x=\"y.txt");
        assert!(!d.contains('\r') && !d.contains('\n'), "{d}");
        assert_eq!(d.matches('"').count(), 2, "one quoted value only: {d}");

        // A non-ASCII name is not lost, it is encoded.
        let d = disposition("inline", "résumé.pdf");
        assert!(d.contains("filename*=UTF-8''r%C3%A9sum%C3%A9.pdf"), "{d}");

        // An empty or blank name still yields a filename rather than an empty
        // quoted string, which some clients save as a file called `"`.
        assert!(disposition("attachment", "").contains("\"download\""));
        assert!(disposition("attachment", "   ").contains("\"download\""));
    }
}
