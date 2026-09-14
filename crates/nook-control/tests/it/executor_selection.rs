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
        "sandbox": { "state": "ready", "image": "nook-job-sandbox:test" },
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
        "sandbox": { "state": "ready", "image": "nook-job-sandbox:test" },
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
        "sandbox": { "state": "ready", "image": "nook-job-sandbox:test" },
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

    // MAIN-655: the same operator, reporting that a build here lands on a pool
    // of its own, is allowed the kind. The wall was never about `build` being
    // dangerous in itself — it was about a privileged agent sharing a machine
    // with other tenants' work, and a tainted pool is the arrangement where it
    // does not.
    let mut isolated = caps_declaring(&["spec", "review", "build"], true);
    isolated["isolated_builds"] = json!(true);
    let pooled = node(&bed, tenant, None, "online", isolated).await;
    assert!(
        jobs::kind_wall_refusal(&state, pooled, "build")
            .await
            .expect("wall")
            .is_none(),
        "a shared operator with an isolated build pool may take build work"
    );
    let offered = state
        .nodes
        .eligible_loop_executors(tenant, person, "claude", "build")
        .await
        .expect("candidates");
    assert!(
        offered.contains(&pooled),
        "…and it is actually offered the work: {offered:?}"
    );
    assert!(
        !offered.contains(&op),
        "while the operator WITHOUT a pool still is not"
    );

    // And the node cannot ask its way in: declaring the kind is not the thing
    // that opens the gate, reporting the arrangement is.
    let asked = node(
        &bed,
        tenant,
        None,
        "online",
        caps_declaring(&["build"], true),
    )
    .await;
    assert!(
        jobs::kind_wall_refusal(&state, asked, "build")
            .await
            .expect("wall")
            .is_some(),
        "declaring loop_kinds=build without isolated_builds changes nothing"
    );

    bed.teardown().await;
}

/// Capacity is a skip, not a failure: the job waits for room rather than being
/// declared unplaceable.
/// MAIN-611 AC-8. A host node that cannot confine a loop agent takes no loop
/// work: the job WAITS, under a typed reason naming the node and what it said,
/// and it is never `failed` — a node-side shortage must not spend the card's
/// strike budget, exactly as `PortsUnavailable` does not.
#[tokio::test]
async fn a_node_that_cannot_sandbox_takes_no_work_and_the_job_waits() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, _user, person, job) = setup(&bed).await;
    let mut caps = caps_declaring(&["spec"], false);
    caps["sandbox"] = json!({
        "state": "unavailable",
        "detail": "no Docker daemon on this node"
    });
    let mine = node(&bed, tenant, Some(person), "online", caps).await;

    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select");
    assert_eq!(
        placed.state, "queued",
        "the run must wait, never fail: the card did nothing wrong"
    );
    assert_eq!(
        placed.queued_reason_kind,
        Some(QueuedReason::SandboxUnavailable {
            node_name: node_name(&bed, mine).await,
            detail: "no Docker daemon on this node".into(),
        }),
        "the gate is a value a client branches on, not a sentence it matches"
    );
    let reason = placed.queued_reason.clone().unwrap_or_default();
    assert!(
        reason.contains("no Docker daemon"),
        "the sentence names what to fix, on the machine to fix it on: {reason}"
    );

    // The operator installs the image; nothing else changes and it places.
    let mut fixed = caps_declaring(&["spec"], false);
    fixed["sandbox"] = json!({ "state": "ready", "image": "nook-job-sandbox:test" });
    bed.db()
        .exec(
            "UPDATE nodes SET capabilities = $2 WHERE id = $1",
            params![mine, fixed],
        )
        .await
        .expect("fix the node");
    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select again");
    assert_eq!(placed.state, "claimed");
    assert_eq!(placed.executor_node_id, Some(mine));

    bed.teardown().await;
}

/// MAIN-643 AC-3/AC-4. A node still PULLING its sandbox image is refused, and
/// says so in its own terms — a state added after the gate was written must not
/// be waved through by a wildcard arm on the way past.
#[tokio::test]
async fn a_node_still_pulling_its_sandbox_takes_no_work_either() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, _user, person, job) = setup(&bed).await;
    let mut caps = caps_declaring(&["spec"], false);
    caps["sandbox"] = json!({
        "state": "pulling",
        "image": "ghcr.io/nook-os/nook-job-sandbox:9.9.9"
    });
    node(&bed, tenant, Some(person), "online", caps).await;

    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select");
    assert_eq!(
        placed.state, "queued",
        "a warming node holds the job; it never fails it"
    );
    let reason = placed.queued_reason.clone().unwrap_or_default();
    assert!(
        reason.contains("pulling") && reason.contains("9.9.9"),
        "the wait says it is temporary and names the image: {reason}"
    );

    bed.teardown().await;
}

/// A node that reports NOTHING is refused too (MAIN-611 AC-8). Silence is an
/// agent from before the sandbox shipped, not evidence that it confines
/// anything — and reading silence as consent is how a fail-closed gate becomes
/// fail-open on exactly the machines that predate it.
#[tokio::test]
async fn a_node_that_reports_no_sandbox_at_all_is_refused() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, _user, person, job) = setup(&bed).await;
    let mut caps = caps_declaring(&["spec"], false);
    caps.as_object_mut().expect("object").remove("sandbox");
    node(&bed, tenant, Some(person), "online", caps).await;

    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select");
    assert_eq!(placed.state, "queued");
    assert!(
        matches!(
            placed.queued_reason_kind,
            Some(QueuedReason::SandboxUnavailable { .. })
        ),
        "an unreported sandbox must refuse, not pass: {:?}",
        placed.queued_reason_kind
    );
    let reason = placed.queued_reason.clone().unwrap_or_default();
    assert!(
        reason.contains("predates"),
        "and says the fix is an agent upgrade: {reason}"
    );

    bed.teardown().await;
}

/// NG-5: a CONTAINERISED node keeps working. It mounts no Docker socket and
/// cannot run a build at all, so there is nothing to confine — refusing it
/// would take the shared operator's spec, review and epic-run work offline the
/// day this shipped, for no security gained.
#[tokio::test]
async fn a_containerised_node_is_exempt_and_still_takes_work() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, _user, person, job) = setup(&bed).await;
    let mut caps = caps_declaring(&["spec"], false);
    caps["sandbox"] = json!({ "state": "exempt", "detail": "/.dockerenv is present" });
    let mine = node(&bed, tenant, Some(person), "online", caps).await;

    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select");
    assert_eq!(placed.state, "claimed");
    assert_eq!(placed.executor_node_id, Some(mine));

    bed.teardown().await;
}

// ── MAIN-618: the free-disk floor ────────────────────────────────────────────

/// What a node's heartbeat put in `nodes.resources`. The shortage sentence is
/// composed on the MACHINE — the floor is stated only there — so a fixture
/// says it the way a node would rather than deriving it.
async fn report_disk(bed: &TestBed, id: NodeId, disks: serde_json::Value, shortage: Option<&str>) {
    bed.db()
        .exec(
            "UPDATE nodes SET resources = $2 WHERE id = $1",
            params![
                id,
                json!({
                    "cpu_percent": 4.0,
                    "mem_used": 1,
                    "mem_total": 2,
                    "load_avg1": 0.1,
                    "active_sessions": 0,
                    "disks": disks,
                    "disk_shortage": shortage,
                })
            ],
        )
        .await
        .expect("resources");
}

fn roomy() -> serde_json::Value {
    json!([{ "label": "job cache, Docker data root", "mount_point": "/",
             "free_bytes": 300_000_000_000u64, "total_bytes": 500_000_000_000u64 }])
}

fn nearly_full() -> serde_json::Value {
    json!([{ "label": "job cache, Docker data root", "mount_point": "/",
             "free_bytes": 2_000_000_000u64, "total_bytes": 500_000_000_000u64 }])
}

/// MAIN-618 AC-3/AC-5. A node below its own floor takes no loop work: the job
/// WAITS under a typed reason carrying the node's words, and it is never
/// `failed` — a full disk is the machine's problem, and spending the card's
/// strike budget on it would blame the card. Recovery is the same sample
/// arriving without the shortage; nothing is restarted and nothing is cleared.
#[tokio::test]
async fn a_node_below_its_disk_floor_takes_no_work_and_the_job_waits() {
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
    let detail = "below the 20.0 GiB free-disk floor: job cache (/) has 1.9 GiB free of 465.7 GiB";
    report_disk(&bed, mine, nearly_full(), Some(detail)).await;

    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select");
    assert_eq!(
        placed.state, "queued",
        "the run must wait, never fail: the card did nothing wrong"
    );
    assert_eq!(
        placed.queued_reason_kind,
        Some(QueuedReason::DiskUnavailable {
            node_name: node_name(&bed, mine).await,
            detail: detail.into(),
        }),
        "the gate is a value a client branches on, not a sentence it matches"
    );
    let reason = placed.queued_reason.clone().unwrap_or_default();
    assert!(
        reason.contains("job cache (/)"),
        "the sentence names the filesystem, so the fix has a target: {reason}"
    );

    // Space comes back on the next heartbeat. Nothing else changes.
    report_disk(&bed, mine, roomy(), None).await;
    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select again");
    assert_eq!(
        placed.state, "claimed",
        "recovery is automatic: one healthy sample is the whole difference"
    );
    assert_eq!(placed.executor_node_id, Some(mine));

    bed.teardown().await;
}

/// MAIN-618: an agent that predates the field is NOT cordoned by it. Silence
/// here means "unknown", the opposite of the sandbox gate's reading of it —
/// gating on it would take a whole fleet offline the day the control plane was
/// upgraded, for a shortage no machine ever reported.
#[tokio::test]
async fn a_node_reporting_no_disk_sample_at_all_is_not_gated() {
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
    // Exactly what an older agent's heartbeat writes: the four fields it knows.
    bed.db()
        .exec(
            "UPDATE nodes SET resources = $2 WHERE id = $1",
            params![
                mine,
                json!({ "cpu_percent": 4.0, "mem_used": 1, "mem_total": 2,
                        "load_avg1": 0.1, "active_sessions": 0 })
            ],
        )
        .await
        .expect("resources");

    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select");
    assert_eq!(placed.state, "claimed");
    assert_eq!(placed.executor_node_id, Some(mine));

    bed.teardown().await;
}

/// MAIN-618 AC-4: the gate is KIND-BLIND, matching the sandbox gate. There is
/// no kind that does well on a full disk — a spec run writes a checkout too —
/// and a gate that held only builds would let the other four die on ENOSPC.
#[tokio::test]
async fn the_disk_floor_holds_back_every_kind() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("exec-disk").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let mine = node(&bed, tenant, Some(person), "online", build_caps(false)).await;
    // BOTH role keys: build placement filters on `role/build` and review on
    // `role/loop`, and a node missing either would be filtered out before the
    // disk gate — which would pass this test for the wrong reason.
    bed.db()
        .exec(
            r#"UPDATE nodes SET labels = '{"role/build": "true", "role/loop": "true"}'
               WHERE id = $1"#,
            params![mine],
        )
        .await
        .expect("label");

    // A control first, on a roomy node: this machine really is eligible for
    // work of a kind the loop below will find queued. Without it, a node
    // filtered out for some unrelated reason would pass the loop's assertions
    // while proving nothing about the disk floor.
    report_disk(&bed, mine, roomy(), None).await;
    let control = JobId::new();
    bed.db()
        .exec(
            "INSERT INTO loop_jobs (id, tenant_id, kind, workspace_id, requested_by, state)
             VALUES ($1,$2,'review',$3,$4,'queued')",
            params![control, tenant, ws, user],
        )
        .await
        .expect("control job");
    assert_eq!(
        jobs::select_executor(&state, tenant, control)
            .await
            .expect("control")
            .state,
        "claimed",
        "the fixture node takes loop work when it has room"
    );

    report_disk(
        &bed,
        mine,
        nearly_full(),
        Some("job cache (/) has 1.9 GiB free"),
    )
    .await;

    for kind in ["spec", "decompose", "build", "epic-run", "review"] {
        let job = JobId::new();
        // A review run is about a repository and carries no card; every other
        // kind is about one. The column CHECK allows exactly one of the two.
        let (task, workspace) = if kind == "review" {
            (None, Some(ws.0))
        } else {
            (Some(target_task(&bed, tenant, user).await.0), None)
        };
        bed.db()
            .exec(
                "INSERT INTO loop_jobs
                    (id, tenant_id, kind, target_task_id, workspace_id, requested_by, state)
                 VALUES ($1,$2,$3,$4,$5,$6,'queued')",
                params![job, tenant, kind, task, workspace, user],
            )
            .await
            .expect("job");

        let placed = jobs::select_executor(&state, tenant, job)
            .await
            .expect("select");
        assert_eq!(placed.state, "queued", "{kind} must wait, not run");
        assert!(
            matches!(
                placed.queued_reason_kind,
                Some(QueuedReason::DiskUnavailable { .. })
            ),
            "{kind} is held by the disk floor, not by something else: {:?}",
            placed.queued_reason_kind
        );
    }

    bed.teardown().await;
}

/// MAIN-618 AC-7: "waiting for space that never frees" must not become a silent
/// forever-wait. A job queued on disk is reached by the starvation escalation
/// exactly like any other queued job.
#[tokio::test]
async fn a_job_queued_on_disk_is_still_reachable_by_the_starvation_sweep() {
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
    report_disk(
        &bed,
        mine,
        nearly_full(),
        Some("job cache (/) has 1.9 GiB free"),
    )
    .await;

    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select");
    assert_eq!(placed.state, "queued");

    // `0` seconds: anything queued with a reason is past the threshold, which
    // is what makes this about REACHABILITY rather than about the clock.
    let ended = jobs::escalate_starved_queued(&state, tenant, 0)
        .await
        .expect("sweep");
    assert_eq!(
        ended, 1,
        "the sweep reaches a disk-queued job like any other"
    );

    bed.teardown().await;
}

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
    assert_eq!(
        placed.queued_reason_kind,
        Some(QueuedReason::AtCapacity),
        "MAIN-494: and says it as a value, so a client is not matching words"
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

/// MAIN-616 AC-1: a job paused on a human still holds its slot.
///
/// The node has not let go of anything — `loop_job::run` has not returned, so
/// the `Sandbox` and its container are alive — and counting only the moving
/// runs is what let three unanswered specs sit on a machine whose capacity says
/// two.
#[tokio::test]
async fn a_job_paused_on_a_human_still_holds_its_nodes_slot() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, user, person, job) = setup(&bed).await;
    let mut caps = caps_declaring(&["spec"], false);
    caps["max_loop_jobs"] = json!(2);
    let mine = node(&bed, tenant, Some(person), "online", caps).await;

    // One moving, one paused: two slots of a two-slot machine.
    for job_state in ["running", "waiting_on_human"] {
        let target = target_task(&bed, tenant, user).await;
        let held = queued_job(&bed, tenant, user, target).await;
        bed.db()
            .exec(
                "UPDATE loop_jobs SET state = $2, executor_node_id = $3 WHERE id = $1",
                params![held, job_state, mine],
            )
            .await
            .expect("occupy");
    }

    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select");
    assert_eq!(
        placed.state, "queued",
        "the third job waits — the paused one has not given the node back"
    );
    let reason = placed.queued_reason.clone().unwrap_or_default();
    assert!(
        reason.contains("waiting on a human"),
        "AC-3: the sentence sends an operator to the interview, not to a bigger \
         machine: {reason}"
    );
    let node_name: String = bed
        .db()
        .query_scalar("SELECT name FROM nodes WHERE id = $1", params![mine])
        .await
        .expect("name");
    assert_eq!(
        placed.queued_reason_kind,
        Some(QueuedReason::WaitingOnHuman {
            node_name,
            paused: 1
        }),
        "and says it as a value, naming the node the answer is owed to"
    );

    bed.teardown().await;
}

/// AC-3's other half: a node that is genuinely full still says so. The remedy
/// for two running jobs is not "go answer something".
#[tokio::test]
async fn a_node_full_of_moving_work_still_reports_plain_capacity() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, user, person, job) = setup(&bed).await;
    let mut caps = caps_declaring(&["spec"], false);
    caps["max_loop_jobs"] = json!(2);
    let mine = node(&bed, tenant, Some(person), "online", caps).await;

    for _ in 0..2 {
        let target = target_task(&bed, tenant, user).await;
        let held = queued_job(&bed, tenant, user, target).await;
        bed.db()
            .exec(
                "UPDATE loop_jobs SET state = 'running', executor_node_id = $2 WHERE id = $1",
                params![held, mine],
            )
            .await
            .expect("occupy");
    }

    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select");
    assert_eq!(placed.state, "queued");
    assert_eq!(
        placed.queued_reason_kind,
        Some(QueuedReason::AtCapacity),
        "nobody is waiting on a human here"
    );

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
    assert_eq!(
        placed.queued_reason_kind,
        Some(QueuedReason::NoRoleLabel {
            label: "role/build".into()
        }),
        "MAIN-494: the gate names the SELECTOR key the label widens to, which \
         is what a client would have to set, not the words in the sentence"
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
        "sandbox": { "state": "ready", "image": "nook-job-sandbox:test" },
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

async fn node_name(bed: &TestBed, id: NodeId) -> String {
    bed.db()
        .query_scalar("SELECT name FROM nodes WHERE id = $1", params![id])
        .await
        .expect("node name")
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
    assert_eq!(
        held.queued_reason_kind,
        Some(QueuedReason::PinnedNodeUnavailable {
            node_name: node_name(&bed, dark).await
        }),
        "MAIN-494: the gate carries the node, so a client does not have to \
         find the name inside the sentence"
    );
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

/// Set the cordon a node reports about itself (MAIN-505).
async fn cordon(bed: &TestBed, node: NodeId, reason: &str, jobs_in_flight: u32) {
    bed.db()
        .exec(
            "UPDATE nodes SET cordon = $2 WHERE id = $1",
            params![
                node,
                json!({
                    "reason": reason,
                    "jobs_in_flight": jobs_in_flight,
                    "since": "2026-08-10T00:00:00Z",
                    "overdue": false
                })
            ],
        )
        .await
        .expect("cordon");
}

/// MAIN-505 AC-2: a node draining before an agent restart takes no new loop
/// work. Without this the deferral never converges — fresh runs keep arriving
/// and the quiet moment it is waiting for never comes.
#[tokio::test]
async fn a_cordoned_node_takes_no_new_loop_work() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, _user, person, job) = setup(&bed).await;
    let draining = node(
        &bed,
        tenant,
        Some(person),
        "online",
        caps("authorized", false),
    )
    .await;
    cordon(&bed, draining, "deferring the agent update to 0.6.7", 1).await;

    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select");
    assert_eq!(
        placed.state, "queued",
        "eligible, online, and still skipped"
    );
    assert!(placed.executor_node_id.is_none());
    // AC-3: "why did nothing get placed on azul" is answered here, by name and
    // in the node's own words, rather than blamed on auth that is in fact fine.
    let reason = placed.queued_reason.expect("a reason is recorded");
    assert!(reason.contains("cordoned"), "{reason}");
    assert!(
        reason.contains("0.6.7"),
        "the node's own sentence: {reason}"
    );

    bed.teardown().await;
}

/// The cordon withholds work; it does not take the node out of the fleet. An
/// uncordoned peer still gets the job.
#[tokio::test]
async fn work_goes_to_an_uncordoned_peer_instead() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, _user, person, job) = setup(&bed).await;
    let draining = node(
        &bed,
        tenant,
        Some(person),
        "online",
        caps("authorized", false),
    )
    .await;
    cordon(&bed, draining, "deferring the agent update to 0.6.7", 1).await;
    let free = node(
        &bed,
        tenant,
        Some(person),
        "online",
        caps("authorized", false),
    )
    .await;

    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select");
    assert_eq!(placed.state, "claimed");
    assert_eq!(placed.executor_node_id, Some(free));

    bed.teardown().await;
}

/// And it lifts: once the node reports no cordon it is an ordinary candidate
/// again, with nothing else having to change. A cordon that never lifted would
/// be worse than no cordon — the machine would go quietly dark.
#[tokio::test]
async fn a_lifted_cordon_makes_the_node_placeable_again() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, tenant, _user, person, job) = setup(&bed).await;
    let n = node(
        &bed,
        tenant,
        Some(person),
        "online",
        caps("authorized", false),
    )
    .await;
    cordon(&bed, n, "deferring the agent update to 0.6.7", 1).await;
    assert_eq!(
        jobs::select_executor(&state, tenant, job)
            .await
            .expect("select")
            .state,
        "queued"
    );

    bed.db()
        .exec("UPDATE nodes SET cordon = NULL WHERE id = $1", params![n])
        .await
        .expect("lift");

    let placed = jobs::select_executor(&state, tenant, job)
        .await
        .expect("select again");
    assert_eq!(placed.state, "claimed");
    assert_eq!(placed.executor_node_id, Some(n));

    bed.teardown().await;
}

// ── MAIN-515: ownership crosses tenants; sharing does not ────────────────────
//
// One human, two orgs, every machine joined under the first: before this, every
// loop job raised in the second parked on "waiting for executor" forever,
// because eligibility ANDed a hard `tenant_id` filter with person-based
// ownership. Reachability already travelled with the owner (MAIN-353) — only
// placement did not.

/// A user in `tenant` for an EXISTING person — how one human holds two orgs.
async fn member(bed: &TestBed, tenant: TenantId, person: Uuid, role: &str) -> UserId {
    let user = UserId::new();
    bed.db()
        .exec(
            "INSERT INTO users (id, tenant_id, person_id, display_name, email, role)
             VALUES ($1, $2, $3, 'U', $4, $5)",
            params![
                user,
                tenant,
                person,
                format!("u-{}@example.test", user.0.simple()),
                role.to_string()
            ],
        )
        .await
        .expect("member");
    user
}

/// One person's two tenants, and a queued `spec` job in the SECOND — the shape
/// every test below starts from. Returns
/// `(state, tenant_a, tenant_b, user_in_a, user_in_b, person, job_in_b)`.
async fn two_tenants(bed: &TestBed) -> (AppState, TenantId, TenantId, UserId, UserId, Uuid, JobId) {
    let a = bed.tenant("xta").await;
    let b = bed.tenant("xtb").await;
    let (in_a, person) = bed.user(a, "owner").await;
    let in_b = member(bed, b, person, "owner").await;
    let target = target_task(bed, b, in_b).await;
    let job = queued_job(bed, b, in_b, target).await;
    (bed.app_state().await, a, b, in_a, in_b, person, job)
}

/// AC-1, the reported bug: the machine follows its owner into their other org.
#[tokio::test]
async fn your_own_node_in_another_tenant_runs_your_job_here() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, a, b, _in_a, _user, person, job) = two_tenants(&bed).await;
    let mine = node(&bed, a, Some(person), "online", caps("authorized", false)).await;

    assert_eq!(
        state
            .nodes
            .eligible_loop_executors(b, person, "claude", "spec")
            .await
            .expect("candidates"),
        vec![mine],
        "a node joined into A is a candidate for its owner's job in B"
    );
    let placed = jobs::select_executor(&state, b, job).await.expect("select");
    assert_eq!(placed.state, "claimed");
    assert_eq!(placed.executor_node_id, Some(mine));

    bed.teardown().await;
}

/// MAIN-576 AC-3, REVERSING MAIN-515's owner-only rule deliberately.
///
/// A loop run is raised as the tenant's owner (`tenant_owner_user_id`), which
/// in a tenant with two owners is whoever joined first. Owner-only crossing
/// therefore made placement depend on an accident of join order: a team whose
/// PM drafts the work could never run it on a member's machine, and the queued
/// reason said "you have no node online" to a person looking at five.
///
/// The boundary is now MEMBERSHIP, not requester-identity — see
/// `a_node_whose_owner_is_a_stranger_is_still_unreachable` for the wall that
/// replaced it.
#[tokio::test]
async fn a_teammate_reaches_a_fellow_members_machine() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, a, b, _in_a, _user, person, _job) = two_tenants(&bed).await;
    let mine = node(&bed, a, Some(person), "online", caps("authorized", false)).await;

    let (teammate, teammate_person) = bed.user(b, "member").await;
    assert_eq!(
        state
            .nodes
            .eligible_loop_executors(b, teammate_person, "claude", "spec")
            .await
            .expect("candidates"),
        vec![mine],
        "the owner is a member of B, so their machine serves B's work"
    );

    let target = target_task(&bed, b, teammate).await;
    let theirs = queued_job(&bed, b, teammate, target).await;
    let placed = jobs::select_executor(&state, b, theirs)
        .await
        .expect("select");
    assert_eq!(placed.state, "claimed", "this is the reported bug");
    assert_eq!(placed.executor_node_id, Some(mine));

    bed.teardown().await;
}

/// MAIN-576 AC-3's wall: membership is the boundary, and a stranger's machine
/// is still nobody's to take. This is what stops the widening from being
/// "any node anywhere".
#[tokio::test]
async fn a_node_whose_owner_is_a_stranger_is_still_unreachable() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, a, b, _in_a, _user, _person, _job) = two_tenants(&bed).await;
    // A person who belongs to A alone — never a member of B.
    let (_outsider, outsider_person) = bed.user(a, "member").await;
    node(
        &bed,
        a,
        Some(outsider_person),
        "online",
        caps("authorized", false),
    )
    .await;

    let (_teammate, teammate_person) = bed.user(b, "member").await;
    assert!(
        state
            .nodes
            .eligible_loop_executors(b, teammate_person, "claude", "spec")
            .await
            .expect("candidates")
            .is_empty(),
        "a machine whose owner does not belong to B is not B's to run work on"
    );

    bed.teardown().await;
}

/// MAIN-576 AC-4, against the case the widening actually introduced: a FELLOW
/// MEMBER's cross-tenant machine. The existing gate tests all run through the
/// requester's own node, which satisfied the OLD leg too — so they never
/// exercised the new one.
#[tokio::test]
async fn every_gate_still_applies_to_a_fellow_members_cross_tenant_node() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, a, b, _in_a, _user, person, _job) = two_tenants(&bed).await;
    // Owned by `person` (a member of B) but NOT authorized for the runtime.
    let unauthorized = node(&bed, a, Some(person), "online", caps("pending", false)).await;

    let (teammate, teammate_person) = bed.user(b, "member").await;
    assert!(
        state
            .nodes
            .eligible_loop_executors(b, teammate_person, "claude", "spec")
            .await
            .expect("candidates")
            .is_empty(),
        "membership opens the door; authorization is still a separate gate"
    );

    let target = target_task(&bed, b, teammate).await;
    let theirs = queued_job(&bed, b, teammate, target).await;
    let held = jobs::select_executor(&state, b, theirs)
        .await
        .expect("select");
    assert_eq!(held.state, "queued");

    // Authorize it and the same machine is placed — the gate was the only
    // difference, not the tenancy.
    // Whole-value assignment, not `jsonb_set`: that spelling is Postgres-only
    // and this suite runs on both engines.
    bed.db()
        .exec(
            "UPDATE nodes SET capabilities = $2 WHERE id = $1",
            params![unauthorized, caps("authorized", false)],
        )
        .await
        .expect("authorize");
    let placed = jobs::select_executor(&state, b, theirs)
        .await
        .expect("again");
    assert_eq!(placed.state, "claimed");
    assert_eq!(placed.executor_node_id, Some(unauthorized));

    bed.teardown().await;
}

/// MAIN-576 AC-8: the reason stops lying. A tenant whose member machines are
/// online and have DECLINED is told exactly that — not "you have no node
/// online", which is what the reader was looking at five of when this was
/// reported.
#[tokio::test]
async fn the_reason_names_withdrawn_consent_rather_than_claiming_nothing_is_online() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, a, b, _in_a, _user, person, _job) = two_tenants(&bed).await;
    let mine = node(&bed, a, Some(person), "online", caps("authorized", false)).await;
    state
        .nodes
        .set_cross_tenant(mine, false)
        .await
        .expect("withdraw");

    // The requester owns nothing, so every ownership-based count is zero —
    // the exact shape that used to fall through to the misleading arm.
    let (teammate, _teammate_person) = bed.user(b, "member").await;
    let target = target_task(&bed, b, teammate).await;
    let theirs = queued_job(&bed, b, teammate, target).await;

    let held = jobs::select_executor(&state, b, theirs)
        .await
        .expect("select");
    assert_eq!(held.state, "queued");
    let reason = held.queued_reason.unwrap_or_default();
    assert!(
        reason.contains("withdrawn cross-tenant consent"),
        "the reason names the consent, got: {reason}"
    );
    assert!(
        !reason.contains("you have no node online"),
        "and does not claim the fleet is empty, got: {reason}"
    );

    bed.teardown().await;
}

/// MAIN-576 AC-8's other half: whichever arm speaks, it says WHO the run was
/// raised as, because "you" is the tenant's oldest owner and not the reader.
#[tokio::test]
async fn the_reason_names_the_identity_the_run_was_raised_as() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, _a, b, _in_a, _user, _person, job) = two_tenants(&bed).await;

    let held = jobs::select_executor(&state, b, job).await.expect("select");
    assert_eq!(held.state, "queued", "nothing is online at all here");
    let reason = held.queued_reason.unwrap_or_default();
    assert!(
        reason.contains("this run was raised as"),
        "the reason attributes itself, got: {reason}"
    );

    bed.teardown().await;
}

/// MAIN-576 AC-6: the widening is consented to, and the consent is withdrawable
/// by the owner alone.
#[tokio::test]
async fn an_owner_can_withdraw_cross_tenant_consent() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, a, b, _in_a, _user, person, job) = two_tenants(&bed).await;
    let mine = node(&bed, a, Some(person), "online", caps("authorized", false)).await;

    state
        .nodes
        .set_cross_tenant(mine, false)
        .await
        .expect("withdraw");
    let held = jobs::select_executor(&state, b, job).await.expect("select");
    assert_eq!(
        held.state, "queued",
        "a machine that declined is not a candidate, even for its own owner"
    );

    state
        .nodes
        .set_cross_tenant(mine, true)
        .await
        .expect("restore");
    let placed = jobs::select_executor(&state, b, job).await.expect("again");
    assert_eq!(placed.state, "claimed", "consent is the whole difference");
    assert_eq!(placed.executor_node_id, Some(mine));

    bed.teardown().await;
}

/// AC-2: the shared-operator branch keeps its tenant scoping. A shared operator
/// is a grant to ONE team, and it stays that team's — even when the requester
/// is the person who joined it, which is the sharper half of the rule.
#[tokio::test]
async fn a_shared_operator_does_not_cross_tenants() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, a, b, in_a, _user, person, job) = two_tenants(&bed).await;
    let unowned = node(&bed, a, None, "online", caps("authorized", true)).await;
    let mine = node(&bed, a, Some(person), "online", caps("authorized", true)).await;

    assert!(
        state
            .nodes
            .eligible_loop_executors(b, person, "claude", "spec")
            .await
            .expect("candidates")
            .is_empty(),
        "neither an unowned nor an owner-joined shared operator serves another tenant"
    );
    assert_eq!(
        jobs::select_executor(&state, b, job)
            .await
            .expect("select")
            .state,
        "queued"
    );

    // …and nothing was taken away from A: both are still candidates there,
    // still in own-before-shared order.
    assert_eq!(
        state
            .nodes
            .eligible_loop_executors(a, person, "claude", "spec")
            .await
            .expect("candidates at home"),
        vec![mine, unowned]
    );
    let target = target_task(&bed, a, in_a).await;
    let at_home = queued_job(&bed, a, in_a, target).await;
    let placed = jobs::select_executor(&state, a, at_home)
        .await
        .expect("select at home");
    assert_eq!(placed.state, "claimed");
    assert_eq!(placed.executor_node_id, Some(mine));

    bed.teardown().await;
}

/// AC-4: crossing the boundary widens WHO is looked at, not WHAT is required.
/// Offline, unauthorized and undeclared-kind all still exclude, and the reason
/// no longer claims you have no node online when you plainly do.
#[tokio::test]
async fn every_eligibility_gate_still_applies_across_the_tenant_boundary() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, a, b, _in_a, _user, person, job) = two_tenants(&bed).await;
    node(&bed, a, Some(person), "offline", caps("authorized", false)).await;
    node(
        &bed,
        a,
        Some(person),
        "online",
        caps("not_authorized", false),
    )
    .await;
    node(
        &bed,
        a,
        Some(person),
        "online",
        caps_declaring(&["review"], false),
    )
    .await;

    assert!(
        state
            .nodes
            .eligible_loop_executors(b, person, "claude", "spec")
            .await
            .expect("candidates")
            .is_empty(),
        "every gate that excluded at home excludes across the boundary too"
    );
    let held = jobs::select_executor(&state, b, job).await.expect("select");
    assert_eq!(held.state, "queued");
    let reason = held.queued_reason.unwrap_or_default();
    assert!(
        reason.contains("your online node(s)"),
        "your nodes are online — wherever they are homed — so the reason must not \
         say otherwise: {reason}"
    );

    // One node that passes every gate, in A, and the job places.
    let good = node(
        &bed,
        a,
        Some(person),
        "online",
        caps_declaring(&["spec"], false),
    )
    .await;
    let placed = jobs::select_executor(&state, b, job)
        .await
        .expect("select again");
    assert_eq!(placed.state, "claimed");
    assert_eq!(placed.executor_node_id, Some(good));

    bed.teardown().await;
}

/// AC-4's label gate, which is where placement used to be undone AFTER the
/// candidate query was widened: the per-candidate lookup was tenant-scoped, so
/// a cross-tenant node was fetched as `None` and silently dropped.
#[tokio::test]
async fn a_cross_tenant_build_still_needs_the_role_build_label() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, a, b, _in_a, user, person, _spec_job) = two_tenants(&bed).await;
    let mine = node(&bed, a, Some(person), "online", build_caps(false)).await;
    let target = target_task(&bed, b, user).await;
    let job = queued_build_job(&bed, b, user, target).await;

    let held = jobs::select_executor(&state, b, job).await.expect("select");
    assert_eq!(
        held.state, "queued",
        "unlabeled is unlabeled, in any tenant"
    );
    assert!(
        held.queued_reason
            .unwrap_or_default()
            .contains("role=build"),
        "and the reason names the label rather than blaming tenancy"
    );

    bed.db()
        .exec(
            r#"UPDATE nodes SET labels = '{"role": "build"}' WHERE id = $1"#,
            params![mine],
        )
        .await
        .expect("label");
    let placed = jobs::select_executor(&state, b, job)
        .await
        .expect("select again");
    assert_eq!(placed.state, "claimed", "the label is the whole difference");
    assert_eq!(placed.executor_node_id, Some(mine));

    bed.teardown().await;
}

/// AC-4's capacity gate: a cordon on a cross-tenant node reads as capacity,
/// exactly as it does at home.
#[tokio::test]
async fn capacity_still_stops_a_cross_tenant_candidate() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, a, b, _in_a, _user, person, job) = two_tenants(&bed).await;
    let mut caps = caps_declaring(&["spec"], false);
    caps["max_loop_jobs"] = json!(0);
    node(&bed, a, Some(person), "online", caps).await;

    let held = jobs::select_executor(&state, b, job).await.expect("select");
    assert_eq!(held.state, "queued");
    assert!(held.queued_reason.unwrap_or_default().contains("capacity"));

    bed.teardown().await;
}

/// AC-6: when the only thing online is a shared operator in the owner's OTHER
/// tenant, the refusal is tenancy — say so, rather than sending them hunting
/// for capacity that was never the problem.
#[tokio::test]
async fn the_reason_names_tenancy_when_the_only_online_node_is_a_foreign_operator() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, a, b, _in_a, _user, person, job) = two_tenants(&bed).await;
    node(&bed, a, Some(person), "online", caps("authorized", true)).await;

    let held = jobs::select_executor(&state, b, job).await.expect("select");
    assert_eq!(held.state, "queued");
    let reason = held.queued_reason.unwrap_or_default();
    assert!(
        reason.contains("shared operator") && reason.contains("another of your tenants"),
        "the reason names the rule that refused it: {reason}"
    );
    assert!(
        !reason.contains("you have no node online"),
        "…and does not claim the machine they are looking at is absent: {reason}"
    );

    bed.teardown().await;
}

/// AC-6's other half, and the state the first cut of this branch missed: an
/// in-tenant shared operator that EXISTS but is ineligible must not turn the
/// tenancy answer back into "you have no node online". Both facts get said —
/// the operator here is no good, and the machines you can see are elsewhere.
#[tokio::test]
async fn an_ineligible_local_operator_does_not_bury_the_tenancy_reason() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (state, a, b, _in_a, _user, person, job) = two_tenants(&bed).await;
    // Online and owned, but a shared operator in the OTHER tenant: refused for
    // where it is joined.
    node(&bed, a, Some(person), "online", caps("authorized", true)).await;
    // …and this tenant does have an operator — it simply is not authorized.
    node(&bed, b, None, "online", caps("not_authorized", true)).await;

    let held = jobs::select_executor(&state, b, job).await.expect("select");
    assert_eq!(held.state, "queued");
    let reason = held.queued_reason.unwrap_or_default();
    assert!(
        !reason.contains("you have no node online"),
        "the owner's machines ARE online — saying otherwise is the sentence \
         that sent them hunting capacity: {reason}"
    );
    assert!(
        reason.contains("another of your tenants"),
        "the tenancy rule is still named: {reason}"
    );
    assert!(
        reason.contains("not authorized"),
        "…alongside the local operator's own state, which is also true: {reason}"
    );

    bed.teardown().await;
}
