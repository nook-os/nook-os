//! MAIN-473: the manual re-review lever. A verdicted head rests under the
//! reconciler's rule — correct until the evidence under the verdict changes
//! (a CI rerun turning green at the same sha). `--force` is the human's way
//! out that is not an empty amend; everything it does NOT change — the
//! unforced decline, the one-live-run refusal — is pinned here too.

use nook_control::repo::jobs::NewLoopJob;
use nook_control::services::jobs;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

/// A forge serving one open PR at a fixed head.
struct StaticForge(Vec<nook_control::services::forge::PullRequest>);

#[async_trait::async_trait]
impl nook_control::services::forge::Forge for StaticForge {
    async fn prs_needing_review(
        &self,
        _repo: &nook_control::services::forge::Repo,
    ) -> anyhow::Result<Vec<nook_control::services::forge::PullRequest>> {
        Ok(self.0.clone())
    }
}

struct Fixture {
    tenant: TenantId,
    user: UserId,
    ws: WorkspaceId,
    state: nook_control::state::AppState,
}

/// A workspace on a GitHub remote whose forge reports PR #7 at head `aaa`.
async fn fixture(bed: &TestBed) -> Fixture {
    let tenant = bed.tenant("force").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    bed.db()
        .exec(
            "UPDATE workspaces SET git_remote_url = 'git@github.com:acme/api.git' WHERE id = $1",
            params![ws],
        )
        .await
        .expect("remote");
    let mut state = bed.app_state().await;
    state.review_demand = std::sync::Arc::new(nook_control::services::forge::ReviewDemand::new(
        Some(Box::new(StaticForge(vec![
            nook_control::services::forge::PullRequest {
                number: 7,
                head_sha: "aaa".into(),
                labels: vec![],
            },
        ]))),
        std::time::Duration::ZERO,
    ));
    Fixture {
        tenant,
        user,
        ws,
        state,
    }
}

/// A completed review run with a recorded verdict at `head` — the resting
/// state force exists to overrule.
async fn verdicted_run(bed: &TestBed, f: &Fixture, head: &str) {
    let job = f
        .state
        .jobs
        .create(NewLoopJob {
            id: JobId(Uuid::now_v7()),
            tenant: f.tenant,
            kind: "review".into(),
            target_task_id: None,
            workspace_id: Some(f.ws),
            requested_by: f.user,
            seed: None,
            predecessor_job_id: None,
            review_pr_number: Some(7),
            review_head_sha: Some(head.into()),
            build_fingerprint: None,
            review_forced: false,
        })
        .await
        .expect("run");
    bed.db()
        .exec(
            "UPDATE loop_jobs SET state = 'completed', review_verdict = 'changes_requested'
              WHERE id = $1",
            params![job.id],
        )
        .await
        .expect("verdict");
}

/// AC-1 + AC-4's first two clauses: the unforced targeted enqueue still
/// declines a verdicted head; force raises exactly one directed run at it.
#[tokio::test]
async fn force_raises_at_an_unchanged_verdicted_head_and_unforced_declines() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    verdicted_run(&bed, &f, "aaa").await;

    let declined = jobs::enqueue_review(&f.state, f.tenant, f.user, f.ws, None, Some(7), false)
        .await
        .expect("unforced enqueue answers");
    assert_eq!(
        declined.raised, 0,
        "the reconciler's rule stands unforced: a verdicted head rests"
    );

    let forced = jobs::enqueue_review(&f.state, f.tenant, f.user, f.ws, None, Some(7), true)
        .await
        .expect("forced enqueue");
    assert_eq!(forced.raised, 1);
    let job = &forced.jobs[0];
    assert_eq!(job.review_pr_number, Some(7));
    assert_eq!(job.review_head_sha.as_deref(), Some("aaa"));
    assert_eq!(job.kind, "review");

    bed.teardown().await;
}

/// AC-3 / AC-4's last clause: one live run per PR stands, forced or not, and
/// the refusal names the run to wait on.
#[tokio::test]
async fn a_live_run_refuses_force_by_id() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;

    // A live (queued) run holds the PR.
    let forced = jobs::enqueue_review(&f.state, f.tenant, f.user, f.ws, None, Some(7), true)
        .await
        .expect("first force raises");
    let live = forced.jobs[0].id;

    let refused = jobs::enqueue_review(&f.state, f.tenant, f.user, f.ws, None, Some(7), true).await;
    match refused {
        Err(nook_control::error::ApiError::Conflict(msg)) => {
            assert!(
                msg.contains(&live.0.to_string()),
                "the refusal names the live run: {msg}"
            );
        }
        other => panic!("expected Conflict naming the live run, got {other:?}"),
    }

    bed.teardown().await;
}

/// Force is a scalpel: without a PR number it is refused outright, and a PR
/// the forge does not report is named rather than silently skipped.
#[tokio::test]
async fn force_needs_a_pr_and_an_unknown_pr_is_named() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;

    let refused = jobs::enqueue_review(&f.state, f.tenant, f.user, f.ws, None, None, true).await;
    assert!(
        matches!(refused, Err(nook_control::error::ApiError::BadRequest(ref m)) if m.contains("PR number")),
        "blanket force refused: {refused:?}"
    );

    let refused =
        jobs::enqueue_review(&f.state, f.tenant, f.user, f.ws, None, Some(99), true).await;
    assert!(
        matches!(refused, Err(nook_control::error::ApiError::BadRequest(ref m)) if m.contains("99")),
        "unknown PR named: {refused:?}"
    );

    bed.teardown().await;
}

/// The workspace ceiling stands on the forced path (round-2 must-fix): `0` is
/// the workspace-level kill switch and refuses outright, and a full ceiling
/// refuses rather than silently exceeding the declared cap.
#[tokio::test]
async fn force_honours_the_review_ceiling_including_off() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;

    // Ceiling 0: reviews are OFF for this repo; force does not override it.
    bed.db()
        .exec(
            "UPDATE workspaces SET review_loop_max_replicas = 0 WHERE id = $1",
            params![f.ws],
        )
        .await
        .expect("ceiling 0");
    let refused = jobs::enqueue_review(&f.state, f.tenant, f.user, f.ws, None, Some(7), true).await;
    assert!(
        matches!(refused, Err(nook_control::error::ApiError::Conflict(ref m)) if m.contains("off")),
        "ceiling 0 refuses a force: {refused:?}"
    );

    // Ceiling 1 with another PR's run live: full, so a force for PR #7 refuses
    // rather than raising a second concurrent run.
    bed.db()
        .exec(
            "UPDATE workspaces SET review_loop_max_replicas = 1 WHERE id = $1",
            params![f.ws],
        )
        .await
        .expect("ceiling 1");
    let other = f
        .state
        .jobs
        .create(NewLoopJob {
            id: JobId(Uuid::now_v7()),
            tenant: f.tenant,
            kind: "review".into(),
            target_task_id: None,
            workspace_id: Some(f.ws),
            requested_by: f.user,
            seed: None,
            predecessor_job_id: None,
            review_pr_number: Some(8),
            review_head_sha: Some("bbb".into()),
            review_forced: false,
            build_fingerprint: None,
        })
        .await
        .expect("live run for PR 8");
    let refused = jobs::enqueue_review(&f.state, f.tenant, f.user, f.ws, None, Some(7), true).await;
    assert!(
        matches!(refused, Err(nook_control::error::ApiError::Conflict(ref m)) if m.contains("ceiling")),
        "a full ceiling refuses a force for another PR: {refused:?}"
    );

    // Room again (the other run ends): the same force raises, and the raised
    // row carries the forced flag the run's environment is built from.
    bed.db()
        .exec(
            "UPDATE loop_jobs SET state = 'completed' WHERE id = $1",
            params![other.id],
        )
        .await
        .expect("finish PR 8 run");
    let forced = jobs::enqueue_review(&f.state, f.tenant, f.user, f.ws, None, Some(7), true)
        .await
        .expect("force raises once there is room");
    assert_eq!(forced.raised, 1);
    assert!(
        forced.jobs[0].review_forced,
        "the job row records that a human forced it"
    );

    bed.teardown().await;
}
