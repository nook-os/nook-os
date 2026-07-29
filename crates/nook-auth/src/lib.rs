//! Shared NookOS session/token validation.
//!
//! One implementation of "who is this caller" — resolving a `nook_session`
//! cookie or a `nook_user_` bearer token to a `(user_id, tenant_id)` — used by
//! BOTH the control plane and `nook-chat`. Chat reuses NookOS auth rather than
//! inventing a second login (MAIN-48 AC-4), and putting the queries here rather
//! than copying them means the two services cannot drift: a change to how a
//! session is validated is a change in one place.
//!
//! The queries take a `&DbPool` and nothing else — no `AppState`, no framework —
//! so any service holding the shared Postgres can call them. The credential
//! plumbing (reading the cookie, splitting the `Authorization` header, the
//! node-cert path) stays in each service; this is only the database check that
//! must be identical.

use nook_db::{params, Db, DbPool};
use uuid::Uuid;

/// The browser session cookie. Its value IS the raw session id (server-side
/// session row is the security boundary, not cookie crypto).
pub const SESSION_COOKIE: &str = "nook_session";
/// Personal access tokens carry this prefix; anything else on the `Bearer`
/// header is a node token, which this crate does not resolve.
pub const USER_TOKEN_PREFIX: &str = "nook_user_";
/// WebSockets cannot send an `Authorization` header, so the bearer token rides
/// in the `sec-websocket-protocol` header beside this marker.
pub const WS_BEARER_PROTOCOL: &str = "nook.bearer";

/// A validated caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolved {
    /// The credential row's id — the `sessions_auth` id for a cookie session,
    /// the `user_tokens` id for a bearer token. Anything keyed per session can
    /// hold it; for a token it is a stable, unique, revocable handle.
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub tenant_id: Uuid,
    /// True for a cookie session (a tenant switch can move it); false for a
    /// bearer token (bound to the tenant it was minted for).
    pub cookie_session: bool,
}

/// Why validation did not yield a caller.
///
/// `Unauthorized` (→ 401) and `Forbidden` (→ 403) are kept distinct: a live
/// session whose tenant grant was revoked is a different answer from no session
/// at all, and collapsing them would hide a revoked grant behind "please log in"
/// (MAIN-26). A database error is surfaced separately so a transient failure is
/// a 500, not a spurious 401.
#[derive(Debug)]
pub enum AuthError {
    Unauthorized,
    Forbidden,
    Db(sqlx::Error),
}

impl From<sqlx::Error> for AuthError {
    fn from(e: sqlx::Error) -> Self {
        AuthError::Db(e)
    }
}

/// The canonical token hash. Identical on both services, so a token the control
/// plane minted (storing only this hash) validates in chat.
pub fn hash_token(token: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

/// Resolve a `nook_session` cookie value (the plaintext session id).
///
/// One query, folding the membership check into the row so 401 (no/expired
/// session) stays distinct from 403 (grant revoked) — the exact shape the
/// control plane's `AuthCtx` used before this was extracted.
pub async fn resolve_session(pool: &DbPool, session_id: Uuid) -> Result<Resolved, AuthError> {
    let (resolved, is_member) = resolve_session_identity(pool, session_id).await?;
    // The membership check is what separates 401 (no/expired session) from 403
    // (session fine, but no grant on its tenant) — see `AuthError`.
    if !is_member {
        return Err(AuthError::Forbidden);
    }
    Ok(resolved)
}

/// Resolve a session to its caller WITHOUT requiring tenant membership, also
/// reporting whether they are a member of the session's tenant.
///
/// For the narrow set of routes a signed-in person may reach before they belong
/// to any tenant — today only accepting an invite (MAIN-98): a local invitee
/// registers, verifies, and signs in with a real session but no `tenant_members`
/// row, then accepts. Every ordinary tenant-scoped route stays on
/// [`resolve_session`], which still 403s a non-member; this is the deliberate,
/// single exception, and it returns `is_member` so the caller can still tell the
/// difference rather than being handed a bare identity.
pub async fn resolve_session_identity(
    pool: &DbPool,
    session_id: Uuid,
) -> Result<(Resolved, bool), AuthError> {
    let row: Option<(Uuid, Uuid, bool)> = pool
        .query_opt(
            "SELECT sa.user_id, sa.tenant_id,
                EXISTS(SELECT 1 FROM tenant_members m
                       WHERE m.tenant_id = sa.tenant_id
                         AND m.principal_type = 'user'
                         AND m.principal_id = sa.user_id) AS is_member
         FROM sessions_auth sa
         WHERE sa.id = $1 AND sa.expires_at > now()",
            params![session_id],
        )
        .await?;

    let (user_id, tenant_id, is_member) = row.ok_or(AuthError::Unauthorized)?;
    Ok((
        Resolved {
            session_id,
            user_id,
            tenant_id,
            cookie_session: true,
        },
        is_member,
    ))
}

/// Resolve a `nook_user_` bearer token, refreshing its last-used timestamp.
pub async fn resolve_bearer(pool: &DbPool, token: &str) -> Result<Resolved, AuthError> {
    let hash = hash_token(token);
    let row: Option<(Uuid, Uuid, Uuid)> = pool
        .query_opt(
            "SELECT id, user_id, tenant_id FROM user_tokens
         WHERE token_hash = $1 AND (expires_at IS NULL OR expires_at > now())",
            params![hash],
        )
        .await?;

    let (token_id, user_id, tenant_id) = row.ok_or(AuthError::Unauthorized)?;
    // Best-effort touch — a failed update must not fail the request.
    let _ = pool
        .exec(
            "UPDATE user_tokens SET last_used_at = now() WHERE id = $1",
            params![token_id],
        )
        .await;
    Ok(Resolved {
        session_id: token_id,
        user_id,
        tenant_id,
        cookie_session: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_sha256_hex() {
        // A known SHA-256 vector: this exact mapping is the contract the control
        // plane relies on when it stores only `hash_token(token)`.
        assert_eq!(
            hash_token("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(hash_token("x").len(), 64, "sha256 hex is 64 chars");
    }
}
