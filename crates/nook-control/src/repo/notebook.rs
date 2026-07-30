//! Notebook and vault data access (MAIN-254).
//!
//! Two traits, split along the line that matters most here:
//!
//! - [`NotebookRepository`] — notes and folders. A person's private notebook
//!   (`user_notes`, `user_note_folders`) and a workspace's shared notes
//!   (`notes`).
//! - [`VaultRepository`] — the three tables that decide who can open what:
//!   `user_vaults` (the app password), `user_passkeys` (its other door), and
//!   `person_vaults` (the notebook's per-note seal).
//!
//! **The zero-knowledge contract, restated because this refactor could quietly
//! weaken it and no test would notice.** Nothing in either trait takes or
//! returns a passphrase, a derived key, or a decrypted body:
//!
//! - Vault methods deal only in `kdf_salt` + `verifier`. Both are derived and
//!   non-reversible; they are what lets the server say "wrong password" without
//!   being able to decrypt. `passphrase_verifier` and `verify_passphrase` stay
//!   in the callers, where the plaintext already legitimately is.
//! - A passkey's `wrapped_secret` is an opaque blob the browser sealed. The
//!   server stores and returns it and cannot open it.
//! - Note bodies cross this boundary as [`StoredUserNote`], whose name says
//!   what it holds: ciphertext. Decryption stays in `routes::notebook`, behind
//!   the same `into_note` conversion as before.
//!
//! Methods are intent-named and coarse; no `sqlx` type appears in any
//! signature, and row mapping lives inside the impls (AC-2).

use async_trait::async_trait;
use nook_db::{params, CiMatch, Db, DbPool, Postgres, TypeMapping};
use nook_types::*;
use uuid::Uuid;

use crate::error::ApiResult;

/// A note row as stored: the body is **still ciphertext**, and
/// `sealed_salt`/`sealed_verifier` are set only on sealed notes (MAIN-100).
/// Named so that a caller holding one cannot mistake it for readable content.
#[derive(Debug, Clone)]
pub struct StoredUserNote {
    pub id: UserNoteId,
    pub folder_id: Option<UserNoteFolderId>,
    pub title: String,
    pub content_enc: Vec<u8>,
    pub sealed_salt: Option<Vec<u8>>,
    pub sealed_verifier: Option<Vec<u8>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// A new private note. The body arrives already encrypted.
#[derive(Debug, Clone)]
pub struct NewUserNote {
    pub person: Uuid,
    pub folder_id: Option<UserNoteFolderId>,
    pub title: String,
    pub content_enc: Vec<u8>,
}

/// A partial note edit. Every field is "leave alone" when `None`; `folder` is
/// tri-state because moving a note to the root is a real move, not a no-op:
/// `None` = leave, `Some(None)` = to root, `Some(Some(f))` = into `f`.
#[derive(Debug, Clone, Default)]
pub struct UserNoteEdit {
    pub title: Option<String>,
    pub content_enc: Option<Vec<u8>>,
    pub folder: Option<Option<UserNoteFolderId>>,
}

/// A client-sealed body: ciphertext the server cannot open, plus the challenge
/// that proves which password opens it.
#[derive(Debug, Clone)]
pub struct SealedBody {
    pub content_enc: Vec<u8>,
    pub salt: Vec<u8>,
    pub verifier: Vec<u8>,
}

/// A folder edit; `parent` is tri-state for the same reason as a note's folder.
#[derive(Debug, Clone, Default)]
pub struct FolderEdit {
    pub name: Option<String>,
    pub parent: Option<Option<UserNoteFolderId>>,
}

/// What a vault stores to check a password: a salt and a verifier, never the
/// password and never a key derived from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultChallenge {
    pub kdf_salt: Vec<u8>,
    pub verifier: Vec<u8>,
}

/// A passkey as enrolled. `wrapped_secret` is opaque to the server.
#[derive(Debug, Clone)]
pub struct NewPasskey {
    pub user: UserId,
    pub tenant: TenantId,
    pub credential_id: String,
    pub label: String,
    pub wrapped_secret: Vec<u8>,
}

#[async_trait]
pub trait NotebookRepository: Send + Sync {
    // ── a person's private notebook ─────────────────────────────────────────

    /// Would moving `folder` under `new_parent` create a cycle (MAIN-84 AC-1)?
    /// Decided by climbing `new_parent`'s ancestor chain — within the person's
    /// own folders — and asking whether `folder` sits on it. Subsumes the
    /// self-parent case, since the chain starts at `new_parent`.
    async fn would_cycle(
        &self,
        person: Uuid,
        folder: UserNoteFolderId,
        new_parent: UserNoteFolderId,
    ) -> ApiResult<bool>;

    /// Is this one of the person's own folders? The guard on every move.
    async fn owns_folder(&self, person: Uuid, folder: UserNoteFolderId) -> ApiResult<bool>;

    /// Note summaries, optionally filtered by a case-insensitive substring over
    /// title + folder path (`q` empty = all). **Bodies stay encrypted and are
    /// never searched** — the summary carries no content at all.
    async fn list_note_summaries(&self, person: Uuid, q: &str) -> ApiResult<Vec<UserNoteSummary>>;

    async fn get_note(&self, person: Uuid, id: UserNoteId) -> ApiResult<Option<StoredUserNote>>;

    async fn create_note(&self, new: NewUserNote) -> ApiResult<StoredUserNote>;

    /// Whether a note is sealed, without loading it. `None` = no such note.
    /// The check that stops a plain body PATCH overwriting a client's sealed
    /// blob with server-encrypted plaintext (MAIN-100).
    async fn is_sealed(&self, person: Uuid, id: UserNoteId) -> ApiResult<Option<bool>>;

    async fn update_note(
        &self,
        person: Uuid,
        id: UserNoteId,
        edit: UserNoteEdit,
    ) -> ApiResult<Option<StoredUserNote>>;

    async fn delete_note(&self, person: Uuid, id: UserNoteId) -> ApiResult<u64>;

    async fn seal_note(
        &self,
        person: Uuid,
        id: UserNoteId,
        body: SealedBody,
    ) -> ApiResult<Option<StoredUserNote>>;

    /// Unseal, replacing the body with server-encrypted ciphertext. Matches
    /// only a currently-sealed note, so unsealing twice is a 404 rather than a
    /// silent body overwrite.
    async fn unseal_note(
        &self,
        person: Uuid,
        id: UserNoteId,
        content_enc: Vec<u8>,
    ) -> ApiResult<Option<StoredUserNote>>;

    // ── folders ─────────────────────────────────────────────────────────────

    async fn list_folders(&self, person: Uuid) -> ApiResult<Vec<UserNoteFolder>>;

    async fn create_folder(
        &self,
        person: Uuid,
        parent: Option<UserNoteFolderId>,
        name: &str,
    ) -> ApiResult<UserNoteFolder>;

    async fn update_folder(
        &self,
        person: Uuid,
        id: UserNoteFolderId,
        edit: FolderEdit,
    ) -> ApiResult<Option<UserNoteFolder>>;

    /// Delete a folder, **reparenting its contents to its own parent** (root if
    /// it had none) — it never cascade-deletes notes (MAIN-84 AC-4). Notes and
    /// child folders rise one level, then the folder goes. All three writes are
    /// one transaction inside this method, so a partial failure cannot orphan
    /// anything. `Ok(false)` = no such folder.
    async fn delete_folder_reparenting(
        &self,
        person: Uuid,
        id: UserNoteFolderId,
    ) -> ApiResult<bool>;

    // ── a workspace's shared notes ──────────────────────────────────────────

    async fn list_workspace_notes(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<Note>>;

    async fn create_workspace_note(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        title: &str,
        content_md: &str,
        kind: &str,
    ) -> ApiResult<Note>;

    /// The workspace's most recent rolling note — what an agent appends to.
    async fn latest_rolling_note(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Option<Note>>;

    /// Append to a note's body in the statement itself (`content_md || $2`),
    /// so two concurrent appends cannot lose one another the way a
    /// read-modify-write would.
    async fn append_to_note(&self, id: NoteId, addition: &str) -> ApiResult<Note>;

    async fn update_workspace_note(
        &self,
        id: NoteId,
        tenant: TenantId,
        title: Option<String>,
        content_md: Option<String>,
    ) -> ApiResult<Option<Note>>;
}

#[async_trait]
pub trait VaultRepository: Send + Sync {
    // ── the user's app password (`user_vaults`) ─────────────────────────────

    /// The challenge a passphrase is verified against. Never the passphrase,
    /// never a key.
    async fn app_password_challenge(&self, user: UserId) -> ApiResult<Option<VaultChallenge>>;

    async fn app_password_set_at(
        &self,
        user: UserId,
    ) -> ApiResult<Option<chrono::DateTime<chrono::Utc>>>;

    async fn has_app_password(&self, user: UserId) -> ApiResult<bool>;

    /// Set it. The caller has already derived the salt and verifier; this never
    /// sees the password. Setting is once-only, and the caller checks
    /// [`Self::has_app_password`] first — a second one would orphan every
    /// secret sealed under the first.
    async fn set_app_password(
        &self,
        user: UserId,
        tenant: TenantId,
        kdf_salt: Vec<u8>,
        verifier: Vec<u8>,
    ) -> ApiResult<()>;

    // ── passkeys (`user_passkeys`) ──────────────────────────────────────────

    async fn passkey_count(&self, user: UserId) -> ApiResult<i64>;

    async fn list_passkeys(&self, user: UserId) -> ApiResult<Vec<VaultPasskey>>;

    /// Enrol or re-enrol. `ON CONFLICT (user_id, credential_id)` means
    /// re-enrolling the same authenticator refreshes its blob rather than
    /// stacking duplicates. Returns the row's `created_at`.
    async fn upsert_passkey(
        &self,
        new: NewPasskey,
    ) -> ApiResult<(Uuid, chrono::DateTime<chrono::Utc>)>;

    async fn delete_passkey(&self, id: Uuid, user: UserId) -> ApiResult<u64>;

    async fn touch_passkey(&self, id: Uuid, user: UserId) -> ApiResult<u64>;

    // ── the person's notebook seal (`person_vaults`) ────────────────────────

    async fn person_challenge(&self, person: Uuid) -> ApiResult<Option<VaultChallenge>>;

    async fn person_vault_created_at(
        &self,
        person: Uuid,
    ) -> ApiResult<Option<chrono::DateTime<chrono::Utc>>>;

    async fn has_person_vault(&self, person: Uuid) -> ApiResult<bool>;

    async fn set_person_vault(
        &self,
        person: Uuid,
        kdf_salt: Vec<u8>,
        verifier: Vec<u8>,
    ) -> ApiResult<()>;
}

// ── the DbPool implementations ──────────────────────────────────────────────

/// The `user_notes` columns every read returns, so two SELECTs cannot drift.
const NOTE_COLUMNS: &str =
    "id, folder_id, title, content_enc, sealed_salt, sealed_verifier, created_at, updated_at";

type NoteRow = (
    UserNoteId,
    Option<UserNoteFolderId>,
    String,
    Vec<u8>,
    Option<Vec<u8>>,
    Option<Vec<u8>>,
    chrono::DateTime<chrono::Utc>,
    chrono::DateTime<chrono::Utc>,
);

fn to_note(r: NoteRow) -> StoredUserNote {
    StoredUserNote {
        id: r.0,
        folder_id: r.1,
        title: r.2,
        content_enc: r.3,
        sealed_salt: r.4,
        sealed_verifier: r.5,
        created_at: r.6,
        updated_at: r.7,
    }
}

pub struct DbNotebookRepository {
    db: DbPool,
}

impl DbNotebookRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl NotebookRepository for DbNotebookRepository {
    async fn would_cycle(
        &self,
        person: Uuid,
        folder: UserNoteFolderId,
        new_parent: UserNoteFolderId,
    ) -> ApiResult<bool> {
        Ok(self
            .db
            .query_scalar(
                "WITH RECURSIVE ancestors AS (
                     SELECT id, parent_id FROM user_note_folders
                     WHERE id = $1 AND person_id = $2
                     UNION ALL
                     SELECT f.id, f.parent_id FROM user_note_folders f
                     JOIN ancestors a ON f.id = a.parent_id AND f.person_id = $2
                 )
                 SELECT EXISTS(SELECT 1 FROM ancestors WHERE id = $3)",
                params![new_parent, person, folder],
            )
            .await?)
    }

    async fn owns_folder(&self, person: Uuid, folder: UserNoteFolderId) -> ApiResult<bool> {
        let found: Option<UserNoteFolderId> = self
            .db
            .query_scalar_opt(
                "SELECT id FROM user_note_folders WHERE id = $1 AND person_id = $2",
                params![folder, person],
            )
            .await?;
        Ok(found.is_some())
    }

    async fn list_note_summaries(&self, person: Uuid, q: &str) -> ApiResult<Vec<UserNoteSummary>> {
        // Case-insensitive search routed through the ci_match seam (MAIN-203
        // exemplar): the bound term stays in $2.
        Ok(self
            .db
            .query_all(
                &format!(
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
                ),
                params![person, q],
            )
            .await?)
    }

    async fn get_note(&self, person: Uuid, id: UserNoteId) -> ApiResult<Option<StoredUserNote>> {
        let row: Option<NoteRow> = self
            .db
            .query_opt(
                &format!("SELECT {NOTE_COLUMNS} FROM user_notes WHERE id = $1 AND person_id = $2"),
                params![id, person],
            )
            .await?;
        Ok(row.map(to_note))
    }

    async fn create_note(&self, new: NewUserNote) -> ApiResult<StoredUserNote> {
        let row: NoteRow = self
            .db
            .query_one(
                &format!(
                    "INSERT INTO user_notes (id, person_id, folder_id, title, content_enc)
                     VALUES ($1, $2, $3, $4, $5) RETURNING {NOTE_COLUMNS}"
                ),
                params![
                    UserNoteId::new(),
                    new.person,
                    new.folder_id.map(|f| f.0),
                    new.title,
                    new.content_enc
                ],
            )
            .await?;
        Ok(to_note(row))
    }

    async fn is_sealed(&self, person: Uuid, id: UserNoteId) -> ApiResult<Option<bool>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT sealed_salt IS NOT NULL FROM user_notes WHERE id = $1 AND person_id = $2",
                params![id, person],
            )
            .await?)
    }

    async fn update_note(
        &self,
        person: Uuid,
        id: UserNoteId,
        edit: UserNoteEdit,
    ) -> ApiResult<Option<StoredUserNote>> {
        // folder is tri-state: None = leave, Some(None) = to root, Some(Some) = set.
        let (set_folder, folder_val) = match edit.folder {
            None => (false, None),
            Some(v) => (true, v),
        };
        let row: Option<NoteRow> = self
            .db
            .query_opt(
                &format!(
                    "UPDATE user_notes SET
                        title = COALESCE($3, title),
                        content_enc = COALESCE($4, content_enc),
                        folder_id = CASE WHEN $5 THEN $6 ELSE folder_id END,
                        updated_at = {}
                     WHERE id = $1 AND person_id = $2 RETURNING {NOTE_COLUMNS}",
                    Postgres.now()
                ),
                params![
                    id,
                    person,
                    edit.title,
                    edit.content_enc,
                    set_folder,
                    folder_val.map(|f| f.0)
                ],
            )
            .await?;
        Ok(row.map(to_note))
    }

    async fn delete_note(&self, person: Uuid, id: UserNoteId) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                "DELETE FROM user_notes WHERE id = $1 AND person_id = $2",
                params![id, person],
            )
            .await?)
    }

    async fn seal_note(
        &self,
        person: Uuid,
        id: UserNoteId,
        body: SealedBody,
    ) -> ApiResult<Option<StoredUserNote>> {
        let row: Option<NoteRow> = self
            .db
            .query_opt(
                &format!(
                    "UPDATE user_notes
                     SET content_enc = $3, sealed_salt = $4, sealed_verifier = $5,
                         updated_at = {}
                     WHERE id = $1 AND person_id = $2 RETURNING {NOTE_COLUMNS}",
                    Postgres.now()
                ),
                params![id, person, body.content_enc, body.salt, body.verifier],
            )
            .await?;
        Ok(row.map(to_note))
    }

    async fn unseal_note(
        &self,
        person: Uuid,
        id: UserNoteId,
        content_enc: Vec<u8>,
    ) -> ApiResult<Option<StoredUserNote>> {
        let row: Option<NoteRow> = self
            .db
            .query_opt(
                &format!(
                    "UPDATE user_notes
                     SET content_enc = $3, sealed_salt = NULL, sealed_verifier = NULL,
                         updated_at = {}
                     WHERE id = $1 AND person_id = $2 AND sealed_salt IS NOT NULL
                     RETURNING {NOTE_COLUMNS}",
                    Postgres.now()
                ),
                params![id, person, content_enc],
            )
            .await?;
        Ok(row.map(to_note))
    }

    async fn list_folders(&self, person: Uuid) -> ApiResult<Vec<UserNoteFolder>> {
        Ok(self
            .db
            .query_all(
                "SELECT * FROM user_note_folders WHERE person_id = $1 ORDER BY name",
                params![person],
            )
            .await?)
    }

    async fn create_folder(
        &self,
        person: Uuid,
        parent: Option<UserNoteFolderId>,
        name: &str,
    ) -> ApiResult<UserNoteFolder> {
        Ok(self
            .db
            .query_one(
                "INSERT INTO user_note_folders (id, person_id, parent_id, name)
                 VALUES ($1, $2, $3, $4) RETURNING *",
                params![UserNoteFolderId::new(), person, parent.map(|p| p.0), name],
            )
            .await?)
    }

    async fn update_folder(
        &self,
        person: Uuid,
        id: UserNoteFolderId,
        edit: FolderEdit,
    ) -> ApiResult<Option<UserNoteFolder>> {
        let (set_parent, parent_val) = match edit.parent {
            None => (false, None),
            Some(v) => (true, v),
        };
        Ok(self
            .db
            .query_opt(
                &format!(
                    "UPDATE user_note_folders SET
                        name = COALESCE($3, name),
                        parent_id = CASE WHEN $4 THEN $5 ELSE parent_id END,
                        updated_at = {}
                     WHERE id = $1 AND person_id = $2 RETURNING *",
                    Postgres.now()
                ),
                params![id, person, edit.name, set_parent, parent_val.map(|p| p.0)],
            )
            .await?)
    }

    async fn delete_folder_reparenting(
        &self,
        person: Uuid,
        id: UserNoteFolderId,
    ) -> ApiResult<bool> {
        let mut tx = self.db.begin().await.map_err(nook_db::DbError::from)?;
        let parent: Option<(Option<UserNoteFolderId>,)> = tx
            .query_opt(
                "SELECT parent_id FROM user_note_folders WHERE id = $1 AND person_id = $2",
                params![id, person],
            )
            .await?;
        let Some((parent_id,)) = parent else {
            tx.rollback().await?;
            return Ok(false);
        };
        tx.exec(
            "UPDATE user_notes SET folder_id = $3 WHERE folder_id = $1 AND person_id = $2",
            params![id, person, parent_id.map(|f| f.0)],
        )
        .await?;
        tx.exec(
            "UPDATE user_note_folders SET parent_id = $3 WHERE parent_id = $1 AND person_id = $2",
            params![id, person, parent_id.map(|f| f.0)],
        )
        .await?;
        tx.exec(
            "DELETE FROM user_note_folders WHERE id = $1 AND person_id = $2",
            params![id, person],
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    async fn list_workspace_notes(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<Note>> {
        Ok(self
            .db
            .query_all(
                "SELECT * FROM notes WHERE tenant_id = $1 AND workspace_id = $2
                 ORDER BY updated_at DESC",
                params![tenant, workspace],
            )
            .await?)
    }

    async fn create_workspace_note(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        title: &str,
        content_md: &str,
        kind: &str,
    ) -> ApiResult<Note> {
        Ok(self
            .db
            .query_one(
                "INSERT INTO notes (id, tenant_id, workspace_id, title, content_md, kind)
                 VALUES ($1, $2, $3, $4, $5, $6) RETURNING *",
                params![NoteId::new(), tenant, workspace, title, content_md, kind],
            )
            .await?)
    }

    async fn latest_rolling_note(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Option<Note>> {
        Ok(self
            .db
            .query_opt(
                "SELECT * FROM notes
                 WHERE tenant_id = $1 AND workspace_id = $2 AND kind = 'rolling'
                 ORDER BY updated_at DESC LIMIT 1",
                params![tenant, workspace],
            )
            .await?)
    }

    async fn append_to_note(&self, id: NoteId, addition: &str) -> ApiResult<Note> {
        Ok(self
            .db
            .query_one(
                &format!(
                    "UPDATE notes SET content_md = content_md || $2, updated_at = {now}
                     WHERE id = $1 RETURNING *",
                    now = Postgres.now()
                ),
                params![id, addition],
            )
            .await?)
    }

    async fn update_workspace_note(
        &self,
        id: NoteId,
        tenant: TenantId,
        title: Option<String>,
        content_md: Option<String>,
    ) -> ApiResult<Option<Note>> {
        Ok(self
            .db
            .query_opt(
                &format!(
                    "UPDATE notes SET
                        title = COALESCE($3, title),
                        content_md = COALESCE($4, content_md),
                        updated_at = {}
                     WHERE id = $1 AND tenant_id = $2 RETURNING *",
                    Postgres.now()
                ),
                params![id, tenant, title, content_md],
            )
            .await?)
    }
}

pub struct DbVaultRepository {
    db: DbPool,
}

impl DbVaultRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl VaultRepository for DbVaultRepository {
    async fn app_password_challenge(&self, user: UserId) -> ApiResult<Option<VaultChallenge>> {
        let row: Option<(Vec<u8>, Vec<u8>)> = self
            .db
            .query_opt(
                "SELECT kdf_salt, verifier FROM user_vaults WHERE user_id = $1",
                params![user],
            )
            .await?;
        Ok(row.map(|(kdf_salt, verifier)| VaultChallenge { kdf_salt, verifier }))
    }

    async fn app_password_set_at(
        &self,
        user: UserId,
    ) -> ApiResult<Option<chrono::DateTime<chrono::Utc>>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT created_at FROM user_vaults WHERE user_id = $1",
                params![user],
            )
            .await?)
    }

    async fn has_app_password(&self, user: UserId) -> ApiResult<bool> {
        let found: Option<Uuid> = self
            .db
            .query_scalar_opt(
                "SELECT user_id FROM user_vaults WHERE user_id = $1",
                params![user],
            )
            .await?;
        Ok(found.is_some())
    }

    async fn set_app_password(
        &self,
        user: UserId,
        tenant: TenantId,
        kdf_salt: Vec<u8>,
        verifier: Vec<u8>,
    ) -> ApiResult<()> {
        self.db
            .exec(
                "INSERT INTO user_vaults (user_id, tenant_id, kdf_salt, verifier)
                 VALUES ($1, $2, $3, $4)",
                params![user, tenant, kdf_salt, verifier],
            )
            .await?;
        Ok(())
    }

    async fn passkey_count(&self, user: UserId) -> ApiResult<i64> {
        Ok(self
            .db
            .query_scalar::<i64>(
                "SELECT count(*) FROM user_passkeys WHERE user_id = $1",
                params![user],
            )
            .await?)
    }

    async fn list_passkeys(&self, user: UserId) -> ApiResult<Vec<VaultPasskey>> {
        type Row = (
            Uuid,
            String,
            String,
            Vec<u8>,
            chrono::DateTime<chrono::Utc>,
            Option<chrono::DateTime<chrono::Utc>>,
        );
        let rows: Vec<Row> = self
            .db
            .query_all(
                "SELECT id, credential_id, label, wrapped_secret, created_at, last_used_at
                 FROM user_passkeys WHERE user_id = $1 ORDER BY created_at",
                params![user],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(
                |(id, credential_id, label, wrapped, created_at, last_used_at)| VaultPasskey {
                    id,
                    credential_id,
                    label,
                    wrapped_secret: base64_encode(&wrapped),
                    created_at,
                    last_used_at,
                },
            )
            .collect())
    }

    async fn upsert_passkey(
        &self,
        new: NewPasskey,
    ) -> ApiResult<(Uuid, chrono::DateTime<chrono::Utc>)> {
        let id = Uuid::now_v7();
        let created_at: (chrono::DateTime<chrono::Utc>,) = self
            .db
            .query_one(
                "INSERT INTO user_passkeys
                    (id, user_id, tenant_id, credential_id, label, wrapped_secret)
                 VALUES ($1, $2, $3, $4, $5, $6)
                 ON CONFLICT (user_id, credential_id)
                 DO UPDATE SET wrapped_secret = EXCLUDED.wrapped_secret,
                               label = EXCLUDED.label
                 RETURNING created_at",
                params![
                    id,
                    new.user,
                    new.tenant,
                    new.credential_id,
                    new.label,
                    new.wrapped_secret
                ],
            )
            .await?;
        Ok((id, created_at.0))
    }

    async fn delete_passkey(&self, id: Uuid, user: UserId) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                "DELETE FROM user_passkeys WHERE id = $1 AND user_id = $2",
                params![id, user],
            )
            .await?)
    }

    async fn touch_passkey(&self, id: Uuid, user: UserId) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                &format!(
                    "UPDATE user_passkeys SET last_used_at = {} WHERE id = $1 AND user_id = $2",
                    Postgres.now()
                ),
                params![id, user],
            )
            .await?)
    }

    async fn person_challenge(&self, person: Uuid) -> ApiResult<Option<VaultChallenge>> {
        let row: Option<(Vec<u8>, Vec<u8>)> = self
            .db
            .query_opt(
                "SELECT kdf_salt, verifier FROM person_vaults WHERE person_id = $1",
                params![person],
            )
            .await?;
        Ok(row.map(|(kdf_salt, verifier)| VaultChallenge { kdf_salt, verifier }))
    }

    async fn person_vault_created_at(
        &self,
        person: Uuid,
    ) -> ApiResult<Option<chrono::DateTime<chrono::Utc>>> {
        Ok(self
            .db
            .query_scalar_opt(
                "SELECT created_at FROM person_vaults WHERE person_id = $1",
                params![person],
            )
            .await?)
    }

    async fn has_person_vault(&self, person: Uuid) -> ApiResult<bool> {
        let found: Option<Uuid> = self
            .db
            .query_scalar_opt(
                "SELECT person_id FROM person_vaults WHERE person_id = $1",
                params![person],
            )
            .await?;
        Ok(found.is_some())
    }

    async fn set_person_vault(
        &self,
        person: Uuid,
        kdf_salt: Vec<u8>,
        verifier: Vec<u8>,
    ) -> ApiResult<()> {
        self.db
            .exec(
                "INSERT INTO person_vaults (person_id, kdf_salt, verifier) VALUES ($1, $2, $3)",
                params![person, kdf_salt, verifier],
            )
            .await?;
        Ok(())
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

// ── in-memory fakes (AC-3) ──────────────────────────────────────────────────
//
// Enough behavior that a caller test is worth trusting: person scoping, the
// cycle guard's ancestor walk, the reparent-not-cascade delete, the tri-state
// folder move, and `unseal` matching only a currently-sealed note. A fake that
// accepted everything would let a caller test pass while the real statement
// refused.
//
// The zero-knowledge contract holds here too: these store the bytes they are
// given and never derive, verify or decrypt anything.

use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Default)]
struct FakeNotebookState {
    notes: Vec<(Uuid, StoredUserNote)>,
    folders: Vec<UserNoteFolder>,
    workspace_notes: Vec<Note>,
}

#[derive(Default)]
pub struct FakeNotebookRepository {
    inner: Mutex<FakeNotebookState>,
}

impl FakeNotebookRepository {
    pub fn new() -> Self {
        Self::default()
    }

    /// The folder a note currently sits in, for asserting a move landed.
    pub fn folder_of(&self, id: UserNoteId) -> Option<Option<UserNoteFolderId>> {
        self.inner
            .lock()
            .unwrap()
            .notes
            .iter()
            .find(|(_, n)| n.id == id)
            .map(|(_, n)| n.folder_id)
    }

    pub fn note_count(&self) -> usize {
        self.inner.lock().unwrap().notes.len()
    }

    pub fn folder_count(&self) -> usize {
        self.inner.lock().unwrap().folders.len()
    }

    /// The stored ciphertext, so a test can prove a body was NOT rewritten.
    pub fn ciphertext_of(&self, id: UserNoteId) -> Option<Vec<u8>> {
        self.inner
            .lock()
            .unwrap()
            .notes
            .iter()
            .find(|(_, n)| n.id == id)
            .map(|(_, n)| n.content_enc.clone())
    }
}

#[async_trait]
impl NotebookRepository for FakeNotebookRepository {
    async fn would_cycle(
        &self,
        person: Uuid,
        folder: UserNoteFolderId,
        new_parent: UserNoteFolderId,
    ) -> ApiResult<bool> {
        let s = self.inner.lock().unwrap();
        // Climb new_parent's ancestor chain within this person's folders and ask
        // whether `folder` is on it. Starting AT new_parent is what makes
        // new_parent == folder (a self-parent) a cycle too.
        let mut cursor = Some(new_parent);
        let mut guard = 0;
        while let Some(cur) = cursor {
            if cur == folder {
                return Ok(true);
            }
            guard += 1;
            if guard > 1000 {
                break; // an already-corrupt chain; the SQL would loop too
            }
            cursor = s
                .folders
                .iter()
                .find(|f| f.id == cur && f.person_id == person)
                .and_then(|f| f.parent_id);
        }
        Ok(false)
    }

    async fn owns_folder(&self, person: Uuid, folder: UserNoteFolderId) -> ApiResult<bool> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .folders
            .iter()
            .any(|f| f.id == folder && f.person_id == person))
    }

    async fn list_note_summaries(&self, person: Uuid, q: &str) -> ApiResult<Vec<UserNoteSummary>> {
        let s = self.inner.lock().unwrap();
        // The folder path, built the way the recursive CTE does.
        let path_of = |mut f: Option<UserNoteFolderId>| {
            let mut parts = Vec::new();
            let mut guard = 0;
            while let Some(id) = f {
                let Some(folder) = s.folders.iter().find(|x| x.id == id) else {
                    break;
                };
                parts.push(folder.name.clone());
                f = folder.parent_id;
                guard += 1;
                if guard > 1000 {
                    break;
                }
            }
            parts.reverse();
            parts.join("/")
        };
        let needle = q.to_lowercase();
        let mut out: Vec<UserNoteSummary> = s
            .notes
            .iter()
            .filter(|(p, _)| *p == person)
            .filter_map(|(_, n)| {
                let path = path_of(n.folder_id);
                // Title and PATH are searched; the body never is.
                if !q.is_empty()
                    && !n.title.to_lowercase().contains(&needle)
                    && !path.to_lowercase().contains(&needle)
                {
                    return None;
                }
                Some(UserNoteSummary {
                    id: n.id,
                    folder_id: n.folder_id,
                    title: n.title.clone(),
                    path,
                    sealed: n.sealed_salt.is_some(),
                    created_at: n.created_at,
                    updated_at: n.updated_at,
                })
            })
            .collect();
        out.sort_by_key(|n| std::cmp::Reverse(n.updated_at));
        Ok(out)
    }

    async fn get_note(&self, person: Uuid, id: UserNoteId) -> ApiResult<Option<StoredUserNote>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .notes
            .iter()
            .find(|(p, n)| *p == person && n.id == id)
            .map(|(_, n)| n.clone()))
    }

    async fn create_note(&self, new: NewUserNote) -> ApiResult<StoredUserNote> {
        let now = chrono::Utc::now();
        let note = StoredUserNote {
            id: UserNoteId::new(),
            folder_id: new.folder_id,
            title: new.title,
            content_enc: new.content_enc,
            sealed_salt: None,
            sealed_verifier: None,
            created_at: now,
            updated_at: now,
        };
        self.inner
            .lock()
            .unwrap()
            .notes
            .push((new.person, note.clone()));
        Ok(note)
    }

    async fn is_sealed(&self, person: Uuid, id: UserNoteId) -> ApiResult<Option<bool>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .notes
            .iter()
            .find(|(p, n)| *p == person && n.id == id)
            .map(|(_, n)| n.sealed_salt.is_some()))
    }

    async fn update_note(
        &self,
        person: Uuid,
        id: UserNoteId,
        edit: UserNoteEdit,
    ) -> ApiResult<Option<StoredUserNote>> {
        let mut s = self.inner.lock().unwrap();
        Ok(s.notes
            .iter_mut()
            .find(|(p, n)| *p == person && n.id == id)
            .map(|(_, n)| {
                // COALESCE: None leaves the column alone.
                if let Some(t) = edit.title {
                    n.title = t;
                }
                if let Some(c) = edit.content_enc {
                    n.content_enc = c;
                }
                // Tri-state: only Some(_) touches the folder, and Some(None)
                // really does move it to the root.
                if let Some(f) = edit.folder {
                    n.folder_id = f;
                }
                n.updated_at = chrono::Utc::now();
                n.clone()
            }))
    }

    async fn delete_note(&self, person: Uuid, id: UserNoteId) -> ApiResult<u64> {
        let mut s = self.inner.lock().unwrap();
        let before = s.notes.len();
        s.notes.retain(|(p, n)| !(*p == person && n.id == id));
        Ok((before - s.notes.len()) as u64)
    }

    async fn seal_note(
        &self,
        person: Uuid,
        id: UserNoteId,
        body: SealedBody,
    ) -> ApiResult<Option<StoredUserNote>> {
        let mut s = self.inner.lock().unwrap();
        Ok(s.notes
            .iter_mut()
            .find(|(p, n)| *p == person && n.id == id)
            .map(|(_, n)| {
                n.content_enc = body.content_enc;
                n.sealed_salt = Some(body.salt);
                n.sealed_verifier = Some(body.verifier);
                n.updated_at = chrono::Utc::now();
                n.clone()
            }))
    }

    async fn unseal_note(
        &self,
        person: Uuid,
        id: UserNoteId,
        content_enc: Vec<u8>,
    ) -> ApiResult<Option<StoredUserNote>> {
        let mut s = self.inner.lock().unwrap();
        Ok(s.notes
            .iter_mut()
            // `AND sealed_salt IS NOT NULL`: unsealing an unsealed note matches
            // nothing, rather than silently overwriting its body.
            .find(|(p, n)| *p == person && n.id == id && n.sealed_salt.is_some())
            .map(|(_, n)| {
                n.content_enc = content_enc;
                n.sealed_salt = None;
                n.sealed_verifier = None;
                n.updated_at = chrono::Utc::now();
                n.clone()
            }))
    }

    async fn list_folders(&self, person: Uuid) -> ApiResult<Vec<UserNoteFolder>> {
        let s = self.inner.lock().unwrap();
        let mut out: Vec<UserNoteFolder> = s
            .folders
            .iter()
            .filter(|f| f.person_id == person)
            .cloned()
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn create_folder(
        &self,
        person: Uuid,
        parent: Option<UserNoteFolderId>,
        name: &str,
    ) -> ApiResult<UserNoteFolder> {
        let now = chrono::Utc::now();
        let folder = UserNoteFolder {
            id: UserNoteFolderId::new(),
            person_id: person,
            parent_id: parent,
            name: name.to_string(),
            created_at: now,
            updated_at: now,
        };
        self.inner.lock().unwrap().folders.push(folder.clone());
        Ok(folder)
    }

    async fn update_folder(
        &self,
        person: Uuid,
        id: UserNoteFolderId,
        edit: FolderEdit,
    ) -> ApiResult<Option<UserNoteFolder>> {
        let mut s = self.inner.lock().unwrap();
        Ok(s.folders
            .iter_mut()
            .find(|f| f.id == id && f.person_id == person)
            .map(|f| {
                if let Some(n) = edit.name {
                    f.name = n;
                }
                if let Some(p) = edit.parent {
                    f.parent_id = p;
                }
                f.updated_at = chrono::Utc::now();
                f.clone()
            }))
    }

    async fn delete_folder_reparenting(
        &self,
        person: Uuid,
        id: UserNoteFolderId,
    ) -> ApiResult<bool> {
        let mut s = self.inner.lock().unwrap();
        let Some(parent_id) = s
            .folders
            .iter()
            .find(|f| f.id == id && f.person_id == person)
            .map(|f| f.parent_id)
        else {
            return Ok(false);
        };
        // Contents RISE one level — they are never cascade-deleted.
        for (p, n) in s.notes.iter_mut() {
            if *p == person && n.folder_id == Some(id) {
                n.folder_id = parent_id;
            }
        }
        for f in s.folders.iter_mut() {
            if f.person_id == person && f.parent_id == Some(id) {
                f.parent_id = parent_id;
            }
        }
        s.folders.retain(|f| !(f.id == id && f.person_id == person));
        Ok(true)
    }

    async fn list_workspace_notes(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Vec<Note>> {
        let s = self.inner.lock().unwrap();
        let mut out: Vec<Note> = s
            .workspace_notes
            .iter()
            .filter(|n| n.tenant_id == tenant && n.workspace_id == workspace)
            .cloned()
            .collect();
        out.sort_by_key(|n| std::cmp::Reverse(n.updated_at));
        Ok(out)
    }

    async fn create_workspace_note(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
        title: &str,
        content_md: &str,
        kind: &str,
    ) -> ApiResult<Note> {
        let now = chrono::Utc::now();
        let note = Note {
            id: NoteId::new(),
            tenant_id: tenant,
            workspace_id: workspace,
            title: title.to_string(),
            content_md: content_md.to_string(),
            kind: kind.to_string(),
            created_at: now,
            updated_at: now,
        };
        self.inner
            .lock()
            .unwrap()
            .workspace_notes
            .push(note.clone());
        Ok(note)
    }

    async fn latest_rolling_note(
        &self,
        tenant: TenantId,
        workspace: WorkspaceId,
    ) -> ApiResult<Option<Note>> {
        let s = self.inner.lock().unwrap();
        Ok(s.workspace_notes
            .iter()
            .filter(|n| n.tenant_id == tenant && n.workspace_id == workspace && n.kind == "rolling")
            .max_by_key(|n| n.updated_at)
            .cloned())
    }

    async fn append_to_note(&self, id: NoteId, addition: &str) -> ApiResult<Note> {
        let mut s = self.inner.lock().unwrap();
        let note = s
            .workspace_notes
            .iter_mut()
            .find(|n| n.id == id)
            .ok_or(crate::error::ApiError::NotFound)?;
        note.content_md.push_str(addition);
        note.updated_at = chrono::Utc::now();
        Ok(note.clone())
    }

    async fn update_workspace_note(
        &self,
        id: NoteId,
        tenant: TenantId,
        title: Option<String>,
        content_md: Option<String>,
    ) -> ApiResult<Option<Note>> {
        let mut s = self.inner.lock().unwrap();
        Ok(s.workspace_notes
            .iter_mut()
            .find(|n| n.id == id && n.tenant_id == tenant)
            .map(|n| {
                if let Some(t) = title {
                    n.title = t;
                }
                if let Some(c) = content_md {
                    n.content_md = c;
                }
                n.updated_at = chrono::Utc::now();
                n.clone()
            }))
    }
}

#[derive(Default)]
struct FakeVaultState {
    /// user → (challenge, tenant, created_at)
    app: HashMap<UserId, (VaultChallenge, TenantId, chrono::DateTime<chrono::Utc>)>,
    /// person → (challenge, created_at)
    person: HashMap<Uuid, (VaultChallenge, chrono::DateTime<chrono::Utc>)>,
    passkeys: Vec<(UserId, VaultPasskey, String)>,
}

#[derive(Default)]
pub struct FakeVaultRepository {
    inner: Mutex<FakeVaultState>,
}

impl FakeVaultRepository {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl VaultRepository for FakeVaultRepository {
    async fn app_password_challenge(&self, user: UserId) -> ApiResult<Option<VaultChallenge>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .app
            .get(&user)
            .map(|(c, _, _)| c.clone()))
    }

    async fn app_password_set_at(
        &self,
        user: UserId,
    ) -> ApiResult<Option<chrono::DateTime<chrono::Utc>>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .app
            .get(&user)
            .map(|(_, _, t)| *t))
    }

    async fn has_app_password(&self, user: UserId) -> ApiResult<bool> {
        Ok(self.inner.lock().unwrap().app.contains_key(&user))
    }

    async fn set_app_password(
        &self,
        user: UserId,
        tenant: TenantId,
        kdf_salt: Vec<u8>,
        verifier: Vec<u8>,
    ) -> ApiResult<()> {
        self.inner.lock().unwrap().app.insert(
            user,
            (
                VaultChallenge { kdf_salt, verifier },
                tenant,
                chrono::Utc::now(),
            ),
        );
        Ok(())
    }

    async fn passkey_count(&self, user: UserId) -> ApiResult<i64> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .passkeys
            .iter()
            .filter(|(u, _, _)| *u == user)
            .count() as i64)
    }

    async fn list_passkeys(&self, user: UserId) -> ApiResult<Vec<VaultPasskey>> {
        let s = self.inner.lock().unwrap();
        let mut out: Vec<VaultPasskey> = s
            .passkeys
            .iter()
            .filter(|(u, _, _)| *u == user)
            .map(|(_, k, _)| k.clone())
            .collect();
        out.sort_by_key(|k| k.created_at);
        Ok(out)
    }

    async fn upsert_passkey(
        &self,
        new: NewPasskey,
    ) -> ApiResult<(Uuid, chrono::DateTime<chrono::Utc>)> {
        let mut s = self.inner.lock().unwrap();
        let wrapped = base64_encode(&new.wrapped_secret);
        // ON CONFLICT (user_id, credential_id): re-enrolling the same
        // authenticator refreshes it rather than stacking a duplicate.
        if let Some((_, k, cred)) = s
            .passkeys
            .iter_mut()
            .find(|(u, _, cred)| *u == new.user && *cred == new.credential_id)
        {
            let _ = cred;
            k.wrapped_secret = wrapped;
            k.label = new.label;
            return Ok((k.id, k.created_at));
        }
        let id = Uuid::now_v7();
        let created_at = chrono::Utc::now();
        s.passkeys.push((
            new.user,
            VaultPasskey {
                id,
                credential_id: new.credential_id.clone(),
                label: new.label,
                wrapped_secret: wrapped,
                created_at,
                last_used_at: None,
            },
            new.credential_id,
        ));
        Ok((id, created_at))
    }

    async fn delete_passkey(&self, id: Uuid, user: UserId) -> ApiResult<u64> {
        let mut s = self.inner.lock().unwrap();
        let before = s.passkeys.len();
        s.passkeys.retain(|(u, k, _)| !(*u == user && k.id == id));
        Ok((before - s.passkeys.len()) as u64)
    }

    async fn touch_passkey(&self, id: Uuid, user: UserId) -> ApiResult<u64> {
        let mut s = self.inner.lock().unwrap();
        Ok(
            match s
                .passkeys
                .iter_mut()
                .find(|(u, k, _)| *u == user && k.id == id)
            {
                Some((_, k, _)) => {
                    k.last_used_at = Some(chrono::Utc::now());
                    1
                }
                None => 0,
            },
        )
    }

    async fn person_challenge(&self, person: Uuid) -> ApiResult<Option<VaultChallenge>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .person
            .get(&person)
            .map(|(c, _)| c.clone()))
    }

    async fn person_vault_created_at(
        &self,
        person: Uuid,
    ) -> ApiResult<Option<chrono::DateTime<chrono::Utc>>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .person
            .get(&person)
            .map(|(_, t)| *t))
    }

    async fn has_person_vault(&self, person: Uuid) -> ApiResult<bool> {
        Ok(self.inner.lock().unwrap().person.contains_key(&person))
    }

    async fn set_person_vault(
        &self,
        person: Uuid,
        kdf_salt: Vec<u8>,
        verifier: Vec<u8>,
    ) -> ApiResult<()> {
        self.inner.lock().unwrap().person.insert(
            person,
            (VaultChallenge { kdf_salt, verifier }, chrono::Utc::now()),
        );
        Ok(())
    }
}
