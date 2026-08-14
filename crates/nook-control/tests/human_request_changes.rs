//! MAIN-591: a human requests changes on a pull request, and both loops obey.
//!
//! The interesting half is not the label — it is that the SUPPRESSION and the
//! REPAIR both have to fall out of one recorded row, with no rule of their own
//! anywhere (NG-4). So these tests assert against the untouched `owed` and the
//! untouched `repair_items`, and the row is the only thing between them.
//!
//! Two levels, for one reason. `changes_request_target` resolves a real
//! `GithubForge` from the workspace's token, so the endpoint cannot be driven
//! end to end against a fake — the route tests here cover the refusals that
//! land BEFORE that resolution (which is where AC-2's "nothing is written"
//! lives), and everything past it is driven through `open_pr_head` and
//! `request_changes` in the same order the route calls them.
//!
//! Engine-neutral (MAIN-264): nothing here names a `sqlx` type. Needs a
//! database: set `DATABASE_URL` (`NOOK_REQUIRE_DB=1` in the suite).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::Json;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::repo::jobs::{NewLoopJob, HUMAN_VERDICT_SOURCE};
use nook_control::services::forge::{Forge, MergeState, PrDetails, PullRequest, Repo};
use nook_control::services::work_source::{repair_label, WorkItem};
use nook_control::services::{jobs, run_reconcile};
use nook_control::state::AppState;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

const PR: u64 = 7;

/// The forge both halves share: `request_changes` writes to it, and the build
/// converger reads the very labels it wrote through `ReviewDemand`.
#[derive(Clone, Default)]
struct FakeForge {
    prs: Arc<Mutex<Vec<PullRequest>>>,
    comments: Arc<Mutex<HashMap<u64, Vec<String>>>>,
    /// What `pr_details` reports — the open/merged/closed answer AC-2 gates on.
    state: Arc<Mutex<HashMap<u64, MergeState>>>,
}

impl FakeForge {
    fn open(head: &str, labels: &[&str]) -> Self {
        Self {
            prs: Arc::new(Mutex::new(vec![PullRequest {
                number: PR,
                head_sha: head.into(),
                labels: labels.iter().map(|l| l.to_string()).collect(),
            }])),
            ..Default::default()
        }
    }
    fn set_state(&self, pr: u64, s: MergeState) {
        self.state.lock().unwrap().insert(pr, s);
    }
    fn labels_of(&self, pr: u64) -> Vec<String> {
        self.prs
            .lock()
            .unwrap()
            .iter()
            .find(|p| p.number == pr)
            .map(|p| p.labels.clone())
            .unwrap_or_default()
    }
    fn comments_of(&self, pr: u64) -> Vec<String> {
        self.comments
            .lock()
            .unwrap()
            .get(&pr)
            .cloned()
            .unwrap_or_default()
    }
    /// The builder pushed the repair: the head moves and the label it was
    /// carrying comes off, exactly as a repair pass leaves the PR.
    fn push(&self, pr: u64, head: &str) {
        let mut prs = self.prs.lock().unwrap();
        if let Some(p) = prs.iter_mut().find(|p| p.number == pr) {
            p.head_sha = head.into();
            p.labels.retain(|l| l != "loop-changes-requested");
        }
    }
}

#[async_trait::async_trait]
impl Forge for FakeForge {
    async fn prs_needing_review(&self, _repo: &Repo) -> anyhow::Result<Vec<PullRequest>> {
        Ok(self.prs.lock().unwrap().clone())
    }
    async fn pr_details(&self, _repo: &Repo, number: u64) -> anyhow::Result<PrDetails> {
        Ok(PrDetails {
            mergeable: Some(true),
            body: String::new(),
            merge_state: self
                .state
                .lock()
                .unwrap()
                .get(&number)
                .copied()
                .unwrap_or(MergeState::Open),
            head_sha: self
                .prs
                .lock()
                .unwrap()
                .iter()
                .find(|p| p.number == number)
                .map(|p| p.head_sha.clone())
                .unwrap_or_default(),
        })
    }
    async fn comment(&self, _repo: &Repo, number: u64, body: &str) -> anyhow::Result<()> {
        self.comments
            .lock()
            .unwrap()
            .entry(number)
            .or_default()
            .push(body.to_string());
        Ok(())
    }
    async fn set_verdict_label(
        &self,
        _repo: &Repo,
        number: u64,
        label: &str,
    ) -> anyhow::Result<()> {
        let mut prs = self.prs.lock().unwrap();
        if let Some(p) = prs.iter_mut().find(|p| p.number == number) {
            p.labels.retain(|l| {
                !matches!(
                    l.as_str(),
                    "loop-approved" | "loop-changes-requested" | "needs-human-review"
                )
            });
            p.labels.push(label.to_string());
        }
        Ok(())
    }
}

fn repo() -> Repo {
    Repo {
        owner: "acme".into(),
        name: "api".into(),
    }
}

fn auth(user: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: false,
    }
}

struct Fixture {
    tenant: TenantId,
    user: UserId,
    ws: WorkspaceId,
    task: TaskId,
    /// A second card in the same workspace with NO recorded PR — AC-2's other
    /// refusal, which needs a card rather than a missing one.
    prless: TaskId,
}

/// A tenant, a GitHub-remoted workspace, and one card parked in In Review with
/// PR #7 recorded on it — the shape `pr_opened` leaves behind.
///
/// Deliberately NO forge token, on the workspace or in the environment. A
/// workspace token OUTRANKS the deployment forge in `ReviewDemand::prs`
/// (MAIN-456), so sealing one here would send `converge_builds` to the real
/// GitHub instead of the fake below — the seam would be gone. Everything past
/// the credential is driven with the fake instead; `changes_request_target`,
/// which is the one thing that needs a real one, is covered on its own below
/// for the refusals that land before it looks for a token.
async fn fixture(bed: &TestBed) -> Fixture {
    let tenant = bed.tenant("human-rc").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    bed.db()
        .exec(
            "UPDATE workspaces SET git_remote_url = 'git@github.com:acme/api.git' WHERE id = $1",
            params![ws],
        )
        .await
        .expect("remote");

    let board = BoardId(Uuid::now_v7());
    let key = format!("H{}", &board.0.simple().to_string()[26..]).to_uppercase();
    bed.db()
        .exec(
            "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1, $2, 'b', $3, 'local')",
            params![board, tenant, key],
        )
        .await
        .expect("board");
    let col = ColumnId::new();
    bed.db()
        .exec(
            "INSERT INTO board_columns (id, board_id, name, position, type)
             VALUES ($1, $2, 'In Review', 0, 'review')",
            params![col, board],
        )
        .await
        .expect("column");
    let task = TaskId::new();
    bed.db()
        .exec(
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, number, type,
                                workspace_id, pr_url)
             VALUES ($1, $2, $3, $4, 'in review', 61, 'task', $5,
                     'https://github.com/acme/api/pull/7')",
            params![task, tenant, board, col, ws],
        )
        .await
        .expect("task");
    let prless = TaskId::new();
    bed.db()
        .exec(
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, number, type,
                                workspace_id)
             VALUES ($1, $2, $3, $4, 'no pr yet', 62, 'task', $5)",
            params![prless, tenant, board, col, ws],
        )
        .await
        .expect("second task");

    Fixture {
        tenant,
        user,
        ws,
        task,
        prless,
    }
}

/// Point the app state's demand at the fake, so `repair_items` asks the same
/// forge the ruling wrote to.
fn with_forge(state: &AppState, forge: &FakeForge) -> AppState {
    let mut state = state.clone();
    state.review_demand = Arc::new(nook_control::services::forge::ReviewDemand::new(
        Some(Box::new(forge.clone())),
        std::time::Duration::ZERO,
    ));
    state
}

async fn post(
    state: &AppState,
    f: &Fixture,
    task: TaskId,
    body: &str,
    request_changes: bool,
) -> nook_control::error::ApiResult<TaskComment> {
    nook_control::routes::task_detail::create_comment(
        State(state.clone()),
        auth(f.user, f.tenant),
        Path(task.to_string()),
        Json(CreateCommentRequest {
            body_md: body.into(),
            author_name: None,
            clear_escalation: false,
            request_changes,
        }),
    )
    .await
    .map(|j| j.0)
}

/// The whole ruling, in the order the route performs it — check the pull
/// request is open and read its head, then post, then record. Returns the head
/// it was recorded at.
///
/// The route reaches `pr` and `workspace` through `changes_request_target`,
/// which resolves a real `GithubForge` from the workspace's credential; here
/// they are the fixture's own facts, so this stays hermetic and the fake is
/// the only forge anything touches.
async fn rule(state: &AppState, forge: &FakeForge, f: &Fixture, body: &str) -> String {
    let row = state
        .tasks
        .get_row(f.tenant, f.task)
        .await
        .expect("read")
        .expect("card");
    let head = jobs::open_pr_head(forge, &repo(), PR).await.expect("open");
    jobs::request_changes(
        state,
        forge,
        &repo(),
        jobs::HumanChangesRequest {
            tenant: f.tenant,
            actor: f.user,
            task: &row,
            workspace: f.ws,
            pr: PR,
            head: &head,
            body,
        },
    )
    .await
    .expect("ruling");
    head
}

/// Every human-ruled row for this workspace, oldest first.
async fn human_rows(bed: &TestBed, f: &Fixture) -> Vec<(i64, String, String, String, String)> {
    bed.db()
        .query_all(
            "SELECT review_pr_number, review_head_sha, review_verdict, state, seed
               FROM loop_jobs
              WHERE workspace_id = $1 AND review_verdict_source = $2
              ORDER BY created_at, id",
            params![f.ws.0, HUMAN_VERDICT_SOURCE],
        )
        .await
        .expect("human rows")
}

fn review_item(head: &str) -> WorkItem {
    WorkItem {
        key: PR as i64,
        fingerprint: head.into(),
        label: format!("PR #{PR}"),
        target_task_id: None,
        claim_first: false,
        unblocked_at: None,
    }
}

/// A completed review run an AGENT concluded, planted straight into the ledger.
async fn agent_verdict(bed: &TestBed, state: &AppState, f: &Fixture, head: &str, verdict: &str) {
    let job = live_review(state, f, head, false).await;
    bed.db()
        .exec(
            "UPDATE loop_jobs SET state = 'completed', review_verdict = $2 WHERE id = $1",
            params![job, verdict],
        )
        .await
        .expect("verdict");
}

/// A review run in flight at `head` — the run AC-7 is about.
async fn live_review(state: &AppState, f: &Fixture, head: &str, forced: bool) -> JobId {
    let job = state
        .jobs
        .create(NewLoopJob {
            id: JobId::new(),
            tenant: f.tenant,
            kind: "review".into(),
            target_task_id: None,
            workspace_id: Some(f.ws),
            requested_by: f.user,
            seed: None,
            predecessor_job_id: None,
            review_pr_number: Some(PR as i64),
            review_head_sha: Some(head.into()),
            build_fingerprint: None,
            review_forced: forced,
        })
        .await
        .expect("review run");
    state.jobs.transition(job.id, "running").await.expect("run");
    job.id
}

/// Move a run's `created_at`, so "raised before the ruling" and "raised after
/// it" are facts rather than a race on the clock's resolution.
async fn backdate(bed: &TestBed, job: JobId, at: chrono::DateTime<chrono::Utc>) {
    bed.db()
        .exec(
            "UPDATE loop_jobs SET created_at = $2 WHERE id = $1",
            params![job, at],
        )
        .await
        .expect("backdate");
}

fn verdict(v: &str) -> ReviewVerdictRequest {
    ReviewVerdictRequest {
        verdict: v.into(),
        body: Some("the agent's findings".into()),
    }
}

/// AC-1: the flag absent leaves the endpoint byte-identical — a comment, and
/// not one other write anywhere.
#[tokio::test]
async fn without_the_flag_the_comment_endpoint_is_unchanged() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let f = fixture(&bed).await;
    let forge = FakeForge::open("aaa", &["loop-approved"]);
    let state = with_forge(&state, &forge);

    post(&state, &f, f.task, "just a thought", false)
        .await
        .expect("an ordinary comment");

    assert_eq!(
        state
            .tasks
            .comments_of(f.task)
            .await
            .expect("comments")
            .len(),
        1
    );
    assert!(human_rows(&bed, &f).await.is_empty(), "no verdict row");
    assert_eq!(
        forge.labels_of(PR),
        vec!["loop-approved".to_string()],
        "the pull request is untouched"
    );
    assert!(forge.comments_of(PR).is_empty(), "nothing posted on the PR");
    bed.teardown().await;
}

/// AC-2: an empty body, and a card with no `pr_url`, are each a 400 that names
/// which — and NOTHING is written, comment included.
#[tokio::test]
async fn an_empty_body_or_a_card_with_no_pr_is_refused_with_nothing_written() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let f = fixture(&bed).await;
    let forge = FakeForge::open("aaa", &[]);
    let state = with_forge(&state, &forge);

    let empty = post(&state, &f, f.task, "   ", true)
        .await
        .expect_err("400");
    assert!(
        format!("{empty:?}").contains("what the builder is told to fix"),
        "the refusal names the body: {empty:?}"
    );

    let no_pr = post(&state, &f, f.prless, "fix the resolver", true)
        .await
        .expect_err("400");
    assert!(
        format!("{no_pr:?}").contains("records no pull request"),
        "the refusal names the missing PR: {no_pr:?}"
    );

    assert!(
        state
            .tasks
            .comments_of(f.task)
            .await
            .expect("comments")
            .is_empty()
            && state
                .tasks
                .comments_of(f.prless)
                .await
                .expect("comments")
                .is_empty(),
        "a refused request writes no comment"
    );
    assert!(human_rows(&bed, &f).await.is_empty());
    assert!(forge.labels_of(PR).is_empty() && forge.comments_of(PR).is_empty());
    bed.teardown().await;
}

/// AC-2, the resolution half: every piece the ruling needs from stored data is
/// refused BY NAME when it is missing.
///
/// Only the refusals that land before the credential lookup — which is the
/// last thing `changes_request_target` does, and the one answer that depends
/// on what is in the environment. Asserting the no-token case here would make
/// this test say different things on a machine with `NOOK_GH_TOKEN` set and on
/// CI, which is the failure this suite was rebuilt to stop making.
#[tokio::test]
async fn the_resolver_names_each_missing_piece() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let f = fixture(&bed).await;

    let row = |id: TaskId| {
        let state = state.clone();
        async move {
            state
                .tasks
                .get_row(f.tenant, id)
                .await
                .expect("read")
                .expect("card")
        }
    };
    let refusal = |t: TaskItem| {
        let state = state.clone();
        async move {
            let Err(e) = jobs::changes_request_target(&state, f.tenant, &t).await else {
                panic!("expected a refusal");
            };
            format!("{e:?}")
        }
    };

    assert!(
        refusal(row(f.prless).await)
            .await
            .contains("records no pull request"),
        "a card with no recorded PR"
    );

    let mut no_workspace = row(f.task).await;
    no_workspace.workspace_id = None;
    assert!(
        refusal(no_workspace).await.contains("no workspace"),
        "a card with no workspace"
    );

    // What `submit-pr` derives when the agent reported no real PR: a compare
    // URL, which names no pull request to rule on.
    let mut compare_url = row(f.task).await;
    compare_url.pr_url = Some("https://github.com/acme/api/compare/main-591?expand=1".into());
    assert!(
        refusal(compare_url).await.contains("names no pull request"),
        "a recorded URL that is not a PR permalink"
    );

    bed.db()
        .exec(
            "UPDATE workspaces SET git_remote_url = 'git@gitlab.com:acme/api.git' WHERE id = $1",
            params![f.ws],
        )
        .await
        .expect("remote");
    assert!(
        refusal(row(f.task).await)
            .await
            .contains("not a GitHub repository"),
        "a workspace whose remote is not GitHub"
    );

    bed.teardown().await;
}

/// AC-2, the forge half: a merged or closed pull request is refused before
/// anything is asked of the database.
#[tokio::test]
async fn a_closed_or_merged_pull_request_is_refused() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let f = fixture(&bed).await;
    let forge = FakeForge::open("aaa", &["loop-approved"]);

    forge.set_state(PR, MergeState::Merged);
    let merged = jobs::open_pr_head(&forge, &repo(), PR)
        .await
        .expect_err("400");
    assert!(
        format!("{merged:?}").contains("already merged"),
        "{merged:?}"
    );

    forge.set_state(PR, MergeState::ClosedUnmerged);
    let closed = jobs::open_pr_head(&forge, &repo(), PR)
        .await
        .expect_err("400");
    assert!(format!("{closed:?}").contains("is closed"), "{closed:?}");

    assert!(human_rows(&bed, &f).await.is_empty());
    let _ = state;
    bed.teardown().await;
}

/// The spine: AC-3, AC-4, AC-5, AC-6 and AC-12 in one walk, plus the re-arm.
///
/// The two assertions that matter most are the ones with NO code behind them:
/// `owed` skips the PR and `repair_items` raises the repair, both from the
/// recorded row alone and neither with a rule about human rulings (NG-4).
#[tokio::test]
async fn a_ruling_holds_the_review_raises_the_repair_and_clears_on_a_push() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let f = fixture(&bed).await;
    let forge = FakeForge::open("aaa", &[]);
    let state = with_forge(&state, &forge);

    // Before the ruling: the head is owed a review and owes no repair.
    let heads = state
        .jobs
        .review_run_heads(f.tenant, f.ws)
        .await
        .expect("heads");
    let now = chrono::Utc::now();
    assert_eq!(
        run_reconcile::owed(&[review_item("aaa")], &heads, 1, now)
            .0
            .len(),
        1,
        "an unreviewed head is owed a review"
    );

    let head = rule(&state, &forge, &f, "the path resolver is case-sensitive").await;
    assert_eq!(head, "aaa", "AC-4: the head is read fresh from the forge");

    // AC-3: the body is on the PULL REQUEST, and the label replaced.
    assert_eq!(forge.comments_of(PR).len(), 1);
    assert!(
        forge.comments_of(PR)[0].contains("the path resolver is case-sensitive")
            && forge.comments_of(PR)[0].contains(&jobs::human_request_marker("aaa")),
        "the ruling and its marker: {:?}",
        forge.comments_of(PR)
    );
    assert_eq!(
        forge.labels_of(PR),
        vec!["loop-changes-requested".to_string()]
    );

    // AC-4: one row, and it is a record rather than work.
    let rows = human_rows(&bed, &f).await;
    assert_eq!(rows.len(), 1, "one row");
    assert_eq!(
        (
            rows[0].0,
            rows[0].1.as_str(),
            rows[0].2.as_str(),
            rows[0].3.as_str()
        ),
        (PR as i64, "aaa", "changes_requested", "completed")
    );
    let executor: Option<Option<Uuid>> = bed
        .db()
        .query_scalar_opt(
            "SELECT executor_node_id FROM loop_jobs WHERE workspace_id = $1
              AND review_verdict_source = $2",
            params![f.ws.0, HUMAN_VERDICT_SOURCE],
        )
        .await
        .expect("executor");
    assert_eq!(executor, Some(None), "no executor — the row is not work");

    // AC-5: the review loop skips it, with `owed` untouched.
    let heads = state
        .jobs
        .review_run_heads(f.tenant, f.ws)
        .await
        .expect("heads");
    assert!(
        run_reconcile::owed(&[review_item("aaa")], &heads, 1, now)
            .0
            .is_empty(),
        "AC-5: the ruled head owes no review"
    );

    // AC-6: the builder picks the repair up, fingerprinted on the ruled head,
    // with a seed that says a person asked and where to read them.
    let c = jobs::converge_builds(&state, f.tenant, f.user, f.ws, None)
        .await
        .expect("converge");
    assert_eq!(c.raised, 1, "the ruling is repair work");
    assert_eq!(c.jobs[0].build_fingerprint.as_deref(), Some("repair:aaa"));
    let seed = c.jobs[0].seed.clone().unwrap_or_default();
    assert!(
        seed.contains("a PERSON requested the changes") && seed.contains("comment"),
        "AC-6: the seed sends the builder to the human's comment: {seed}"
    );
    assert_eq!(
        seed,
        repair_label(PR, Some(HUMAN_VERDICT_SOURCE)),
        "the seed IS `repair_label`'s human case"
    );

    // AC-12: one event, naming the PR and the head.
    let recorded: i64 = bed
        .db()
        .query_scalar(
            "SELECT count(*) FROM events
              WHERE tenant_id = $1 AND kind = 'task.changes_requested'",
            params![f.tenant],
        )
        .await
        .expect("count");
    assert_eq!(recorded, 1, "one event records the request");
    let payload: serde_json::Value = bed
        .db()
        .query_scalar(
            "SELECT payload FROM events
              WHERE tenant_id = $1 AND kind = 'task.changes_requested'
              ORDER BY occurred_at DESC LIMIT 1",
            params![f.tenant],
        )
        .await
        .expect("the request event");
    assert_eq!(payload["pr"], serde_json::json!(PR), "{payload}");
    assert_eq!(payload["head"], serde_json::json!("aaa"), "{payload}");

    // The push answers it: the repair clears and the review re-arms — both by
    // the fingerprint moving, with nothing to reset.
    forge.push(PR, "bbb");
    let heads = state
        .jobs
        .review_run_heads(f.tenant, f.ws)
        .await
        .expect("heads");
    assert_eq!(
        run_reconcile::owed(&[review_item("bbb")], &heads, 1, now)
            .0
            .len(),
        1,
        "a new head is owed a review again"
    );
    bed.teardown().await;
}

/// AC-7: a review run already live at that head cannot overwrite the ruling —
/// for every verdict that posts one, and the pull request keeps its label.
#[tokio::test]
async fn a_live_agent_run_cannot_overwrite_a_human_ruling() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let f = fixture(&bed).await;
    let forge = FakeForge::open("aaa", &[]);
    let state = with_forge(&state, &forge);

    let live = live_review(&state, &f, "aaa", false).await;
    rule(&state, &forge, &f, "AC-2 is not met").await;

    for v in ["approved", "needs_human", "changes_requested"] {
        let refused = jobs::record_verdict(&state, f.tenant, live, &verdict(v))
            .await
            .expect_err("409");
        assert!(
            matches!(refused, nook_control::error::ApiError::Conflict(_)),
            "{v}: a live run must be refused, got {refused:?}"
        );
        assert!(
            format!("{refused:?}").contains("a person requested changes"),
            "{v}: the refusal names the human request: {refused:?}"
        );
    }
    assert_eq!(
        forge.labels_of(PR),
        vec!["loop-changes-requested".to_string()],
        "AC-7: the ruling's label survives every attempt"
    );
    assert_eq!(
        forge.comments_of(PR).len(),
        1,
        "a refused verdict posts nothing"
    );
    bed.teardown().await;
}

/// AC-8: `--force` remains the override — but only for a run raised AFTER the
/// ruling. A run created before it is exactly the one AC-7 is for.
///
/// The refusal is asserted through the real `record_verdict`, which returns
/// before it touches GitHub. The case that is NOT refused cannot be: that path
/// posts the verdict, so it is asserted against `human_hold_refuses`, which is
/// the rule `record_verdict` consults and nothing else.
#[tokio::test]
async fn only_a_forced_run_raised_after_the_ruling_escapes_the_hold() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let f = fixture(&bed).await;
    let forge = FakeForge::open("aaa", &[]);
    let state = with_forge(&state, &forge);

    let before = live_review(&state, &f, "aaa", true).await;
    // Backdated rather than merely written first: the two clocks agree to the
    // millisecond at best, and "before" and "after" are the whole assertion.
    // A computed instant, not `- INTERVAL '1 hour'`, so both engines run it.
    backdate(
        &bed,
        before,
        chrono::Utc::now() - chrono::Duration::hours(1),
    )
    .await;
    rule(&state, &forge, &f, "AC-2 is not met").await;

    let refused = jobs::record_verdict(&state, f.tenant, before, &verdict("approved"))
        .await
        .expect_err("409");
    assert!(
        matches!(refused, nook_control::error::ApiError::Conflict(_)),
        "a forced run ALREADY in flight when the person ruled is still refused: {refused:?}"
    );
    assert_eq!(
        forge.labels_of(PR),
        vec!["loop-changes-requested".to_string()],
        "and its `loop-approved` never goes back on"
    );

    let ruled_at = state
        .jobs
        .human_rejection_at(f.tenant, f.ws, PR as i64, "aaa")
        .await
        .expect("the ruling");
    assert!(ruled_at.is_some());
    let held = |job: &LoopJob| jobs::human_hold_refuses(job, ruled_at);

    let earlier = state
        .jobs
        .get(f.tenant, before)
        .await
        .expect("read")
        .expect("job");
    assert!(held(&earlier), "the run in flight at the time is held");

    // That run is over — a refused verdict ends its pass — and 0046 allows one
    // live run per pull request, so the deliberate re-review comes after it.
    state
        .jobs
        .transition(before, "failed")
        .await
        .expect("the refused run ends");
    let after = live_review(&state, &f, "aaa", true).await;
    backdate(&bed, after, chrono::Utc::now() + chrono::Duration::hours(1)).await;
    let later = state
        .jobs
        .get(f.tenant, after)
        .await
        .expect("read")
        .expect("job");
    assert!(
        !held(&later),
        "AC-8: a deliberate `--force` re-review raised after the ruling concludes normally"
    );

    // And an UNforced run raised just as late is still held — `--force` is the
    // override, not the passage of time.
    let mut unforced = later.clone();
    unforced.review_forced = false;
    assert!(held(&unforced));
    bed.teardown().await;
}

/// AC-11: the case the feature exists for — an APPROVED pull request. The
/// label is replaced, which is what makes it ineligible for yolo's merge.
#[tokio::test]
async fn a_ruling_on_an_approved_pull_request_replaces_the_label() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let f = fixture(&bed).await;
    let forge = FakeForge::open("aaa", &["loop-approved"]);
    let state = with_forge(&state, &forge);
    agent_verdict(&bed, &state, &f, "aaa", "approved").await;

    // Nothing owing before: an approved, reviewed head is neither review work
    // nor repair work.
    assert_eq!(
        jobs::converge_builds(&state, f.tenant, f.user, f.ws, None)
            .await
            .expect("converge")
            .raised,
        0
    );

    rule(
        &state,
        &forge,
        &f,
        "this regresses the case-insensitive path",
    )
    .await;

    assert_eq!(
        forge.labels_of(PR),
        vec!["loop-changes-requested".to_string()],
        "AC-11: `loop-approved` is gone, so yolo will not merge it"
    );
    assert_eq!(
        state
            .jobs
            .recorded_review_verdicts(f.tenant, f.ws)
            .await
            .expect("verdicts")
            .into_iter()
            .map(|v| v.review_verdict)
            .collect::<Vec<_>>(),
        vec!["changes_requested".to_string()],
        "the newest recorded verdict is the human's, so the label restore \
         cannot put `loop-approved` back"
    );
    assert_eq!(
        jobs::converge_builds(&state, f.tenant, f.user, f.ws, None)
            .await
            .expect("converge")
            .raised,
        1,
        "and the builder is sent back to it"
    );
    bed.teardown().await;
}

/// Two rulings at one head record one row: the ledger says "a human ruled at
/// aaa", and saying it twice would neither suppress more nor repair twice.
#[tokio::test]
async fn a_second_ruling_at_the_same_head_records_one_row() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let f = fixture(&bed).await;
    let forge = FakeForge::open("aaa", &[]);
    let state = with_forge(&state, &forge);

    rule(&state, &forge, &f, "first thought").await;
    rule(&state, &forge, &f, "and another").await;

    assert_eq!(human_rows(&bed, &f).await.len(), 1, "one row per head");
    let events: i64 = bed
        .db()
        .query_scalar(
            "SELECT count(*) FROM events
              WHERE tenant_id = $1 AND kind = 'task.changes_requested'",
            params![f.tenant],
        )
        .await
        .expect("count");
    assert_eq!(
        events, 1,
        "and one event — the feed cannot show two rulings the ledger does not hold"
    );
    assert_eq!(
        forge.comments_of(PR).len(),
        2,
        "both rulings still reach the pull request — that is the person's text"
    );
    assert_eq!(
        jobs::converge_builds(&state, f.tenant, f.user, f.ws, None)
            .await
            .expect("converge")
            .raised,
        1,
        "and exactly one repair"
    );
    bed.teardown().await;
}
