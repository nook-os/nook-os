//! Repository completeness (MAIN-223): a workspace knows its URL, clone-to-node
//! pins the checkout by id, MCP resolution is deterministic, and the MCP work
//! tools authorize as the person behind the call.
//!
//! Everything runs against a private `nook_testkit::TestBed`; only rows this test
//! creates are ever touched. Set `DATABASE_URL`.
//!
//! Note on scope: actual node placement (dispatch success, worktree creation)
//! needs a live node connection in the in-memory registry, which a unit test
//! cannot stand up. These tests exercise the part that was dead code — the
//! person-based authorization threaded through the real backend methods — and the
//! id-pinned association the clone flow performs on success.

use axum::extract::{Path, State};
use axum::Json;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::error::ApiError;
use nook_control::mcp_backend::McpBackend;
use nook_control::routes::workspaces::{associate_cloned_checkout, clone_to_node};
use nook_control::services::kanban::{KanbanProvider, LocalBoardProvider};
use nook_db::{params, Db};
use nook_mcp::{McpCaller, NookBackend};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

fn user_ctx(user: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: false,
    }
}

/// Insert a workspace with a chosen name/slug and no remote yet.
async fn workspace(bed: &TestBed, tenant: TenantId, name: &str, slug: &str) -> WorkspaceId {
    let id = WorkspaceId::new();
    bed.db()
        .exec(
            "INSERT INTO workspaces (id, tenant_id, name, slug) VALUES ($1, $2, $3, $4)",
            params![id, tenant, name, slug],
        )
        .await
        .expect("workspace");
    id
}

/// A checkout row carrying a raw remote URL — the shape discovery writes.
async fn checkout(
    bed: &TestBed,
    tenant: TenantId,
    node: NodeId,
    ws: WorkspaceId,
    path: &str,
    url: &str,
) {
    bed.db()
        .exec(
            "INSERT INTO node_workspaces (id, tenant_id, node_id, workspace_id, path, git_remote_url, kind)
         VALUES ($1, $2, $3, $4, $5, $6, 'clone')",
            params![NodeWorkspaceId::new(), tenant, node, ws, path, url],
        )
        .await
        .expect("checkout");
}

async fn remote_of(bed: &TestBed, ws: WorkspaceId) -> Option<String> {
    // ONE row, nullable column — `query_one` so a missing workspace panics
    // rather than reading as "no remote".
    bed.db()
        .query_scalar::<Option<String>>(
            "SELECT git_remote_url FROM workspaces WHERE id = $1",
            params![ws],
        )
        .await
        .expect("read git_remote_url")
}

// ── AC-1: backfill ───────────────────────────────────────────────────────────

#[tokio::test]
async fn backfill_adopts_agreeing_remotes_and_leaves_disagreeing_null() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("rc").await;
    let (_u, person) = bed.user(tenant, "member").await;
    let node = bed.node(tenant, person).await;

    // Agreeing: two checkouts, one URL → adopt it.
    let agree = workspace(&bed, tenant, "agree", "agree").await;
    checkout(
        &bed,
        tenant,
        node,
        agree,
        "/a1",
        "git@github.com:acme/a.git",
    )
    .await;
    checkout(
        &bed,
        tenant,
        node,
        agree,
        "/a2",
        "git@github.com:acme/a.git",
    )
    .await;

    // Disagreeing: two checkouts, two URLs → leave NULL.
    let clash = workspace(&bed, tenant, "clash", "clash").await;
    checkout(
        &bed,
        tenant,
        node,
        clash,
        "/c1",
        "git@github.com:acme/one.git",
    )
    .await;
    checkout(
        &bed,
        tenant,
        node,
        clash,
        "/c2",
        "git@github.com:acme/two.git",
    )
    .await;

    // The migration's backfill statement, with `AS w` rather than a bare `w`:
    // SQLite's UPDATE grammar requires the keyword and Postgres accepts it
    // either way, so one spelling reads on both engines (MAIN-472).
    bed.db()
        .exec(
            "WITH agreed AS (
             SELECT workspace_id, min(git_remote_url) AS url
             FROM node_workspaces
             WHERE git_remote_url IS NOT NULL
             GROUP BY workspace_id
             HAVING count(DISTINCT git_remote_url) = 1
         )
         UPDATE workspaces AS w
         SET git_remote_url = a.url
         FROM agreed a
         WHERE w.id = a.workspace_id AND w.git_remote_url IS NULL",
            params![],
        )
        .await
        .expect("backfill");

    assert_eq!(
        remote_of(&bed, agree).await.as_deref(),
        Some("git@github.com:acme/a.git"),
        "agreeing checkouts backfill the workspace remote"
    );
    assert_eq!(
        remote_of(&bed, clash).await,
        None,
        "disagreeing checkouts leave the remote NULL"
    );

    bed.teardown().await;
}

// ── AC-2: clone associates by id, not by remote re-derivation ────────────────

#[tokio::test]
async fn clone_association_pins_the_workspace_by_id_not_the_remote() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("rc").await;
    let (_u, person) = bed.user(tenant, "member").await;
    let node = bed.node(tenant, person).await;

    let url = "git@github.com:acme/shared.git";
    let normalized = nook_control::services::discovery::normalize_remote(url);

    // A DECOY workspace already owns this remote's normalized form — the thing
    // discovery would match on. The clone must NOT associate to it.
    let decoy = workspace(&bed, tenant, "decoy", "decoy").await;
    bed.db()
        .exec(
            "UPDATE workspaces SET git_remote_normalized = $2 WHERE id = $1",
            params![decoy, normalized.clone()],
        )
        .await
        .expect("seed decoy remote");

    let target = workspace(&bed, tenant, "target", "target").await;
    associate_cloned_checkout(&state, tenant, node, target, "/srv/target", url)
        .await
        .expect("associate");

    let owner: WorkspaceId = bed
        .db()
        .query_scalar(
            "SELECT workspace_id FROM node_workspaces WHERE node_id = $1 AND path = $2",
            params![node, "/srv/target"],
        )
        .await
        .expect("the associated checkout");
    assert_eq!(
        owner, target,
        "the checkout is pinned to the target id, never re-derived onto the remote's decoy owner"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn clone_to_node_rejects_a_workspace_without_a_stored_url() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("rc").await;
    let (user, person) = bed.user(tenant, "member").await;
    let node = bed.node(tenant, person).await;
    let ws = workspace(&bed, tenant, "no-url", "no-url").await;

    let err = clone_to_node(
        State(state),
        user_ctx(user, tenant),
        Path(ws),
        Json(WorkspaceCloneRequest {
            node_id: node,
            credential_id: None,
        }),
    )
    .await
    .expect_err("a workspace with no stored URL cannot be cloned");
    assert!(
        matches!(err, ApiError::BadRequest(ref m) if m.contains("no stored git remote URL")),
        "pointed no-URL error, got {err:?}"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn clone_to_node_refuses_a_node_the_caller_cannot_use() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("rc").await;
    let (user, _person) = bed.user(tenant, "member").await;
    // Node owned by someone else, not shared → the caller may not use it.
    let stranger = Uuid::now_v7();
    let node = bed.node(tenant, stranger).await;
    let ws = workspace(&bed, tenant, "has-url", "has-url").await;
    bed.db()
        .exec(
            "UPDATE workspaces SET git_remote_url = $2 WHERE id = $1",
            params![ws, "git@github.com:acme/x.git"],
        )
        .await
        .expect("set url");

    let err = clone_to_node(
        State(state),
        user_ctx(user, tenant),
        Path(ws),
        Json(WorkspaceCloneRequest {
            node_id: node,
            credential_id: None,
        }),
    )
    .await
    .expect_err("cloning onto an unusable node is refused");
    assert!(
        matches!(err, ApiError::ForbiddenMsg(_)),
        "own/shared-node rule refuses it, got {err:?}"
    );

    bed.teardown().await;
}

// ── AC-3: deterministic MCP workspace resolution ─────────────────────────────

#[tokio::test]
async fn resolve_workspace_by_id_slug_and_ambiguous_name() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("rc").await;
    let backend = McpBackend {
        state: bed.app_state().await,
    };

    let solo = workspace(&bed, tenant, "solo", "solo-slug").await;
    // Two workspaces share a NAME but have distinct (unique) slugs.
    let dup_a = workspace(&bed, tenant, "dup", "dup-a").await;
    let _dup_b = workspace(&bed, tenant, "dup", "dup-b").await;

    // By id (unique).
    assert_eq!(
        backend
            .resolve_workspace(tenant, &solo.0.to_string())
            .await
            .unwrap(),
        solo
    );
    // By slug (unique) — even when the name is ambiguous.
    assert_eq!(
        backend.resolve_workspace(tenant, "dup-a").await.unwrap(),
        dup_a
    );
    // By a unique name.
    assert_eq!(
        backend.resolve_workspace(tenant, "solo").await.unwrap(),
        solo
    );

    // A bare ambiguous name errors, naming the slugs, instead of picking one.
    let err = backend
        .resolve_workspace(tenant, "dup")
        .await
        .expect_err("ambiguous name must not resolve arbitrarily");
    let msg = err.to_string();
    assert!(
        msg.contains("dup-a") && msg.contains("dup-b"),
        "names the slugs: {msg}"
    );

    // Nothing at all.
    assert!(backend.resolve_workspace(tenant, "nope").await.is_err());

    bed.teardown().await;
}

// ── AC-4: the work tools authorize as the caller ─────────────────────────────

/// A board with one `unstarted` column, plus a task on it.
async fn board_with_task(tenant: TenantId, bed: &TestBed) -> (BoardId, TaskId) {
    let board = BoardId::new();
    bed.db()
        .exec(
            "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b',$3,'local')",
            params![
                board,
                tenant,
                format!("B{}", &board.0.simple().to_string()[..6]).to_uppercase()
            ],
        )
        .await
        .expect("board");
    bed.db()
        .exec(
            "INSERT INTO board_columns (id, board_id, name, position, type)
         VALUES ($1,$2,'Todo',0,'unstarted')",
            params![Uuid::now_v7(), board],
        )
        .await
        .expect("column");
    let provider = LocalBoardProvider {
        repo: std::sync::Arc::new(nook_control::repo::tasks::DbTaskRepository::new(bed.db())),
    };
    let task = provider
        .create_task(
            tenant,
            board,
            None,
            CreateTaskRequest {
                title: "t".into(),
                description: None,
                column_id: None,
                column_type: None,
                workspace_id: None,
                priority: None,
                type_: None,
                visibility: None,
                parent: None,
                labels: vec![],
            },
        )
        .await
        .expect("task");
    (board, task.id)
}

#[tokio::test]
async fn dispatch_task_threads_the_caller_and_reaches_placement() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("rc").await;
    let (user, person) = bed.user(tenant, "member").await;
    let (_board, task) = board_with_task(tenant, &bed).await;
    let backend = McpBackend {
        state: bed.app_state().await,
    };
    let caller = McpCaller {
        person_id: person,
        user_id: user,
        tenant_id: tenant,
    };

    // The caller owns no ONLINE node (the registry has no live connection in a
    // test), so placement reaches the scheduler and returns its no-eligible-node
    // error — proving the caller's person was threaded through, not the old
    // silent `None`. The task was visible to the caller (viewer threaded), or it
    // would have 404'd first.
    let err = backend
        .dispatch_task(caller, task.0.to_string())
        .await
        .expect_err("no online owned node → no eligible node");
    let msg = err.to_string();
    assert!(
        msg.contains("eligible") || msg.contains("node"),
        "reached placement with the caller identity, got: {msg}"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn start_work_refuses_a_node_the_caller_cannot_use() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("rc").await;
    let (user, person) = bed.user(tenant, "member").await;
    let (_board, task) = board_with_task(tenant, &bed).await;
    // Pin the task to a node owned by a stranger. Passing `node = None` routes
    // start-work through `task.assigned_node_id`, so spawn authorization runs
    // against that node without needing it ONLINE (resolve-by-name would demand a
    // live registry connection a test cannot stand up).
    let stranger = Uuid::now_v7();
    let node = bed.node(tenant, stranger).await;
    bed.db()
        .exec(
            "UPDATE tasks SET assigned_node_id = $2 WHERE id = $1",
            params![task, node],
        )
        .await
        .expect("pin the task to the stranger's node");
    let backend = McpBackend {
        state: bed.app_state().await,
    };
    let caller = McpCaller {
        person_id: person,
        user_id: user,
        tenant_id: tenant,
    };

    // The caller's identity is threaded into spawn authorization: an unowned,
    // unshared node is refused before any worktree op — exactly the HTTP rule.
    let err = backend
        .start_work(caller, task.0.to_string(), None, None)
        .await
        .expect_err("start-work on an unusable node is refused");
    let msg = err.to_string();
    assert!(
        msg.contains("own") || msg.contains("shared"),
        "own/shared-node rule refuses it, got: {msg}"
    );

    bed.teardown().await;
}
