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
#[derive(Debug, Clone, Default, Deserialize, utoipa::IntoParams)]
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
    /// The node asking, so a builder sees the cards dispatched to ITS machine
    /// plus everything undispatched. Omit it and nothing is narrowed — a human
    /// listing the board sees dispatched cards too.
    pub node: Option<String>,
    pub column_type: Option<String>,
    pub priority: Option<i32>,
    /// Repeatable issue-type filter (MAIN-59). ORs within types (`type=epic&type=bug`
    /// returns either) and ANDs with the other filters. Named `type_`; the query
    /// key is `type`.
    #[serde(rename = "type", default)]
    pub type_: Vec<String>,
    /// Repeatable per-task visibility filter (MAIN-103): `private`|`team`|`org`.
    /// ORs within values and ANDs with the other filters. It only NARROWS within
    /// what the viewer may already see — the viewer predicate still applies, so
    /// `visibility=private` never reveals another person's private card.
    #[serde(default)]
    pub visibility: Vec<String>,
    /// Filter on the derived blocker state.
    pub is_blocked: Option<bool>,
    /// An epic's children (MAIN-81): a uuid or key (`NOOK-7`). Returns the tasks
    /// whose `parent_task_id` is that epic, across every column (an epic's
    /// tickets span backlog and board), still respecting `archived`.
    pub parent: Option<String>,
    pub workspace: Option<uuid::Uuid>,
    /// Free-text search: case-insensitive substring across the task's title,
    /// description body, and display key (`MAIN-42`). ANDs with the other
    /// filters. Absent = no text filter.
    pub q: Option<String>,
    /// Include archived tasks. Default (absent/false) excludes them, so the
    /// agent pick can never claim archived work (MAIN-15 AC-2).
    pub archived: Option<bool>,
    /// Include tasks in a `backlog`-type column. Default (absent/false) excludes
    /// them: the backlog is a human refinement space the loop never draws from
    /// (MAIN-80). Set `backlog=true` to see them. Independent of labels.
    pub backlog: Option<bool>,
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
                // Not lower-cased: ILIKE is already case-insensitive, and the
                // raw term keeps a key search like `MAIN-42` intact.
                "q" => f.q = Some(v),
                "assignee" => f.assignee = Some(v),
                "node" => f.node = Some(v),
                "column_type" => f.column_type = Some(v),
                "priority" => f.priority = Some(num(&k, &v)?),
                "type" => many(&mut f.type_),
                // Validated against the three values — junk is a 400, not a
                // silently ignored filter, so a typo can't quietly widen a list.
                "visibility" => {
                    for val in v
                        .split(',')
                        .map(|s| s.trim().to_lowercase())
                        .filter(|s| !s.is_empty())
                    {
                        if !matches!(val.as_str(), "private" | "team" | "org") {
                            return Err(ApiError::BadRequest(format!(
                                "visibility must be one of private, team, org — got {val:?}"
                            )));
                        }
                        f.visibility.push(val);
                    }
                }
                "limit" => f.limit = Some(num(&k, &v)?),
                "parent" => f.parent = Some(v),
                "is_blocked" => f.is_blocked = Some(flag(&k, &v)?),
                "archived" => f.archived = Some(flag(&k, &v)?),
                "backlog" => f.backlog = Some(flag(&k, &v)?),
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
    Ok(Json(pick(&state, auth.tenant_id, auth.user_id, f).await?))
}

/// The pick query itself, callable from MCP as well as HTTP.
///
/// Shared rather than duplicated: two implementations of "which tasks are
/// pickable" would drift, and the one an agent uses decides what work happens
/// while the one a human sees decides whether they believe it.
pub async fn pick(
    state: &AppState,
    tenant: TenantId,
    viewer: UserId,
    f: TaskFilter,
) -> ApiResult<Vec<TaskItem>> {
    let rows = query_rows(state.tasks.as_ref(), tenant, viewer, &f).await?;
    tasks::enrich(
        state.tasks.as_ref(),
        &state.cfg.public_base_url,
        viewer,
        rows,
    )
    .await
}

/// The pick SQL, before enrichment. Split out from `pick` so the archived
/// exclusion (and the rest of the filter) can be tested against a real database
/// without constructing an `AppState`.
pub async fn query_rows(
    repo: &dyn crate::repo::tasks::TaskRepository,
    tenant: TenantId,
    viewer: UserId,
    f: &TaskFilter,
) -> ApiResult<Vec<TaskItem>> {
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
        const TYPES: [&str; 6] = [
            "backlog",
            "unstarted",
            "started",
            "review",
            "completed",
            "canceled",
        ];
        if !TYPES.contains(&ct) {
            return Err(ApiError::BadRequest(format!(
                "{ct:?} is not a column type — expected one of {}",
                TYPES.join(", ")
            )));
        }
    }
    // Resolve the epic parent (uuid or key) to an id for the children filter
    // (MAIN-81). A parent that does not resolve is a 400, not an empty list, so
    // a typo is not silently "this epic has no tickets".
    let parent_id: Option<uuid::Uuid> = match f.parent.as_deref() {
        Some(p) => Some(
            tasks::resolve_id(repo, tenant, p)
                .await
                .map_err(|_| {
                    ApiError::BadRequest(format!("parent {p:?} is not a task in this tenant"))
                })?
                .0,
        ),
        None => None,
    };
    let labels: Vec<String> = f.label.iter().map(|l| l.trim().to_lowercase()).collect();
    let not_labels: Vec<String> = f
        .not_label
        .iter()
        .map(|l| l.trim().to_lowercase())
        .collect();
    let types: Vec<String> = f.type_.iter().map(|t| t.trim().to_lowercase()).collect();
    let node_id = match f.node.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => None,
        Some(id) => Some(
            id.parse::<uuid::Uuid>()
                .map_err(|_| ApiError::BadRequest(format!("{id:?} is not a node id")))?,
        ),
    };

    let rows = repo
        .pick_tasks(
            tenant,
            viewer,
            crate::repo::tasks::PickParams {
                board: f.board.clone(),
                workspace: f.workspace,
                column_type: f.column_type.clone(),
                priority: f.priority,
                unassigned_only,
                assignee: assignee_id,
                labels,
                not_labels,
                is_blocked: f.is_blocked,
                created_after: f.cursor,
                limit,
                archived: f.archived.unwrap_or(false),
                // `%term%` for a substring match; None disables the clause.
                q: f.q.as_ref().map(|s| format!("%{s}%")),
                types,
                parent: parent_id,
                backlog: f.backlog.unwrap_or(false),
                visibility: f.visibility.clone(),
                node: node_id,
            },
        )
        .await?;

    Ok(rows)
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
    // AC-4 (MAIN-142): the build loop, as it runs TODAY, is a human-or-agent
    // typing `nook claim` inside a session — there is no `build` job kind yet
    // for the executor wall to catch. So the wall is applied here, against the
    // node the claiming session actually runs on.
    //
    // A claim with NO session context is out of this check's reach and is left
    // exactly as it was: the control plane cannot tell where it came from, and
    // refusing every context-less claim would break every human on the board.
    if let Some(session) = req.session_id {
        if let Some(node) = state
            .sessions
            .by_id_unscoped(session)
            .await?
            .map(|s| s.node_id)
        {
            if state.nodes.is_shared_operator(node).await? {
                return Err(ApiError::ForbiddenMsg(
                    "shared operator nodes do not run the build loop".into(),
                ));
            }
        }
    }
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
    let id = tasks::resolve_id(state.tasks.as_ref(), tenant, ident).await?;

    // Visibility guard (MAIN-76 AC-9): a private card is claimable only by its
    // owner (creator or assignee). Refused as NotFound — consistent with the
    // read filter, and so a non-owner agent cannot claim it even by id. Checked
    // first so a non-owner cannot even distinguish a backlog/epic refusal from
    // "does not exist".
    let existing = state
        .tasks
        .get_row(tenant, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !tasks::visible_to(&existing, claimant) {
        return Err(ApiError::NotFound);
    }

    // Backlog and epic tasks are never claimable (MAIN-80 AC-4), refused BEFORE
    // the assignee UPDATE with distinct 400s so a caller can tell "never
    // claimable" apart from the 409 lost-claim race. The backlog is a human
    // refinement space; an epic is a container, not a unit of work. The task
    // row is already loaded above for the visibility guard (its `type_` is the
    // epic test); only the column's type needs a lookup.
    let column_kind = state.tasks.column_type_of(existing.column_id).await?;
    if column_kind.as_deref() == Some("backlog") {
        return Err(ApiError::BadRequest(
            "task is in the backlog — send it to the board first".into(),
        ));
    }
    if existing.type_ == "epic" {
        return Err(ApiError::BadRequest(
            "epics are containers and cannot be claimed".into(),
        ));
    }

    // Resolving the target column is a separate read, but it cannot race: a
    // column's type does not change under a claim, and if the column is missing
    // the caller gets a 409 naming the type before anything is written.
    let target = match column_type.as_deref() {
        Some(ct) => {
            let board = state
                .tasks
                .board_of_task(id)
                .await?
                .ok_or(ApiError::NotFound)?;
            Some(tasks::column_of_type(state.tasks.as_ref(), board, ct).await?)
        }
        None => None,
    };

    // A claim that takes the card into `started` is an agent taking work, and
    // that is exactly what carries a lease (MAIN-229 AC-2). A claim that leaves
    // the card where it is does not — nothing has started, so there is nothing
    // to reclaim.
    let lease =
        (column_type.as_deref() == Some("started")).then_some(state.cfg.max_claim_secs as i64);
    let updated = state
        .tasks
        .claim_task(id, tenant, claimant, target, lease)
        .await?;

    let Some(task) = updated else {
        // Losing a race is the expected outcome for all but one caller, so the
        // message says what to do rather than merely that something failed.
        let current = state.tasks.assignee_of(id, tenant).await?;
        return match current {
            Some(Some(_)) => Err(ApiError::Conflict(
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
            // Redact a private card's title from the tenant activity feed
            // (MAIN-76 AC-3), even though the claimant now owns it.
            .payload(serde_json::json!({
                "task_id": id,
                "title": tasks::public_title(&task),
            })),
    )
    .await;
    // Board automation (MAIN-73): a claim that also moves the card (a started
    // column_type was given) fires that column's rules. No-op when the claim did
    // not change the column (`existing.column_id == task.column_id`).
    crate::services::triggers::on_column_change(
        state,
        tenant,
        task.id,
        task.board_id,
        existing.column_id,
        task.column_id,
    )
    .await;
    state
        .registry
        .publish(tenant, nook_proto::UiEvent::TaskChanged { task_id: id });

    tasks::enrich_one(
        state.tasks.as_ref(),
        &state.cfg.public_base_url,
        claimant,
        task,
    )
    .await
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
    let id = tasks::resolve_id(state.tasks.as_ref(), auth.tenant_id, &ident).await?;
    let task = state
        .tasks
        .release_assignment(id, auth.tenant_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    state.registry.publish(
        auth.tenant_id,
        nook_proto::UiEvent::TaskChanged { task_id: id },
    );
    Ok(Json(
        tasks::enrich_one(
            state.tasks.as_ref(),
            &state.cfg.public_base_url,
            auth.user_id,
            task,
        )
        .await?,
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

    /// The issue-type filter (MAIN-59) is repeatable and case-folded, like
    /// labels — `type=epic&type=bug` ORs the two.
    #[test]
    fn the_type_filter_is_repeatable_and_case_folded() {
        let a = TaskFilter::parse(Some("type=epic&type=bug")).unwrap();
        assert_eq!(a.type_, vec!["epic", "bug"]);
        let b = TaskFilter::parse(Some("type=Epic,BUG")).unwrap();
        assert_eq!(b.type_, a.type_);
        // No type filter is the empty vec — the default, unchanged behaviour.
        assert!(TaskFilter::parse(Some("label=x")).unwrap().type_.is_empty());
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

    #[test]
    fn archived_flag_parses_and_defaults_to_absent() {
        assert_eq!(TaskFilter::parse(Some("")).unwrap().archived, None);
        assert_eq!(
            TaskFilter::parse(Some("archived=true")).unwrap().archived,
            Some(true)
        );
        assert_eq!(
            TaskFilter::parse(Some("archived=false")).unwrap().archived,
            Some(false)
        );
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

    /// `q` parses as a raw term (not lower-cased — ILIKE handles case, and a
    /// key search like `MAIN-42` must survive intact) (MAIN-54).
    #[test]
    fn q_parses_and_keeps_its_case() {
        assert_eq!(TaskFilter::parse(Some("")).unwrap().q, None);
        assert_eq!(
            TaskFilter::parse(Some("q=Postmark")).unwrap().q.as_deref(),
            Some("Postmark")
        );
        assert_eq!(
            TaskFilter::parse(Some("q=MAIN-42")).unwrap().q.as_deref(),
            Some("MAIN-42")
        );
    }
}

/// Behavioral tests for the archived exclusion (MAIN-15 AC-2), against a real
/// database. They self-provision the schema and no-op without `NOOK_REQUIRE_DB`,
/// like the identity DB tests.
#[cfg(test)]
mod db_tests {
    use super::{query_rows, TaskFilter};
    use nook_db::dialect::type_mapping;
    use nook_db::{params, Db, DbPool};
    use nook_types::{TaskId, TenantId};
    use sqlx::postgres::PgPoolOptions;
    use uuid::Uuid;

    async fn pool() -> Option<DbPool> {
        if std::env::var("NOOK_REQUIRE_DB").ok().as_deref() != Some("1") {
            return None;
        }
        let url = std::env::var("DATABASE_URL").ok()?;
        let db = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .ok()?;
        crate::MIGRATOR.run(&db).await.ok()?;
        Some(nook_db::EnginePool::from_pg(db))
    }

    /// Insert a board + one column + a task (archived or not), returning the id.
    async fn task(db: &DbPool, tenant: Uuid, board: Uuid, col: Uuid, archived: bool) -> TaskId {
        let id = Uuid::new_v4();
        db.exec(
            &format!(
                "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, archived_at)
             VALUES ($1, $2, $3, $4, 't', CASE WHEN $5 THEN {} ELSE NULL END)",
                type_mapping(db.engine()).now()
            ),
            params![id, tenant, board, col, archived],
        )
        .await
        .unwrap();
        TaskId(id)
    }

    /// Insert a task with a title, description, and number, returning its id.
    async fn titled_task(
        db: &DbPool,
        tenant: Uuid,
        board: Uuid,
        col: Uuid,
        number: i32,
        title: &str,
        description: &str,
    ) -> TaskId {
        let id = Uuid::new_v4();
        db.exec(
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, description, number)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
            params![id, tenant, board, col, title, description, number],
        )
        .await
        .unwrap();
        TaskId(id)
    }

    /// AC-1/AC-4: `q` matches title, description body, and display key,
    /// case-insensitively, and combines with the other filters.
    #[tokio::test]
    async fn q_searches_title_description_and_key() {
        let Some(db) = pool().await else {
            eprintln!("skipping q_searches_title_description_and_key — no DATABASE_URL");
            return;
        };
        let tenant = Uuid::new_v4();
        let board = Uuid::new_v4();
        let col = Uuid::new_v4();
        db.exec(
            "INSERT INTO tenants (id, name, slug) VALUES ($1, 'S54', $2)",
            params![tenant, format!("s54-{tenant}")],
        )
        .await
        .unwrap();
        db.exec(
            "INSERT INTO boards (id, tenant_id, name, key) VALUES ($1, $2, 'B', $3)",
            params![
                board,
                tenant,
                format!("SR{}", &board.simple().to_string()[..6]).to_uppercase()
            ],
        )
        .await
        .unwrap();
        let board_key: String = db
            .query_scalar("SELECT key FROM boards WHERE id = $1", params![board])
            .await
            .unwrap();
        db.exec(
            "INSERT INTO board_columns (id, board_id, name, type, position)
             VALUES ($1, $2, 'Todo', 'unstarted', 0)",
            params![col, board],
        )
        .await
        .unwrap();

        let by_title = titled_task(&db, tenant, board, col, 1, "Alpha Postmark work", "n/a").await;
        let by_desc = titled_task(
            &db,
            tenant,
            board,
            col,
            42,
            "Beta",
            "mentions Postmark in the body",
        )
        .await;
        let unrelated = titled_task(&db, tenant, board, col, 7, "Gamma", "unrelated text").await;

        let search = |q: &str| {
            let db = db.clone();
            let f = TaskFilter {
                board: Some(board.to_string()),
                q: Some(q.to_string()),
                ..Default::default()
            };
            async move {
                query_rows(
                    &crate::repo::tasks::DbTaskRepository::new(db.clone()),
                    TenantId(tenant),
                    nook_types::UserId::new(),
                    &f,
                )
                .await
                .unwrap()
                .into_iter()
                .map(|t| t.id)
                .collect::<Vec<_>>()
            }
        };

        // Case-insensitive substring across title AND description body.
        let hits = search("postmark").await;
        assert!(hits.contains(&by_title), "matches in the title");
        assert!(hits.contains(&by_desc), "matches in the description body");
        assert!(!hits.contains(&unrelated), "excludes non-matching tasks");
        assert_eq!(search("POSTMARK").await, hits, "search is case-insensitive");

        // By display key: full key, and bare number both find task #42.
        let full_key = format!("{}-42", board_key);
        assert!(
            search(&full_key).await.contains(&by_desc),
            "found by full key"
        );
        assert!(
            search("42").await.contains(&by_desc),
            "found by bare number"
        );

        // No matches → empty (distinct from a board with tasks).
        assert!(
            search("zznothingmatches").await.is_empty(),
            "no matches is empty"
        );

        let _ = db
            .exec("DELETE FROM tasks WHERE board_id = $1", params![board])
            .await;
        let _ = db
            .exec(
                "DELETE FROM board_columns WHERE board_id = $1",
                params![board],
            )
            .await;
        let _ = db
            .exec("DELETE FROM boards WHERE id = $1", params![board])
            .await;
        let _ = db
            .exec("DELETE FROM tenants WHERE id = $1", params![tenant])
            .await;
    }

    #[tokio::test]
    async fn pick_excludes_archived_by_default_and_includes_when_asked() {
        let Some(db) = pool().await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        let tenant = Uuid::new_v4();
        let board = Uuid::new_v4();
        let col = Uuid::new_v4();
        db.exec(
            "INSERT INTO tenants (id, name, slug) VALUES ($1, 'A15', $2)",
            params![tenant, format!("a15-{tenant}")],
        )
        .await
        .unwrap();
        db.exec(
            "INSERT INTO boards (id, tenant_id, name, key) VALUES ($1, $2, 'B', $3)",
            params![
                board,
                tenant,
                format!("B{}", &board.simple().to_string()[..6])
            ],
        )
        .await
        .unwrap();
        db.exec(
            "INSERT INTO board_columns (id, board_id, name, type, position)
             VALUES ($1, $2, 'Done', 'completed', 0)",
            params![col, board],
        )
        .await
        .unwrap();

        let live = task(&db, tenant, board, col, false).await;
        let archived = task(&db, tenant, board, col, true).await;

        let f = TaskFilter {
            board: Some(board.to_string()),
            ..Default::default()
        };
        let default_ids: Vec<TaskId> = query_rows(
            &crate::repo::tasks::DbTaskRepository::new(db.clone()),
            TenantId(tenant),
            nook_types::UserId::new(),
            &f,
        )
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.id)
        .collect();

        let with_archived = TaskFilter {
            archived: Some(true),
            ..f.clone()
        };
        let all_ids: Vec<TaskId> = query_rows(
            &crate::repo::tasks::DbTaskRepository::new(db.clone()),
            TenantId(tenant),
            nook_types::UserId::new(),
            &with_archived,
        )
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.id)
        .collect();

        // A direct id fetch still resolves the archived task (AC-6 / by-key).
        let by_id: i64 = db
            .query_scalar(
                "SELECT count(*) FROM tasks WHERE id = $1",
                params![archived.0],
            )
            .await
            .unwrap();

        // cleanup
        let _ = db
            .exec("DELETE FROM tasks WHERE board_id = $1", params![board])
            .await;
        let _ = db
            .exec(
                "DELETE FROM board_columns WHERE board_id = $1",
                params![board],
            )
            .await;
        let _ = db
            .exec("DELETE FROM boards WHERE id = $1", params![board])
            .await;
        let _ = db
            .exec("DELETE FROM tenants WHERE id = $1", params![tenant])
            .await;

        assert!(default_ids.contains(&live), "live task is picked");
        assert!(
            !default_ids.contains(&archived),
            "archived task is NOT in the default pick — a loop can never claim it"
        );
        assert!(
            all_ids.contains(&live) && all_ids.contains(&archived),
            "archived=true returns both"
        );
        assert_eq!(by_id, 1, "an archived task is still resolvable by id");
    }

    /// MAIN-103 AC-3: the `visibility=` param parses the three values and rejects
    /// anything else with a 400 — a typo can't become a silently-ignored filter.
    #[test]
    fn visibility_filter_parses_and_rejects_junk() {
        let f = TaskFilter::parse(Some("visibility=private,team")).unwrap();
        assert_eq!(
            f.visibility,
            vec!["private".to_string(), "team".to_string()]
        );
        assert!(
            TaskFilter::parse(Some("visibility=bogus")).is_err(),
            "an unknown visibility value is a 400"
        );
    }

    /// A task with a chosen visibility and creator.
    async fn vis_task(
        db: &DbPool,
        tenant: Uuid,
        board: Uuid,
        col: Uuid,
        number: i32,
        visibility: &str,
        created_by: Uuid,
    ) -> TaskId {
        let id = Uuid::new_v4();
        db.exec(
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, number, visibility, created_by)
             VALUES ($1, $2, $3, $4, 't', $5, $6, $7)",
            params![id, tenant, board, col, number, visibility, created_by],
        )
        .await
        .unwrap();
        TaskId(id)
    }

    /// MAIN-103 AC-3: the visibility filter NARROWS within what the viewer may
    /// see and never widens — `visibility=private` shows only the caller's own
    /// private cards, never a teammate's.
    #[tokio::test]
    async fn visibility_filter_narrows_and_never_widens() {
        let Some(db) = pool().await else {
            eprintln!("skipping visibility_filter_narrows_and_never_widens — no DATABASE_URL");
            return;
        };
        let tenant = Uuid::new_v4();
        let board = Uuid::new_v4();
        let col = Uuid::new_v4();
        let me = Uuid::new_v4();
        let other = Uuid::new_v4();
        db.exec(
            "INSERT INTO tenants (id, name, slug) VALUES ($1, 'V103', $2)",
            params![tenant, format!("v103-{tenant}")],
        )
        .await
        .unwrap();
        db.exec(
            "INSERT INTO boards (id, tenant_id, name, key) VALUES ($1, $2, 'B', $3)",
            params![
                board,
                tenant,
                format!("V{}", &board.simple().to_string()[..6]).to_uppercase()
            ],
        )
        .await
        .unwrap();
        db.exec(
            "INSERT INTO board_columns (id, board_id, name, type, position)
             VALUES ($1, $2, 'Todo', 'unstarted', 0)",
            params![col, board],
        )
        .await
        .unwrap();

        let my_private = vis_task(&db, tenant, board, col, 1, "private", me).await;
        let team_card = vis_task(&db, tenant, board, col, 2, "team", other).await;
        let their_private = vis_task(&db, tenant, board, col, 3, "private", other).await;

        let list = |visibility: Vec<String>| {
            let db = db.clone();
            let f = TaskFilter {
                board: Some(board.to_string()),
                visibility,
                ..Default::default()
            };
            async move {
                query_rows(
                    &crate::repo::tasks::DbTaskRepository::new(db.clone()),
                    TenantId(tenant),
                    nook_types::UserId(me),
                    &f,
                )
                .await
                .unwrap()
                .into_iter()
                .map(|t| t.id)
                .collect::<Vec<_>>()
            }
        };

        // No filter: I see my private card and the team card, never the other
        // person's private card (the viewer predicate, MAIN-76).
        let all = list(vec![]).await;
        assert!(all.contains(&my_private) && all.contains(&team_card));
        assert!(
            !all.contains(&their_private),
            "a teammate's private card is never visible"
        );

        // visibility=private narrows to ONLY my own private card — it does not
        // widen to the teammate's private card.
        let privates = list(vec!["private".into()]).await;
        assert!(
            privates.contains(&my_private),
            "my private card passes the filter"
        );
        assert!(
            !privates.contains(&team_card),
            "the team card is filtered out"
        );
        assert!(
            !privates.contains(&their_private),
            "visibility=private must NOT reveal a teammate's private card"
        );

        // visibility=team narrows to the team card only.
        let teams = list(vec!["team".into()]).await;
        assert!(teams.contains(&team_card));
        assert!(!teams.contains(&my_private) && !teams.contains(&their_private));
    }
}
