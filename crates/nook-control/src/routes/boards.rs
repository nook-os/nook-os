use axum::extract::{Path, State};
use axum::Json;
use nook_db::{params, Db, Postgres, TypeMapping};
use nook_types::*;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::events::{self, EventDraft};
use crate::services::kanban::provider_err;
use crate::state::AppState;

#[utoipa::path(get, path = "/api/v1/boards",
    operation_id = "list_boards", responses((status = 200, body = [Board])))]
pub async fn list(State(state): State<AppState>, auth: AuthCtx) -> ApiResult<Json<Vec<Board>>> {
    Ok(Json(state.kanban.all_boards(auth.tenant_id).await?))
}

#[utoipa::path(post, path = "/api/v1/boards",
    operation_id = "create_board",
    request_body = CreateBoardRequest,
    responses((status = 200, body = BoardDetail)))]
pub async fn create(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<CreateBoardRequest>,
) -> ApiResult<Json<BoardDetail>> {
    // Without a key, every task on this board would have no `NOOK-42` to be
    // called by — which is the one thing a PR body can reference. Derived here
    // rather than left null, because "the board I made in the UI has no keys"
    // is not a state anybody would choose.
    let key = match req.key.as_deref() {
        Some(k) => validate_key(k)?,
        None => unique_key(&state, auth.tenant_id, &req.name).await?,
    };

    let board: Board = state
        .db
        .query_one(
            "INSERT INTO boards (id, tenant_id, workspace_id, name, key, provider)
         VALUES ($1, $2, $3, $4, $5, 'local') RETURNING *",
            params![
                BoardId::new(),
                auth.tenant_id,
                req.workspace_id.map(|w| w.0),
                &req.name,
                &key
            ],
        )
        .await?;

    // Name AND type. A board whose columns have no types is one automation
    // cannot navigate — "move it to started" has nothing to resolve.
    let mut columns = Vec::new();
    for (i, (name, kind)) in [
        ("Triage", "backlog"),
        ("Todo", "unstarted"),
        ("In Progress", "started"),
        ("In Review", "review"),
        ("Done", "completed"),
    ]
    .iter()
    .enumerate()
    {
        let col: BoardColumn = state
            .db
            .query_one(
                "INSERT INTO board_columns (id, board_id, name, position, type)
             VALUES ($1, $2, $3, $4, $5) RETURNING *",
                params![ColumnId::new(), board.id, *name, i as i32, *kind],
            )
            .await?;
        columns.push(col);
    }

    Ok(Json(BoardDetail {
        board,
        columns,
        tasks: vec![],
    }))
}

#[utoipa::path(get, path = "/api/v1/boards/{id}",
    operation_id = "get_board",
    params(("id" = String, Path,)),
    responses((status = 200, body = BoardDetail), (status = 404)))]
pub async fn get_one(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<BoardId>,
) -> ApiResult<Json<BoardDetail>> {
    let provider = provider_for_board(&state, auth.tenant_id, id).await?;
    let detail = state
        .kanban
        .get(&provider)
        .ok_or(ApiError::NotFound)?
        .board_detail(auth.tenant_id, id)
        .await
        .map_err(provider_err)?;

    // Visibility (MAIN-76): the provider returns every card on the board; drop
    // the ones this viewer may not see (private cards they neither created nor
    // are assigned) BEFORE enrich, so a private title never even reaches the
    // response. The provider trait stays viewer-agnostic; the filter lives here
    // where the caller's identity is known.
    let visible: Vec<_> = detail
        .tasks
        .into_iter()
        .filter(|t| crate::services::tasks::visible_to(t, auth.user_id))
        .collect();

    // Keys, deep links and labels are computed rather than stored, so the
    // board's cards would otherwise render without any of them. Enriched here
    // rather than in the provider because only this layer knows the public URL
    // — a provider that had to be told its own deployment's hostname would be
    // the wrong shape for the external ones this trait exists to allow.
    let detail = BoardDetail {
        tasks: crate::services::tasks::enrich(
            state.tasks.as_ref(),
            &state.cfg.public_base_url,
            auth.user_id,
            visible,
        )
        .await?,
        ..detail
    };
    Ok(Json(detail))
}

#[utoipa::path(post, path = "/api/v1/boards/{id}/tasks",
    operation_id = "create_task",
    params(("id" = String, Path,)),
    request_body = CreateTaskRequest,
    responses((status = 200, body = TaskItem)))]
pub async fn create_task(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<BoardId>,
    Json(req): Json<CreateTaskRequest>,
) -> ApiResult<Json<TaskItem>> {
    let provider = provider_for_board(&state, auth.tenant_id, id).await?;
    let workspace_id = req.workspace_id;
    let task = state
        .kanban
        .get(&provider)
        .ok_or(ApiError::NotFound)?
        .create_task(auth.tenant_id, id, Some(auth.user_id), req)
        .await
        .map_err(provider_err)?;

    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("task.created")
            .actor("user", auth.user_id.0)
            // A private card's title must not reach the tenant activity feed
            // (MAIN-76 AC-3): `public_title` omits it for a private task.
            .payload(serde_json::json!({
                "task_id": task.id,
                "title": crate::services::tasks::public_title(&task),
            })),
    )
    .await;
    let _ = workspace_id;
    state.registry.publish(
        auth.tenant_id,
        nook_proto::UiEvent::TaskChanged { task_id: task.id },
    );

    // A private card must not ping the whole tenant. The notification is a
    // tenant-wide toast + phone push + channel delivery, so skip it entirely for
    // a private task (MAIN-76 AC-3) — its title and existence stay with its
    // owner. Grab visibility before enrich (which does not touch it).
    let is_private = task.visibility == "private";
    let enriched = crate::services::tasks::enrich_one(
        state.tasks.as_ref(),
        &state.cfg.public_base_url,
        auth.user_id,
        task,
    )
    .await?;

    if !is_private {
        // Tell the tenant a card landed. The activity event above feeds the
        // Activity page — a place you go look — but a task appearing on the board
        // (an import, an agent filing an issue, a teammate adding work) is exactly
        // the kind of thing worth a toast and a phone buzz, which is the notify
        // path, not the activity path. Carries the key and a deep link so the toast
        // is actionable rather than just "something changed".
        let mut draft = crate::services::notify::Draft::new(match enriched.key.as_deref() {
            Some(k) => format!("New task: {k}"),
            None => "New task".to_string(),
        })
        .level("info")
        .kind("task.created")
        .body(enriched.title.clone())
        .payload(serde_json::json!({
            "task_id": enriched.id,
            "key": enriched.key,
        }));
        if let Some(url) = enriched.url.clone() {
            draft = draft.link(url);
        }
        crate::services::notify::raise(&state, auth.tenant_id, draft).await;
    }

    Ok(Json(enriched))
}

#[utoipa::path(patch, path = "/api/v1/tasks/{id}",
    operation_id = "update_task",
    params(("id" = String, Path,)),
    request_body = UpdateTaskRequest,
    responses(
        (status = 200, body = TaskItem),
        (status = 404),
        // Optimistic-concurrency conflict: the caller sent `expected_updated_at`
        // and the task changed since; body carries the current task (MAIN-36).
        (status = 409, body = TaskItem)))]
pub async fn update_task(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(ident): Path<String>,
    Json(req): Json<UpdateTaskRequest>,
) -> ApiResult<Json<TaskItem>> {
    // Accepts `NOOK-42` as well as a uuid: agents are handed keys, and an
    // endpoint that took only ids would be one an agent cannot call with what
    // it was given.
    let id =
        crate::services::tasks::resolve_id(state.tasks.as_ref(), auth.tenant_id, &ident).await?;
    // Load the whole task so visibility is checked before any change: a private
    // card cannot be seen OR altered by anyone but its owner (MAIN-76 AC-5),
    // including a tenant admin — refused as NotFound, consistent with the read.
    let existing: TaskItem = state
        .db
        .query_opt(
            "SELECT * FROM tasks WHERE id = $1 AND tenant_id = $2",
            params![id, auth.tenant_id],
        )
        .await?
        .ok_or(ApiError::NotFound)?;
    if !crate::services::tasks::visible_to(&existing, auth.user_id) {
        return Err(ApiError::NotFound);
    }
    // Visibility-change gate (MAIN-85): a `team`/`org` card is visible to every
    // tenant member, so without this ANY of them could flip a teammate's card to
    // `private` and hide it from its collaborators. Only the card's owner (its
    // creator or assignee) or a tenant owner/admin may change its scope. Fires
    // ONLY on an actual change — a PATCH that omits `visibility` or round-trips
    // the current value is untouched, so column moves and field edits keep
    // working for any member who can see the card (AC-2). Checked before the
    // provider write, so a refusal rejects the ENTIRE request (AC-3), and after
    // the visibility read above, so a card the caller cannot see is still a 404,
    // not a 403 (AC-4).
    if let Some(new_vis) = req.visibility.as_deref() {
        if new_vis != existing.visibility && !crate::services::tasks::owns(&existing, auth.user_id)
        {
            auth.require_tenant_admin(&state).await?;
        }
    }
    let board_id = existing.board_id;
    let provider = provider_for_board(&state, auth.tenant_id, board_id).await?;
    let moved_column = req.column_id.is_some() || req.column_type.is_some();
    let task = state
        .kanban
        .get(&provider)
        .ok_or(ApiError::NotFound)?
        .update_task(auth.tenant_id, Some(auth.user_id), id, req)
        .await
        .map_err(provider_err)?;

    if moved_column {
        events::record(
            &state,
            auth.tenant_id,
            EventDraft::new("task.moved")
                .actor("user", auth.user_id.0)
                .payload(serde_json::json!({
                    "task_id": task.id,
                    "title": crate::services::tasks::public_title(&task),
                })),
        )
        .await;
        // Board automation (MAIN-73): fire the destination column's rules. The
        // engine no-ops when the column did not actually change.
        crate::services::triggers::on_column_change(
            &state,
            auth.tenant_id,
            task.id,
            board_id,
            existing.column_id,
            task.column_id,
        )
        .await;
    }
    state.registry.publish(
        auth.tenant_id,
        nook_proto::UiEvent::TaskChanged { task_id: task.id },
    );
    Ok(Json(
        crate::services::tasks::enrich_one(
            state.tasks.as_ref(),
            &state.cfg.public_base_url,
            auth.user_id,
            task,
        )
        .await?,
    ))
}

/// Archive (`archive = true`) or unarchive a single task, resolved by key or
/// uuid. Archiving hides it from the board and the pick query; unarchiving
/// returns it to its column. Emits the usual TaskChanged so other viewers
/// update live (AC-7). Archived tasks remain resolvable by id/key, so this can
/// unarchive one that is no longer on the board.
pub(crate) async fn set_archived(
    state: &AppState,
    auth: &AuthCtx,
    ident: &str,
    archive: bool,
) -> ApiResult<TaskItem> {
    let id =
        crate::services::tasks::resolve_id(state.tasks.as_ref(), auth.tenant_id, ident).await?;
    let task: TaskItem = state
        .db
        .query_opt(
            &format!(
                "UPDATE tasks SET archived_at = CASE WHEN $3 THEN {now} ELSE NULL END,
                          updated_at = {now}
         WHERE id = $1 AND tenant_id = $2 RETURNING *",
                now = Postgres.now()
            ),
            params![id, auth.tenant_id, archive],
        )
        .await?
        .ok_or(ApiError::NotFound)?;

    events::record(
        state,
        auth.tenant_id,
        EventDraft::new(if archive {
            "task.archived"
        } else {
            "task.unarchived"
        })
        .actor("user", auth.user_id.0)
        .payload(serde_json::json!({
            "task_id": task.id,
            "title": crate::services::tasks::public_title(&task),
        })),
    )
    .await;
    state.registry.publish(
        auth.tenant_id,
        nook_proto::UiEvent::TaskChanged { task_id: task.id },
    );
    crate::services::tasks::enrich_one(
        state.tasks.as_ref(),
        &state.cfg.public_base_url,
        auth.user_id,
        task,
    )
    .await
}

#[utoipa::path(post, path = "/api/v1/tasks/{id}/archive",
    operation_id = "archive_task",
    params(("id" = String, Path,)),
    responses((status = 200, body = TaskItem), (status = 404)))]
pub async fn archive_task(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(ident): Path<String>,
) -> ApiResult<Json<TaskItem>> {
    auth.require_user()?;
    Ok(Json(set_archived(&state, &auth, &ident, true).await?))
}

#[utoipa::path(post, path = "/api/v1/tasks/{id}/unarchive",
    operation_id = "unarchive_task",
    params(("id" = String, Path,)),
    responses((status = 200, body = TaskItem), (status = 404)))]
pub async fn unarchive_task(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(ident): Path<String>,
) -> ApiResult<Json<TaskItem>> {
    auth.require_user()?;
    Ok(Json(set_archived(&state, &auth, &ident, false).await?))
}

/// Archive every live task in one completed/canceled column at once (AC-4).
/// Refused for any other column type — bulk archive is finished-work cleanup,
/// not a way to sweep in-progress work off the board (NG-3).
#[utoipa::path(post, path = "/api/v1/columns/{id}/archive-completed",
    operation_id = "archive_completed_in_column",
    params(("id" = String, Path,)),
    responses((status = 200, body = nook_types::OpResponse), (status = 403), (status = 404)))]
pub async fn archive_completed_in_column(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(column_id): Path<ColumnId>,
) -> ApiResult<Json<nook_types::OpResponse>> {
    auth.require_user()?;
    // board_columns has no tenant_id; scope through its board.
    let col: Option<String> = state
        .db
        .query_scalar_opt(
            "SELECT c.type FROM board_columns c
         JOIN boards b ON b.id = c.board_id
         WHERE c.id = $1 AND b.tenant_id = $2",
            params![column_id, auth.tenant_id],
        )
        .await?;
    let col_type = col.ok_or(ApiError::NotFound)?;
    if col_type != "completed" && col_type != "canceled" {
        return Err(ApiError::ForbiddenMsg(
            "bulk archive is only for completed or canceled columns".into(),
        ));
    }

    let ids: Vec<TaskId> = state
        .db
        .query_scalar_all(
            &format!(
                "UPDATE tasks SET archived_at = {now}, updated_at = {now}
         WHERE column_id = $1 AND tenant_id = $2 AND archived_at IS NULL
         RETURNING id",
                now = Postgres.now()
            ),
            params![column_id, auth.tenant_id],
        )
        .await?;

    // One TaskChanged per card so each disappears live; the board query
    // invalidation keys off any of them (AC-7).
    for id in &ids {
        state.registry.publish(
            auth.tenant_id,
            nook_proto::UiEvent::TaskChanged { task_id: *id },
        );
    }
    events::record(
        &state,
        auth.tenant_id,
        EventDraft::new("task.archived_bulk")
            .actor("user", auth.user_id.0)
            .payload(serde_json::json!({ "column_id": column_id, "count": ids.len() })),
    )
    .await;

    Ok(Json(nook_types::OpResponse {
        ok: true,
        path: None,
        message: format!("archived {} task(s)", ids.len()),
    }))
}

#[utoipa::path(delete, path = "/api/v1/tasks/{id}",
    operation_id = "delete_task",
    params(("id" = String, Path,)),
    responses((status = 204), (status = 404)))]
pub async fn delete_task(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(ident): Path<String>,
) -> ApiResult<axum::http::StatusCode> {
    let id =
        crate::services::tasks::resolve_id(state.tasks.as_ref(), auth.tenant_id, &ident).await?;
    let res = state
        .db
        .exec(
            "DELETE FROM tasks WHERE id = $1 AND tenant_id = $2",
            params![id, auth.tenant_id],
        )
        .await?;
    if res == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[utoipa::path(patch, path = "/api/v1/boards/{id}",
    operation_id = "update_board",
    params(("id" = String, Path,)),
    request_body = UpdateBoardRequest,
    responses((status = 200, body = Board), (status = 404)))]
pub async fn update_board(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<BoardId>,
    Json(req): Json<UpdateBoardRequest>,
) -> ApiResult<Json<Board>> {
    let key = match req.key.as_deref() {
        Some(k) => Some(validate_key(k)?),
        None => None,
    };
    // Validate the automation config before it can be stored (MAIN-73 AC-1): a
    // stored rule set is always one the engine can run. Omitted leaves it as is.
    if let Some(automation) = &req.automation {
        crate::services::triggers::validate(automation)?;
    }
    let board: Option<Board> = state
        .db
        .query_opt(
            &format!(
                "UPDATE boards SET name = $3, key = COALESCE($4, key),
                           automation = COALESCE($5, automation), updated_at = {}
         WHERE id = $1 AND tenant_id = $2 RETURNING *",
                Postgres.now()
            ),
            params![id, auth.tenant_id, &req.name, key, req.automation],
        )
        .await?;
    board.map(Json).ok_or(ApiError::NotFound)
}

/// The first word of a board's name, as a key.
///
/// "NookOS Bootstrap" → `NOOK`. Deliberately not the whole name flattened and
/// cut, which is what produced `NOOKO` — a key nobody would choose, printed on
/// every task forever.
fn derive_key(name: &str) -> String {
    let first: String = name
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(4)
        .collect::<String>()
        .to_uppercase();
    if first.is_empty() {
        "BOARD".to_string()
    } else {
        first
    }
}

/// A board key: the `NOOK` in `NOOK-42`.
fn validate_key(key: &str) -> ApiResult<String> {
    let k = key.trim().to_uppercase();
    if k.is_empty() || k.len() > 10 {
        return Err(ApiError::BadRequest(
            "a board key must be 1–10 characters".into(),
        ));
    }
    if !k.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(ApiError::BadRequest(format!(
            "a board key may only contain letters and digits — got {key:?}"
        )));
    }
    Ok(k)
}

/// Derive a key from a board's name, and make it unique in the tenant.
///
/// The FIRST WORD, not the whole name flattened: "NookOS Bootstrap" should be
/// `NOOK`, not `NOOKO` — which is what you get by running the words together
/// and cutting at five characters, and which reads as a typo forever after.
async fn unique_key(state: &AppState, tenant: TenantId, name: &str) -> ApiResult<String> {
    let base = derive_key(name);

    for n in 1..100 {
        let candidate = if n == 1 {
            base.clone()
        } else {
            format!("{base}{n}")
        };
        let taken: Option<uuid::Uuid> = state
            .db
            .query_scalar_opt(
                "SELECT id FROM boards WHERE tenant_id = $1 AND key = $2",
                params![tenant, &candidate],
            )
            .await?;
        if taken.is_none() {
            return Ok(candidate);
        }
    }
    Err(ApiError::BadRequest(
        "could not derive a free board key — pass one explicitly".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creating a task raises a notification, not only an activity event.
    ///
    /// The two are different surfaces: the activity event feeds the Activity
    /// page you go and look at; the notification is the toast + phone buzz +
    /// channel fan-out. A card landing on the board — an import, an agent
    /// filing an issue — is worth the second, and for a long time got only the
    /// first, so nobody was told. Asserted at the source because the failure is
    /// silence, which no runtime test of the happy path would notice.
    #[test]
    fn create_task_raises_a_notification() {
        let src = include_str!("boards.rs");
        let body = src
            .split("pub async fn create_task(")
            .nth(1)
            .expect("create_task handler")
            .split("\npub ")
            .next()
            .expect("handler body");
        assert!(
            body.contains("notify::raise"),
            "create_task must raise a notification so a new card reaches the \
             inbox/toasts, not just the Activity feed"
        );
        assert!(
            body.contains("\"task.created\""),
            "the notification should carry the `task.created` kind so channels \
             can route it"
        );
    }

    /// "NookOS Bootstrap" must become NOOK. The first implementation flattened
    /// the whole name and cut at five, producing NOOKO — a key nobody would
    /// choose, on every task, permanently.
    #[test]
    fn a_key_comes_from_the_first_word() {
        assert_eq!(derive_key("NookOS Bootstrap"), "NOOK");
        assert_eq!(derive_key("Engineering"), "ENGI");
        assert_eq!(derive_key("web-ui rewrite"), "WEBU");
        // A name with no usable letters still needs a key.
        assert_eq!(derive_key("  "), "BOARD");
        assert_eq!(derive_key("!!! ???"), "BOARD");
        assert_eq!(derive_key(""), "BOARD");
    }

    #[test]
    fn keys_are_uppercased_and_bounded() {
        assert_eq!(validate_key("nook").unwrap(), "NOOK");
        assert_eq!(validate_key(" web ").unwrap(), "WEB");
        assert!(validate_key("").is_err());
        assert!(validate_key("has space").is_err());
        assert!(validate_key("dash-ed").is_err());
        assert!(validate_key("ABCDEFGHIJK").is_err());
    }
}

#[utoipa::path(delete, path = "/api/v1/boards/{id}",
    operation_id = "delete_board",
    params(("id" = String, Path,)),
    responses((status = 204), (status = 404)))]
pub async fn delete_board(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<BoardId>,
) -> ApiResult<axum::http::StatusCode> {
    let res = state
        .db
        .exec(
            "DELETE FROM boards WHERE id = $1 AND tenant_id = $2",
            params![id, auth.tenant_id],
        )
        .await?;
    if res == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[utoipa::path(post, path = "/api/v1/boards/{id}/columns",
    operation_id = "add_column",
    params(("id" = String, Path,)),
    request_body = CreateColumnRequest,
    responses((status = 200, body = BoardColumn), (status = 404)))]
pub async fn add_column(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(board_id): Path<BoardId>,
    Json(req): Json<CreateColumnRequest>,
) -> ApiResult<Json<BoardColumn>> {
    // Tenant must own the board.
    let owned: Option<BoardId> = state
        .db
        .query_scalar_opt(
            "SELECT id FROM boards WHERE id = $1 AND tenant_id = $2",
            params![board_id, auth.tenant_id],
        )
        .await?;
    if owned.is_none() {
        return Err(ApiError::NotFound);
    }
    let max_pos: Option<i32> = state
        .db
        .query_scalar(
            "SELECT max(position) FROM board_columns WHERE board_id = $1",
            params![board_id],
        )
        .await?;
    let col: BoardColumn = state
        .db
        .query_one(
            "INSERT INTO board_columns (id, board_id, name, position)
         VALUES ($1, $2, $3, $4) RETURNING *",
            params![
                ColumnId::new(),
                board_id,
                &req.name,
                max_pos.unwrap_or(-1) + 1
            ],
        )
        .await?;
    Ok(Json(col))
}

#[utoipa::path(patch, path = "/api/v1/columns/{id}",
    operation_id = "update_column",
    params(("id" = String, Path,)),
    request_body = UpdateColumnRequest,
    responses((status = 200, body = BoardColumn), (status = 404)))]
pub async fn update_column(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<ColumnId>,
    Json(req): Json<UpdateColumnRequest>,
) -> ApiResult<Json<BoardColumn>> {
    // Column must belong to a board the tenant owns.
    let col: Option<BoardColumn> = state
        .db
        .query_opt(
            "UPDATE board_columns SET
            name = COALESCE($2, name),
            position = COALESCE($3, position)
         WHERE id = $1 AND board_id IN (SELECT id FROM boards WHERE tenant_id = $4)
         RETURNING *",
            params![id, req.name, req.position, auth.tenant_id],
        )
        .await?;
    col.map(Json).ok_or(ApiError::NotFound)
}

#[utoipa::path(delete, path = "/api/v1/columns/{id}",
    operation_id = "delete_column",
    params(("id" = String, Path,)),
    responses((status = 204), (status = 404)))]
pub async fn delete_column(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<ColumnId>,
) -> ApiResult<axum::http::StatusCode> {
    // Deleting a column cascades its tasks (schema ON DELETE CASCADE).
    let res = state
        .db
        .exec(
            "DELETE FROM board_columns
         WHERE id = $1 AND board_id IN (SELECT id FROM boards WHERE tenant_id = $2)",
            params![id, auth.tenant_id],
        )
        .await?;
    if res == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

pub(crate) async fn provider_for_board(
    state: &AppState,
    tenant: TenantId,
    board: BoardId,
) -> ApiResult<String> {
    let provider: String = state
        .db
        .query_scalar_opt(
            "SELECT provider FROM boards WHERE id = $1 AND tenant_id = $2",
            params![board, tenant],
        )
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(provider)
}
