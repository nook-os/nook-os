//! The inbox, the channel registry, and the door anything can knock on.
//!
//! `POST /api/v1/notify` is the important one. It is what `nook notify` calls,
//! what an agent's finish hook calls, and what a CI job calls — one entry point
//! that fans out to every connected UI and every configured channel. Nothing
//! that wants to tell you something needs to know how you want to be told.

use axum::extract::{Path, Query, State};
use axum::Json;
use nook_types::*;
use serde::Deserialize;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::services::notify;
use crate::state::AppState;

#[derive(Debug, Default, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct InboxQuery {
    pub limit: Option<i64>,
    /// Only what has not been read.
    pub unread: Option<bool>,
}

#[utoipa::path(get, path = "/api/v1/notifications",
    operation_id = "list_notifications", params(InboxQuery),
    responses((status = 200, body = NotificationPage)))]
pub async fn list(
    State(state): State<AppState>,
    auth: AuthCtx,
    Query(q): Query<InboxQuery>,
) -> ApiResult<Json<NotificationPage>> {
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let rows: Vec<Notification> = sqlx::query_as(
        "SELECT id, tenant_id, user_id, level, title, body, kind, link, payload,
                read_at, created_at
         FROM notifications
         WHERE tenant_id = $1
           -- Tenant-wide (user_id IS NULL) or addressed to this person. Never
           -- somebody else's.
           AND (user_id IS NULL OR user_id = $2)
           AND (NOT $3::bool OR read_at IS NULL)
         ORDER BY created_at DESC
         LIMIT $4",
    )
    .bind(auth.tenant_id)
    .bind(auth.user_id.0)
    .bind(q.unread.unwrap_or(false))
    .bind(limit)
    .fetch_all(&state.db)
    .await?;

    let (unread,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM notifications
         WHERE tenant_id = $1 AND (user_id IS NULL OR user_id = $2) AND read_at IS NULL",
    )
    .bind(auth.tenant_id)
    .bind(auth.user_id.0)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(NotificationPage {
        notifications: rows,
        unread,
    }))
}

/// Mark one read, or all of them when no id is given.
#[utoipa::path(post, path = "/api/v1/notifications/read",
    operation_id = "mark_notifications_read",
    responses((status = 200, body = NotificationPage)))]
pub async fn mark_read(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Json<NotificationPage>> {
    let id = body.get("id").and_then(|v| v.as_str());
    match id {
        Some(id) => {
            let uuid: uuid::Uuid = id
                .parse()
                .map_err(|_| ApiError::BadRequest("that is not a notification id".into()))?;
            sqlx::query(
                "UPDATE notifications SET read_at = now()
                 WHERE id = $1 AND tenant_id = $2 AND read_at IS NULL",
            )
            .bind(uuid)
            .bind(auth.tenant_id)
            .execute(&state.db)
            .await?;
        }
        None => {
            sqlx::query(
                "UPDATE notifications SET read_at = now()
                 WHERE tenant_id = $1 AND (user_id IS NULL OR user_id = $2) AND read_at IS NULL",
            )
            .bind(auth.tenant_id)
            .bind(auth.user_id.0)
            .execute(&state.db)
            .await?;
        }
    }
    list(State(state), auth, Query(InboxQuery::default())).await
}

#[utoipa::path(delete, path = "/api/v1/notifications",
    operation_id = "clear_notifications", responses((status = 204)))]
pub async fn clear(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<axum::http::StatusCode> {
    sqlx::query(
        "DELETE FROM notifications WHERE tenant_id = $1 AND (user_id IS NULL OR user_id = $2)",
    )
    .bind(auth.tenant_id)
    .bind(auth.user_id.0)
    .execute(&state.db)
    .await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Raise a notification.
///
/// Deliberately open to any authenticated caller INCLUDING a node token: the
/// whole point is that a machine finishing a job can say so. A node can already
/// report events about itself; this is the same trust, with a nicer surface.
#[utoipa::path(post, path = "/api/v1/notify",
    operation_id = "notify", request_body = NotifyRequest,
    responses((status = 202)))]
pub async fn notify_now(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<NotifyRequest>,
) -> ApiResult<axum::http::StatusCode> {
    if req.title.trim().is_empty() {
        return Err(ApiError::BadRequest("a notification needs a title".into()));
    }
    // A node token may call this, which is the point — and also why it needs a
    // budget. A looping hook or a compromised machine would otherwise fill the
    // inbox and spend the tenant's Twilio balance doing it.
    if !state.notify_limit.allow(auth.tenant_id) {
        return Err(ApiError::TooManyRequests(
            "too many notifications — this tenant is rate limited. If this is a \
             hook firing in a loop, that is what this message is for."
                .into(),
        ));
    }
    let mut draft = notify::Draft::new(req.title.trim().chars().take(200).collect::<String>())
        .level(req.level.unwrap_or_else(|| "info".into()))
        .kind(req.kind.unwrap_or_else(|| "custom".into()));
    if let Some(b) = req.body {
        draft = draft.body(b.chars().take(4000).collect::<String>());
    }
    if let Some(l) = req.link {
        draft = draft.link(l);
    }
    if let Some(p) = req.payload {
        draft = draft.payload(p);
    }
    notify::raise(&state, auth.tenant_id, draft).await;
    Ok(axum::http::StatusCode::ACCEPTED)
}

// ── channels ────────────────────────────────────────────────────────────────

/// What providers exist and what each one needs, so the UI builds its forms
/// from the server rather than from a copy of this list that drifts.
#[utoipa::path(get, path = "/api/v1/notification-channels/kinds",
    operation_id = "list_channel_kinds", responses((status = 200, body = [ChannelKind])))]
pub async fn kinds() -> Json<Vec<ChannelKind>> {
    Json(notify::kinds())
}

#[utoipa::path(get, path = "/api/v1/notification-channels",
    operation_id = "list_channels", responses((status = 200, body = [NotificationChannel])))]
pub async fn list_channels(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<Vec<NotificationChannel>>> {
    // `config` is never selected — it holds bot tokens and webhook URLs, and
    // this list is fetched often and logged freely.
    let rows: Vec<NotificationChannel> = sqlx::query_as(
        "SELECT id, tenant_id, kind, name, enabled, levels, kinds,
                last_ok_at, last_error, created_at, updated_at
         FROM notification_channels WHERE tenant_id = $1 ORDER BY name",
    )
    .bind(auth.tenant_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

#[utoipa::path(post, path = "/api/v1/notification-channels",
    operation_id = "create_channel", request_body = CreateChannelRequest,
    responses((status = 200, body = NotificationChannel)))]
pub async fn create_channel(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<CreateChannelRequest>,
) -> ApiResult<Json<NotificationChannel>> {
    auth.require_user()?;
    if !notify::kinds().iter().any(|k| k.id == req.kind) {
        return Err(ApiError::BadRequest(format!(
            "{:?} is not a channel kind — see /notification-channels/kinds",
            req.kind
        )));
    }
    if req.name.trim().is_empty() {
        return Err(ApiError::BadRequest("a channel needs a name".into()));
    }
    guard_config(&req.config)?;

    // A signing secret per channel, generated here and never shown again. The
    // receiver is told it once, when they can write it down.
    let secret = crate::routes::join::random_token("", 32);
    let row: NotificationChannel = sqlx::query_as(
        "INSERT INTO notification_channels (id, tenant_id, kind, name, config, levels, kinds, secret)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         RETURNING id, tenant_id, kind, name, enabled, levels, kinds,
                   last_ok_at, last_error, created_at, updated_at",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(auth.tenant_id)
    .bind(&req.kind)
    .bind(req.name.trim())
    .bind(&req.config)
    .bind(&req.levels)
    .bind(&req.kinds)
    .bind(&secret)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(row))
}

/// Refuse a channel config whose URL points back inside this network.
///
/// Checked when it is configured as well as when it is delivered: a person who
/// pasted the wrong thing should be told now, not have it fail silently later.
fn guard_config(config: &serde_json::Value) -> ApiResult<()> {
    for key in ["url", "server", "webhook_url"] {
        if let Some(u) = config.get(key).and_then(|v| v.as_str()) {
            notify::guard_url(u).map_err(|e| ApiError::BadRequest(e.to_string()))?;
        }
    }
    Ok(())
}

#[utoipa::path(patch, path = "/api/v1/notification-channels/{id}",
    operation_id = "update_channel", params(("id" = String, Path,)),
    request_body = UpdateChannelRequest,
    responses((status = 200, body = NotificationChannel), (status = 404)))]
pub async fn update_channel(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<uuid::Uuid>,
    Json(req): Json<UpdateChannelRequest>,
) -> ApiResult<Json<NotificationChannel>> {
    auth.require_user()?;
    if let Some(c) = &req.config {
        guard_config(c)?;
    }
    // COALESCE on config means omitting it keeps the stored secrets. A UI that
    // cannot read them back must be able to save a name change without
    // blanking the token it never saw.
    let row: Option<NotificationChannel> = sqlx::query_as(
        "UPDATE notification_channels SET
            name = COALESCE($3, name),
            config = COALESCE($4, config),
            enabled = COALESCE($5, enabled),
            levels = COALESCE($6, levels),
            kinds = COALESCE($7, kinds),
            updated_at = now()
         WHERE id = $1 AND tenant_id = $2
         RETURNING id, tenant_id, kind, name, enabled, levels, kinds,
                   last_ok_at, last_error, created_at, updated_at",
    )
    .bind(id)
    .bind(auth.tenant_id)
    .bind(req.name.as_deref())
    .bind(req.config.as_ref())
    .bind(req.enabled)
    .bind(req.levels.as_ref())
    .bind(req.kinds.as_ref())
    .fetch_optional(&state.db)
    .await?;
    row.map(Json).ok_or(ApiError::NotFound)
}

#[utoipa::path(delete, path = "/api/v1/notification-channels/{id}",
    operation_id = "delete_channel", params(("id" = String, Path,)),
    responses((status = 204), (status = 404)))]
pub async fn delete_channel(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<uuid::Uuid>,
) -> ApiResult<axum::http::StatusCode> {
    auth.require_user()?;
    let res = sqlx::query("DELETE FROM notification_channels WHERE id = $1 AND tenant_id = $2")
        .bind(id)
        .bind(auth.tenant_id)
        .execute(&state.db)
        .await?;
    if res.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Send a sample through one channel and report what happened.
#[utoipa::path(post, path = "/api/v1/notification-channels/{id}/test",
    operation_id = "test_channel", params(("id" = String, Path,)),
    responses((status = 200), (status = 400, description = "delivery failed")))]
pub async fn test_channel(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<uuid::Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    auth.require_user()?;
    match notify::test_channel(&state, auth.tenant_id, id).await {
        Ok(()) => Ok(Json(serde_json::json!({ "ok": true }))),
        // The provider's own message, not a generic one: "403 invalid_token"
        // sends somebody to the right place, "delivery failed" does not.
        Err(e) => Err(ApiError::BadRequest(e.to_string())),
    }
}
