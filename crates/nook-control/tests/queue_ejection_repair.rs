//! MAIN-542: a pull request the MERGE QUEUE ejected goes back to the repair
//! queue, the same way a conflicting one does — and is told what happened.
//!
//! The dead state is MAIN-516's, reached by a different door. The queue ejects
//! a PR whose post-merge build fails; the PR stays open, its head does not
//! move, and its recorded verdict is still `approved` at a head nobody has
//! touched. `rejected_review_heads` has no entry, `repair_items` drops it, and
//! nothing re-triggers because nothing moved. Observed on #409/#410.
//!
//! So this card registers ejection as a THIRD cause on MAIN-516's one recorder
//! (AC-9) rather than building a second, and the tests below are the join
//! between the hygiene pass and the build converger — which no unit test of
//! either half can see. Engine-neutral, so both legs run it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use nook_control::repo::jobs::{NewLoopJob, CONFLICT_VERDICT_SOURCE, EJECTION_VERDICT_SOURCE};
use nook_control::services::forge::{
    Forge, MergeState, PrDetails, PullRequest, QueueEjection, Repo,
};
use nook_control::services::work_source::WorkItem;
use nook_control::services::{jobs, pr_hygiene, run_reconcile};
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

/// One forge for both halves — the hygiene pass holds it as a `&dyn Forge` and
/// the build converger reaches the same instance through `ReviewDemand`, so
/// what the pass writes is what the converger then sees.
#[derive(Clone, Default)]
struct FakeForge {
    prs: Arc<Mutex<Vec<PullRequest>>>,
    mergeable: Arc<Mutex<HashMap<u64, Option<bool>>>>,
    /// Per PR, because only the one with a card carries a `Closes` line — the
    /// mirror's only join.
    bodies: Arc<Mutex<HashMap<u64, String>>>,
    comments: Arc<Mutex<HashMap<u64, Vec<String>>>>,
    /// What the merge queue last did, as `queue_ejection` reports it.
    ejections: Arc<Mutex<HashMap<u64, QueueEjection>>>,
}

impl FakeForge {
    fn new(prs: Vec<PullRequest>) -> Self {
        Self {
            prs: Arc::new(Mutex::new(prs)),
            ..Default::default()
        }
    }
    fn set_body(&self, pr: u64, body: &str) {
        self.bodies.lock().unwrap().insert(pr, body.to_string());
    }
    fn set_mergeable(&self, pr: u64, m: Option<bool>) {
        self.mergeable.lock().unwrap().insert(pr, m);
    }
    /// The queue threw this PR out at `head`, for GitHub's `reason`.
    fn eject(&self, pr: u64, head: &str, reason: &str) {
        self.ejections.lock().unwrap().insert(
            pr,
            QueueEjection {
                head_sha: head.into(),
                reason: reason.into(),
            },
        );
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
    /// The builder pushed its rebase: the head moves, its label comes off, and
    /// the ejection is about a head that no longer exists — which is exactly
    /// what the forge stops reporting.
    fn push(&self, pr: u64, head: &str) {
        let mut prs = self.prs.lock().unwrap();
        if let Some(p) = prs.iter_mut().find(|p| p.number == pr) {
            p.head_sha = head.into();
            p.labels.retain(|l| l != "loop-changes-requested");
        }
        self.ejections.lock().unwrap().remove(&pr);
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
            body: self
                .bodies
                .lock()
                .unwrap()
                .get(&number)
                .cloned()
                .unwrap_or_default(),
            merge_state: MergeState::Open,
        })
    }
    async fn queue_ejection(
        &self,
        _repo: &Repo,
        number: u64,
    ) -> anyhow::Result<Option<QueueEjection>> {
        Ok(self.ejections.lock().unwrap().get(&number).cloned())
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

/// The ejected pull request, and the conflicting one it is told apart from.
const PR: u64 = 7;
const OTHER_PR: u64 = 8;

struct Fixture {
    tenant: TenantId,
    user: UserId,
    ws: WorkspaceId,
    task: TaskId,
    body: String,
}

/// A tenant, a GitHub-remoted workspace, and one card parked in In Review with
/// PR #7 recorded on it — the shape `pr_opened` leaves behind, and the shape a
/// PR reaches the merge queue in.
async fn fixture(bed: &TestBed) -> Fixture {
    let tenant = bed.tenant("ejection").await;
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
    let key = format!("E{}", &board.0.simple().to_string()[26..]).to_uppercase();
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
             VALUES ($1, $2, $3, $4, 'approved then ejected', 42, 'task', $5,
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
        body: format!("What changed\n\nCloses {key}-42\n\nRisk: Low"),
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

/// A completed review run an AGENT concluded, planted straight into the ledger.
async fn agent_verdict(
    bed: &TestBed,
    state: &nook_control::state::AppState,
    f: &Fixture,
    pr: u64,
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
            review_pr_number: Some(pr as i64),
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

/// Every row the CONTROL PLANE recorded for one PR, oldest first: `(head,
/// source, verdict, state, seed)`.
async fn recorded_rows(
    bed: &TestBed,
    f: &Fixture,
    pr: u64,
) -> Vec<(String, String, String, String, String)> {
    bed.db()
        .query_all(
            "SELECT review_head_sha, review_verdict_source, review_verdict, state, seed
               FROM loop_jobs
              WHERE workspace_id = $1 AND review_pr_number = $2
                AND review_verdict_source IS NOT NULL
              ORDER BY created_at, id",
            params![f.ws.0, pr as i64],
        )
        .await
        .expect("recorded rows")
}

/// The approved, ejected pull request as the queue leaves it: open, head
/// unmoved, still carrying the verdict the reviewer gave it.
async fn ejected_fixture(
    bed: &TestBed,
    state: &nook_control::state::AppState,
    f: &Fixture,
) -> FakeForge {
    agent_verdict(bed, state, f, PR, "aaa", "approved").await;
    let forge = FakeForge::new(vec![PullRequest {
        number: PR,
        head_sha: "aaa".into(),
        labels: vec!["loop-approved".into()],
    }]);
    forge.set_body(PR, &f.body);
    forge
}

/// AC-1/AC-2/AC-4/AC-5: an ejected PR is recorded as rejected at the head the
/// queue threw out, ONE repair follows on the next converge with no human
/// action, that repair's instruction says what actually happened, and a second
/// pass adds nothing.
#[tokio::test]
async fn an_ejection_raises_exactly_one_repair_that_names_the_post_merge_failure() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    let forge = ejected_fixture(&bed, &state, &f).await;
    let state = with_forge(&state, &forge);

    // Before the queue took it: approved, green, nothing owing.
    let quiet = jobs::converge_builds(&state, f.tenant, f.user, f.ws, None)
        .await
        .expect("converge");
    assert_eq!(
        quiet.raised, 0,
        "an approved, unqueued PR is not repair work"
    );

    forge.eject(PR, "aaa", "failed_checks");
    let prs = forge.prs_needing_review(&repo()).await.unwrap();
    let healed = pr_hygiene::heal(&state, &forge, &repo(), f.tenant, f.user, f.ws, &prs)
        .await
        .expect("heal");
    assert_eq!(
        (healed.recorded, healed.marked),
        (1, 1),
        "the row the queue reads AND the label a human reads"
    );

    let rows = recorded_rows(&bed, &f, PR).await;
    assert_eq!(rows.len(), 1, "one recorded rejection");
    assert_eq!(rows[0].0, "aaa", "recorded at the EJECTED head");
    assert_eq!(
        rows[0].1, EJECTION_VERDICT_SOURCE,
        "AC-4: the row names queue ejection as its cause"
    );
    assert_eq!(rows[0].2, "changes_requested");
    assert_eq!(rows[0].3, "completed");
    assert!(
        rows[0].4.contains("ejected from the merge queue")
            && rows[0].4.contains("failed_checks")
            && rows[0].4.contains("no agent read this head"),
        "AC-4: the row names its cause, quotes the forge's reason, and disclaims findings: {}",
        rows[0].4
    );

    // The PR comment is the contract the builder reads, and it has to say the
    // one thing re-running this branch's checks would never reveal.
    let comment = forge.comments_of(PR).join("\n");
    assert!(
        comment.starts_with(&format!("{} aaa", pr_hygiene::EJECTION_MARK)),
        "a marker of its own, per head: {comment}"
    );
    assert!(
        comment.contains("CURRENT base branch")
            && comment.contains("nothing wrong")
            && comment.contains("failed_checks"),
        "AC-5, on the pull request: {comment}"
    );

    // AC-1: the very next converge raises the repair, with no human action.
    let c = jobs::converge_builds(&state, f.tenant, f.user, f.ws, None)
        .await
        .expect("converge");
    assert_eq!(c.raised, 1, "the ejected PR is repair work now");
    assert_eq!(
        c.jobs[0].build_fingerprint.as_deref(),
        Some("repair:aaa"),
        "AC-3: fingerprinted on the ejected head, which the rebase moves"
    );
    // AC-5: the run's own instruction, which is what the builder is started
    // with. A repair that only says "PR #7" sends it to re-run green checks.
    let seed = c.jobs[0].seed.clone().expect("the repair is seeded");
    assert!(
        seed.contains("merge queue EJECTED it")
            && seed.contains("CURRENT base")
            && seed.contains("Rebase onto the current base branch"),
        "AC-5: the instruction says what actually happened: {seed}"
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

    // AC-2: a second pass at the same head records nothing further, and no
    // second repair follows it.
    let prs = forge.prs_needing_review(&repo()).await.unwrap();
    let healed = pr_hygiene::heal(&state, &forge, &repo(), f.tenant, f.user, f.ws, &prs)
        .await
        .expect("heal 2");
    assert_eq!(healed.recorded, 0, "one repair per ejected head");
    assert_eq!(recorded_rows(&bed, &f, PR).await.len(), 1);
    assert_eq!(forge.comments_of(PR).len(), 1, "one ejection comment");
    let again = jobs::converge_builds(&state, f.tenant, f.user, f.ws, None)
        .await
        .expect("converge 2");
    assert_eq!(again.raised, 0, "no duplicate repair");

    bed.teardown().await;
}

/// AC-3/AC-7: the repair's push moves the head, which clears the repair
/// fingerprint by itself — and the ordinary review path is owed the REBASED
/// head, never the ejected one. No review run is raised to diagnose the
/// ejection.
#[tokio::test]
async fn the_rebase_clears_the_repair_and_hands_the_new_head_to_review() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    let forge = ejected_fixture(&bed, &state, &f).await;
    let state = with_forge(&state, &forge);
    forge.eject(PR, "aaa", "failed_checks");

    let prs = forge.prs_needing_review(&repo()).await.unwrap();
    pr_hygiene::heal(&state, &forge, &repo(), f.tenant, f.user, f.ws, &prs)
        .await
        .expect("heal");
    let c = jobs::converge_builds(&state, f.tenant, f.user, f.ws, None)
        .await
        .expect("converge");
    assert_eq!(c.raised, 1);

    // AC-7: the ejected head counts as concluded, so no review run is raised to
    // look at an ejection — and the head the builder is about to push owes one.
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
        unblocked_at: None,
    };
    let now = chrono::Utc::now();
    assert!(
        run_reconcile::owed(&[review_item("aaa")], &heads, 1, now)
            .0
            .is_empty(),
        "AC-7: the ejected head owes no review"
    );
    assert_eq!(
        run_reconcile::owed(&[review_item("bbb")], &heads, 1, now)
            .0
            .len(),
        1,
        "AC-7: the head the rebase will push does"
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
        "a PR the queue is no longer holding an ejection against is not repair work"
    );
    assert_eq!(
        recorded_rows(&bed, &f, PR).await.len(),
        1,
        "and the old rejection is not re-recorded at the new head"
    );

    bed.teardown().await;
}

/// AC-6: `needs-human-review` opts the PR out of all of this, unchanged. A
/// person owns it, and neither the record nor the label is ours to write.
#[tokio::test]
async fn an_escalated_pr_records_nothing() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;

    let forge = FakeForge::new(vec![PullRequest {
        number: PR,
        head_sha: "aaa".into(),
        labels: vec!["needs-human-review".into()],
    }]);
    forge.set_body(PR, &f.body);
    forge.eject(PR, "aaa", "failed_checks");
    let state = with_forge(&state, &forge);

    let prs = forge.prs_needing_review(&repo()).await.unwrap();
    let healed = pr_hygiene::heal(&state, &forge, &repo(), f.tenant, f.user, f.ws, &prs)
        .await
        .expect("heal");
    assert_eq!((healed.recorded, healed.marked, healed.restored), (0, 0, 0));
    assert!(recorded_rows(&bed, &f, PR).await.is_empty());
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

/// AC-2's other half: a head that already carries a `changes_requested` — an
/// agent's here — is in the repair queue on its own account. The ejection
/// records nothing on top of it; a second row saying the same thing would only
/// compete to be the newest one read.
///
/// The PR reaches the check carrying a stale `loop-approved`, which is what
/// leaves the ledger and the label disagreeing in the first place.
#[tokio::test]
async fn an_existing_rejection_at_that_head_is_left_alone() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    agent_verdict(&bed, &state, &f, PR, "aaa", "changes_requested").await;

    let forge = FakeForge::new(vec![PullRequest {
        number: PR,
        head_sha: "aaa".into(),
        labels: vec!["loop-approved".into()],
    }]);
    forge.set_body(PR, &f.body);
    forge.eject(PR, "aaa", "failed_checks");
    let state = with_forge(&state, &forge);

    let prs = forge.prs_needing_review(&repo()).await.unwrap();
    let healed = pr_hygiene::heal(&state, &forge, &repo(), f.tenant, f.user, f.ws, &prs)
        .await
        .expect("heal");
    assert_eq!(healed.recorded, 0, "the agent's rejection stands alone");
    assert_eq!(healed.marked, 1, "the label still catches up to the ledger");
    assert!(
        recorded_rows(&bed, &f, PR).await.is_empty(),
        "nothing was recorded on top of the agent's verdict"
    );

    // And the repair the agent's own verdict earns still comes — one, at the
    // head it rejected, instructed as a reviewer's repair and not as an
    // ejection's.
    let c = jobs::converge_builds(&state, f.tenant, f.user, f.ws, None)
        .await
        .expect("converge");
    assert_eq!(c.raised, 1);
    assert_eq!(c.jobs[0].build_fingerprint.as_deref(), Some("repair:aaa"));
    assert_eq!(c.jobs[0].seed.as_deref(), Some("repair PR #7"));

    bed.teardown().await;
}

/// AC-4: three causes, three honest labels. An ejection-caused row and a
/// conflict-caused row sit side by side in the workspace's review listing and
/// say which they are — and an agent's says nothing, because an agent's is the
/// only one that was actually reviewed.
#[tokio::test]
async fn an_ejection_row_and_a_conflict_row_are_told_apart_in_the_listing() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    agent_verdict(&bed, &state, &f, PR, "aaa", "approved").await;

    let forge = FakeForge::new(vec![
        PullRequest {
            number: PR,
            head_sha: "aaa".into(),
            labels: vec!["loop-approved".into()],
        },
        PullRequest {
            number: OTHER_PR,
            head_sha: "bbb".into(),
            labels: vec!["loop-approved".into()],
        },
    ]);
    forge.set_body(PR, &f.body);
    forge.eject(PR, "aaa", "failed_checks");
    forge.set_mergeable(OTHER_PR, Some(false));
    let state = with_forge(&state, &forge);

    let prs = forge.prs_needing_review(&repo()).await.unwrap();
    let healed = pr_hygiene::heal(&state, &forge, &repo(), f.tenant, f.user, f.ws, &prs)
        .await
        .expect("heal");
    assert_eq!(healed.recorded, 2, "one row per PR, one cause each");

    let listed = state
        .jobs
        .list_reviews_for_workspace(f.tenant, f.ws, &nook_testkit::first_page(50))
        .await
        .expect("listing")
        .rows;
    let source_of = |pr: i64| -> Option<String> {
        listed
            .iter()
            .find(|r| r.job.review_pr_number == Some(pr) && r.job.review_verdict_source.is_some())
            .and_then(|r| r.job.review_verdict_source.clone())
    };
    assert_eq!(
        source_of(PR as i64).as_deref(),
        Some(EJECTION_VERDICT_SOURCE)
    );
    assert_eq!(
        source_of(OTHER_PR as i64).as_deref(),
        Some(CONFLICT_VERDICT_SOURCE)
    );
    assert!(
        listed
            .iter()
            .any(|r| r.job.review_verdict.as_deref() == Some("approved")
                && r.job.review_verdict_source.is_none()),
        "and the agent's own verdict claims no cause at all"
    );

    // The comments the two PRs carry are the same statement, made to a human.
    assert!(forge.comments_of(PR)[0].contains(pr_hygiene::EJECTION_MARK));
    assert!(forge.comments_of(OTHER_PR)[0].contains(pr_hygiene::CONFLICT_MARK));

    bed.teardown().await;
}
