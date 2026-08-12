//! MAIN-516: a merge that makes an APPROVED pull request conflict puts it back
//! in the repair queue — the queue the builder actually reads.
//!
//! The hygiene pass's label was never the queue. `repair_items` reads the job
//! ledger (`rejected_review_heads`), and an approved PR's recorded verdict says
//! `approved` — correctly, for the head that was reviewed. So a conflicting PR
//! carried the label, matched no row, and raised nothing; a conflict moves no
//! head, so nothing ever re-triggered. Observed on #407, stranded twelve hours
//! with a green review.
//!
//! These run the REAL pass and the REAL converger against a live bed, with a
//! fake forge whose answers the test dictates — the point is the join between
//! them, which no unit test of either half can see. Engine-neutral, so both
//! legs run it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use nook_control::repo::jobs::{NewLoopJob, CONFLICT_VERDICT_SOURCE};
use nook_control::services::forge::{Forge, MergeState, PrDetails, PullRequest, Repo};
use nook_control::services::work_source::WorkItem;
use nook_control::services::{jobs, pr_hygiene, run_reconcile};
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

/// One forge for both halves: the hygiene pass holds it as a `&dyn Forge`, and
/// the build converger reaches the same instance through `ReviewDemand`. Shared
/// state is the whole point — the label the pass writes is the label the
/// converger must then see.
#[derive(Clone, Default)]
struct FakeForge {
    prs: Arc<Mutex<Vec<PullRequest>>>,
    /// `None` is GitHub still computing; `Some(false)` is a conflict.
    mergeable: Arc<Mutex<HashMap<u64, Option<bool>>>>,
    body: Arc<String>,
    comments: Arc<Mutex<HashMap<u64, Vec<String>>>>,
}

impl FakeForge {
    fn new(body: String, prs: Vec<PullRequest>) -> Self {
        Self {
            prs: Arc::new(Mutex::new(prs)),
            mergeable: Arc::new(Mutex::new(HashMap::new())),
            body: Arc::new(body),
            comments: Arc::new(Mutex::new(HashMap::new())),
        }
    }
    fn set_mergeable(&self, pr: u64, m: Option<bool>) {
        self.mergeable.lock().unwrap().insert(pr, m);
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
    /// The builder pushed a rebase: the head moves and its own label comes off,
    /// exactly as the repair pass leaves the PR.
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
            mergeable: self
                .mergeable
                .lock()
                .unwrap()
                .get(&number)
                .copied()
                .unwrap_or(Some(true)),
            body: self.body.to_string(),
            merge_state: MergeState::Open,
        })
    }
    async fn issue_comment_bodies(&self, _repo: &Repo, number: u64) -> anyhow::Result<Vec<String>> {
        Ok(self.comments_of(number))
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

const PR: u64 = 7;

struct Fixture {
    tenant: TenantId,
    user: UserId,
    ws: WorkspaceId,
    task: TaskId,
    /// The PR body's `Closes` line — the card mirror's only join.
    body: String,
}

/// A tenant, a GitHub-remoted workspace, and one card parked in In Review with
/// PR #7 recorded on it — the shape `pr_opened` leaves behind.
async fn fixture(bed: &TestBed) -> Fixture {
    let tenant = bed.tenant("conflict").await;
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
    let key = format!("C{}", &board.0.simple().to_string()[26..]).to_uppercase();
    bed.db()
        .exec(
            "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1, $2, 'b', $3, 'local')",
            params![board, tenant, key.clone()],
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
             VALUES ($1, $2, $3, $4, 'approved then conflicted', 61, 'task', $5,
                     'https://github.com/acme/api/pull/7')",
            params![task, tenant, board, col, ws],
        )
        .await
        .expect("task");

    Fixture {
        tenant,
        user,
        ws,
        task,
        body: format!("What changed\n\nCloses {key}-61\n\nRisk: Low"),
    }
}

/// Point the app state's demand at the fake, so `repair_items` asks the same
/// forge the hygiene pass writes to.
fn with_forge(
    state: &nook_control::state::AppState,
    forge: &FakeForge,
) -> nook_control::state::AppState {
    let mut state = state.clone();
    state.review_demand = Arc::new(nook_control::services::forge::ReviewDemand::new(
        Some(Box::new(forge.clone())),
        std::time::Duration::ZERO,
    ));
    state
}

/// A completed review run that an AGENT concluded, planted straight into the
/// ledger — the fact the hygiene pass reads, without the state machine.
async fn agent_verdict(
    bed: &TestBed,
    state: &nook_control::state::AppState,
    f: &Fixture,
    head: &str,
    verdict: &str,
) {
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
            review_forced: false,
        })
        .await
        .expect("review run");
    bed.db()
        .exec(
            "UPDATE loop_jobs SET state = 'completed', review_verdict = $2 WHERE id = $1",
            params![job.id, verdict],
        )
        .await
        .expect("verdict");
}

/// Every conflict-recorded row for this PR, newest last: `(head, verdict, state,
/// seed)`. Reading `review_verdict_source` is AC-6 — a verdict no agent
/// produced is marked as such in the ledger itself.
async fn conflict_rows(bed: &TestBed, f: &Fixture) -> Vec<(String, String, String, String)> {
    bed.db()
        .query_all(
            "SELECT review_head_sha, review_verdict, state, seed FROM loop_jobs
              WHERE workspace_id = $1 AND review_verdict_source = $2
              ORDER BY created_at, id",
            params![f.ws.0, CONFLICT_VERDICT_SOURCE],
        )
        .await
        .expect("conflict rows")
}

/// AC-1/AC-2/AC-3/AC-6: an approved PR that goes CONFLICTING is recorded as
/// rejected at the head it conflicts at, one repair is raised on the next
/// converge, and a second pass adds nothing.
#[tokio::test]
async fn a_conflict_on_an_approved_pr_raises_exactly_one_repair() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    agent_verdict(&bed, &state, &f, "aaa", "approved").await;

    let forge = FakeForge::new(
        f.body.clone(),
        vec![PullRequest {
            number: PR,
            head_sha: "aaa".into(),
            labels: vec!["loop-approved".into()],
        }],
    );
    let state = with_forge(&state, &forge);

    // Before the merge collided with it: approved, mergeable, nothing owing.
    let quiet = jobs::converge_builds(&state, f.tenant, f.user, f.ws, None)
        .await
        .expect("converge");
    assert_eq!(quiet.raised, 0, "an approved, clean PR is not repair work");

    forge.set_mergeable(PR, Some(false));
    let prs = forge.prs_needing_review(&repo()).await.unwrap();
    let healed = pr_hygiene::heal(&state, &forge, &repo(), f.tenant, f.user, f.ws, &prs)
        .await
        .expect("heal");
    assert_eq!(
        (healed.recorded, healed.marked),
        (1, 1),
        "the row the queue reads AND the label a human reads"
    );

    let rows = conflict_rows(&bed, &f).await;
    assert_eq!(rows.len(), 1, "one recorded rejection");
    assert_eq!(rows[0].0, "aaa", "recorded at the CONFLICTING head");
    assert_eq!(rows[0].1, "changes_requested");
    assert_eq!(rows[0].2, "completed");
    assert!(
        rows[0].3.contains("conflicts with its base branch")
            && rows[0].3.contains("no agent read this head"),
        "AC-6: the row names its cause and disclaims findings: {}",
        rows[0].3
    );

    // AC-1: the very next converge raises the repair, with no human action.
    let c = jobs::converge_builds(&state, f.tenant, f.user, f.ws, None)
        .await
        .expect("converge");
    assert_eq!(c.raised, 1, "the conflicting PR is repair work now");
    assert_eq!(
        c.jobs[0].build_fingerprint.as_deref(),
        Some("repair:aaa"),
        "AC-3: fingerprinted on the conflicting head, which the rebase moves"
    );
    assert_eq!(
        state
            .tasks
            .get_row(f.tenant, f.task)
            .await
            .expect("read")
            .expect("card")
            .assignee_user_id,
        None,
        "a repair never claims the card out from under In Review"
    );

    // AC-2: a second hygiene pass at the same head records nothing further —
    // and no second repair follows it.
    let prs = forge.prs_needing_review(&repo()).await.unwrap();
    let healed = pr_hygiene::heal(&state, &forge, &repo(), f.tenant, f.user, f.ws, &prs)
        .await
        .expect("heal 2");
    assert_eq!(healed.recorded, 0, "one repair per conflicting head");
    assert_eq!(conflict_rows(&bed, &f).await.len(), 1);
    assert_eq!(forge.comments_of(PR).len(), 1, "one conflict comment");
    let again = jobs::converge_builds(&state, f.tenant, f.user, f.ws, None)
        .await
        .expect("converge 2");
    assert_eq!(again.raised, 0, "no duplicate repair");

    bed.teardown().await;
}

/// AC-3/AC-4: the repair's push moves the head, which clears the repair
/// fingerprint by itself — and the ordinary review path is owed the REBASED
/// head, never the conflicting one. No review run diagnoses a conflict.
#[tokio::test]
async fn the_rebase_clears_the_repair_and_hands_the_new_head_to_review() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    agent_verdict(&bed, &state, &f, "aaa", "approved").await;

    let forge = FakeForge::new(
        f.body.clone(),
        vec![PullRequest {
            number: PR,
            head_sha: "aaa".into(),
            labels: vec!["loop-approved".into()],
        }],
    );
    forge.set_mergeable(PR, Some(false));
    let state = with_forge(&state, &forge);

    let prs = forge.prs_needing_review(&repo()).await.unwrap();
    pr_hygiene::heal(&state, &forge, &repo(), f.tenant, f.user, f.ws, &prs)
        .await
        .expect("heal");
    let c = jobs::converge_builds(&state, f.tenant, f.user, f.ws, None)
        .await
        .expect("converge");
    assert_eq!(c.raised, 1);

    // AC-4, on the review side: the conflicting head counts as concluded, so
    // no review run is raised to look at a conflict, and the head the builder
    // is about to push is owed one.
    let heads = state
        .jobs
        .review_run_heads(f.tenant, f.ws)
        .await
        .expect("heads");
    let review_item = |head: &str| WorkItem {
        key: PR as i64,
        fingerprint: head.into(),
        label: format!("PR #{PR}"),
        target_task_id: None,
        claim_first: false,
    };
    let now = chrono::Utc::now();
    assert!(
        run_reconcile::owed(&[review_item("aaa")], &heads, 1, now)
            .0
            .is_empty(),
        "AC-4: the conflicting head owes no review"
    );
    assert_eq!(
        run_reconcile::owed(&[review_item("bbb")], &heads, 1, now)
            .0
            .len(),
        1,
        "AC-4: the head the rebase will push does"
    );

    // The repair concludes on the SAME PR, and pushes: the head moves.
    let job = c.jobs[0].clone();
    state.jobs.transition(job.id, "running").await.expect("run");
    jobs::record_build_outcome(
        &state,
        f.tenant,
        job.id,
        &BuildOutcomeRequest {
            outcome: "pr_opened".into(),
            url: Some("https://github.com/acme/api/pull/7".into()),
            question: None,
        },
    )
    .await
    .expect("outcome");
    state
        .jobs
        .transition(job.id, "completed")
        .await
        .expect("done");
    forge.push(PR, "bbb");
    forge.set_mergeable(PR, Some(true));

    let after = jobs::converge_builds(&state, f.tenant, f.user, f.ws, None)
        .await
        .expect("converge 3");
    assert_eq!(after.raised, 0, "the rebase answered the repair");
    let prs = forge.prs_needing_review(&repo()).await.unwrap();
    let healed = pr_hygiene::heal(&state, &forge, &repo(), f.tenant, f.user, f.ws, &prs)
        .await
        .expect("heal 2");
    assert_eq!(
        (healed.recorded, healed.marked),
        (0, 0),
        "a mergeable PR is not conflict work"
    );
    assert_eq!(
        conflict_rows(&bed, &f).await.len(),
        1,
        "and the old rejection is not re-recorded at the new head"
    );

    bed.teardown().await;
}

/// AC-5: `needs-human-review` opts the PR out of all of this, unchanged. A
/// person owns it, and neither the record nor the label is ours to write.
#[tokio::test]
async fn an_escalated_pr_records_nothing() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;

    let forge = FakeForge::new(
        f.body.clone(),
        vec![PullRequest {
            number: PR,
            head_sha: "aaa".into(),
            labels: vec!["needs-human-review".into()],
        }],
    );
    forge.set_mergeable(PR, Some(false));
    let state = with_forge(&state, &forge);

    let prs = forge.prs_needing_review(&repo()).await.unwrap();
    let healed = pr_hygiene::heal(&state, &forge, &repo(), f.tenant, f.user, f.ws, &prs)
        .await
        .expect("heal");
    assert_eq!((healed.recorded, healed.marked, healed.restored), (0, 0, 0));
    assert!(conflict_rows(&bed, &f).await.is_empty());
    assert!(forge.comments_of(PR).is_empty());
    assert_eq!(forge.labels_of(PR), vec!["needs-human-review"]);
    assert_eq!(
        jobs::converge_builds(&state, f.tenant, f.user, f.ws, None)
            .await
            .expect("converge")
            .raised,
        0,
        "an escalated PR raises no repair"
    );

    bed.teardown().await;
}

/// AC-2's other half: a head an AGENT already rejected is in the repair queue
/// on its own account. The conflict records nothing on top of it — a second row
/// saying the same thing would only compete to be the newest one read.
///
/// The PR reaches the conflict check carrying a stale `loop-approved`, which is
/// what leaves the ledger and the label disagreeing in the first place.
#[tokio::test]
async fn an_existing_rejection_at_that_head_is_left_alone() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    agent_verdict(&bed, &state, &f, "aaa", "changes_requested").await;

    let forge = FakeForge::new(
        f.body.clone(),
        vec![PullRequest {
            number: PR,
            head_sha: "aaa".into(),
            labels: vec!["loop-approved".into()],
        }],
    );
    forge.set_mergeable(PR, Some(false));
    let state = with_forge(&state, &forge);

    let prs = forge.prs_needing_review(&repo()).await.unwrap();
    let healed = pr_hygiene::heal(&state, &forge, &repo(), f.tenant, f.user, f.ws, &prs)
        .await
        .expect("heal");
    assert_eq!(healed.recorded, 0, "the agent's rejection stands alone");
    assert_eq!(healed.marked, 1, "the label still catches up to the ledger");
    assert!(
        conflict_rows(&bed, &f).await.is_empty(),
        "nothing was recorded on top of the agent's verdict"
    );

    // And the repair the agent's own verdict earns still comes — one, at the
    // head it rejected.
    let c = jobs::converge_builds(&state, f.tenant, f.user, f.ws, None)
        .await
        .expect("converge");
    assert_eq!(c.raised, 1);
    assert_eq!(c.jobs[0].build_fingerprint.as_deref(), Some("repair:aaa"));

    bed.teardown().await;
}
