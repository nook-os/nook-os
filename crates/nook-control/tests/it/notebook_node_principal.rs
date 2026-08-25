//! A node token must not reach the notebook (MAIN-577).
//!
//! `node_token_ctx` resolves a node's token to an `AuthCtx` carrying the TENANT
//! OWNER's `user_id` — deliberately, so a machine's tenant-scoped queries and
//! event attribution work — marked `Principal::Node`. The notebook's whole
//! access model is `person_id_for`, and it used to resolve that borrowed user
//! id into the owner's person without asking what kind of principal was
//! holding it. Every enrolled machine has such a token on disk.
//!
//! Driven through the real handlers, one per verb class, because the defect was
//! that nine routes each independently forgot the check — a single route's test
//! would have passed throughout.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::error::ApiError;
use nook_control::routes::notebook;
use nook_control::routes::notebook::NoteListQuery;
use nook_control::services::identity::{login_identity, IdentityClaims};
use nook_testkit::TestBed;
use nook_types::*;
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

fn user_ctx(user: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: false,
    }
}

/// Exactly what `node_token_ctx` builds: the tenant owner's user id, a nil
/// session, and a node principal. Constructed here rather than by minting a
/// token so the test states the shape it is defending against.
fn node_ctx(owner: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: owner,
        tenant_id: tenant,
        principal: Principal::Node(NodeId(Uuid::now_v7())),
        cookie_session: false,
    }
}

/// The refusal `require_user()` gives everywhere else — a 403, not a
/// notebook-specific shape (AC-2). Asserted as a status, not merely "it
/// errored", so a 404 or a 500 would fail this.
#[track_caller]
fn assert_refused(what: &str, err: ApiError) {
    assert!(
        matches!(err, ApiError::ForbiddenMsg(_)),
        "{what}: a node principal must be refused by `require_user`, got {err:?}"
    );
    let res = err.into_response();
    assert_eq!(
        res.status(),
        StatusCode::FORBIDDEN,
        "{what}: the refusal must be the same 403 the rest of the API gives a node token"
    );
}

#[tokio::test]
async fn a_node_token_is_refused_by_every_notebook_route() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping notebook node-principal test — no DATABASE_URL");
        return;
    };
    let state = bed.app_state().await;

    let sub = format!("owner-{}", Uuid::now_v7().simple());
    let (owner, tenant) = login_identity(&state, claims(&sub, "Owner"))
        .await
        .expect("owner signs in");
    let user = user_ctx(owner.id, tenant.id);
    let node = node_ctx(owner.id, tenant.id);

    // The owner's own fixture, made as a user — the same rows a node token
    // reached before this fix.
    let folder = notebook::create_folder(
        State(state.clone()),
        user,
        Json(CreateUserNoteFolder {
            name: "Private".into(),
            parent_id: None,
        }),
    )
    .await
    .expect("owner creates a folder")
    .0;
    let note = notebook::create_note(
        State(state.clone()),
        user,
        Json(CreateUserNote {
            title: "Roadmap".into(),
            content_md: "# secret plans".into(),
            folder_id: Some(folder.id),
        }),
    )
    .await
    .expect("owner creates a note")
    .0;

    // ── list ────────────────────────────────────────────────────────────────
    assert_refused(
        "list notes",
        notebook::list_notes(State(state.clone()), node, Query(NoteListQuery { q: None }))
            .await
            .expect_err("a node token must not list the owner's notes"),
    );

    // ── read ────────────────────────────────────────────────────────────────
    assert_refused(
        "get note",
        notebook::get_note(State(state.clone()), node, Path(note.id))
            .await
            .expect_err("a node token must not read the owner's note"),
    );

    // ── create ──────────────────────────────────────────────────────────────
    assert_refused(
        "create note",
        notebook::create_note(
            State(state.clone()),
            node,
            Json(CreateUserNote {
                title: "Planted".into(),
                content_md: "by a machine".into(),
                folder_id: None,
            }),
        )
        .await
        .expect_err("a node token must not write into the owner's notebook"),
    );

    // ── update (rename) ─────────────────────────────────────────────────────
    assert_refused(
        "update note",
        notebook::update_note(
            State(state.clone()),
            node,
            Path(note.id),
            Json(UpdateUserNote {
                title: Some("Renamed".into()),
                ..Default::default()
            }),
        )
        .await
        .expect_err("a node token must not rename the owner's note"),
    );

    // ── append (the body-write shape `nook notes append` sends: a PATCH whose
    //    `content_md` is the old body plus more) ──────────────────────────────
    assert_refused(
        "append to note",
        notebook::update_note(
            State(state.clone()),
            node,
            Path(note.id),
            Json(UpdateUserNote {
                content_md: Some("# secret plans\nand a line a machine added".into()),
                ..Default::default()
            }),
        )
        .await
        .expect_err("a node token must not append to the owner's note"),
    );

    // ── delete ──────────────────────────────────────────────────────────────
    assert_refused(
        "delete note",
        notebook::delete_note(State(state.clone()), node, Path(note.id))
            .await
            .expect_err("a node token must not delete the owner's note"),
    );

    // ── folders: list, create, update, delete ───────────────────────────────
    assert_refused(
        "list folders",
        notebook::list_folders(State(state.clone()), node)
            .await
            .expect_err("a node token must not list the owner's folders"),
    );
    assert_refused(
        "create folder",
        notebook::create_folder(
            State(state.clone()),
            node,
            Json(CreateUserNoteFolder {
                name: "Planted".into(),
                parent_id: None,
            }),
        )
        .await
        .expect_err("a node token must not create a folder"),
    );
    assert_refused(
        "update folder",
        notebook::update_folder(
            State(state.clone()),
            node,
            Path(folder.id),
            Json(UpdateUserNoteFolder {
                name: Some("Renamed".into()),
                ..Default::default()
            }),
        )
        .await
        .expect_err("a node token must not rename a folder"),
    );
    assert_refused(
        "delete folder",
        notebook::delete_folder(State(state.clone()), node, Path(folder.id))
            .await
            .expect_err("a node token must not delete a folder"),
    );

    // ── the vault + seal routes, which guarded themselves already — asserted
    //    so the one-chokepoint move cannot quietly drop their check ──────────
    assert_refused(
        "vault status",
        notebook::vault_status(State(state.clone()), node)
            .await
            .expect_err("a node token must not read the owner's vault status"),
    );
    assert_refused(
        "set vault passphrase",
        notebook::set_vault_passphrase(
            State(state.clone()),
            node,
            Json(SetVaultPassphraseRequest {
                passphrase: "a-machine-chose-this".into(),
            }),
        )
        .await
        .expect_err("a node token must not set the owner's app password"),
    );
    assert_refused(
        "verify vault passphrase",
        notebook::verify_vault_passphrase(
            State(state.clone()),
            node,
            Json(SetVaultPassphraseRequest {
                passphrase: "guess".into(),
            }),
        )
        .await
        .expect_err("a node token must not test the owner's app password"),
    );
    assert_refused(
        "seal note",
        notebook::seal_note(
            State(state.clone()),
            node,
            Path(note.id),
            Json(SealNoteRequest {
                salt: String::new(),
                verifier: String::new(),
                ciphertext: String::new(),
                passphrase: "guess".into(),
            }),
        )
        .await
        .expect_err("a node token must not seal the owner's note"),
    );
    assert_refused(
        "unseal note",
        notebook::unseal_note(
            State(state.clone()),
            node,
            Path(note.id),
            Json(UnsealNoteRequest {
                content_md: "plaintext".into(),
                passphrase: "guess".into(),
            }),
        )
        .await
        .expect_err("a node token must not unseal the owner's note"),
    );

    // ── Nothing the node tried landed, and the user is unaffected (AC-3) ─────
    let listed = notebook::list_notes(State(state.clone()), user, Query(NoteListQuery { q: None }))
        .await
        .expect("the owner still lists their notes")
        .0;
    assert_eq!(listed.len(), 1, "no note was planted or deleted");
    assert_eq!(listed[0].id, note.id);

    let read = notebook::get_note(State(state.clone()), user, Path(note.id))
        .await
        .expect("the owner still reads their note")
        .0;
    assert_eq!(read.title, "Roadmap", "the rename did not land");
    assert_eq!(read.content_md.as_deref(), Some("# secret plans"));

    let folders = notebook::list_folders(State(state.clone()), user)
        .await
        .expect("the owner still lists their folders")
        .0;
    assert_eq!(folders.len(), 1, "no folder was planted or deleted");
    assert_eq!(folders[0].name, "Private", "the folder rename did not land");

    assert!(
        !notebook::vault_status(State(state.clone()), user)
            .await
            .expect("the owner still reads their vault status")
            .0
            .configured,
        "the node's app password was not set"
    );

    bed.teardown().await;
}

/// The cross-person case the borrowed identity makes possible: person B owns a
/// tenant, a machine enrolled there holds a node token, and person A's notebook
/// must be unreachable through it — before AND after, since the token resolves
/// to B's person, never A's. What this pins is that the refusal is not merely
/// "the wrong rows"; there are no rows.
#[tokio::test]
async fn a_node_token_cannot_reach_another_persons_notebook() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping notebook cross-person node test — no DATABASE_URL");
        return;
    };
    let state = bed.app_state().await;

    let a_sub = format!("alice-{}", Uuid::now_v7().simple());
    let b_sub = format!("bob-{}", Uuid::now_v7().simple());
    let (alice, a_tenant) = login_identity(&state, claims(&a_sub, "Alice"))
        .await
        .expect("alice signs in");
    let (bob, b_tenant) = login_identity(&state, claims(&b_sub, "Bob"))
        .await
        .expect("bob signs in");

    let hers = notebook::create_note(
        State(state.clone()),
        user_ctx(alice.id, a_tenant.id),
        Json(CreateUserNote {
            title: "Alice only".into(),
            content_md: "# hers".into(),
            folder_id: None,
        }),
    )
    .await
    .expect("alice creates a note")
    .0;

    // Bob's tenant's machine, borrowing Bob's user id.
    let bobs_node = node_ctx(bob.id, b_tenant.id);
    assert_refused(
        "read another person's note",
        notebook::get_note(State(state.clone()), bobs_node, Path(hers.id))
            .await
            .expect_err("a node token must not read a stranger's note"),
    );

    // And Bob himself, as a user, still cannot see it — the refusal above is
    // the principal check, not person scoping doing the work.
    let err = notebook::get_note(
        State(state.clone()),
        user_ctx(bob.id, b_tenant.id),
        Path(hers.id),
    )
    .await
    .expect_err("bob must not read alice's note");
    assert!(
        matches!(err, ApiError::NotFound),
        "a stranger's note is a 404 for a user, got {err:?}"
    );

    bed.teardown().await;
}
