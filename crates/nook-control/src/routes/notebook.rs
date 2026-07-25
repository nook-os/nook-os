//! The personal notebook (MAIN-66): a person-owned, cross-org, private store of
//! folders + markdown notes, encrypted at rest.
//!
//! **Invisible to operators by construction.** Access here is ONE predicate —
//! the owning `person_id` — and nothing else. This module deliberately never
//! reaches for the permission catalog or the visibility policy: there is no
//! role, binding, or policy field that can grant a second person (operator,
//! org admin, anyone) a view of someone's notebook, because none is consulted.
//! That is not a rule to remember, it is the shape of the code — and a
//! source-text test at the bottom of this file fails the build if the words
//! `perm` or `policy` ever appear in it. If you are tempted to add an operator
//! read path, that is a different, sealed resource by design; do not.
//!
//! Note bodies are stored as `content_enc` ciphertext (AES-256-GCM under
//! `SECRETS_KEY`, via `state.vault`); a raw `SELECT` yields bytea, never text.
//! Titles, folder names, and the derived folder path are plaintext metadata the
//! server needs for the tree, listing, and search.

use axum::extract::{Path, Query, State};
use axum::Json;
use nook_types::*;
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::AuthCtx;
use crate::crypto::Vault;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Resolve the caller's `person_id` — the owner key for every notebook row.
///
/// A user is per-tenant; the person behind them is the same across every org
/// they belong to, which is exactly why the notebook follows the person and
/// looks identical whichever tenant they signed into (AC-3). This is the whole
/// access model: scope every query by the value this returns and nothing else.
async fn person_id_for(state: &AppState, auth: &AuthCtx) -> ApiResult<Uuid> {
    let row: Option<(Uuid,)> = sqlx::query_as("SELECT person_id FROM users WHERE id = $1")
        .bind(auth.user_id)
        .fetch_optional(&state.db)
        .await?;
    row.map(|(p,)| p).ok_or(ApiError::NotFound)
}

/// The most a title or folder name may be — measured after trim, by Unicode
/// scalar count. A title is a label for a tree, not a document (MAIN-84 AC-3).
const MAX_NAME_LEN: usize = 200;

/// Reject a blank or over-long title / folder name (MAIN-84 AC-2, AC-3).
///
/// Trims ONLY to decide accept/reject — the value is stored exactly as sent
/// (NG-2), so leading/trailing spaces a person typed on purpose survive. On
/// update, callers apply this only when a name was supplied, so an omitted name
/// still means "leave unchanged".
fn validate_name(value: &str, what: &str) -> ApiResult<()> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ApiError::BadRequest(format!("a {what} cannot be blank")));
    }
    let len = trimmed.chars().count();
    if len > MAX_NAME_LEN {
        return Err(ApiError::BadRequest(format!(
            "a {what} is at most {MAX_NAME_LEN} characters (got {len})"
        )));
    }
    Ok(())
}

/// Would moving `folder` under `new_parent` create a cycle (MAIN-84 AC-1)?
///
/// True when `new_parent` is `folder` itself or any descendant of it. Decided by
/// climbing `new_parent`'s ancestor chain — within the person's own folders —
/// and asking whether `folder` sits on it. Subsumes the self-parent case (the
/// chain starts at `new_parent`, so `new_parent == folder` is caught at once).
async fn would_cycle(
    state: &AppState,
    person: Uuid,
    folder: UserNoteFolderId,
    new_parent: UserNoteFolderId,
) -> ApiResult<bool> {
    let (cycles,): (bool,) = sqlx::query_as(
        "WITH RECURSIVE ancestors AS (
             SELECT id, parent_id FROM user_note_folders
             WHERE id = $1 AND person_id = $2
             UNION ALL
             SELECT f.id, f.parent_id FROM user_note_folders f
             JOIN ancestors a ON f.id = a.parent_id AND f.person_id = $2
         )
         SELECT EXISTS(SELECT 1 FROM ancestors WHERE id = $3)",
    )
    .bind(new_parent)
    .bind(person)
    .bind(folder)
    .fetch_one(&state.db)
    .await?;
    Ok(cycles)
}

/// A note row straight from the table — body still ciphertext. Kept private so
/// the encrypted bytes never leave this module without going through `decrypt`.
#[derive(sqlx::FromRow)]
struct NoteRow {
    id: UserNoteId,
    folder_id: Option<UserNoteFolderId>,
    title: String,
    content_enc: Vec<u8>,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

impl NoteRow {
    fn decrypt(self, vault: &Vault) -> ApiResult<UserNote> {
        Ok(UserNote {
            id: self.id,
            folder_id: self.folder_id,
            title: self.title,
            content_md: vault
                .decrypt_string(&self.content_enc)
                .map_err(ApiError::Internal)?,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

/// Confirm a folder belongs to this person, so a note/folder can never be moved
/// under another person's folder by guessing an id. Returns the id back for
/// convenient binding, or NotFound if it is not theirs (or does not exist).
async fn owned_folder(
    state: &AppState,
    person: Uuid,
    folder: UserNoteFolderId,
) -> ApiResult<UserNoteFolderId> {
    let found: Option<(UserNoteFolderId,)> =
        sqlx::query_as("SELECT id FROM user_note_folders WHERE id = $1 AND person_id = $2")
            .bind(folder)
            .bind(person)
            .fetch_optional(&state.db)
            .await?;
    found.map(|(id,)| id).ok_or(ApiError::NotFound)
}

// ── Notes ────────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
pub struct NoteListQuery {
    /// Case-insensitive substring over title + folder path. Absent = list all.
    pub q: Option<String>,
}

#[utoipa::path(get, path = "/api/v1/notebook/notes",
    operation_id = "notebook_list_notes",
    params(("q" = Option<String>, Query,)),
    responses((status = 200, body = [UserNoteSummary])))]
pub async fn list_notes(
    State(state): State<AppState>,
    auth: AuthCtx,
    Query(query): Query<NoteListQuery>,
) -> ApiResult<Json<Vec<UserNoteSummary>>> {
    let person = person_id_for(&state, &auth).await?;
    let q = query.q.unwrap_or_default();
    // The folder path ("Work/Ideas") is built with a recursive CTE scoped to the
    // person, so search covers the plaintext path as well as the title (AC-6).
    // Body content is encrypted and deliberately NOT searchable.
    let rows: Vec<UserNoteSummary> = sqlx::query_as(
        r#"
        WITH RECURSIVE folder_path AS (
            SELECT id, name::text AS path, parent_id
            FROM user_note_folders
            WHERE person_id = $1 AND parent_id IS NULL
          UNION ALL
            SELECT f.id, fp.path || '/' || f.name, f.parent_id
            FROM user_note_folders f
            JOIN folder_path fp ON f.parent_id = fp.id
        )
        SELECT n.id, n.folder_id, n.title,
               COALESCE(fp.path, '') AS path,
               n.created_at, n.updated_at
        FROM user_notes n
        LEFT JOIN folder_path fp ON fp.id = n.folder_id
        WHERE n.person_id = $1
          AND ($2 = ''
               OR n.title ILIKE '%' || $2 || '%'
               OR COALESCE(fp.path, '') ILIKE '%' || $2 || '%')
        ORDER BY n.updated_at DESC
        "#,
    )
    .bind(person)
    .bind(&q)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

#[utoipa::path(get, path = "/api/v1/notebook/notes/{id}",
    operation_id = "notebook_get_note",
    params(("id" = String, Path,)),
    responses((status = 200, body = UserNote), (status = 404)))]
pub async fn get_note(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<UserNoteId>,
) -> ApiResult<Json<UserNote>> {
    let person = person_id_for(&state, &auth).await?;
    let row: Option<NoteRow> =
        sqlx::query_as("SELECT * FROM user_notes WHERE id = $1 AND person_id = $2")
            .bind(id)
            .bind(person)
            .fetch_optional(&state.db)
            .await?;
    row.ok_or(ApiError::NotFound)?
        .decrypt(&state.vault)
        .map(Json)
}

#[utoipa::path(post, path = "/api/v1/notebook/notes",
    operation_id = "notebook_create_note",
    request_body = CreateUserNote,
    responses((status = 200, body = UserNote)))]
pub async fn create_note(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<CreateUserNote>,
) -> ApiResult<Json<UserNote>> {
    let person = person_id_for(&state, &auth).await?;
    validate_name(&req.title, "note title")?;
    if let Some(folder) = req.folder_id {
        owned_folder(&state, person, folder).await?;
    }
    let enc = state
        .vault
        .encrypt(req.content_md.as_bytes())
        .map_err(ApiError::Internal)?;
    let row: NoteRow = sqlx::query_as(
        "INSERT INTO user_notes (id, person_id, folder_id, title, content_enc)
         VALUES ($1, $2, $3, $4, $5) RETURNING *",
    )
    .bind(UserNoteId::new())
    .bind(person)
    .bind(req.folder_id)
    .bind(&req.title)
    .bind(&enc)
    .fetch_one(&state.db)
    .await?;
    row.decrypt(&state.vault).map(Json)
}

#[utoipa::path(patch, path = "/api/v1/notebook/notes/{id}",
    operation_id = "notebook_update_note",
    params(("id" = String, Path,)),
    request_body = UpdateUserNote,
    responses((status = 200, body = UserNote), (status = 404)))]
pub async fn update_note(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<UserNoteId>,
    Json(req): Json<UpdateUserNote>,
) -> ApiResult<Json<UserNote>> {
    let person = person_id_for(&state, &auth).await?;
    // A blank title on update is a 400, not a silent COALESCE no-op (AC-2); an
    // omitted title (None) still leaves the name unchanged.
    if let Some(title) = &req.title {
        validate_name(title, "note title")?;
    }
    // A move must target one of the person's own folders (or root).
    if let Some(Some(folder)) = req.folder_id {
        owned_folder(&state, person, folder).await?;
    }
    // Encrypt a new body only when one was supplied; a title-only or move-only
    // update must not touch the ciphertext.
    let enc = match &req.content_md {
        Some(md) => Some(
            state
                .vault
                .encrypt(md.as_bytes())
                .map_err(ApiError::Internal)?,
        ),
        None => None,
    };
    // folder_id is tri-state: None = leave, Some(None) = to root, Some(Some) = set.
    let (set_folder, folder_val) = match req.folder_id {
        None => (false, None),
        Some(v) => (true, v),
    };
    let row: Option<NoteRow> = sqlx::query_as(
        "UPDATE user_notes SET
            title = COALESCE($3, title),
            content_enc = COALESCE($4, content_enc),
            folder_id = CASE WHEN $5 THEN $6 ELSE folder_id END,
            updated_at = now()
         WHERE id = $1 AND person_id = $2 RETURNING *",
    )
    .bind(id)
    .bind(person)
    .bind(&req.title)
    .bind(enc)
    .bind(set_folder)
    .bind(folder_val)
    .fetch_optional(&state.db)
    .await?;
    row.ok_or(ApiError::NotFound)?
        .decrypt(&state.vault)
        .map(Json)
}

#[utoipa::path(delete, path = "/api/v1/notebook/notes/{id}",
    operation_id = "notebook_delete_note",
    params(("id" = String, Path,)),
    responses((status = 204), (status = 404)))]
pub async fn delete_note(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<UserNoteId>,
) -> ApiResult<axum::http::StatusCode> {
    let person = person_id_for(&state, &auth).await?;
    let done = sqlx::query("DELETE FROM user_notes WHERE id = $1 AND person_id = $2")
        .bind(id)
        .bind(person)
        .execute(&state.db)
        .await?;
    if done.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

// ── Folders ──────────────────────────────────────────────────────────────────

#[utoipa::path(get, path = "/api/v1/notebook/folders",
    operation_id = "notebook_list_folders",
    responses((status = 200, body = [UserNoteFolder])))]
pub async fn list_folders(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<Vec<UserNoteFolder>>> {
    let person = person_id_for(&state, &auth).await?;
    let folders: Vec<UserNoteFolder> =
        sqlx::query_as("SELECT * FROM user_note_folders WHERE person_id = $1 ORDER BY name")
            .bind(person)
            .fetch_all(&state.db)
            .await?;
    Ok(Json(folders))
}

#[utoipa::path(post, path = "/api/v1/notebook/folders",
    operation_id = "notebook_create_folder",
    request_body = CreateUserNoteFolder,
    responses((status = 200, body = UserNoteFolder)))]
pub async fn create_folder(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<CreateUserNoteFolder>,
) -> ApiResult<Json<UserNoteFolder>> {
    let person = person_id_for(&state, &auth).await?;
    validate_name(&req.name, "folder name")?;
    if let Some(parent) = req.parent_id {
        owned_folder(&state, person, parent).await?;
    }
    let folder: UserNoteFolder = sqlx::query_as(
        "INSERT INTO user_note_folders (id, person_id, parent_id, name)
         VALUES ($1, $2, $3, $4) RETURNING *",
    )
    .bind(UserNoteFolderId::new())
    .bind(person)
    .bind(req.parent_id)
    .bind(&req.name)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(folder))
}

#[utoipa::path(patch, path = "/api/v1/notebook/folders/{id}",
    operation_id = "notebook_update_folder",
    params(("id" = String, Path,)),
    request_body = UpdateUserNoteFolder,
    responses((status = 200, body = UserNoteFolder), (status = 404)))]
pub async fn update_folder(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<UserNoteFolderId>,
    Json(req): Json<UpdateUserNoteFolder>,
) -> ApiResult<Json<UserNoteFolder>> {
    let person = person_id_for(&state, &auth).await?;
    if let Some(name) = &req.name {
        validate_name(name, "folder name")?;
    }
    if let Some(Some(parent)) = req.parent_id {
        // The parent must be the person's own folder, and the move must not put
        // the folder inside its own subtree (AC-1) — which would drop it out of
        // the root-anchored path CTE and make its notes' paths render empty. The
        // cycle check subsumes the old self-parent guard.
        owned_folder(&state, person, parent).await?;
        if would_cycle(&state, person, id, parent).await? {
            return Err(ApiError::BadRequest(
                "that move would put a folder inside its own subtree".into(),
            ));
        }
    }
    let (set_parent, parent_val) = match req.parent_id {
        None => (false, None),
        Some(v) => (true, v),
    };
    let folder: Option<UserNoteFolder> = sqlx::query_as(
        "UPDATE user_note_folders SET
            name = COALESCE($3, name),
            parent_id = CASE WHEN $4 THEN $5 ELSE parent_id END,
            updated_at = now()
         WHERE id = $1 AND person_id = $2 RETURNING *",
    )
    .bind(id)
    .bind(person)
    .bind(&req.name)
    .bind(set_parent)
    .bind(parent_val)
    .fetch_optional(&state.db)
    .await?;
    folder.map(Json).ok_or(ApiError::NotFound)
}

#[utoipa::path(delete, path = "/api/v1/notebook/folders/{id}",
    operation_id = "notebook_delete_folder",
    params(("id" = String, Path,)),
    responses((status = 204), (status = 404)))]
pub async fn delete_folder(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<UserNoteFolderId>,
) -> ApiResult<axum::http::StatusCode> {
    let person = person_id_for(&state, &auth).await?;
    // Deleting a folder REPARENTS its contents to its own parent (root if it had
    // none) — it never cascade-deletes notes (AC-4). Notes and any child folders
    // rise one level, then the folder itself goes. All in one transaction so a
    // partial failure cannot orphan anything.
    let mut tx = state.db.begin().await?;
    let parent: Option<(Option<UserNoteFolderId>,)> =
        sqlx::query_as("SELECT parent_id FROM user_note_folders WHERE id = $1 AND person_id = $2")
            .bind(id)
            .bind(person)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((parent_id,)) = parent else {
        return Err(ApiError::NotFound);
    };
    sqlx::query("UPDATE user_notes SET folder_id = $3 WHERE folder_id = $1 AND person_id = $2")
        .bind(id)
        .bind(person)
        .bind(parent_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE user_note_folders SET parent_id = $3 WHERE parent_id = $1 AND person_id = $2",
    )
    .bind(id)
    .bind(person)
    .bind(parent_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM user_note_folders WHERE id = $1 AND person_id = $2")
        .bind(id)
        .bind(person)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    /// The operator-invisibility guarantee (AC-5) is only real if this module
    /// stays free of the permission catalog and the visibility policy — access
    /// here is the `person_id` predicate and nothing else. Asserted against the
    /// source TEXT (comments stripped, so the docs above may name them to explain
    /// their absence), mirroring `auth/session_guard.rs`. If someone wires the
    /// notebook into `perm.rs` or `policy.rs`, this fails the build.
    #[test]
    fn notebook_never_consults_permissions_or_policy() {
        let src = include_str!("notebook.rs");
        let code: String = src
            .split("mod tests")
            .next()
            .expect("module body")
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for forbidden in ["perm", "policy"] {
            assert!(
                !code.contains(forbidden),
                "notebook.rs must not reference `{forbidden}` — the notebook is \
                 person-owned and operator-invisible by construction. See the \
                 module docs for why."
            );
        }
        // And it must actually scope by person, or the check above passes
        // trivially on an empty file.
        assert!(
            code.contains("person_id"),
            "the notebook must scope every query by person_id"
        );
    }
}
