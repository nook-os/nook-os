//! Personal notebook (MAIN-66): person-scoping across tenants, at-rest
//! encryption, folder reparenting, and search — driven through the real
//! handlers against a live Postgres. Set `DATABASE_URL`.

use axum::extract::{Path, Query, State};
use axum::Json;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::crypto;
use nook_control::error::ApiError;
use nook_control::routes::notebook;
use nook_control::routes::notebook::NoteListQuery;
use nook_control::services::identity::{login_identity, IdentityClaims};
use nook_testkit::TestBed;
use nook_types::*;
use sqlx::PgPool;
use uuid::Uuid;

fn claims(subject: &str, name: &str) -> IdentityClaims {
    IdentityClaims {
        issuer: "test-idp".into(),
        subject: subject.into(),
        email: Some(format!("{subject}@example.test")),
        email_verified: false,
        display_name: Some(name.into()),
        avatar_url: None,
        raw_claims: serde_json::json!({}),
    }
}

fn auth(user: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: false,
    }
}

async fn person_of(db: &PgPool, user: UserId) -> Uuid {
    sqlx::query_scalar::<_, Uuid>("SELECT person_id FROM users WHERE id = $1")
        .bind(user)
        .fetch_one(db)
        .await
        .expect("person_id")
}

fn mk_note(title: &str, content: &str, folder: Option<UserNoteFolderId>) -> CreateUserNote {
    CreateUserNote {
        title: title.into(),
        content_md: content.into(),
        folder_id: folder,
    }
}

/// The whole feature in one test, on this bed's private database (MAIN-156) —
/// the per-person scoping is exercised here, and there is no shared-DB
/// contention to serialize against.
#[tokio::test]
async fn notebook_is_person_scoped_encrypted_and_reparents() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping notebook test — no DATABASE_URL");
        return;
    };
    let state = bed.app_state().await;

    let a_sub = format!("alice-{}", Uuid::now_v7().simple());
    let b_sub = format!("bob-{}", Uuid::now_v7().simple());
    let (a_user, a_tenant) = login_identity(&state, claims(&a_sub, "Alice"))
        .await
        .expect("alice signs in");
    let (b_user, b_tenant) = login_identity(&state, claims(&b_sub, "Bob"))
        .await
        .expect("bob signs in");
    let person_a = person_of(&bed.pool, a_user.id).await;
    let a = auth(a_user.id, a_tenant.id);
    let b = auth(b_user.id, b_tenant.id);

    // ── Create a folder + a note in it (AC-4) ───────────────────────────────
    let folder = notebook::create_folder(
        State(state.clone()),
        a,
        Json(CreateUserNoteFolder {
            name: "Work".into(),
            parent_id: None,
        }),
    )
    .await
    .expect("create folder")
    .0;
    let note = notebook::create_note(
        State(state.clone()),
        a,
        Json(mk_note("Roadmap", "# secret plans", Some(folder.id))),
    )
    .await
    .expect("create note")
    .0;
    assert_eq!(
        note.content_md.as_deref(),
        Some("# secret plans"),
        "get-back is decrypted"
    );
    assert!(!note.sealed, "a fresh note is not sealed");
    assert_eq!(note.folder_id, Some(folder.id));

    // ── At rest it is ciphertext (AC-2) ─────────────────────────────────────
    let stored: Vec<u8> = sqlx::query_scalar("SELECT content_enc FROM user_notes WHERE id = $1")
        .bind(note.id)
        .fetch_one(&bed.pool)
        .await
        .expect("content_enc");
    assert_ne!(
        stored,
        b"# secret plans".to_vec(),
        "a raw select must not expose the body"
    );
    assert_eq!(
        state.vault.decrypt_string(&stored).unwrap(),
        "# secret plans",
        "but the vault round-trips it"
    );

    // ── Person B sees an empty notebook and cannot read A's note (AC-3) ──────
    let b_list = notebook::list_notes(State(state.clone()), b, Query(NoteListQuery { q: None }))
        .await
        .expect("b lists")
        .0;
    assert!(b_list.is_empty(), "B's notebook is empty");
    let b_get = notebook::get_note(State(state.clone()), b, Path(note.id)).await;
    assert!(b_get.is_err(), "B cannot read A's note by id");

    // ── Same person, a DIFFERENT tenant → the SAME notebook (AC-3) ───────────
    // A second user row for person A in a fresh tenant: the notebook follows the
    // person, not the tenant/user.
    let t2 = bed.tenant("t2").await;
    // A second user row for person A (custom person_id → kept as a raw insert).
    let a2_user = UserId::new();
    sqlx::query(
        "INSERT INTO users (id, tenant_id, person_id, display_name, email)
         VALUES ($1, $2, $3, 'Alice', $4)",
    )
    .bind(a2_user)
    .bind(t2)
    .bind(person_a)
    .bind(format!("{a_sub}+t2@example.test"))
    .execute(&bed.pool)
    .await
    .expect("alice in tenant 2");
    let a2 = auth(a2_user, t2);
    let a2_list = notebook::list_notes(State(state.clone()), a2, Query(NoteListQuery { q: None }))
        .await
        .expect("a2 lists")
        .0;
    assert_eq!(a2_list.len(), 1, "same notebook signed into another tenant");
    assert_eq!(a2_list[0].id, note.id);
    assert_eq!(
        a2_list[0].path, "Work",
        "the folder path is plaintext metadata"
    );

    // ── Search over title + path, person-scoped (AC-6) ──────────────────────
    let by_title = notebook::list_notes(
        State(state.clone()),
        a,
        Query(NoteListQuery {
            q: Some("road".into()),
        }),
    )
    .await
    .expect("search title")
    .0;
    assert_eq!(by_title.len(), 1, "ILIKE matches the title");
    let by_path = notebook::list_notes(
        State(state.clone()),
        a,
        Query(NoteListQuery {
            q: Some("work".into()),
        }),
    )
    .await
    .expect("search path")
    .0;
    assert_eq!(by_path.len(), 1, "ILIKE matches the folder path");
    let miss = notebook::list_notes(
        State(state.clone()),
        a,
        Query(NoteListQuery {
            q: Some("nonsense".into()),
        }),
    )
    .await
    .expect("search miss")
    .0;
    assert!(miss.is_empty());

    // ── Update: move to root + change body (AC-4) ───────────────────────────
    let moved = notebook::update_note(
        State(state.clone()),
        a,
        Path(note.id),
        Json(UpdateUserNote {
            title: None,
            content_md: Some("# revised".into()),
            folder_id: Some(None), // to root
        }),
    )
    .await
    .expect("update note")
    .0;
    assert_eq!(moved.folder_id, None, "moved to root");
    assert_eq!(
        moved.content_md.as_deref(),
        Some("# revised"),
        "body re-encrypted + decrypted"
    );

    // ── Folder delete REPARENTS notes, never cascade-deletes (AC-4) ─────────
    // A nested folder with a note; deleting the parent lifts the child + note.
    let parent = notebook::create_folder(
        State(state.clone()),
        a,
        Json(CreateUserNoteFolder {
            name: "Parent".into(),
            parent_id: None,
        }),
    )
    .await
    .expect("parent")
    .0;
    let child = notebook::create_folder(
        State(state.clone()),
        a,
        Json(CreateUserNoteFolder {
            name: "Child".into(),
            parent_id: Some(parent.id),
        }),
    )
    .await
    .expect("child")
    .0;
    let child_note = notebook::create_note(
        State(state.clone()),
        a,
        Json(mk_note("Nested", "body", Some(child.id))),
    )
    .await
    .expect("child note")
    .0;
    // Delete the CHILD → its note reparents to the child's parent (Parent).
    notebook::delete_folder(State(state.clone()), a, Path(child.id))
        .await
        .expect("delete child");
    let reparented = notebook::get_note(State(state.clone()), a, Path(child_note.id))
        .await
        .expect("note survived the folder delete")
        .0;
    assert_eq!(
        reparented.folder_id,
        Some(parent.id),
        "the note rose to the deleted folder's parent, not gone"
    );

    // Delete a note.
    notebook::delete_note(State(state.clone()), a, Path(note.id))
        .await
        .expect("delete note");
    assert!(
        notebook::get_note(State(state.clone()), a, Path(note.id))
            .await
            .is_err(),
        "deleted note is gone"
    );

    bed.teardown().await;
}

/// MAIN-84 hardening: the folder-cycle guard, the blank-name rejection, and the
/// 200-char cap — all on create and update, for notes and folders, driven
/// through the real handlers.
#[tokio::test]
async fn notebook_hardening_cycles_blanks_and_length() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping notebook hardening test — no DATABASE_URL");
        return;
    };
    let state = bed.app_state().await;
    let sub = format!("carol-{}", Uuid::now_v7().simple());
    let (user, tenant) = login_identity(&state, claims(&sub, "Carol"))
        .await
        .expect("carol signs in");
    let c = auth(user.id, tenant.id);

    // A generic fn, not a closure: it is applied to Results with different Ok
    // types (folder vs note), which a single closure cannot be.
    fn is_400<T>(r: &Result<T, ApiError>) -> bool {
        matches!(r, Err(ApiError::BadRequest(_)))
    }

    // ── AC-2 / AC-3: blank + over-long on CREATE, note and folder ────────────
    for bad in ["", "   "] {
        assert!(
            is_400(
                &notebook::create_folder(
                    State(state.clone()),
                    c,
                    Json(CreateUserNoteFolder {
                        name: bad.into(),
                        parent_id: None
                    }),
                )
                .await
            ),
            "a blank folder name is a 400: {bad:?}"
        );
        assert!(
            is_400(
                &notebook::create_note(State(state.clone()), c, Json(mk_note(bad, "b", None)))
                    .await
            ),
            "a blank note title is a 400: {bad:?}"
        );
    }
    assert!(
        is_400(
            &notebook::create_folder(
                State(state.clone()),
                c,
                Json(CreateUserNoteFolder {
                    name: "x".repeat(201),
                    parent_id: None
                }),
            )
            .await
        ),
        "a 201-char folder name is a 400"
    );
    // 200 is exactly at the cap → accepted.
    let f200 = notebook::create_folder(
        State(state.clone()),
        c,
        Json(CreateUserNoteFolder {
            name: "y".repeat(200),
            parent_id: None,
        }),
    )
    .await
    .expect("a 200-char name is accepted")
    .0;
    assert_eq!(f200.name.chars().count(), 200);

    // ── AC-1: cycle guard ────────────────────────────────────────────────────
    // A, then B under A. Moving A under B (its own descendant) must be refused,
    // and A must be unchanged. Self-parent is the same guard.
    let a = notebook::create_folder(
        State(state.clone()),
        c,
        Json(CreateUserNoteFolder {
            name: "A".into(),
            parent_id: None,
        }),
    )
    .await
    .expect("A")
    .0;
    let b = notebook::create_folder(
        State(state.clone()),
        c,
        Json(CreateUserNoteFolder {
            name: "B".into(),
            parent_id: Some(a.id),
        }),
    )
    .await
    .expect("B under A")
    .0;
    let cyc = notebook::update_folder(
        State(state.clone()),
        c,
        Path(a.id),
        Json(UpdateUserNoteFolder {
            name: None,
            parent_id: Some(Some(b.id)),
        }),
    )
    .await;
    assert!(is_400(&cyc), "moving A under its descendant B is a 400");
    let a_parent: Option<UserNoteFolderId> =
        sqlx::query_scalar("SELECT parent_id FROM user_note_folders WHERE id = $1")
            .bind(a.id)
            .fetch_one(&bed.pool)
            .await
            .unwrap();
    assert_eq!(
        a_parent, None,
        "A's parent is unchanged after the rejected move"
    );
    assert!(
        is_400(
            &notebook::update_folder(
                State(state.clone()),
                c,
                Path(a.id),
                Json(UpdateUserNoteFolder {
                    name: None,
                    parent_id: Some(Some(a.id))
                }),
            )
            .await
        ),
        "self-parent is refused by the same guard"
    );
    // A legal move to an unrelated folder still works.
    let d = notebook::create_folder(
        State(state.clone()),
        c,
        Json(CreateUserNoteFolder {
            name: "D".into(),
            parent_id: None,
        }),
    )
    .await
    .expect("D")
    .0;
    let b_moved = notebook::update_folder(
        State(state.clone()),
        c,
        Path(b.id),
        Json(UpdateUserNoteFolder {
            name: None,
            parent_id: Some(Some(d.id)),
        }),
    )
    .await
    .expect("a move to an unrelated folder succeeds")
    .0;
    assert_eq!(b_moved.parent_id, Some(d.id));

    // ── AC-2 / AC-3 on UPDATE: folder rename ─────────────────────────────────
    assert!(
        is_400(
            &notebook::update_folder(
                State(state.clone()),
                c,
                Path(a.id),
                Json(UpdateUserNoteFolder {
                    name: Some("   ".into()),
                    parent_id: None
                }),
            )
            .await
        ),
        "a blank rename is a 400, not a silent COALESCE no-op"
    );
    let a_name: String = sqlx::query_scalar("SELECT name FROM user_note_folders WHERE id = $1")
        .bind(a.id)
        .fetch_one(&bed.pool)
        .await
        .unwrap();
    assert_eq!(
        a_name, "A",
        "the name did not change on the rejected blank rename"
    );
    // An omitted name leaves it unchanged (a move-only / no-op update).
    let kept = notebook::update_folder(
        State(state.clone()),
        c,
        Path(a.id),
        Json(UpdateUserNoteFolder {
            name: None,
            parent_id: None,
        }),
    )
    .await
    .expect("omitted name is fine")
    .0;
    assert_eq!(kept.name, "A", "an omitted name leaves it unchanged");
    assert!(
        is_400(
            &notebook::update_folder(
                State(state.clone()),
                c,
                Path(a.id),
                Json(UpdateUserNoteFolder {
                    name: Some("z".repeat(201)),
                    parent_id: None
                }),
            )
            .await
        ),
        "a 201-char rename is a 400"
    );
    let a200 = notebook::update_folder(
        State(state.clone()),
        c,
        Path(a.id),
        Json(UpdateUserNoteFolder {
            name: Some("z".repeat(200)),
            parent_id: None,
        }),
    )
    .await
    .expect("a 200-char rename is accepted")
    .0;
    assert_eq!(a200.name.chars().count(), 200);

    // ── AC-2 / AC-3 on UPDATE: note title ────────────────────────────────────
    let note = notebook::create_note(
        State(state.clone()),
        c,
        Json(mk_note("Title", "body", None)),
    )
    .await
    .expect("note")
    .0;
    for bad in ["", "   "] {
        assert!(
            is_400(
                &notebook::update_note(
                    State(state.clone()),
                    c,
                    Path(note.id),
                    Json(UpdateUserNote {
                        title: Some(bad.into()),
                        content_md: None,
                        folder_id: None
                    }),
                )
                .await
            ),
            "a blank note title on update is a 400: {bad:?}"
        );
    }
    assert!(
        is_400(
            &notebook::update_note(
                State(state.clone()),
                c,
                Path(note.id),
                Json(UpdateUserNote {
                    title: Some("t".repeat(201)),
                    content_md: None,
                    folder_id: None
                }),
            )
            .await
        ),
        "a 201-char note title is a 400"
    );
    // An omitted title with a body change leaves the title unchanged.
    let body_only = notebook::update_note(
        State(state.clone()),
        c,
        Path(note.id),
        Json(UpdateUserNote {
            title: None,
            content_md: Some("new".into()),
            folder_id: None,
        }),
    )
    .await
    .expect("title omitted is fine")
    .0;
    assert_eq!(
        body_only.title, "Title",
        "an omitted title leaves it unchanged"
    );

    bed.teardown().await;
}

/// AC-5 belt-and-braces: no operator surface may read notebook rows. The
/// in-module source-text test proves `notebook.rs` never consults perm/policy;
/// this proves the operator route never selects the notebook tables.
#[test]
fn operator_routes_never_touch_the_notebook() {
    let op = include_str!("../src/routes/operator.rs");
    assert!(
        !op.contains("user_notes") && !op.contains("user_note_folders"),
        "operator.rs must never query the person-owned notebook tables"
    );
}

// ── MAIN-100: person vault + zero-knowledge seal contract ──────────────────────

fn b64e(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn b64d(s: &str) -> Vec<u8> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s.as_bytes())
        .expect("base64")
}

/// Build a seal request from a client blob (fabricated with the same primitive
/// the browser would use) + the app password that authorizes it.
fn seal_req(sealed: &crypto::Sealed, passphrase: &str) -> SealNoteRequest {
    SealNoteRequest {
        salt: b64e(&sealed.salt),
        verifier: b64e(&sealed.verifier),
        ciphertext: b64e(&sealed.ciphertext),
        passphrase: passphrase.into(),
    }
}

fn pass_req(passphrase: &str) -> SetVaultPassphraseRequest {
    SetVaultPassphraseRequest {
        passphrase: passphrase.into(),
    }
}

/// The seal contract end to end: set-once person vault, seal (server never sees
/// plaintext), sealed read returns a blob not the body, a body PATCH on a sealed
/// note 409s while title edits/search keep working, unseal round-trips back to a
/// normal note, and every new endpoint is person-scoped (MAIN-100 AC-1..AC-6).
#[tokio::test]
async fn notebook_person_vault_and_seal_contract() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping notebook seal test — no DATABASE_URL");
        return;
    };
    let state = bed.app_state().await;

    let a_sub = format!("seal-a-{}", Uuid::now_v7().simple());
    let b_sub = format!("seal-b-{}", Uuid::now_v7().simple());
    let (a_user, a_tenant) = login_identity(&state, claims(&a_sub, "Ana"))
        .await
        .expect("ana signs in");
    let (b_user, b_tenant) = login_identity(&state, claims(&b_sub, "Ben"))
        .await
        .expect("ben signs in");
    let a = auth(a_user.id, a_tenant.id);
    let b = auth(b_user.id, b_tenant.id);
    let pass = "correct horse staple";

    // ── Person vault: unset → too-short → set → set-once conflict → verify ────
    let st = notebook::vault_status(State(state.clone()), a)
        .await
        .expect("status")
        .0;
    assert!(!st.configured, "no vault yet");

    let short =
        notebook::set_vault_passphrase(State(state.clone()), a, Json(pass_req("short"))).await;
    assert!(matches!(short, Err(ApiError::BadRequest(_))), "min 8 chars");

    let set = notebook::set_vault_passphrase(State(state.clone()), a, Json(pass_req(pass)))
        .await
        .expect("set")
        .0;
    assert!(set.configured);

    let again = notebook::set_vault_passphrase(State(state.clone()), a, Json(pass_req(pass))).await;
    assert!(matches!(again, Err(ApiError::Conflict(_))), "set once only");

    notebook::verify_vault_passphrase(State(state.clone()), a, Json(pass_req(pass)))
        .await
        .expect("verify accepts the right password");
    let bad =
        notebook::verify_vault_passphrase(State(state.clone()), a, Json(pass_req("nope"))).await;
    assert!(
        matches!(bad, Err(ApiError::Forbidden)),
        "wrong password → 403"
    );

    // ── Sealing with no vault → 428 (B has not set one) ──────────────────────
    let b_note = notebook::create_note(State(state.clone()), b, Json(mk_note("B", "b body", None)))
        .await
        .expect("b note")
        .0;
    let junk = crypto::seal_with_passphrase(b"x", "whatever").unwrap();
    let setup = notebook::seal_note(
        State(state.clone()),
        b,
        Path(b_note.id),
        Json(seal_req(&junk, "whatever")),
    )
    .await;
    assert!(
        matches!(setup, Err(ApiError::SetupRequired(_))),
        "no vault → 428 SetupRequired"
    );

    // ── Seal A's note with a client blob (server never sees plaintext) ───────
    let note = notebook::create_note(
        State(state.clone()),
        a,
        Json(mk_note("Plans", "# plaintext body", None)),
    )
    .await
    .expect("note")
    .0;
    let plaintext = b"# sealed body \xE2\x9C\x93".to_vec(); // non-ascii byte on purpose
    let sealed = crypto::seal_with_passphrase(&plaintext, pass).unwrap();

    let wrong = notebook::seal_note(
        State(state.clone()),
        a,
        Path(note.id),
        Json(seal_req(&sealed, "not the pass")),
    )
    .await;
    assert!(
        matches!(wrong, Err(ApiError::Forbidden)),
        "wrong app password → 403"
    );

    let out = notebook::seal_note(
        State(state.clone()),
        a,
        Path(note.id),
        Json(seal_req(&sealed, pass)),
    )
    .await
    .expect("seal")
    .0;
    assert!(out.sealed, "now sealed");
    assert!(
        out.content_md.is_none(),
        "a sealed response carries no plaintext"
    );
    let blob = out.blob.expect("blob present");
    assert_eq!(blob.iterations, crypto::KDF_ITERATIONS);
    assert_eq!(
        b64d(&blob.salt),
        sealed.salt,
        "the blob echoes the seal salt"
    );

    // ── At rest: the vault layer peels only to the SEALED ciphertext, which the
    //    server cannot open; only the passphrase does ─────────────────────────
    let stored: Vec<u8> = sqlx::query_scalar("SELECT content_enc FROM user_notes WHERE id = $1")
        .bind(note.id)
        .fetch_one(&bed.pool)
        .await
        .unwrap();
    let unwrapped = state.vault.decrypt(&stored).unwrap();
    assert_ne!(
        unwrapped, plaintext,
        "the vault layer alone never yields plaintext"
    );
    let blob_ct = b64d(&blob.ciphertext);
    assert_eq!(blob_ct, unwrapped, "read blob == stored sealed ciphertext");
    let opened =
        crypto::open_with_passphrase(&blob_ct, &sealed.salt, &sealed.verifier, pass).unwrap();
    assert_eq!(opened, plaintext, "only the passphrase opens it");

    // ── Reading it back: sealed:true + blob, no content_md (AC-4) ─────────────
    let got = notebook::get_note(State(state.clone()), a, Path(note.id))
        .await
        .expect("get")
        .0;
    assert!(got.sealed && got.content_md.is_none() && got.blob.is_some());

    // ── Body PATCH on a sealed note 409s; a title edit still works (AC-4/5) ────
    let body_patch = notebook::update_note(
        State(state.clone()),
        a,
        Path(note.id),
        Json(UpdateUserNote {
            content_md: Some("hijack".into()),
            ..Default::default()
        }),
    )
    .await;
    assert!(
        matches!(body_patch, Err(ApiError::Conflict(_))),
        "body PATCH on sealed → 409"
    );
    let retitled = notebook::update_note(
        State(state.clone()),
        a,
        Path(note.id),
        Json(UpdateUserNote {
            title: Some("Renamed".into()),
            ..Default::default()
        }),
    )
    .await
    .expect("retitle")
    .0;
    assert!(retitled.sealed, "a title edit keeps a note sealed");

    // ── Title search still finds the sealed note, marked sealed (AC-5) ────────
    let hits = notebook::list_notes(
        State(state.clone()),
        a,
        Query(NoteListQuery {
            q: Some("Renamed".into()),
        }),
    )
    .await
    .expect("search")
    .0;
    assert!(
        hits.iter().any(|n| n.id == note.id && n.sealed),
        "search finds the sealed note by title and marks it sealed"
    );

    // ── Cross-person isolation: B (own vault set) cannot seal/unseal A's note ─
    let _ =
        notebook::set_vault_passphrase(State(state.clone()), b, Json(pass_req("ben pass word")))
            .await
            .expect("b vault");
    // B's blob is consistent with B's own password, so the seal passes its blob
    // check and fails only on person scoping — proving the isolation, not the
    // blob guard.
    let b_blob = crypto::seal_with_passphrase(b"ben body", "ben pass word").unwrap();
    let b_seals_a = notebook::seal_note(
        State(state.clone()),
        b,
        Path(note.id),
        Json(seal_req(&b_blob, "ben pass word")),
    )
    .await;
    assert!(
        matches!(b_seals_a, Err(ApiError::NotFound)),
        "B cannot seal A's note"
    );
    let b_unseals_a = notebook::unseal_note(
        State(state.clone()),
        b,
        Path(note.id),
        Json(UnsealNoteRequest {
            content_md: "x".into(),
            passphrase: "ben pass word".into(),
        }),
    )
    .await;
    assert!(
        matches!(b_unseals_a, Err(ApiError::NotFound)),
        "B cannot unseal A's note"
    );

    // ── Unseal round-trips back to a normal server-encrypted note (AC-3) ──────
    let recovered = String::from_utf8(opened).unwrap();
    let un = notebook::unseal_note(
        State(state.clone()),
        a,
        Path(note.id),
        Json(UnsealNoteRequest {
            content_md: recovered.clone(),
            passphrase: pass.into(),
        }),
    )
    .await
    .expect("unseal")
    .0;
    assert!(!un.sealed, "back to unsealed");
    assert_eq!(un.content_md.as_deref(), Some(recovered.as_str()));
    let after: Vec<u8> = sqlx::query_scalar("SELECT content_enc FROM user_notes WHERE id = $1")
        .bind(note.id)
        .fetch_one(&bed.pool)
        .await
        .unwrap();
    assert_eq!(
        state.vault.decrypt_string(&after).unwrap(),
        recovered,
        "the note is server-readable again after unsealing"
    );

    bed.teardown().await;
}
