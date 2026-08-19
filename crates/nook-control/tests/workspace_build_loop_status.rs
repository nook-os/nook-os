//! MAIN-495: what the build loop can actually deliver, for the surface that
//! sets its ceiling.
//!
//! The number under test is `capacity`, and the mistake it exists to prevent is
//! a tenant-wide total: a fleet of four machines reporting eight slots while a
//! job sits behind the one node that may build, and its two. So every test here
//! scripts a node into exactly one ineligible state and asserts BOTH halves —
//! that it added nothing, and that it is named with the ground it failed on.
//! A count alone would pass while the panel told somebody to fix the wrong
//! thing.

use axum::extract::{Path, State};
use nook_control::auth::{AuthCtx, Principal};
use nook_control::routes::workspaces::{build_loop_status, set_build_loop};
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use serde_json::json;
use uuid::Uuid;

fn user_ctx(user: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: false,
    }
}

fn req(n: i32) -> axum::Json<SetBuildLoopSettingsRequest> {
    axum::Json(serde_json::from_value(json!({ "concurrency": n })).expect("request body"))
}

/// Capabilities for a machine that may take build work. `slots` is what the
/// node ADVERTISES; `None` is the older agent that reports nothing.
fn build_caps(slots: Option<u32>) -> serde_json::Value {
    let mut c = json!({
        "loop_kinds": ["spec", "decompose", "review", "epic-run", "build"],
        "sandbox": { "state": "ready", "image": "nook-job-sandbox:test" },
        "runtime_auth": [
            { "id": "claude", "label": "Claude Code", "runtime": "claude", "state": "authorized" }
        ]
    });
    if let Some(n) = slots {
        c["max_loop_jobs"] = json!(n);
    }
    c
}

/// Insert a node with an explicit name, owner, status and capabilities.
async fn node(
    bed: &TestBed,
    tenant: TenantId,
    name: &str,
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
                name.to_string(),
                format!("h-{}", id.0.simple()),
                status.to_string(),
                owner,
                caps
            ],
        )
        .await
        .expect("create node");
    id
}

/// The `role=build` label, which lives in its own column and widens to the
/// `role/build` selector key placement matches on (MAIN-463).
async fn label_for_build(bed: &TestBed, id: NodeId) {
    bed.db()
        .exec(
            r#"UPDATE nodes SET labels = '{"role": "build"}' WHERE id = $1"#,
            params![id],
        )
        .await
        .expect("label");
}

/// A node that qualifies in every respect, advertising `slots`.
async fn eligible_node(
    bed: &TestBed,
    tenant: TenantId,
    name: &str,
    owner: Uuid,
    slots: Option<u32>,
) -> NodeId {
    let id = node(bed, tenant, name, Some(owner), "online", build_caps(slots)).await;
    label_for_build(bed, id).await;
    id
}

/// A board with a card on it — a build job must name one (`loop_jobs_target_check`),
/// and 0050's per-card index means one card per LIVE run.
async fn card(
    bed: &TestBed,
    tenant: TenantId,
    creator: UserId,
    board: BoardId,
    col: ColumnId,
) -> TaskId {
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

async fn board_with_column(bed: &TestBed, tenant: TenantId) -> (BoardId, ColumnId) {
    let board = BoardId::new();
    bed.db()
        .exec(
            "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b',$3,'local')",
            // The RANDOM tail of the v7 uuid: its leading bytes are a shared
            // timestamp, so a prefix-derived key would collide across tests.
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
    (board, col)
}

fn blocker(status: &BuildLoopStatus, name: &str) -> BuildCapacityBlocker {
    status
        .blocked
        .iter()
        .find(|b| b.node_name == name)
        .unwrap_or_else(|| panic!("{name} is not counted and must therefore be named: {status:?}"))
        .reason
        .clone()
}

#[tokio::test]
async fn capacity_sums_the_viewers_eligible_nodes_and_nothing_else() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("blstatus").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let (_, stranger) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;

    eligible_node(&bed, tenant, "mine-a", person, Some(2)).await;
    eligible_node(&bed, tenant, "mine-b", person, Some(3)).await;
    // Somebody else's machine, eligible in every other way: capacity is the
    // VIEWER's, because placement will only ever reach for their own nodes.
    eligible_node(&bed, tenant, "theirs", stranger, Some(8)).await;

    let state = bed.app_state().await;
    // An OWNER, so the fleet-wide scope applies and the stranger's node is one
    // this caller may already see on the Nodes page.
    let got = build_loop_status(State(state), user_ctx(user, tenant), Path(ws))
        .await
        .expect("status")
        .0;

    assert_eq!(got.capacity, 5, "2 + 3, and never the stranger's 8");
    assert_eq!(got.eligible_nodes, 2);
    assert_eq!(
        blocker(&got, "theirs"),
        BuildCapacityBlocker::NotYours,
        "excluded for the reason it was excluded FOR, not for being idle"
    );

    bed.teardown().await;
}

/// The blocker list NAMES machines, so it answers to the same visibility rule
/// the Nodes page does (MAIN-132): a member sees their own and the shared ones,
/// never a teammate's private box.
///
/// What scoping must NOT do is move a number. The case that would have broken
/// is the cross-tenant one (MAIN-515): a machine the member owns in ANOTHER
/// tenant reaches the fleet list through the own-person leg rather than the
/// tenant leg, and a scope fix that narrowed the wrong argument would drop its
/// slots out of `capacity` while placement still used them.
#[tokio::test]
async fn a_member_keeps_every_slot_but_not_a_teammates_node_name() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("blscope").await;
    let elsewhere = bed.tenant("blscope-other").await;
    let (admin, _) = bed.user(tenant, "owner").await;
    let (member, person) = bed.user(tenant, "member").await;
    let (_, stranger) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;

    eligible_node(&bed, tenant, "mine", person, Some(2)).await;
    eligible_node(&bed, elsewhere, "mine-elsewhere", person, Some(3)).await;
    // A teammate's private machine: not the member's to see, and not theirs to
    // use either.
    eligible_node(&bed, tenant, "theirs", stranger, Some(8)).await;
    // Shared, so it IS one the member may already see — withholding it would
    // hide a real reason rather than protect anything.
    let shared = node(
        &bed,
        tenant,
        "shared-box",
        Some(stranger),
        "offline",
        build_caps(Some(4)),
    )
    .await;
    bed.db()
        .exec(
            "UPDATE nodes SET shared = true WHERE id = $1",
            params![shared],
        )
        .await
        .expect("share");

    let state = bed.app_state().await;
    let seen = build_loop_status(State(state.clone()), user_ctx(member, tenant), Path(ws))
        .await
        .expect("status")
        .0;

    assert_eq!(
        seen.capacity, 5,
        "2 here plus 3 in the other tenant — the scope bounds the NAMES, and \
         a machine of theirs elsewhere is still a machine a build lands on"
    );
    assert_eq!(seen.eligible_nodes, 2);

    let named: Vec<&str> = seen.blocked.iter().map(|b| b.node_name.as_str()).collect();
    assert!(
        !named.contains(&"theirs"),
        "a member must not learn the name of a machine /api/v1/nodes would not \
         have shown them: {named:?}"
    );
    assert!(
        named.contains(&"shared-box"),
        "a shared node is already visible to them, and it is offline for a \
         reason worth reading: {named:?}"
    );

    // The owner's fleet-wide view is unchanged, which is what makes the line
    // above a SCOPE rule rather than the list quietly having been dropped.
    let by_owner = build_loop_status(State(state.clone()), user_ctx(admin, tenant), Path(ws))
        .await
        .expect("status")
        .0;
    assert_eq!(blocker(&by_owner, "theirs"), BuildCapacityBlocker::NotYours);

    bed.teardown().await;
}

/// A node that says it is DRAINING (MAIN-505) and one told centrally to stop
/// claiming are one instruction spelled two ways, and must read the same here.
/// Neither is blocked: the machine still accepts build work, and calling it
/// blocked would render one draining node as "no node of yours accepts build
/// work".
#[tokio::test]
async fn a_draining_node_delivers_nothing_while_still_being_a_node_that_builds() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("blcordon").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;

    let n = eligible_node(&bed, tenant, "azul", person, Some(2)).await;
    bed.db()
        .exec(
            "UPDATE nodes SET cordon = $2 WHERE id = $1",
            params![
                n,
                json!({
                    "reason": "draining for an agent upgrade",
                    "jobs_in_flight": 1,
                    "since": "2026-08-11T00:00:00Z",
                    "overdue": false
                })
            ],
        )
        .await
        .expect("cordon");

    let state = bed.app_state().await;
    let got = build_loop_status(State(state), user_ctx(user, tenant), Path(ws))
        .await
        .expect("status")
        .0;

    assert_eq!(
        got.capacity, 0,
        "placement will not put a build here, so neither may the number that \
         claims to describe placement"
    );
    assert_eq!(
        got.eligible_nodes, 1,
        "draining is 'not right now', not 'never' — the same reading the \
         central 0 already gets"
    );
    assert!(got.blocked.is_empty());

    bed.teardown().await;
}

#[tokio::test]
async fn every_ineligible_node_is_named_with_its_own_ground() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("blgrounds").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let (_, stranger) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;

    // One node per ground, each differing from an eligible one in exactly one
    // respect — which is what makes the reason it is given falsifiable.
    let dark = node(
        &bed,
        tenant,
        "dark",
        Some(person),
        "offline",
        build_caps(Some(2)),
    )
    .await;
    label_for_build(&bed, dark).await;

    node(
        &bed,
        tenant,
        "unlabelled",
        Some(person),
        "online",
        build_caps(Some(2)),
    )
    .await;

    let wrong_kind = node(
        &bed,
        tenant,
        "specs-only",
        Some(person),
        "online",
        json!({
            "loop_kinds": ["spec"],
            "sandbox": { "state": "ready", "image": "nook-job-sandbox:test" },
            "runtime_auth": [
                { "id": "claude", "label": "Claude Code", "runtime": "claude", "state": "authorized" }
            ],
            "max_loop_jobs": 2
        }),
    )
    .await;
    label_for_build(&bed, wrong_kind).await;

    let unauthorized = node(
        &bed,
        tenant,
        "logged-out",
        Some(person),
        "online",
        json!({ "loop_kinds": ["build"], "runtime_auth": [], "max_loop_jobs": 2, "sandbox": { "state": "ready", "image": "nook-job-sandbox:test" } }),
    )
    .await;
    label_for_build(&bed, unauthorized).await;

    let operator = node(
        &bed,
        tenant,
        "operator",
        Some(person),
        "online",
        json!({
            "loop_kinds": ["spec", "build"],
            "sandbox": { "state": "ready", "image": "nook-job-sandbox:test" },
            "runtime_auth": [
                { "id": "claude", "label": "Claude Code", "runtime": "claude", "state": "authorized" }
            ],
            "shared_operator": true,
            "max_loop_jobs": 4
        }),
    )
    .await;
    label_for_build(&bed, operator).await;

    eligible_node(&bed, tenant, "theirs", stranger, Some(8)).await;

    let state = bed.app_state().await;
    let got = build_loop_status(State(state), user_ctx(user, tenant), Path(ws))
        .await
        .expect("status")
        .0;

    assert_eq!(got.capacity, 0, "not one of these may take a build");
    assert_eq!(got.eligible_nodes, 0);
    assert_eq!(blocker(&got, "dark"), BuildCapacityBlocker::Offline);
    assert_eq!(
        blocker(&got, "unlabelled"),
        BuildCapacityBlocker::NoRoleLabel {
            label: "role/build".into()
        },
        "the ground names the SELECTOR key the label widens to — what a person \
         would have to set, not the words in a sentence"
    );
    assert_eq!(
        blocker(&got, "specs-only"),
        BuildCapacityBlocker::KindNotAccepted {
            kind: "build".into()
        }
    );
    assert_eq!(
        blocker(&got, "logged-out"),
        BuildCapacityBlocker::RuntimeNotAuthorized {
            runtime: "claude".into()
        },
        "an unreported runtime is a refusal here, not an unknown: placement \
         requires a positive authorized entry"
    );
    assert_eq!(
        blocker(&got, "operator"),
        BuildCapacityBlocker::SharedOperator,
        "the build wall is the control plane's, so the operator's own labels \
         and declarations change nothing about it (MAIN-383)"
    );
    assert_eq!(blocker(&got, "theirs"), BuildCapacityBlocker::NotYours);

    bed.teardown().await;
}

#[tokio::test]
async fn a_node_reporting_no_slots_counts_as_what_placement_assumes() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("blunreported").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;

    eligible_node(&bed, tenant, "old-agent", person, None).await;

    let state = bed.app_state().await;
    let got = build_loop_status(State(state), user_ctx(user, tenant), Path(ws))
        .await
        .expect("status")
        .0;

    assert_eq!(
        got.capacity,
        nook_control::services::jobs::CAPACITY_WHEN_UNREPORTED,
        "the same number placement assumes — inventing a second answer here \
         would make the report disagree with the thing it describes"
    );
    assert_eq!(got.eligible_nodes, 1);
    assert!(got.blocked.is_empty());

    bed.teardown().await;
}

#[tokio::test]
async fn an_operators_central_number_is_the_one_that_counts() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("blcentral").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;

    // Advertised 2, set centrally to 4 (MAIN-508): placement uses the central
    // value without the node restarting, and so must the report of it.
    let n = eligible_node(&bed, tenant, "azul", person, Some(2)).await;
    bed.db()
        .exec(
            "UPDATE nodes SET max_loop_jobs = 4 WHERE id = $1",
            params![n],
        )
        .await
        .expect("central capacity");

    let state = bed.app_state().await;
    let got = build_loop_status(State(state), user_ctx(user, tenant), Path(ws))
        .await
        .expect("status")
        .0;

    assert_eq!(got.capacity, 4);

    // Zero is a cordon, not an absence: the machine is still eligible, it is
    // simply delivering nothing, and folding it into `eligible_nodes` would
    // make this read as "no node of yours accepts build work".
    bed.db()
        .exec(
            "UPDATE nodes SET max_loop_jobs = 0 WHERE id = $1",
            params![n],
        )
        .await
        .expect("cordon");
    let state = bed.app_state().await;
    let got = build_loop_status(State(state), user_ctx(user, tenant), Path(ws))
        .await
        .expect("status")
        .0;
    assert_eq!(got.capacity, 0);
    assert_eq!(got.eligible_nodes, 1);

    bed.teardown().await;
}

#[tokio::test]
async fn zero_eligible_nodes_is_an_absence_rather_than_a_limit() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("blempty").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;

    let state = bed.app_state().await;
    let got = build_loop_status(State(state), user_ctx(user, tenant), Path(ws))
        .await
        .expect("status")
        .0;

    assert_eq!(got.capacity, 0);
    assert_eq!(
        got.eligible_nodes, 0,
        "the two are carried apart so a UI can say 'no node of yours accepts \
         build work' rather than 'capacity 0', which reads as a setting"
    );
    assert!(got.blocked.is_empty(), "there is no node to blame");

    bed.teardown().await;
}

#[tokio::test]
async fn a_ceiling_above_capacity_still_saves_and_is_reported_as_shortfall() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("blshortfall").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    eligible_node(&bed, tenant, "solo", person, Some(2)).await;

    let state = bed.app_state().await;
    let auth = user_ctx(user, tenant);

    let unset = build_loop_status(State(state.clone()), auth, Path(ws))
        .await
        .expect("status")
        .0;
    assert_eq!(unset.desired, 1, "unset means the default ceiling of one");
    assert_eq!(unset.shortfall, 0);

    // AC-5: advisory only. Fleet capacity changes without warning, so a
    // refusal correct at write time is wrong an hour later.
    let saved = set_build_loop(State(state.clone()), auth, Path(ws), req(3))
        .await
        .expect("a ceiling above capacity is still a legal declaration")
        .0;
    assert_eq!(saved.concurrency, Some(3));

    let over = build_loop_status(State(state.clone()), auth, Path(ws))
        .await
        .expect("status")
        .0;
    assert_eq!(over.desired, 3);
    assert_eq!(over.capacity, 2);
    assert_eq!(over.shortfall, 1, "three asked for, two deliverable");

    let _ = set_build_loop(State(state.clone()), auth, Path(ws), req(2))
        .await
        .expect("set 2");
    let level = build_loop_status(State(state.clone()), auth, Path(ws))
        .await
        .expect("status")
        .0;
    assert_eq!(
        level.shortfall, 0,
        "at capacity is healthy, whatever the fleet happens to be busy with"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn running_counts_the_runs_holding_a_slot_and_never_the_queued_one() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("blrunning").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    eligible_node(&bed, tenant, "solo", person, Some(2)).await;

    let (board, col) = board_with_column(&bed, tenant).await;
    for st in ["running", "running", "queued", "completed"] {
        let target = card(&bed, tenant, user, board, col).await;
        bed.db()
            .exec(
                "INSERT INTO loop_jobs (id, tenant_id, workspace_id, kind, target_task_id, requested_by, state)
                 VALUES ($1,$2,$3,'build',$4,$5,$6)",
                params![JobId::new(), tenant, ws, target, user, st.to_string()],
            )
            .await
            .expect("build run");
    }

    let state = bed.app_state().await;
    let auth = user_ctx(user, tenant);
    let _ = set_build_loop(State(state.clone()), auth, Path(ws), req(3))
        .await
        .expect("set 3");

    let got = build_loop_status(State(state.clone()), auth, Path(ws))
        .await
        .expect("status")
        .0;

    assert_eq!(
        got.running, 2,
        "a queued run is not running — it is very often what the shortfall is \
         about, and counting it would hide that"
    );
    assert_eq!(got.shortfall, 1);

    bed.teardown().await;
}
