//! MAIN-477 AC-3: passes that conclude nothing post nothing, anywhere.
//!
//! A `skipped` verdict defers to a review already on the PR: it must record
//! itself (so the head counts as reviewed and the wakeup rule rests) while
//! touching neither GitHub nor the board — proven here by running WITHOUT any
//! forge configured, where a posting path would fail loudly. A run that
//! records no verdict at all never reaches `record_verdict`, and its silence
//! is the absence this test's fixture demonstrates by construction.
//!
//! Engine-neutral (MAIN-264): nothing here names a `sqlx` type.

use nook_control::services::jobs;
use nook_testkit::TestBed;
use nook_types::*;

#[tokio::test]
async fn a_skipped_verdict_records_itself_and_posts_nothing() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("vsilent").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    let job = state
        .jobs
        .create(nook_control::repo::jobs::NewLoopJob {
            id: JobId::new(),
            tenant,
            kind: "review".into(),
            target_task_id: None,
            workspace_id: Some(ws),
            requested_by: user,
            seed: None,
            predecessor_job_id: None,
            review_pr_number: Some(7),
            review_head_sha: Some("aaa111".into()),
            review_forced: false,
            build_fingerprint: None,
        })
        .await
        .expect("review run");
    state
        .jobs
        .transition(job.id, "running")
        .await
        .expect("running");

    // No forge is configured in this bed: a skipped verdict must not need one.
    let done = jobs::record_verdict(
        &state,
        tenant,
        job.id,
        &ReviewVerdictRequest {
            verdict: "skipped".into(),
            body: None,
        },
    )
    .await
    .expect("a skip records without touching GitHub or the board");
    assert_eq!(done.review_verdict.as_deref(), Some("skipped"));

    bed.teardown().await;
}
