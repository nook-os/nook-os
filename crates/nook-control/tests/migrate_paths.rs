//! MAIN-107: the coordinated path rewrite behind `nook migrate-workspaces`.
//!
//! The migration renames checkout directories on disk. Doing that through
//! ordinary discovery would be destructive — the reconcile deletes rows whose
//! path is no longer reported and re-inserts the moved paths as brand-new
//! checkouts (a fresh `.env` delivery, a dropped `worktree_path`). So the CLI
//! calls `migrate-paths` instead, which rewrites `node_workspaces.path` and
//! `tasks.worktree_path` IN PLACE, in one transaction, preserving row identity.
//!
//! These tests assert exactly that: both tables rewritten, ids stable, paths
//! that do not belong to the node refused, and a follow-up discovery of the NEW
//! paths producing zero inserts or deletes.
//!
//! Every assertion is scoped to rows this test created (MAIN-93): the shared dev
//! database is aged, so nothing here counts globally.
//!
//! Needs a running Postgres: set `DATABASE_URL`.

use nook_control::services::discovery::{self, migrate_paths};
use nook_control::state::AppState;
use nook_proto::DiscoveredWorkspace;
use nook_types::{
    BoardId, MigratePathPair, NodeId, NodeWorkspaceId, TaskId, TenantId, WorkspaceId,
};
use sqlx::PgPool;
use uuid::Uuid;

mod common;
use common::test_pool;

/// The one repo this test moves, and the two on-disk layouts it lives in.
struct Fixture {
    tenant: TenantId,
    node: NodeId,
    workspace: WorkspaceId,
    /// node_workspaces row id for the primary checkout.
    primary_ws: NodeWorkspaceId,
    /// node_workspaces row id for the sibling worktree.
    worktree_ws: NodeWorkspaceId,
    task: TaskId,
    remote: String,
}

/// A tenant, an online node, a workspace, a primary checkout + sibling worktree,
/// and a task whose worktree lives at the primary's OLD path.
async fn seed(db: &PgPool, legacy: &str, slug: &str) -> Fixture {
    let tenant = TenantId::new();
    let node = NodeId::new();
    let workspace = WorkspaceId::new();
    let remote = format!("git@github.com:acme/m107-{}.git", Uuid::now_v7().simple());
    let normalized = discovery::normalize_remote(&remote);

    sqlx::query("INSERT INTO tenants (id, slug, name) VALUES ($1, $2, $2)")
        .bind(tenant)
        .bind(format!("t-{}", Uuid::now_v7().simple()))
        .execute(db)
        .await
        .expect("tenant");
    sqlx::query(
        "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status)
         VALUES ($1, $2, $3, $3, 'online')",
    )
    .bind(node)
    .bind(tenant)
    .bind(format!("n-{}", Uuid::now_v7().simple()))
    .execute(db)
    .await
    .expect("node");
    sqlx::query(
        "INSERT INTO workspaces (id, tenant_id, name, slug, git_remote_normalized)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(workspace)
    .bind(tenant)
    .bind(format!("acme/w-{}", Uuid::now_v7().simple()))
    .bind(format!("w-{}", Uuid::now_v7().simple()))
    .bind(&normalized)
    .execute(db)
    .await
    .expect("workspace");

    let primary_ws = NodeWorkspaceId::new();
    let worktree_ws = NodeWorkspaceId::new();
    let insert_nw = |id: NodeWorkspaceId, path: String, worktree: bool| {
        let normalized = normalized.clone();
        let remote = remote.clone();
        async move {
            sqlx::query(
                "INSERT INTO node_workspaces
                   (id, tenant_id, node_id, workspace_id, path, git_remote_url,
                    git_remote_normalized, git_branch, git_status)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,'main',$8)",
            )
            .bind(id)
            .bind(tenant)
            .bind(node)
            .bind(workspace)
            .bind(path)
            .bind(&remote)
            .bind(&normalized)
            .bind(serde_json::json!({ "dirty": false, "worktree": worktree }))
            .execute(db)
            .await
            .expect("node_workspace");
        }
    };
    insert_nw(primary_ws, format!("{legacy}/acme/repo"), false).await;
    insert_nw(worktree_ws, format!("{legacy}/acme/repo__feature"), true).await;
    let _ = slug; // the caller derives new paths from it directly.

    // A board + column + task whose worktree is the primary's OLD path.
    let board = BoardId::new();
    sqlx::query(
        "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b',$3,'local')",
    )
    .bind(board)
    .bind(tenant)
    .bind(format!("B{}", &board.0.simple().to_string()[..6]).to_uppercase())
    .execute(db)
    .await
    .expect("board");
    let column = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO board_columns (id, board_id, name, position, type)
         VALUES ($1,$2,'In Progress',0,'started')",
    )
    .bind(column)
    .bind(board)
    .execute(db)
    .await
    .expect("column");
    let task = TaskId::new();
    sqlx::query(
        "INSERT INTO tasks (id, tenant_id, board_id, column_id, title,
                            worktree_path, worktree_node_id)
         VALUES ($1,$2,$3,$4,'work',$5,$6)",
    )
    .bind(task)
    .bind(tenant)
    .bind(board)
    .bind(column)
    .bind(format!("{legacy}/acme/repo"))
    .bind(node)
    .execute(db)
    .await
    .expect("task");

    Fixture {
        tenant,
        node,
        workspace,
        primary_ws,
        worktree_ws,
        task,
        remote,
    }
}

async fn cleanup(db: &PgPool, tenant: TenantId) {
    let _ = sqlx::query("DELETE FROM tenants WHERE id = $1")
        .bind(tenant)
        .execute(db)
        .await;
}

async fn nw_path(db: &PgPool, id: NodeWorkspaceId) -> Option<String> {
    sqlx::query_as::<_, (String,)>("SELECT path FROM node_workspaces WHERE id = $1")
        .bind(id)
        .fetch_optional(db)
        .await
        .expect("query")
        .map(|(p,)| p)
}

async fn task_worktree(db: &PgPool, id: TaskId) -> Option<String> {
    sqlx::query_as::<_, (Option<String>,)>("SELECT worktree_path FROM tasks WHERE id = $1")
        .bind(id)
        .fetch_one(db)
        .await
        .expect("query")
        .0
}

#[tokio::test]
async fn rewrites_both_tables_and_preserves_row_ids() {
    let Some(db) = test_pool().await else { return };
    let legacy = "/home/u/.nook/workspace";
    let slug = "/home/u/.nook/workspace/cp.example.com";
    let f = seed(&db, legacy, slug).await;

    let pairs = vec![
        MigratePathPair {
            old: format!("{legacy}/acme/repo"),
            new: format!("{slug}/acme/repo"),
        },
        MigratePathPair {
            old: format!("{legacy}/acme/repo__feature"),
            new: format!("{slug}/acme/repo__feature"),
        },
    ];

    let resp = migrate_paths(&db, f.node, &pairs).await.expect("migrate");
    assert_eq!(resp.node_workspaces_updated, 2, "both checkouts rewritten");
    assert_eq!(resp.tasks_updated, 1, "the task's worktree_path rewritten");

    // Same row ids, new paths — identity preserved, not delete-and-reinsert.
    assert_eq!(
        nw_path(&db, f.primary_ws).await.as_deref(),
        Some(format!("{slug}/acme/repo").as_str())
    );
    assert_eq!(
        nw_path(&db, f.worktree_ws).await.as_deref(),
        Some(format!("{slug}/acme/repo__feature").as_str())
    );
    assert_eq!(
        task_worktree(&db, f.task).await.as_deref(),
        Some(format!("{slug}/acme/repo").as_str()),
        "the task follows its worktree to the new path"
    );

    cleanup(&db, f.tenant).await;
}

#[tokio::test]
async fn refuses_a_path_that_does_not_belong_to_the_node() {
    use nook_control::error::ApiError;
    let Some(db) = test_pool().await else { return };
    let legacy = "/home/u/.nook/workspace";
    let slug = "/home/u/.nook/workspace/cp.example.com";
    let f = seed(&db, legacy, slug).await;

    // One real pair and one that names a path this node does not hold.
    let pairs = vec![
        MigratePathPair {
            old: format!("{legacy}/acme/repo"),
            new: format!("{slug}/acme/repo"),
        },
        MigratePathPair {
            old: "/some/other/nodes/checkout".into(),
            new: "/wherever".into(),
        },
    ];

    let err = migrate_paths(&db, f.node, &pairs)
        .await
        .expect_err("a stray path is refused");
    assert!(
        matches!(err, ApiError::BadRequest(_)),
        "an unowned path is a 400, got {err:?}"
    );

    // And nothing moved — the refusal is before any write, so the real pair's
    // row is untouched too.
    assert_eq!(
        nw_path(&db, f.primary_ws).await.as_deref(),
        Some(format!("{legacy}/acme/repo").as_str()),
        "a refused request leaves every row where it was"
    );

    cleanup(&db, f.tenant).await;
}

#[tokio::test]
async fn a_followup_discovery_of_the_new_paths_churns_nothing() {
    let Some(db) = test_pool().await else { return };
    let legacy = "/home/u/.nook/workspace";
    let slug = "/home/u/.nook/workspace/cp.example.com";
    let f = seed(&db, legacy, slug).await;

    let pairs = vec![
        MigratePathPair {
            old: format!("{legacy}/acme/repo"),
            new: format!("{slug}/acme/repo"),
        },
        MigratePathPair {
            old: format!("{legacy}/acme/repo__feature"),
            new: format!("{slug}/acme/repo__feature"),
        },
    ];
    migrate_paths(&db, f.node, &pairs).await.expect("migrate");

    // The state of this node's checkouts right after the rewrite.
    let before: Vec<(NodeWorkspaceId, String)> =
        sqlx::query_as("SELECT id, path FROM node_workspaces WHERE node_id = $1 ORDER BY path")
            .bind(f.node)
            .fetch_all(&db)
            .await
            .expect("before");
    assert_eq!(before.len(), 2);

    // Now the agent connects and discovery reports the NEW paths. Because the
    // rows already sit at those paths, the reconcile must upsert them in place
    // (same ids) and delete nothing — zero churn.
    let state = AppState::new(db.clone(), test_config(), None).await;
    let discovered = vec![
        DiscoveredWorkspace {
            path: format!("{slug}/acme/repo"),
            name: "acme/repo".into(),
            git_remote_url: Some(f.remote.clone()),
            branch: Some("main".into()),
            dirty: false,
            worktree: false,
        },
        DiscoveredWorkspace {
            path: format!("{slug}/acme/repo__feature"),
            name: "acme/repo".into(),
            git_remote_url: Some(f.remote.clone()),
            branch: Some("feature".into()),
            dirty: false,
            worktree: true,
        },
    ];
    discovery::reconcile(&state, f.tenant, f.node, discovered)
        .await
        .expect("reconcile");

    let after: Vec<(NodeWorkspaceId, String)> =
        sqlx::query_as("SELECT id, path FROM node_workspaces WHERE node_id = $1 ORDER BY path")
            .bind(f.node)
            .fetch_all(&db)
            .await
            .expect("after");

    assert_eq!(
        before, after,
        "a discovery of the new paths changes no rows"
    );
    // The workspace was already known; reconcile must not spawn a second one.
    let ws_count: (i64,) = sqlx::query_as("SELECT count(*) FROM workspaces WHERE id = $1")
        .bind(f.workspace)
        .fetch_one(&db)
        .await
        .expect("ws count");
    assert_eq!(ws_count.0, 1);

    cleanup(&db, f.tenant).await;
}

/// A throwaway config for `AppState::new`, matching the other suites'.
fn test_config() -> nook_control::config::Config {
    nook_control::config::Config {
        app_env: "test".into(),
        bind: "127.0.0.1:0".into(),
        shutdown_grace_secs: 25,
        agent_bind: "127.0.0.1:0".into(),
        agent_public_url: None,
        agent_tls_cert: None,
        agent_tls_key: None,
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
        default_tenant_name: "dev".into(),
        auth_dev_mode: true,
        mcp_token: None,
        dev_join_token: None,
        dist_dir: "/nonexistent".into(),
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
        redis_url: None,
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
