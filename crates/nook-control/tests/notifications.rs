//! MAIN-91: comments and escalation labels become notable, and the kind
//! catalog is exposed. End-to-end through the real routes against a live
//! Postgres — a comment on a team card raises a notification, a comment on a
//! private card does not, `blocked` notifies while an ordinary label is silent,
//! and the catalog route lists every notable kind. Set `DATABASE_URL`.

use axum::extract::{Path, State};
use axum::Json;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::config::Config;
use nook_control::services::identity::{login_identity, IdentityClaims};
use nook_control::state::AppState;
use nook_types::*;
use sqlx::PgPool;
use tokio::sync::Mutex;
use uuid::Uuid;

mod common;
use common::test_pool;

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
        queue_provider: "database".into(),
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

async fn make_task(
    state: &AppState,
    tenant: TenantId,
    board: BoardId,
    creator: UserId,
    title: &str,
    visibility: &str,
) -> TaskItem {
    let provider = state.kanban.get("local").expect("local provider");
    provider
        .create_task(
            tenant,
            board,
            Some(creator),
            CreateTaskRequest {
                title: title.into(),
                description: None,
                column_id: None,
                column_type: None,
                workspace_id: None,
                priority: None,
                type_: None,
                visibility: Some(visibility.into()),
                parent: None,
                labels: vec![],
            },
        )
        .await
        .expect("create task")
}

async fn seed_label(pool: &PgPool, tenant: TenantId, name: &str) {
    sqlx::query("INSERT INTO labels (id, tenant_id, name) VALUES ($1, $2, $3)")
        .bind(Uuid::now_v7())
        .bind(tenant)
        .bind(name)
        .execute(pool)
        .await
        .expect("seed label");
}

/// Notifications for a tenant of a given kind — the fan-out inserts the row
/// synchronously in `raise` before spawning delivery, so it is queryable the
/// moment the route returns.
async fn notes(pool: &PgPool, tenant: TenantId, kind: &str) -> Vec<Notification> {
    sqlx::query_as(
        "SELECT id, tenant_id, user_id, level, title, body, kind, link, payload,
                read_at, created_at
         FROM notifications WHERE tenant_id = $1 AND kind = $2 ORDER BY created_at",
    )
    .bind(tenant)
    .bind(kind)
    .fetch_all(pool)
    .await
    .expect("notes")
}

#[tokio::test]
async fn comments_and_escalation_labels_notify() {
    let _serial = SERIAL.lock().await;
    let Some(pool) = test_pool().await else {
        eprintln!("skipping notifications test — no DATABASE_URL");
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;

    let sub = format!("owner-{}", Uuid::now_v7().simple());
    let (owner, tenant) = login_identity(&state, claims(&sub, "Ada"))
        .await
        .expect("owner signs in");
    let a = owner.id;

    let board: BoardId = sqlx::query_scalar(
        "INSERT INTO boards (id, tenant_id, name, key, provider)
         VALUES ($1, $2, 'b', $3, 'local') RETURNING id",
    )
    .bind(BoardId::new())
    .bind(tenant.id)
    .bind(format!("N{}", &Uuid::now_v7().simple().to_string()[..6]).to_uppercase())
    .fetch_one(&pool)
    .await
    .expect("board");
    sqlx::query(
        "INSERT INTO board_columns (id, board_id, name, position, type)
         VALUES ($1, $2, 'Todo', 0, 'unstarted')",
    )
    .bind(Uuid::now_v7())
    .bind(board)
    .execute(&pool)
    .await
    .expect("column");

    let team = make_task(&state, tenant.id, board, a, "shared work", "team").await;
    let secret = make_task(&state, tenant.id, board, a, "hush hush", "private").await;

    // ── AC-1: a comment on a TEAM card raises a deep-linked notification ─────
    let _ = nook_control::routes::task_detail::create_comment(
        State(state.clone()),
        auth(a, tenant.id),
        Path(team.id.to_string()),
        Json(CreateCommentRequest {
            body_md: "can someone review this?".into(),
            author_name: None,
        }),
    )
    .await
    .expect("comment on team card");

    let comment_notes = notes(&pool, tenant.id, "task.comment.created").await;
    assert_eq!(comment_notes.len(), 1, "one comment notification");
    let n = &comment_notes[0];
    assert!(
        n.body.contains("can someone review this?"),
        "the excerpt rides in the body: {:?}",
        n.body
    );
    assert!(
        n.link
            .as_deref()
            .unwrap_or_default()
            .contains("board?task="),
        "deep-linked to the card: {:?}",
        n.link
    );

    // ── AC-1: a comment on a PRIVATE card raises nothing ────────────────────
    let _ = nook_control::routes::task_detail::create_comment(
        State(state.clone()),
        auth(a, tenant.id),
        Path(secret.id.to_string()),
        Json(CreateCommentRequest {
            body_md: "for my eyes only".into(),
            author_name: None,
        }),
    )
    .await
    .expect("comment on private card");
    assert_eq!(
        notes(&pool, tenant.id, "task.comment.created").await.len(),
        1,
        "a private card's comment must not add a notification"
    );

    // ── AC-2: an escalation label notifies; an ordinary one is silent ───────
    seed_label(&pool, tenant.id, "blocked").await;
    seed_label(&pool, tenant.id, "frontend").await;

    let _ = nook_control::routes::labels::add(
        State(state.clone()),
        auth(a, tenant.id),
        Path((team.id.to_string(), "blocked".into())),
    )
    .await
    .expect("add blocked");
    let label_notes = notes(&pool, tenant.id, "task.label.added").await;
    assert_eq!(label_notes.len(), 1, "blocked raises one notification");
    assert!(
        label_notes[0].body.contains("blocked"),
        "the label is named: {:?}",
        label_notes[0].body
    );

    let _ = nook_control::routes::labels::add(
        State(state.clone()),
        auth(a, tenant.id),
        Path((team.id.to_string(), "frontend".into())),
    )
    .await
    .expect("add frontend");
    assert_eq!(
        notes(&pool, tenant.id, "task.label.added").await.len(),
        1,
        "an ordinary label must stay silent"
    );

    let _ = sqlx::query("DELETE FROM tenants WHERE id = $1 AND slug <> 'dev'")
        .bind(tenant.id)
        .execute(&pool)
        .await;
}

/// AC-3: the catalog route lists every notable kind with label, description,
/// and group — including the two new ones — and nothing uncatalogued.
#[tokio::test]
async fn catalog_route_lists_every_notable_kind() {
    let catalog = nook_control::events::catalog();
    let ids: Vec<&str> = catalog.iter().map(|k| k.id.as_str()).collect();
    for expected in [
        "task.comment.created",
        "task.label.added",
        "task.claimed",
        "node.connected",
    ] {
        assert!(ids.contains(&expected), "catalog missing {expected}");
    }
    for k in &catalog {
        assert!(!k.label.is_empty(), "{} needs a label", k.id);
        assert!(!k.description.is_empty(), "{} needs a description", k.id);
        assert!(
            k.id.starts_with(&k.group),
            "{} not in group {}",
            k.id,
            k.group
        );
    }
    // The handler returns exactly the catalog.
    let Json(served) = nook_control::routes::notifications::notification_kinds().await;
    assert_eq!(
        served.len(),
        catalog.len(),
        "route serves the whole catalog"
    );
}
