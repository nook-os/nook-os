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
use crate::repo::notebook::{
    FolderDeletion, FolderEdit, NewUserNote, SealedBody, StoredUserNote, UserNoteEdit,
};
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

/// Reject a blank, over-long, or slash-carrying title / folder name (MAIN-84
/// AC-2, AC-3; MAIN-574 AC-5).
///
/// `/` is refused because a note is addressed by a slash-delimited path — a
/// name that contains the separator makes its own path unparseable, and no live
/// name contained one when this landed.
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
    if trimmed.contains('/') {
        return Err(ApiError::BadRequest(format!(
            "a {what} cannot contain '/' — it separates the parts of a note's path"
        )));
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
    let cycles = state
        .notebook
        .would_cycle(person, folder, new_parent)
        .await?;
    Ok(cycles)
}

/// The stored note (body still ciphertext) becomes the API note here, and
/// ONLY here — `StoredUserNote` is named so a caller cannot mistake it for
/// readable content, and this is the single place the vault wrap comes off.
/// `sealed_salt`/`sealed_verifier` are set only on sealed notes (MAIN-100).
trait IntoApiNote {
    /// Turn a row into the API note. A sealed row (its `sealed_salt` set) yields
    /// the client-decrypt blob — the server peels only its own vault wrap, never
    /// the seal itself — while an unsealed row yields the decrypted `content_md`.
    fn into_note(self, vault: &Vault) -> ApiResult<UserNote>;
}

impl IntoApiNote for StoredUserNote {
    fn into_note(self, vault: &Vault) -> ApiResult<UserNote> {
        let StoredUserNote {
            id,
            folder_id,
            title,
            content_enc,
            sealed_salt,
            created_at,
            updated_at,
            ..
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
    let Some(challenge) = state.vaults.person_challenge(person).await? else {
        return Err(ApiError::SetupRequired(
            "set a notebook app password before sealing notes".into(),
        ));
    };
    let (salt, verifier) = (challenge.kdf_salt, challenge.verifier);
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
    if state.notebook.owns_folder(person, folder).await? {
        Ok(folder)
    } else {
        Err(ApiError::NotFound)
    }
}

/// Refuse a folder name that is already taken under `parent` (`None` = the
/// notebook root), ignoring `except` — the folder being renamed or moved.
///
/// A 409 naming the taken name (MAIN-574 AC-4), because the alternatives are
/// both worse: a raw constraint violation reaches the client as a 500 it can
/// say nothing about, and a silent auto-suffix hands back a folder under a
/// name nobody asked for.
async fn folder_name_free(
    state: &AppState,
    person: Uuid,
    parent: Option<UserNoteFolderId>,
    name: &str,
    except: Option<UserNoteFolderId>,
) -> ApiResult<()> {
    if state
        .notebook
        .folder_name_taken(person, parent, name, except)
        .await?
    {
        return Err(taken(name, "folder"));
    }
    Ok(())
}

/// The note twin of [`folder_name_free`], keyed on the containing folder.
async fn note_title_free(
    state: &AppState,
    person: Uuid,
    folder: Option<UserNoteFolderId>,
    title: &str,
    except: Option<UserNoteId>,
) -> ApiResult<()> {
    if state
        .notebook
        .note_title_taken(person, folder, title, except)
        .await?
    {
        return Err(taken(title, "note"));
    }
    Ok(())
}

fn taken(name: &str, what: &str) -> ApiError {
    ApiError::Conflict(format!("a {what} named \"{name}\" is already here"))
}

/// Turn the unique index's own refusal into that same 409.
///
/// The check above answers the common case with a readable message; this closes
/// the window between it and the write, where a second request can take the
/// name first. Without it that race is the 500 AC-4 rules out — and the index,
/// not the check, is what actually guarantees the uniqueness.
fn conflict_if_taken(e: ApiError, name: &str, what: &str) -> ApiError {
    match &e {
        ApiError::Db(db) if db.is_unique_violation() => taken(name, what),
        _ => e,
    }
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
    let rows = state.notebook.list_note_summaries(person, q).await?;
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
    let row: Option<StoredUserNote> = state.notebook.get_note(person, id).await?;
    row.ok_or(ApiError::NotFound)?.into_note(&state.vault)
}

#[utoipa::path(post, path = "/api/v1/notebook/notes",
    operation_id = "notebook_create_note",
    request_body = CreateUserNote,
    responses((status = 200, body = UserNote), (status = 409)))]
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
    note_title_free(state, person, req.folder_id, &req.title, None).await?;
    let enc = state
        .vault
        .encrypt(req.content_md.as_bytes())
        .map_err(ApiError::Internal)?;
    let row: StoredUserNote = state
        .notebook
        .create_note(NewUserNote {
            person,
            folder_id: req.folder_id,
            title: req.title.clone(),
            content_enc: enc,
        })
        .await
        .map_err(|e| conflict_if_taken(e, &req.title, "note"))?;
    row.into_note(&state.vault)
}

#[utoipa::path(patch, path = "/api/v1/notebook/notes/{id}",
    operation_id = "notebook_update_note",
    params(("id" = String, Path,)),
    request_body = UpdateUserNote,
    responses((status = 200, body = UserNote), (status = 404), (status = 409)))]
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
    // A rename and a move are the same collision from the index's side, and
    // either one alone still needs the other half of the key — so the effective
    // pair is resolved from the stored note before it is asked about (AC-4).
    let moving_title = if req.title.is_some() || req.folder_id.is_some() {
        let current = state
            .notebook
            .get_note(person, id)
            .await?
            .ok_or(ApiError::NotFound)?;
        let title = req.title.clone().unwrap_or(current.title);
        let folder = req.folder_id.unwrap_or(current.folder_id);
        note_title_free(state, person, folder, &title, Some(id)).await?;
        Some(title)
    } else {
        None
    };
    // A sealed note's body may only change through the seal contract — a plain
    // body PATCH would overwrite the client's sealed blob with server-encrypted
    // plaintext, silently breaking the seal. Title/move-only edits still pass.
    if req.content_md.is_some() {
        let sealed = state.notebook.is_sealed(person, id).await?;
        if sealed == Some(true) {
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
    let row = state
        .notebook
        .update_note(
            person,
            id,
            UserNoteEdit {
                title: req.title.clone(),
                content_enc: enc,
                // folder is tri-state: None = leave, Some(None) = to root,
                // Some(Some) = set.
                folder: req.folder_id,
            },
        )
        .await
        // A body-only edit cannot collide, so there is no name to name.
        .map_err(|e| match &moving_title {
            Some(title) => conflict_if_taken(e, title, "note"),
            None => e,
        })?;
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
    let done = state.notebook.delete_note(person, id).await?;
    if done == 0 {
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
    let folders: Vec<UserNoteFolder> = state.notebook.list_folders(person).await?;
    Ok(folders)
}

#[utoipa::path(post, path = "/api/v1/notebook/folders",
    operation_id = "notebook_create_folder",
    request_body = CreateUserNoteFolder,
    responses((status = 200, body = UserNoteFolder), (status = 409)))]
pub async fn create_folder(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<CreateUserNoteFolder>,
) -> ApiResult<Json<UserNoteFolder>> {
    let person = person_id_for(&state, &auth).await?;
    Ok(Json(create_folder_for(&state, person, req).await?))
}

/// Create a folder for a resolved person. Shared with the MCP tool, so the name
/// rule and the "parent must be yours" check cannot differ between surfaces.
pub(crate) async fn create_folder_for(
    state: &AppState,
    person: Uuid,
    req: CreateUserNoteFolder,
) -> ApiResult<UserNoteFolder> {
    validate_name(&req.name, "folder name")?;
    if let Some(parent) = req.parent_id {
        owned_folder(state, person, parent).await?;
    }
    folder_name_free(state, person, req.parent_id, &req.name, None).await?;
    state
        .notebook
        .create_folder(person, req.parent_id, &req.name)
        .await
        .map_err(|e| conflict_if_taken(e, &req.name, "folder"))
}

#[utoipa::path(patch, path = "/api/v1/notebook/folders/{id}",
    operation_id = "notebook_update_folder",
    params(("id" = String, Path,)),
    request_body = UpdateUserNoteFolder,
    responses((status = 200, body = UserNoteFolder), (status = 404), (status = 409)))]
pub async fn update_folder(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<UserNoteFolderId>,
    Json(req): Json<UpdateUserNoteFolder>,
) -> ApiResult<Json<UserNoteFolder>> {
    let person = person_id_for(&state, &auth).await?;
    Ok(Json(update_folder_for(&state, person, id, req).await?))
}

/// Rename and/or MOVE a folder, for a resolved person. Shared with the MCP tool
/// — the cycle guard below is the reason this is extracted rather than
/// reimplemented: a second copy is a second chance to omit it.
pub(crate) async fn update_folder_for(
    state: &AppState,
    person: Uuid,
    id: UserNoteFolderId,
    req: UpdateUserNoteFolder,
) -> ApiResult<UserNoteFolder> {
    if let Some(name) = &req.name {
        validate_name(name, "folder name")?;
    }
    if let Some(Some(parent)) = req.parent_id {
        // The parent must be the person's own folder, and the move must not put
        // the folder inside its own subtree (AC-1) — which would drop it out of
        // the root-anchored path CTE and make its notes' paths render empty. The
        // cycle check subsumes the old self-parent guard.
        owned_folder(state, person, parent).await?;
        if would_cycle(state, person, id, parent).await? {
            return Err(ApiError::BadRequest(
                "that move would put a folder inside its own subtree".into(),
            ));
        }
    }
    // As for a note: a rename and a move share one collision, so the effective
    // (parent, name) comes off the stored folder before it is asked about.
    let moving_name = if req.name.is_some() || req.parent_id.is_some() {
        let current = state
            .notebook
            .get_folder(person, id)
            .await?
            .ok_or(ApiError::NotFound)?;
        let name = req.name.clone().unwrap_or(current.name);
        let parent = req.parent_id.unwrap_or(current.parent_id);
        folder_name_free(state, person, parent, &name, Some(id)).await?;
        Some(name)
    } else {
        None
    };
    let folder = state
        .notebook
        .update_folder(
            person,
            id,
            FolderEdit {
                name: req.name.clone(),
                parent: req.parent_id,
            },
        )
        .await
        .map_err(|e| match &moving_name {
            Some(name) => conflict_if_taken(e, name, "folder"),
            None => e,
        })?;
    folder.ok_or(ApiError::NotFound)
}

#[utoipa::path(delete, path = "/api/v1/notebook/folders/{id}",
    operation_id = "notebook_delete_folder",
    params(("id" = String, Path,)),
    responses((status = 204), (status = 404), (status = 409)))]
pub async fn delete_folder(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<UserNoteFolderId>,
) -> ApiResult<axum::http::StatusCode> {
    let person = person_id_for(&state, &auth).await?;
    delete_folder_for(&state, person, id).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Delete a folder for a resolved person, REPARENTING its contents to its own
/// parent (root if it had none) — it never cascade-deletes notes (AC-4). Notes
/// and any child folders rise one level, then the folder itself goes. All in one
/// transaction so a partial failure cannot orphan anything. Shared with MCP so a
/// tool cannot delete a folder by a path that skips the reparenting.
pub(crate) async fn delete_folder_for(
    state: &AppState,
    person: Uuid,
    id: UserNoteFolderId,
) -> ApiResult<()> {
    match state.notebook.delete_folder_reparenting(person, id).await? {
        FolderDeletion::Deleted => Ok(()),
        FolderDeletion::NoSuchFolder => Err(ApiError::NotFound),
        // The reparenting is a move, and a move onto a taken name is the same
        // 409 the PATCH verbs give (MAIN-574) — the delete is refused whole
        // rather than half-performed, so the folder is still there to retry.
        FolderDeletion::Collision { what, name } => Err(ApiError::Conflict(format!(
            "deleting this folder would move a {what} named \"{name}\" beside one of the \
             same name — rename one of them first"
        ))),
    }
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
    let created_at = state.vaults.person_vault_created_at(person).await?;
    Ok(Json(NotebookVaultStatus {
        configured: created_at.is_some(),
        created_at,
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
    if state.vaults.has_person_vault(person).await? {
        return Err(ApiError::Conflict(
            "an app password is already set and cannot be changed".into(),
        ));
    }
    // The passphrase never reaches the repository: only the derived, one-way
    // salt + verifier do.
    let (salt, verifier) = crate::crypto::passphrase_verifier(&req.passphrase);
    state
        .vaults
        .set_person_vault(person, salt, verifier)
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
    let challenge = state
        .vaults
        .person_challenge(person)
        .await?
        .ok_or(ApiError::NotFound)?;
    if crate::crypto::verify_passphrase(&req.passphrase, &challenge.kdf_salt, &challenge.verifier) {
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
    let row: Option<StoredUserNote> = state
        .notebook
        .seal_note(
            person,
            id,
            SealedBody {
                content_enc: enc,
                salt,
                verifier,
            },
        )
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
    let row: Option<StoredUserNote> = state.notebook.unseal_note(person, id, enc).await?;
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
