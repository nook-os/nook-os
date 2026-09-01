//! Task/board callers, exercised with **no database at all** (MAIN-248 AC-3).
//!
//! These are the **callers**, not the repository. Testing the fake against the
//! trait would prove only that the fake does what the fake does; the value is in
//! running `services::tasks` — real code, unmodified — against an in-memory
//! store, and in pinning the rules that used to need a migrated Postgres to
//! assert.
//!
//! Stop Postgres and run `cargo test -p nook-control --test task_fake`; it
//! passes, which is the AC's own verification step 3.

use nook_control::repo::tasks::{FakeTaskRepository, NewTask, TaskEdit, TaskRepository};
use nook_control::services::tasks::{column_of_type, enrich, resolve_id};
use nook_types::{TenantId, UserId};

fn seeded() -> (FakeTaskRepository, TenantId, nook_types::BoardId) {
    let repo = FakeTaskRepository::new();
    let tenant = TenantId::new();
    let board = repo.with_board(tenant, "Main", "MAIN").id;
    repo.with_column(board, "Triage", "backlog", 0);
    repo.with_column(board, "Todo", "unstarted", 1);
    repo.with_column(board, "In Progress", "started", 2);
    (repo, tenant, board)
}

async fn task(
    repo: &FakeTaskRepository,
    tenant: TenantId,
    board: nook_types::BoardId,
    title: &str,
) -> nook_types::TaskItem {
    let column = repo.first_column(board).await.unwrap().unwrap();
    repo.create_task(NewTask {
        tenant,
        board,
        column_id: column,
        title: title.into(),
        description: None,
        position: 0,
        workspace_id: None,
        priority: 0,
        type_: "task".into(),
        visibility: "team".into(),
        created_by: None,
        parent_task_id: None,
        labels: vec![],
    })
    .await
    .unwrap()
}

/// Numbers are allocated per board, once each. In the real impl a `FOR UPDATE`
/// on the board row is what guarantees this; the fake holds the counter under
/// the same lock as the insert, so a caller test cannot pass while the
/// allocation is broken.
#[tokio::test]
async fn task_numbers_are_allocated_once_each_per_board() {
    let (repo, tenant, board) = seeded();
    let other = repo.with_board(tenant, "Ops", "OPS").id;
    repo.with_column(other, "Todo", "unstarted", 0);

    let a = task(&repo, tenant, board, "first").await;
    let b = task(&repo, tenant, board, "second").await;
    assert_eq!((a.number, b.number), (Some(1), Some(2)));

    // A second board counts independently — the number is per board, not global.
    let c = task(&repo, tenant, other, "elsewhere").await;
    assert_eq!(c.number, Some(1));
}

/// `enrich` builds `BOARD-N` keys and deep links without touching the database
/// per task. Two batched reads regardless of how many tasks arrive.
#[tokio::test]
async fn enrich_builds_keys_and_urls_from_the_board() {
    let (repo, tenant, board) = seeded();
    let t = task(&repo, tenant, board, "ship it").await;

    let out = enrich(&repo, "https://nook.example/", UserId::new(), vec![t])
        .await
        .expect("enrich");
    assert_eq!(out[0].key.as_deref(), Some("MAIN-1"));
    assert_eq!(
        out[0].url.as_deref(),
        Some("https://nook.example/board?task=MAIN-1"),
        "the deep link uses the key, and the trailing slash is trimmed"
    );
}

/// A private epic's key must not leak onto a child a non-owner can see
/// (MAIN-86). The owner still sees it — otherwise the redaction would be a bug
/// rather than a rule.
#[tokio::test]
async fn a_private_parents_key_is_redacted_for_a_non_owner() {
    let (repo, tenant, board) = seeded();
    let owner = UserId::new();
    let stranger = UserId::new();

    let column = repo.first_column(board).await.unwrap().unwrap();
    let epic = repo
        .create_task(NewTask {
            tenant,
            board,
            column_id: column,
            title: "secret epic".into(),
            description: None,
            position: 0,
            workspace_id: None,
            priority: 0,
            type_: "epic".into(),
            visibility: "private".into(),
            created_by: Some(owner.0),
            parent_task_id: None,
            labels: vec![],
        })
        .await
        .unwrap();

    let child = repo
        .create_task(NewTask {
            tenant,
            board,
            column_id: column,
            title: "child".into(),
            description: None,
            position: 0,
            workspace_id: None,
            priority: 0,
            type_: "task".into(),
            visibility: "team".into(),
            created_by: None,
            parent_task_id: Some(epic.id.0),
            labels: vec![],
        })
        .await
        .unwrap();

    let seen_by_stranger = enrich(&repo, "https://x/", stranger, vec![child.clone()])
        .await
        .unwrap();
    assert_eq!(
        seen_by_stranger[0].parent_key, None,
        "a stranger sees the child parentless rather than a key they cannot open"
    );

    let seen_by_owner = enrich(&repo, "https://x/", owner, vec![child])
        .await
        .unwrap();
    assert_eq!(
        seen_by_owner[0].parent_key.as_deref(),
        Some("MAIN-1"),
        "the epic's owner still sees it"
    );
}

/// Keys resolve case-insensitively and are tenant-scoped — a uuid is not an
/// authorisation, and neither is knowing a key.
#[tokio::test]
async fn tasks_resolve_by_key_or_uuid_within_the_tenant_only() {
    let (repo, tenant, board) = seeded();
    let t = task(&repo, tenant, board, "findable").await;

    assert_eq!(resolve_id(&repo, tenant, "MAIN-1").await.unwrap(), t.id);
    assert_eq!(
        resolve_id(&repo, tenant, "main-1").await.unwrap(),
        t.id,
        "board keys match case-insensitively"
    );
    assert_eq!(
        resolve_id(&repo, tenant, &t.id.0.to_string())
            .await
            .unwrap(),
        t.id
    );

    let other_tenant = TenantId::new();
    assert!(
        resolve_id(&repo, other_tenant, "MAIN-1").await.is_err(),
        "another tenant cannot resolve this key"
    );
    assert!(
        resolve_id(&repo, other_tenant, &t.id.0.to_string())
            .await
            .is_err(),
        "…nor its uuid"
    );
}

/// The optimistic-concurrency precondition (MAIN-36): a guarded update whose
/// `updated_at` has moved matches nothing, which is how a lost race is told
/// apart from a successful edit.
#[tokio::test]
async fn a_guarded_update_fails_once_the_row_has_moved() {
    let (repo, tenant, board) = seeded();
    let t = task(&repo, tenant, board, "contended").await;
    let seen_at = t.updated_at;

    // Somebody else edits first.
    repo.update_fields(
        tenant,
        t.id,
        TaskEdit {
            title: Some("theirs".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .expect("the unguarded edit lands");

    // Our guarded edit, based on what we last saw, now matches no row.
    let lost = repo
        .update_fields(
            tenant,
            t.id,
            TaskEdit {
                title: Some("ours".into()),
                expected_updated_at: Some(seen_at),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(lost.is_none(), "a stale precondition matches no row");

    let still = repo.get_row(tenant, t.id).await.unwrap().unwrap();
    assert_eq!(still.title, "theirs", "and the winner's edit stands");
}

/// The workspace flag is the clear-vs-omit distinction `COALESCE` cannot
/// express. Omitting leaves the field; setting it to `None` clears it.
#[tokio::test]
async fn clearing_a_workspace_is_told_apart_from_not_mentioning_it() {
    let (repo, tenant, board) = seeded();
    let t = task(&repo, tenant, board, "has a workspace").await;
    let ws = nook_types::WorkspaceId::new();

    repo.update_fields(
        tenant,
        t.id,
        TaskEdit {
            set_workspace: true,
            workspace_id: Some(ws.0),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        repo.get_row(tenant, t.id)
            .await
            .unwrap()
            .unwrap()
            .workspace_id,
        Some(ws)
    );

    // Not mentioned → left alone.
    repo.update_fields(
        tenant,
        t.id,
        TaskEdit {
            title: Some("renamed".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        repo.get_row(tenant, t.id)
            .await
            .unwrap()
            .unwrap()
            .workspace_id,
        Some(ws),
        "an omitted workspace is not a clear"
    );

    // Mentioned as null → cleared.
    repo.update_fields(
        tenant,
        t.id,
        TaskEdit {
            set_workspace: true,
            workspace_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        repo.get_row(tenant, t.id)
            .await
            .unwrap()
            .unwrap()
            .workspace_id,
        None,
        "an explicit null clears it"
    );
}

/// Labels are filed with the task, inside the create — the pick query must
/// never see a task without the labels it was created with.
#[tokio::test]
async fn a_task_arrives_already_carrying_its_labels() {
    let (repo, tenant, board) = seeded();
    let column = repo.first_column(board).await.unwrap().unwrap();
    let t = repo
        .create_task(NewTask {
            tenant,
            board,
            column_id: column,
            title: "labelled".into(),
            description: None,
            position: 0,
            workspace_id: None,
            priority: 0,
            type_: "task".into(),
            visibility: "team".into(),
            created_by: None,
            parent_task_id: None,
            labels: vec!["agent-ready".into(), "urgent".into()],
        })
        .await
        .unwrap();

    assert_eq!(repo.labels_of(t.id), vec!["agent-ready", "urgent"]);

    repo.detach_label(tenant, t.id, "urgent").await.unwrap();
    assert_eq!(repo.labels_of(t.id), vec!["agent-ready"]);
}

/// Column types resolve by meaning, lowest position winning — so renaming
/// "In Progress" to "Doing" stays cosmetic.
#[tokio::test]
async fn columns_resolve_by_type_not_by_name() {
    let (repo, tenant, board) = seeded();
    let _ = tenant;
    let started = column_of_type(&repo, board, "started").await.unwrap();
    assert_eq!(
        repo.column_by_name(board, "in progress").await.unwrap(),
        Some(started),
        "the semantic type finds the column its name happens to have"
    );

    // A board with no column of that type is a pointed 409, not a 500.
    let bare = repo.with_board(tenant, "Bare", "BARE").id;
    let err = column_of_type(&repo, bare, "review").await.unwrap_err();
    assert!(
        format!("{err:?}").contains("review"),
        "the refusal names the missing type: {err:?}"
    );
}
