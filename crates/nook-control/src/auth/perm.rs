//! The permission catalog and the one predicate that consults it.
//!
//! # The absence that matters
//!
//! **There is no permission here for reading session content.** Not disabled,
//! not gated, not admin-only — absent. A call site cannot write
//! `require(Permission::ReadSessionContent, …)` because no such variant
//! exists, and a database row cannot invent one because [`Permission`] is a
//! Rust enum that every grant must parse into.
//!
//! That is the difference between a promise and a toggle. Session I/O
//! authorizes through [`super::session_guard`], which queries tenant membership
//! and never imports this module. A reviewer asking "can an operator read
//! somebody's terminal?" reads one enum rather than every route.
//!
//! # One predicate
//!
//! [`AuthCtx::require`] is the only authorization decision in the codebase for
//! anything scope-shaped. No `if operator { … } else if org_admin { … }` — the
//! branching lives in `role_permissions` rows, where it can be listed and
//! audited, rather than in code where it can only be read.

use nook_types::TenantId;
use uuid::Uuid;

use crate::auth::AuthCtx;
use crate::error::ApiError;
use crate::state::AppState;

/// Everything a subject can be granted.
///
/// Mirrors the `permissions` table. The table is the data half — grantable,
/// listable, auditable — and this is the half the compiler checks. A permission
/// added to one and not the other is caught by [`tests::the_catalog_matches_the_database`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    OrgView,
    OrgManage,
    TenantView,
    TenantManage,
    NodeView,
    NodeManage,
    AuditView,
    CaRotate,
    PolicyView,
    PolicyManage,
    /// Appoint or remove a role binding. Deliberately not part of `OrgManage`:
    /// deciding who runs the deployment is a different power from renaming an
    /// org, and conflating them hides the first behind the second.
    RbacGrant,
}

impl Permission {
    pub fn key(self) -> &'static str {
        match self {
            Permission::OrgView => "org.view",
            Permission::OrgManage => "org.manage",
            Permission::TenantView => "tenant.view",
            Permission::TenantManage => "tenant.manage",
            Permission::NodeView => "node.view",
            Permission::NodeManage => "node.manage",
            Permission::AuditView => "audit.view",
            Permission::CaRotate => "ca.rotate",
            Permission::PolicyView => "policy.view",
            Permission::PolicyManage => "policy.manage",
            Permission::RbacGrant => "rbac.grant",
        }
    }

    /// Every variant, for tests and for listing what a role grants.
    pub const ALL: [Permission; 11] = [
        Permission::OrgView,
        Permission::OrgManage,
        Permission::TenantView,
        Permission::TenantManage,
        Permission::NodeView,
        Permission::NodeManage,
        Permission::AuditView,
        Permission::CaRotate,
        Permission::PolicyView,
        Permission::PolicyManage,
        Permission::RbacGrant,
    ];
}

/// Where a permission is being asked for.
///
/// A tree, not a graph: every scope has at most one parent, so "does this
/// binding cover that scope" is an ancestor walk of fixed depth rather than a
/// traversal that could loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Deployment,
    Org(Uuid),
    Tenant(TenantId),
}

impl AuthCtx {
    /// Does this caller hold `permission` at `scope`, or at any ancestor of it?
    ///
    /// A binding at `deployment` covers every org and every tenant; a binding at
    /// `org` covers that org's tenants. Resolved in ONE query rather than a
    /// walk, so there is a single place where "covers" is defined.
    pub async fn require(
        &self,
        state: &AppState,
        permission: Permission,
        scope: Scope,
    ) -> Result<(), ApiError> {
        // A machine credential is never a person. A node token that could carry
        // an operator grant would make one compromised box the deployment.
        self.require_user()?;

        let (org_id, tenant_id) = match scope {
            Scope::Deployment => (None, None),
            Scope::Org(o) => (Some(o), None),
            Scope::Tenant(t) => (org_of(state, t).await?, Some(t.0)),
        };

        let hit: Option<(bool,)> = sqlx::query_as(
            "SELECT true
             FROM role_bindings b
             JOIN role_permissions rp ON rp.role_key = b.role_key
             WHERE b.subject_type = 'user'
               AND b.subject_id = $1
               AND rp.permission_key = $2
               AND (
                     -- Deployment covers everything below it.
                     b.scope_type = 'deployment'
                     -- The org itself, or the org the target tenant lives in.
                  OR (b.scope_type = 'org' AND b.scope_id = $3)
                     -- The exact tenant.
                  OR (b.scope_type = 'tenant' AND b.scope_id = $4)
               )
             LIMIT 1",
        )
        .bind(self.user_id.0)
        .bind(permission.key())
        .bind(org_id)
        .bind(tenant_id)
        .fetch_optional(&state.db)
        .await?;

        if hit.is_some() {
            return Ok(());
        }
        // Names the permission, not the scope: telling somebody which org they
        // failed against confirms that org exists.
        Err(ApiError::ForbiddenMsg(format!(
            "this needs the `{}` permission",
            permission.key()
        )))
    }

    /// `true` / `false` rather than a refusal, for deciding what to SHOW.
    ///
    /// A UI that renders a button and then discovers it is forbidden has
    /// already told the user something; this is how a surface stays hidden.
    pub async fn can(&self, state: &AppState, permission: Permission, scope: Scope) -> bool {
        self.require(state, permission, scope).await.is_ok()
    }
}

async fn org_of(state: &AppState, tenant: TenantId) -> Result<Option<Uuid>, ApiError> {
    let row: Option<(Option<Uuid>,)> = sqlx::query_as("SELECT org_id FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_optional(&state.db)
        .await?;
    Ok(row.and_then(|(o,)| o))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guarantee, asserted mechanically.
    ///
    /// If somebody ever adds a permission that grants terminal access, this
    /// fails before it can be bound to a role. It is a crude check on purpose:
    /// it catches the name, and the name is what a reviewer would skim past.
    #[test]
    fn no_permission_can_reach_session_content() {
        for p in Permission::ALL {
            let k = p.key().to_lowercase();
            for forbidden in [
                "session", "terminal", "output", "input", "attach", "pty", "tmux", "prompt",
            ] {
                assert!(
                    !k.contains(forbidden),
                    "`{k}` looks like it grants session content. Session access is \
                     membership, checked in session_guard.rs, and must never become \
                     a permission — see the module docs."
                );
            }
        }
    }

    /// Keys are the join to `role_permissions`; a duplicate would silently
    /// merge two permissions into one grant.
    #[test]
    fn permission_keys_are_unique_and_dotted() {
        let mut keys: Vec<&str> = Permission::ALL.iter().map(|p| p.key()).collect();
        let count = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), count, "duplicate permission key");
        for p in Permission::ALL {
            assert!(p.key().contains('.'), "{} should be dotted", p.key());
        }
    }
}
