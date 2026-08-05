//! Placement resolves to a real clone-checkout host or an explicit needs-clone
//! (MAIN-227 AC-1/AC-2/AC-3), and the checkout-mutating gitops routes require the
//! person chokepoint the way start-work does (AC-4/AC-5).
//!
//! `pick`/`dispatch` need a live node — the registry's online filter — which the
//! `online_node` helper stands up by registering a node handle (the seam
//! `dispatch_ownership` uses). Set `DATABASE_URL`.

use axum::extract::{Path, State};
use axum::Json;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::error::{ApiError, ApiResult};
use nook_control::routes::gitops;
use nook_control::services::schedule::{clone_hosts, pick, Placement};
use nook_control::state::AppState;
use nook_control::ws::registry::NodeHandle;
use nook_db::{params, Db};
use nook_proto::ControlToNode;
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

/// An owned node, registered ONLINE in the registry so `pick` treats it as a live
/// candidate (the seam `dispatch_ownership` uses).
async fn online_node(state: &AppState, tenant: TenantId, owner: Uuid) -> NodeId {
    let id = NodeId::new();
    state
        .db
        .exec(
            "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status, owner_person_id, resources)
         VALUES ($1, $2, $3, $4, 'online', $5, '{\"mem_total\": 32}'::jsonb)",
            params![
                id,
                tenant,
                format!("n-{}", id.0.simple()),
                format!("h-{}", id.0.simple()),
                owner
            ],
        )
        .await
        .expect("node");
    let (tx, _rx) = tokio::sync::mpsc::channel::<ControlToNode>(4);
    state.registry.register_node(
        id,
        NodeHandle {
            tenant_id: tenant,
            tx,
        },
    );
    id
}

/// A board with an `unstarted` (Todo) column and a task on it, in `ws`, visible
/// to the tenant — the fixture `dispatch` needs.
async fn board_task(bed: &TestBed, tenant: TenantId, ws: WorkspaceId, creator: UserId) -> TaskId {
    let board = BoardId::new();
    bed.db()
        .exec(
            "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b',$3,'local')",
            // The random tail, not the shared v7 timestamp prefix, so two boards
            // created milliseconds apart don't collide on the unique (tenant, key).
            params![
                board,
                tenant,
                format!("B{}", &board.0.simple().to_string()[26..]).to_uppercase()
            ],
        )
        .await
        .expect("board");
    let col = ColumnId::new();
    bed.db()
        .exec(
            "INSERT INTO board_columns (id, board_id, name, position, type) VALUES ($1,$2,'Todo',0,'unstarted')",
            params![col, board],
        )
        .await
        .expect("column");
    let task = TaskId::new();
    bed.db()
        .exec(
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, position, workspace_id, visibility, created_by)
         VALUES ($1, $2, $3, $4, 't', 0, $5, 'team', $6)",
            params![task, tenant, board, col, ws, creator],
        )
        .await
        .expect("task");
    task
}

async fn checkout(
    bed: &TestBed,
    tenant: TenantId,
    node: NodeId,
    ws: WorkspaceId,
    path: &str,
    kind: &str,
    missing: bool,
) -> NodeWorkspaceId {
    let id = NodeWorkspaceId::new();
    bed.db()
        .exec(
            "INSERT INTO node_workspaces (id, tenant_id, node_id, workspace_id, path, kind, missing_at)
         VALUES ($1, $2, $3, $4, $5, $6, CASE WHEN $7 THEN now() ELSE NULL END)",
            params![id, tenant, node, ws, path, kind, missing],
        )
    .await
    .expect("checkout");
    id
}

fn ctx(user: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: false,
    }
}

// ── AC-2: only a present clone makes a node a host ───────────────────────────

#[tokio::test]
async fn clone_hosts_counts_only_present_clones() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("pl").await;
    let (_u, person) = bed.user(tenant, "member").await;
    let node = bed.node(tenant, person).await;
    let other = bed.node(tenant, person).await;
    let ws = bed.workspace(tenant).await;

    // A present clone on `node` — a host, pinned to its id.
    let clone = checkout(&bed, tenant, node, ws, "/srv/clone", "clone", false).await;
    // A worktree on the same node — NOT a host on its own.
    checkout(&bed, tenant, node, ws, "/srv/wt", "worktree", false).await;
    // A tombstoned clone on `other` — NOT a host.
    checkout(&bed, tenant, other, ws, "/srv/gone", "clone", true).await;

    let repo = nook_control::repo::workspaces::DbWorkspaceRepository::new(bed.db());
    let hosts = clone_hosts(&repo, tenant, ws).await.unwrap();
    assert_eq!(
        hosts.get(&node),
        Some(&clone),
        "the present clone is the host"
    );
    assert_eq!(
        hosts.len(),
        1,
        "worktree and tombstoned clone are not hosts"
    );
    assert!(
        !hosts.contains_key(&other),
        "a node with only a tombstoned clone is not a host"
    );

    bed.teardown().await;
}

// ── AC-1 / AC-3: pick + dispatch outcomes on a live (registered) node ────────

#[tokio::test]
async fn pick_places_on_a_clone_host_and_needs_clone_otherwise() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("pl").await;
    let (user, person) = bed.user(tenant, "member").await;
    let node = online_node(&state, tenant, person).await;

    // A workspace with a CLONE checkout on the node → Placed, pinned to its id.
    let ws_clone = bed.workspace(tenant).await;
    let clone = checkout(&bed, tenant, node, ws_clone, "/srv/clone", "clone", false).await;
    match pick(&state, tenant, Some(user), Some(ws_clone))
        .await
        .unwrap()
    {
        Placement::Placed {
            node_id,
            checkout_id,
        } => {
            assert_eq!(node_id, node);
            assert_eq!(
                checkout_id,
                Some(clone),
                "pinned to the clone's checkout id"
            );
        }
        other => panic!("expected Placed, got {other:?}"),
    }

    // A workspace present only as a WORKTREE on the node → NeedsClone (AC-2).
    let ws_wt = bed.workspace(tenant).await;
    checkout(&bed, tenant, node, ws_wt, "/srv/wt", "worktree", false).await;
    match pick(&state, tenant, Some(user), Some(ws_wt)).await.unwrap() {
        Placement::NeedsClone { node_id } => assert_eq!(node_id, node),
        other => panic!("expected NeedsClone, got {other:?}"),
    }

    bed.teardown().await;
}

#[tokio::test]
async fn dispatch_flags_needs_clone_only_without_a_clone_host() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("pl").await;
    let (user, person) = bed.user(tenant, "member").await;
    let node = online_node(&state, tenant, person).await;

    // Hosted as a clone → placed, no needs-clone flag.
    let ws_clone = bed.workspace(tenant).await;
    checkout(&bed, tenant, node, ws_clone, "/srv/clone", "clone", false).await;
    let task_a = board_task(&bed, tenant, ws_clone, user).await;
    let a = nook_control::services::taskwork::dispatch(&state, tenant, user, Some(user), task_a)
        .await
        .unwrap();
    assert!(!a.needs_clone, "a clone host places cleanly");
    assert_eq!(a.assigned_node_id, Some(node));

    // Hosted only as a worktree → node still assigned, but needs_clone surfaces.
    let ws_wt = bed.workspace(tenant).await;
    checkout(&bed, tenant, node, ws_wt, "/srv/wt", "worktree", false).await;
    let task_b = board_task(&bed, tenant, ws_wt, user).await;
    let b = nook_control::services::taskwork::dispatch(&state, tenant, user, Some(user), task_b)
        .await
        .unwrap();
    assert!(
        b.needs_clone,
        "no clone host → needs_clone surfaced at dispatch time"
    );
    assert_eq!(
        b.assigned_node_id,
        Some(node),
        "the chosen node is still assigned (NG-2)"
    );

    bed.teardown().await;
}

// ── AC-4 / AC-5: every mutating route gates on person-may-use-node ───────────

fn is_forbidden(r: &ApiResult<Json<OpResponse>>) -> bool {
    matches!(r, Err(ApiError::ForbiddenMsg(_)))
}

#[tokio::test]
async fn mutating_gitops_routes_require_person_may_use_node() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("pl").await;
    // The node's owner, and an unrelated member who neither owns nor shares it.
    let (owner, owner_person) = bed.user(tenant, "member").await;
    let (stranger, _p) = bed.user(tenant, "member").await;
    let node = bed.node(tenant, owner_person).await;
    let ws = bed.workspace(tenant).await;

    // Run each route with the given identity and return its result.
    macro_rules! run {
        ($name:literal, $call:expr) => {{
            let r: ApiResult<Json<OpResponse>> = $call;
            (($name), r)
        }};
    }

    for (who, user, expect_forbidden) in [("owner", owner, false), ("stranger", stranger, true)] {
        let cases = vec![
            run!(
                "clone_repo",
                gitops::clone_repo(
                    State(state.clone()),
                    ctx(user, tenant),
                    Path(node),
                    Json(CloneRequest {
                        url: "git@github.com:acme/x.git".into(),
                        name: None,
                        credential_id: None,
                        background: false,
                    }),
                )
                .await
            ),
            run!(
                "add_worktree",
                gitops::add_worktree(
                    State(state.clone()),
                    ctx(user, tenant),
                    Path(ws),
                    Json(WorktreeRequest {
                        node_id: node,
                        branch: "b".into()
                    }),
                )
                .await
            ),
            run!(
                "git_commit",
                gitops::git_commit(
                    State(state.clone()),
                    ctx(user, tenant),
                    Path(ws),
                    Json(GitCommitRequest {
                        node_id: node,
                        message: "m".into(),
                        paths: None
                    }),
                )
                .await
            ),
            run!(
                "git_push",
                gitops::git_push(
                    State(state.clone()),
                    ctx(user, tenant),
                    Path(ws),
                    Json(GitPushRequest {
                        node_id: node,
                        credential_id: None
                    }),
                )
                .await
            ),
            run!(
                "remove_worktree",
                gitops::remove_worktree(
                    State(state.clone()),
                    ctx(user, tenant),
                    Path(ws),
                    Json(RemoveWorktreeRequest {
                        node_id: node,
                        path: "/p".into()
                    }),
                )
                .await
            ),
            run!(
                "init_project",
                gitops::init_project(
                    State(state.clone()),
                    ctx(user, tenant),
                    Path(node),
                    Json(InitProjectRequest {
                        name: "proj".into()
                    }),
                )
                .await
            ),
        ];

        for (route, r) in cases {
            if expect_forbidden {
                assert!(
                    is_forbidden(&r),
                    "{who} on {route}: expected 403 (ForbiddenMsg)"
                );
            } else {
                // The authorized owner clears the gate — the error it then hits is
                // the offline-node 400, never a 403 (AC-5: 403 distinct from 400).
                assert!(
                    !is_forbidden(&r),
                    "{who} on {route}: authorized person wrongly 403'd"
                );
            }
        }
    }

    bed.teardown().await;
}

/// Dispatch places a card on a machine. It does NOT move it.
///
/// The column move was hardcoded to Todo and unconditional, so a mis-click on a
/// card sitting In Review yanked an urgent ticket back into the queue. Where a
/// card sits is the human's statement about progress; which machine should take
/// it is a different question.
#[tokio::test]
async fn dispatch_assigns_a_node_without_moving_the_card() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("disp").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let _node = online_node(&state, tenant, person).await;
    let ws = bed.workspace(tenant).await;
    let task = board_task(&bed, tenant, ws, user).await;
    let before = state
        .tasks
        .get_row(tenant, task)
        .await
        .expect("row")
        .expect("task")
        .column_id;

    let updated =
        nook_control::services::taskwork::dispatch(&state, tenant, user, Some(user), task)
            .await
            .expect("dispatch");

    assert!(
        updated.assigned_node_id.is_some(),
        "dispatch must still place the card on a machine"
    );
    assert_eq!(
        updated.column_id, before,
        "dispatch moved the card — that is what cost an urgent ticket its column"
    );

    bed.teardown().await;
}
