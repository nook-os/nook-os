//! Node enrollment: `nook join --server URL --token nook_join_…`.

use axum::extract::State;
use axum::Json;
use chrono::{Duration, Utc};
use nook_types::*;
use rand::distr::Alphanumeric;
use rand::Rng;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::events::{self, EventDraft};
use crate::seed::hash_token;
use crate::state::AppState;

fn random_token(prefix: &str, len: usize) -> String {
    let body: String = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(char::from)
        .collect();
    format!("{prefix}{body}")
}

/// POST /api/v1/nodes/join-tokens — mint a token to enroll a new machine.
#[utoipa::path(post, path = "/api/v1/nodes/join-tokens",
    responses((status = 200, body = CreateJoinTokenResponse)))]
pub async fn create_join_token(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<CreateJoinTokenResponse>> {
    // Enrolling machines is an act of administration. A node that could mint
    // join tokens could quietly attach machines an operator never approved.
    auth.require_user()?;
    let token = random_token("nook_join_", 32);
    let expires_at = Utc::now() + Duration::hours(24);
    sqlx::query(
        "INSERT INTO join_tokens (id, tenant_id, token_hash, name, created_by, expires_at)
         VALUES ($1, $2, $3, '', $4, $5)",
    )
    .bind(JoinTokenId::new())
    .bind(auth.tenant_id)
    .bind(hash_token(&token))
    .bind(auth.user_id)
    .bind(expires_at)
    .execute(&state.db)
    .await?;
    Ok(Json(CreateJoinTokenResponse { token, expires_at }))
}

/// POST /api/v1/nodes/join — unauthenticated; the join token IS the
/// credential. Idempotent per (tenant, name): re-joining an existing node
/// rotates its token instead of creating a duplicate, which keeps container
/// reboots predictable. Join tokens stay valid until expiry (M1 simplicity);
/// `used_at` records last use for audit.
#[utoipa::path(post, path = "/api/v1/nodes/join",
    request_body = JoinRequest,
    responses((status = 200, body = JoinResponse), (status = 401, description = "bad token")))]
pub async fn join(
    State(state): State<AppState>,
    Json(req): Json<JoinRequest>,
) -> ApiResult<Json<JoinResponse>> {
    let row: Option<(JoinTokenId, TenantId)> = sqlx::query_as(
        "UPDATE join_tokens SET used_at = now()
         WHERE token_hash = $1 AND expires_at > now()
         RETURNING id, tenant_id",
    )
    .bind(hash_token(&req.token))
    .fetch_optional(&state.db)
    .await?;
    let Some((_, tenant_id)) = row else {
        return Err(ApiError::Unauthorized);
    };

    let name = if req.name.trim().is_empty() {
        req.hostname.clone()
    } else {
        req.name.clone()
    };
    let node_token = random_token("nook_node_", 40);

    let (node_id,): (NodeId,) = sqlx::query_as(
        "INSERT INTO nodes (id, tenant_id, name, hostname, platform, node_token_hash, status)
         VALUES ($1, $2, $3, $4, $5, $6, 'offline')
         ON CONFLICT (tenant_id, name) DO UPDATE SET
            hostname = EXCLUDED.hostname,
            platform = EXCLUDED.platform,
            node_token_hash = EXCLUDED.node_token_hash,
            updated_at = now()
         RETURNING id",
    )
    .bind(NodeId::new())
    .bind(tenant_id)
    .bind(&name)
    .bind(&req.hostname)
    .bind(&req.platform)
    .bind(hash_token(&node_token))
    .fetch_one(&state.db)
    .await?;

    events::record(
        &state,
        tenant_id,
        EventDraft::new("node.joined")
            .actor("node", node_id.0)
            .node(node_id)
            .payload(serde_json::json!({ "name": name, "hostname": req.hostname })),
    )
    .await;

    Ok(Json(JoinResponse {
        node_id,
        node_name: name,
        node_token,
    }))
}
