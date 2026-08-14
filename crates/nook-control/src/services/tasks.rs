//! Task reads: enrichment, the filtered pick query, and safe claiming.
//!
//! Three fields on `TaskItem` are computed rather than stored — `key`, `url`
//! and `labels`. Storing `key` would let it disagree with the two columns it is
//! made of; storing `url` would bake this deployment's hostname into rows that
//! outlive it. So every path that returns a task goes through [`enrich`], and
//! the cost of that is two extra queries for a whole board rather than two per
//! task.

use crate::repo::tasks::TaskRepository;
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

/// The SQL twin of [`visible_by_cols`] — the ONE place the rule is written for a
/// query (MAIN-265).
///
/// It was written out four more times, by hand, in `pick_tasks`,
/// `epic_children`, `related_tasks` and the Mission Control overview join.
/// MAIN-261 proved those copies agreed; it could not stop the next one drifting,
/// and the failure mode of a drifted copy is a LEAK — a private card shown to a
/// stranger, which is silent rather than a crash.
///
/// **Why a Rust fragment and not a view.** The obvious alternative is a database
/// view carrying the predicate. It does not fit: the rule is parameterised by
/// the *viewer*, so a view would need the viewer as a column or a session
/// setting, and these four queries reach `tasks` under different aliases through
/// different joins (`t` beside `boards`, `board_columns`, `task_relations`,
/// `sessions`). Every one of them would have to be reshaped around the view —
/// a far larger change than the drift it prevents, and NG-1 forbids changing
/// behaviour. A fragment keeps each query exactly the shape it is, and makes the
/// predicate the only thing they share.
///
/// `alias` is the `tasks` alias in the caller's query; `viewer` is the SQL
/// expression holding the viewer's id — a bind marker (`$2`) at every current
/// call site. Both are interpolated, so both must be caller-authored literals,
/// never user input; that is why this takes an alias rather than a whole
/// predicate string.
///
/// Callers that treat "no viewer" as "sees everything" — the overview endpoint —
/// wrap this in their own `IS NULL` leg. That is a different question (does this
/// endpoint scope by viewer at all?) from the one answered here (given a viewer,
/// what may they see?), and keeping them apart is what stops the wrapper being
/// mistaken for part of the rule.
pub fn visible_sql(alias: &str, viewer: &str) -> String {
    format!(
        "({alias}.visibility <> 'private' \
         OR {alias}.created_by = {viewer} \
         OR {alias}.assignee_user_id = {viewer})"
    )
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

/// The SQL twin of [`public_title`]: keep only cards nobody's privacy covers.
///
/// A DIFFERENT rule from [`visible_sql`], and the difference is the whole reason
/// this exists separately (MAIN-265). That one answers "given a viewer, what may
/// they see?" and so has a viewer to compare against. This one has none — the
/// operator digest names cards to a reader who is not being scoped, so every
/// private card is dropped outright rather than resolved against an owner.
///
/// Written out here rather than left inline so that `visibility <> 'private'`
/// appears in exactly one FILE — the property `tests/visibility_one_definition.rs`
/// asserts. A bare copy in a query is then unambiguously a mistake, instead of
/// something a reader has to classify before knowing whether it is one.
///
/// `alias` is the `tasks` alias, the same as [`visible_sql`] takes; `""` for a
/// single-table query that needs no qualifier. Taking the alias rather than a
/// ready-made `"t."` prefix keeps the two functions the same shape, so a caller
/// cannot pass the right string to the wrong one.
pub fn public_only_sql(alias: &str) -> String {
    let qualifier = if alias.is_empty() {
        String::new()
    } else {
        format!("{alias}.")
    };
    format!("{qualifier}visibility <> 'private'")
}

/// Fill in `key`, `url` and `labels` for a batch of tasks.
///
/// Batched deliberately: the board endpoint returns every task at once, and an
/// N+1 there is the difference between one render and two hundred round trips.
/// Two queries regardless of how many tasks come in.
pub async fn enrich(
    repo: &dyn TaskRepository,
    base_url: &str,
    viewer: UserId,
    mut tasks: Vec<TaskItem>,
) -> ApiResult<Vec<TaskItem>> {
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
    let keys = repo.board_keys(&board_ids).await?;

    let label_rows = repo.labels_for_tasks(&ids).await?;

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
    let parents = repo.parent_info(&parent_ids).await?;

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
    repo: &dyn TaskRepository,
    base_url: &str,
    viewer: UserId,
    task: TaskItem,
) -> ApiResult<TaskItem> {
    Ok(enrich(repo, base_url, viewer, vec![task])
        .await?
        .pop()
        .expect("enrich preserves length"))
}

/// Resolve a task by uuid **or** human key (`NOOK-42`, case-insensitively).
///
/// Agents are told keys, not uuids — `Closes NOOK-42` is the join between a PR
/// and its issue — so every task-addressed endpoint accepts both. Tenant-scoped
/// either way: a uuid is not an authorisation.
pub async fn resolve_id(
    repo: &dyn TaskRepository,
    tenant: TenantId,
    ident: &str,
) -> ApiResult<TaskId> {
    if let Ok(uuid) = ident.parse::<uuid::Uuid>() {
        return repo
            .id_by_uuid(tenant, uuid)
            .await?
            .ok_or(ApiError::NotFound);
    }

    let (key, number) = split_key(ident).ok_or_else(|| {
        ApiError::BadRequest(format!(
            "{ident:?} is neither a task id nor a key like NOOK-42"
        ))
    })?;
    repo.id_by_key(tenant, &key, number)
        .await?
        .ok_or(ApiError::NotFound)
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
pub async fn column_of_type(
    repo: &dyn TaskRepository,
    board: BoardId,
    column_type: &str,
) -> ApiResult<ColumnId> {
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
    let found = repo.column_of_type(board, column_type).await?;

    // 409, not 500 and not 404: the request was well formed and the board is
    // real, but this board has no column meaning that. Naming the missing type
    // is the difference between a fixable message and a mystery.
    found.ok_or_else(|| {
        ApiError::Conflict(format!(
            "this board has no {column_type:?} column — add one, or give an explicit column_id"
        ))
    })
}

/// The `KanbanProvider` name that owns a task, via its board. Resolving the
/// name to an instance stays in `mcp_backend`, because the registry is
/// orchestration rather than data.
pub async fn board_provider_for_task(
    repo: &dyn TaskRepository,
    tenant: TenantId,
    task_id: TaskId,
) -> ApiResult<Option<String>> {
    repo.board_provider_for_task(tenant, task_id).await
}

/// One task row by id, scoped to its tenant.
pub async fn get_row(
    repo: &dyn TaskRepository,
    tenant: TenantId,
    id: TaskId,
) -> ApiResult<Option<TaskItem>> {
    repo.get_row(tenant, id).await
}

/// Drop a task's assignee.
pub async fn clear_assignee(
    repo: &dyn TaskRepository,
    tenant: TenantId,
    id: TaskId,
) -> ApiResult<TaskItem> {
    repo.clear_assignee(tenant, id).await
}

/// Set a task's priority. The clamp stays at the call site, where it always was.
pub async fn set_priority_row(
    repo: &dyn TaskRepository,
    tenant: TenantId,
    id: TaskId,
    priority: i32,
) -> ApiResult<TaskItem> {
    repo.set_priority(tenant, id, priority).await
}

/// Record an agent-authored comment.
pub async fn insert_agent_comment(
    repo: &dyn TaskRepository,
    tenant: TenantId,
    task_id: TaskId,
    author_id: uuid::Uuid,
    author_name: &str,
    body_md: &str,
) -> ApiResult<TaskComment> {
    repo.insert_agent_comment(tenant, task_id, author_id, author_name, body_md)
        .await
}

/// Put a posted comment on the activity timeline.
///
/// Comments are the content; events remain the timeline. Both, so the activity
/// feed still shows that something was said without becoming the place the
/// saying is stored.
///
/// A NON-private card's comment carries an excerpt + human key so the
/// notification bridge (MAIN-91) can raise a notification; a PRIVATE card's
/// event carries neither — its body must not reach the tenant-wide feed or a
/// channel, and the absence of an excerpt is exactly what keeps it silent
/// (NG-4, matching MAIN-76's title omission).
///
/// Shared with the MCP door rather than written twice (MAIN-584 AC-11): MCP
/// published only a UI refresh, so a comment made there — and now an UNBLOCK
/// made there, on the one surface where an agent can clear its own stop — never
/// reached the feed a human audits.
pub async fn record_comment_created(
    state: &crate::state::AppState,
    tenant: TenantId,
    task_id: TaskId,
    actor: UserId,
    author_name: &str,
    body_md: &str,
) -> ApiResult<()> {
    let meta = state.tasks.task_visibility_naming(task_id, tenant).await?;
    let mut payload = serde_json::json!({ "task_id": task_id, "author": author_name });
    if let Some((visibility, number, board_key)) = meta {
        if visibility != "private" {
            // One line, trimmed, capped — a teaser, not the whole comment.
            let excerpt: String = body_md
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(140)
                .collect();
            payload["excerpt"] = serde_json::json!(excerpt);
            if let (Some(k), Some(n)) = (board_key, number) {
                payload["key"] = serde_json::json!(format!("{k}-{n}"));
            }
        }
    }
    crate::events::record(
        state,
        tenant,
        crate::events::EventDraft::new("task.comment.created")
            .actor("user", actor.0)
            .payload(payload),
    )
    .await;
    Ok(())
}

/// Get-or-create a label by name and attach it to a task.
pub async fn attach_label(
    repo: &dyn TaskRepository,
    tenant: TenantId,
    task_id: TaskId,
    name: &str,
) -> ApiResult<()> {
    repo.attach_label(tenant, task_id, name).await
}

/// Detach a label from a task by name, and run whatever taking it OFF means.
///
/// Every surface that removes a label comes through here — the REST route, MCP
/// `remove_label`, a board automation's `RemoveBoardLabel` — because the
/// consequence is a property of the LABEL, not of who removed it. For
/// `needs-human-review` that consequence is MAIN-386 AC-5's reset, and wiring
/// it to one surface meant a card unstopped over MCP came back to auto-fire
/// still carrying its three failures, to be re-escalated by the first one
/// after.
///
/// The reset answers ALL THREE escalation labels, not just the ladder's own
/// (MAIN-584 AC-6). They are raised by different machinery — the agent's
/// `blocked` outcome, the starved-queue reaper, the ladder — but they say one
/// thing between them, "a person must look at this", and a card lifted out of
/// any of them is equally owed a clean first rung.
///
/// Answers whether anything was actually removed, so a caller can record an
/// event only when the board really changed.
pub async fn detach_label(
    state: &crate::state::AppState,
    tenant: TenantId,
    task_id: TaskId,
    name: &str,
) -> ApiResult<bool> {
    let Some(label_id) = state.tasks.label_id_by_name(tenant, name).await? else {
        return Ok(false);
    };
    if state.tasks.detach_label_id(task_id, label_id).await? == 0 {
        return Ok(false);
    }
    if crate::events::ESCALATION_LABELS.contains(&name) {
        crate::services::build_ladder::on_stop_lifted(state, tenant, task_id).await;
    }
    Ok(true)
}

/// Restart a card a human has just ruled on (MAIN-584 AC-4).
///
/// Three writes, and the card needs all three: every escalation label off (so
/// the pick query stops excluding it), `agent-ready` on (so the pick query
/// starts including it), and [`crate::repo::tasks::TaskRepository::unblock`]'s
/// stamp — which is the one that actually restarts anything, because a
/// `blocked` outcome leaves the card's fingerprint already recorded as done and
/// no label change moves a fingerprint.
///
/// Idempotent by construction (AC-8): a card carrying no escalation is asking
/// for the same end state and gets it, having named nothing cleared.
///
/// The COLUMN and the CLAIM are deliberately untouched (AC-7, NG-2). A card a
/// human left in In Progress stays there; dragging a board somebody arranged is
/// not part of answering a question.
///
/// Records ONE event naming the labels that actually came off (AC-13), which is
/// what tells a human's restart from an agent's label churn — an agent removes
/// one label at a time and stamps nothing. Here rather than at each door so the
/// three surfaces cannot record it differently, or forget to.
pub async fn unblock(
    state: &crate::state::AppState,
    tenant: TenantId,
    actor: UserId,
    task_id: TaskId,
) -> ApiResult<()> {
    let mut cleared = Vec::new();
    for name in crate::events::ESCALATION_LABELS {
        if detach_label(state, tenant, task_id, name).await? {
            cleared.push(name.to_string());
        }
    }
    state.tasks.set_agent_ready(tenant, task_id, true).await?;
    state.tasks.unblock(tenant, task_id).await?;

    let meta = state.tasks.task_naming(task_id).await.ok().flatten();
    let (title, key) = match meta {
        Some((title, Some(n), Some(k))) => (title, format!("{k}-{n}")),
        Some((title, _, _)) => (title, String::new()),
        None => (String::new(), String::new()),
    };
    crate::events::record(
        state,
        tenant,
        crate::events::EventDraft::new("task.unblocked")
            .actor("user", actor.0)
            .payload(serde_json::json!({
                "task_id": task_id,
                "task": title,
                "key": key,
                "cleared": cleared,
            })),
    )
    .await;
    Ok(())
}
