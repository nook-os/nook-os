//! Dispatch pauses while a workspace's own default branch is red (MAIN-543).
//!
//! The pause is DERIVED on every pass and stored nowhere, so the tests here are
//! written the way the feature is: nothing is ever cleared between a red pass
//! and a green one — the forge simply answers differently, and the next pass
//! places the job.
//!
//! Needs Postgres: `DATABASE_URL` (`NOOK_REQUIRE_DB=1` in the suite).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use nook_control::services::forge::{CiRun, Forge, Repo, ReviewDemand};
use nook_control::services::{jobs, loops};
use nook_control::state::AppState;
use nook_db::{params, Db};
use nook_types::*;
use serde_json::json;
use uuid::Uuid;

use nook_testkit::TestBed;

/// A forge whose default-branch answer the test dictates, per repository, and
/// which counts how often it was asked.
struct CiForge {
    answers: Mutex<std::collections::HashMap<String, Result<Option<CiRun>, String>>>,
    calls: Arc<AtomicUsize>,
}

impl CiForge {
    /// Not `new`: it hands back the call counter alongside the forge, which is
    /// the shape clippy asks about — the same naming `forge.rs`'s own fake uses.
    fn answering(
        answers: Vec<(&str, Result<Option<CiRun>, String>)>,
    ) -> (Box<dyn Forge>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Box::new(CiForge {
                answers: Mutex::new(
                    answers
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v))
                        .collect(),
                ),
                calls: calls.clone(),
            }),
            calls,
        )
    }
}

#[async_trait::async_trait]
impl Forge for CiForge {
    async fn prs_needing_review(
        &self,
        _repo: &Repo,
    ) -> anyhow::Result<Vec<nook_control::services::forge::PullRequest>> {
        Ok(vec![])
    }

    async fn default_branch_ci(&self, repo: &Repo) -> anyhow::Result<Option<CiRun>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.answers.lock().unwrap().get(&repo.name).cloned() {
            Some(Ok(run)) => Ok(run),
            Some(Err(e)) => anyhow::bail!("{e}"),
            // A repository the test said nothing about has no CI at all.
            None => Ok(None),
        }
    }
}

fn red_run() -> CiRun {
    CiRun {
        branch: "main".into(),
        workflow: "CI".into(),
        conclusion: "failure".into(),
        url: "https://github.com/acme/api/actions/runs/77".into(),
        head_sha: "badc0ffee00".into(),
    }
}

fn green_run() -> CiRun {
    CiRun {
        conclusion: "success".into(),
        ..red_run()
    }
}

/// Capabilities for a node that may take build work.
fn build_caps() -> serde_json::Value {
    json!({
        "loop_kinds": ["build"],
        "runtime_auth": [
            { "id": "claude", "label": "Claude Code", "runtime": "claude", "state": "authorized" }
        ]
    })
}

/// An online, owned, authorized, `role=build`-labelled node — everything
/// placement wants, so the trunk's health is the only variable in these tests.
async fn build_node(bed: &TestBed, tenant: TenantId, owner: Uuid) -> NodeId {
    let id = NodeId::new();
    bed.db()
        .exec(
            "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status, owner_person_id,
                                capabilities, labels)
             VALUES ($1,$2,$3,$4,'online',$5,$6,'{\"role\": \"build\"}')",
            params![
                id,
                tenant,
                format!("n-{}", id.0.simple()),
                format!("h-{}", id.0.simple()),
                owner,
                build_caps()
            ],
        )
        .await
        .expect("node");
    id
}

/// A board + column + task to anchor a build job on.
async fn target_task(bed: &TestBed, tenant: TenantId, creator: UserId) -> TaskId {
    let board = BoardId::new();
    bed.db()
        .exec(
            "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b',$3,'local')",
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
             VALUES ($1,$2,'Todo',0,'unstarted')",
            params![col, board],
        )
        .await
        .expect("column");
    let task = TaskId::new();
    bed.db()
        .exec(
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, type, created_by)
             VALUES ($1,$2,$3,$4,'t','task',$5)",
            params![task, tenant, board, col, creator],
        )
        .await
        .expect("task");
    task
}

/// A queued BUILD job for `ws`, targeting a fresh card.
async fn queued_build(
    bed: &TestBed,
    tenant: TenantId,
    user: UserId,
    ws: WorkspaceId,
) -> (JobId, TaskId) {
    let task = target_task(bed, tenant, user).await;
    let id = JobId::new();
    bed.db()
        .exec(
            "INSERT INTO loop_jobs (id, tenant_id, kind, target_task_id, workspace_id,
                                    requested_by, state)
             VALUES ($1,$2,'build',$3,$4,$5,'queued')",
            params![id, tenant, task, ws, user],
        )
        .await
        .expect("build job");
    (id, task)
}

/// A workspace pointing at `owner/name` on GitHub.
async fn repo_workspace(bed: &TestBed, tenant: TenantId, name: &str) -> WorkspaceId {
    let ws = bed.workspace(tenant).await;
    bed.db()
        .exec(
            "UPDATE workspaces SET git_remote_url = $2 WHERE id = $1",
            params![ws, format!("git@github.com:acme/{name}.git")],
        )
        .await
        .expect("remote");
    ws
}

/// The state with a dictated forge behind it. TTL zero, because the forge cache
/// this shares a home with must never stand between a green trunk and the next
/// pass.
fn with_ci(state: &AppState, forge: Box<dyn Forge>) -> AppState {
    let mut state = state.clone();
    state.review_demand = Arc::new(ReviewDemand::new(Some(forge), std::time::Duration::ZERO));
    state
}

/// AC-1 + AC-2: a red trunk holds the build queued, and a green one places it
/// on the very NEXT pass — no human action, no flag to clear, no restart. The
/// only thing that changes between the two calls is what the forge says.
#[tokio::test]
async fn a_red_trunk_holds_the_build_and_a_green_one_places_it_next_pass() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("redmain").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let ws = repo_workspace(&bed, tenant, "api").await;
    let node = build_node(&bed, tenant, person).await;
    let base = bed.app_state().await;
    let (job, _task) = queued_build(&bed, tenant, user, ws).await;

    let (forge, _) = CiForge::answering(vec![("api", Ok(Some(red_run())))]);
    let held = jobs::place_queued_in_order(&with_ci(&base, forge), tenant)
        .await
        .expect("pass");
    assert!(held.is_empty(), "a red trunk places nothing");

    let held = base.jobs.reload(job).await.expect("reload");
    assert_eq!(held.state, "queued");
    let reason = held.queued_reason.clone().unwrap_or_default();
    assert!(
        reason.contains("actions/runs/77"),
        "AC-6: the reason names the failing run: {reason}"
    );
    assert!(
        !reason.contains("no eligible executor"),
        "AC-6: and does not read as any existing cause: {reason}"
    );

    // Nothing is touched in between — the trunk simply goes green.
    let (forge, _) = CiForge::answering(vec![("api", Ok(Some(green_run())))]);
    let placed = jobs::place_queued_in_order(&with_ci(&base, forge), tenant)
        .await
        .expect("pass");
    assert_eq!(placed.len(), 1, "AC-2: the next pass dispatches by itself");
    assert_eq!(placed[0].id, job);
    assert_eq!(placed[0].executor_node_id, Some(node));

    bed.teardown().await;
}

/// AC-3: every unreadable signal dispatches. A forge error, a branch with
/// nothing completed yet, and a deployment with no forge at all are three
/// spellings of "we do not know", and none of them may stop the fleet.
#[tokio::test]
async fn an_unreadable_signal_dispatches() {
    for answer in [
        Err("502 upstream".to_string()),
        Ok(None),
        Ok(Some(CiRun {
            conclusion: "cancelled".into(),
            ..red_run()
        })),
    ] {
        let Some(mut bed) = TestBed::new().await else {
            return;
        };
        let tenant = bed.tenant("failopen").await;
        let (user, person) = bed.user(tenant, "owner").await;
        let ws = repo_workspace(&bed, tenant, "api").await;
        let _node = build_node(&bed, tenant, person).await;
        let base = bed.app_state().await;
        let (job, _task) = queued_build(&bed, tenant, user, ws).await;

        let (forge, _) = CiForge::answering(vec![("api", answer.clone())]);
        let placed = jobs::place_queued_in_order(&with_ci(&base, forge), tenant)
            .await
            .expect("pass");
        assert_eq!(
            placed.len(),
            1,
            "AC-3: {answer:?} is unknown, and unknown dispatches"
        );
        assert_eq!(placed[0].id, job);

        bed.teardown().await;
    }
}

/// AC-3 again, from the other end: a deployment with NO forge, and a workspace
/// with no forge remote at all, both dispatch. The pause can only ever be a
/// definite answer.
#[tokio::test]
async fn no_forge_and_no_remote_both_dispatch() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("noforge").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let with_remote = repo_workspace(&bed, tenant, "api").await;
    let no_remote = bed.workspace(tenant).await;
    let _node = build_node(&bed, tenant, person).await;
    let base = bed.app_state().await;
    let (a, _) = queued_build(&bed, tenant, user, with_remote).await;
    let (b, _) = queued_build(&bed, tenant, user, no_remote).await;

    // No forge on the deployment at all: the pre-MAIN-543 behaviour exactly.
    let mut state = base.clone();
    state.review_demand = Arc::new(ReviewDemand::new(None, std::time::Duration::ZERO));
    let placed = jobs::place_queued_in_order(&state, tenant)
        .await
        .expect("pass");
    let ids: Vec<JobId> = placed.iter().map(|j| j.id).collect();
    assert!(ids.contains(&a), "no forge dispatches");

    // And with a forge that would say red, the workspace carrying no remote is
    // not something it can be asked about.
    let (forge, calls) = CiForge::answering(vec![("api", Ok(Some(red_run())))]);
    jobs::place_queued_in_order(&with_ci(&base, forge), tenant)
        .await
        .expect("pass");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "b was already placed");
    let placed_b = base.jobs.reload(b).await.expect("reload");
    assert_eq!(placed_b.state, "claimed", "no remote, nothing to read");

    bed.teardown().await;
}

/// AC-4: a run still in progress is not a verdict. The forge is asked for
/// COMPLETED runs only, so an unfinished one never reaches the rule — the
/// completed answer beneath it is what counts, and a green one dispatches even
/// while a later run is mid-flight.
#[tokio::test]
async fn an_in_progress_run_does_not_extend_the_pause() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("inflight").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let ws = repo_workspace(&bed, tenant, "api").await;
    let _node = build_node(&bed, tenant, person).await;
    let base = bed.app_state().await;
    let (job, _task) = queued_build(&bed, tenant, user, ws).await;

    // The fix is building: the latest COMPLETED run is the green one before it.
    let (forge, _) = CiForge::answering(vec![("api", Ok(Some(green_run())))]);
    let placed = jobs::place_queued_in_order(&with_ci(&base, forge), tenant)
        .await
        .expect("pass");
    assert_eq!(placed.len(), 1, "a build in flight pauses nothing");
    assert_eq!(placed[0].id, job);

    bed.teardown().await;
}

/// AC-5: the pause stops CLAIMING and nothing else. A run already in flight
/// when the trunk went red is left exactly where it was — the same semantics as
/// a node cordon at capacity `0`.
#[tokio::test]
async fn a_run_already_in_flight_is_untouched() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("inflight2").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let ws = repo_workspace(&bed, tenant, "api").await;
    let node = build_node(&bed, tenant, person).await;
    let base = bed.app_state().await;
    let (running, _task) = queued_build(&bed, tenant, user, ws).await;
    bed.db()
        .exec(
            "UPDATE loop_jobs SET state = 'running', executor_node_id = $2 WHERE id = $1",
            params![running, node],
        )
        .await
        .expect("in flight");

    let (forge, _) = CiForge::answering(vec![("api", Ok(Some(red_run())))]);
    jobs::place_queued_in_order(&with_ci(&base, forge), tenant)
        .await
        .expect("pass");

    let after = base.jobs.reload(running).await.expect("reload");
    assert_eq!(after.state, "running", "AC-5: finish what you hold");
    assert_eq!(after.executor_node_id, Some(node));
    assert!(
        after.queued_reason.is_none(),
        "and nothing is written on it"
    );

    bed.teardown().await;
}

/// AC-7: per workspace, never global. One repo's red trunk says nothing about
/// another's, even in the same tenant and the same dispatch pass.
#[tokio::test]
async fn one_repos_red_trunk_does_not_stop_another() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("perws").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let red = repo_workspace(&bed, tenant, "api").await;
    let green = repo_workspace(&bed, tenant, "web").await;
    // Two slots, so capacity is never what decides this.
    let node = build_node(&bed, tenant, person).await;
    bed.db()
        .exec(
            r#"UPDATE nodes SET capabilities = capabilities || '{"max_loop_jobs": 2}'::jsonb
               WHERE id = $1"#,
            params![node],
        )
        .await
        .expect("capacity");
    let base = bed.app_state().await;
    let (held, _) = queued_build(&bed, tenant, user, red).await;
    let (goes, _) = queued_build(&bed, tenant, user, green).await;

    let (forge, _) = CiForge::answering(vec![
        ("api", Ok(Some(red_run()))),
        ("web", Ok(Some(green_run()))),
    ]);
    let placed = jobs::place_queued_in_order(&with_ci(&base, forge), tenant)
        .await
        .expect("pass");
    let ids: Vec<JobId> = placed.iter().map(|j| j.id).collect();
    assert_eq!(ids, vec![goes], "AC-7: only the red repo is held");
    assert_eq!(
        base.jobs.reload(held).await.expect("reload").state,
        "queued"
    );

    bed.teardown().await;
}

/// NG-1: the pause is BUILD work only. A review run in the same workspace is a
/// separate judgement, and a red trunk is no reason to stop reviewing.
#[tokio::test]
async fn a_review_run_is_not_paused_by_a_red_trunk() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("reviewrun").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let ws = repo_workspace(&bed, tenant, "api").await;
    let id = NodeId::new();
    bed.db()
        .exec(
            "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status, owner_person_id,
                                capabilities, labels)
             VALUES ($1,$2,$3,$4,'online',$5,$6,'{\"role\": \"loop\"}')",
            params![
                id,
                tenant,
                format!("n-{}", id.0.simple()),
                format!("h-{}", id.0.simple()),
                person,
                json!({
                    "loop_kinds": ["review"],
                    "runtime_auth": [{ "id": "claude", "label": "Claude Code",
                                       "runtime": "claude", "state": "authorized" }]
                })
            ],
        )
        .await
        .expect("node");
    let base = bed.app_state().await;
    let job = JobId::new();
    bed.db()
        .exec(
            "INSERT INTO loop_jobs (id, tenant_id, kind, workspace_id, requested_by, state,
                                    review_pr_number, review_head_sha)
             VALUES ($1,$2,'review',$3,$4,'queued',7,'abc')",
            params![job, tenant, ws, user],
        )
        .await
        .expect("review job");

    let (forge, calls) = CiForge::answering(vec![("api", Ok(Some(red_run())))]);
    let placed = jobs::place_queued_in_order(&with_ci(&base, forge), tenant)
        .await
        .expect("pass");
    assert_eq!(placed.len(), 1, "NG-1: reviews keep running");
    assert_eq!(placed[0].id, job);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the trunk is not even read for a review run"
    );

    bed.teardown().await;
}

/// One forge read per workspace per pass, however many builds are queued for
/// it — the memo is what keeps a pause off the rate limit.
#[tokio::test]
async fn a_pass_reads_each_workspace_once() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("memo").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let ws = repo_workspace(&bed, tenant, "api").await;
    let _node = build_node(&bed, tenant, person).await;
    let base = bed.app_state().await;
    for _ in 0..3 {
        queued_build(&bed, tenant, user, ws).await;
    }

    let (forge, calls) = CiForge::answering(vec![("api", Ok(Some(red_run())))]);
    jobs::place_queued_in_order(&with_ci(&base, forge), tenant)
        .await
        .expect("pass");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "three jobs, one read");

    bed.teardown().await;
}

/// AC-9: a job the pause is holding is NOT starvation-escalated. Held across
/// the full window it keeps its card unlabelled and its run alive — otherwise a
/// trunk red for longer than `starve_secs` would hand every waiting card back
/// to a human, which is exactly what this card exists to avoid.
#[tokio::test]
async fn a_paused_job_is_never_starved_but_an_ordinary_one_still_is() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("starve").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let red = repo_workspace(&bed, tenant, "api").await;
    let green = repo_workspace(&bed, tenant, "web").await;
    let _node = build_node(&bed, tenant, person).await;
    let base = bed.app_state().await;
    loops::set(&*base.settings, tenant, true).await.expect("on");
    let (held, held_card) = queued_build(&bed, tenant, user, red).await;
    let (starved, starved_card) = queued_build(&bed, tenant, user, green).await;

    // Both have been waiting, unchanged, for longer than the window.
    for job in [held, starved] {
        bed.db()
            .exec(
                "UPDATE loop_jobs SET queued_reason = 'held', updated_at = $2 WHERE id = $1",
                params![job, chrono::Utc::now() - chrono::Duration::hours(3)],
            )
            .await
            .expect("age");
    }

    let (forge, _) = CiForge::answering(vec![
        ("api", Ok(Some(red_run()))),
        ("web", Ok(Some(green_run()))),
    ]);
    let state = with_ci(&base, forge);
    let ended = jobs::escalate_starved_queued(&state, tenant, 1_800)
        .await
        .expect("scan");
    assert_eq!(ended, 1, "only the job nothing is pausing is starved");

    assert_eq!(
        base.jobs.reload(held).await.expect("reload").state,
        "queued",
        "AC-9: the paused run is still alive"
    );
    let labels = card_labels(&bed, held_card).await;
    assert!(
        !labels.iter().any(|l| l == "blocked"),
        "AC-9: and its card is unlabelled: {labels:?}"
    );

    assert_eq!(
        base.jobs.reload(starved).await.expect("reload").state,
        "canceled",
        "MAIN-496's rule is otherwise untouched"
    );
    assert!(card_labels(&bed, starved_card)
        .await
        .iter()
        .any(|l| l == "blocked"));

    bed.teardown().await;
}

async fn card_labels(bed: &TestBed, task: TaskId) -> Vec<String> {
    bed.db()
        .query_scalar_all::<String>(
            "SELECT l.name FROM labels l JOIN task_labels tl ON tl.label_id = l.id
              WHERE tl.task_id = $1",
            params![task],
        )
        .await
        .expect("labels")
}
