//! The client this instance registered for itself at an IdP (MAIN-651).
//!
//! Keyed by issuer, and free functions rather than a trait: there is one row,
//! nothing consumes it but discovery, and there is no aggregate here to name.
//!
//! The row holds no credential — see `0084_oidc_registered_client.sql` and
//! [`crate::auth::dcr`] for why a registration is always a public client.

use nook_db::{params, Db, DbPool};

use crate::error::ApiResult;

/// The id this instance was issued at `issuer`, if it has registered there.
pub async fn remembered(db: &DbPool, issuer: &str) -> ApiResult<Option<String>> {
    Ok(db
        .query_scalar_opt::<String>(
            "SELECT client_id FROM oidc_registered_client WHERE issuer = $1",
            params![issuer],
        )
        .await?)
}

/// Remember a registration.
///
/// `ON CONFLICT DO NOTHING`, so two replicas registering at once keep the row
/// that got there first rather than the one that wrote last. Either id is valid
/// at the IdP; what matters is that every replica afterwards reads the same one.
pub async fn remember(db: &DbPool, issuer: &str, client_id: &str) -> ApiResult<()> {
    db.exec(
        "INSERT INTO oidc_registered_client (issuer, client_id) VALUES ($1, $2)
         ON CONFLICT (issuer) DO NOTHING",
        params![issuer, client_id],
    )
    .await?;
    Ok(())
}
