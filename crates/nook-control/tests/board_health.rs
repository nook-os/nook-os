//! The board-health report (MAIN-570): four exact rules over board state, each
//! asserted against rows this test creates and never a global count.
//!
//! Driven through the real route handler, so the tenant gate and the card
//! visibility scope are exercised alongside the SQL. Set `DATABASE_URL`.
//!
//! Engine-neutral by construction — every insert binds through `params!`, there
//! is no interval arithmetic and no JSON, so it runs on the SQLite leg too.

use axum::extract::{Path, State};
use nook_control::auth::{AuthCtx, Principal};
use nook_control::routes::boards::board_health;
use nook_db::{params, Db, DbPool};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

/// The columns every check needs a card to sit in.
struct Fixture {
    tenant: TenantId,
    board: BoardId,
    backlog: ColumnId,
    todo: ColumnId,
    done: ColumnId,
    canceled: ColumnId,
    /// Bumped per insert so keys are stable and ordering is deterministic.
    next: std::cell::Cell<i32>,
}

async fn fixture(db: &DbPool, tenant: TenantId) -> Fixture {
    let board = BoardId(Uuid::now_v7());
    db.exec(
        "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b',$3,'local')",
        params![
            board,
            tenant,
            // The random tail, not the shared v7 timestamp prefix, so two boards
            // in one test do not collide on the key.
            format!("B{}", &board.0.simple().to_string()[26..]).to_uppercase()
        ],
    )
    .await
    .expect("board");
    let (backlog, todo, done, canceled) = (
        ColumnId(Uuid::now_v7()),
        ColumnId(Uuid::now_v7()),
        ColumnId(Uuid::now_v7()),
        ColumnId(Uuid::now_v7()),
    );
    db.exec(
        "INSERT INTO board_columns (id, board_id, name, position, type)
         VALUES ($1,$2,'Triage',0,'backlog'), ($3,$2,'Todo',1,'unstarted'),
                ($4,$2,'Done',2,'completed'), ($5,$2,'Canceled',3,'canceled')",
        params![backlog, board, todo, done, canceled],
    )
    .await
    .expect("columns");
    Fixture {
        tenant,
        board,
        backlog,
        todo,
        done,
        canceled,
        next: std::cell::Cell::new(1),
    }
}

/// A card, described by the only four things any check reads.
#[derive(Default, Clone, Copy)]
struct Card {
    archived: bool,
    epic: bool,
    parent: Option<TaskId>,
    /// `None` = a team card everybody sees; `Some(owner)` = private to `owner`.
    private_to: Option<UserId>,
}

async fn card(db: &DbPool, f: &Fixture, col: ColumnId, c: Card) -> TaskId {
    let id = TaskId::new();
    let number = f.next.get();
    f.next.set(number + 1);
    db.exec(
        "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, number, type,
                            archived_at, parent_task_id, visibility, created_by)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)",
        params![
            id,
            f.tenant,
            f.board,
            col,
            format!("card {number}"),
            number,
            if c.epic { "epic" } else { "task" }.to_string(),
            c.archived.then(chrono::Utc::now),
            c.parent.map(|p| p.0),
            if c.private_to.is_some() {
                "private"
            } else {
                "team"
            }
            .to_string(),
            c.private_to.map(|u| u.0)
        ],
    )
    .await
    .expect("task");
    id
}

async fn label(db: &DbPool, f: &Fixture, task: TaskId, name: &str) {
    // One label row per tenant; a second attach reuses it.
    let existing: Option<Uuid> = db
        .query_scalar_opt(
            "SELECT id FROM labels WHERE tenant_id = $1 AND name = $2",
            params![f.tenant, name],
        )
        .await
        .expect("label lookup");
    let label = match existing {
        Some(id) => id,
        None => {
            let id = Uuid::now_v7();
            db.exec(
                "INSERT INTO labels (id, tenant_id, name, color) VALUES ($1,$2,$3,'#3fb950')",
                params![id, f.tenant, name],
            )
            .await
            .expect("label");
            id
        }
    };
    db.exec(
        "INSERT INTO task_labels (task_id, label_id) VALUES ($1,$2)",
        params![task, label],
    )
    .await
    .expect("task_label");
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

/// The report as the endpoint returns it, indexed by check.
async fn report(
    bed: &TestBed,
    f: &Fixture,
    viewer: UserId,
) -> std::collections::HashMap<BoardHealthCheckKind, BoardHealthCheck> {
    let state = bed.app_state().await;
    let out = board_health(State(state), ctx(viewer, f.tenant), Path(f.board))
        .await
        .expect("health")
        .0;
    assert_eq!(out.board_id, f.board);
    // Every check is present, including the ones that found nothing (AC-7).
    assert_eq!(out.checks.len(), BoardHealthCheckKind::ALL.len());
    out.checks.into_iter().map(|c| (c.check, c)).collect()
}

/// The ids a check found — and, in the same breath, that its count agrees.
fn found(
    report: &std::collections::HashMap<BoardHealthCheckKind, BoardHealthCheck>,
    check: BoardHealthCheckKind,
) -> std::collections::HashSet<TaskId> {
    let c = report.get(&check).expect("check present");
    assert_eq!(
        c.count,
        c.tasks.len() as i64,
        "{check:?} count disagrees with its ids"
    );
    c.tasks.iter().map(|t| t.id).collect()
}

#[tokio::test]
async fn archived_not_done_finds_work_archived_while_unfinished() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("bh-and").await;
    let (viewer, _p) = bed.user(tenant, "member").await;
    let db = bed.db();
    let f = fixture(&db, tenant).await;

    let lost = card(
        &db,
        &f,
        f.todo,
        Card {
            archived: true,
            ..Default::default()
        },
    )
    .await;
    // Archived AND finished is ordinary cleanup, not a lie.
    let tidied = card(
        &db,
        &f,
        f.done,
        Card {
            archived: true,
            ..Default::default()
        },
    )
    .await;
    let canceled = card(
        &db,
        &f,
        f.canceled,
        Card {
            archived: true,
            ..Default::default()
        },
    )
    .await;
    let live = card(&db, &f, f.todo, Card::default()).await;

    let r = report(&bed, &f, viewer).await;
    let ids = found(&r, BoardHealthCheckKind::ArchivedNotDone);
    assert!(
        ids.contains(&lost),
        "an archived Todo card is the lost class"
    );
    assert!(!ids.contains(&tidied));
    assert!(!ids.contains(&canceled));
    assert!(!ids.contains(&live));
    assert_eq!(ids.len(), 1);

    // The key travels with the id — a report of bare uuids names nothing (AC-1).
    let keys: Vec<_> = r[&BoardHealthCheckKind::ArchivedNotDone]
        .tasks
        .iter()
        .map(|t| t.key.clone())
        .collect();
    assert!(
        keys.iter()
            .all(|k| k.as_deref().is_some_and(|k| k.contains('-'))),
        "expected board keys, got {keys:?}"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn done_agent_ready_finds_finished_cards_still_labelled_ready() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("bh-dar").await;
    let (viewer, _p) = bed.user(tenant, "member").await;
    let db = bed.db();
    let f = fixture(&db, tenant).await;

    let merged = card(&db, &f, f.done, Card::default()).await;
    label(&db, &f, merged, "agent-ready").await;
    // Archived cards are INCLUDED here (AC-3) — MAIN-154 and MAIN-537 are both.
    let merged_and_archived = card(
        &db,
        &f,
        f.done,
        Card {
            archived: true,
            ..Default::default()
        },
    )
    .await;
    label(&db, &f, merged_and_archived, "agent-ready").await;
    let clean = card(&db, &f, f.done, Card::default()).await;
    let waiting = card(&db, &f, f.todo, Card::default()).await;
    label(&db, &f, waiting, "agent-ready").await;
    // A different label on a Done card is not this check.
    let other = card(&db, &f, f.done, Card::default()).await;
    label(&db, &f, other, "blocked").await;

    let ids = found(
        &report(&bed, &f, viewer).await,
        BoardHealthCheckKind::DoneAgentReady,
    );
    assert!(ids.contains(&merged));
    assert!(ids.contains(&merged_and_archived));
    assert!(!ids.contains(&clean));
    assert!(!ids.contains(&waiting), "a Todo card is still workable");
    assert!(!ids.contains(&other));
    assert_eq!(ids.len(), 2);

    bed.teardown().await;
}

#[tokio::test]
async fn epics_closeable_counts_canceled_as_finished_and_ignores_archived_children() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("bh-close").await;
    let (viewer, _p) = bed.user(tenant, "member").await;
    let db = bed.db();
    let f = fixture(&db, tenant).await;

    let epic = Card {
        epic: true,
        ..Default::default()
    };
    let child = |parent: TaskId, archived: bool| Card {
        archived,
        parent: Some(parent),
        ..Default::default()
    };

    let all_done = card(&db, &f, f.backlog, epic).await;
    card(&db, &f, f.done, child(all_done, false)).await;
    card(&db, &f, f.done, child(all_done, false)).await;

    // Canceled counts as finished — there is nothing left to do either way, and
    // this is where the check deliberately parts from the backlog head's
    // client-side done/total (NG-8).
    let with_canceled = card(&db, &f, f.backlog, epic).await;
    card(&db, &f, f.done, child(with_canceled, false)).await;
    card(&db, &f, f.canceled, child(with_canceled, false)).await;

    // One live child holds it open.
    let still_open = card(&db, &f, f.backlog, epic).await;
    card(&db, &f, f.done, child(still_open, false)).await;
    card(&db, &f, f.todo, child(still_open, false)).await;

    // Its only open child was withdrawn, so nothing is left to do (AC-4).
    let open_child_archived = card(&db, &f, f.backlog, epic).await;
    card(&db, &f, f.done, child(open_child_archived, false)).await;
    card(&db, &f, f.todo, child(open_child_archived, true)).await;

    // Zero children is `epics_empty`, never this check.
    let childless = card(&db, &f, f.backlog, epic).await;

    let ids = found(
        &report(&bed, &f, viewer).await,
        BoardHealthCheckKind::EpicsCloseable,
    );
    assert!(ids.contains(&all_done));
    assert!(ids.contains(&with_canceled));
    assert!(!ids.contains(&still_open));
    assert!(ids.contains(&open_child_archived));
    assert!(!ids.contains(&childless), "an empty epic is not closeable");
    assert_eq!(ids.len(), 3);

    bed.teardown().await;
}

#[tokio::test]
async fn epics_empty_is_zero_non_archived_children() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("bh-empty").await;
    let (viewer, _p) = bed.user(tenant, "member").await;
    let db = bed.db();
    let f = fixture(&db, tenant).await;

    let none = card(
        &db,
        &f,
        f.backlog,
        Card {
            epic: true,
            ..Default::default()
        },
    )
    .await;

    let only_archived = card(
        &db,
        &f,
        f.backlog,
        Card {
            epic: true,
            ..Default::default()
        },
    )
    .await;
    card(
        &db,
        &f,
        f.todo,
        Card {
            archived: true,
            parent: Some(only_archived),
            ..Default::default()
        },
    )
    .await;

    let populated = card(
        &db,
        &f,
        f.backlog,
        Card {
            epic: true,
            ..Default::default()
        },
    )
    .await;
    card(
        &db,
        &f,
        f.todo,
        Card {
            parent: Some(populated),
            ..Default::default()
        },
    )
    .await;

    let ids = found(
        &report(&bed, &f, viewer).await,
        BoardHealthCheckKind::EpicsEmpty,
    );
    assert!(ids.contains(&none));
    assert!(ids.contains(&only_archived));
    assert!(!ids.contains(&populated));
    assert_eq!(ids.len(), 2);

    bed.teardown().await;
}

/// A second tenant's board is a different report — never a wider one.
#[tokio::test]
async fn the_report_is_scoped_to_its_own_tenant() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let a = bed.tenant("bh-a").await;
    let b = bed.tenant("bh-b").await;
    let (viewer_a, _p) = bed.user(a, "member").await;
    let (viewer_b, _p) = bed.user(b, "member").await;
    let db = bed.db();
    let fa = fixture(&db, a).await;
    let fb = fixture(&db, b).await;

    let mine = card(
        &db,
        &fa,
        fa.todo,
        Card {
            archived: true,
            ..Default::default()
        },
    )
    .await;
    let theirs = card(
        &db,
        &fb,
        fb.todo,
        Card {
            archived: true,
            ..Default::default()
        },
    )
    .await;

    let ra = found(
        &report(&bed, &fa, viewer_a).await,
        BoardHealthCheckKind::ArchivedNotDone,
    );
    assert_eq!(ra, std::collections::HashSet::from([mine]));
    let rb = found(
        &report(&bed, &fb, viewer_b).await,
        BoardHealthCheckKind::ArchivedNotDone,
    );
    assert_eq!(rb, std::collections::HashSet::from([theirs]));

    // And B's board is not readable from A at all.
    let state = bed.app_state().await;
    let refused = board_health(State(state), ctx(viewer_a, a), Path(fb.board)).await;
    assert!(
        matches!(refused, Err(nook_control::error::ApiError::NotFound)),
        "another tenant's board must not report"
    );

    bed.teardown().await;
}

/// A private card the viewer does not own is absent from the ids AND from the
/// count — a health check must not become a side-channel around MAIN-76.
#[tokio::test]
async fn a_private_card_is_absent_from_ids_and_from_the_count() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("bh-vis").await;
    let (viewer, _p) = bed.user(tenant, "member").await;
    let (owner, _p2) = bed.user(tenant, "member").await;
    let db = bed.db();
    let f = fixture(&db, tenant).await;

    let secret = card(
        &db,
        &f,
        f.done,
        Card {
            private_to: Some(owner),
            ..Default::default()
        },
    )
    .await;
    label(&db, &f, secret, "agent-ready").await;
    let shared = card(&db, &f, f.done, Card::default()).await;
    label(&db, &f, shared, "agent-ready").await;

    let stranger = found(
        &report(&bed, &f, viewer).await,
        BoardHealthCheckKind::DoneAgentReady,
    );
    assert_eq!(
        stranger,
        std::collections::HashSet::from([shared]),
        "a private card must not reach a stranger's report"
    );

    // Its owner sees both — the card is real, it was scoped, not dropped.
    let mine = found(
        &report(&bed, &f, owner).await,
        BoardHealthCheckKind::DoneAgentReady,
    );
    assert_eq!(mine.len(), 2);
    assert!(mine.contains(&secret));

    bed.teardown().await;
}

/// A board with nothing wrong still reports four checks, all zero — an all-zero
/// board reads as healthy rather than as a page that failed (AC-7).
#[tokio::test]
async fn a_healthy_board_reports_four_zeroes() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("bh-ok").await;
    let (viewer, _p) = bed.user(tenant, "member").await;
    let db = bed.db();
    let f = fixture(&db, tenant).await;
    card(&db, &f, f.todo, Card::default()).await;
    card(&db, &f, f.done, Card::default()).await;

    let r = report(&bed, &f, viewer).await;
    for check in BoardHealthCheckKind::ALL {
        assert_eq!(r[&check].count, 0, "{check:?} should be clean");
        assert!(r[&check].tasks.is_empty());
    }

    bed.teardown().await;
}
