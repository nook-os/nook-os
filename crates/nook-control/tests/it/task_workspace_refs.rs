//! `@slug` in a card's description, from the write to the wire (MAIN-632).
//!
//! Two halves, and they are separate on purpose. Storage (AC-1/AC-2) proves the
//! reference is an ID and not the text — a slug rename must leave every card
//! that named the workspace still pointing at it. Dispatch (AC-4/AC-8) drives
//! the real `jobs::dispatch_to_node` and reads the message that actually went
//! to the node, because a reference the control plane resolved and did not send
//! is a reference the run never had.
//!
//! Needs Postgres: `DATABASE_URL` (`NOOK_REQUIRE_DB=1` in the suite).

use nook_control::services::jobs;
use nook_control::services::kanban::{KanbanProvider, LocalBoardProvider};
use nook_control::ws::registry::NodeHandle;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use tokio::sync::mpsc;
use uuid::Uuid;

fn provider(bed: &TestBed) -> LocalBoardProvider {
    LocalBoardProvider {
        repo: std::sync::Arc::new(nook_control::repo::tasks::DbTaskRepository::new(bed.db())),
    }
}

async fn board(bed: &TestBed, tenant: TenantId, key: &str) -> (BoardId, ColumnId) {
    let board = BoardId::new();
    bed.db()
        .exec(
            "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b',$3,'local')",
            params![board, tenant, key],
        )
        .await
        .expect("board");
    let col = ColumnId::new();
    bed.db()
        .exec(
            "INSERT INTO board_columns (id, board_id, name, position, type)
             VALUES ($1,$2,'Todo',0,'unstarted')",
            params![col, board],
        )
        .await
        .expect("column");
    (board, col)
}

/// A workspace with a slug an `@mention` can name.
async fn workspace(
    bed: &TestBed,
    tenant: TenantId,
    slug: &str,
    remote: Option<&str>,
) -> WorkspaceId {
    let id = WorkspaceId::new();
    bed.db()
        .exec(
            "INSERT INTO workspaces (id, tenant_id, name, slug, git_remote_url)
             VALUES ($1,$2,$3,$4,$5)",
            params![
                id,
                tenant,
                format!("The {slug}"),
                slug,
                remote.map(str::to_string)
            ],
        )
        .await
        .expect("workspace");
    id
}

async fn card(
    bed: &TestBed,
    tenant: TenantId,
    board: BoardId,
    ws: WorkspaceId,
    body: &str,
) -> TaskId {
    provider(bed)
        .create_task(
            tenant,
            board,
            None,
            CreateTaskRequest {
                title: "cross-repo work".into(),
                description: Some(body.into()),
                column_id: None,
                column_type: Some("unstarted".into()),
                workspace_id: Some(ws),
                priority: None,
                type_: None,
                visibility: None,
                parent: None,
                labels: Vec::new(),
            },
        )
        .await
        .expect("card")
        .id
}

/// The stored reference rows, by slug — the ids, read back through the join the
/// detail endpoint reads.
async fn stored(bed: &TestBed, tenant: TenantId, task: TaskId) -> Vec<(WorkspaceId, String)> {
    let repo = nook_control::repo::tasks::DbTaskRepository::new(bed.db());
    use nook_control::repo::tasks::TaskRepository;
    repo.workspace_refs_of(tenant, task)
        .await
        .expect("refs")
        .into_iter()
        .map(|r| (r.workspace_id, r.slug))
        .collect()
}

/// AC-1: two `@slug`s in one body are stored as two workspace IDS. The text is
/// left exactly as written — it is the ids the run reads, which is what makes a
/// later slug rename harmless.
#[tokio::test]
async fn two_mentions_are_stored_as_workspace_ids() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("twr-store").await;
    let (b, _) = board(&bed, tenant, "TWR").await;
    let own = bed.workspace(tenant).await;
    let web = workspace(
        &bed,
        tenant,
        "nook-web",
        Some("git@example.test:acme/web.git"),
    )
    .await;
    let api = workspace(
        &bed,
        tenant,
        "nook-api",
        Some("git@example.test:acme/api.git"),
    )
    .await;

    let body = "Wire @nook-web against @nook-api's new endpoint.";
    let task = card(&bed, tenant, b, own, body).await;

    let mut rows = stored(&bed, tenant, task).await;
    rows.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(
        rows,
        vec![(api, "nook-api".to_string()), (web, "nook-web".to_string())],
        "both mentions resolve to stored workspace ids"
    );

    // A rename is exactly the case the id exists for: the reference survives it.
    bed.db()
        .exec(
            "UPDATE workspaces SET slug = 'nook-frontend' WHERE id = $1",
            params![web],
        )
        .await
        .expect("rename");
    assert!(
        stored(&bed, tenant, task)
            .await
            .iter()
            .any(|(id, slug)| *id == web && slug == "nook-frontend"),
        "renaming a slug must not orphan the reference — that is why the id is stored"
    );

    bed.teardown().await;
}

/// AC-2: an unresolvable `@word` is not an error and not a reference. Writing a
/// description is not a place to fail on a typo, and the body is stored
/// byte-for-byte either way.
#[tokio::test]
async fn an_unknown_slug_is_left_as_plain_text() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("twr-typo").await;
    let (b, _) = board(&bed, tenant, "TWT").await;
    let own = bed.workspace(tenant).await;

    let body = "Check @not-a-real-slug and mail dev@nookos.local about it.";
    let task = card(&bed, tenant, b, own, body).await;

    assert!(
        stored(&bed, tenant, task).await.is_empty(),
        "an unknown slug stores no reference"
    );
    let (round_trip,): (String,) = bed
        .db()
        .query_one("SELECT description FROM tasks WHERE id = $1", params![task])
        .await
        .expect("body");
    assert_eq!(round_trip, body, "the description round-trips unchanged");

    bed.teardown().await;
}

/// NG-3: a slug resolves within the card's tenant or not at all. The same
/// spelling in another tenant is a different repo and must not be reachable.
#[tokio::test]
async fn a_slug_does_not_resolve_across_tenants() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let mine = bed.tenant("twr-mine").await;
    let theirs = bed.tenant("twr-theirs").await;
    workspace(
        &bed,
        theirs,
        "nook-web",
        Some("git@example.test:other/web.git"),
    )
    .await;
    let (b, _) = board(&bed, mine, "TWX").await;
    let own = bed.workspace(mine).await;

    let task = card(&bed, mine, b, own, "See @nook-web.").await;

    assert!(
        stored(&bed, mine, task).await.is_empty(),
        "another tenant's slug is not a workspace here"
    );

    bed.teardown().await;
}

// ── dispatch ────────────────────────────────────────────────────────────────

async fn node(bed: &TestBed, tenant: TenantId) -> NodeId {
    let id = NodeId::new();
    bed.db()
        .exec(
            "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status)
             VALUES ($1,$2,$3,$4,'online')",
            params![
                id,
                tenant,
                format!("n-{}", id.0.simple()),
                format!("h-{}", id.0.simple())
            ],
        )
        .await
        .expect("node");
    id
}

async fn checkout(bed: &TestBed, tenant: TenantId, n: NodeId, ws: WorkspaceId, path: &str) {
    bed.db()
        .exec(
            "INSERT INTO node_workspaces (id, tenant_id, node_id, workspace_id, path,
                                          git_remote_url, git_branch, kind)
             VALUES ($1,$2,$3,$4,$5,'git@example.test:acme/repo.git','trunk','clone')",
            params![Uuid::now_v7(), tenant, n, ws, path],
        )
        .await
        .expect("checkout");
}

async fn build_job(
    bed: &TestBed,
    tenant: TenantId,
    task: TaskId,
    ws: WorkspaceId,
    executor: NodeId,
) -> LoopJob {
    let id = JobId::new();
    bed.db()
        .exec(
            "INSERT INTO loop_jobs
                (id, tenant_id, kind, target_task_id, workspace_id, requested_by, state,
                 executor_node_id)
             VALUES ($1,$2,'build',$3,$4,$5,'claimed',$6)",
            params![id, tenant, task, ws, Uuid::now_v7(), executor],
        )
        .await
        .expect("job");
    bed.db()
        .query_one("SELECT * FROM loop_jobs WHERE id = $1", params![id])
        .await
        .expect("load")
}

/// Dispatch the job and hand back the references the node was actually sent.
async fn dispatched_references(
    bed: &TestBed,
    tenant: TenantId,
    job: &LoopJob,
    n: NodeId,
) -> Vec<WorkspaceRef> {
    let state = bed.app_state().await;
    let (tx, mut rx) = mpsc::channel(4);
    state.registry.register_node(
        n,
        NodeHandle {
            tenant_id: tenant,
            tx,
        },
    );
    jobs::dispatch_to_node(&state, tenant, job)
        .await
        .expect("dispatch");
    match rx.try_recv().expect("a RunLoopJob was sent") {
        nook_proto::ControlToNode::RunLoopJob { references, .. } => references,
        other => panic!("expected RunLoopJob, got {other:?}"),
    }
}

/// AC-4: the run carries its card's references, each located on the executor it
/// was placed on. The path is the whole point — without it the node has a name
/// and nothing to mount.
#[tokio::test]
async fn a_run_carries_its_cards_references_with_this_executors_path() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("twr-dispatch").await;
    let (b, _) = board(&bed, tenant, "TWD").await;
    let own = bed.workspace(tenant).await;
    let web = workspace(
        &bed,
        tenant,
        "nook-web",
        Some("git@example.test:acme/web.git"),
    )
    .await;
    let n = node(&bed, tenant).await;
    checkout(&bed, tenant, n, own, "/checkouts/own").await;
    checkout(&bed, tenant, n, web, "/checkouts/nook-web").await;

    let task = card(&bed, tenant, b, own, "Match @nook-web's fetch shape.").await;
    let job = build_job(&bed, tenant, task, own, n).await;

    let refs = dispatched_references(&bed, tenant, &job, n).await;
    assert_eq!(refs.len(), 1, "one reference on the wire: {refs:?}");
    assert_eq!(refs[0].workspace_id, web);
    assert_eq!(refs[0].slug, "nook-web");
    assert_eq!(
        refs[0].path.as_deref(),
        Some("/checkouts/nook-web"),
        "the path is this executor's checkout of the referenced workspace"
    );

    bed.teardown().await;
}

/// AC-8: a reference this executor holds no checkout of does NOT hold the job
/// back. It is dispatched, the reference travels with no path, and the node's
/// brief is what tells the agent which repo it did not get. Placement is
/// untouched (NG-2) — there is no new queued reason here to assert, and that
/// absence is the property.
#[tokio::test]
async fn a_reference_the_executor_lacks_still_dispatches() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("twr-absent").await;
    let (b, _) = board(&bed, tenant, "TWA").await;
    let own = bed.workspace(tenant).await;
    let api = workspace(
        &bed,
        tenant,
        "nook-api",
        Some("git@example.test:acme/api.git"),
    )
    .await;
    let n = node(&bed, tenant).await;
    // The card's OWN workspace is checked out here; the referenced one is not.
    checkout(&bed, tenant, n, own, "/checkouts/own").await;

    let task = card(&bed, tenant, b, own, "Follows @nook-api's contract.").await;
    let job = build_job(&bed, tenant, task, own, n).await;

    let refs = dispatched_references(&bed, tenant, &job, n).await;
    assert_eq!(refs.len(), 1, "the reference still travels: {refs:?}");
    assert_eq!(refs[0].workspace_id, api);
    assert_eq!(
        refs[0].path, None,
        "no checkout here, so no path — and the run proceeds anyway"
    );
    assert_eq!(
        refs[0].git_remote_url.as_deref(),
        Some("git@example.test:acme/api.git"),
        "the remote rides along, so the brief can name what the run could not read"
    );

    let after: LoopJob = bed
        .db()
        .query_one("SELECT * FROM loop_jobs WHERE id = $1", params![job.id])
        .await
        .expect("reload");
    assert_eq!(after.state, "running", "the job was dispatched, not held");

    bed.teardown().await;
}

/// The card's OWN workspace is never a reference, even when the body names it:
/// the run is already in that repo read-write, and mounting the same host path
/// a second time read-only is a contradiction Docker would have to resolve.
#[tokio::test]
async fn the_cards_own_workspace_is_not_mounted_twice() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("twr-self").await;
    let (b, _) = board(&bed, tenant, "TWS").await;
    let own = workspace(
        &bed,
        tenant,
        "nook-os",
        Some("git@example.test:acme/os.git"),
    )
    .await;
    let n = node(&bed, tenant).await;
    checkout(&bed, tenant, n, own, "/checkouts/own").await;

    let task = card(&bed, tenant, b, own, "All inside @nook-os.").await;
    assert_eq!(
        stored(&bed, tenant, task).await.len(),
        1,
        "the body named it, so the card records it"
    );

    let job = build_job(&bed, tenant, task, own, n).await;
    assert!(
        dispatched_references(&bed, tenant, &job, n)
            .await
            .is_empty(),
        "…but the run is already in that repo, so nothing is mounted for it"
    );

    bed.teardown().await;
}
