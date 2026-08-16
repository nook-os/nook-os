//! MAIN-592: the MCP surface acts for the CALLER, never for the instance.
//!
//! Every tenant-scoped tool used to read its tenant from `first_tenant` and its
//! actor from that tenant's owner, so any authenticated caller was served the
//! first tenant's board and every MCP write was attributed to a person who had
//! not made it. Driven at the `NookBackend` layer with a resolved `McpCaller` —
//! the call the tools make once `mcp_auth` has resolved the caller — so the
//! scoping under test is the real one.
//!
//! Two full tenants are built and the caller is resolved to the SECOND, because
//! the defect served the first: a one-tenant test would have passed throughout.
//!
//! Needs Postgres: set `DATABASE_URL` (`NOOK_REQUIRE_DB=1` in the suite).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use nook_control::mcp_backend::McpBackend;
use nook_control::repo::tasks::DbTaskRepository;
use nook_control::services::kanban::{KanbanProvider, LocalBoardProvider};
use nook_control::services::notebook_queries;
use nook_control::state::AppState;
use nook_control::ws::registry::NodeHandle;
use nook_db::{params, Db};
use nook_mcp::{BuildRunQuery, McpCaller, NookBackend, TaskQuery};
use nook_proto::ControlToNode;
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

/// One tenant with everything a tenant-scoped tool can be pointed at.
struct Org {
    id: TenantId,
    caller: McpCaller,
    node: NodeId,
    node_name: String,
    workspace: WorkspaceId,
    workspace_slug: String,
    session: SessionId,
    board: BoardId,
    task: TaskId,
    task_key: String,
    attachment: Uuid,
}

const NOTE: &str = "the rolling note nobody else may read";

async fn org(bed: &TestBed, state: &AppState, hint: &str) -> Org {
    let id = bed.tenant(hint).await;
    let (user, person) = bed.user(id, "owner").await;
    state
        .identity
        .grant_membership(id, user, "owner")
        .await
        .expect("membership");
    let node = bed.node(id, person).await;
    // Online, because `resolve_node` only ever sees online nodes: with an idle
    // registry every probe that names a node would fail for both tenants alike
    // and prove nothing.
    let (tx, _rx) = tokio::sync::mpsc::channel::<ControlToNode>(4);
    state
        .registry
        .register_node(node, NodeHandle { tenant_id: id, tx });
    let node_name = state
        .nodes
        .get(id, node)
        .await
        .expect("node")
        .expect("node row")
        .name;

    let workspace = WorkspaceId::new();
    let workspace_slug = format!("ws-{hint}-{}", workspace.0.simple());
    bed.db()
        .exec(
            "INSERT INTO workspaces (id, tenant_id, name, slug) VALUES ($1, $2, $3, $3)",
            params![workspace, id, workspace_slug.clone()],
        )
        .await
        .expect("workspace");
    notebook_queries::create_note(
        &*state.notebook,
        id,
        workspace,
        CreateNoteRequest {
            title: None,
            content_md: NOTE.into(),
            kind: Some("rolling".into()),
        },
    )
    .await
    .expect("workspace note");

    let session = SessionId::new();
    bed.db()
        .exec(
            "INSERT INTO sessions (id, tenant_id, workspace_id, node_id, name, runtime, status)
             VALUES ($1, $2, $3, $4, 'dev', 'bash', 'running')",
            params![session, id, workspace, node],
        )
        .await
        .expect("session");

    let board = BoardId::new();
    // From the TAIL of the uuid, not its head: `BoardId::new` is a v7, whose
    // leading hex is a millisecond clock — two boards made in one test share
    // their first six characters, which would give both orgs the same card key
    // and quietly turn every cross-tenant probe below into a same-tenant one.
    let simple = board.0.simple().to_string();
    let board_key = format!("B{}", &simple[simple.len() - 6..]).to_uppercase();
    bed.db()
        .exec(
            "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b',$3,'local')",
            params![board, id, board_key.clone()],
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
    let task = provider(bed)
        .create_task(id, board, Some(user), new_task(hint))
        .await
        .expect("task");
    let (number,): (i32,) = bed
        .db()
        .query_one("SELECT number FROM tasks WHERE id = $1", params![task.id])
        .await
        .expect("the card's number");

    // A file on that card, inserted directly: the probe only needs the RECORD
    // to exist in tenant A, and standing up a content store would test the
    // upload path instead of the scoping.
    let content = Uuid::now_v7();
    let attachment = Uuid::now_v7();
    bed.db()
        .exec(
            "INSERT INTO user_content
               (id, tenant_id, uploaded_by, filename, content_type, size_bytes, sha256, storage_key)
             VALUES ($1, $2, $3, 'logs.zip', 'application/zip', 5, 'x', $4)",
            params![content, id, user, content.to_string()],
        )
        .await
        .expect("content");
    bed.db()
        .exec(
            "INSERT INTO task_attachments
               (id, tenant_id, user_content_id, parent_kind, parent_id, attached_by)
             VALUES ($1, $2, $3, 'task', $4, $5)",
            params![attachment, id, content, task.id, user],
        )
        .await
        .expect("attachment");

    Org {
        id,
        caller: McpCaller {
            person_id: person,
            user_id: user,
            tenant_id: id,
        },
        node,
        node_name,
        workspace,
        workspace_slug,
        session,
        board,
        task: task.id,
        task_key: format!("{board_key}-{number}"),
        attachment,
    }
}

fn provider(bed: &TestBed) -> LocalBoardProvider {
    LocalBoardProvider {
        repo: Arc::new(DbTaskRepository::new(bed.db())),
    }
}

fn new_task(title: &str) -> CreateTaskRequest {
    CreateTaskRequest {
        title: title.into(),
        description: None,
        column_id: None,
        column_type: None,
        workspace_id: None,
        priority: None,
        type_: None,
        visibility: None,
        parent: None,
        labels: vec![],
    }
}

/// AC-6: a caller resolved to tenant B is served its OWN tenant's rows and none
/// of tenant A's, on every tool that lists.
#[tokio::test]
async fn a_caller_lists_only_its_own_tenants_rows() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    // A tunnel zone, because `list_tunnels` refuses a deployment without one
    // before it ever looks at the caller — and "no tunnels" has to mean the
    // caller has none, not that the surface is off.
    let mut cfg = nook_infra::Config::for_test();
    cfg.tunnel_domain = Some("tunnels.test".into());
    let state = AppState::new(bed.db(), cfg, None).await;
    let a = org(&bed, &state, "orga").await;
    let b = org(&bed, &state, "orgb").await;
    let mcp = McpBackend {
        state: state.clone(),
    };

    let workspaces = mcp
        .list_workspaces(b.caller.clone())
        .await
        .expect("list workspaces");
    assert!(
        workspaces.iter().any(|w| w.workspace.id == b.workspace)
            && !workspaces.iter().any(|w| w.workspace.id == a.workspace),
        "own workspace yes, the other tenant's no: {:?}",
        workspaces
            .iter()
            .map(|w| w.workspace.id)
            .collect::<Vec<_>>()
    );

    let nodes = mcp.list_nodes(b.caller.clone()).await.expect("list nodes");
    assert!(
        nodes.iter().any(|n| n.id == b.node) && !nodes.iter().any(|n| n.id == a.node),
        "the caller's fleet only"
    );

    let sessions = mcp
        .list_sessions(b.caller.clone(), false)
        .await
        .expect("list sessions");
    assert!(
        sessions.iter().any(|s| s.id == b.session) && !sessions.iter().any(|s| s.id == a.session),
        "the caller's sessions only"
    );

    let tasks = mcp
        .list_tasks(b.caller.clone(), TaskQuery::default())
        .await
        .expect("list tasks");
    assert!(
        tasks.iter().any(|t| t.id == b.task) && !tasks.iter().any(|t| t.id == a.task),
        "the caller's board only"
    );

    let events = mcp
        .get_activity(b.caller.clone(), None, 200)
        .await
        .expect("activity");
    assert!(
        !events.iter().any(|e| e.tenant_id == a.id),
        "the other tenant's activity never appears"
    );

    assert!(
        mcp.list_tunnels(b.caller.clone())
            .await
            .expect("tunnels")
            .is_empty(),
        "no tunnels, and certainly not another tenant's"
    );

    bed.teardown().await;
}

/// AC-3 and AC-6: every tenant-scoped tool, called by tenant B and pointed at
/// tenant A's own workspace / node / session / card / attachment, answers
/// not-found rather than serving it — and never a refusal, which would turn the
/// tool into an existence oracle for another tenant's names.
///
/// Table-driven so a tool added later that forgets the caller fails here rather
/// than passing silently: the list is the tool surface, and every entry names
/// something that exists in the OTHER tenant.
#[tokio::test]
async fn every_tenant_scoped_tool_refuses_another_tenants_names() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let a = org(&bed, &state, "namea").await;
    let b = org(&bed, &state, "nameb").await;
    let mcp = McpBackend {
        state: state.clone(),
    };
    assert_ne!(
        a.task_key, b.task_key,
        "the two orgs must have distinct card keys, or every probe below is a \
         same-tenant call that passes for the wrong reason"
    );
    let c = || b.caller.clone();
    let ws = || a.workspace_slug.clone();
    let key = || a.task_key.clone();
    let sid = || a.session.to_string();
    let tid = || a.task.to_string();

    type Probe<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + 'a>>;
    let probes: Vec<(&str, Probe)> = vec![
        (
            "start_session",
            Box::pin(async {
                mcp.start_session(c(), ws(), None, "bash".into())
                    .await
                    .map(drop)
            }),
        ),
        (
            "send_to_session",
            Box::pin(async { mcp.send_to_session(c(), sid(), "hi".into()).await }),
        ),
        (
            "read_session",
            Box::pin(async { mcp.read_session(c(), sid(), 10).await.map(drop) }),
        ),
        (
            "kill_session",
            Box::pin(async { mcp.kill_session(c(), sid()).await }),
        ),
        (
            "get_activity",
            Box::pin(async { mcp.get_activity(c(), Some(ws()), 10).await.map(drop) }),
        ),
        (
            "get_notes",
            Box::pin(async { mcp.get_notes(c(), ws()).await.map(drop) }),
        ),
        (
            "append_note",
            Box::pin(async { mcp.append_note(c(), ws(), "x".into()).await.map(drop) }),
        ),
        (
            "add_worktree",
            Box::pin(async {
                mcp.add_worktree(c(), ws(), "br".into(), None)
                    .await
                    .map(drop)
            }),
        ),
        (
            "clone_repo",
            Box::pin(async {
                mcp.clone_repo(c(), "https://x/y.git".into(), Some(a.node_name.clone()))
                    .await
                    .map(drop)
            }),
        ),
        (
            "create_project",
            Box::pin(async {
                mcp.create_project(c(), "p".into(), None, Some(a.node_name.clone()))
                    .await
                    .map(drop)
            }),
        ),
        (
            "dispatch_task",
            Box::pin(async { mcp.dispatch_task(c(), tid()).await.map(drop) }),
        ),
        (
            "start_work",
            Box::pin(async { mcp.start_work(c(), tid(), None, None).await.map(drop) }),
        ),
        (
            "move_task",
            Box::pin(async { mcp.move_task(c(), tid(), "Todo".into()).await.map(drop) }),
        ),
        (
            "submit_pr",
            Box::pin(async { mcp.submit_pr(c(), tid(), None).await.map(drop) }),
        ),
        (
            "get_task",
            Box::pin(async { mcp.get_task(c(), key()).await.map(drop) }),
        ),
        (
            "set_task_description",
            Box::pin(async {
                mcp.set_task_description(c(), key(), "d".into())
                    .await
                    .map(drop)
            }),
        ),
        (
            "claim_task",
            Box::pin(async { mcp.claim_task(c(), key(), None).await.map(drop) }),
        ),
        (
            "release_task",
            Box::pin(async { mcp.release_task(c(), key()).await.map(drop) }),
        ),
        (
            "comment_task",
            Box::pin(async {
                mcp.comment_task(c(), key(), "x".into(), None, false)
                    .await
                    .map(drop)
            }),
        ),
        (
            "add_label",
            Box::pin(async { mcp.add_label(c(), key(), "blocked".into()).await.map(drop) }),
        ),
        (
            "remove_label",
            Box::pin(async {
                mcp.remove_label(c(), key(), "blocked".into())
                    .await
                    .map(drop)
            }),
        ),
        (
            "set_priority",
            Box::pin(async { mcp.set_priority(c(), key(), 1).await.map(drop) }),
        ),
        (
            "set_task_parent",
            Box::pin(async { mcp.set_task_parent(c(), key(), None).await.map(drop) }),
        ),
        (
            "link_tasks",
            Box::pin(async {
                mcp.link_tasks(c(), key(), key(), "relates".into())
                    .await
                    .map(drop)
            }),
        ),
        (
            "list_task_attachments",
            Box::pin(async { mcp.list_task_attachments(c(), key()).await.map(drop) }),
        ),
        (
            "read_task_attachment",
            Box::pin(async {
                mcp.read_task_attachment(c(), a.attachment.to_string())
                    .await
                    .map(drop)
            }),
        ),
        (
            "list_build_runs",
            Box::pin(async {
                mcp.list_build_runs(
                    c(),
                    BuildRunQuery {
                        workspace: ws(),
                        live_only: false,
                        kind: None,
                        limit: None,
                    },
                )
                .await
                .map(drop)
            }),
        ),
        (
            "get_build_run",
            Box::pin(async { mcp.get_build_run(c(), key(), 10).await.map(drop) }),
        ),
        (
            "open_tunnel",
            Box::pin(async { mcp.open_tunnel(c(), sid(), 5173).await.map(drop) }),
        ),
    ];

    for (tool, probe) in probes {
        let err = probe
            .await
            .err()
            .unwrap_or_else(|| panic!("{tool} served another tenant's row"));
        assert!(
            !forbidden(&err),
            "{tool} refused rather than answering not-found, which tells the \
             caller the name exists in some other tenant: {err}"
        );
    }

    // The controls. Every refusal above has to be the CALLER's doing, so the
    // same calls are made by the tenant that owns those names and must succeed —
    // otherwise a probe that fails for everyone would read as isolation.
    let own = a.caller.clone();
    mcp.get_task(own.clone(), a.task_key.clone())
        .await
        .expect("its own tenant reads its own card");
    let notes = mcp
        .get_notes(own.clone(), a.workspace_slug.clone())
        .await
        .expect("its own tenant reads its own workspace notes");
    assert!(notes.iter().any(|n| n.content_md == NOTE));
    assert_eq!(
        mcp.list_task_attachments(own.clone(), a.task_key.clone())
            .await
            .expect("its own tenant lists its own files")
            .len(),
        1
    );
    mcp.read_task_attachment(own.clone(), a.attachment.to_string())
        .await
        .expect("its own tenant reads its own file");
    mcp.clone_repo(own, "https://x/y.git".into(), Some(a.node_name.clone()))
        .await
        .expect_err("no repo is really cloned here — but the node RESOLVED");

    // The two tools that name nothing of tenant A's still cannot reach it: they
    // write, and what they write lands in the caller's own tenant.
    let made = mcp
        .create_task(b.caller.clone(), "mine".into(), None, None)
        .await
        .expect("a card in the caller's own tenant");
    let (owner,): (Uuid,) = bed
        .db()
        .query_one(
            "SELECT tenant_id FROM tasks WHERE id = $1",
            params![made.id],
        )
        .await
        .expect("the new card's tenant");
    assert_eq!(owner, b.id.0, "create_task files in the CALLER's tenant");

    bed.teardown().await;
}

/// A refusal, as opposed to a not-found. `Unauthorized` counts: it says the same
/// thing about the name to anyone who can read the difference.
fn forbidden(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<nook_control::error::ApiError>(),
        Some(
            nook_control::error::ApiError::Forbidden
                | nook_control::error::ApiError::ForbiddenMsg(_)
                | nook_control::error::ApiError::Unauthorized
        )
    )
}

/// AC-8: what MCP writes is attributed to the CALLER. The comment's author, the
/// claim's assignee and the description edit all used to record the first
/// tenant's owner — a person who had not touched the card and, on any other
/// instance, is not even a colleague.
#[tokio::test]
async fn writes_are_attributed_to_the_caller() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    // The FIRST tenant of this instance, which is what every MCP write used to
    // be attributed to. Built first so it holds that position.
    let first = org(&bed, &state, "firstorg").await;
    let b = org(&bed, &state, "writer").await;
    let mcp = McpBackend {
        state: state.clone(),
    };

    mcp.comment_task(
        b.caller.clone(),
        b.task.to_string(),
        "mine".into(),
        Some("an agent".into()),
        false,
    )
    .await
    .expect("comment");
    let (author,): (Option<Uuid>,) = bed
        .db()
        .query_one(
            "SELECT author_id FROM task_comments WHERE task_id = $1",
            params![b.task],
        )
        .await
        .expect("the comment's author");
    assert_eq!(
        author,
        Some(b.caller.user_id.0),
        "the comment records the caller, not {:?}",
        first.caller.user_id
    );

    mcp.claim_task(b.caller.clone(), b.task.to_string(), None)
        .await
        .expect("claim");
    let (assignee,): (Option<Uuid>,) = bed
        .db()
        .query_one(
            "SELECT assignee_user_id FROM tasks WHERE id = $1",
            params![b.task],
        )
        .await
        .expect("the claim");
    assert_eq!(
        assignee,
        Some(b.caller.user_id.0),
        "the claim records the caller"
    );

    let edited = mcp
        .set_task_description(b.caller.clone(), b.task.to_string(), "a body".into())
        .await
        .expect("describe");
    assert_eq!(edited.description.as_deref(), Some("a body"));

    let created = mcp
        .create_task(b.caller.clone(), "filed by the caller".into(), None, None)
        .await
        .expect("create");
    let (creator,): (Option<Uuid>,) = bed
        .db()
        .query_one(
            "SELECT created_by FROM tasks WHERE id = $1",
            params![created.id],
        )
        .await
        .expect("the creator");
    assert_eq!(
        creator,
        Some(b.caller.user_id.0),
        "a card filed over MCP names the caller as its author"
    );
    let (board,): (Uuid,) = bed
        .db()
        .query_one(
            "SELECT board_id FROM tasks WHERE id = $1",
            params![created.id],
        )
        .await
        .expect("the board");
    assert_eq!(board, b.board.0, "and lands on the caller's own board");
    assert_ne!(board, first.board.0);

    bed.teardown().await;
}

/// AC-7: the personal notebook is private to a PERSON, and fetch-by-id is where
/// that has to hold — a list that filters is easy, a getter that trusts the id
/// is the hole. Two people in ONE tenant, because tenancy is not what protects
/// a notebook.
#[tokio::test]
async fn a_notebook_is_invisible_to_another_person() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("nbiso").await;
    let (_ua, alice) = bed.user(tenant, "member").await;
    let (_ub, bob) = bed.user(tenant, "member").await;
    let mcp = McpBackend {
        state: state.clone(),
    };

    let folder = mcp
        .notebook_create_folder(
            alice,
            CreateUserNoteFolder {
                name: "Private".into(),
                parent_id: None,
            },
        )
        .await
        .expect("alice's folder");
    let note = mcp
        .notebook_create_note(
            alice,
            CreateUserNote {
                title: "Alice only".into(),
                content_md: "the launch codes".into(),
                folder_id: Some(folder.id),
            },
        )
        .await
        .expect("alice's note");

    assert!(
        mcp.notebook_list_notes(bob, None)
            .await
            .expect("bob lists")
            .is_empty(),
        "bob's notebook is his own"
    );
    assert!(
        mcp.notebook_list_notes(bob, Some("Alice".into()))
            .await
            .expect("bob searches")
            .is_empty(),
        "and search is not a way round the list"
    );
    assert!(
        mcp.notebook_list_folders(bob)
            .await
            .expect("bob's folders")
            .is_empty(),
        "nor are folders"
    );

    // Every by-id door, with a REAL id belonging to Alice.
    type Probe<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + 'a>>;
    let probes: Vec<(&str, Probe)> = vec![
        (
            "notebook_get_note",
            Box::pin(async { mcp.notebook_get_note(bob, note.id).await.map(drop) }),
        ),
        (
            "notebook_update_note",
            Box::pin(async {
                mcp.notebook_update_note(
                    bob,
                    note.id,
                    UpdateUserNote {
                        title: Some("taken".into()),
                        content_md: None,
                        folder_id: None,
                    },
                )
                .await
                .map(drop)
            }),
        ),
        (
            "notebook_delete_note",
            Box::pin(async { mcp.notebook_delete_note(bob, note.id).await }),
        ),
        (
            "notebook_update_folder",
            Box::pin(async {
                mcp.notebook_update_folder(
                    bob,
                    folder.id,
                    UpdateUserNoteFolder {
                        name: Some("taken".into()),
                        parent_id: None,
                    },
                )
                .await
                .map(drop)
            }),
        ),
        (
            "notebook_delete_folder",
            Box::pin(async { mcp.notebook_delete_folder(bob, folder.id).await }),
        ),
    ];
    for (tool, probe) in probes {
        assert!(
            probe.await.is_err(),
            "{tool} reached another person's notebook by id"
        );
    }

    // And Alice's notebook is intact — a refused write must not have half-run.
    let hers = mcp.notebook_get_note(alice, note.id).await.expect("alice");
    assert_eq!(hers.title, "Alice only");
    assert_eq!(hers.content_md.as_deref(), Some("the launch codes"));

    bed.teardown().await;
}

/// AC-1: the two helpers that answered "which tenant" from the instance are
/// gone, and nothing in the MCP backend may resolve one that way again.
///
/// Asserted against the source text because the regression is a NEW call site,
/// not a change to an existing one — and a behavioural test only ever covers the
/// tools somebody remembered to write one for.
#[test]
fn the_mcp_backend_never_resolves_a_tenant_from_the_instance() {
    let src = include_str!("../src/mcp_backend.rs");
    // The module doc explains the ban and has to be able to name them.
    let code: String = src
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    for banned in ["first_tenant", "first_user"] {
        assert!(
            !code.contains(banned),
            "mcp_backend.rs reaches `{banned}` — MCP must take its tenant and \
             its actor from the request's McpCaller, never from the instance \
             (MAIN-592)"
        );
    }
    assert!(
        !code.contains("async fn tenant(&self)") && !code.contains("async fn user(&self)"),
        "the instance-scoped `tenant()`/`user()` helpers are back"
    );
}
