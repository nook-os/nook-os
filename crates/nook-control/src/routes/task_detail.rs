//! One whole issue: comments, relations, and the detail read the loop depends on.
//!
//! ## On agent identity
//!
//! The spec asked for `author_type = 'agent'` for MCP callers. NookOS has no
//! agent principal — `Principal` is `User | Node`, and MCP authenticates with a
//! person's token — so "this was an agent" is not something the server can
//! know. Inventing a bot identity to make the field look right would be
//! recording a fact nobody established.
//!
//! What it does instead: the author is the real caller, and a client may pass
//! `author_name` to say which tool it was ("loop-build on azul"). The
//! attribution is then honest at both levels — *this person's credential*, used
//! by *this tool* — and no permission hangs on the string, so a client lying
//! about it gains nothing.

use axum::extract::{Path, State};
use axum::Json;
use nook_types::*;

use crate::auth::{AuthCtx, Principal};
use crate::error::{ApiError, ApiResult};
use crate::services::jobs;
use crate::services::tasks;
use crate::state::AppState;

/// Why an unblock with no body is refused (MAIN-584 AC-5): the whole point of
/// riding the comment endpoint is that the ruling which released the card ends
/// up ON the card. An unblock nobody can read is a card that restarted for no
/// stated reason.
///
/// Shared with the MCP door, which enforces the same rule from the same string.
pub const UNBLOCK_NEEDS_A_RULING: &str =
    "an unblock needs a comment body — the ruling that releases the card is what goes on it";

/// Its twin for a change request (MAIN-591 AC-2). The body IS the contract the
/// builder's repair pass works from, so a bodyless request would put a pull
/// request back in the repair queue with nothing to repair.
pub const A_REQUEST_NEEDS_A_RULING: &str =
    "requesting changes needs a comment body — it is what the builder is told to fix";

// ── comments ────────────────────────────────────────────────────────────────

#[utoipa::path(get, path = "/api/v1/tasks/{id}/comments",
    operation_id = "list_comments", params(("id" = String, Path,)),
    responses((status = 200, body = [TaskComment]), (status = 404)))]
pub async fn list_comments(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(ident): Path<String>,
) -> ApiResult<Json<Vec<TaskComment>>> {
    let task = tasks::resolve_id(state.tasks.as_ref(), auth.tenant_id, &ident).await?;
    Ok(Json(comments_of(&state, task).await?))
}

pub async fn comments_of(state: &AppState, task: TaskId) -> ApiResult<Vec<TaskComment>> {
    // Oldest first: a comment thread is read as a narrative, and the loop
    // parses the latest verdict by taking the last one.
    state.tasks.comments_of(task).await
}

#[utoipa::path(get, path = "/api/v1/tasks/{id}/revisions",
    operation_id = "list_description_revisions", params(("id" = String, Path,)),
    responses((status = 200, body = [TaskDescriptionRevision]), (status = 404)))]
pub async fn list_revisions(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(ident): Path<String>,
) -> ApiResult<Json<Vec<TaskDescriptionRevision>>> {
    let task = tasks::resolve_id(state.tasks.as_ref(), auth.tenant_id, &ident).await?;
    // The stored bodies ARE the card's descriptions, so this route must refuse
    // exactly where the detail read does (MAIN-76): a private card a viewer
    // cannot see must 404 here too, or its whole description history leaks
    // through the sibling route.
    let row = state
        .tasks
        .get_row(auth.tenant_id, task)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !tasks::visible_to(&row, auth.user_id) {
        return Err(ApiError::NotFound);
    }
    // Newest first: the reader is undoing the most recent clobber (MAIN-470).
    Ok(Json(
        state
            .tasks
            .description_revisions_of(auth.tenant_id, task)
            .await?,
    ))
}

#[utoipa::path(post, path = "/api/v1/tasks/{id}/comments",
    operation_id = "create_comment", params(("id" = String, Path,)),
    request_body = CreateCommentRequest,
    responses((status = 200, body = TaskComment), (status = 400), (status = 404)))]
pub async fn create_comment(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(ident): Path<String>,
    Json(req): Json<CreateCommentRequest>,
) -> ApiResult<Json<TaskComment>> {
    // Before anything is written, so an unblock refused here leaves the card
    // exactly as it was — no comment, no label change, no stamp (MAIN-584 AC-5).
    if req.body_md.trim().is_empty() {
        return Err(ApiError::BadRequest(if req.request_changes {
            A_REQUEST_NEEDS_A_RULING.into()
        } else if req.clear_escalation {
            UNBLOCK_NEEDS_A_RULING.into()
        } else {
            "a comment needs a body".into()
        }));
    }
    let task = tasks::resolve_id(state.tasks.as_ref(), auth.tenant_id, &ident).await?;

    // Every one of AC-2's conditions is settled HERE, before the first write:
    // a refused request must leave the card exactly as it was — no comment, no
    // label, no verdict row. Resolving the pull request afterwards would post
    // the ruling to the card and then discover there was nowhere to send it.
    let changes = if req.request_changes {
        let row = state
            .tasks
            .get_row(auth.tenant_id, task)
            .await?
            .ok_or(ApiError::NotFound)?;
        let target = jobs::changes_request_target(&state, auth.tenant_id, &row).await?;
        let head = jobs::open_pr_head(&target.forge, &target.repo, target.pr).await?;
        Some((row, target, head))
    } else {
        None
    };

    // See the module note. A node is `system` because a machine reporting on
    // its own work is not a person; a user token is `user` even when a tool is
    // driving it, because that is who authorised it.
    let (author_type, author_id) = match auth.principal {
        Principal::Node(_) => ("system", None),
        Principal::User => ("user", Some(auth.user_id.0)),
    };
    let name = match req.author_name.as_deref().map(str::trim) {
        Some(n) if !n.is_empty() => n.chars().take(80).collect::<String>(),
        _ => display_name(&state, auth.user_id).await,
    };

    let row = state
        .tasks
        .create_comment(crate::repo::tasks::NewComment {
            tenant: auth.tenant_id,
            task,
            author_type: author_type.to_string(),
            author_id,
            author_name: name.clone(),
            body_md: req.body_md.clone(),
        })
        .await?;

    tasks::record_comment_created(
        &state,
        auth.tenant_id,
        task,
        auth.user_id,
        &name,
        &req.body_md,
    )
    .await?;

    // After the card comment, for MAIN-584's reason read the other way round:
    // the ruling the builder is sent to read is on the card before the pull
    // request is put back in the repair queue that will send it.
    if let Some((row, target, head)) = changes {
        jobs::request_changes(
            &state,
            &target.forge,
            &target.repo,
            jobs::HumanChangesRequest {
                tenant: auth.tenant_id,
                actor: auth.user_id,
                task: &row,
                workspace: target.workspace,
                pr: target.pr,
                head: &head,
                body: &req.body_md,
            },
        )
        .await?;
    }

    // The ruling is on the card BEFORE the card is restarted: a run raised
    // against a card whose reason is not yet written would read a stop with no
    // answer under it (MAIN-584 AC-4).
    if req.clear_escalation {
        tasks::unblock(&state, auth.tenant_id, auth.user_id, task).await?;
    }

    state.registry.publish(
        auth.tenant_id,
        nook_proto::UiEvent::TaskChanged { task_id: task },
    );
    Ok(Json(row))
}

#[utoipa::path(patch, path = "/api/v1/comments/{id}",
    operation_id = "update_comment", params(("id" = String, Path,)),
    request_body = UpdateCommentRequest,
    responses((status = 200, body = TaskComment), (status = 403), (status = 404)))]
pub async fn update_comment(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<uuid::Uuid>,
    Json(req): Json<UpdateCommentRequest>,
) -> ApiResult<Json<TaskComment>> {
    owned_comment(&state, &auth, id).await?;
    let row = state
        .tasks
        .update_comment(id, auth.tenant_id, &req.body_md)
        .await?;
    state.registry.publish(
        auth.tenant_id,
        nook_proto::UiEvent::TaskChanged {
            task_id: row.task_id,
        },
    );
    Ok(Json(row))
}

#[utoipa::path(delete, path = "/api/v1/comments/{id}",
    operation_id = "delete_comment", params(("id" = String, Path,)),
    responses((status = 204), (status = 403), (status = 404)))]
pub async fn delete_comment(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<uuid::Uuid>,
) -> ApiResult<axum::http::StatusCode> {
    let task = owned_comment(&state, &auth, id).await?;
    state.tasks.delete_comment(id, auth.tenant_id).await?;
    // The comment's files go with it, bytes included (MAIN-533 AC-7). No
    // foreign key could do this: `task_attachments.parent_id` is polymorphic,
    // and the objects live outside the database entirely.
    crate::services::attachments::purge_comment(&state, auth.tenant_id, id).await?;
    state.registry.publish(
        auth.tenant_id,
        nook_proto::UiEvent::TaskChanged { task_id: task },
    );
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Editing is the author's own right, and nobody else's.
///
/// Returns the comment's task so callers can publish a change for it.
async fn owned_comment(state: &AppState, auth: &AuthCtx, id: uuid::Uuid) -> ApiResult<TaskId> {
    let row = state.tasks.comment_author(id, auth.tenant_id).await?;
    let (author_id, task) = row.ok_or(ApiError::NotFound)?;
    if author_id != Some(auth.user_id.0) {
        return Err(ApiError::ForbiddenMsg(
            "only the author can edit or delete a comment".into(),
        ));
    }
    Ok(task)
}

async fn display_name(state: &AppState, user: UserId) -> String {
    // A user's display name is identity data, so it comes from that aggregate's
    // repository rather than a second copy of the query here (MAIN-246/249).
    state
        .identity
        .get_user(user)
        .await
        .ok()
        .flatten()
        .map(|u| u.display_name)
        .unwrap_or_else(|| "unknown".into())
}

// ── relations ───────────────────────────────────────────────────────────────

#[utoipa::path(post, path = "/api/v1/tasks/{id}/relations",
    operation_id = "create_relation", params(("id" = String, Path,)),
    request_body = CreateRelationRequest,
    responses((status = 200, body = TaskRelation), (status = 409)))]
pub async fn create_relation(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(ident): Path<String>,
    Json(req): Json<CreateRelationRequest>,
) -> ApiResult<Json<TaskRelation>> {
    let from = tasks::resolve_id(state.tasks.as_ref(), auth.tenant_id, &ident).await?;
    let to = tasks::resolve_id(
        state.tasks.as_ref(),
        auth.tenant_id,
        &req.to_task.to_string(),
    )
    .await?;
    Ok(Json(
        link(&state, auth.tenant_id, auth.user_id, from, to, &req.kind).await?,
    ))
}

/// Create a relation. Shared with MCP so both doors enforce the same rules —
/// notably the cycle check, which is not a nicety: a ring of `blocks` edges is
/// a set of tasks none of which can ever be picked up.
pub async fn link(
    state: &AppState,
    tenant: TenantId,
    viewer: UserId,
    from: TaskId,
    to: TaskId,
    kind: &str,
) -> ApiResult<TaskRelation> {
    const KINDS: [&str; 3] = ["blocks", "relates", "duplicates"];
    if !KINDS.contains(&kind) {
        return Err(ApiError::BadRequest(format!(
            "{kind:?} is not a relation kind — expected one of {}",
            KINDS.join(", ")
        )));
    }
    if from == to {
        return Err(ApiError::BadRequest(
            "a task cannot relate to itself".into(),
        ));
    }

    // Both ends must be visible to the caller (MAIN-76): a non-owner must not be
    // able to link to — and thereby confirm the existence of, or leak into their
    // own detail — a private task they neither created nor are assigned. A
    // private endpoint they cannot see is refused as NotFound.
    for end in [from, to] {
        let t = state
            .tasks
            .get_row(tenant, end)
            .await?
            .ok_or(ApiError::NotFound)?;
        if !tasks::visible_to(&t, viewer) {
            return Err(ApiError::NotFound);
        }
    }

    // A blocks-cycle is a deadlock nothing can ever pick up: every task in the
    // ring waits on another member forever. Cheaper to refuse than to explain
    // later why the queue is permanently empty.
    if kind == "blocks" && reaches(state, to, from).await? {
        return Err(ApiError::Conflict(
            "that would create a cycle: the blocked task already blocks this one, \
             directly or through a chain, and neither could ever start"
                .into(),
        ));
    }

    let row = state.tasks.upsert_relation(tenant, from, to, kind).await?;

    state
        .registry
        .publish(tenant, nook_proto::UiEvent::TaskChanged { task_id: from });
    Ok(row)
}

/// Can `start` reach `target` by following `blocks` edges?
///
/// Recursive in SQL rather than in a loop of round trips: the depth is unknown
/// and each hop would otherwise be a query. `UNION` (not `UNION ALL`) is what
/// terminates it — an existing cycle in the data would otherwise make the walk
/// itself run forever.
async fn reaches(state: &AppState, start: TaskId, target: TaskId) -> ApiResult<bool> {
    state.tasks.blocks_reaches(start, target).await
}

#[utoipa::path(delete, path = "/api/v1/relations/{id}",
    operation_id = "delete_relation", params(("id" = String, Path,)),
    responses((status = 204), (status = 404)))]
pub async fn delete_relation(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<uuid::Uuid>,
) -> ApiResult<axum::http::StatusCode> {
    let res = state.tasks.delete_relation(id, auth.tenant_id).await?;
    if res == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

// ── the whole issue ─────────────────────────────────────────────────────────

#[utoipa::path(get, path = "/api/v1/tasks/{id}",
    operation_id = "get_task", params(("id" = String, Path,)),
    responses((status = 200, body = TaskDetail), (status = 404)))]
pub async fn get_task(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(ident): Path<String>,
) -> ApiResult<Json<TaskDetail>> {
    let id = tasks::resolve_id(state.tasks.as_ref(), auth.tenant_id, &ident).await?;
    Ok(Json(
        detail(&state, auth.tenant_id, auth.user_id, id).await?,
    ))
}

pub async fn detail(
    state: &AppState,
    tenant: TenantId,
    viewer: UserId,
    id: TaskId,
) -> ApiResult<TaskDetail> {
    let task = state
        .tasks
        .get_row(tenant, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    // MAIN-76: a private card is a 404 for anyone but its creator or assignee.
    if !tasks::visible_to(&task, viewer) {
        return Err(ApiError::NotFound);
    }
    let task = tasks::enrich_one(
        state.tasks.as_ref(),
        &state.cfg.public_base_url,
        viewer,
        task,
    )
    .await?;

    let related = related_tasks(state, viewer, id).await?;
    let blocked_by: Vec<RelatedTask> = related
        .iter()
        .filter(|r| r.kind == "blocked_by")
        .cloned()
        .collect();
    let blocking: Vec<RelatedTask> = related
        .iter()
        .filter(|r| r.kind == "blocking")
        .cloned()
        .collect();
    let other: Vec<RelatedTask> = related
        .iter()
        .filter(|r| r.kind != "blocked_by" && r.kind != "blocking")
        .cloned()
        .collect();

    // Derived, never stored. A blocker is resolved when its column type says
    // the work is finished or abandoned — so moving a blocker to Done unblocks
    // this task with no write here at all, and there is no flag to drift.
    let is_blocked = blocked_by
        .iter()
        .any(|r| r.column_type != "completed" && r.column_type != "canceled");

    // An epic's detail carries its children (MAIN-81); anything else has none.
    // `column_type` lets a reader compute done/total inline. Ordered by priority
    // then age, matching the board's own sense of "what to look at first".
    // Filtered by visibility (MAIN-76): a private child the viewer neither
    // created nor is assigned must not leak its title/key through epic detail —
    // the same predicate the list/board reads enforce.
    let children: Vec<EpicChild> = if task.type_ == "epic" {
        state.tasks.epic_children(id, viewer).await?
    } else {
        Vec::new()
    };

    Ok(TaskDetail {
        task,
        comments: comments_of(state, id).await?,
        blocked_by,
        blocking,
        related: other,
        is_blocked,
        children,
    })
}

/// Both directions in one query, with `kind` rewritten to the reader's point of
/// view: an edge "A blocks B" is `blocking` when you asked about A and
/// `blocked_by` when you asked about B. The raw direction is a fact about the
/// row; what a person needs is which side they are on.
async fn related_tasks(
    state: &AppState,
    viewer: UserId,
    id: TaskId,
) -> ApiResult<Vec<RelatedTask>> {
    // The OTHER side of each relation is filtered by visibility (MAIN-76): a
    // private task the viewer neither created nor is assigned must not surface
    // its title/key through a relation on a task they can see.
    let rows = state.tasks.related_tasks(id, viewer).await?;
    Ok(rows)
}
