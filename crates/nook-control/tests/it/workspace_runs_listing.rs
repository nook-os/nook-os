//! What a workspace's run listings SAY, and how far they reach (MAIN-557).
//!
//! Two questions, and only a database answers either. The fields are joins and
//! a derivation — a card's branch, a requester's display name, the head out of
//! a repair fingerprint — so an in-memory fake proves nothing about them. And
//! the paging is a keyset: whether a walk drops or repeats a row under a
//! concurrent insert is a property of the WHERE clause, not of the wrapper.
//!
//! Engine-neutral by construction, so both legs run it: every value binds
//! through `params!`, there is no interval arithmetic and no JSON, and every
//! timestamp is whole seconds — SQLite stores a `timestamptz` as text at
//! millisecond resolution (MAIN-442), and a sub-second literal would compare
//! differently on the two engines.
//!
//! Needs a database: `NOOK_REQUIRE_DB=1` in the suite.

use chrono::{DateTime, TimeZone, Utc};
use nook_db::{params, Db};
use nook_types::*;

use nook_testkit::{first_page, page_after, TestBed};

/// A board and a column for cards to live in. One per test: the cards below
/// differ by the run that owns them, never by where they sit.
async fn board(bed: &TestBed, tenant: TenantId) -> (BoardId, ColumnId) {
    let board = BoardId::new();
    bed.db()
        .exec(
            "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b',$3,'local')",
            // The RANDOM tail of the v7 uuid: its leading bytes are a shared
            // timestamp, so two boards made in one test collide on a prefix.
            params![
                board,
                tenant,
                format!("B{}", &board.0.simple().to_string()[26..32]).to_uppercase()
            ],
        )
        .await
        .expect("board");
    let col = ColumnId::new();
    bed.db()
        .exec(
            "INSERT INTO board_columns (id, board_id, name, position, type)
             VALUES ($1,$2,'Triage',0,'unstarted')",
            params![col, board],
        )
        .await
        .expect("column");
    (board, col)
}

/// A card, with the branch it records. Every build run needs one of its own:
/// `loop_jobs` requires a target for a non-review kind, and 0050 allows one
/// LIVE build run per card.
#[allow(clippy::too_many_arguments)]
async fn card(
    bed: &TestBed,
    tenant: TenantId,
    creator: UserId,
    workspace: WorkspaceId,
    board: BoardId,
    col: ColumnId,
    branch: Option<&str>,
) -> TaskId {
    let task = TaskId::new();
    bed.db()
        .exec(
            "INSERT INTO tasks
                (id, tenant_id, board_id, column_id, title, type, created_by,
                 workspace_id, branch)
             VALUES ($1,$2,$3,$4,'t','task',$5,$6,$7)",
            params![
                task,
                tenant,
                board,
                col,
                creator,
                workspace,
                branch.map(str::to_string)
            ],
        )
        .await
        .expect("task");
    task
}

/// One build run, with both timestamps and its fingerprint stated outright —
/// the three things every assertion below is about.
#[allow(clippy::too_many_arguments)]
async fn build_run(
    bed: &TestBed,
    tenant: TenantId,
    user: UserId,
    workspace: WorkspaceId,
    task: TaskId,
    fingerprint: Option<&str>,
    created: DateTime<Utc>,
    updated: DateTime<Utc>,
) -> JobId {
    let id = JobId::new();
    bed.db()
        .exec(
            "INSERT INTO loop_jobs
                (id, tenant_id, kind, target_task_id, workspace_id, requested_by,
                 state, build_fingerprint, created_at, updated_at)
             VALUES ($1,$2,'build',$3,$4,$5,'queued',$6,$7,$8)",
            params![
                id,
                tenant,
                task,
                workspace,
                user,
                fingerprint.map(str::to_string),
                created,
                updated
            ],
        )
        .await
        .expect("build run");
    id
}

async fn review_run(
    bed: &TestBed,
    tenant: TenantId,
    user: UserId,
    workspace: WorkspaceId,
    pr: i64,
    head: &str,
) -> JobId {
    let id = JobId::new();
    bed.db()
        .exec(
            "INSERT INTO loop_jobs
                (id, tenant_id, kind, workspace_id, requested_by, state,
                 review_pr_number, review_head_sha)
             VALUES ($1,$2,'review',$3,$4,'completed',$5,$6)",
            params![id, tenant, workspace, user, pr, head.to_string()],
        )
        .await
        .expect("review run");
    id
}

fn at(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_770_000_000 + secs, 0).unwrap()
}

/// AC-1/AC-2/AC-3/AC-4 on the build listing, in one row each: the two shapes a
/// build's commit comes in, and the two shapes its branch comes in.
#[tokio::test]
async fn a_build_row_carries_updated_at_branch_initiator_and_commit() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("runs-fields").await;
    let (user, _person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    let (b, c) = board(&bed, tenant).await;
    let branched = card(&bed, tenant, user, ws, b, c, Some("main-557-runs")).await;
    let unbranched = card(&bed, tenant, user, ws, b, c, None).await;
    // A repair run is raised AT the head its verdict was written for; a fresh
    // one is raised at a fingerprint of the card's contract, which is not a
    // commit at all.
    let repair = build_run(
        &bed,
        tenant,
        user,
        ws,
        branched,
        Some("repair:9f1c0de"),
        at(0),
        at(300),
    )
    .await;
    let fresh = build_run(
        &bed,
        tenant,
        user,
        ws,
        unbranched,
        Some("2f0a5b6c-1111-4222-8333-444455556666"),
        at(10),
        at(10),
    )
    .await;

    let rows = state
        .jobs
        .list_builds_for_workspace(tenant, user, ws, &first_page(50))
        .await
        .expect("list")
        .rows;

    let repaired = rows.iter().find(|r| r.id == repair.0).expect("repair row");
    assert_eq!(repaired.created_at, at(0));
    assert_eq!(
        repaired.updated_at,
        at(300),
        "AC-1: the row's last activity, which is what elapsed time is measured from"
    );
    assert_eq!(
        repaired.branch.as_deref(),
        Some("main-557-runs"),
        "AC-2: the branch its card records"
    );
    assert_eq!(
        repaired.initiator.as_deref(),
        Some("U"),
        "AC-3: a display name, not the requester's uuid"
    );
    assert_eq!(
        repaired.commit_sha.as_deref(),
        Some("9f1c0de"),
        "AC-4: the head the repair was raised at"
    );

    let first = rows.iter().find(|r| r.id == fresh.0).expect("fresh row");
    assert_eq!(
        first.branch, None,
        "AC-2: a card with no branch recorded reports none rather than a guess"
    );
    assert_eq!(
        first.commit_sha, None,
        "AC-4: build_fingerprint is not a sha and must not be served as one"
    );
    assert_eq!(first.updated_at, at(10));

    bed.teardown().await;
}

/// AC-1/AC-2/AC-3 on the review listing, and AC-7's other half: the row is
/// still a whole `LoopJob`, so nothing that read one before reads less now.
#[tokio::test]
async fn a_review_row_keeps_its_job_and_gains_the_new_fields() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("runs-review-fields").await;
    let (user, _person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    let run = review_run(&bed, tenant, user, ws, 341, "abcdef1234567890").await;

    let rows = state
        .jobs
        .list_reviews_for_workspace(tenant, ws, &first_page(50))
        .await
        .expect("list")
        .rows;
    let row = rows.iter().find(|r| r.job.id == run).expect("the run");

    assert_eq!(row.job.review_pr_number, Some(341));
    assert_eq!(row.job.review_head_sha.as_deref(), Some("abcdef1234567890"));
    assert!(
        row.job.updated_at >= row.job.created_at,
        "AC-1: the job's own last movement rides along"
    );
    assert_eq!(row.initiator.as_deref(), Some("U"), "AC-3");
    assert_eq!(
        row.branch, None,
        "AC-2: no head ref is recorded on this side, so the field is null \
         rather than derived from the PR number by a forge call per row"
    );

    bed.teardown().await;
}

/// AC-5: a full walk visits every run exactly once and stops.
///
/// The rows deliberately SHARE a `created_at`, which is AC-6's case: the sort
/// key the cursor encodes is the run's id, so rows indistinguishable by time
/// still have one total order to page through.
#[tokio::test]
async fn paging_walks_the_whole_set_with_no_duplicate_or_skip() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("runs-walk").await;
    let (user, _person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    let (b, c) = board(&bed, tenant).await;
    let mut raised = Vec::new();
    for _ in 0..7 {
        let task = card(&bed, tenant, user, ws, b, c, None).await;
        raised.push(build_run(&bed, tenant, user, ws, task, None, at(0), at(0)).await);
    }

    let mut seen: Vec<uuid::Uuid> = Vec::new();
    let mut args = first_page(3);
    let mut pages = 0;
    loop {
        let page = state
            .jobs
            .list_builds_for_workspace(tenant, user, ws, &args)
            .await
            .expect("page");
        pages += 1;
        assert!(pages <= 10, "the walk must terminate, not spin");
        seen.extend(page.rows.iter().map(|r| r.id));
        match page.next_cursor {
            // Opaque by contract: handed back verbatim, never parsed.
            Some(token) => args = page_after(&token, 3),
            None => break,
        }
    }

    assert_eq!(
        pages, 3,
        "7 rows at 3 a page: two full pages and a short one"
    );
    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), seen.len(), "no run appeared twice");
    let mut expected: Vec<uuid::Uuid> = raised.iter().map(|j| j.0).collect();
    expected.sort();
    assert_eq!(unique, expected, "every run appeared");

    bed.teardown().await;
}

/// AC-5's hard half: a run raised BETWEEN two page fetches neither duplicates
/// a row already returned nor pushes an unseen one past the cursor.
///
/// The newcomer is newest, so it belongs ahead of the first page — a place the
/// walk has already passed. Seeing it would mean the walk went backwards;
/// losing one of the five would mean the cursor drifted.
#[tokio::test]
async fn a_run_raised_mid_walk_neither_duplicates_nor_skips() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("runs-midwalk").await;
    let (user, _person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    let (b, c) = board(&bed, tenant).await;
    let mut raised = Vec::new();
    for _ in 0..5 {
        let task = card(&bed, tenant, user, ws, b, c, None).await;
        raised.push(build_run(&bed, tenant, user, ws, task, None, at(0), at(0)).await);
    }

    let first = state
        .jobs
        .list_builds_for_workspace(tenant, user, ws, &first_page(2))
        .await
        .expect("first page");
    let cursor = first.next_cursor.clone().expect("a full page continues");

    let late_card = card(&bed, tenant, user, ws, b, c, None).await;
    let intruder = build_run(&bed, tenant, user, ws, late_card, None, at(60), at(60)).await;

    let mut seen: Vec<uuid::Uuid> = first.rows.iter().map(|r| r.id).collect();
    let mut args = page_after(&cursor, 2);
    loop {
        let page = state
            .jobs
            .list_builds_for_workspace(tenant, user, ws, &args)
            .await
            .expect("page");
        seen.extend(page.rows.iter().map(|r| r.id));
        match page.next_cursor {
            Some(token) => args = page_after(&token, 2),
            None => break,
        }
    }

    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), seen.len(), "no run appeared twice");
    let mut expected: Vec<uuid::Uuid> = raised.iter().map(|j| j.0).collect();
    expected.sort();
    assert_eq!(
        unique, expected,
        "the walk saw the five it started with — all of them, and only them"
    );
    assert!(
        !seen.contains(&intruder.0),
        "the newcomer sorts ahead of a page already passed; it is the NEXT \
         walk's row, not a duplicate injected into this one"
    );

    // And it is not lost: a fresh walk starts at it.
    let restart = state
        .jobs
        .list_builds_for_workspace(tenant, user, ws, &first_page(2))
        .await
        .expect("restart");
    assert_eq!(restart.rows[0].id, intruder.0, "newest first");

    bed.teardown().await;
}

/// AC-7: no cursor is the first page, newest first — what every caller that
/// never passes one has always got.
#[tokio::test]
async fn no_cursor_is_the_first_page() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("runs-default").await;
    let (user, _person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    let (b, c) = board(&bed, tenant).await;
    let old_card = card(&bed, tenant, user, ws, b, c, None).await;
    let new_card = card(&bed, tenant, user, ws, b, c, None).await;
    let older = build_run(&bed, tenant, user, ws, old_card, None, at(0), at(0)).await;
    let newer = build_run(&bed, tenant, user, ws, new_card, None, at(60), at(60)).await;
    let review = review_run(&bed, tenant, user, ws, 7, "deadbeef").await;

    let builds = state
        .jobs
        .list_builds_for_workspace(tenant, user, ws, &first_page(50))
        .await
        .expect("builds");
    assert_eq!(
        builds.rows.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![newer.0, older.0],
        "newest first, both kinds' listings unchanged in order"
    );
    assert!(
        builds.next_cursor.is_none(),
        "a short page is the end of the list, not another round trip"
    );

    let reviews = state
        .jobs
        .list_reviews_for_workspace(tenant, ws, &first_page(50))
        .await
        .expect("reviews");
    assert_eq!(
        reviews.rows.iter().map(|r| r.job.id).collect::<Vec<_>>(),
        vec![review],
        "a build run is not a review run"
    );
    assert!(reviews.next_cursor.is_none());

    bed.teardown().await;
}

/// The sort key is the run's ID, not its `created_at` — stated by the one row
/// on which the two disagree.
///
/// Everything a running system inserts makes them agree: ids are UUID v7, so
/// id order IS creation order, and the listing read `created_at DESC` before
/// this card. A backdated row is the only way to say which key the contract now
/// names, and it is the key AC-6's cursor encodes — a walk that ordered by the
/// timestamp while paging on the id would drift the moment the two parted.
#[tokio::test]
async fn the_order_is_the_id_not_the_timestamp() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("runs-order").await;
    let (user, _person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    let (b, c) = board(&bed, tenant).await;
    let first_card = card(&bed, tenant, user, ws, b, c, None).await;
    let second_card = card(&bed, tenant, user, ws, b, c, None).await;
    // Raised first, so its id is smaller; stamped LATEST, so `created_at DESC`
    // would put it on top.
    let raised_first = build_run(&bed, tenant, user, ws, first_card, None, at(900), at(900)).await;
    let raised_second = build_run(&bed, tenant, user, ws, second_card, None, at(0), at(0)).await;

    let rows = state
        .jobs
        .list_builds_for_workspace(tenant, user, ws, &first_page(50))
        .await
        .expect("list")
        .rows;
    assert_eq!(
        rows.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![raised_second.0, raised_first.0],
        "the later-raised run leads, though it carries the older timestamp"
    );

    // And the cursor agrees with that order rather than with the timestamps:
    // paging one at a time walks the same sequence.
    let page = state
        .jobs
        .list_builds_for_workspace(tenant, user, ws, &first_page(1))
        .await
        .expect("page one");
    assert_eq!(page.rows[0].id, raised_second.0);
    let rest = state
        .jobs
        .list_builds_for_workspace(
            tenant,
            user,
            ws,
            &page_after(&page.next_cursor.expect("a full page continues"), 1),
        )
        .await
        .expect("page two");
    assert_eq!(rest.rows[0].id, raised_first.0);

    bed.teardown().await;
}

/// The repair prefix is matched case-SENSITIVELY, identically on both engines.
///
/// This is the one spelling the two disagree on: `LIKE 'repair:%'` is
/// case-sensitive on Postgres and case-insensitive on SQLite, so a `Repair:`
/// fingerprint would yield a commit on one engine and null on the other — from
/// the same row, in a file whose rule is SQL both engines run identically.
/// Nothing writes a differently-cased fingerprint today; this is what keeps the
/// answer from depending on which database is underneath if one ever does.
#[tokio::test]
async fn a_miscased_repair_prefix_is_not_a_commit_on_either_engine() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("runs-case").await;
    let (user, _person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    let (b, c) = board(&bed, tenant).await;
    let lower = card(&bed, tenant, user, ws, b, c, None).await;
    let upper = card(&bed, tenant, user, ws, b, c, None).await;
    let real = build_run(
        &bed,
        tenant,
        user,
        ws,
        lower,
        Some("repair:9f1c0de"),
        at(0),
        at(0),
    )
    .await;
    let miscased = build_run(
        &bed,
        tenant,
        user,
        ws,
        upper,
        Some("Repair:9f1c0de"),
        at(0),
        at(0),
    )
    .await;

    let rows = state
        .jobs
        .list_builds_for_workspace(tenant, user, ws, &first_page(50))
        .await
        .expect("list")
        .rows;
    let of = |id: JobId| {
        rows.iter()
            .find(|r| r.id == id.0)
            .expect("listed")
            .commit_sha
            .clone()
    };
    assert_eq!(of(real).as_deref(), Some("9f1c0de"));
    assert_eq!(
        of(miscased),
        None,
        "only the exact prefix the loop writes is a repair fingerprint"
    );

    bed.teardown().await;
}
