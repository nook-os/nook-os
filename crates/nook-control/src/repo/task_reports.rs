//! What automation has written on a card (MAIN-603).
//!
//! One table, one natural key — `(task_id, key)` — and every write is an upsert
//! on it. That is the whole of AC-1: a producer re-running does not decide
//! whether to insert or update, because there is no second row to create.
//!
//! **Every method takes the tenant and matches on it.** A task id is not an
//! authorisation, and the route above resolves visibility separately (AC-8);
//! this layer's job is that a uuid from another tenant finds nothing.

use async_trait::async_trait;
use nook_db::{dialect::type_mapping, params, Db, DbPool};
use nook_types::*;
use uuid::Uuid;

use crate::error::ApiResult;

/// A report to write. The caller has already validated the key and the limits —
/// this layer stores what it is given.
#[derive(Debug, Clone)]
pub struct NewTaskReport {
    pub tenant: TenantId,
    pub task: TaskId,
    pub key: String,
    pub title: String,
    pub body_md: String,
    pub author_type: String,
    pub author_id: Option<Uuid>,
    pub author_name: String,
}

/// The columns the API type is built from, spelled once because three queries
/// select exactly it.
const REPORT_COLUMNS: &str = "id, task_id, key, title, body_md, author_type, \
                              author_id, author_name, created_at, updated_at";

#[async_trait]
pub trait TaskReportRepository: Send + Sync {
    /// Create or replace the report at this key, and hand back what is now
    /// stored.
    async fn put(&self, new: NewTaskReport) -> ApiResult<TaskReport>;

    /// One card's reports, most recently updated first (AC-5).
    async fn list(&self, tenant: TenantId, task: TaskId) -> ApiResult<Vec<TaskReport>>;

    /// Remove one key. `false` when there was nothing there, which the route
    /// turns into a 404 rather than a silent success.
    async fn delete(&self, tenant: TenantId, task: TaskId, key: &str) -> ApiResult<bool>;
}

pub struct DbTaskReportRepository {
    db: DbPool,
}

impl DbTaskReportRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl TaskReportRepository for DbTaskReportRepository {
    async fn put(&self, new: NewTaskReport) -> ApiResult<TaskReport> {
        // `created_at` is deliberately absent from the update list: the first
        // write is when this key appeared, and a re-run must move `updated_at`
        // alone or AC-10's "visibly stale" reads the wrong clock.
        //
        // Two statements rather than one `RETURNING`, as the attachment join
        // does: a data-modifying CTE is Postgres only and this runs on both
        // engines. The row is read back by its natural key, not by the id we
        // offered — on a conflict that id was never used.
        self.db
            .exec(
                &format!(
                    "INSERT INTO task_reports
                        (id, tenant_id, task_id, key, title, body_md,
                         author_type, author_id, author_name)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                     ON CONFLICT (task_id, key) DO UPDATE
                       SET title = EXCLUDED.title,
                           body_md = EXCLUDED.body_md,
                           author_type = EXCLUDED.author_type,
                           author_id = EXCLUDED.author_id,
                           author_name = EXCLUDED.author_name,
                           updated_at = {now}",
                    now = type_mapping(self.db.engine()).now(),
                ),
                params![
                    Uuid::now_v7(),
                    new.tenant,
                    new.task,
                    &new.key,
                    &new.title,
                    &new.body_md,
                    &new.author_type,
                    new.author_id,
                    &new.author_name
                ],
            )
            .await?;

        Ok(self
            .db
            .query_one(
                &format!(
                    "SELECT {REPORT_COLUMNS} FROM task_reports
                      WHERE tenant_id = $1 AND task_id = $2 AND key = $3"
                ),
                params![new.tenant, new.task, &new.key],
            )
            .await?)
    }

    async fn list(&self, tenant: TenantId, task: TaskId) -> ApiResult<Vec<TaskReport>> {
        // `id` breaks the tie so the order is total: two reports written in the
        // same millisecond — which is SQLite's whole clock resolution — must not
        // swap places between two reads of the same card.
        Ok(self
            .db
            .query_all(
                &format!(
                    "SELECT {REPORT_COLUMNS} FROM task_reports
                      WHERE tenant_id = $1 AND task_id = $2
                      ORDER BY updated_at DESC, id DESC"
                ),
                params![tenant, task],
            )
            .await?)
    }

    async fn delete(&self, tenant: TenantId, task: TaskId, key: &str) -> ApiResult<bool> {
        Ok(self
            .db
            .exec(
                "DELETE FROM task_reports WHERE tenant_id = $1 AND task_id = $2 AND key = $3",
                params![tenant, task, key],
            )
            .await?
            > 0)
    }
}
