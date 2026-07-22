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
    // Read the serving certificate fresh rather than caching: an operator who
    // renews it should not have to restart the control plane for join tokens
    // to start naming the new one.
    let ca_fingerprint = state.cfg.agent_tls_cert.as_deref().and_then(|path| {
        let pem = std::fs::read_to_string(path).ok()?;
        crate::ca::fingerprint_pem(&pem).ok()
    });

    Ok(Json(CreateJoinTokenResponse {
        token,
        expires_at,
        ca_fingerprint,
    }))
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

// ── mTLS enrolment ──────────────────────────────────────────────────────────
//
// Bootstrap and steady state are separate problems, modelled on Kubernetes TLS
// bootstrapping. `enroll` is for a machine with no key yet and spends a join
// token. `renew` is for a machine that already has one and spends nothing —
// its keypair IS the credential, which is what keeps a long outage from
// costing a manual re-join.

/// Trade a join token for a certificate signed by the tenant's CA.
#[utoipa::path(post, path = "/api/v1/nodes/enroll",
    operation_id = "enroll_node",
    request_body = EnrollRequest,
    responses((status = 200, body = EnrollResponse), (status = 401), (status = 400)))]
pub async fn enroll(
    State(state): State<AppState>,
    Json(req): Json<EnrollRequest>,
) -> ApiResult<Json<EnrollResponse>> {
    // Spend the token first: a CSR that fails to sign must not leave a token
    // usable for a second attempt by someone else.
    let row: Option<(uuid::Uuid, uuid::Uuid)> = sqlx::query_as(
        "UPDATE join_tokens SET used_at = now()
         WHERE token_hash = $1 AND expires_at > now()
         RETURNING id, tenant_id",
    )
    .bind(hash_token(&req.token))
    .fetch_optional(&state.db)
    .await?;
    let Some((_, tenant_uuid)) = row else {
        return Err(ApiError::Unauthorized);
    };
    let tenant = TenantId(tenant_uuid);

    // Lazily mint the tenant's CA on first enrolment. Only ever when there is
    // none — never as a silent replacement for one that failed to load.
    if crate::ca::trust_bundle(&state.db, tenant).await?.is_empty() {
        crate::ca::generate(&state.db, &state.vault, tenant, true)
            .await
            .map_err(ApiError::Internal)?;
    }

    let name = req.name.unwrap_or_else(|| "node".to_string());
    let node_id = uuid::Uuid::now_v7();
    let node_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status)
         VALUES ($1, $2, $3, $4, 'offline')
         ON CONFLICT (tenant_id, name) DO UPDATE SET updated_at = now()
         RETURNING id",
    )
    .bind(node_id)
    .bind(tenant)
    .bind(&name)
    // The node token stays for now as the transitional credential; the
    // certificate supersedes it once the handshake is wired.
    .bind(crate::seed::hash_token(&format!("enroll-{node_id}")))
    .fetch_one(&state.db)
    .await?;

    issue(&state, tenant, node_id, &req.csr_pem).await
}

/// Renew on the strength of the key the node already holds.
///
/// Works whether the certificate expired five minutes or five months ago, and
/// whether or not the tenant's CA rotated meanwhile — which is precisely why
/// the response carries the whole trust bundle rather than just a certificate.
#[utoipa::path(post, path = "/api/v1/nodes/renew",
    operation_id = "renew_node_cert",
    request_body = RenewRequest,
    responses((status = 200, body = EnrollResponse), (status = 403), (status = 404)))]
pub async fn renew(
    State(state): State<AppState>,
    Json(req): Json<RenewRequest>,
) -> ApiResult<Json<EnrollResponse>> {
    let row: Option<(
        uuid::Uuid,
        Option<String>,
        Option<chrono::DateTime<chrono::Utc>>,
    )> = sqlx::query_as("SELECT tenant_id, public_key_pem, revoked_at FROM nodes WHERE id = $1")
        .bind(req.node_id)
        .fetch_optional(&state.db)
        .await?;
    let Some((tenant_uuid, known_key, revoked_at)) = row else {
        return Err(ApiError::NotFound);
    };
    // Revocation is an explicit act, and it must outrank "my certificate
    // expired" — otherwise a compromised machine simply waits and re-enrols.
    if revoked_at.is_some() {
        return Err(ApiError::ForbiddenMsg(
            "this node has been revoked — re-enrol it with a new join token".into(),
        ));
    }

    // The CSR must carry the same public key the node enrolled with. rcgen has
    // already verified the CSR's self-signature, so matching the key proves
    // possession of the corresponding private key.
    let presented = crate::ca::csr_public_key_pem(&req.csr_pem).map_err(ApiError::Internal)?;
    match known_key {
        Some(k) if k.trim() == presented.trim() => {}
        Some(_) => {
            return Err(ApiError::ForbiddenMsg(
                "that key is not the one this node enrolled with".into(),
            ))
        }
        None => {
            return Err(ApiError::ForbiddenMsg(
                "this node has never enrolled a key — use a join token".into(),
            ))
        }
    }

    issue(&state, TenantId(tenant_uuid), req.node_id.0, &req.csr_pem).await
}

/// Sign, record what was issued, and hand back the trust bundle.
async fn issue(
    state: &AppState,
    tenant: TenantId,
    node_id: uuid::Uuid,
    csr_pem: &str,
) -> ApiResult<Json<EnrollResponse>> {
    let leaf = crate::ca::sign_node_csr(&state.db, &state.vault, tenant, node_id, csr_pem)
        .await
        .map_err(ApiError::Internal)?;

    // Recording which CA signed it is what lets the retirement guard answer
    // "does this CA still have live leaves?".
    sqlx::query(
        "UPDATE nodes SET ca_id = $2, cert_not_after = $3, cert_pem = $4,
                public_key_pem = $5, updated_at = now()
          WHERE id = $1",
    )
    .bind(node_id)
    .bind(leaf.ca_id)
    .bind(leaf.not_after)
    .bind(&leaf.cert_pem)
    .bind(&leaf.public_key_pem)
    .execute(&state.db)
    .await?;

    let ca_bundle = crate::ca::trust_bundle(&state.db, tenant)
        .await?
        .into_iter()
        .map(|c| c.cert_pem)
        .collect();

    events::record(
        state,
        tenant,
        EventDraft::new("node.cert_issued")
            .node(NodeId(node_id))
            .payload(serde_json::json!({ "not_after": leaf.not_after, "ca_id": leaf.ca_id })),
    )
    .await;

    Ok(Json(EnrollResponse {
        node_id: NodeId(node_id),
        cert_pem: leaf.cert_pem,
        ca_bundle,
        not_after: leaf.not_after,
    }))
}
