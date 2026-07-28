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
use nook_db::{CiMatch, Postgres, TypeMapping};
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
    // One definition, shared with the node-ownership guard (MAIN-130).
    crate::auth::person_id_of(state, auth.user_id).await
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
/// the encrypted bytes never leave this module without going through `into_note`.
/// `sealed_salt`/`sealed_verifier` are set only on sealed notes (MAIN-100).
#[derive(sqlx::FromRow)]
struct NoteRow {
    id: UserNoteId,
    folder_id: Option<UserNoteFolderId>,
    title: String,
    content_enc: Vec<u8>,
    sealed_salt: Option<Vec<u8>>,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

impl NoteRow {
    /// Turn a row into the API note. A sealed row (its `sealed_salt` set) yields
    /// the client-decrypt blob — the server peels only its own vault wrap, never
    /// the seal itself — while an unsealed row yields the decrypted `content_md`.
    fn into_note(self, vault: &Vault) -> ApiResult<UserNote> {
        let NoteRow {
            id,
            folder_id,
            title,
            content_enc,
            sealed_salt,
            created_at,
            updated_at,
        } = self;
        let (content_md, sealed, blob) = match sealed_salt {
            Some(salt) => {
                // `content_enc` is the client's sealed ciphertext under the vault
                // wrap; peel the wrap to hand the still-sealed bytes back.
                let ciphertext = vault.decrypt(&content_enc).map_err(ApiError::Internal)?;
                let blob = SealedBlob {
                    salt: b64(&salt),
                    iterations: crate::crypto::KDF_ITERATIONS,
                    ciphertext: b64(&ciphertext),
                };
                (None, true, Some(blob))
            }
            None => {
                let md = vault
                    .decrypt_string(&content_enc)
                    .map_err(ApiError::Internal)?;
                (Some(md), false, None)
            }
        };
        Ok(UserNote {
            id,
            folder_id,
            title,
            content_md,
            sealed,
            blob,
            created_at,
            updated_at,
        })
    }
}

/// base64 (standard) encode — the wire form for the seal blob's byte fields.
fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Decode a base64 seal field, naming it in the error so a bad blob is obvious.
fn unb64(s: &str, what: &str) -> ApiResult<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s.as_bytes())
        .map_err(|_| ApiError::BadRequest(format!("{what} is not valid base64")))
}

/// Confirm a passphrase against the person's notebook vault before it seals or
/// unseals anything — the person-scoped twin of gitops' app-password check.
/// 428 when no vault is set yet, 403 on a wrong passphrase.
async fn require_person_app_password(
    state: &AppState,
    person: Uuid,
    passphrase: &str,
) -> ApiResult<()> {
    let row: Option<(Vec<u8>, Vec<u8>)> =
        sqlx::query_as("SELECT kdf_salt, verifier FROM person_vaults WHERE person_id = $1")
            .bind(person)
            .fetch_optional(&state.db)
            .await?;
    let Some((salt, verifier)) = row else {
        return Err(ApiError::SetupRequired(
            "set a notebook app password before sealing notes".into(),
        ));
    };
    if !crate::crypto::verify_passphrase(passphrase, &salt, &verifier) {
        return Err(ApiError::Forbidden);
    }
    Ok(())
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
    Ok(Json(
        list_notes_for(&state, person, query.q.as_deref().unwrap_or_default()).await?,
    ))
}

// ── Person-scoped service paths ────────────────────────────────────────────
// The six functions below take a resolved `person` (a `users.person_id`) and
// carry all the validation / encryption / decrypt logic. The route handlers
// above resolve the person from `AuthCtx` and delegate here; the MCP notebook
// tools (MAIN-102) resolve the person from the caller's own OIDC identity and
// call the identical functions, so both surfaces share one implementation
// (never fresh SQL) and MAIN-84's name/cycle rules and the seal exclusion apply
// to both.

/// List a person's note summaries, optionally filtered by a case-insensitive
/// substring over title + folder path (`q` empty = all). Bodies stay encrypted
/// and are never searched.
pub(crate) async fn list_notes_for(
    state: &AppState,
    person: Uuid,
    q: &str,
) -> ApiResult<Vec<UserNoteSummary>> {
    // Case-insensitive search routed through the ci_match seam (MAIN-203
    // exemplar): the Postgres arm emits the same `ILIKE '%' || $2 || '%'` as
    // before; the bound term stays in $2. Behavior is bit-identical.
    let rows: Vec<UserNoteSummary> = sqlx::query_as(&format!(
        r#"
        WITH RECURSIVE folder_path AS (
            SELECT id, {name_cast} AS path, parent_id
            FROM user_note_folders
            WHERE person_id = $1 AND parent_id IS NULL
          UNION ALL
            SELECT f.id, fp.path || '/' || f.name, f.parent_id
            FROM user_note_folders f
            JOIN folder_path fp ON f.parent_id = fp.id
        )
        SELECT n.id, n.folder_id, n.title,
               COALESCE(fp.path, '') AS path,
               (n.sealed_salt IS NOT NULL) AS sealed,
               n.created_at, n.updated_at
        FROM user_notes n
        LEFT JOIN folder_path fp ON fp.id = n.folder_id
        WHERE n.person_id = $1
          AND ($2 = ''
               OR {title_match}
               OR {path_match})
        ORDER BY n.updated_at DESC
        "#,
        name_cast = Postgres.cast("name", "text"),
        title_match = Postgres.ci_match("n.title", "'%' || $2 || '%'"),
        path_match = Postgres.ci_match("COALESCE(fp.path, '')", "'%' || $2 || '%'"),
    ))
    .bind(person)
    .bind(q)
    .fetch_all(&state.db)
    .await?;
    Ok(rows)
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
    Ok(Json(get_note_for(&state, person, id).await?))
}

/// Fetch and decrypt one of the person's notes (404 if not theirs). A sealed
/// note comes back with `content_md: None` and its still-client-encrypted blob.
pub(crate) async fn get_note_for(
    state: &AppState,
    person: Uuid,
    id: UserNoteId,
) -> ApiResult<UserNote> {
    let row: Option<NoteRow> =
        sqlx::query_as("SELECT * FROM user_notes WHERE id = $1 AND person_id = $2")
            .bind(id)
            .bind(person)
            .fetch_optional(&state.db)
            .await?;
    row.ok_or(ApiError::NotFound)?.into_note(&state.vault)
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
    Ok(Json(create_note_for(&state, person, req).await?))
}

/// Create a note for the person: validates the title (MAIN-84), checks folder
/// ownership, encrypts the body, and returns the decrypted note.
pub(crate) async fn create_note_for(
    state: &AppState,
    person: Uuid,
    req: CreateUserNote,
) -> ApiResult<UserNote> {
    validate_name(&req.title, "note title")?;
    if let Some(folder) = req.folder_id {
        owned_folder(state, person, folder).await?;
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
    row.into_note(&state.vault)
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
    Ok(Json(update_note_for(&state, person, id, req).await?))
}

/// Update one of the person's notes (title / body / move). Enforces the
/// title-blank and seal-body rules; encrypts a new body only when supplied.
pub(crate) async fn update_note_for(
    state: &AppState,
    person: Uuid,
    id: UserNoteId,
    req: UpdateUserNote,
) -> ApiResult<UserNote> {
    // A blank title on update is a 400, not a silent COALESCE no-op (AC-2); an
    // omitted title (None) still leaves the name unchanged.
    if let Some(title) = &req.title {
        validate_name(title, "note title")?;
    }
    // A move must target one of the person's own folders (or root).
    if let Some(Some(folder)) = req.folder_id {
        owned_folder(state, person, folder).await?;
    }
    // A sealed note's body may only change through the seal contract — a plain
    // body PATCH would overwrite the client's sealed blob with server-encrypted
    // plaintext, silently breaking the seal. Title/move-only edits still pass.
    if req.content_md.is_some() {
        let sealed: Option<(bool,)> = sqlx::query_as(
            "SELECT sealed_salt IS NOT NULL FROM user_notes WHERE id = $1 AND person_id = $2",
        )
        .bind(id)
        .bind(person)
        .fetch_optional(&state.db)
        .await?;
        if matches!(sealed, Some((true,))) {
            return Err(ApiError::Conflict(
                "this note is sealed — change its body through seal/unseal, not a plain update"
                    .into(),
            ));
        }
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
    let row: Option<NoteRow> = sqlx::query_as(&format!(
        "UPDATE user_notes SET
            title = COALESCE($3, title),
            content_enc = COALESCE($4, content_enc),
            folder_id = CASE WHEN $5 THEN $6 ELSE folder_id END,
            updated_at = {}
         WHERE id = $1 AND person_id = $2 RETURNING *",
        Postgres.now()
    ))
    .bind(id)
    .bind(person)
    .bind(&req.title)
    .bind(enc)
    .bind(set_folder)
    .bind(folder_val)
    .fetch_optional(&state.db)
    .await?;
    row.ok_or(ApiError::NotFound)?.into_note(&state.vault)
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
    delete_note_for(&state, person, id).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Delete one of the person's notes; `NotFound` if it isn't theirs.
pub(crate) async fn delete_note_for(
    state: &AppState,
    person: Uuid,
    id: UserNoteId,
) -> ApiResult<()> {
    let done = sqlx::query("DELETE FROM user_notes WHERE id = $1 AND person_id = $2")
        .bind(id)
        .bind(person)
        .execute(&state.db)
        .await?;
    if done.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(())
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
    Ok(Json(list_folders_for(&state, person).await?))
}

/// List the person's folders (read-only; alphabetical).
pub(crate) async fn list_folders_for(
    state: &AppState,
    person: Uuid,
) -> ApiResult<Vec<UserNoteFolder>> {
    let folders: Vec<UserNoteFolder> =
        sqlx::query_as("SELECT * FROM user_note_folders WHERE person_id = $1 ORDER BY name")
            .bind(person)
            .fetch_all(&state.db)
            .await?;
    Ok(folders)
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
    let folder: Option<UserNoteFolder> = sqlx::query_as(&format!(
        "UPDATE user_note_folders SET
            name = COALESCE($3, name),
            parent_id = CASE WHEN $4 THEN $5 ELSE parent_id END,
            updated_at = {}
         WHERE id = $1 AND person_id = $2 RETURNING *",
        Postgres.now()
    ))
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

// ── Person vault + note sealing (MAIN-100) ─────────────────────────────────────
//
// An OPTIONAL per-note zero-knowledge seal: the client encrypts the body under a
// key derived from the person's app password and sends only ciphertext, so the
// server stores a blob it cannot open (a database dump plus SECRETS_KEY reveals
// nothing without the password, which is never stored). The person vault is the
// person-scoped twin of `user_vaults` (routes/vault.rs), set once. Titles stay
// plaintext, so search over sealed notes keeps working (AC-5).

/// Has this person set their notebook app password yet?
#[utoipa::path(get, path = "/api/v1/notebook/vault/status",
    operation_id = "notebook_vault_status",
    responses((status = 200, body = NotebookVaultStatus)))]
pub async fn vault_status(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<NotebookVaultStatus>> {
    auth.require_user()?;
    let person = person_id_for(&state, &auth).await?;
    let row: Option<(chrono::DateTime<chrono::Utc>,)> =
        sqlx::query_as("SELECT created_at FROM person_vaults WHERE person_id = $1")
            .bind(person)
            .fetch_optional(&state.db)
            .await?;
    Ok(Json(NotebookVaultStatus {
        configured: row.is_some(),
        created_at: row.map(|(t,)| t),
    }))
}

/// Set the notebook app password. Once only — a second attempt is a conflict,
/// not an overwrite, so a stray call can never orphan sealed notes.
#[utoipa::path(post, path = "/api/v1/notebook/vault/passphrase",
    operation_id = "notebook_set_vault_passphrase",
    request_body = SetVaultPassphraseRequest,
    responses((status = 200, body = NotebookVaultStatus), (status = 409)))]
pub async fn set_vault_passphrase(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<SetVaultPassphraseRequest>,
) -> ApiResult<Json<NotebookVaultStatus>> {
    auth.require_user()?;
    let person = person_id_for(&state, &auth).await?;
    if req.passphrase.chars().count() < 8 {
        return Err(ApiError::BadRequest(
            "app password must be at least 8 characters".into(),
        ));
    }
    let existing: Option<(Uuid,)> =
        sqlx::query_as("SELECT person_id FROM person_vaults WHERE person_id = $1")
            .bind(person)
            .fetch_optional(&state.db)
            .await?;
    if existing.is_some() {
        return Err(ApiError::Conflict(
            "an app password is already set and cannot be changed".into(),
        ));
    }
    let (salt, verifier) = crate::crypto::passphrase_verifier(&req.passphrase);
    sqlx::query("INSERT INTO person_vaults (person_id, kdf_salt, verifier) VALUES ($1, $2, $3)")
        .bind(person)
        .bind(&salt)
        .bind(&verifier)
        .execute(&state.db)
        .await?;
    Ok(Json(NotebookVaultStatus {
        configured: true,
        created_at: Some(chrono::Utc::now()),
    }))
}

/// Check the app password without decrypting anything — lets the client unlock
/// (and hold the password for sealing/unsealing) with a clear yes/no.
#[utoipa::path(post, path = "/api/v1/notebook/vault/verify",
    operation_id = "notebook_verify_vault_passphrase",
    request_body = SetVaultPassphraseRequest,
    responses((status = 204), (status = 403), (status = 404)))]
pub async fn verify_vault_passphrase(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<SetVaultPassphraseRequest>,
) -> ApiResult<axum::http::StatusCode> {
    auth.require_user()?;
    let person = person_id_for(&state, &auth).await?;
    let row: Option<(Vec<u8>, Vec<u8>)> =
        sqlx::query_as("SELECT kdf_salt, verifier FROM person_vaults WHERE person_id = $1")
            .bind(person)
            .fetch_optional(&state.db)
            .await?;
    let (salt, verifier) = row.ok_or(ApiError::NotFound)?;
    if crate::crypto::verify_passphrase(&req.passphrase, &salt, &verifier) {
        Ok(axum::http::StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::Forbidden)
    }
}

/// Seal a note: store the client-produced sealed blob, authorized by the app
/// password against the person vault. The server never receives the plaintext —
/// only the already-sealed ciphertext, which it additionally vault-wraps.
#[utoipa::path(post, path = "/api/v1/notebook/notes/{id}/seal",
    operation_id = "notebook_seal_note",
    params(("id" = String, Path,)),
    request_body = SealNoteRequest,
    responses((status = 200, body = UserNote), (status = 403), (status = 404), (status = 428)))]
pub async fn seal_note(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<UserNoteId>,
    Json(req): Json<SealNoteRequest>,
) -> ApiResult<Json<UserNote>> {
    let person = person_id_for(&state, &auth).await?;
    // 428 when no vault, 403 on a wrong app password — before touching the note.
    require_person_app_password(&state, person, &req.passphrase).await?;
    let salt = unb64(&req.salt, "salt")?;
    let verifier = unb64(&req.verifier, "verifier")?;
    let ciphertext = unb64(&req.ciphertext, "ciphertext")?;
    // The blob must actually open with this app password, or the note would be
    // sealed under a key its owner can never reproduce.
    if !crate::crypto::verify_passphrase(&req.passphrase, &salt, &verifier) {
        return Err(ApiError::BadRequest(
            "the sealed blob does not match your app password".into(),
        ));
    }
    // Wrap the client ciphertext under the server vault at rest — a DB dump plus
    // SECRETS_KEY still cannot open it without the app password.
    let enc = state
        .vault
        .encrypt(&ciphertext)
        .map_err(ApiError::Internal)?;
    let row: Option<NoteRow> = sqlx::query_as(&format!(
        "UPDATE user_notes
         SET content_enc = $3, sealed_salt = $4, sealed_verifier = $5, updated_at = {}
         WHERE id = $1 AND person_id = $2 RETURNING *",
        Postgres.now()
    ))
    .bind(id)
    .bind(person)
    .bind(&enc)
    .bind(&salt)
    .bind(&verifier)
    .fetch_optional(&state.db)
    .await?;
    row.ok_or(ApiError::NotFound)?
        .into_note(&state.vault)
        .map(Json)
}

/// Unseal a note: the client decrypted the sealed body locally and sends back
/// the recovered plaintext, which converts the row to a normal server-encrypted
/// note. Only a currently-sealed note converts.
#[utoipa::path(post, path = "/api/v1/notebook/notes/{id}/unseal",
    operation_id = "notebook_unseal_note",
    params(("id" = String, Path,)),
    request_body = UnsealNoteRequest,
    responses((status = 200, body = UserNote), (status = 403), (status = 404), (status = 428)))]
pub async fn unseal_note(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<UserNoteId>,
    Json(req): Json<UnsealNoteRequest>,
) -> ApiResult<Json<UserNote>> {
    let person = person_id_for(&state, &auth).await?;
    require_person_app_password(&state, person, &req.passphrase).await?;
    let enc = state
        .vault
        .encrypt(req.content_md.as_bytes())
        .map_err(ApiError::Internal)?;
    let row: Option<NoteRow> = sqlx::query_as(&format!(
        "UPDATE user_notes
         SET content_enc = $3, sealed_salt = NULL, sealed_verifier = NULL, updated_at = {}
         WHERE id = $1 AND person_id = $2 AND sealed_salt IS NOT NULL RETURNING *",
        Postgres.now()
    ))
    .bind(id)
    .bind(person)
    .bind(&enc)
    .fetch_optional(&state.db)
    .await?;
    row.ok_or(ApiError::NotFound)?
        .into_note(&state.vault)
        .map(Json)
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
