//! Mission Control's aggregate read must compose the existing visibility rules
//! exactly, never becoming a side-channel around them (MAIN-226 AC-6): checkouts
//! are node-scoped (own + shared), sessions are creator-scoped (MAIN-133).
//!
//! Runs against a private `nook_testkit::TestBed`. Set `DATABASE_URL`.

use nook_control::services::overview_queries::overview;
use nook_db::{params, Db, DbPool};
use nook_testkit::TestBed;
use nook_types::*;

async fn checkout(
    db: &DbPool,
    tenant: TenantId,
    node: NodeId,
    ws: WorkspaceId,
    path: &str,
    kind: &str,
) -> NodeWorkspaceId {
    let id = NodeWorkspaceId::new();
    db.exec(
        "INSERT INTO node_workspaces (id, tenant_id, node_id, workspace_id, path, kind)
         VALUES ($1, $2, $3, $4, $5, $6)",
        params![id, tenant, node, ws, path, kind],
    )
    .await
    .expect("checkout");
    id
}

async fn session(
    db: &DbPool,
    tenant: TenantId,
    ws: WorkspaceId,
    node: NodeId,
    creator: UserId,
    checkout: NodeWorkspaceId,
) -> SessionId {
    let id = SessionId::new();
    db.exec(
        "INSERT INTO sessions
             (id, tenant_id, workspace_id, node_id, name, runtime, status, created_by, checkout_id)
         VALUES ($1, $2, $3, $4, 's', 'bash', 'running', $5, $6)",
        params![id, tenant, ws, node, creator, checkout],
    )
    .await
    .expect("session");
    id
}

/// Every checkout id across the payload's workspaces.
fn all_checkout_ids(ov: &Overview) -> Vec<NodeWorkspaceId> {
    ov.workspaces
        .iter()
        .flat_map(|w| w.checkouts.iter().map(|c| c.id))
        .collect()
}

/// Every session id bound under any checkout.
fn all_session_ids(ov: &Overview) -> Vec<SessionId> {
    ov.workspaces
        .iter()
        .flat_map(|w| {
            w.checkouts
                .iter()
                .flat_map(|c| c.sessions.iter().map(|s| s.id))
        })
        .collect()
}

#[tokio::test]
async fn overview_scopes_checkouts_by_node_and_sessions_by_creator() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ov").await;
    let (user_a, person_a) = bed.user(tenant, "member").await;
    let (user_b, person_b) = bed.user(tenant, "member").await;
    let node_a = bed.node(tenant, person_a).await;
    let node_b = bed.node(tenant, person_b).await;
    let ws = bed.workspace(tenant).await;

    let co_a = checkout(&bed.db(), tenant, node_a, ws, "/srv/a", "clone").await;
    let co_b = checkout(&bed.db(), tenant, node_b, ws, "/srv/b", "clone").await;
    let sess_a = session(&bed.db(), tenant, ws, node_a, user_a, co_a).await;
    let sess_b = session(&bed.db(), tenant, ws, node_b, user_b, co_b).await;

    // Admin scope (both None): the whole fleet — both checkouts, both sessions.
    let admin = overview(&bed.db(), tenant, None, None, None).await.unwrap();
    let admin_cos = all_checkout_ids(&admin);
    assert!(
        admin_cos.contains(&co_a) && admin_cos.contains(&co_b),
        "admin sees both checkouts"
    );
    let admin_sessions = all_session_ids(&admin);
    assert!(
        admin_sessions.contains(&sess_a) && admin_sessions.contains(&sess_b),
        "admin sees both sessions"
    );

    // Member A: only their own node's checkout, only their own session.
    let a = overview(
        &bed.db(),
        tenant,
        Some(person_a),
        Some(user_a),
        Some(user_a),
    )
    .await
    .unwrap();
    let a_cos = all_checkout_ids(&a);
    assert!(a_cos.contains(&co_a), "member A sees their own checkout");
    assert!(
        !a_cos.contains(&co_b),
        "member A does NOT see B's node checkout — no side-channel"
    );
    let a_sessions = all_session_ids(&a);
    assert_eq!(
        a_sessions,
        vec![sess_a],
        "member A sees only their own session"
    );

    // Share node B with the team: A now sees B's checkout (own+shared), but STILL
    // not B's session — the two axes are independent.
    bed.db()
        .exec(
            "UPDATE nodes SET shared = true WHERE id = $1",
            params![node_b],
        )
        .await
        .expect("share node B");
    let shared = overview(
        &bed.db(),
        tenant,
        Some(person_a),
        Some(user_a),
        Some(user_a),
    )
    .await
    .unwrap();
    let shared_cos = all_checkout_ids(&shared);
    assert!(
        shared_cos.contains(&co_a) && shared_cos.contains(&co_b),
        "a shared node's checkout becomes visible"
    );
    assert!(
        !all_session_ids(&shared).contains(&sess_b),
        "sharing a node does NOT expose its owner's sessions"
    );

    let _ = person_b;
    bed.teardown().await;
}

#[tokio::test]
async fn overview_groups_the_hierarchy_and_omits_empty_workspaces() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ov").await;
    let (user, person) = bed.user(tenant, "member").await;
    let node = bed.node(tenant, person).await;
    let ws = bed.workspace(tenant).await;
    // A second workspace with NO checkouts/sessions → must be omitted.
    let _empty = bed.workspace(tenant).await;

    let clone = checkout(&bed.db(), tenant, node, ws, "/srv/clone", "clone").await;
    let wt = checkout(&bed.db(), tenant, node, ws, "/srv/wt", "worktree").await;
    let sess = session(&bed.db(), tenant, ws, node, user, clone).await;

    let ov = overview(&bed.db(), tenant, Some(person), Some(user), Some(user))
        .await
        .unwrap();
    assert_eq!(ov.workspaces.len(), 1, "the empty workspace is omitted");
    let w = &ov.workspaces[0];
    assert_eq!(w.id, ws);
    assert_eq!(w.checkouts.len(), 2, "both checkouts appear under the repo");

    // Kind badges carry through; the session sits under its clone, the worktree is bare.
    let clone_row = w.checkouts.iter().find(|c| c.id == clone).unwrap();
    let wt_row = w.checkouts.iter().find(|c| c.id == wt).unwrap();
    assert_eq!(clone_row.kind, "clone");
    assert_eq!(wt_row.kind, "worktree");
    assert_eq!(
        clone_row.sessions.iter().map(|s| s.id).collect::<Vec<_>>(),
        vec![sess]
    );
    assert!(wt_row.sessions.is_empty());

    bed.teardown().await;
}

// ── MAIN-226 review fix: the dev seed populates Mission Control ──────────────

#[tokio::test]
async fn dev_seed_populates_mission_control() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    // TestBed's template runs `seed::run` (Config::for_test, tenant slug "test"),
    // which now seeds the Mission Control demo: a repo with a remote, a clone + a
    // worktree, a tombstoned checkout, a bound session, and a loose terminal.
    let tenant: TenantId = bed
        .db()
        .query_scalar("SELECT id FROM tenants WHERE slug = 'test'", params![])
        .await
        .expect("the seeded dev tenant");

    let ov = overview(&bed.db(), tenant, None, None, None).await.unwrap();
    let demo = ov
        .workspaces
        .iter()
        .find(|w| w.slug == "mission-demo")
        .expect("the demo workspace is seeded");

    assert!(
        demo.git_remote_url.is_some(),
        "the demo repo shows its remote"
    );
    assert_eq!(demo.checkouts.len(), 3, "clone + worktree + tombstoned");
    assert!(demo.checkouts.iter().any(|c| c.kind == "clone"));
    assert!(demo.checkouts.iter().any(|c| c.kind == "worktree"));
    assert!(
        demo.checkouts.iter().any(|c| c.missing_at.is_some()),
        "a tombstoned checkout for the ghosting"
    );
    assert!(
        demo.checkouts.iter().any(|c| !c.sessions.is_empty()),
        "a session bound to a checkout"
    );
    assert!(
        !ov.loose_sessions.is_empty(),
        "a loose $HOME terminal with no workspace"
    );

    bed.teardown().await;
}

// ── MAIN-230: the ticket a checkout is working ───────────────────────────────
//
// The join that names the ticket must compose card visibility exactly. A
// private card a teammate is working on a SHARED node is the hole: the checkout
// is legitimately visible, so the row appears either way — but the key and title
// must not ride along on it.

/// A board with a known key and one column of `col_type`.
async fn board_with_column(
    db: &DbPool,
    tenant: TenantId,
    key: &str,
    col_type: &str,
) -> (BoardId, ColumnId) {
    let board = BoardId::new();
    db.exec(
        "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b',$3,'local')",
        params![board, tenant, key],
    )
    .await
    .expect("board");
    let col = ColumnId::new();
    db.exec(
        "INSERT INTO board_columns (id, board_id, name, position, type)
         VALUES ($1,$2,'Doing',0,$3)",
        params![col, board, col_type],
    )
    .await
    .expect("column");
    (board, col)
}

/// A task pinned to a checkout by `checkout_id` (the durable join) or to a
/// session by `session_id` (the fresh-worktree fallback), with a visibility.
#[allow(clippy::too_many_arguments)]
async fn task_on(
    db: &DbPool,
    tenant: TenantId,
    board: BoardId,
    col: ColumnId,
    number: i32,
    creator: UserId,
    visibility: &str,
    checkout: Option<NodeWorkspaceId>,
    session: Option<SessionId>,
) -> TaskId {
    let id = TaskId::new();
    db.exec(
        "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, type, number,
                            created_by, visibility, checkout_id, session_id)
         VALUES ($1,$2,$3,$4,$5,'task',$6,$7,$8,$9,$10)",
        params![
            id,
            tenant,
            board,
            col,
            format!("title-{number}"),
            number,
            creator,
            visibility,
            checkout.map(|v| v.0),
            session.map(|v| v.0)
        ],
    )
    .await
    .expect("task");
    id
}

/// Every task key the overview exposes, wherever it sits.
fn keys(ov: &Overview) -> Vec<String> {
    let mut out: Vec<String> = ov
        .workspaces
        .iter()
        .flat_map(|w| w.checkouts.iter())
        .flat_map(|c| c.tasks.iter())
        .map(|t| t.key.clone())
        .collect();
    out.sort();
    out
}

#[tokio::test]
async fn a_checkout_names_its_ticket_by_checkout_id_and_by_session_fallback() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ovtask").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let node = bed.node(tenant, person).await;
    let (board, col) = board_with_column(&bed.db(), tenant, "MAIN", "started").await;

    // (1) The durable join: discovery has scanned, so the task points at the
    //     checkout directly.
    let scanned = checkout(&bed.db(), tenant, node, ws, "/srv/a", "worktree").await;
    task_on(
        &bed.db(),
        tenant,
        board,
        col,
        41,
        user,
        "team",
        Some(scanned),
        None,
    )
    .await;

    // (2) The fallback: work started seconds ago. `tasks.checkout_id` is still
    //     NULL — MAIN-225 never guesses it — so only the session knows where the
    //     work is. Without this join the chip would not appear until the next
    //     scan, which is exactly when you most want it.
    let fresh = checkout(&bed.db(), tenant, node, ws, "/srv/b", "worktree").await;
    let sess = session(&bed.db(), tenant, ws, node, user, fresh).await;
    task_on(
        &bed.db(),
        tenant,
        board,
        col,
        42,
        user,
        "team",
        None,
        Some(sess),
    )
    .await;

    let ov = overview(&bed.db(), tenant, None, None, None).await.unwrap();
    assert_eq!(keys(&ov), vec!["MAIN-41", "MAIN-42"]);

    // The chip carries what it needs to render and link.
    let t = ov.workspaces[0]
        .checkouts
        .iter()
        .find(|c| c.id == fresh)
        .expect("the fresh checkout is present")
        .tasks
        .first()
        .expect("and names its ticket");
    assert_eq!(t.key, "MAIN-42");
    assert_eq!(t.title, "title-42");
    assert_eq!(t.column_type, "started", "the TYPE, not the column name");

    bed.teardown().await;
}

/// Load-bearing (AC-1/AC-5): the ticket join must not leak a private card.
#[tokio::test]
async fn a_private_card_never_rides_out_on_a_shared_nodes_checkout() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ovtask").await;
    let (owner, owner_person) = bed.user(tenant, "owner").await;
    let (member, member_person) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;

    // A SHARED node: the member can legitimately see its checkouts, which is
    // what makes this the interesting case rather than a trivially hidden one.
    let node = bed.node(tenant, owner_person).await;
    bed.db()
        .exec(
            "UPDATE nodes SET shared = true WHERE id = $1",
            params![node],
        )
        .await
        .expect("share the node");

    let (board, col) = board_with_column(&bed.db(), tenant, "MAIN", "started").await;
    let co = checkout(&bed.db(), tenant, node, ws, "/srv/secret", "worktree").await;
    task_on(
        &bed.db(),
        tenant,
        board,
        col,
        7,
        owner,
        "private",
        Some(co),
        None,
    )
    .await;

    // The member sees the checkout…
    let seen = overview(
        &bed.db(),
        tenant,
        Some(member_person),
        Some(member),
        Some(member),
    )
    .await
    .unwrap();
    assert!(
        seen.workspaces
            .iter()
            .flat_map(|w| w.checkouts.iter())
            .any(|c| c.id == co),
        "the shared node's checkout is visible — that is the premise"
    );
    // …and nothing about the ticket on it.
    assert_eq!(keys(&seen), Vec::<String>::new(), "no private key leaks");

    // The owner, and an admin (task_viewer = None), both see it.
    let mine = overview(
        &bed.db(),
        tenant,
        Some(owner_person),
        Some(owner),
        Some(owner),
    )
    .await
    .unwrap();
    assert_eq!(keys(&mine), vec!["MAIN-7"], "its owner sees their own card");

    let admin = overview(&bed.db(), tenant, None, None, None).await.unwrap();
    assert_eq!(
        keys(&admin),
        vec!["MAIN-7"],
        "an admin sees the whole board"
    );

    bed.teardown().await;
}

/// An archived card is off the board, so it must be off the checkout too —
/// otherwise a finished worktree keeps advertising work nobody is doing.
#[tokio::test]
async fn archived_tasks_do_not_label_a_checkout() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ovtask").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let node = bed.node(tenant, person).await;
    let (board, col) = board_with_column(&bed.db(), tenant, "MAIN", "completed").await;
    let co = checkout(&bed.db(), tenant, node, ws, "/srv/old", "worktree").await;
    let t = task_on(
        &bed.db(),
        tenant,
        board,
        col,
        9,
        user,
        "team",
        Some(co),
        None,
    )
    .await;

    assert_eq!(
        keys(&overview(&bed.db(), tenant, None, None, None).await.unwrap()),
        vec!["MAIN-9"]
    );

    bed.db()
        .exec(
            "UPDATE tasks SET archived_at = $2 WHERE id = $1",
            params![t, chrono::Utc::now()],
        )
        .await
        .expect("archive");

    assert_eq!(
        keys(&overview(&bed.db(), tenant, None, None, None).await.unwrap()),
        Vec::<String>::new(),
        "an archived card stops labelling its checkout"
    );

    bed.teardown().await;
}
