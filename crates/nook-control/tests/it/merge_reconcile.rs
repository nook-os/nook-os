//! MAIN-491 end to end: the control plane moves a card to Done once its pull
//! request has merged, and never moves one it should not.
//!
//! Through the real sweep against a live database, with a fake forge whose
//! merged / closed-unmerged / error answers the test dictates. Every case is a
//! pair — what the first pass does, and that a second pass does nothing more —
//! because the whole design of this sweep is that it runs forever.
//!
//! Each test runs on its OWN private database (`nook_testkit::TestBed`).
//!
//! Needs Postgres: `DATABASE_URL` (`NOOK_REQUIRE_DB=1` in the suite).

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use nook_control::services::forge::{Forge, MergeState, MergedPr, PrDetails, PullRequest, Repo};
use nook_control::services::merge_reconcile;
use nook_control::state::AppState;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

const REMOTE: &str = "git@github.com:acme/api.git";

fn repo() -> Repo {
    Repo {
        owner: "acme".into(),
        name: "api".into(),
    }
}

/// A forge that answers from a script and counts how often it was asked.
///
/// `failing` is the outage: BOTH reads fail, because AC-7 is about the forge
/// being unreachable rather than about one endpoint being unlucky.
#[derive(Default)]
struct FakeForge {
    details: HashMap<u64, MergeState>,
    merged: Vec<MergedPr>,
    failing: bool,
    calls: Arc<AtomicUsize>,
}

impl FakeForge {
    fn ended(pr: u64, how: MergeState) -> Self {
        Self {
            details: HashMap::from([(pr, how)]),
            ..Default::default()
        }
    }

    fn down() -> Self {
        Self {
            failing: true,
            ..Default::default()
        }
    }

    fn merged_body(mut self, number: u64, body: &str) -> Self {
        self.merged.push(MergedPr {
            number,
            body: body.to_string(),
            merged_at: chrono::Utc::now() - chrono::Duration::hours(1),
        });
        self
    }
}

#[async_trait::async_trait]
impl Forge for FakeForge {
    async fn prs_needing_review(&self, _repo: &Repo) -> anyhow::Result<Vec<PullRequest>> {
        anyhow::bail!("the merge sweep must not ask for the review queue")
    }

    async fn pr_details(&self, _repo: &Repo, number: u64) -> anyhow::Result<PrDetails> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.failing {
            anyhow::bail!("503 upstream");
        }
        Ok(PrDetails {
            mergeable: Some(true),
            body: String::new(),
            merge_state: *self
                .details
                .get(&number)
                .unwrap_or_else(|| panic!("no answer staged for PR {number}")),
            head_sha: String::new(),
        })
    }

    async fn merged_prs_since(
        &self,
        _repo: &Repo,
        _since: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<Vec<MergedPr>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.failing {
            anyhow::bail!("503 upstream");
        }
        Ok(self.merged.clone())
    }
}

struct Fixture {
    tenant: TenantId,
    ws: WorkspaceId,
    board: BoardId,
    /// `(Todo, In Progress, In Review, Done)`.
    todo: ColumnId,
    review: ColumnId,
    done: ColumnId,
    key: String,
}

/// A tenant, a workspace pointing at a GitHub repo, and a four-column board.
async fn fixture(bed: &TestBed) -> Fixture {
    let tenant = bed.tenant("merge").await;
    let ws = bed.workspace(tenant).await;
    bed.db()
        .exec(
            "UPDATE workspaces SET git_remote_url = $2 WHERE id = $1",
            params![ws, REMOTE],
        )
        .await
        .expect("remote");

    let board = BoardId(Uuid::now_v7());
    let key = format!("B{}", &board.0.simple().to_string()[26..]).to_uppercase();
    bed.db()
        .exec(
            "INSERT INTO boards (id, tenant_id, name, key, provider)
             VALUES ($1, $2, 'b', $3, 'local')",
            params![board, tenant, key.clone()],
        )
        .await
        .expect("board");

    let mut cols = Vec::new();
    for (pos, (name, kind)) in [
        ("Todo", "unstarted"),
        ("In Progress", "started"),
        ("In Review", "review"),
        ("Done", "completed"),
    ]
    .iter()
    .enumerate()
    {
        let col = ColumnId::new();
        bed.db()
            .exec(
                "INSERT INTO board_columns (id, board_id, name, position, type)
                 VALUES ($1, $2, $3, $4, $5)",
                params![col, board, name.to_string(), pos as i32, kind.to_string()],
            )
            .await
            .expect("column");
        cols.push(col);
    }
    Fixture {
        tenant,
        ws,
        board,
        todo: cols[0],
        review: cols[2],
        done: cols[3],
        key,
    }
}

/// A card in `column`, with `pr_url` recorded or not. Returns `(id, KEY-N)`.
async fn card(
    bed: &TestBed,
    f: &Fixture,
    number: i32,
    column: ColumnId,
    pr_url: Option<&str>,
) -> (TaskId, String) {
    let id = TaskId::new();
    bed.db()
        .exec(
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, type,
                                number, workspace_id, pr_url)
             VALUES ($1, $2, $3, $4, 'c', 'task', $5, $6, $7)",
            params![
                id,
                f.tenant,
                f.board,
                column,
                number,
                f.ws,
                pr_url.map(str::to_string)
            ],
        )
        .await
        .expect("task");
    (id, format!("{}-{number}", f.key))
}

struct Card {
    column: ColumnId,
    pr_url: Option<String>,
    lease: Option<chrono::DateTime<chrono::Utc>>,
}

async fn read(bed: &TestBed, id: TaskId) -> Card {
    let row: (
        ColumnId,
        Option<String>,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = bed
        .db()
        .query_one(
            "SELECT column_id, pr_url, claim_expires_at FROM tasks WHERE id = $1",
            params![id],
        )
        .await
        .expect("card");
    Card {
        column: row.0,
        pr_url: row.1,
        lease: row.2,
    }
}

async fn comments(bed: &TestBed, id: TaskId) -> Vec<String> {
    bed.db()
        .query_scalar_all(
            "SELECT body_md FROM task_comments WHERE task_id = $1 ORDER BY created_at",
            params![id],
        )
        .await
        .expect("comments")
}

async fn labels(bed: &TestBed, id: TaskId) -> Vec<String> {
    bed.db()
        .query_scalar_all(
            "SELECT l.name FROM task_labels tl JOIN labels l ON l.id = tl.label_id
             WHERE tl.task_id = $1 ORDER BY l.name",
            params![id],
        )
        .await
        .expect("labels")
}

async fn sweep(
    state: &AppState,
    forge: &FakeForge,
    f: &Fixture,
) -> nook_control::error::ApiResult<merge_reconcile::Reconciled> {
    merge_reconcile::reconcile_workspace(state, forge, &repo(), f.tenant, f.ws).await
}

const PR_URL: &str = "https://github.com/acme/api/pull/7";

// ── AC-3 + AC-4: the merge, the move, and exactly one comment ──────────────

#[tokio::test]
async fn a_merged_card_moves_to_done_once_and_says_so_once() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    let (task, _) = card(&bed, &f, 1, f.review, Some(PR_URL)).await;
    // A claim lease, as start-work leaves one: AC-3 says a completed card holds
    // no worker, so the same write that moves it must clear this.
    bed.db()
        .exec(
            "UPDATE tasks SET claim_expires_at = $2 WHERE id = $1",
            params![task, chrono::Utc::now() + chrono::Duration::hours(1)],
        )
        .await
        .expect("lease");

    let forge = FakeForge::ended(7, MergeState::Merged);
    let first = sweep(&state, &forge, &f).await.expect("first pass");
    assert_eq!(first.moved, 1);

    let c = read(&bed, task).await;
    assert_eq!(c.column, f.done, "the card is in the completed column");
    assert_eq!(c.lease, None, "a completed card holds no worker");
    assert_eq!(
        comments(&bed, task).await,
        vec![format!(
            "Reconciled: PR {PR_URL} is merged; card moved to Done."
        )]
    );

    // The second pass is where a sweep that ran forever would accrete: the card
    // is out of the candidate set entirely now, so nothing is even asked.
    let second = sweep(&state, &forge, &f).await.expect("second pass");
    assert_eq!(second, merge_reconcile::Reconciled::default());
    assert_eq!(comments(&bed, task).await.len(), 1, "no second comment");
    assert_eq!(read(&bed, task).await.column, f.done);
    bed.teardown().await;
}

/// The move must fire board automation exactly as a human drag does (AC-3) —
/// asserted through a rule the board itself carries, not through the internals.
#[tokio::test]
async fn the_move_fires_the_boards_completed_column_automation() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    bed.db()
        .exec(
            "UPDATE boards SET automation = $2 WHERE id = $1",
            params![
                f.board,
                serde_json::json!({ "completed": [{ "kind": "add_board_label", "label": "shipped" }] })
            ],
        )
        .await
        .expect("automation");
    let (task, _) = card(&bed, &f, 1, f.review, Some(PR_URL)).await;

    sweep(&state, &FakeForge::ended(7, MergeState::Merged), &f)
        .await
        .expect("sweep");

    assert_eq!(
        labels(&bed, task).await,
        vec!["shipped"],
        "the completed column's rule ran, so nothing on this board can tell a \
         reconciled card from a dragged one"
    );
    bed.teardown().await;
}

// ── AC-5: a card a human owns is not touched ───────────────────────────────

#[tokio::test]
async fn a_card_needing_human_review_is_never_auto_moved() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    let (task, _) = card(&bed, &f, 1, f.review, Some(PR_URL)).await;
    state
        .tasks
        .attach_label(f.tenant, task, "needs-human-review")
        .await
        .expect("label");

    let done = sweep(&state, &FakeForge::ended(7, MergeState::Merged), &f)
        .await
        .expect("sweep");

    assert_eq!(done, merge_reconcile::Reconciled::default());
    assert_eq!(read(&bed, task).await.column, f.review, "no move");
    assert!(comments(&bed, task).await.is_empty(), "no comment");
    assert_eq!(labels(&bed, task).await, vec!["needs-human-review"]);
    bed.teardown().await;
}

// ── AC-6: closed without merging raises a hand, and never moves the card ────

#[tokio::test]
async fn a_closed_unmerged_pr_labels_the_card_once_and_moves_nothing() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    let (task, _) = card(&bed, &f, 1, f.review, Some(PR_URL)).await;

    let forge = FakeForge::ended(7, MergeState::ClosedUnmerged);
    let first = sweep(&state, &forge, &f).await.expect("first pass");
    assert_eq!(first.escalated, 1);
    assert_eq!(first.moved, 0);

    assert_eq!(read(&bed, task).await.column, f.review, "never moved");
    assert_eq!(labels(&bed, task).await, vec!["needs-human-review"]);
    let said = comments(&bed, task).await;
    assert_eq!(said.len(), 1);
    assert!(
        said[0].contains(PR_URL),
        "the comment names the PR: {said:?}"
    );
    assert!(said[0].contains("closed without merging"));

    // The label insert is the once-fence — a later pass over the same card does
    // nothing further, however many times it runs.
    let second = sweep(&state, &forge, &f).await.expect("second pass");
    assert_eq!(second, merge_reconcile::Reconciled::default());
    assert_eq!(comments(&bed, task).await.len(), 1);
    assert_eq!(labels(&bed, task).await.len(), 1);
    bed.teardown().await;
}

/// An OPEN pull request is simply unfinished — the third state, and the one the
/// sweep owes nothing to.
#[tokio::test]
async fn an_open_pr_is_left_alone() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    let (task, _) = card(&bed, &f, 1, f.review, Some(PR_URL)).await;

    let done = sweep(&state, &FakeForge::ended(7, MergeState::Open), &f)
        .await
        .expect("sweep");

    assert_eq!(done, merge_reconcile::Reconciled::default());
    assert_eq!(read(&bed, task).await.column, f.review);
    assert!(comments(&bed, task).await.is_empty());
    bed.teardown().await;
}

// ── AC-2: the body join, and the backfill that makes the board self-heal ────

#[tokio::test]
async fn a_card_with_no_recorded_pr_is_joined_by_closes_and_backfilled() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    let (task, key) = card(&bed, &f, 1, f.todo, None).await;

    let forge = FakeForge::default().merged_body(9, &format!("What changed\n\nCloses {key}\n"));
    let first = sweep(&state, &forge, &f).await.expect("first pass");
    assert_eq!((first.moved, first.backfilled), (1, 1));

    let c = read(&bed, task).await;
    assert_eq!(c.column, f.done);
    assert_eq!(
        c.pr_url.as_deref(),
        Some("https://github.com/acme/api/pull/9"),
        "the board healed itself: the card now records the PR it shipped as"
    );
    assert_eq!(comments(&bed, task).await.len(), 1);

    // The second pass reaches the card by the FIRST join now (it has a pr_url),
    // and the guarded move refuses it — so still one comment, still one move.
    let forge = FakeForge {
        details: HashMap::from([(9, MergeState::Merged)]),
        merged: forge.merged.clone(),
        ..Default::default()
    };
    let second = sweep(&state, &forge, &f).await.expect("second pass");
    assert_eq!(second, merge_reconcile::Reconciled::default());
    assert_eq!(comments(&bed, task).await.len(), 1);
    bed.teardown().await;
}

/// The joins are ORDERED (AC-2): a card that already records a PR is answered
/// by that PR, and a `Closes` line naming it in some other merged PR's body
/// never overrides it.
#[tokio::test]
async fn a_recorded_pr_url_outranks_the_body_join() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    let (task, key) = card(&bed, &f, 1, f.review, Some(PR_URL)).await;

    // PR #7 is the card's own, and it is still open. PR #9 merged saying it
    // closes the same card — a stale or copied body.
    let forge = FakeForge::ended(7, MergeState::Open).merged_body(9, &format!("Closes {key}"));
    let done = sweep(&state, &forge, &f).await.expect("sweep");

    assert_eq!(done, merge_reconcile::Reconciled::default());
    let c = read(&bed, task).await;
    assert_eq!(c.column, f.review, "the card's own PR is still open");
    assert_eq!(c.pr_url.as_deref(), Some(PR_URL), "not overwritten");
    bed.teardown().await;
}

// ── NG-5: a card that already reached a completed column is not touched ─────

#[tokio::test]
async fn a_card_already_done_is_outside_the_sweep_entirely() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    let (recorded, _) = card(&bed, &f, 1, f.done, Some(PR_URL)).await;
    let (no_url, key) = card(&bed, &f, 2, f.done, None).await;

    // The staged answers would move both, if either were a candidate: #7 is
    // merged, and #9's body closes the second card.
    let forge = FakeForge::ended(7, MergeState::Merged).merged_body(9, &format!("Closes {key}"));
    let done = sweep(&state, &forge, &f).await.expect("sweep");

    assert_eq!(done, merge_reconcile::Reconciled::default());
    assert!(comments(&bed, recorded).await.is_empty());
    assert_eq!(
        read(&bed, no_url).await.pr_url,
        None,
        "not even a pr_url is stamped on a completed card"
    );
    assert_eq!(
        forge.calls.load(Ordering::SeqCst),
        1,
        "one call — the merged listing. A completed card is never asked about"
    );
    bed.teardown().await;
}

// ── AC-7: a forge error is an outage, and an outage writes nothing ─────────

#[tokio::test]
async fn a_forge_error_holds_the_whole_pass() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    let (task, _) = card(&bed, &f, 1, f.review, Some(PR_URL)).await;

    let err = sweep(&state, &FakeForge::down(), &f).await;
    assert!(err.is_err(), "an unreachable forge fails the pass");

    assert_eq!(read(&bed, task).await.column, f.review);
    assert!(comments(&bed, task).await.is_empty());
    assert!(labels(&bed, task).await.is_empty());
    bed.teardown().await;
}

/// The read-before-write ordering, which is what makes the hold total: a
/// workspace whose SECOND forge read fails must not have acted on the first.
#[tokio::test]
async fn a_failure_partway_through_the_reads_writes_nothing_at_all() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    let (task, _) = card(&bed, &f, 1, f.review, Some(PR_URL)).await;

    // The per-PR read succeeds and says "merged"; the merged listing then dies.
    struct HalfDown(FakeForge);
    #[async_trait::async_trait]
    impl Forge for HalfDown {
        async fn prs_needing_review(&self, _repo: &Repo) -> anyhow::Result<Vec<PullRequest>> {
            anyhow::bail!("not asked")
        }
        async fn pr_details(&self, repo: &Repo, number: u64) -> anyhow::Result<PrDetails> {
            self.0.pr_details(repo, number).await
        }
        async fn merged_prs_since(
            &self,
            _repo: &Repo,
            _since: chrono::DateTime<chrono::Utc>,
        ) -> anyhow::Result<Vec<MergedPr>> {
            anyhow::bail!("503 upstream")
        }
    }

    let forge = HalfDown(FakeForge::ended(7, MergeState::Merged));
    let err = merge_reconcile::reconcile_workspace(&state, &forge, &repo(), f.tenant, f.ws).await;
    assert!(err.is_err());

    assert_eq!(
        read(&bed, task).await.column,
        f.review,
        "the merge was read but not acted on — the pass held instead"
    );
    assert!(comments(&bed, task).await.is_empty());
    bed.teardown().await;
}

// ── AC-8: safe on every replica ────────────────────────────────────────────

#[tokio::test]
async fn two_sweeps_over_one_card_produce_one_move_and_one_comment() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    let (task, _) = card(&bed, &f, 1, f.review, Some(PR_URL)).await;

    let forge = FakeForge::ended(7, MergeState::Merged);
    let (a, b) = tokio::join!(sweep(&state, &forge, &f), sweep(&state, &forge, &f));
    let moved = a.expect("pass a").moved + b.expect("pass b").moved;

    assert_eq!(moved, 1, "exactly one replica moved the card");
    assert_eq!(comments(&bed, task).await.len(), 1, "and exactly one spoke");
    assert_eq!(read(&bed, task).await.column, f.done);
    bed.teardown().await;
}

// ── AC-1: the loops switch gates the forge itself ──────────────────────────

/// With loops off the sweep makes NO forge call and no board write. The gate has
/// to sit before the network, not before the write — a poll that still costs
/// rate limit is not "off".
#[tokio::test]
async fn loops_off_asks_the_forge_nothing() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let mut state = bed.app_state().await;
    let (task, _) = card(&bed, &f, 1, f.review, Some(PR_URL)).await;

    let calls = Arc::new(AtomicUsize::new(0));
    state.review_demand = Arc::new(nook_control::services::forge::ReviewDemand::new(
        Some(Box::new(FakeForge {
            details: HashMap::from([(7, MergeState::Merged)]),
            calls: calls.clone(),
            ..Default::default()
        })),
        std::time::Duration::ZERO,
    ));

    nook_control::services::loops::set(&*state.settings, f.tenant, false)
        .await
        .expect("loops off");
    merge_reconcile::pass(&state).await.expect("pass");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no forge call");
    assert_eq!(read(&bed, task).await.column, f.review, "no board write");

    // The same pass, with the switch on, does reach the forge and the board —
    // so the assertion above is about the gate and not about a broken fixture.
    nook_control::services::loops::set(&*state.settings, f.tenant, true)
        .await
        .expect("loops on");
    merge_reconcile::pass(&state).await.expect("pass");
    assert!(calls.load(Ordering::SeqCst) > 0, "the forge was asked");
    assert_eq!(read(&bed, task).await.column, f.done);
    bed.teardown().await;
}

/// A workspace with no GitHub remote is a silent no-op (AC-7's last clause) —
/// the dogfood workspace's local path is the case that actually occurs.
#[tokio::test]
async fn a_workspace_with_no_forge_remote_asks_nothing() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let mut state = bed.app_state().await;
    bed.db()
        .exec(
            "UPDATE workspaces SET git_remote_url = '/workspace/nook-dogfood.git' WHERE id = $1",
            params![f.ws],
        )
        .await
        .expect("local remote");
    let (task, _) = card(&bed, &f, 1, f.review, Some(PR_URL)).await;

    let calls = Arc::new(AtomicUsize::new(0));
    state.review_demand = Arc::new(nook_control::services::forge::ReviewDemand::new(
        Some(Box::new(FakeForge {
            details: HashMap::from([(7, MergeState::Merged)]),
            calls: calls.clone(),
            ..Default::default()
        })),
        std::time::Duration::ZERO,
    ));
    nook_control::services::loops::set(&*state.settings, f.tenant, true)
        .await
        .expect("loops on");

    merge_reconcile::pass(&state).await.expect("pass");

    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(read(&bed, task).await.column, f.review);
    bed.teardown().await;
}
