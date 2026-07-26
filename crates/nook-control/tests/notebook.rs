//! Personal notebook (MAIN-66): person-scoping across tenants, at-rest
//! encryption, folder reparenting, and search — driven through the real
//! handlers against a live Postgres. Set `DATABASE_URL`.

use axum::extract::{Path, Query, State};
use axum::Json;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::config::Config;
use nook_control::error::ApiError;
use nook_control::routes::notebook;
use nook_control::routes::notebook::NoteListQuery;
use nook_control::services::identity::{login_identity, IdentityClaims};
use nook_control::state::AppState;
use nook_types::*;
use sqlx::PgPool;
use tokio::sync::Mutex;
use uuid::Uuid;

mod common;
use common::test_pool;

// Provisioning the first identity races on the seeded tenant; serialize like the
// other identity-driven suites.
static SERIAL: Mutex<()> = Mutex::const_new(());

fn test_config() -> Config {
    Config {
        app_env: "test".into(),
        bind: "127.0.0.1:0".into(),
        shutdown_grace_secs: 25,
        public_base_url: "http://localhost:8080".into(),
        web_origin: "http://localhost:5173".into(),
        database_url: std::env::var("DATABASE_URL").unwrap_or_default(),
        oidc_issuer_url: None,
        oidc_client_id: None,
        oidc_device_client_id: None,
        oidc_device_authorization_endpoint: None,
        oidc_client_secret: None,
        oidc_redirect_url: None,
        oidc_scopes: "openid profile email".into(),
        session_secret: "0".repeat(64),
        session_ttl_hours: 168,
        default_tenant_name: format!("test-{}", Uuid::now_v7().simple()),
        auth_dev_mode: true,
        mcp_token: None,
        dev_join_token: None,
        dist_dir: "/nonexistent".into(),
        agent_bind: "127.0.0.1:0".into(),
        agent_public_url: None,
        agent_tls_cert: None,
        agent_tls_key: None,
        releases_repo: "nook-os/nook-os".into(),
        artifact_store: "disk".into(),
        artifact_prefix: "nook".into(),
        artifact_redirect: false,
        s3_bucket: None,
        s3_endpoint: None,
        s3_region: None,
        s3_access_key_id: None,
        s3_secret_access_key: None,
        s3_path_style: true,
        cache_provider: "memory".into(),
        mail_provider: "capture".into(),
        smtp_host: None,
        smtp_port: 587,
        smtp_tls: "starttls".into(),
        smtp_from: "NookOS <no-reply@localhost>".into(),
        smtp_username: None,
        smtp_password: None,
        postmark_token: None,
        postmark_api_url: "https://api.postmarkapp.com/email".into(),
        mail_from: "NookOS <no-reply@localhost>".into(),
        mail_send_enabled: false,
        mail_notifications_enabled: false,
        mail_max_per_month: Some(100),
        mail_max_per_day: None,
        trusted_proxies: Vec::new(),
    }
}

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

/// The whole feature in one test — the notebook shares a deployment-wide table,
/// so separate parallel tests would step on each other; the SERIAL lock and
/// per-person scoping keep this hermetic.
#[tokio::test]
async fn notebook_is_person_scoped_encrypted_and_reparents() {
    let _serial = SERIAL.lock().await;
    let Some(pool) = test_pool().await else {
        eprintln!("skipping notebook test — no DATABASE_URL");
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;

    let a_sub = format!("alice-{}", Uuid::now_v7().simple());
    let b_sub = format!("bob-{}", Uuid::now_v7().simple());
    let (a_user, a_tenant) = login_identity(&state, claims(&a_sub, "Alice"))
        .await
        .expect("alice signs in");
    let (b_user, b_tenant) = login_identity(&state, claims(&b_sub, "Bob"))
        .await
        .expect("bob signs in");
    let person_a = person_of(&pool, a_user.id).await;
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
    assert_eq!(note.content_md, "# secret plans", "get-back is decrypted");
    assert_eq!(note.folder_id, Some(folder.id));

    // ── At rest it is ciphertext (AC-2) ─────────────────────────────────────
    let stored: Vec<u8> = sqlx::query_scalar("SELECT content_enc FROM user_notes WHERE id = $1")
        .bind(note.id)
        .fetch_one(&pool)
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
    let t2 = TenantId::new();
    sqlx::query("INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $2)")
        .bind(t2)
        .bind(format!("t2-{}", t2.0.simple()))
        .execute(&pool)
        .await
        .expect("tenant 2");
    let a2_user = UserId::new();
    sqlx::query(
        "INSERT INTO users (id, tenant_id, person_id, display_name, email)
         VALUES ($1, $2, $3, 'Alice', $4)",
    )
    .bind(a2_user)
    .bind(t2)
    .bind(person_a)
    .bind(format!("{a_sub}+t2@example.test"))
    .execute(&pool)
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
        moved.content_md, "# revised",
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

    // Cleanup created tenants (never the seeded 'dev' one).
    for t in [a_tenant.id, b_tenant.id, t2] {
        let _ = sqlx::query("DELETE FROM tenants WHERE id = $1 AND slug <> 'dev'")
            .bind(t)
            .execute(&pool)
            .await;
    }
}

/// MAIN-84 hardening: the folder-cycle guard, the blank-name rejection, and the
/// 200-char cap — all on create and update, for notes and folders, driven
/// through the real handlers.
#[tokio::test]
async fn notebook_hardening_cycles_blanks_and_length() {
    let _serial = SERIAL.lock().await;
    let Some(pool) = test_pool().await else {
        eprintln!("skipping notebook hardening test — no DATABASE_URL");
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;
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
            .fetch_one(&pool)
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
        .fetch_one(&pool)
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

    let _ = sqlx::query("DELETE FROM tenants WHERE id = $1 AND slug <> 'dev'")
        .bind(tenant.id)
        .execute(&pool)
        .await;
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
