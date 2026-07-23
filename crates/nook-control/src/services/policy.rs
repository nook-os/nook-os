//! Per-org visibility policy: what an operator may see of a tenant's work.
//!
//! # Two things this is not
//!
//! It is **not** how session content is protected. Terminal streams, prompts
//! and code are outside this mechanism entirely — see `auth/session_guard.rs`.
//! No value set here can widen access to them, because nothing in the session
//! path reads this module. That separation is deliberate: a policy knob that
//! *could* expose session content would make the guarantee a setting.
//!
//! It is **not** environment configuration. Policy is rows with timestamps,
//! never updated in place, because the question it has to answer is "what could
//! my employer see on March 12" — and a config value cannot be asked about the
//! past.
//!
//! # Default closed
//!
//! The absence of a row means off. A new org therefore starts at minimum
//! visibility without anything having to insert defaults, and a bug in a
//! seeding path cannot accidentally open a field.

use nook_types::{PolicyField, TenantId};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// The fields an org can opt into showing.
///
/// Mirrors RBAC.md's policy-gated list. Everything NOT here is either always
/// visible (node names, tenant existence, PR existence) or never visible
/// (session content) — and "never visible" is not represented here at all,
/// because a field you could name is a field somebody could enable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    RepositoryNames,
    BranchNames,
    WorktreePaths,
    TaskTitles,
    PrTitles,
}

impl Field {
    pub fn key(self) -> &'static str {
        match self {
            Field::RepositoryNames => "repository_names",
            Field::BranchNames => "branch_names",
            Field::WorktreePaths => "worktree_paths",
            Field::TaskTitles => "task_titles",
            Field::PrTitles => "pr_titles",
        }
    }

    /// What a person is agreeing to, in words they did not have to be a
    /// developer to understand. Shown to every user, not just the operator.
    pub fn describes(self) -> &'static str {
        match self {
            Field::RepositoryNames => "The names and URLs of your repositories",
            Field::BranchNames => "The names of branches you are working on",
            Field::WorktreePaths => "Where checkouts live on your machines",
            Field::TaskTitles => "The titles and descriptions of your board tasks",
            Field::PrTitles => "The titles of your pull requests and which repo they are in",
        }
    }

    pub const ALL: [Field; 5] = [
        Field::RepositoryNames,
        Field::BranchNames,
        Field::WorktreePaths,
        Field::TaskTitles,
        Field::PrTitles,
    ];

    pub fn parse(key: &str) -> Option<Field> {
        Field::ALL.into_iter().find(|f| f.key() == key)
    }
}

/// Is one field currently on for this org?
///
/// The newest row wins; no row means off.
pub async fn enabled(db: &PgPool, org: Uuid, field: Field) -> ApiResult<bool> {
    let row: Option<(bool,)> = sqlx::query_as(
        "SELECT enabled FROM org_visibility_policy
         WHERE org_id = $1 AND field = $2
         ORDER BY changed_at DESC LIMIT 1",
    )
    .bind(org)
    .bind(field.key())
    .fetch_optional(db)
    .await?;
    Ok(row.map(|(e,)| e).unwrap_or(false))
}

/// Every field with its current state, for a UI to render.
pub async fn current(db: &PgPool, org: Uuid) -> ApiResult<Vec<PolicyField>> {
    let mut out = Vec::with_capacity(Field::ALL.len());
    for f in Field::ALL {
        out.push(PolicyField {
            field: f.key().to_string(),
            description: f.describes().to_string(),
            enabled: enabled(db, org, f).await?,
        });
    }
    Ok(out)
}

/// Change one field. Appends a row, records an event, and tells the people it
/// affects.
///
/// A silent widening is the failure mode that turns governance into betrayal:
/// somebody's employer gains visibility and they find out never. So the people
/// under the org are notified, using the same fan-out as everything else — it
/// reaches their browser, and their phone if they wired one up.
pub async fn set(
    state: &AppState,
    org: Uuid,
    field: &str,
    enabled_now: bool,
    by: Uuid,
) -> ApiResult<()> {
    let f = Field::parse(field)
        .ok_or_else(|| ApiError::BadRequest(format!("{field:?} is not a policy field")))?;

    // Appended, never updated: the history IS the feature.
    sqlx::query(
        "INSERT INTO org_visibility_policy (id, org_id, field, enabled, changed_by)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(Uuid::now_v7())
    .bind(org)
    .bind(f.key())
    .bind(enabled_now)
    .bind(by)
    .execute(&state.db)
    .await?;

    // Every tenant in the org hears about it, in their own tenant's feed.
    let tenants: Vec<(TenantId,)> = sqlx::query_as("SELECT id FROM tenants WHERE org_id = $1")
        .bind(org)
        .fetch_all(&state.db)
        .await?;

    for (tenant,) in tenants {
        crate::events::record(
            state,
            tenant,
            crate::events::EventDraft::new("policy.changed")
                .actor("user", by)
                .payload(serde_json::json!({
                    "field": f.key(),
                    "enabled": enabled_now,
                    "description": f.describes(),
                })),
        )
        .await;

        crate::services::notify::raise(
            state,
            tenant,
            crate::services::notify::Draft::new(if enabled_now {
                "Your organization can now see more"
            } else {
                "Your organization can now see less"
            })
            // Widening is a warning; narrowing is not. The asymmetry is the
            // point — one of them is somebody gaining sight of your work.
            .level(if enabled_now { "warning" } else { "info" })
            .kind("policy.changed")
            .body(format!(
                "{} — {}",
                f.describes(),
                if enabled_now {
                    "now visible to your organization's operators"
                } else {
                    "no longer visible"
                }
            )),
        )
        .await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Session content must not be nameable as a policy field. If it were, a
    /// row could be inserted for it, and somebody would eventually write the
    /// code that honours that row.
    #[test]
    fn no_policy_field_touches_session_content() {
        for f in Field::ALL {
            let k = f.key();
            for forbidden in [
                "session",
                "terminal",
                "prompt",
                "output",
                "keystroke",
                "code",
            ] {
                assert!(
                    !k.contains(forbidden),
                    "`{k}` looks like session content, which is never policy-controlled"
                );
            }
        }
        assert!(Field::parse("session_content").is_none());
        assert!(Field::parse("terminal_output").is_none());
    }

    /// An unknown key must not silently become a stored row that nothing reads
    /// — or worse, one that a later version reads as something else.
    #[test]
    fn unknown_fields_are_refused() {
        assert!(Field::parse("repository_names").is_some());
        assert!(Field::parse("not_a_field").is_none());
        assert!(Field::parse("").is_none());
    }

    /// Every field needs plain language, because every user is shown it.
    #[test]
    fn every_field_explains_itself() {
        for f in Field::ALL {
            let d = f.describes();
            assert!(d.len() > 15, "{} needs a real description", f.key());
            assert!(!d.contains('_'), "{} reads like a column name", f.key());
        }
    }
}
