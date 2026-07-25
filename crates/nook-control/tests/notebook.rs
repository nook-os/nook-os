//! Personal notebook (MAIN-66): person-scoping across tenants, at-rest
//! encryption, folder reparenting, and search — driven through the real
//! handlers against a live Postgres. Set `DATABASE_URL`.

use axum::extract::{Path, Query, State};
use axum::Json;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::config::Config;
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
