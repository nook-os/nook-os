//! Who may touch a tenant's session content. Membership, and nothing else.
//!
//! # This module deliberately does not import `perm.rs`
//!
//! Session content — the terminal stream, the prompts, the code on screen — is
//! the one thing NookOS promises an operator can never see. A promise with a
//! toggle is not a promise, so the guarantee is structural rather than
//! configured:
//!
//! 1. There is no permission for it. [`super::perm::Permission`] has no variant
//!    a call site could name.
//! 2. This guard asks ONE question — "is this person a member of that tenant?"
//!    — against `tenant_members` and `users.tenant_id`. It never consults
//!    `role_bindings`, so no role at any scope can produce access.
//! 3. It never consults visibility policy. Policy governs *metadata*; there is
//!    no policy value that reaches this code path, because this code path does
//!    not read policy.
//!
//! An operator bound at `deployment` therefore gets 403 here exactly as a
//! stranger would, and `tests/session_isolation.rs` asserts that against every
//! session route the router exposes.
//!
//! If you are here to add "…unless the caller is an operator", the answer is
//! no. That is the feature this file exists to prevent.

use nook_types::TenantId;

use crate::auth::{AuthCtx, Principal};
use crate::error::ApiError;
use crate::state::AppState;

impl AuthCtx {
    /// May this caller read or write session content belonging to `tenant`?
    ///
    /// Membership is: your current tenant is that tenant, or you hold a row in
    /// `tenant_members` for it. A node may act on sessions in its own tenant,
    /// because a node running the session is how the bytes exist at all.
    pub async fn require_session_access(
        &self,
        state: &AppState,
        tenant: TenantId,
    ) -> Result<(), ApiError> {
        // The fast path, and the common one: this is your own tenant. Note
        // that `self.tenant_id` comes from the authenticated context and never
        // from the request, so it cannot be pointed at somebody else.
        if self.tenant_id == tenant {
            return Ok(());
        }

        // A machine credential is confined to the tenant it belongs to, full
        // stop. There is no membership table for machines and there should not
        // be: a node reaching into another tenant's sessions is one compromised
        // box becoming all of them.
        if matches!(self.principal, Principal::Node(_)) {
            return Err(refusal());
        }

        // Explicit membership. This query is the entire authorization surface
        // for session content — `role_bindings` is deliberately not joined.
        let member: Option<(bool,)> = sqlx::query_as(
            "SELECT true FROM tenant_members
             WHERE tenant_id = $1 AND principal_type = 'user' AND principal_id = $2
             LIMIT 1",
        )
        .bind(tenant)
        .bind(self.user_id.0)
        .fetch_optional(&state.db)
        .await?;

        if member.is_some() {
            Ok(())
        } else {
            Err(refusal())
        }
    }
}

/// One message for every refusal here.
///
/// Identical whether the session does not exist, belongs to another tenant, or
/// the caller is a deployment operator — because a message that distinguished
/// them would confirm that somebody else's session exists.
fn refusal() -> ApiError {
    ApiError::ForbiddenMsg(
        "session content belongs to the tenant that owns it. Operator and \
         administrative roles do not grant access to terminals, prompts or code."
            .into(),
    )
}

#[cfg(test)]
mod tests {
    /// The guarantee is only structural if this file stays free of scope
    /// resolution. Asserted against the source text, because the failure mode
    /// is somebody adding one plausible line during a refactor.
    #[test]
    fn this_guard_never_consults_roles_or_policy() {
        let src = include_str!("session_guard.rs");
        // CODE only. The module docs above discuss the permission catalog at
        // length in order to explain why it is not used here, and a check that
        // matched prose would fail on its own explanation.
        let code: String = src
            .split("mod tests")
            .next()
            .expect("module body")
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        for forbidden in [
            "role_bindings",
            "role_permissions",
            "visibility_policy",
            "perm::",
            "Permission",
        ] {
            assert!(
                !code.contains(forbidden),
                "session_guard.rs must not reference `{forbidden}` — session access is \
                 membership, not a permission. See the module docs for why."
            );
        }
        // And it must still actually check membership, or the test above
        // passes trivially on an empty file.
        assert!(
            code.contains("tenant_members"),
            "the guard must query membership"
        );
    }
}
