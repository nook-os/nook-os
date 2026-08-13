//! What GitHub delivered, recorded (MAIN-554).
//!
//! One table, one write, and no reads yet: this card builds the door and
//! records what came through it (NG-1) — children 2-5 grow the handlers that
//! read these rows. A repository rather than a query in the route because that
//! is where SQL lives in this crate, and because the children will extend this
//! trait rather than scatter selects across their handlers.

use async_trait::async_trait;
use nook_db::{params, Db, DbPool};
use nook_types::*;
use uuid::Uuid;

use crate::error::ApiResult;

/// A delivery to record. `status` is [`crate::services::forge_webhook`]'s
/// vocabulary, and `error` is set only alongside `error`.
#[derive(Debug, Clone)]
pub struct NewDelivery {
    pub tenant: TenantId,
    pub workspace: WorkspaceId,
    pub delivery_id: String,
    pub event: String,
    pub action: Option<String>,
    /// `""` when the delivery names no repository — see the migration.
    pub repo_full_name: String,
    pub payload: serde_json::Value,
    pub status: &'static str,
    pub error: Option<String>,
}

#[async_trait]
pub trait ForgeDeliveryRepository: Send + Sync {
    /// `true` when the row was written, `false` when this delivery was already
    /// here.
    ///
    /// A redelivery is the SAME delivery — GitHub's UI offers the button, and
    /// its at-least-once retry uses it on its own — so the second one yields to
    /// the unique index rather than erroring. The caller turns the `false` into
    /// the 200 that tells an operator it was recognised (AC-2).
    async fn record(&self, new: NewDelivery) -> ApiResult<bool>;
}

pub struct DbForgeDeliveryRepository {
    db: DbPool,
}

impl DbForgeDeliveryRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl ForgeDeliveryRepository for DbForgeDeliveryRepository {
    async fn record(&self, new: NewDelivery) -> ApiResult<bool> {
        Ok(self
            .db
            .exec(
                "INSERT INTO forge_deliveries
                    (id, tenant_id, workspace_id, delivery_id, event, action,
                     repo_full_name, payload, status, error)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                 ON CONFLICT (workspace_id, delivery_id) DO NOTHING",
                params![
                    Uuid::now_v7(),
                    new.tenant,
                    new.workspace,
                    new.delivery_id,
                    new.event,
                    new.action,
                    new.repo_full_name,
                    new.payload,
                    new.status.to_string(),
                    new.error
                ],
            )
            .await?
            > 0)
    }
}
