//! Task reads: enrichment, the filtered pick query, and safe claiming.
//!
//! Three fields on `TaskItem` are computed rather than stored — `key`, `url`
//! and `labels`. Storing `key` would let it disagree with the two columns it is
//! made of; storing `url` would bake this deployment's hostname into rows that
//! outlive it. So every path that returns a task goes through [`enrich`], and
//! the cost of that is two extra queries for a whole board rather than two per
//! task.

use nook_db::{params, Db, DbPool, Postgres, TypeMapping};
use nook_types::*;
use std::collections::HashMap;

use crate::error::{ApiError, ApiResult};

/// Whether `viewer` may see or act on `task` — the ONE task-visibility predicate
/// (MAIN-76). A `private` card is confined to its creator and its assignee;
/// `team` and `org` cards are visible to the whole tenant (org's cross-tenant
/// reach is a later ticket, so today it matches team). Deliberately NOT a role
/// or policy check (NG-3): visibility is a per-task owner predicate, and this is
/// the single definition every read/claim path routes through.
pub fn visible_to(task: &TaskItem, viewer: UserId) -> bool {
    visible_by_cols(
        &task.visibility,
        task.created_by,
        task.assignee_user_id,
        viewer,
    )
}

/// The same visibility rule as [`visible_to`], but over the loose columns
/// rather than a whole `TaskItem`. `enrich` loads only these three fields for a
/// child's *parent* (it never holds the parent as a full task), and gates the
/// derived `parent_key` on this so a private epic's key never leaks onto a
/// child a non-owner can see (MAIN-86). Kept identical to `visible_to` by
/// construction — that predicate now routes through it too.
pub fn visible_by_cols(
    visibility: &str,
    created_by: Option<UserId>,
    assignee: Option<UserId>,
    viewer: UserId,
) -> bool {
    visibility != "private" || owns_fields(created_by, assignee, viewer)
}

/// The card's owner set — its creator or its assignee. The single definition
/// shared by the visibility read predicate above AND the visibility-change gate
/// (MAIN-85), so "who owns this card" can never come to mean two different
/// things in the two places it decides access.
pub fn owns(task: &TaskItem, user: UserId) -> bool {
    owns_fields(task.created_by, task.assignee_user_id, user)
}

/// The owner test over just the two loose columns it needs, so the whole-task
/// [`owns`] (MAIN-85's visibility-change gate) and the loose-column
/// [`visible_by_cols`] (MAIN-86's enrich redaction) share ONE definition of
/// "who owns this card" — the two can never drift.
fn owns_fields(created_by: Option<UserId>, assignee: Option<UserId>, user: UserId) -> bool {
    created_by == Some(user) || assignee == Some(user)
}

/// The task title safe to put in a TENANT-FACING event payload or notification
/// (MAIN-76 AC-3/AC-4). A private card's title must not reach the tenant
/// activity feed, the live event websocket, or a notification channel — those
/// broadcast to the whole tenant, and a private card is confined to its owner.
/// Returns `None` for a private card (the payload then carries no title) and the
/// real title otherwise.
pub fn public_title(task: &TaskItem) -> Option<&str> {
    (task.visibility != "private").then_some(task.title.as_str())
}

/// Fill in `key`, `url` and `labels` for a batch of tasks.
///
/// Batched deliberately: the board endpoint returns every task at once, and an
/// N+1 there is the difference between one render and two hundred round trips.
/// Two queries regardless of how many tasks come in.
pub async fn enrich(
    db: &DbPool,
    base_url: &str,
    viewer: UserId,
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
    let keys: HashMap<uuid::Uuid, Option<String>> = db
        .query_all::<(uuid::Uuid, Option<String>)>(
            "SELECT id, key FROM boards WHERE id = ANY($1)",
            params![&board_ids[..]],
        )
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
    )> = db
        .query_all(
            "SELECT tl.task_id, l.id, l.tenant_id, l.name, l.color, l.created_at
             FROM task_labels tl
             JOIN labels l ON l.id = tl.label_id
             WHERE tl.task_id = ANY($1)
             ORDER BY l.name",
            params![&ids[..]],
        )
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

    // Parent number AND visibility, one query for the whole batch, so an epic's
    // children can show "under NOOK-7" without an N+1 (MAIN-81). The number
    // builds the `BOARD-N` key; the visibility/owner columns decide whether THIS
    // viewer may see that key at all — a private epic's key must not leak onto a
    // child a non-owner can see (MAIN-86). The parent is on the same board as
    // the child, so the child's board key builds the parent key too.
    let parent_ids: Vec<uuid::Uuid> = {
        let mut v: Vec<uuid::Uuid> = tasks
            .iter()
            .filter_map(|t| t.parent_task_id.map(|p| p.0))
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    // (number, visibility, created_by, assignee) — enough to build the key AND
    // decide whether this viewer may see it.
    type ParentInfo = (Option<i32>, String, Option<UserId>, Option<UserId>);
    let parents: HashMap<uuid::Uuid, ParentInfo> = if parent_ids.is_empty() {
        HashMap::new()
    } else {
        db.query_all::<(
            uuid::Uuid,
            Option<i32>,
            String,
            Option<UserId>,
            Option<UserId>,
        )>(
            "SELECT id, number, visibility, created_by, assignee_user_id
             FROM tasks WHERE id = ANY($1)",
            params![&parent_ids[..]],
        )
        .await?
        .into_iter()
        .map(|(id, number, visibility, created_by, assignee)| {
            (id, (number, visibility, created_by, assignee))
        })
        .collect()
    };

    let base = base_url.trim_end_matches('/');
    for t in &mut tasks {
        t.labels = by_task.remove(&t.id.0).unwrap_or_default();
        t.key = match (keys.get(&t.board_id.0).and_then(|k| k.clone()), t.number) {
            (Some(k), Some(n)) => Some(format!("{k}-{n}")),
            _ => None,
        };
        t.parent_key = match (
            t.parent_task_id,
            keys.get(&t.board_id.0).and_then(|k| k.clone()),
        ) {
            (Some(p), Some(board_key)) => match parents.get(&p.0) {
                // A private epic's key is blanked for a viewer who is neither its
                // creator nor its assignee: the child renders parentless to them
                // rather than exposing a card they cannot open (MAIN-86 AC-3).
                Some((number, visibility, created_by, assignee))
                    if visible_by_cols(visibility, *created_by, *assignee, viewer) =>
                {
                    number.map(|n| format!("{board_key}-{n}"))
                }
                _ => None,
            },
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
    db: &DbPool,
    base_url: &str,
    viewer: UserId,
    task: TaskItem,
) -> Result<TaskItem, sqlx::Error> {
    Ok(enrich(db, base_url, viewer, vec![task])
        .await?
        .pop()
        .expect("enrich preserves length"))
}

/// Resolve a task by uuid **or** human key (`NOOK-42`, case-insensitively).
///
/// Agents are told keys, not uuids — `Closes NOOK-42` is the join between a PR
/// and its issue — so every task-addressed endpoint accepts both. Tenant-scoped
/// either way: a uuid is not an authorisation.
pub async fn resolve_id(db: &DbPool, tenant: TenantId, ident: &str) -> ApiResult<TaskId> {
    if let Ok(uuid) = ident.parse::<uuid::Uuid>() {
        let found: Option<TaskId> = db
            .query_scalar_opt(
                "SELECT id FROM tasks WHERE id = $1 AND tenant_id = $2",
                params![uuid, tenant],
            )
            .await?;
        return found.ok_or(ApiError::NotFound);
    }

    let (key, number) = split_key(ident).ok_or_else(|| {
        ApiError::BadRequest(format!(
            "{ident:?} is neither a task id nor a key like NOOK-42"
        ))
    })?;
    let found: Option<TaskId> = db
        .query_scalar_opt(
            "SELECT t.id FROM tasks t
         JOIN boards b ON b.id = t.board_id
         WHERE t.tenant_id = $1 AND upper(b.key) = upper($2) AND t.number = $3",
            params![tenant, &key, number],
        )
        .await?;
    found.ok_or(ApiError::NotFound)
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
pub async fn column_of_type(db: &DbPool, board: BoardId, column_type: &str) -> ApiResult<ColumnId> {
    const TYPES: [&str; 6] = [
        "backlog",
        "unstarted",
        "started",
        "review",
        "completed",
        "canceled",
    ];
    if !TYPES.contains(&column_type) {
        return Err(ApiError::BadRequest(format!(
            "{column_type:?} is not a column type — expected one of {}",
            TYPES.join(", ")
        )));
    }
    let found: Option<ColumnId> = db
        .query_scalar_opt(
            "SELECT id FROM board_columns WHERE board_id = $1 AND type = $2
         ORDER BY position LIMIT 1",
            params![board, column_type],
        )
        .await?;

    // 409, not 500 and not 404: the request was well formed and the board is
    // real, but this board has no column meaning that. Naming the missing type
    // is the difference between a fixable message and a mystery.
    found.ok_or_else(|| {
        ApiError::Conflict(format!(
            "this board has no {column_type:?} column — add one, or give an explicit column_id"
        ))
    })
}

/// The `KanbanProvider` name that owns a task, via its board. Moved verbatim out
/// of `mcp_backend` (MAIN-245); resolving the name to an instance stays there,
/// because the registry is orchestration rather than data.
pub async fn board_provider_for_task(
    db: &DbPool,
    tenant: TenantId,
    task_id: TaskId,
) -> ApiResult<Option<String>> {
    Ok(db
        .query_scalar_opt(
            "SELECT b.provider FROM boards b
             JOIN tasks t ON t.board_id = b.id
             WHERE t.id = $1 AND t.tenant_id = $2",
            params![task_id, tenant],
        )
        .await?)
}

/// One task row by id, scoped to its tenant. Moved verbatim out of
/// `mcp_backend` (MAIN-245).
pub async fn get_row(db: &DbPool, tenant: TenantId, id: TaskId) -> ApiResult<Option<TaskItem>> {
    Ok(db
        .query_opt(
            "SELECT * FROM tasks WHERE id = $1 AND tenant_id = $2",
            params![id, tenant],
        )
        .await?)
}

/// Drop a task's assignee. Moved verbatim out of `mcp_backend` (MAIN-245).
pub async fn clear_assignee(db: &DbPool, tenant: TenantId, id: TaskId) -> ApiResult<TaskItem> {
    Ok(db
        .query_one(
            &format!(
                "UPDATE tasks SET assignee_user_id = NULL, updated_at = {now}
             WHERE id = $1 AND tenant_id = $2 RETURNING *",
                now = Postgres.now()
            ),
            params![id, tenant],
        )
        .await?)
}

/// Set a task's priority. Moved verbatim out of `mcp_backend` (MAIN-245); the
/// clamp stays at the call site, where it always was.
pub async fn set_priority_row(
    db: &DbPool,
    tenant: TenantId,
    id: TaskId,
    priority: i32,
) -> ApiResult<TaskItem> {
    Ok(db
        .query_one(
            &format!(
                "UPDATE tasks SET priority = $3, updated_at = {now}
             WHERE id = $1 AND tenant_id = $2 RETURNING *",
                now = Postgres.now()
            ),
            params![id, tenant, priority],
        )
        .await?)
}

/// Record an agent-authored comment. Moved verbatim out of `mcp_backend`
/// (MAIN-245).
pub async fn insert_agent_comment(
    db: &DbPool,
    tenant: TenantId,
    task_id: TaskId,
    author_id: uuid::Uuid,
    author_name: &str,
    body_md: &str,
) -> ApiResult<TaskComment> {
    Ok(db
        .query_one(
            "INSERT INTO task_comments (id, tenant_id, task_id, author_type, author_id, author_name, body_md)
             VALUES ($1, $2, $3, 'agent', $4, $5, $6)
             RETURNING id, tenant_id, task_id, author_type, author_id, author_name,
                       body_md, created_at, updated_at",
            params![uuid::Uuid::now_v7(), tenant, task_id, author_id, author_name, body_md],
        )
        .await?)
}

/// Get-or-create a label by name and attach it to a task. Moved verbatim out of
/// `mcp_backend` (MAIN-245); the two statements travelled together because the
/// upsert exists only to feed the attach.
pub async fn attach_label(
    db: &DbPool,
    tenant: TenantId,
    task_id: TaskId,
    name: &str,
) -> ApiResult<()> {
    let label_id: uuid::Uuid = db
        .query_scalar(
            "INSERT INTO labels (id, tenant_id, name) VALUES ($1, $2, $3)
             ON CONFLICT (tenant_id, name) DO UPDATE SET name = EXCLUDED.name RETURNING id",
            params![uuid::Uuid::now_v7(), tenant, name],
        )
        .await?;
    db.exec(
        "INSERT INTO task_labels (task_id, label_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        params![task_id, label_id],
    )
    .await?;
    Ok(())
}

/// Detach a label from a task by name. Moved verbatim out of `mcp_backend`
/// (MAIN-245).
pub async fn detach_label(
    db: &DbPool,
    tenant: TenantId,
    task_id: TaskId,
    name: &str,
) -> ApiResult<()> {
    db.exec(
        "DELETE FROM task_labels tl USING labels l
             WHERE tl.label_id = l.id AND tl.task_id = $1
               AND l.tenant_id = $2 AND l.name = $3",
        params![task_id, tenant, name],
    )
    .await?;
    Ok(())
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
