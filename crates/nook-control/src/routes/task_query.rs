//! `GET /tasks` and `POST /tasks/{id}/claim` — how an agent finds work and
//! takes it without racing another agent.
//!
//! These two are the whole reason the rest of the board exists. The pick step
//! is one compound filter ("labeled agent-ready, unassigned, not blocked,
//! highest priority first, oldest first") and it has to be ONE query: an agent
//! that fetched a board and filtered client-side would be reading a snapshot
//! that is already wrong by the time it decides.
//!
//! Claiming is the other half. Two agents polling the same queue will pick the
//! same task — that is normal, not an error — so the claim has to be atomic and
//! the loser has to be told plainly enough to go and pick again.

use axum::extract::{Path, RawQuery, State};
use axum::Json;
use nook_types::*;
use serde::Deserialize;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::services::tasks;
use crate::state::AppState;

// `parameter_in = Query` is not the default — without it utoipa emits every
// field as a PATH parameter, and the generated TypeScript then types the query
// object as `undefined`, so no caller can pass a filter at all.
#[derive(Debug, Default, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TaskFilter {
    /// Board id or key (`ENG`). Omit to search the whole tenant.
    pub board: Option<String>,
    /// Repeatable. ALL must be present.
    #[serde(default)]
    pub label: Vec<String>,
    /// Repeatable. NONE may be present.
    #[serde(default)]
    pub not_label: Vec<String>,
    /// A user id, or the literal `none` for unassigned.
    pub assignee: Option<String>,
    pub column_type: Option<String>,
    pub priority: Option<i32>,
    /// Filter on the derived blocker state.
    pub is_blocked: Option<bool>,
    pub workspace: Option<uuid::Uuid>,
    pub limit: Option<i64>,
    /// Opaque: the `created_at` of the last row of the previous page.
    pub cursor: Option<chrono::DateTime<chrono::Utc>>,
}

impl TaskFilter {
    /// Parse the query string by hand.
    ///
    /// `serde_urlencoded`, which `Query` uses, cannot express a repeated key:
    /// `?label=a&label=b` fails with "invalid type: string, expected a
    /// sequence". Repeatable label filters are the whole point of the pick
    /// query, so this walks the pairs itself — and accepts `label=a,b` as well,
    /// because both forms are things people and clients actually send and
    /// neither is worth a support question.
    pub fn parse(raw: Option<&str>) -> Result<Self, ApiError> {
        let mut f = TaskFilter::default();
        let Some(raw) = raw else { return Ok(f) };

        for (k, v) in form_urlencoded::parse(raw.as_bytes()) {
            let v = v.trim().to_string();
            if v.is_empty() {
                continue;
            }
            let many = |out: &mut Vec<String>| {
                out.extend(
                    v.split(',')
                        .map(|s| s.trim().to_lowercase())
                        .filter(|s| !s.is_empty()),
                );
            };
            match k.as_ref() {
                "label" => many(&mut f.label),
                "not_label" => many(&mut f.not_label),
                "board" => f.board = Some(v),
                "assignee" => f.assignee = Some(v),
                "column_type" => f.column_type = Some(v),
                "priority" => f.priority = Some(num(&k, &v)?),
                "limit" => f.limit = Some(num(&k, &v)?),
                "is_blocked" => f.is_blocked = Some(flag(&k, &v)?),
                "workspace" => {
                    f.workspace = Some(v.parse().map_err(|_| {
                        ApiError::BadRequest(format!("workspace must be a uuid, got {v:?}"))
                    })?)
                }
                "cursor" => {
                    f.cursor = Some(v.parse().map_err(|_| {
                        ApiError::BadRequest(format!("cursor must be a timestamp, got {v:?}"))
                    })?)
                }
                // Unknown keys are ignored rather than rejected: clients append
                // cache-busters and UI state, and failing the pick query over
                // one would be a poor trade.
                _ => {}
            }
        }
        Ok(f)
    }
}

fn num<T: std::str::FromStr>(key: &str, v: &str) -> Result<T, ApiError> {
    v.parse()
        .map_err(|_| ApiError::BadRequest(format!("{key} must be a number, got {v:?}")))
}

fn flag(key: &str, v: &str) -> Result<bool, ApiError> {
    match v.to_lowercase().as_str() {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        _ => Err(ApiError::BadRequest(format!(
            "{key} must be true or false, got {v:?}"
        ))),
    }
}

/// The pick query.
///
/// Built as one statement with bound parameters rather than assembled from
/// strings — every filter here is caller-supplied, and a query builder that
/// interpolated any of them would be an injection with a board's worth of data
/// behind it. The cost is a slightly denser SQL body; the benefit is that no
/// value ever reaches the parser.
#[utoipa::path(get, path = "/api/v1/tasks",
    operation_id = "query_tasks", params(TaskFilter),
    responses((status = 200, body = [TaskItem])))]
pub async fn query(
    State(state): State<AppState>,
    auth: AuthCtx,
    RawQuery(raw): RawQuery,
) -> ApiResult<Json<Vec<TaskItem>>> {
    let f = TaskFilter::parse(raw.as_deref())?;
    Ok(Json(pick(&state, auth.tenant_id, f).await?))
}

/// The pick query itself, callable from MCP as well as HTTP.
///
/// Shared rather than duplicated: two implementations of "which tasks are
/// pickable" would drift, and the one an agent uses decides what work happens
/// while the one a human sees decides whether they believe it.
pub async fn pick(state: &AppState, tenant: TenantId, f: TaskFilter) -> ApiResult<Vec<TaskItem>> {
    let limit = f.limit.unwrap_or(50).clamp(1, 200);

    // `assignee=none` means unassigned, which is a different question from "no
    // assignee filter" — hence the two flags rather than one nullable id.
    let (unassigned_only, assignee_id) = match f.assignee.as_deref() {
        None => (false, None),
        Some("none") | Some("null") => (true, None),
        Some(id) => (
            false,
            Some(id.parse::<uuid::Uuid>().map_err(|_| {
                ApiError::BadRequest(format!("{id:?} is not a user id (or the word `none`)"))
            })?),
        ),
    };
    if let Some(ct) = f.column_type.as_deref() {
        const TYPES: [&str; 5] = ["backlog", "unstarted", "started", "completed", "canceled"];
        if !TYPES.contains(&ct) {
            return Err(ApiError::BadRequest(format!(
                "{ct:?} is not a column type — expected one of {}",
                TYPES.join(", ")
            )));
        }
    }
    let labels: Vec<String> = f.label.iter().map(|l| l.trim().to_lowercase()).collect();
    let not_labels: Vec<String> = f
        .not_label
        .iter()
        .map(|l| l.trim().to_lowercase())
        .collect();

    let rows: Vec<TaskItem> = sqlx::query_as(
        r#"
        SELECT t.* FROM tasks t
        JOIN boards b ON b.id = t.board_id
        JOIN board_columns c ON c.id = t.column_id
        WHERE t.tenant_id = $1
          AND ($2::text IS NULL OR b.id::text = $2 OR upper(b.key) = upper($2))
          AND ($3::uuid IS NULL OR t.workspace_id = $3)
          AND ($4::text IS NULL OR c.type = $4)
          AND ($5::int  IS NULL OR t.priority = $5)
          AND (NOT $6::bool OR t.assignee_user_id IS NULL)
          AND ($7::uuid IS NULL OR t.assignee_user_id = $7)
          -- every required label must be present
          AND (cardinality($8::text[]) = 0 OR (
                SELECT count(DISTINCT l.name) FROM task_labels tl
                JOIN labels l ON l.id = tl.label_id
                WHERE tl.task_id = t.id AND l.name = ANY($8)
              ) = cardinality($8::text[]))
          -- and none of the excluded ones
          AND NOT EXISTS (
                SELECT 1 FROM task_labels tl
                JOIN labels l ON l.id = tl.label_id
                WHERE tl.task_id = t.id AND l.name = ANY($9::text[]))
          -- blocked is DERIVED: an unfinished task pointing here with `blocks`
          AND ($10::bool IS NULL OR $10 = EXISTS (
                SELECT 1 FROM task_relations r
                JOIN tasks bt ON bt.id = r.from_task
                JOIN board_columns bc ON bc.id = bt.column_id
                WHERE r.to_task = t.id AND r.kind = 'blocks'
                  AND bc.type NOT IN ('completed', 'canceled')))
          AND ($11::timestamptz IS NULL OR t.created_at > $11)
        -- priority 0 means "unset", which sorts last rather than first
        ORDER BY CASE WHEN t.priority = 0 THEN 5 ELSE t.priority END, t.created_at
        LIMIT $12
        "#,
    )
    .bind(tenant)
    .bind(f.board.as_deref())
    .bind(f.workspace)
    .bind(f.column_type.as_deref())
    .bind(f.priority)
    .bind(unassigned_only)
    .bind(assignee_id)
    .bind(&labels)
    .bind(&not_labels)
    .bind(f.is_blocked)
    .bind(f.cursor)
    .bind(limit)
    .fetch_all(&state.db)
    .await?;

    tasks::enrich(&state.db, &state.cfg.public_base_url, rows)
        .await
        .map_err(Into::into)
}

/// Take the work, atomically.
///
/// The assignment and the move are one statement with the "still unassigned"
/// test in its WHERE clause, so two agents racing cannot both win: the second
/// UPDATE matches zero rows and gets a 409 carrying the current state, which is
/// enough for it to pick again without another round trip.
#[utoipa::path(post, path = "/api/v1/tasks/{id}/claim",
    operation_id = "claim_task", params(("id" = String, Path,)),
    request_body = ClaimTaskRequest,
    responses((status = 200, body = TaskItem), (status = 409, description = "already claimed")))]
pub async fn claim(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(ident): Path<String>,
    Json(req): Json<ClaimTaskRequest>,
) -> ApiResult<Json<TaskItem>> {
    let claimant = req.assignee_user_id.unwrap_or(auth.user_id);
    Ok(Json(
        claim_inner(&state, auth.tenant_id, claimant, &ident, req.column_type).await?,
    ))
}

/// The atomic claim, shared with MCP.
pub async fn claim_inner(
    state: &AppState,
    tenant: TenantId,
    claimant: UserId,
    ident: &str,
    column_type: Option<String>,
) -> ApiResult<TaskItem> {
    let id = tasks::resolve_id(&state.db, tenant, ident).await?;

    // Resolving the target column is a separate read, but it cannot race: a
    // column's type does not change under a claim, and if the column is missing
    // the caller gets a 409 naming the type before anything is written.
    let target = match column_type.as_deref() {
        Some(ct) => {
            let board: (BoardId,) = sqlx::query_as("SELECT board_id FROM tasks WHERE id = $1")
                .bind(id)
                .fetch_one(&state.db)
                .await?;
            Some(tasks::column_of_type(&state.db, board.0, ct).await?)
        }
        None => None,
    };

    let updated: Option<TaskItem> = sqlx::query_as(
        "UPDATE tasks SET
             assignee_user_id = $1,
             column_id = coalesce($2, column_id),
             updated_at = now()
         WHERE id = $3 AND tenant_id = $4 AND assignee_user_id IS NULL
         RETURNING *",
    )
    .bind(claimant)
    .bind(target)
    .bind(id)
    .bind(tenant)
    .fetch_optional(&state.db)
    .await?;

    let Some(task) = updated else {
        // Losing a race is the expected outcome for all but one caller, so the
        // message says what to do rather than merely that something failed.
        let current: Option<(Option<UserId>,)> =
            sqlx::query_as("SELECT assignee_user_id FROM tasks WHERE id = $1 AND tenant_id = $2")
                .bind(id)
                .bind(tenant)
                .fetch_optional(&state.db)
                .await?;
        return match current {
            Some((Some(_),)) => Err(ApiError::Conflict(
                "somebody else claimed this first — pick another task".into(),
            )),
            _ => Err(ApiError::NotFound),
        };
    };

    crate::events::record(
        state,
        tenant,
        crate::events::EventDraft::new("task.claimed")
            .actor("user", claimant.0)
            .payload(serde_json::json!({ "task_id": id, "title": task.title })),
    )
    .await;
    state
        .registry
        .publish(tenant, nook_proto::UiEvent::TaskChanged { task_id: id });

    tasks::enrich_one(&state.db, &state.cfg.public_base_url, task)
        .await
        .map_err(Into::into)
}

/// Give the work back: clear the assignee so somebody else can pick it up.
#[utoipa::path(post, path = "/api/v1/tasks/{id}/release",
    operation_id = "release_task", params(("id" = String, Path,)),
    responses((status = 200, body = TaskItem), (status = 404)))]
pub async fn release(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(ident): Path<String>,
) -> ApiResult<Json<TaskItem>> {
    let id = tasks::resolve_id(&state.db, auth.tenant_id, &ident).await?;
    let task: TaskItem = sqlx::query_as(
        "UPDATE tasks SET assignee_user_id = NULL, updated_at = now()
         WHERE id = $1 AND tenant_id = $2 RETURNING *",
    )
    .bind(id)
    .bind(auth.tenant_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(ApiError::NotFound)?;
    state.registry.publish(
        auth.tenant_id,
        nook_proto::UiEvent::TaskChanged { task_id: id },
    );
    Ok(Json(
        tasks::enrich_one(&state.db, &state.cfg.public_base_url, task).await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pick query is `?label=agent-ready&not_label=blocked&assignee=none`.
    /// `Query<T>` could not parse a repeated key at all, which is what sent
    /// this through a hand-written parser — so the repeat case is the one that
    /// must never regress.
    #[test]
    fn repeated_and_comma_separated_labels_both_work() {
        let a = TaskFilter::parse(Some("label=agent-ready&label=urgent")).unwrap();
        assert_eq!(a.label, vec!["agent-ready", "urgent"]);

        let b = TaskFilter::parse(Some("label=agent-ready,urgent")).unwrap();
        assert_eq!(b.label, a.label, "both spellings mean the same thing");

        // Mixed, and case-folded to match how labels are stored.
        let c = TaskFilter::parse(Some("label=A,B&label=C")).unwrap();
        assert_eq!(c.label, vec!["a", "b", "c"]);
    }

    #[test]
    fn the_whole_pick_query_parses() {
        let f = TaskFilter::parse(Some(
            "board=NOOK&label=agent-ready&not_label=blocked&assignee=none\
             &is_blocked=false&priority=1&limit=10",
        ))
        .unwrap();
        assert_eq!(f.board.as_deref(), Some("NOOK"));
        assert_eq!(f.label, vec!["agent-ready"]);
        assert_eq!(f.not_label, vec!["blocked"]);
        assert_eq!(f.assignee.as_deref(), Some("none"));
        assert_eq!(f.is_blocked, Some(false));
        assert_eq!(f.priority, Some(1));
        assert_eq!(f.limit, Some(10));
    }

    /// `is_blocked` absent, `false`, and `true` are three different questions.
    /// Collapsing absent into false would silently hide every blocked task
    /// from an unfiltered board.
    #[test]
    fn absent_is_not_the_same_as_false() {
        assert_eq!(TaskFilter::parse(Some("")).unwrap().is_blocked, None);
        assert_eq!(
            TaskFilter::parse(Some("is_blocked=false"))
                .unwrap()
                .is_blocked,
            Some(false)
        );
        assert_eq!(
            TaskFilter::parse(Some("is_blocked=true"))
                .unwrap()
                .is_blocked,
            Some(true)
        );
        assert_eq!(TaskFilter::parse(None).unwrap().is_blocked, None);
    }

    #[test]
    fn bad_values_are_named_not_swallowed() {
        assert!(TaskFilter::parse(Some("priority=high")).is_err());
        assert!(TaskFilter::parse(Some("is_blocked=maybe")).is_err());
        assert!(TaskFilter::parse(Some("workspace=not-a-uuid")).is_err());
        // Unknown keys are tolerated — clients append their own.
        assert!(TaskFilter::parse(Some("_t=123456&label=x")).is_ok());
    }

    /// Percent-encoding has to survive, or a label with a space silently
    /// becomes a different filter that matches nothing.
    #[test]
    fn values_are_percent_decoded() {
        let f = TaskFilter::parse(Some("label=needs%20review")).unwrap();
        assert_eq!(f.label, vec!["needs review"]);
    }
}
