//! `nook teach` — one skill, every agent, every machine.
//!
//! The control plane is the authority here rather than a relay. A command that
//! only pushed to whoever was connected would make "does my fleet know this?"
//! depend on which laptops were open at the moment somebody ran it, and a node
//! that joins next week would silently be the one machine that never learned
//! the skill. So the skill is STORED, and the fan-out is the fast path — nodes
//! that miss it converge when they reconnect (see `ws::node` on register).
//!
//! What the node does with it lives in `nook-node`: detect the agents actually
//! installed, write `<skills>/<name>/SKILL.md` under each. This end decides
//! what is worth sending and to whom.

use axum::extract::{Path, State};
use axum::Json;
use nook_types::*;
use sha2::{Digest, Sha256};

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// The name check lives in `nook-proto`, next to the message that carries it,
/// so this end and the node cannot drift apart — a name accepted here and
/// refused there is a skill that reports as taught and exists nowhere. The node
/// still applies it independently; it is the end that makes a path out of it.
fn validate_name(name: &str) -> Result<String, ApiError> {
    nook_proto::valid_skill_name(name)
        .map(str::to_string)
        .map_err(ApiError::BadRequest)
}

use nook_proto::skill_name_from_frontmatter as name_from_frontmatter;

fn digest(content: &str) -> String {
    format!("{:x}", Sha256::digest(content.as_bytes()))
}

#[utoipa::path(get, path = "/api/v1/skills",
    operation_id = "list_skills", responses((status = 200, body = [SkillSummary])))]
pub async fn list(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<Vec<SkillSummary>>> {
    let rows: Vec<SkillSummary> = sqlx::query_as(
        "SELECT id, name, sha256, length(content)::bigint AS size, updated_at
         FROM skills WHERE tenant_id = $1 ORDER BY name",
    )
    .bind(auth.tenant_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

#[utoipa::path(get, path = "/api/v1/skills/{name}",
    operation_id = "get_skill",
    params(("name" = String, Path,)),
    responses((status = 200, body = Skill), (status = 404)))]
pub async fn get_one(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(name): Path<String>,
) -> ApiResult<Json<Skill>> {
    let row: Option<Skill> = sqlx::query_as(
        "SELECT id, tenant_id, name, content, sha256, updated_at, updated_by
         FROM skills WHERE tenant_id = $1 AND name = $2",
    )
    .bind(auth.tenant_id)
    .bind(&name)
    .fetch_optional(&state.db)
    .await?;
    row.map(Json).ok_or(ApiError::NotFound)
}

/// Teach the fleet. Re-teaching the same name replaces it, because "I improved
/// the skill, push it everywhere" is the common case and it has to be one verb.
#[utoipa::path(post, path = "/api/v1/skills",
    operation_id = "teach_skill",
    request_body = TeachRequest,
    responses((status = 200, body = TeachResponse), (status = 400)))]
pub async fn teach(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<TeachRequest>,
) -> ApiResult<Json<TeachResponse>> {
    // Writing files onto every machine in the fleet is an act on the fleet. A
    // node token authenticates one machine reporting about itself, and must
    // never be able to reprogram its peers.
    auth.require_user()?;

    if req.content.trim().is_empty() {
        return Err(ApiError::BadRequest("that skill file is empty".into()));
    }
    // A skill is a document agents load into context. There is no legitimate
    // multi-megabyte skill, and this ships to every machine.
    const MAX: usize = 512 * 1024;
    if req.content.len() > MAX {
        return Err(ApiError::BadRequest(format!(
            "that skill is {} KiB; the limit is {} KiB",
            req.content.len() / 1024,
            MAX / 1024
        )));
    }

    let name = validate_name(
        &req.name
            .clone()
            .or_else(|| name_from_frontmatter(&req.content))
            .ok_or_else(|| {
                ApiError::BadRequest(
                    "could not tell what this skill is called: it has no frontmatter `name:`, \
                     so pass one explicitly"
                        .into(),
                )
            })?,
    )?;
    let sha = digest(&req.content);

    let summary: SkillSummary = sqlx::query_as(
        "INSERT INTO skills (id, tenant_id, name, content, sha256, updated_by)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (tenant_id, name) DO UPDATE
           SET content = EXCLUDED.content,
               sha256 = EXCLUDED.sha256,
               updated_at = now(),
               updated_by = EXCLUDED.updated_by
         RETURNING id, name, sha256, length(content)::bigint AS size, updated_at",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(auth.tenant_id)
    .bind(&name)
    .bind(&req.content)
    .bind(&sha)
    .bind(auth.user_id.0)
    .fetch_one(&state.db)
    .await?;

    let (delivered_to, offline) = fan_out(
        &state,
        auth.tenant_id,
        nook_proto::ControlToNode::InstallSkill {
            name: name.clone(),
            content: req.content.clone(),
            sha256: sha,
        },
    )
    .await?;

    crate::events::record(
        &state,
        auth.tenant_id,
        crate::events::EventDraft::new("skill.taught")
            .actor("user", auth.user_id.0)
            .payload(serde_json::json!({
                "skill": name,
                "delivered": delivered_to.len(),
                "offline": offline.len(),
            })),
    )
    .await;

    Ok(Json(TeachResponse {
        skill: summary,
        delivered_to,
        offline,
    }))
}

/// Unteach: forget it here, and tell every node to remove it.
#[utoipa::path(delete, path = "/api/v1/skills/{name}",
    operation_id = "unteach_skill",
    params(("name" = String, Path,)),
    responses((status = 200, body = TeachResponse), (status = 404)))]
pub async fn unteach(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(name): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    auth.require_user()?;
    let deleted = sqlx::query("DELETE FROM skills WHERE tenant_id = $1 AND name = $2")
        .bind(auth.tenant_id)
        .bind(&name)
        .execute(&state.db)
        .await?;
    if deleted.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }

    // Deleted here FIRST, so a node that is offline right now converges by not
    // being re-taught on reconnect. The fan-out only accelerates what the
    // stored state already decided — which is why an offline node is not an
    // error, and why the order matters.
    let (delivered_to, offline) = fan_out(
        &state,
        auth.tenant_id,
        nook_proto::ControlToNode::ForgetSkill { name: name.clone() },
    )
    .await?;

    crate::events::record(
        &state,
        auth.tenant_id,
        crate::events::EventDraft::new("skill.unteached")
            .actor("user", auth.user_id.0)
            .payload(serde_json::json!({ "skill": name })),
    )
    .await;

    Ok(Json(serde_json::json!({
        "name": name,
        "delivered_to": delivered_to,
        "offline": offline,
    })))
}

/// Send to every node in the tenant, and say which ones were not there.
///
/// Offline nodes are NAMED rather than counted: "3 nodes were offline" is not
/// something an operator can act on, and saying nothing at all would let a
/// half-taught fleet read as a success.
async fn fan_out(
    state: &AppState,
    tenant_id: TenantId,
    msg: nook_proto::ControlToNode,
) -> Result<(Vec<String>, Vec<String>), ApiError> {
    let nodes: Vec<(NodeId, String)> =
        sqlx::query_as("SELECT id, name FROM nodes WHERE tenant_id = $1 ORDER BY name")
            .bind(tenant_id)
            .fetch_all(&state.db)
            .await?;

    let mut delivered = Vec::new();
    let mut offline = Vec::new();
    for (id, name) in nodes {
        if state.registry.send_to_node(id, msg.clone()) {
            delivered.push(name);
        } else {
            offline.push(name);
        }
    }
    Ok((delivered, offline))
}

/// Everything this tenant knows, for a node that just connected.
///
/// This is what makes the store worth having: a node that was offline when a
/// skill was taught, or one joining for the first time, is handed the whole set
/// on register. The node skips writes whose sha it already has, so the steady
/// state costs nothing.
pub async fn all_for_tenant(
    db: &sqlx::PgPool,
    tenant_id: TenantId,
) -> Result<Vec<nook_proto::ControlToNode>, sqlx::Error> {
    let rows: Vec<(String, String, String)> =
        sqlx::query_as("SELECT name, content, sha256 FROM skills WHERE tenant_id = $1")
            .bind(tenant_id)
            .fetch_all(db)
            .await?;
    Ok(rows
        .into_iter()
        .map(
            |(name, content, sha256)| nook_proto::ControlToNode::InstallSkill {
                name,
                content,
                sha256,
            },
        )
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The name rules themselves are tested in `nook-proto`, where they live.
    /// What matters here is that this route actually applies them and turns a
    /// refusal into a 400 rather than a 500 — the wiring, not the policy.
    #[test]
    fn a_bad_name_is_a_bad_request_not_a_crash() {
        for bad in ["..", "a/b", "has space", ""] {
            match validate_name(bad) {
                Err(ApiError::BadRequest(_)) => {}
                other => panic!("expected 400 for {bad:?}, got {other:?}"),
            }
        }
        assert_eq!(validate_name("code-review").unwrap(), "code-review");
    }

    #[test]
    fn the_digest_is_of_the_content() {
        // Empty-string SHA-256, so a silently-wrong hasher is caught.
        assert_eq!(
            digest(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_ne!(digest("a"), digest("b"));
    }
}
