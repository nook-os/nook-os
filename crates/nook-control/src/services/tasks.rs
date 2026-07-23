//! Task reads: enrichment, the filtered pick query, and safe claiming.
//!
//! Three fields on `TaskItem` are computed rather than stored — `key`, `url`
//! and `labels`. Storing `key` would let it disagree with the two columns it is
//! made of; storing `url` would bake this deployment's hostname into rows that
//! outlive it. So every path that returns a task goes through [`enrich`], and
//! the cost of that is two extra queries for a whole board rather than two per
//! task.

use nook_types::*;
use sqlx::PgPool;
use std::collections::HashMap;

use crate::error::{ApiError, ApiResult};

/// Fill in `key`, `url` and `labels` for a batch of tasks.
///
/// Batched deliberately: the board endpoint returns every task at once, and an
/// N+1 there is the difference between one render and two hundred round trips.
/// Two queries regardless of how many tasks come in.
pub async fn enrich(
    db: &PgPool,
    base_url: &str,
    mut tasks: Vec<TaskItem>,
) -> Result<Vec<TaskItem>, sqlx::Error> {
    if tasks.is_empty() {
        return Ok(tasks);
    }
    let ids: Vec<uuid::Uuid> = tasks.iter().map(|t| t.id.0).collect();

    // Board keys, one row per board rather than one per task.
    let board_ids: Vec<uuid::Uuid> = {
        let mut v: Vec<uuid::Uuid> = tasks.iter().map(|t| t.board_id.0).collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    let keys: HashMap<uuid::Uuid, Option<String>> =
        sqlx::query_as::<_, (uuid::Uuid, Option<String>)>(
            "SELECT id, key FROM boards WHERE id = ANY($1)",
        )
        .bind(&board_ids)
        .fetch_all(db)
        .await?
        .into_iter()
        .collect();

    let label_rows: Vec<(
        uuid::Uuid,
        uuid::Uuid,
        TenantId,
        String,
        String,
        chrono::DateTime<chrono::Utc>,
    )> = sqlx::query_as(
        "SELECT tl.task_id, l.id, l.tenant_id, l.name, l.color, l.created_at
             FROM task_labels tl
             JOIN labels l ON l.id = tl.label_id
             WHERE tl.task_id = ANY($1)
             ORDER BY l.name",
    )
    .bind(&ids)
    .fetch_all(db)
    .await?;

    let mut by_task: HashMap<uuid::Uuid, Vec<Label>> = HashMap::new();
    for (task_id, id, tenant_id, name, color, created_at) in label_rows {
        by_task.entry(task_id).or_default().push(Label {
            id,
            tenant_id,
            name,
            color,
            created_at,
        });
    }

    let base = base_url.trim_end_matches('/');
    for t in &mut tasks {
        t.labels = by_task.remove(&t.id.0).unwrap_or_default();
        t.key = match (keys.get(&t.board_id.0).and_then(|k| k.clone()), t.number) {
            (Some(k), Some(n)) => Some(format!("{k}-{n}")),
            _ => None,
        };
        // Deep link by key where there is one, else by id — a task created
        // before keys existed still needs somewhere to point.
        t.url = Some(match &t.key {
            Some(k) => format!("{base}/board?task={k}"),
            None => format!("{base}/board?task={}", t.id),
        });
    }
    Ok(tasks)
}

/// One task, enriched.
pub async fn enrich_one(
    db: &PgPool,
    base_url: &str,
    task: TaskItem,
) -> Result<TaskItem, sqlx::Error> {
    Ok(enrich(db, base_url, vec![task])
        .await?
        .pop()
        .expect("enrich preserves length"))
}

/// Resolve a task by uuid **or** human key (`NOOK-42`, case-insensitively).
///
/// Agents are told keys, not uuids — `Closes NOOK-42` is the join between a PR
/// and its issue — so every task-addressed endpoint accepts both. Tenant-scoped
/// either way: a uuid is not an authorisation.
pub async fn resolve_id(db: &PgPool, tenant: TenantId, ident: &str) -> ApiResult<TaskId> {
    if let Ok(uuid) = ident.parse::<uuid::Uuid>() {
        let found: Option<(TaskId,)> =
            sqlx::query_as("SELECT id FROM tasks WHERE id = $1 AND tenant_id = $2")
                .bind(uuid)
                .bind(tenant)
                .fetch_optional(db)
                .await?;
        return found.map(|r| r.0).ok_or(ApiError::NotFound);
    }

    let (key, number) = split_key(ident).ok_or_else(|| {
        ApiError::BadRequest(format!(
            "{ident:?} is neither a task id nor a key like NOOK-42"
        ))
    })?;
    let found: Option<(TaskId,)> = sqlx::query_as(
        "SELECT t.id FROM tasks t
         JOIN boards b ON b.id = t.board_id
         WHERE t.tenant_id = $1 AND upper(b.key) = upper($2) AND t.number = $3",
    )
    .bind(tenant)
    .bind(&key)
    .bind(number)
    .fetch_optional(db)
    .await?;
    found.map(|r| r.0).ok_or(ApiError::NotFound)
}

/// `NOOK-42` → `("ENG", 42)`.
///
/// Splits at the LAST hyphen, so a board key containing one (`WEB-UI-7`) still
/// resolves. Board keys are generated without hyphens, but a human may set one
/// explicitly and it should not silently mean a different task.
fn split_key(ident: &str) -> Option<(String, i32)> {
    let (key, num) = ident.trim().rsplit_once('-')?;
    if key.is_empty() {
        return None;
    }
    let n: i32 = num.parse().ok()?;
    Some((key.to_string(), n))
}

/// Resolve a column TYPE to the column that means it on a given board.
///
/// Lowest position wins when a board has two of a type — a deliberate choice
/// rather than an error, because a board with "In Review" and "In Progress"
/// both marked `started` is a reasonable thing for a human to build.
pub async fn column_of_type(db: &PgPool, board: BoardId, column_type: &str) -> ApiResult<ColumnId> {
    const TYPES: [&str; 5] = ["backlog", "unstarted", "started", "completed", "canceled"];
    if !TYPES.contains(&column_type) {
        return Err(ApiError::BadRequest(format!(
            "{column_type:?} is not a column type — expected one of {}",
            TYPES.join(", ")
        )));
    }
    let found: Option<(ColumnId,)> = sqlx::query_as(
        "SELECT id FROM board_columns WHERE board_id = $1 AND type = $2
         ORDER BY position LIMIT 1",
    )
    .bind(board)
    .bind(column_type)
    .fetch_optional(db)
    .await?;

    // 409, not 500 and not 404: the request was well formed and the board is
    // real, but this board has no column meaning that. Naming the missing type
    // is the difference between a fixable message and a mystery.
    found.map(|r| r.0).ok_or_else(|| {
        ApiError::Conflict(format!(
            "this board has no {column_type:?} column — add one, or give an explicit column_id"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Keys go into PR bodies and branch names, where a human types them. A
    /// key that parses differently from how it was printed points at the wrong
    /// task, silently.
    #[test]
    fn a_human_key_splits_at_the_last_hyphen() {
        assert_eq!(split_key("NOOK-42"), Some(("NOOK".into(), 42)));
        assert_eq!(split_key("  NOOK-42  "), Some(("NOOK".into(), 42)));
        // A hyphenated board key still resolves, and to the right number.
        assert_eq!(split_key("WEB-UI-7"), Some(("WEB-UI".into(), 7)));

        for bad in ["ENG", "-42", "ENG-", "ENG-x", "", "42"] {
            assert_eq!(split_key(bad), None, "must refuse {bad:?}");
        }
    }
}
