//! Channel CRUD, scoped to the caller's tenant (MAIN-49 AC-1, AC-5).
//!
//! v1 channels are tenant-owned: the generic owner model with
//! `owner_type='tenant'`, `owner_id = the caller's tenant`. A caller only ever
//! sees, updates, posts to, or subscribes to channels of a tenant they belong
//! to — enforced here and reused by the message and websocket handlers via
//! [`access`].

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use nook_types::{ChatChannel, CreateChatChannel, UpdateChatChannel};
use uuid::Uuid;

use crate::{AppState, Caller, ChatError};

#[derive(sqlx::FromRow)]
struct ChannelRow {
    id: Uuid,
    name: String,
    slug: String,
    archived_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

impl From<ChannelRow> for ChatChannel {
    fn from(r: ChannelRow) -> Self {
        ChatChannel {
            id: r.id,
            name: r.name,
            slug: r.slug,
            archived: r.archived_at.is_some(),
            created_at: r.created_at,
        }
    }
}

/// A channel's tenant-scope facts, resolved once and reused by every handler
/// that touches a channel by id.
pub struct Access {
    pub archived: bool,
}

/// Resolve a channel to the tenant scope, or refuse. A channel of another tenant
/// is 403 (AC-5, "cross-tenant access is refused"); a channel that does not
/// exist at all is 404. The two are kept distinct on purpose — a member of the
/// owning tenant gets a real answer, everyone else gets the same 403 whether or
/// not the channel is archived.
pub async fn access(
    db: &sqlx::PgPool,
    channel_id: Uuid,
    tenant: Uuid,
) -> Result<Access, ChatError> {
    let row: Option<(Uuid, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT owner_id, archived_at FROM chat_channels
         WHERE id = $1 AND owner_type = 'tenant'",
    )
    .bind(channel_id)
    .fetch_optional(db)
    .await
    .map_err(|_| ChatError::Internal)?;

    match row {
        None => Err(ChatError::NotFound),
        Some((owner, _)) if owner != tenant => Err(ChatError::Forbidden),
        Some((_, archived_at)) => Ok(Access {
            archived: archived_at.is_some(),
        }),
    }
}

pub async fn create(
    State(state): State<AppState>,
    caller: Caller,
    Json(req): Json<CreateChatChannel>,
) -> Result<(StatusCode, Json<ChatChannel>), ChatError> {
    let name = req.name.trim();
    if name.is_empty() {
        return Err(ChatError::BadRequest("a channel needs a name".into()));
    }
    let slug = slugify(name);
    let row = sqlx::query_as::<_, ChannelRow>(
        "INSERT INTO chat_channels (id, owner_type, owner_id, name, slug)
         VALUES ($1, 'tenant', $2, $3, $4)
         RETURNING id, name, slug, archived_at, created_at",
    )
    .bind(Uuid::now_v7())
    .bind(caller.tenant_id)
    .bind(name)
    .bind(&slug)
    .fetch_one(&state.db)
    .await
    .map_err(|e| {
        if is_unique_violation(&e) {
            ChatError::Conflict("a channel with that name already exists".into())
        } else {
            ChatError::Internal
        }
    })?;
    Ok((StatusCode::CREATED, Json(row.into())))
}

pub async fn list(
    State(state): State<AppState>,
    caller: Caller,
) -> Result<Json<Vec<ChatChannel>>, ChatError> {
    // Archived channels drop out of the default list but keep their history
    // (AC-1); the caller only ever sees their own tenant's (AC-5).
    let rows = sqlx::query_as::<_, ChannelRow>(
        "SELECT id, name, slug, archived_at, created_at FROM chat_channels
         WHERE owner_type = 'tenant' AND owner_id = $1 AND archived_at IS NULL
         ORDER BY created_at",
    )
    .bind(caller.tenant_id)
    .fetch_all(&state.db)
    .await
    .map_err(|_| ChatError::Internal)?;
    Ok(Json(rows.into_iter().map(Into::into).collect()))
}

pub async fn update(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateChatChannel>,
) -> Result<Json<ChatChannel>, ChatError> {
    // Scope check first, so another tenant's channel is a clean 403 (AC-5)
    // rather than a silent no-op update.
    access(&state.db, id, caller.tenant_id).await?;

    if let Some(name) = req.name.as_deref() {
        if name.trim().is_empty() {
            return Err(ChatError::BadRequest(
                "a channel name cannot be blank".into(),
            ));
        }
    }

    // `$4` says "archived was supplied"; when it was, `$5` sets archived_at to
    // now (archive) or NULL (restore). name is COALESCEd so an absent name is
    // left untouched.
    let row = sqlx::query_as::<_, ChannelRow>(
        "UPDATE chat_channels
         SET name = COALESCE($3, name),
             archived_at = CASE
                 WHEN $4 THEN (CASE WHEN $5 THEN now() ELSE NULL END)
                 ELSE archived_at
             END
         WHERE id = $1 AND owner_type = 'tenant' AND owner_id = $2
         RETURNING id, name, slug, archived_at, created_at",
    )
    .bind(id)
    .bind(caller.tenant_id)
    .bind(req.name.as_deref().map(str::trim))
    .bind(req.archived.is_some())
    .bind(req.archived.unwrap_or(false))
    .fetch_optional(&state.db)
    .await
    .map_err(|_| ChatError::Internal)?
    .ok_or(ChatError::NotFound)?;
    Ok(Json(row.into()))
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.code().as_deref() == Some("23505"))
}

/// A URL-safe slug from a channel name: lowercase, `[a-z0-9-]`, collapsed dashes,
/// capped. The `(owner, slug)` uniqueness constraint makes the slug the channel's
/// stable handle within its tenant.
pub fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut pending_dash = false;
    for ch in name.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            out.push(lower);
            pending_dash = false;
        } else {
            pending_dash = true;
        }
    }
    if out.len() > 64 {
        out.truncate(64);
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("channel");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::slugify;

    #[test]
    fn slugs_are_lowercase_and_url_safe() {
        assert_eq!(slugify("General"), "general");
        assert_eq!(slugify("  Big Team Chat!!  "), "big-team-chat");
        assert_eq!(slugify("a/b\\c"), "a-b-c");
        assert_eq!(slugify("™™™"), "channel");
        assert!(slugify(&"x".repeat(200)).len() <= 64);
        assert!(!slugify("trailing---").ends_with('-'));
    }
}
