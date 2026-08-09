//! Loop-job and interaction callers against the in-memory fakes, with **no
//! database at all** (MAIN-255 AC-3).
//!
//! Every rule pinned here is a `WHERE state = …` guard, and each exists because
//! two writers can arrive at once: two dispatchers placing the same job, two
//! people answering the same question, a reaper racing a job that just
//! finished. Without the guard the second write silently wins; with it, it
//! matches nothing and the caller can tell.
//!
//! `cargo test -p nook-control --test jobs_fake` passes with the database
//! stopped.

use nook_control::repo::jobs::{
    FakeInteractionRepository, FakeLoopJobRepository, InteractionRepository, LoopJobRepository,
    NewInteraction, NewLoopJob,
};
use nook_control::repo::nodes::{FakeNodeRepository, NodeRepository};
use nook_types::*;

fn tenant() -> TenantId {
    TenantId::new()
}

async fn queued(repo: &FakeLoopJobRepository, t: TenantId, task: TaskId) -> JobId {
    let id = JobId::new();
    repo.create(NewLoopJob {
        id,
        tenant: t,
        kind: "spec".into(),
        target_task_id: Some(task),
        workspace_id: None,
        requested_by: UserId::new(),
        seed: None,
        predecessor_job_id: None,
        review_pr_number: None,
        review_head_sha: None,
        build_fingerprint: None,
    })
    .await
    .unwrap();
    id
}

// ── placing a job ───────────────────────────────────────────────────────────

#[tokio::test]
async fn two_dispatchers_racing_the_same_job_produce_exactly_one_winner() {
    let repo = FakeLoopJobRepository::new();
    let t = tenant();
    let job = queued(&repo, t, TaskId::new()).await;
    let (a, b) = (NodeId::new(), NodeId::new());

    let first = repo.claim_for_executor(job, a).await.unwrap();
    assert!(first.is_some(), "the first dispatcher places it");
    assert_eq!(first.unwrap().executor_node_id, Some(a));

    let second = repo.claim_for_executor(job, b).await.unwrap();
    assert!(
        second.is_none(),
        "`AND state = 'queued'` matched nothing — a steal would otherwise \
         re-place a job already running elsewhere"
    );
    assert_eq!(repo.state_of(job).as_deref(), Some("claimed"));
}

#[tokio::test]
async fn placing_a_job_clears_the_reason_it_was_stuck() {
    let repo = FakeLoopJobRepository::new();
    let t = tenant();
    let job = queued(&repo, t, TaskId::new()).await;

    repo.set_queued_reason(job, "no eligible executor")
        .await
        .unwrap();
    assert_eq!(
        repo.queued_reason_of(job),
        Some(Some("no eligible executor".into()))
    );

    repo.claim_for_executor(job, NodeId::new()).await.unwrap();
    assert_eq!(
        repo.queued_reason_of(job),
        Some(None),
        "a placed job must not still explain why it could not be placed"
    );
}

#[tokio::test]
async fn a_reason_is_not_written_onto_a_job_that_has_already_been_placed() {
    let repo = FakeLoopJobRepository::new();
    let t = tenant();
    let job = queued(&repo, t, TaskId::new()).await;
    repo.claim_for_executor(job, NodeId::new()).await.unwrap();

    let wrote = repo
        .set_queued_reason(job, "no eligible executor")
        .await
        .unwrap();
    assert_eq!(
        wrote, 0,
        "the guard stops a slow placement attempt annotating a running job \
         with a stale excuse"
    );
    assert_eq!(repo.queued_reason_of(job), Some(None));
}

// ── the reaper ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn only_jobs_on_a_node_that_stopped_reporting_are_reaped() {
    let repo = FakeLoopJobRepository::new();
    let t = tenant();
    let (stale, live, silent) = (NodeId::new(), NodeId::new(), NodeId::new());
    repo.set_node_last_seen(stale, chrono::Utc::now() - chrono::Duration::hours(2));
    repo.set_node_last_seen(live, chrono::Utc::now());
    // `silent` never reported at all — no row, so the INNER JOIN drops it.

    let on_stale = queued(&repo, t, TaskId::new()).await;
    let on_live = queued(&repo, t, TaskId::new()).await;
    let on_silent = queued(&repo, t, TaskId::new()).await;
    repo.claim_for_executor(on_stale, stale).await.unwrap();
    repo.claim_for_executor(on_live, live).await.unwrap();
    repo.claim_for_executor(on_silent, silent).await.unwrap();

    let reaped = repo.reap_stale_executors(600).await.unwrap();
    assert_eq!(reaped.len(), 1);
    assert_eq!(reaped[0].id, on_stale);
    assert_eq!(repo.state_of(on_stale).as_deref(), Some("failed"));
    assert_eq!(repo.state_of(on_live).as_deref(), Some("claimed"));
    assert_eq!(
        repo.state_of(on_silent).as_deref(),
        Some("claimed"),
        "a node that never reported does not strand its jobs — the join is \
         INNER and guarded on `last_seen_at IS NOT NULL`"
    );
}

#[tokio::test]
async fn a_job_that_finished_before_the_sweep_is_left_alone() {
    let repo = FakeLoopJobRepository::new();
    let t = tenant();
    let node = NodeId::new();
    repo.set_node_last_seen(node, chrono::Utc::now() - chrono::Duration::hours(2));
    let job = queued(&repo, t, TaskId::new()).await;
    repo.claim_for_executor(job, node).await.unwrap();

    // It completed between scan and update.
    repo.force_state(job, "succeeded");

    let reaped = repo.reap_stale_executors(600).await.unwrap();
    assert!(
        reaped.is_empty(),
        "`state IN ('claimed','running')` is what stops a finished run being \
         marked failed by a late reaper"
    );
    assert_eq!(repo.state_of(job).as_deref(), Some("succeeded"));
    assert_eq!(
        repo.reap_stale_executors(600).await.unwrap().len(),
        0,
        "and a second reaper cannot double-fail it either"
    );
}

// ── scoping ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn another_tenants_job_is_invisible() {
    let repo = FakeLoopJobRepository::new();
    let (mine, theirs) = (tenant(), tenant());
    let task = TaskId::new();
    let job = queued(&repo, theirs, task).await;

    assert!(repo.get(mine, job).await.unwrap().is_none());
    assert!(repo.executor_of(mine, job).await.unwrap().is_none());
    assert!(repo.list_for_task(mine, task).await.unwrap().is_empty());
    assert!(
        repo.target_task_of_unscoped(job).await.unwrap().is_some(),
        "the unscoped lookup does find it — that is what its name says, and \
         the node socket authorizes on what comes back"
    );
}

#[tokio::test]
async fn executor_of_tells_no_such_job_apart_from_not_placed_yet() {
    let repo = FakeLoopJobRepository::new();
    let t = tenant();
    let job = queued(&repo, t, TaskId::new()).await;

    assert_eq!(
        repo.executor_of(t, job).await.unwrap(),
        Some(None),
        "the job exists and is unplaced"
    );
    assert_eq!(
        repo.executor_of(t, JobId::new()).await.unwrap(),
        None,
        "no such job — collapsing these two would let an unplaced job pass a \
         'is this node the executor' check"
    );

    let node = NodeId::new();
    repo.claim_for_executor(job, node).await.unwrap();
    assert_eq!(repo.executor_of(t, job).await.unwrap(), Some(Some(node)));
}

#[tokio::test]
async fn in_flight_on_node_covers_claimed_and_running_only() {
    let repo = FakeLoopJobRepository::new();
    let t = tenant();
    let node = NodeId::new();
    let (a, b, done) = (
        queued(&repo, t, TaskId::new()).await,
        queued(&repo, t, TaskId::new()).await,
        queued(&repo, t, TaskId::new()).await,
    );
    for j in [a, b, done] {
        repo.claim_for_executor(j, node).await.unwrap();
    }
    repo.force_state(b, "running");
    repo.force_state(done, "succeeded");

    let mut ids = repo.in_flight_on_node(node).await.unwrap();
    ids.sort_by_key(|i| i.0);
    let mut want = vec![a, b];
    want.sort_by_key(|i| i.0);
    assert_eq!(ids, want, "a finished run is not stranded by a disconnect");
}

// ── transcript ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_transcript_belongs_to_its_job_and_keeps_its_order() {
    let repo = FakeLoopJobRepository::new();
    let t = tenant();
    let (a, b) = (
        queued(&repo, t, TaskId::new()).await,
        queued(&repo, t, TaskId::new()).await,
    );

    repo.append_transcript(a, "system", "one").await.unwrap();
    repo.append_transcript(b, "system", "other job")
        .await
        .unwrap();
    repo.append_transcript(a, "agent", "two").await.unwrap();

    let lines = repo.transcript(a).await.unwrap();
    assert_eq!(
        lines.iter().map(|l| l.content.as_str()).collect::<Vec<_>>(),
        vec!["one", "two"]
    );
    assert_eq!(repo.transcript(b).await.unwrap().len(), 1);
}

// ── interactions ────────────────────────────────────────────────────────────

#[tokio::test]
async fn two_people_racing_to_answer_produce_one_answer() {
    let repo = FakeInteractionRepository::new();
    let t = tenant();
    let id = InteractionId::new();
    repo.create(NewInteraction {
        id,
        tenant: t,
        job_id: None,
        task_id: None,
        prompt: "ship it?".into(),
        choices: Some(vec!["yes".into(), "no".into()]),
        requested_by_node_id: None,
        requested_by_session_id: None,
    })
    .await
    .unwrap();

    let (alice, bob) = (UserId::new(), UserId::new());
    let first = repo.answer(id, alice, "yes").await.unwrap();
    assert!(first.is_some());
    assert_eq!(first.unwrap().answered_by, Some(alice));

    let second = repo.answer(id, bob, "no").await.unwrap();
    assert!(
        second.is_none(),
        "`AND state = 'pending'` — the second answer must not overwrite the first"
    );
    let still = repo.get(t, id).await.unwrap().unwrap();
    assert_eq!(still.response.as_deref(), Some("yes"));
    assert_eq!(still.answered_by, Some(alice));
}

#[tokio::test]
async fn an_answered_question_cannot_then_be_canceled() {
    let repo = FakeInteractionRepository::new();
    let t = tenant();
    let id = InteractionId::new();
    repo.create(NewInteraction {
        id,
        tenant: t,
        job_id: None,
        task_id: None,
        prompt: "?".into(),
        choices: None,
        requested_by_node_id: None,
        requested_by_session_id: None,
    })
    .await
    .unwrap();
    repo.answer(id, UserId::new(), "yes").await.unwrap();

    assert!(repo.cancel(id).await.unwrap().is_none());
    assert_eq!(repo.state_of(id).as_deref(), Some("answered"));
}

#[tokio::test]
async fn finishing_a_job_cancels_only_its_own_pending_questions() {
    let repo = FakeInteractionRepository::new();
    let (t, other_tenant) = (tenant(), tenant());
    let (job, other_job) = (JobId::new(), JobId::new());

    let mk = |id: InteractionId, tn: TenantId, j: Option<JobId>| NewInteraction {
        id,
        tenant: tn,
        job_id: j,
        task_id: None,
        prompt: "?".into(),
        choices: None,
        requested_by_node_id: None,
        requested_by_session_id: None,
    };
    let (mine, answered, others, cross_tenant, unattached) = (
        InteractionId::new(),
        InteractionId::new(),
        InteractionId::new(),
        InteractionId::new(),
        InteractionId::new(),
    );
    repo.create(mk(mine, t, Some(job))).await.unwrap();
    repo.create(mk(answered, t, Some(job))).await.unwrap();
    repo.create(mk(others, t, Some(other_job))).await.unwrap();
    repo.create(mk(cross_tenant, other_tenant, Some(job)))
        .await
        .unwrap();
    repo.create(mk(unattached, t, None)).await.unwrap();
    repo.answer(answered, UserId::new(), "yes").await.unwrap();

    let canceled = repo.cancel_for_job(t, job).await.unwrap();
    assert_eq!(canceled.len(), 1);
    assert_eq!(canceled[0].id, mine);

    assert_eq!(repo.state_of(mine).as_deref(), Some("canceled"));
    assert_eq!(
        repo.state_of(answered).as_deref(),
        Some("answered"),
        "an already-answered question is not retroactively canceled"
    );
    assert_eq!(repo.state_of(others).as_deref(), Some("pending"));
    assert_eq!(
        repo.state_of(cross_tenant).as_deref(),
        Some("pending"),
        "the write is tenant-scoped — another tenant's rows are not this job's \
         to cancel even under the same job id"
    );
    assert_eq!(repo.state_of(unattached).as_deref(), Some("pending"));
}

#[tokio::test]
async fn pending_questions_are_listed_per_tenant_oldest_first() {
    let repo = FakeInteractionRepository::new();
    let (mine, theirs) = (tenant(), tenant());
    let mk = |id: InteractionId, tn: TenantId| NewInteraction {
        id,
        tenant: tn,
        job_id: None,
        task_id: None,
        prompt: "?".into(),
        choices: None,
        requested_by_node_id: None,
        requested_by_session_id: None,
    };
    let (first, second, answered) = (
        InteractionId::new(),
        InteractionId::new(),
        InteractionId::new(),
    );
    repo.create(mk(first, mine)).await.unwrap();
    repo.create(mk(second, mine)).await.unwrap();
    repo.create(mk(answered, mine)).await.unwrap();
    repo.create(mk(InteractionId::new(), theirs)).await.unwrap();
    repo.answer(answered, UserId::new(), "y").await.unwrap();

    let pending = repo.list_pending(mine).await.unwrap();
    assert_eq!(
        pending.len(),
        2,
        "answered rows drop out; other tenants never appear"
    );
    assert!(pending.iter().all(|i| i.tenant_id == mine));
}

// ── executor selection now lives on the node aggregate ──────────────────────

#[tokio::test]
async fn your_own_authorized_node_beats_the_shared_operator() {
    let nodes = FakeNodeRepository::new();
    let t = tenant();
    let me = uuid::Uuid::now_v7();

    let authorized = serde_json::json!({
        "loop_kinds": ["spec", "decompose"],
        "runtime_auth": [{ "runtime": "claude", "state": "authorized" }]
    });
    let operator = serde_json::json!({
        "shared_operator": true,
        "loop_kinds": ["spec", "decompose"],
        "runtime_auth": [{ "runtime": "claude", "state": "authorized" }]
    });

    let op = nodes.add(t, "operator", None, true);
    nodes.set_capabilities(op, operator);
    let mine = nodes.add(t, "mine", Some(me), false);
    nodes.set_capabilities(mine, authorized);
    for n in [op, mine] {
        nodes
            .record_capabilities(
                n,
                nook_control::repo::nodes::ReportedCapabilities {
                    capabilities: nodes.get(t, n).await.unwrap().unwrap().capabilities,
                    hostname: "h".into(),
                    platform: "linux".into(),
                },
            )
            .await
            .unwrap();
    }

    assert_eq!(
        nodes
            .eligible_loop_executors(t, me, "claude", "spec")
            .await
            .unwrap()
            .first()
            .copied(),
        Some(mine),
        "own-before-shared is the ORDER BY, not the caller's job"
    );
}

#[tokio::test]
async fn an_unauthorized_node_is_not_an_executor_however_online_it_is() {
    let nodes = FakeNodeRepository::new();
    let t = tenant();
    let me = uuid::Uuid::now_v7();
    let mine = nodes.add(t, "mine", Some(me), false);
    nodes
        .record_capabilities(
            mine,
            nook_control::repo::nodes::ReportedCapabilities {
                capabilities: serde_json::json!({
                    "loop_kinds": ["spec"],
                    "runtime_auth": [{ "runtime": "claude", "state": "not_authorized" }]
                }),
                hostname: "h".into(),
                platform: "linux".into(),
            },
        )
        .await
        .unwrap();

    assert!(nodes
        .eligible_loop_executors(t, me, "claude", "spec")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        nodes.owned_online_count(t, me).await.unwrap(),
        1,
        "…and that distinction is exactly what lets the queued reason say \
         'your node is online but not authorized' rather than 'no node'"
    );
    assert_eq!(nodes.shared_operator_online_count(t).await.unwrap(), 0);
}

#[tokio::test]
async fn an_offline_node_is_never_picked() {
    let nodes = FakeNodeRepository::new();
    let t = tenant();
    let me = uuid::Uuid::now_v7();
    let mine = nodes.add(t, "mine", Some(me), false);
    // Seeded `offline`; give it authorization but never bring it online.
    nodes.set_capabilities(
        mine,
        serde_json::json!({
            "loop_kinds": ["spec"],
            "runtime_auth": [{ "runtime": "claude", "state": "authorized" }]
        }),
    );

    assert!(nodes
        .eligible_loop_executors(t, me, "claude", "spec")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(nodes.owned_online_count(t, me).await.unwrap(), 0);
}

// ── the loop-job kind wall (MAIN-142) ───────────────────────────────────────

/// Fixture: an ONLINE, claude-authorized node declaring `kinds`. `add` seeds a
/// node offline, and `record_capabilities` is what brings it online — so both
/// steps are here rather than at each call site.
async fn online_declaring(
    nodes: &FakeNodeRepository,
    t: TenantId,
    name: &str,
    owner: Option<uuid::Uuid>,
    operator: bool,
    caps: serde_json::Value,
) -> NodeId {
    let id = nodes.add(t, name, owner, operator);
    nodes.set_capabilities(id, caps.clone());
    nodes
        .record_capabilities(
            id,
            nook_control::repo::nodes::ReportedCapabilities {
                capabilities: caps,
                hostname: "h".into(),
                platform: "linux".into(),
            },
        )
        .await
        .unwrap();
    id
}

fn declaring(kinds: &[&str], operator: bool) -> serde_json::Value {
    serde_json::json!({
        "shared_operator": operator,
        "loop_kinds": kinds,
        "runtime_auth": [{ "runtime": "claude", "state": "authorized" }],
    })
}

/// A node runs the stages it declared and no others. The refusal is not a
/// silent skip: it names the kind and what the node does accept, because a job
/// queueing forever with no reason is the failure this replaces.
#[tokio::test]
async fn a_node_is_offered_only_the_kinds_it_declares() {
    let nodes = FakeNodeRepository::new();
    let t = tenant();
    let me = uuid::Uuid::now_v7();
    let mine = online_declaring(
        &nodes,
        t,
        "mine",
        Some(me),
        false,
        declaring(&["spec"], false),
    )
    .await;

    assert_eq!(
        nodes
            .eligible_loop_executors(t, me, "claude", "spec")
            .await
            .unwrap(),
        vec![mine],
        "the kind it declared"
    );
    assert!(
        nodes
            .eligible_loop_executors(t, me, "claude", "decompose")
            .await
            .unwrap()
            .is_empty(),
        "a kind it did not declare is not offered, however capable the node is"
    );
}

/// AC-1's default, stated as a test: a node that configured nothing runs
/// nothing. The alternative — silence meaning "anything" — would have enrolled
/// every existing machine in agent work the moment this shipped.
#[tokio::test]
async fn a_node_declaring_nothing_accepts_nothing() {
    let nodes = FakeNodeRepository::new();
    let t = tenant();
    let me = uuid::Uuid::now_v7();
    let mine = online_declaring(
        &nodes,
        t,
        "mine",
        Some(me),
        false,
        serde_json::json!({ "runtime_auth": [{ "runtime": "claude", "state": "authorized" }] }),
    )
    .await;
    let _ = mine;

    for kind in ["spec", "decompose", "review", "epic-run", "build"] {
        assert!(
            nodes
                .eligible_loop_executors(t, me, "claude", kind)
                .await
                .unwrap()
                .is_empty(),
            "{kind} was offered to a node that declared no kinds"
        );
    }
}

/// The wall AC-3 exists for: a shared operator that declares `build` — the
/// lying-or-misconfigured node — is still never offered build work, and is
/// still refused at claim. Its own configuration is not consulted.
#[tokio::test]
async fn a_shared_operator_never_runs_build_however_it_is_configured() {
    let nodes = FakeNodeRepository::new();
    let t = tenant();
    let me = uuid::Uuid::now_v7();
    let op = online_declaring(
        &nodes,
        t,
        "operator",
        None,
        true,
        declaring(&["spec", "review", "build"], true),
    )
    .await;

    assert!(
        nodes
            .eligible_loop_executors(t, me, "claude", "build")
            .await
            .unwrap()
            .is_empty(),
        "a shared operator declaring `build` is still not offered build work"
    );
    assert_eq!(
        nodes
            .eligible_loop_executors(t, me, "claude", "review")
            .await
            .unwrap(),
        vec![op],
        "…while the stages it is FOR are unaffected"
    );
    assert!(
        nodes.is_shared_operator(op).await.unwrap(),
        "and the wall's question is answered from the stored row"
    );
}

/// A personal node may run build work — the wall is about shared substrate,
/// not about build being dangerous.
#[tokio::test]
async fn a_personal_node_may_run_build() {
    let nodes = FakeNodeRepository::new();
    let t = tenant();
    let me = uuid::Uuid::now_v7();
    let mine = online_declaring(
        &nodes,
        t,
        "mine",
        Some(me),
        false,
        declaring(&["build"], false),
    )
    .await;

    assert_eq!(
        nodes
            .eligible_loop_executors(t, me, "claude", "build")
            .await
            .unwrap(),
        vec![mine]
    );
}

/// The capacity the control plane reads back is the one the node reported, and
/// an absent one is distinguishable from a zero — zero means "stop claiming",
/// absent means "an agent too old to say".
#[tokio::test]
async fn a_reported_capacity_round_trips_and_absent_is_not_zero() {
    let nodes = FakeNodeRepository::new();
    let t = tenant();
    let me = uuid::Uuid::now_v7();

    let capped = nodes.add(t, "capped", Some(me), false);
    nodes.set_capabilities(
        capped,
        serde_json::json!({ "loop_kinds": ["spec"], "max_loop_jobs": 0 }),
    );
    let silent = nodes.add(t, "silent", Some(me), false);
    nodes.set_capabilities(silent, serde_json::json!({ "loop_kinds": ["spec"] }));

    assert_eq!(
        nodes.loop_profile(capped).await.unwrap(),
        Some((vec!["spec".to_string()], Some(0)))
    );
    assert_eq!(
        nodes.loop_profile(silent).await.unwrap(),
        Some((vec!["spec".to_string()], None)),
        "absent is None, not Some(0) — one disables claiming, the other does not"
    );
}

// ── MAIN-144: the epic-run kind's dedupe ─────────────────────────────────────

/// One pass per epic at a time, and the refusal names the job already running —
/// the caller's next move is to watch that one, not to retry.
#[tokio::test]
async fn one_epic_run_per_epic_and_the_live_one_is_named() {
    let repo = FakeLoopJobRepository::default();
    let t = tenant();
    let epic = TaskId::new();
    let other_epic = TaskId::new();

    assert_eq!(
        repo.active_epic_run_for(t, epic).await.expect("query"),
        None,
        "no pass in flight yet"
    );

    let id = JobId::new();
    repo.create(NewLoopJob {
        id,
        tenant: t,
        kind: "epic-run".into(),
        target_task_id: Some(epic),
        workspace_id: None,
        requested_by: UserId::new(),
        seed: None,
        predecessor_job_id: None,
        review_pr_number: None,
        review_head_sha: None,
        build_fingerprint: None,
    })
    .await
    .expect("create");

    assert_eq!(
        repo.active_epic_run_for(t, epic).await.expect("query"),
        Some(id),
        "the live pass is returned BY ID, so a refusal can name it"
    );
    assert_eq!(
        repo.active_epic_run_for(t, other_epic)
            .await
            .expect("query"),
        None,
        "another epic's queue is its own"
    );

    // A finished pass frees the epic: terminal states are not "in flight".
    repo.transition(id, "completed").await.expect("finish");
    assert_eq!(
        repo.active_epic_run_for(t, epic).await.expect("query"),
        None
    );
}
