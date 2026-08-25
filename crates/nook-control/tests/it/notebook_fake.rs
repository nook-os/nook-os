//! Notebook and vault callers against the in-memory fakes, with **no database
//! at all** (MAIN-254 AC-3).
//!
//! The rules pinned here are the quiet ones. A folder delete that cascaded
//! instead of reparenting would destroy notes. A cycle guard that missed the
//! self-parent case would make a subtree unreachable. An unseal that matched an
//! unsealed note would overwrite its body. And the zero-knowledge contract is
//! only worth what a test says it is, so the vault cases assert what the
//! repository is *given* and *returns*, never a password.
//!
//! `cargo test -p nook-control --test notebook_fake` passes with the database
//! stopped.

use nook_control::repo::notebook::{
    FakeNotebookRepository, FakeVaultRepository, FolderDeletion, FolderEdit, NewPasskey,
    NewUserNote, NotebookRepository, SealedBody, UserNoteEdit, VaultChallenge, VaultRepository,
};
use nook_control::services::notebook_queries;
use nook_types::*;
use uuid::Uuid;

fn person() -> Uuid {
    Uuid::now_v7()
}

async fn note(
    repo: &FakeNotebookRepository,
    p: Uuid,
    folder: Option<UserNoteFolderId>,
    title: &str,
) -> UserNoteId {
    repo.create_note(NewUserNote {
        person: p,
        folder_id: folder,
        title: title.to_string(),
        content_enc: b"ciphertext".to_vec(),
    })
    .await
    .unwrap()
    .id
}

// ── folders: reparent, never cascade (MAIN-84 AC-4) ─────────────────────────

#[tokio::test]
async fn deleting_a_folder_lifts_its_contents_instead_of_destroying_them() {
    let repo = FakeNotebookRepository::new();
    let p = person();
    let outer = repo.create_folder(p, None, "outer").await.unwrap();
    let inner = repo
        .create_folder(p, Some(outer.id), "inner")
        .await
        .unwrap();
    let deep = repo.create_folder(p, Some(inner.id), "deep").await.unwrap();
    let in_inner = note(&repo, p, Some(inner.id), "kept").await;

    assert_eq!(
        repo.delete_folder_reparenting(p, inner.id).await.unwrap(),
        FolderDeletion::Deleted
    );

    assert_eq!(
        repo.note_count(),
        1,
        "a folder delete never destroys a note"
    );
    assert_eq!(
        repo.folder_of(in_inner),
        Some(Some(outer.id)),
        "the note rises to the deleted folder's parent"
    );
    let folders = repo.list_folders(p).await.unwrap();
    let deep_now = folders
        .iter()
        .find(|f| f.id == deep.id)
        .expect("still there");
    assert_eq!(
        deep_now.parent_id,
        Some(outer.id),
        "a child folder rises too"
    );
    assert!(!folders.iter().any(|f| f.id == inner.id));
}

#[tokio::test]
async fn deleting_a_root_folder_sends_its_contents_to_the_root() {
    let repo = FakeNotebookRepository::new();
    let p = person();
    let top = repo.create_folder(p, None, "top").await.unwrap();
    let n = note(&repo, p, Some(top.id), "note").await;

    assert_eq!(
        repo.delete_folder_reparenting(p, top.id).await.unwrap(),
        FolderDeletion::Deleted
    );
    assert_eq!(
        repo.folder_of(n),
        Some(None),
        "no parent to rise to means the root, not deletion"
    );
    assert_eq!(repo.note_count(), 1);
}

/// The reparenting a delete performs is a MOVE, so it obeys the uniqueness the
/// index enforces (MAIN-574): a child landing beside a row of the same name
/// refuses the whole delete, and the folder — which is leaving — never blocks
/// its own contents.
#[tokio::test]
async fn deleting_a_folder_refuses_when_a_child_name_is_taken_where_it_would_land() {
    let repo = FakeNotebookRepository::new();
    let p = person();
    let work = repo.create_folder(p, None, "Work").await.unwrap();
    repo.create_folder(p, None, "Archive").await.unwrap();
    repo.create_folder(p, Some(work.id), "Archive")
        .await
        .unwrap();

    assert_eq!(
        repo.delete_folder_reparenting(p, work.id).await.unwrap(),
        FolderDeletion::Collision {
            what: "folder",
            name: "Archive".into()
        }
    );
    assert_eq!(
        repo.folder_count(),
        3,
        "nothing moved and nothing was deleted"
    );
}

/// A child carrying the deleted folder's OWN name is NOT a collision — the row
/// it would clash with is the one going away — so `Work/Work` stays deletable.
#[tokio::test]
async fn a_child_named_after_the_folder_being_deleted_still_rises() {
    let repo = FakeNotebookRepository::new();
    let p = person();
    let work = repo.create_folder(p, None, "Work").await.unwrap();
    let inner = repo.create_folder(p, Some(work.id), "Work").await.unwrap();

    assert_eq!(
        repo.delete_folder_reparenting(p, work.id).await.unwrap(),
        FolderDeletion::Deleted
    );
    let left = repo.list_folders(p).await.unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!((left[0].id, left[0].parent_id), (inner.id, None));
}

#[tokio::test]
async fn deleting_a_folder_that_is_not_yours_reports_not_found_and_changes_nothing() {
    let repo = FakeNotebookRepository::new();
    let (mine, theirs) = (person(), person());
    let f = repo.create_folder(theirs, None, "theirs").await.unwrap();

    assert_eq!(
        repo.delete_folder_reparenting(mine, f.id).await.unwrap(),
        FolderDeletion::NoSuchFolder
    );
    assert_eq!(repo.folder_count(), 1);
}

// ── the cycle guard (MAIN-84 AC-1) ──────────────────────────────────────────

#[tokio::test]
async fn a_folder_cannot_be_moved_under_itself_or_its_own_descendant() {
    let repo = FakeNotebookRepository::new();
    let p = person();
    let a = repo.create_folder(p, None, "a").await.unwrap();
    let b = repo.create_folder(p, Some(a.id), "b").await.unwrap();
    let c = repo.create_folder(p, Some(b.id), "c").await.unwrap();

    assert!(
        repo.would_cycle(p, a.id, a.id).await.unwrap(),
        "self-parent is a cycle — the chain starts AT new_parent, which is what \
         makes this case fall out rather than needing its own check"
    );
    assert!(
        repo.would_cycle(p, a.id, b.id).await.unwrap(),
        "a under its child"
    );
    assert!(
        repo.would_cycle(p, a.id, c.id).await.unwrap(),
        "a under its grandchild — the walk has to climb, not just look one up"
    );
    assert!(
        !repo.would_cycle(p, c.id, a.id).await.unwrap(),
        "moving a leaf up is fine"
    );
}

#[tokio::test]
async fn the_cycle_walk_stays_inside_one_persons_folders() {
    let repo = FakeNotebookRepository::new();
    let (mine, theirs) = (person(), person());
    let a = repo.create_folder(mine, None, "a").await.unwrap();
    // Another person's folder that happens to name mine as its parent.
    let intruder = repo
        .create_folder(theirs, Some(a.id), "theirs")
        .await
        .unwrap();

    assert!(
        !repo.would_cycle(mine, a.id, intruder.id).await.unwrap(),
        "the ancestor walk is person-scoped, so another person's row is not on \
         my chain"
    );
    assert!(!repo.owns_folder(mine, intruder.id).await.unwrap());
}

// ── the tri-state move ──────────────────────────────────────────────────────

#[tokio::test]
async fn a_note_move_distinguishes_leave_alone_from_move_to_root() {
    let repo = FakeNotebookRepository::new();
    let p = person();
    let f = repo.create_folder(p, None, "f").await.unwrap();
    let n = note(&repo, p, Some(f.id), "n").await;

    // None = leave alone. A title-only edit must not evict the note.
    repo.update_note(
        p,
        n,
        UserNoteEdit {
            title: Some("renamed".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        repo.folder_of(n),
        Some(Some(f.id)),
        "a title-only edit leaves the folder alone"
    );

    // Some(None) = a real move to the root.
    repo.update_note(
        p,
        n,
        UserNoteEdit {
            folder: Some(None),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(repo.folder_of(n), Some(None));
}

#[tokio::test]
async fn a_title_only_edit_does_not_rewrite_the_body() {
    let repo = FakeNotebookRepository::new();
    let p = person();
    let n = note(&repo, p, None, "n").await;
    let before = repo.ciphertext_of(n).unwrap();

    repo.update_note(
        p,
        n,
        UserNoteEdit {
            title: Some("new title".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(
        repo.ciphertext_of(n).unwrap(),
        before,
        "COALESCE: a None body must leave the ciphertext untouched"
    );
}

// ── the seal contract (MAIN-100) ────────────────────────────────────────────

#[tokio::test]
async fn sealing_marks_the_note_and_unsealing_only_matches_a_sealed_one() {
    let repo = FakeNotebookRepository::new();
    let p = person();
    let n = note(&repo, p, None, "n").await;
    assert_eq!(repo.is_sealed(p, n).await.unwrap(), Some(false));

    repo.seal_note(
        p,
        n,
        SealedBody {
            content_enc: b"client-sealed".to_vec(),
            salt: b"salt".to_vec(),
            verifier: b"verifier".to_vec(),
        },
    )
    .await
    .unwrap();
    assert_eq!(repo.is_sealed(p, n).await.unwrap(), Some(true));

    // Unsealing works once.
    assert!(repo
        .unseal_note(p, n, b"server-encrypted".to_vec())
        .await
        .unwrap()
        .is_some());
    assert_eq!(repo.is_sealed(p, n).await.unwrap(), Some(false));

    // A second unseal matches nothing — the `AND sealed_salt IS NOT NULL`
    // guard. Without it this would overwrite the body a second time.
    let body_now = repo.ciphertext_of(n).unwrap();
    assert!(repo
        .unseal_note(p, n, b"clobbered".to_vec())
        .await
        .unwrap()
        .is_none());
    assert_eq!(repo.ciphertext_of(n).unwrap(), body_now);
}

#[tokio::test]
async fn is_sealed_reports_none_for_someone_elses_note() {
    let repo = FakeNotebookRepository::new();
    let (mine, theirs) = (person(), person());
    let n = note(&repo, theirs, None, "theirs").await;

    assert_eq!(
        repo.is_sealed(mine, n).await.unwrap(),
        None,
        "not 'unsealed' — the note is not visible at all"
    );
    assert!(repo.get_note(mine, n).await.unwrap().is_none());
    assert_eq!(repo.delete_note(mine, n).await.unwrap(), 0);
}

// ── search never touches the body ───────────────────────────────────────────

#[tokio::test]
async fn search_matches_title_and_folder_path_but_never_the_body() {
    let repo = FakeNotebookRepository::new();
    let p = person();
    let work = repo.create_folder(p, None, "work").await.unwrap();
    let deep = repo
        .create_folder(p, Some(work.id), "invoices")
        .await
        .unwrap();
    let by_title = note(&repo, p, None, "Quarterly plan").await;
    let by_path = note(&repo, p, Some(deep.id), "untitled").await;

    let titled = repo.list_note_summaries(p, "quarterly").await.unwrap();
    assert_eq!(titled.len(), 1);
    assert_eq!(titled[0].id, by_title, "case-insensitive title match");

    let pathed = repo.list_note_summaries(p, "work/inv").await.unwrap();
    assert_eq!(pathed.len(), 1);
    assert_eq!(pathed[0].id, by_path);
    assert_eq!(
        pathed[0].path, "work/invoices",
        "the CTE builds a full path"
    );

    // The bodies are all `ciphertext`; searching for it must find nothing.
    assert!(
        repo.list_note_summaries(p, "ciphertext")
            .await
            .unwrap()
            .is_empty(),
        "bodies stay encrypted and are never searched"
    );
    assert_eq!(repo.list_note_summaries(p, "").await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_summary_carries_no_content_at_all() {
    let repo = FakeNotebookRepository::new();
    let p = person();
    note(&repo, p, None, "n").await;
    let summaries = repo.list_note_summaries(p, "").await.unwrap();
    // The type itself is the guarantee: a summary has no body field. Asserting
    // the sealed flag rides along keeps that meaningful rather than vacuous.
    assert_eq!(summaries.len(), 1);
    assert!(!summaries[0].sealed);
}

// ── the vault: derived material only ────────────────────────────────────────

#[tokio::test]
async fn a_vault_stores_only_the_salt_and_verifier_it_was_given() {
    let repo = FakeVaultRepository::new();
    let (user, tenant) = (UserId::new(), TenantId::new());
    assert!(!repo.has_app_password(user).await.unwrap());
    assert!(repo.app_password_challenge(user).await.unwrap().is_none());

    repo.set_app_password(user, tenant, b"salt".to_vec(), b"verifier".to_vec())
        .await
        .unwrap();

    assert!(repo.has_app_password(user).await.unwrap());
    assert_eq!(
        repo.app_password_challenge(user).await.unwrap(),
        Some(VaultChallenge {
            kdf_salt: b"salt".to_vec(),
            verifier: b"verifier".to_vec(),
        }),
        "what comes back is exactly what went in — the repository derives, \
         verifies and decrypts nothing"
    );
    assert!(repo.app_password_set_at(user).await.unwrap().is_some());
}

#[tokio::test]
async fn the_app_password_and_the_notebook_seal_are_separate_vaults() {
    let repo = FakeVaultRepository::new();
    let (user, tenant, p) = (UserId::new(), TenantId::new(), person());

    repo.set_app_password(user, tenant, b"a".to_vec(), b"b".to_vec())
        .await
        .unwrap();
    assert!(
        !repo.has_person_vault(p).await.unwrap(),
        "setting the app password does not set the notebook's seal"
    );

    repo.set_person_vault(p, b"c".to_vec(), b"d".to_vec())
        .await
        .unwrap();
    assert_eq!(
        repo.person_challenge(p).await.unwrap().unwrap().kdf_salt,
        b"c".to_vec()
    );
    assert_eq!(
        repo.app_password_challenge(user)
            .await
            .unwrap()
            .unwrap()
            .kdf_salt,
        b"a".to_vec(),
        "and the two never bleed into each other"
    );
}

#[tokio::test]
async fn re_enrolling_a_passkey_refreshes_it_rather_than_stacking_a_duplicate() {
    let repo = FakeVaultRepository::new();
    let (user, tenant) = (UserId::new(), TenantId::new());

    let mk = |label: &str, secret: &[u8]| NewPasskey {
        user,
        tenant,
        credential_id: "cred-1".into(),
        label: label.to_string(),
        wrapped_secret: secret.to_vec(),
    };

    let (first, _) = repo.upsert_passkey(mk("laptop", b"blob-1")).await.unwrap();
    let (second, _) = repo
        .upsert_passkey(mk("laptop renamed", b"blob-2"))
        .await
        .unwrap();

    assert_eq!(first, second, "same authenticator, same row");
    assert_eq!(repo.passkey_count(user).await.unwrap(), 1);
    let keys = repo.list_passkeys(user).await.unwrap();
    assert_eq!(keys[0].label, "laptop renamed");
    // The blob comes back base64-encoded and otherwise untouched.
    use base64::Engine;
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(&keys[0].wrapped_secret)
            .unwrap(),
        b"blob-2".to_vec(),
        "the server stores and returns the browser's blob without opening it"
    );
}

#[tokio::test]
async fn a_passkey_belongs_to_its_user() {
    let repo = FakeVaultRepository::new();
    let (mine, theirs, tenant) = (UserId::new(), UserId::new(), TenantId::new());
    let (id, _) = repo
        .upsert_passkey(NewPasskey {
            user: theirs,
            tenant,
            credential_id: "c".into(),
            label: "k".into(),
            wrapped_secret: b"blob".to_vec(),
        })
        .await
        .unwrap();

    assert!(repo.list_passkeys(mine).await.unwrap().is_empty());
    assert_eq!(repo.delete_passkey(id, mine).await.unwrap(), 0);
    assert_eq!(repo.touch_passkey(id, mine).await.unwrap(), 0);
    assert_eq!(repo.passkey_count(theirs).await.unwrap(), 1);

    assert_eq!(repo.touch_passkey(id, theirs).await.unwrap(), 1);
    assert!(repo.list_passkeys(theirs).await.unwrap()[0]
        .last_used_at
        .is_some());
    assert_eq!(repo.delete_passkey(id, theirs).await.unwrap(), 1);
}

// ── workspace notes, through the real caller ────────────────────────────────

#[tokio::test]
async fn an_untitled_workspace_note_becomes_a_rolling_note() {
    let repo = FakeNotebookRepository::new();
    let (tenant, ws) = (TenantId::new(), WorkspaceId::new());

    let note = notebook_queries::create_note(
        &repo,
        tenant,
        ws,
        CreateNoteRequest {
            title: None,
            content_md: "first".into(),
            kind: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(note.title, "Rolling notes");
    assert_eq!(note.kind, "rolling", "the shape an agent appends to");

    let latest = notebook_queries::latest_rolling_note(&repo, tenant, ws)
        .await
        .unwrap()
        .expect("found");
    assert_eq!(latest.id, note.id);

    let appended = notebook_queries::append_to_note(&repo, note.id, "\nsecond".into())
        .await
        .unwrap();
    assert_eq!(
        appended.content_md, "first\nsecond",
        "append concatenates in place — two appends cannot lose one another the \
         way a read-modify-write would"
    );
}

#[tokio::test]
async fn workspace_notes_are_scoped_to_their_tenant_and_workspace() {
    let repo = FakeNotebookRepository::new();
    let (tenant, other_tenant) = (TenantId::new(), TenantId::new());
    let (ws, other_ws) = (WorkspaceId::new(), WorkspaceId::new());

    let mine = repo
        .create_workspace_note(tenant, ws, "mine", "x", "rolling")
        .await
        .unwrap();
    repo.create_workspace_note(tenant, other_ws, "other ws", "x", "rolling")
        .await
        .unwrap();
    repo.create_workspace_note(other_tenant, ws, "other tenant", "x", "rolling")
        .await
        .unwrap();

    let listed = notebook_queries::list_notes(&repo, tenant, ws)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, mine.id);

    assert!(
        repo.update_workspace_note(mine.id, other_tenant, Some("hijacked".into()), None)
            .await
            .unwrap()
            .is_none(),
        "another tenant cannot edit it"
    );
    assert!(
        notebook_queries::latest_rolling_note(&repo, other_tenant, other_ws)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_folder_rename_leaves_its_parent_alone() {
    let repo = FakeNotebookRepository::new();
    let p = person();
    let parent = repo.create_folder(p, None, "parent").await.unwrap();
    let child = repo
        .create_folder(p, Some(parent.id), "child")
        .await
        .unwrap();

    let renamed = repo
        .update_folder(
            p,
            child.id,
            FolderEdit {
                name: Some("renamed".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .expect("updated");
    assert_eq!(renamed.name, "renamed");
    assert_eq!(
        renamed.parent_id,
        Some(parent.id),
        "a name-only edit must not evict the folder to the root"
    );
}
