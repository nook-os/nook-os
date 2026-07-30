//! Managed fleet content — the control plane as source of truth (MAIN-78).
//!
//! The `nookos` skill and the hook set ship embedded in the binary and are
//! installed on each machine at `nook setup`. That makes them drift across a
//! fleet and impossible to steer centrally. This module makes the control plane
//! *hold* that content: seeded from the embedded defaults on boot, checksummed
//! and versioned, so the rest of the fleet-controlled-skills epic (push, per-node
//! matrix, UI, editor) has one authority to build on.
//!
//! This ticket is the store and the read side only. Nothing is pushed and no
//! node behavior changes (NG-1/NG-5): a node still installs the embedded set at
//! `nook setup`. The apply-ready representations below (the `InstallSkill`
//! payload and the settings.json hooks fragment) are *defined* here so
//! sub-ticket 2's push has a settled shape; they are not sent yet.

use axum::extract::State;
use axum::Json;
use nook_types::*;
use sha2::{Digest, Sha256};

use crate::auth::perm::{Permission, Scope};
use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::repo::admin::ManagedContentRepository;
use crate::state::AppState;

/// The managed skill's name — a path component on every machine
/// (`<skills>/nookos/SKILL.md`) and the key of its store row.
const MANAGED_SKILL_NAME: &str = "nookos";
/// The single hook set's row name (there is one, not one-per-hook).
const HOOKS_NAME: &str = "default";

/// The canonical skill document, embedded so the control plane seeds exactly what
/// a node ships with. Same file the node embeds (`wizard/skills.rs`).
const NOOKOS_SKILL: &str = include_str!("../../../../skills/nookos/SKILL.md");

fn digest(content: &str) -> String {
    format!("{:x}", Sha256::digest(content.as_bytes()))
}

/// The hook set's stored body: the `~/.claude/settings.json` `hooks` fragment,
/// pretty-printed, from the shared canonical set (`nook_proto::hooks`).
fn hooks_content() -> String {
    serde_json::to_string_pretty(&nook_proto::hooks::claude_settings_fragment())
        .expect("the hooks fragment is plain JSON and always serializes")
}

// ── Seed ─────────────────────────────────────────────────────────────────────

/// Seed (or refresh) the managed store from the embedded defaults. Called on
/// boot from `seed::run`, all environments — this is built-in content, not a dev
/// fixture.
///
/// Idempotent by design (AC-2): a fresh deploy inserts the rows; a deploy whose
/// shipped default is unchanged is a no-op that leaves any operator edit intact;
/// a deploy carrying a *newer* default refreshes the row and bumps its version.
/// "Newer" is decided by the default's own sha, recorded per row as
/// `default_sha256`, so it is the shipped content changing — not the row
/// differing from the default — that triggers a refresh.
pub async fn seed(repo: &dyn ManagedContentRepository) -> ApiResult<()> {
    upsert_default(repo, "skill", MANAGED_SKILL_NAME, NOOKOS_SKILL).await?;
    upsert_default(repo, "hooks", HOOKS_NAME, &hooks_content()).await?;
    Ok(())
}

/// Seed or refresh one managed row from a shipped default (the primitive `seed`
/// applies to each embedded default). Public so the seed rules can be exercised
/// against a synthetic key without disturbing the real rows.
pub async fn upsert_default(
    repo: &dyn ManagedContentRepository,
    kind: &str,
    name: &str,
    content: &str,
) -> ApiResult<()> {
    let default_sha = digest(content);

    match repo.default_state(kind, name).await? {
        // Fresh: install at version 1, content == default.
        None => {
            repo.install_default(kind, name, content, &default_sha)
                .await?
        }
        // The shipped default advanced: refresh the row to it and bump version.
        // This is the one case that overwrites — a newer default is meant to win.
        Some(state) if state.default_sha256 != default_sha => {
            repo.refresh_to_default(kind, name, content, &default_sha, state.version + 1)
                .await?
        }
        // Unchanged default: leave the row exactly as it is (an operator edit
        // from sub-ticket 5 survives a redeploy of the same binary).
        Some(_) => {}
    }
    Ok(())
}

// ── Read API ─────────────────────────────────────────────────────────────────

/// Gate the read endpoints exactly as node management is gated: `node.manage` on
/// the caller's own tenant, the same guard as `POST /nodes/{id}/update` (AC-3).
/// A node token is refused (it is not a person); an operator is allowed.
async fn require_node_manage(state: &AppState, auth: &AuthCtx) -> Result<(), ApiError> {
    auth.require(state, Permission::NodeManage, Scope::Tenant(auth.tenant_id))
        .await
}

#[utoipa::path(get, path = "/api/v1/managed/skills",
    operation_id = "list_managed_skills",
    responses((status = 200, body = [ManagedContent]), (status = 403)))]
pub async fn list_skills(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<Vec<ManagedContent>>> {
    require_node_manage(&state, &auth).await?;
    let rows = state.managed.list_kind("skill").await?;
    Ok(Json(rows))
}

#[utoipa::path(get, path = "/api/v1/managed/hooks",
    operation_id = "get_managed_hooks",
    responses((status = 200, body = ManagedContent), (status = 403), (status = 404)))]
pub async fn get_hooks(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<ManagedContent>> {
    require_node_manage(&state, &auth).await?;
    let row = state.managed.get("hooks", HOOKS_NAME).await?;
    row.map(Json).ok_or(ApiError::NotFound)
}

// ── Apply-ready representations (defined now, pushed in sub-ticket 2) ─────────

/// The managed skills, expressed in the `ControlToNode::InstallSkill` shape a
/// node applies (AC-4). Sub-ticket 2's push is `send_to_node(node, payload)` over
/// these; here we only prove the stored row maps cleanly onto the wire type.
pub async fn managed_skills_as_install(
    repo: &dyn ManagedContentRepository,
) -> ApiResult<Vec<nook_proto::ControlToNode>> {
    Ok(repo
        .payloads_of_kind("skill")
        .await?
        .into_iter()
        .map(|p| nook_proto::ControlToNode::InstallSkill {
            name: p.name,
            content: p.content,
            sha256: p.sha256,
        })
        .collect())
}

/// The managed hook set, expressed in the `ControlToNode::InstallHooks` shape a
/// node applies (MAIN-105 AC-3). `None` when the store has no hooks row, so
/// connect-replay simply sends nothing rather than an empty push.
pub async fn managed_hooks_as_install(
    repo: &dyn ManagedContentRepository,
) -> ApiResult<Option<nook_proto::ControlToNode>> {
    Ok(repo
        .payload("hooks", HOOKS_NAME)
        .await?
        .map(|p| nook_proto::ControlToNode::InstallHooks {
            content: p.content,
            sha256: p.sha256,
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded defaults are non-empty and their shas are what we store.
    #[test]
    fn embedded_defaults_are_present_and_hash() {
        assert!(NOOKOS_SKILL.contains("nookos") || NOOKOS_SKILL.len() > 100);
        // The hook set renders to a valid settings.json `hooks` object.
        let hooks: serde_json::Value = serde_json::from_str(&hooks_content()).unwrap();
        assert!(hooks.get("Stop").is_some(), "hooks fragment has events");
        // Empty-string sha, so a silently-wrong hasher is caught.
        assert_eq!(
            digest(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    /// A stored skill row maps onto the `InstallSkill` payload sub-ticket 2 will
    /// push (AC-4): name/content/sha survive the round trip unchanged.
    #[test]
    fn managed_skill_round_trips_into_install_skill() {
        let name = MANAGED_SKILL_NAME.to_string();
        let content = NOOKOS_SKILL.to_string();
        let sha256 = digest(&content);
        let msg = nook_proto::ControlToNode::InstallSkill {
            name: name.clone(),
            content: content.clone(),
            sha256: sha256.clone(),
        };
        // Serializes as the adjacently-tagged wire message a node decodes.
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "install_skill");
        assert_eq!(json["data"]["name"], name);
        assert_eq!(json["data"]["sha256"], sha256);
        assert_eq!(json["data"]["content"], content);
    }

    /// The stored hooks row maps onto the `InstallHooks` payload connect-replay
    /// pushes (MAIN-105 AC-3): content and sha survive unchanged, tagged as the
    /// wire message a node decodes.
    #[test]
    fn managed_hooks_round_trip_into_install_hooks() {
        let content = hooks_content();
        let sha256 = digest(&content);
        let msg = nook_proto::ControlToNode::InstallHooks {
            content: content.clone(),
            sha256: sha256.clone(),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "install_hooks");
        assert_eq!(json["data"]["content"], content);
        assert_eq!(json["data"]["sha256"], sha256);
    }

    /// The hooks body is the apply-ready settings.json fragment (AC-4): valid
    /// JSON, one array per Claude event, each entry a command hook.
    #[test]
    fn hooks_body_is_a_valid_settings_fragment() {
        let v: serde_json::Value = serde_json::from_str(&hooks_content()).unwrap();
        let obj = v.as_object().expect("an object");
        assert!(!obj.is_empty());
        for (_event, list) in obj {
            let entry = &list.as_array().expect("array")[0];
            assert_eq!(entry["hooks"][0]["type"], "command");
        }
    }
}
