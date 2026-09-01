//! The routes-side task/board rules, with **no database at all** (MAIN-249 AC-3).
//!
//! MAIN-248 put the task *services* behind the trait; this card put the routes
//! and the pick query behind it too. These are the rules that migration could
//! quietly break — the atomic claim, the backlog and epic exclusions, the
//! visibility predicate on every read path — and each one used to need a
//! migrated Postgres to assert.
//!
//! Stop Postgres and run `cargo test -p nook-control --test task_routes_fake`;
//! it passes, which is the AC's own verification step 3.

use nook_control::repo::tasks::{FakeTaskRepository, NewTask, PickParams, TaskRepository};
use nook_types::{BoardId, ColumnId, TaskId, TenantId, UserId};

struct Fx {
    repo: FakeTaskRepository,
    tenant: TenantId,
    board: BoardId,
    triage: ColumnId,
    todo: ColumnId,
}

fn fx() -> Fx {
    let repo = FakeTaskRepository::new();
    let tenant = TenantId::new();
    let board = repo.with_board(tenant, "Main", "MAIN").id;
    let triage = repo.with_column(board, "Triage", "backlog", 0);
    let todo = repo.with_column(board, "Todo", "unstarted", 1);
    repo.with_column(board, "In Progress", "started", 2);
    Fx {
        repo,
        tenant,
        board,
        triage,
        todo,
    }
}

impl Fx {
    async fn task(
        &self,
        title: &str,
        column: ColumnId,
        type_: &str,
        visibility: &str,
        created_by: Option<UserId>,
        labels: &[&str],
    ) -> TaskId {
        self.repo
            .create_task(NewTask {
                tenant: self.tenant,
                board: self.board,
                column_id: column,
                title: title.into(),
                description: None,
                position: 0,
                workspace_id: None,
                priority: 0,
                type_: type_.into(),
                visibility: visibility.into(),
                created_by: created_by.map(|u| u.0),
                parent_task_id: None,
                labels: labels.iter().map(|s| s.to_string()).collect(),
            })
            .await
            .unwrap()
            .id
    }

    fn pick(&self) -> PickParams {
        PickParams {
            limit: 50,
            ..Default::default()
        }
    }
}

/// Two agents racing to claim the same task: exactly one wins. The real
/// statement's `WHERE … assignee_user_id IS NULL` is what guarantees it, and the
/// fake tests the same condition inside the same critical section.
#[tokio::test]
async fn only_one_claimant_can_win() {
    let f = fx();
    let id = f.task("contended", f.todo, "task", "team", None, &[]).await;
    let (a, b) = (UserId::new(), UserId::new());

    let first = f
        .repo
        .claim_task(id, f.tenant, a, None, None)
        .await
        .unwrap();
    assert!(first.is_some(), "the first claim takes it");

    let second = f
        .repo
        .claim_task(id, f.tenant, b, None, None)
        .await
        .unwrap();
    assert!(
        second.is_none(),
        "the second matches no row — a 409, not a steal"
    );

    assert_eq!(
        f.repo.assignee_of(id, f.tenant).await.unwrap(),
        Some(Some(a)),
        "and the winner keeps it"
    );

    // Releasing puts it back in the pool, and the loser can then take it.
    f.repo.release_assignment(id, f.tenant).await.unwrap();
    assert!(f
        .repo
        .claim_task(id, f.tenant, b, None, None)
        .await
        .unwrap()
        .is_some());
}

/// A claim can move the task in the same statement. The column is optional —
/// omitting it must leave the card where it is.
#[tokio::test]
async fn claiming_moves_the_task_only_when_asked() {
    let f = fx();
    let id = f.task("t", f.todo, "task", "team", None, &[]).await;

    f.repo
        .claim_task(id, f.tenant, UserId::new(), None, None)
        .await
        .unwrap();
    assert_eq!(
        f.repo
            .get_row(f.tenant, id)
            .await
            .unwrap()
            .unwrap()
            .column_id,
        f.todo,
        "no column given, no move"
    );

    f.repo.release_assignment(id, f.tenant).await.unwrap();
    let started = f
        .repo
        .column_of_type(f.board, "started")
        .await
        .unwrap()
        .unwrap();
    f.repo
        .claim_task(id, f.tenant, UserId::new(), Some(started), None)
        .await
        .unwrap();
    assert_eq!(
        f.repo
            .get_row(f.tenant, id)
            .await
            .unwrap()
            .unwrap()
            .column_id,
        started
    );
}

/// The loop never draws from the backlog, and never picks an epic. Both are the
/// MAIN-80 exclusions, and both are in the pick query rather than in a caller.
#[tokio::test]
async fn the_pick_skips_the_backlog_and_epics() {
    let f = fx();
    let on_board = f
        .task("board work", f.todo, "task", "team", None, &[])
        .await;
    let _in_triage = f
        .task("triage work", f.triage, "task", "team", None, &[])
        .await;
    let _epic = f.task("an epic", f.todo, "epic", "team", None, &[]).await;

    let got = f
        .repo
        .pick_tasks(f.tenant, UserId::new(), f.pick())
        .await
        .unwrap();
    assert_eq!(
        got.iter().map(|t| t.id).collect::<Vec<_>>(),
        vec![on_board],
        "neither the backlog card nor the epic is pickable"
    );

    // …but both are reachable when explicitly asked for.
    let with_backlog = f
        .repo
        .pick_tasks(
            f.tenant,
            UserId::new(),
            PickParams {
                backlog: true,
                ..f.pick()
            },
        )
        .await
        .unwrap();
    assert_eq!(with_backlog.len(), 2, "backlog=true lifts the exclusion");

    let epics = f
        .repo
        .pick_tasks(
            f.tenant,
            UserId::new(),
            PickParams {
                types: vec!["epic".into()],
                ..f.pick()
            },
        )
        .await
        .unwrap();
    assert_eq!(epics.len(), 1, "type=epic surfaces epics on purpose");
}

/// A private card is invisible to anyone but its creator or assignee — the same
/// predicate the claim path enforces, so the list never shows work that could
/// not then be started.
#[tokio::test]
async fn the_pick_hides_other_peoples_private_cards() {
    let f = fx();
    let owner = UserId::new();
    let stranger = UserId::new();
    let _secret = f
        .task("secret", f.todo, "task", "private", Some(owner), &[])
        .await;
    let shared = f.task("shared", f.todo, "task", "team", None, &[]).await;

    let seen: Vec<TaskId> = f
        .repo
        .pick_tasks(f.tenant, stranger, f.pick())
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(seen, vec![shared], "a stranger sees only the shared card");

    assert_eq!(
        f.repo
            .pick_tasks(f.tenant, owner, f.pick())
            .await
            .unwrap()
            .len(),
        2,
        "its owner sees both"
    );
}

/// The explicit visibility filter can only NARROW the viewer predicate — asking
/// for `private` still shows only your own, never a teammate's.
#[tokio::test]
async fn an_explicit_visibility_filter_cannot_widen_the_view() {
    let f = fx();
    let owner = UserId::new();
    let stranger = UserId::new();
    f.task("mine", f.todo, "task", "private", Some(owner), &[])
        .await;

    let got = f
        .repo
        .pick_tasks(
            f.tenant,
            stranger,
            PickParams {
                visibility: vec!["private".into()],
                ..f.pick()
            },
        )
        .await
        .unwrap();
    assert!(
        got.is_empty(),
        "asking for private does not reveal someone else's"
    );

    let mine = f
        .repo
        .pick_tasks(
            f.tenant,
            owner,
            PickParams {
                visibility: vec!["private".into()],
                ..f.pick()
            },
        )
        .await
        .unwrap();
    assert_eq!(mine.len(), 1);
}

/// Label filters: every required label must be present, and none of the
/// excluded ones — the exact combination the loop's pick uses.
#[tokio::test]
async fn label_filters_require_all_and_exclude_any() {
    let f = fx();
    let ready = f
        .task("ready", f.todo, "task", "team", None, &["agent-ready"])
        .await;
    let _blocked = f
        .task(
            "blocked",
            f.todo,
            "task",
            "team",
            None,
            &["agent-ready", "blocked"],
        )
        .await;
    let _plain = f.task("plain", f.todo, "task", "team", None, &[]).await;

    let got: Vec<TaskId> = f
        .repo
        .pick_tasks(
            f.tenant,
            UserId::new(),
            PickParams {
                labels: vec!["agent-ready".into()],
                not_labels: vec!["blocked".into()],
                ..f.pick()
            },
        )
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(got, vec![ready], "the loop's own pick, in one assertion");
}

/// Priority 0 means "unset" and sorts last rather than first.
#[tokio::test]
async fn unset_priority_sorts_last() {
    let f = fx();
    let unset = f.task("unset", f.todo, "task", "team", None, &[]).await;
    let urgent = f.task("urgent", f.todo, "task", "team", None, &[]).await;
    f.repo.set_priority(f.tenant, urgent, 1).await.unwrap();

    let order: Vec<TaskId> = f
        .repo
        .pick_tasks(f.tenant, UserId::new(), f.pick())
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(order, vec![urgent, unset], "priority 1 before unset");
}

/// Archived work is off the board and never pickable unless asked for.
#[tokio::test]
async fn archived_work_leaves_the_pick() {
    let f = fx();
    let id = f.task("done", f.todo, "task", "team", None, &[]).await;

    f.repo.set_archived(id, f.tenant, true).await.unwrap();
    assert!(f
        .repo
        .pick_tasks(f.tenant, UserId::new(), f.pick())
        .await
        .unwrap()
        .is_empty());

    let with_archived = f
        .repo
        .pick_tasks(
            f.tenant,
            UserId::new(),
            PickParams {
                archived: true,
                ..f.pick()
            },
        )
        .await
        .unwrap();
    assert_eq!(with_archived.len(), 1);

    // …and unarchiving puts it back.
    f.repo.set_archived(id, f.tenant, false).await.unwrap();
    assert_eq!(
        f.repo
            .pick_tasks(f.tenant, UserId::new(), f.pick())
            .await
            .unwrap()
            .len(),
        1
    );
}

/// A `blocks` cycle is a deadlock nothing can ever pick up, so the relation
/// route refuses one. The reachability walk must also terminate on a cycle that
/// already exists.
#[tokio::test]
async fn blocks_reachability_finds_a_cycle_and_terminates() {
    let f = fx();
    let a = f.task("a", f.todo, "task", "team", None, &[]).await;
    let b = f.task("b", f.todo, "task", "team", None, &[]).await;
    let c = f.task("c", f.todo, "task", "team", None, &[]).await;

    f.repo
        .upsert_relation(f.tenant, a, b, "blocks")
        .await
        .unwrap();
    f.repo
        .upsert_relation(f.tenant, b, c, "blocks")
        .await
        .unwrap();

    assert!(f.repo.blocks_reaches(a, c).await.unwrap(), "transitively");
    assert!(!f.repo.blocks_reaches(c, a).await.unwrap(), "not backwards");

    // Close the ring anyway, then confirm the walk still returns.
    f.repo
        .upsert_relation(f.tenant, c, a, "blocks")
        .await
        .unwrap();
    assert!(f.repo.blocks_reaches(a, a).await.unwrap());
}

/// Attaching a label reports whether anything changed, so an agent re-applying
/// one on every poll does not flood the activity timeline a human reads.
#[tokio::test]
async fn re_attaching_a_label_reports_no_change() {
    let f = fx();
    let id = f.task("t", f.todo, "task", "team", None, &[]).await;
    let label = f
        .repo
        .upsert_label(f.tenant, "agent-ready", "#f0a000")
        .await
        .unwrap();

    assert_eq!(f.repo.attach_label_id(id, label.id).await.unwrap(), 1);
    assert_eq!(
        f.repo.attach_label_id(id, label.id).await.unwrap(),
        0,
        "already attached — the caller records no event"
    );
    assert_eq!(f.repo.detach_label_id(id, label.id).await.unwrap(), 1);
    assert_eq!(f.repo.detach_label_id(id, label.id).await.unwrap(), 0);
}

/// Deleting a label removes it from the vocabulary and detaches it — but must
/// not delete anybody's work.
#[tokio::test]
async fn deleting_a_label_keeps_the_tasks() {
    let f = fx();
    let id = f.task("t", f.todo, "task", "team", None, &["wip"]).await;
    let label = f.repo.upsert_label(f.tenant, "wip", "#888").await.unwrap();

    assert_eq!(f.repo.delete_label(label.id, f.tenant).await.unwrap(), 1);
    assert!(f.repo.list_labels(f.tenant).await.unwrap().is_empty());
    assert!(
        f.repo.get_row(f.tenant, id).await.unwrap().is_some(),
        "removing a label from the vocabulary must not delete the work"
    );
    assert!(f.repo.labels_of_task(id).await.unwrap().is_empty());
}

/// Archiving a whole column reports what actually moved, so the caller
/// publishes one change per card and no more.
#[tokio::test]
async fn bulk_archive_reports_only_what_moved() {
    let f = fx();
    let a = f.task("a", f.todo, "task", "team", None, &[]).await;
    let b = f.task("b", f.todo, "task", "team", None, &[]).await;
    f.repo.set_archived(a, f.tenant, true).await.unwrap();

    let moved = f
        .repo
        .archive_all_in_column(f.todo, f.tenant)
        .await
        .unwrap();
    assert_eq!(
        moved,
        vec![b],
        "the already-archived card is not moved again"
    );

    let again = f
        .repo
        .archive_all_in_column(f.todo, f.tenant)
        .await
        .unwrap();
    assert!(again.is_empty(), "and a second run moves nothing");
}

/// Board keys are unique per tenant — the check the key generator loops on.
#[tokio::test]
async fn board_keys_are_taken_per_tenant() {
    let f = fx();
    assert!(f.repo.board_key_taken(f.tenant, "MAIN").await.unwrap());
    assert!(!f.repo.board_key_taken(f.tenant, "OPS").await.unwrap());
    assert!(
        !f.repo
            .board_key_taken(TenantId::new(), "MAIN")
            .await
            .unwrap(),
        "another tenant may use the same key"
    );
}

/// Only the author may edit or delete a comment, which the route decides from
/// the author the repository reports.
#[tokio::test]
async fn a_comments_author_is_reported_for_the_ownership_check() {
    use nook_control::repo::tasks::NewComment;
    let f = fx();
    let id = f.task("t", f.todo, "task", "team", None, &[]).await;
    let me = UserId::new();

    let c = f
        .repo
        .create_comment(NewComment {
            tenant: f.tenant,
            task: id,
            author_type: "user".into(),
            author_id: Some(me.0),
            author_name: "me".into(),
            body_md: "hello".into(),
        })
        .await
        .unwrap();

    assert_eq!(
        f.repo.comment_author(c.id, f.tenant).await.unwrap(),
        Some((Some(me.0), id))
    );
    assert_eq!(
        f.repo.comment_author(c.id, TenantId::new()).await.unwrap(),
        None,
        "and it is tenant-scoped"
    );

    f.repo.delete_comment(c.id, f.tenant).await.unwrap();
    assert!(f.repo.comments_of(id).await.unwrap().is_empty());
}

/// Column administration is scoped through the board's tenant, because
/// `board_columns` has no `tenant_id` of its own. A caller in another tenant
/// must not be able to rename or delete somebody's column by id.
#[tokio::test]
async fn column_administration_is_scoped_through_the_board() {
    let f = fx();
    let other = TenantId::new();

    assert!(f.repo.board_in_tenant(f.board, f.tenant).await.unwrap());
    assert!(
        !f.repo.board_in_tenant(f.board, other).await.unwrap(),
        "another tenant does not own this board"
    );

    // A cross-tenant rename matches nothing rather than succeeding.
    assert!(f
        .repo
        .update_column(f.todo, other, Some("Renamed".into()), None)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        f.repo.board_columns(f.board).await.unwrap()[1].name,
        "Todo",
        "and the column is untouched"
    );

    // The owner's rename lands, and COALESCE leaves the unmentioned field alone.
    let renamed = f
        .repo
        .update_column(f.todo, f.tenant, Some("Doing".into()), None)
        .await
        .unwrap()
        .expect("owner may rename");
    assert_eq!((renamed.name.as_str(), renamed.position), ("Doing", 1));

    // …and a cross-tenant delete removes nothing.
    assert_eq!(f.repo.delete_column(f.todo, other).await.unwrap(), 0);
    assert_eq!(f.repo.delete_column(f.todo, f.tenant).await.unwrap(), 1);
}

/// Deleting a column cascades its tasks (the schema's ON DELETE CASCADE), so a
/// caller cannot be left with rows pointing at a column that is gone.
#[tokio::test]
async fn deleting_a_column_takes_its_tasks_with_it() {
    let f = fx();
    let doomed = f
        .task("in the doomed column", f.todo, "task", "team", None, &[])
        .await;
    let kept = f
        .task("elsewhere", f.triage, "task", "team", None, &[])
        .await;

    assert_eq!(f.repo.delete_column(f.todo, f.tenant).await.unwrap(), 1);
    assert!(f.repo.get_row(f.tenant, doomed).await.unwrap().is_none());
    assert!(
        f.repo.get_row(f.tenant, kept).await.unwrap().is_some(),
        "a task in another column is untouched"
    );
}

/// A new column appends after the last one; the first on an empty board lands
/// at 0 (`max(position)` is NULL, and `-1 + 1` is the caller's arithmetic).
#[tokio::test]
async fn columns_append_after_the_last_position() {
    let f = fx();
    let bare = f.repo.with_board(f.tenant, "Bare", "BARE").id;

    assert_eq!(f.repo.max_column_position(bare).await.unwrap(), None);
    let first = f.repo.append_column(bare, "Todo", 0).await.unwrap();
    assert_eq!(first.position, 0);

    let max = f.repo.max_column_position(bare).await.unwrap();
    assert_eq!(max, Some(0));
    let second = f
        .repo
        .append_column(bare, "Doing", max.unwrap_or(-1) + 1)
        .await
        .unwrap();
    assert_eq!(second.position, 1);
}

/// Deleting a board and reading its provider are both tenant-scoped.
#[tokio::test]
async fn boards_are_deleted_and_read_within_their_tenant() {
    let f = fx();
    let other = TenantId::new();

    assert_eq!(
        f.repo
            .board_provider(f.board, f.tenant)
            .await
            .unwrap()
            .as_deref(),
        Some("local")
    );
    assert!(
        f.repo
            .board_provider(f.board, other)
            .await
            .unwrap()
            .is_none(),
        "another tenant cannot read the provider — the route turns this into 404"
    );

    assert_eq!(f.repo.delete_board(f.board, other).await.unwrap(), 0);
    assert_eq!(f.repo.delete_board(f.board, f.tenant).await.unwrap(), 1);
}
