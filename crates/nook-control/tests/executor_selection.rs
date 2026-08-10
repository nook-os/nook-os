//! Loop-job executor selection (MAIN-160): own node preferred, operator
//! fallback, no-eligible reason, atomic claim under contention, ineligible
//! runtime skipped. Each test runs on its OWN private database (MAIN-156).
//!
//! Needs Postgres: `DATABASE_URL` (`NOOK_REQUIRE_DB=1` in the suite).

use nook_control::services::jobs;
use nook_control::state::AppState;
use nook_db::{params, Db};
use nook_types::*;
use serde_json::json;
use uuid::Uuid;

use nook_testkit::TestBed;

/// A board + column + a team-visible task to anchor a job on.
async fn target_task(bed: &TestBed, tenant: TenantId, creator: UserId) -> TaskId {
    let board = BoardId::new();
    bed.db()
        .exec(
            "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b',$3,'local')",
            // The RANDOM tail of the v7 uuid — its leading bytes are a shared
            // timestamp, so two boards made in the same test would collide on a
            // prefix-derived key.
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

/// A queued spec job on `target`, requested by `user`.
async fn queued_job(bed: &TestBed, tenant: TenantId, user: UserId, target: TaskId) -> JobId {
    let id = JobId::new();
    bed.db()
        .exec(
            "INSERT INTO loop_jobs (id, tenant_id, kind, target_task_id, requested_by, state)
         VALUES ($1,$2,'spec',$3,$4,'queued')",
            params![id, tenant, target, user],
        )
        .await
        .expect("job");
    id
}

/// Insert a node with an explicit status, owner, and capabilities jsonb.
async fn node(
    bed: &TestBed,
    tenant: TenantId,
    owner: Option<Uuid>,
    status: &str,
    caps: serde_json::Value,
) -> NodeId {
    let id = NodeId::new();
    bed.db()
        .exec(
            "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status, owner_person_id, capabilities)
         VALUES ($1,$2,$3,$4,$5,$6,$7)",
            params![
                id,
                tenant,
                format!("n-{}", id.0.simple()),
                format!("h-{}", id.0.simple()),
                status,
                owner,
                caps
            ],
        )
        .await
        .expect("node");
    id
}

/// Capabilities reporting the `claude` runtime in the given auth state.
///
/// `loop_kinds` is part of the fixture because a node that declares none
/// accepts none (MAIN-142) — these tests are about the AUTH gate, so they
/// declare the kinds and let the auth state be the variable.
fn caps(state: &str, operator: bool) -> serde_json::Value {
    let mut c = json!({
        "loop_kinds": ["spec", "decompose", "review", "epic-run"],
        "runtime_auth": [
            { "id": "claude", "label": "Claude Code", "runtime": "claude", "state": state }
        ]
    });
    if operator {
        c["shared_operator"] = json!(true);
    }
    c
}

async fn setup(bed: &TestBed) -> (AppState, TenantId, UserId, Uuid, JobId) {
    let tenant = bed.tenant("exec").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let target = target_task(bed, tenant, user).await;
    let job = queued_job(bed, tenant, user, target).await;
    (bed.app_state().await, tenant, user, person, job)
}

#[tokio::test]
async fn own_node_is_preferred_over_the_operator() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, _user, person, job) = setup(&bed).await;
    let mine = node(
        &bed,
        tenant,
        Some(person),
        "online",
        caps("authorized", false),
    )
    .await;
    let _operator = node(&bed, tenant, None, "online", caps("authorized", true)).await;

    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select");
    assert_eq!(placed.state, "claimed");
    assert_eq!(
        placed.executor_node_id,
        Some(mine),
        "prefers the owned node"
    );
    assert!(placed.queued_reason.is_none());

    bed.teardown().await;
}

#[tokio::test]
async fn operator_is_the_fallback_when_no_owned_node_is_eligible() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, _user, person, job) = setup(&bed).await;
    // The requester's own node is online but NOT authorized — skipped.
    let _mine = node(
        &bed,
        tenant,
        Some(person),
        "online",
        caps("not_authorized", false),
    )
    .await;
    let operator = node(&bed, tenant, None, "online", caps("authorized", true)).await;

    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select");
    assert_eq!(placed.state, "claimed");
    assert_eq!(
        placed.executor_node_id,
        Some(operator),
        "falls back to the operator"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn no_eligible_executor_leaves_the_job_queued_with_a_reason() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, _user, person, job) = setup(&bed).await;
    // Owned node online but unauthorized; no operator at all.
    let _mine = node(
        &bed,
        tenant,
        Some(person),
        "online",
        caps("not_authorized", false),
    )
    .await;

    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select");
    assert_eq!(placed.state, "queued", "unplaceable stays queued");
    assert!(placed.executor_node_id.is_none());
    let reason = placed.queued_reason.expect("a reason is recorded");
    assert!(
        reason.contains("not authorized") && reason.contains("claude"),
        "reason names the failed gate: {reason}"
    );

    // An offline owned node yields the 'no node online' reason instead.
    let job2 = queued_job(&bed, tenant, _user, target_task(&bed, tenant, _user).await).await;
    let _offline = node(
        &bed,
        tenant,
        Some(person),
        "offline",
        caps("authorized", false),
    )
    .await;
    // (the online-but-unauthorized node from above still counts as online)
    let placed2 = jobs::select_executor(&state, tenant, job2)
        .await
        .expect("select2");
    assert_eq!(placed2.state, "queued");
    assert!(placed2.queued_reason.is_some());

    bed.teardown().await;
}

#[tokio::test]
async fn ineligible_runtime_is_skipped() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, _user, person, job) = setup(&bed).await;
    // A node reporting a DIFFERENT runtime authorized, not claude.
    let other = json!({
        "loop_kinds": ["spec", "decompose"],
        "runtime_auth": [{ "id": "codex", "label": "Codex", "runtime": "codex", "state": "authorized" }]
    });
    let _mine = node(&bed, tenant, Some(person), "online", other).await;

    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select");
    assert_eq!(
        placed.state, "queued",
        "a node without claude authorized is not eligible"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn concurrent_selection_claims_a_job_exactly_once() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, _user, person, job) = setup(&bed).await;
    let mine = node(
        &bed,
        tenant,
        Some(person),
        "online",
        caps("authorized", false),
    )
    .await;

    // Two consumers race the same queued job.
    let (a, b) = tokio::join!(
        jobs::select_executor(&state, tenant, job),
        jobs::select_executor(&state, tenant, job),
    );
    let a = a.expect("a");
    let b = b.expect("b");

    // Both observe the SAME claim — one wrote it, the other read it back — and
    // the job is claimed by the one node exactly once.
    assert_eq!(a.state, "claimed");
    assert_eq!(b.state, "claimed");
    assert_eq!(a.executor_node_id, Some(mine));
    assert_eq!(b.executor_node_id, Some(mine));

    let (state_str, exec): (String, Option<NodeId>) = bed
        .db()
        .query_one(
            "SELECT state, executor_node_id FROM loop_jobs WHERE id = $1",
            params![job],
        )
        .await
        .unwrap();
    assert_eq!(state_str, "claimed");
    assert_eq!(exec, Some(mine));

    bed.teardown().await;
}

// ── the loop-job kind wall, against the real SQL (MAIN-142) ─────────────────
//
// These run against Postgres deliberately. The in-memory fake compares Rust
// strings and cannot see how the declared kinds are actually stored: the first
// version of this query expanded the jsonb array and compared `k.value::text`,
// which is `"spec"` WITH its quotes and matched nothing. Every fake test passed.

/// Capabilities for a node that is authorized but declares `kinds`.
fn caps_declaring(kinds: &[&str], operator: bool) -> serde_json::Value {
    let mut c = json!({
        "loop_kinds": kinds,
        "runtime_auth": [
            { "id": "claude", "label": "Claude Code", "runtime": "claude", "state": "authorized" }
        ]
    });
    if operator {
        c["shared_operator"] = json!(true);
    }
    c
}

#[tokio::test]
async fn a_job_of_a_kind_no_node_declares_stays_queued_with_the_reason() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, _user, person, job) = setup(&bed).await;
    // Online, authorized, owned — and declares only `decompose`. The job is
    // `spec`, so this node is not a candidate.
    node(
        &bed,
        tenant,
        Some(person),
        "online",
        caps_declaring(&["decompose"], false),
    )
    .await;

    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select");
    assert_eq!(placed.state, "queued");
    let reason = placed.queued_reason.unwrap_or_default();
    assert!(
        reason.contains("spec"),
        "the reason names the kind that found no home: {reason}"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn a_node_declaring_the_kind_is_placed() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, _user, person, job) = setup(&bed).await;
    let mine = node(
        &bed,
        tenant,
        Some(person),
        "online",
        caps_declaring(&["spec"], false),
    )
    .await;

    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select");
    assert_eq!(placed.state, "claimed");
    assert_eq!(placed.executor_node_id, Some(mine));

    bed.teardown().await;
}

/// The wall AC-3 is for. A shared operator that declares `build` — the
/// misconfigured or lying node — is still refused, at the offer and again at
/// the claim. Its declaration is never consulted for this rule.
#[tokio::test]
async fn a_shared_operator_declaring_build_is_still_refused_build_work() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, _user, person, _job) = setup(&bed).await;
    let op = node(
        &bed,
        tenant,
        None,
        "online",
        caps_declaring(&["spec", "review", "build"], true),
    )
    .await;

    // Offer: not a candidate for build, though it is for review.
    let for_build = state
        .nodes
        .eligible_loop_executors(tenant, person, "claude", "build")
        .await
        .expect("candidates");
    assert!(
        for_build.is_empty(),
        "a shared operator declaring build is offered none"
    );
    let for_review = state
        .nodes
        .eligible_loop_executors(tenant, person, "claude", "review")
        .await
        .expect("candidates");
    assert_eq!(for_review, vec![op], "…while review is unaffected");
    // MAIN-144 AC-5: epic-run rides the same declared-kinds gate. THIS node
    // declares spec/review/build and not epic-run — so both the offer and the
    // wall skip it, and the wall's refusal names the missing declaration (the
    // declared-kinds rule, not the build rule).
    let for_epic_run = state
        .nodes
        .eligible_loop_executors(tenant, person, "claude", "epic-run")
        .await
        .expect("candidates");
    assert!(
        for_epic_run.is_empty(),
        "a node that does not declare epic-run is not offered it"
    );
    let undeclared = jobs::kind_wall_refusal(&state, op, "epic-run")
        .await
        .expect("wall")
        .expect("a refusal");
    assert!(
        undeclared.contains("does not accept epic-run"),
        "the refusal is the declared-kinds filter, by name: {undeclared}"
    );
    // …and on a shared operator that DOES declare epic-run, the wall passes
    // it: the build rule is about build alone and has no opinion here.
    let op_with_epic_run = node(
        &bed,
        tenant,
        None,
        "online",
        caps_declaring(&["spec", "review", "epic-run"], true),
    )
    .await;
    assert!(
        jobs::kind_wall_refusal(&state, op_with_epic_run, "epic-run")
            .await
            .expect("wall")
            .is_none(),
        "epic-run is not build: the wall does not refuse it on an operator that declares it"
    );

    // Claim: refused again, independently, with a message naming the rule.
    let refusal = jobs::kind_wall_refusal(&state, op, "build")
        .await
        .expect("wall")
        .expect("a refusal");
    assert!(
        refusal.contains("shared operator"),
        "the refusal says why: {refusal}"
    );
    assert!(
        jobs::kind_wall_refusal(&state, op, "review")
            .await
            .expect("wall")
            .is_none(),
        "and it does not refuse what the operator is for"
    );

    bed.teardown().await;
}

/// Capacity is a skip, not a failure: the job waits for room rather than being
/// declared unplaceable.
#[tokio::test]
async fn a_node_at_capacity_is_skipped_and_the_job_waits() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, user, person, job) = setup(&bed).await;
    let mut caps = caps_declaring(&["spec"], false);
    caps["max_loop_jobs"] = json!(1);
    let mine = node(&bed, tenant, Some(person), "online", caps).await;

    // One job already in flight on that node fills its single slot.
    let target = target_task(&bed, tenant, user).await;
    let held = queued_job(&bed, tenant, user, target).await;
    bed.db()
        .exec(
            "UPDATE loop_jobs SET state = 'running', executor_node_id = $2 WHERE id = $1",
            params![held, mine],
        )
        .await
        .expect("occupy");

    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select");
    assert_eq!(placed.state, "queued");
    let reason = placed.queued_reason.clone().unwrap_or_default();
    assert!(
        reason.contains("capacity"),
        "the reason says it is busy, not ineligible: {reason}"
    );

    // The slot frees; the same job places without anything else changing.
    bed.db()
        .exec(
            "UPDATE loop_jobs SET state = 'completed' WHERE id = $1",
            params![held],
        )
        .await
        .expect("free");
    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select again");
    assert_eq!(placed.state, "claimed");
    assert_eq!(placed.executor_node_id, Some(mine));

    bed.teardown().await;
}

/// `max_loop_jobs: 0` is how an operator quiesces a node without deleting it.
#[tokio::test]
async fn a_zero_capacity_node_never_claims() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, _user, person, job) = setup(&bed).await;
    let mut caps = caps_declaring(&["spec"], false);
    caps["max_loop_jobs"] = json!(0);
    node(&bed, tenant, Some(person), "online", caps).await;

    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select");
    assert_eq!(placed.state, "queued");
    assert!(placed
        .queued_reason
        .unwrap_or_default()
        .contains("capacity"));

    bed.teardown().await;
}

/// MAIN-508 AC-2, the criterion the whole card turns on: an operator's capacity
/// is honoured by PLACEMENT, with the node agent untouched.
///
/// The node's `capabilities` — the only thing a restart rewrites — is never
/// written after registration here. A job that could not be placed a moment ago
/// is placed on the same row, so the number reached the dispatcher without the
/// restart that would have stranded whatever that machine was building.
#[tokio::test]
async fn a_central_capacity_is_honoured_without_the_node_reporting_again() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, user, person, job) = setup(&bed).await;
    let mut caps = caps_declaring(&["spec"], false);
    caps["max_loop_jobs"] = json!(1);
    let mine = node(&bed, tenant, Some(person), "online", caps.clone()).await;

    // Its one slot is taken, so the job has nowhere to go.
    let target = target_task(&bed, tenant, user).await;
    let held = queued_job(&bed, tenant, user, target).await;
    bed.db()
        .exec(
            "UPDATE loop_jobs SET state = 'running', executor_node_id = $2 WHERE id = $1",
            params![held, mine],
        )
        .await
        .expect("occupy");
    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select");
    assert_eq!(placed.state, "queued");

    // The operator raises it centrally. Nothing else moves: the held job is
    // still running, and the node still advertises 1.
    state
        .nodes
        .set_max_loop_jobs(mine, tenant, Some(3))
        .await
        .expect("set capacity");

    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select again");
    assert_eq!(placed.state, "claimed", "the new number reached placement");
    assert_eq!(placed.executor_node_id, Some(mine));

    let (still, running): (serde_json::Value, i64) = bed
        .db()
        .query_one(
            "SELECT n.capabilities, (SELECT count(*) FROM loop_jobs WHERE id = $2 AND state = 'running')
               FROM nodes n WHERE n.id = $1",
            params![mine, held],
        )
        .await
        .expect("read back");
    assert_eq!(
        still, caps,
        "the node never re-reported — no restart happened"
    );
    assert_eq!(running, 1, "and the work already on it was undisturbed");

    bed.teardown().await;
}

/// AC-5's other half: the cordon is settable centrally too, and it stops the
/// node being CHOSEN without touching what it is already running — which is
/// what makes it usable on a machine mid-build.
#[tokio::test]
async fn a_central_zero_cordons_a_node_that_advertises_capacity() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, _user, person, job) = setup(&bed).await;
    let mut caps = caps_declaring(&["spec"], false);
    caps["max_loop_jobs"] = json!(4);
    let mine = node(&bed, tenant, Some(person), "online", caps).await;

    state
        .nodes
        .set_max_loop_jobs(mine, tenant, Some(0))
        .await
        .expect("cordon");

    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select");
    assert_eq!(placed.state, "queued");
    assert!(
        placed
            .queued_reason
            .unwrap_or_default()
            .contains("capacity"),
        "a cordon reads as capacity, the same as the node's own zero does"
    );

    bed.teardown().await;
}

/// AC-3: a host that pins its own number keeps it, even against a central value
/// already stored — the escape hatch for a box sized by something outside
/// NookOS.
#[tokio::test]
async fn a_pinned_host_outranks_the_central_value_at_placement() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, _user, person, job) = setup(&bed).await;
    let mut caps = caps_declaring(&["spec"], false);
    caps["max_loop_jobs"] = json!(0);
    caps["max_loop_jobs_pinned"] = json!(true);
    let mine = node(&bed, tenant, Some(person), "online", caps).await;

    // Stored directly: the endpoint refuses this write on a pinned node, and
    // the point here is that even a value already in the column loses.
    state
        .nodes
        .set_max_loop_jobs(mine, tenant, Some(4))
        .await
        .expect("store");

    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select");
    assert_eq!(
        placed.state, "queued",
        "the host's own zero still governs the machine"
    );

    bed.teardown().await;
}

/// MAIN-383 AC-3: build placement is `role=build`, an owner's explicit label.
/// An otherwise perfectly eligible node — owned, online, authorized, declaring
/// `build` — gets no build work until it wears the label, and the queued
/// reason says exactly that instead of blaming auth or declarations. The label
/// is set old-style (`role=build`) on purpose: `placement_of` widens it to the
/// `role/build=true` key the selector reads (MAIN-463), so the test also
/// proves the widening is in the dispatch path.
#[tokio::test]
async fn a_build_job_waits_for_a_role_build_label_and_places_once_it_exists() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, user, person, _spec_job) = setup(&bed).await;
    let mine = node(
        &bed,
        tenant,
        Some(person),
        "online",
        caps_declaring(&["build"], false),
    )
    .await;

    let target = target_task(&bed, tenant, user).await;
    let job = JobId::new();
    bed.db()
        .exec(
            "INSERT INTO loop_jobs (id, tenant_id, kind, target_task_id, requested_by, state)
         VALUES ($1,$2,'build',$3,$4,'queued')",
            params![job, tenant, target, user],
        )
        .await
        .expect("build job");

    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select");
    assert_eq!(placed.state, "queued", "no labeled node, no placement");
    let reason = placed.queued_reason.clone().unwrap_or_default();
    assert!(
        reason.contains("role=build"),
        "the reason names the missing label, not auth or kinds: {reason}"
    );

    bed.db()
        .exec(
            r#"UPDATE nodes SET labels = '{"role": "build"}' WHERE id = $1"#,
            params![mine],
        )
        .await
        .expect("label");
    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select again");
    assert_eq!(placed.state, "claimed", "the label is the whole difference");
    assert_eq!(placed.executor_node_id, Some(mine));

    bed.teardown().await;
}

// ── MAIN-480: the worktree pin ───────────────────────────────────────────────

/// A queued BUILD job — the only kind that carries state across passes and so
/// the only kind the pin applies to.
async fn queued_build_job(bed: &TestBed, tenant: TenantId, user: UserId, target: TaskId) -> JobId {
    let id = JobId::new();
    bed.db()
        .exec(
            "INSERT INTO loop_jobs (id, tenant_id, kind, target_task_id, requested_by, state)
         VALUES ($1,$2,'build',$3,$4,'queued')",
            params![id, tenant, target, user],
        )
        .await
        .expect("build job");
    id
}

/// Capabilities for a node that may take build work: the kind declared and the
/// `role=build` label the build wall requires (MAIN-383).
fn build_caps(operator: bool) -> serde_json::Value {
    let mut c = json!({
        "loop_kinds": ["spec", "decompose", "review", "epic-run", "build"],
        "runtime_auth": [
            { "id": "claude", "label": "Claude Code", "runtime": "claude", "state": "authorized" }
        ]
    });
    if operator {
        c["shared_operator"] = json!(true);
    }
    c
}

/// A node that may take build work: the kind declared (capabilities) AND the
/// `role=build` label the build wall requires, which lives in its own column.
async fn build_node(bed: &TestBed, tenant: TenantId, owner: Option<Uuid>, status: &str) -> NodeId {
    let id = node(bed, tenant, owner, status, build_caps(false)).await;
    bed.db()
        .exec(
            r#"UPDATE nodes SET labels = '{"role": "build"}' WHERE id = $1"#,
            params![id],
        )
        .await
        .expect("label");
    id
}

async fn record_worktree(bed: &TestBed, task: TaskId, node: NodeId, path: &str) {
    bed.db()
        .exec(
            "UPDATE tasks SET worktree_path = $2, worktree_node_id = $3 WHERE id = $1",
            params![task, path, node],
        )
        .await
        .expect("record worktree");
}

/// MAIN-480 AC-5: the card's recorded node is the ONLY candidate. The pin is
/// not a preference — a pass elsewhere abandons the warm session and, after a
/// crash, the only copy of the interrupted work.
#[tokio::test]
async fn a_recorded_worktree_pins_the_build_to_its_node() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("pin").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let target = target_task(&bed, tenant, user).await;
    let job = queued_build_job(&bed, tenant, user, target).await;
    let state = bed.app_state().await;

    let holder = build_node(&bed, tenant, Some(person), "online").await;
    let _other = build_node(&bed, tenant, Some(person), "online").await;
    record_worktree(&bed, target, holder, "/cache/worktrees/build-ws-MAIN-42").await;

    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select");
    assert_eq!(placed.state, "claimed");
    assert_eq!(
        placed.executor_node_id,
        Some(holder),
        "the node holding this card's worktree is the only candidate"
    );
    bed.teardown().await;
}

/// MAIN-480 AC-5: pinned and dark means WAIT. Placing it elsewhere would be
/// worse than waiting, so the job stays queued and the reason names the node
/// and the way out.
#[tokio::test]
async fn a_pinned_build_waits_for_its_node_and_says_which() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("pindark").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let target = target_task(&bed, tenant, user).await;
    let job = queued_build_job(&bed, tenant, user, target).await;
    let state = bed.app_state().await;

    let dark = build_node(&bed, tenant, Some(person), "offline").await;
    // A perfectly good alternative that must NOT be used.
    let _alive = build_node(&bed, tenant, Some(person), "online").await;
    record_worktree(&bed, target, dark, "/cache/worktrees/build-ws-MAIN-43").await;

    let held = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select");
    assert_eq!(held.state, "queued", "it waits rather than starting over");
    assert_eq!(held.executor_node_id, None);
    let reason = held.queued_reason.unwrap_or_default();
    assert!(
        reason.contains("holds this card's worktree"),
        "the reason must name why it is waiting: {reason}"
    );
    assert!(
        reason.contains("Prune"),
        "and the way out, so a dead node is not a dead end: {reason}"
    );
    bed.teardown().await;
}

/// MAIN-480 AC-5/AC-6: the pin is released by pruning the record — after which
/// the loop places the work anywhere eligible, exactly as before.
#[tokio::test]
async fn clearing_the_record_releases_the_pin() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("unpin").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let target = target_task(&bed, tenant, user).await;
    let job = queued_build_job(&bed, tenant, user, target).await;
    let state = bed.app_state().await;

    let dark = build_node(&bed, tenant, Some(person), "offline").await;
    let alive = build_node(&bed, tenant, Some(person), "online").await;
    record_worktree(&bed, target, dark, "/cache/worktrees/build-ws-MAIN-44").await;
    assert_eq!(
        jobs::select_executor(&state, tenant, job)
            .await
            .expect("select")
            .state,
        "queued"
    );

    state.tasks.clear_worktree(target).await.expect("clear");

    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select again");
    assert_eq!(placed.state, "claimed");
    assert_eq!(placed.executor_node_id, Some(alive));
    bed.teardown().await;
}

/// MAIN-480 NG-2: only build work is pinned. A review or spec run carries
/// nothing across passes, and a `worktree_node_id` set by the human start-work
/// path is none of their business.
#[tokio::test]
async fn a_spec_job_is_not_pinned_by_a_cards_worktree() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("nopin").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let target = target_task(&bed, tenant, user).await;
    let job = queued_job(&bed, tenant, user, target).await;
    let state = bed.app_state().await;

    let dark = node(
        &bed,
        tenant,
        Some(person),
        "offline",
        caps("authorized", false),
    )
    .await;
    let alive = node(
        &bed,
        tenant,
        Some(person),
        "online",
        caps("authorized", false),
    )
    .await;
    record_worktree(&bed, target, dark, "/checkouts/human-start-work").await;

    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select");
    assert_eq!(placed.state, "claimed");
    assert_eq!(placed.executor_node_id, Some(alive));
    bed.teardown().await;
}
