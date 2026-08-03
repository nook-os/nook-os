//! Who may touch a session's content. Membership of the tenant, AND ownership
//! of the machine it runs on.
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
//!
//! # Membership was not enough
//!
//! Found in prod, 2026-08-03: a second OWNER of a tenant attached to sessions
//! running on someone else's machines. Membership was the whole gate, so every
//! member of a tenant could read, type into and kill any terminal in it — the
//! promise above held against operators and strangers and not against the person
//! at the next desk.
//!
//! So content now also requires that the caller's PERSON owns the node. Owners
//! keep tenant-wide session METADATA (capacity and audit, MAIN-133); this is the
//! line that makes "session content stays private regardless" true rather than
//! stated. A terminal belongs to the machine's owner, and a role — including
//! `owner` — is not a way in.
//!
//! Note what this is NOT: ownership is a fact about the node row, not a
//! permission and not a policy. It can only NARROW. The structural test below
//! still forbids every route into `role_bindings` or `Permission`, because
//! widening is the failure this file exists to prevent and narrowing is not.
//!
//! Shared machines are deliberately NOT an exception here, unlike
//! `require_person_may_use_node`. Sharing a node lets the team RUN work on it;
//! it does not hand them the screens of work already running.

use nook_types::TenantId;

use crate::auth::{AuthCtx, Principal};
use crate::error::ApiError;
use crate::state::AppState;

impl AuthCtx {
    /// May this caller read or write the content of a session in `tenant`,
    /// running on `node`?
    ///
    /// Two gates, both required for a person. Membership is: your current tenant
    /// is that tenant, or you hold a row in `tenant_members` for it. Ownership
    /// is: `nodes.owner_person_id` is your person. A node credential passes on
    /// membership alone — a node running the session is how the bytes exist at
    /// all, and confining it to its own tenant is the check that matters there.
    pub async fn require_session_access(
        &self,
        state: &AppState,
        tenant: TenantId,
        node: nook_types::NodeId,
    ) -> Result<(), ApiError> {
        // TENANT first. Same tenant, or an explicit `tenant_members` row —
        // note `self.tenant_id` comes from the authenticated context and never
        // from the request, so it cannot be pointed at somebody else.
        //
        // This used to `return Ok(())` on the same-tenant match, which is what
        // let a co-owner attach to a colleague's terminals: the fast path
        // answered before anything asked whose machine it was. It now only
        // ESTABLISHES the tenant; the ownership gate below still has to pass.
        let same_tenant = self.tenant_id == tenant;

        // A machine credential is confined to the tenant it belongs to, full
        // stop. There is no membership table for machines and there should not
        // be: a node reaching into another tenant's sessions is one compromised
        // box becoming all of them.
        if matches!(self.principal, Principal::Node(_)) {
            return if same_tenant { Ok(()) } else { Err(refusal()) };
        }

        // Explicit membership. This query is the entire authorization surface
        // for session content — `role_bindings` is deliberately not joined.
        let member = same_tenant
            || state
                .identity
                .has_active_membership(self.user_id, tenant)
                .await?;

        if !member {
            return Err(refusal());
        }
        self.require_node_owner(state, node).await
    }
}

/// One message for every refusal here.
///
/// Identical whether the session does not exist, belongs to another tenant, or
/// the caller is a deployment operator — because a message that distinguished
/// them would confirm that somebody else's session exists.
fn refusal() -> ApiError {
    ApiError::ForbiddenMsg(
        "terminals belong to the person who owns the machine. Tenant membership, \
         operator and administrative roles — including `owner` — do not grant \
         access to terminals, prompts or code."
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
        //
        // This looked for the literal `tenant_members` until MAIN-246 moved the
        // query behind `IdentityRepository`. The intent is unchanged — the guard
        // must consult membership and nothing else — so it now names the call
        // that does it. Weakening this to "contains something" would give up the
        // only thing standing between a refactor and a guard that checks nothing.
        assert!(
            code.contains("has_active_membership"),
            "the guard must check membership"
        );
    }
}
