//! The typed queued reason (MAIN-494), against a real database.
//!
//! `jobs_fake.rs` pins the guard behaviour on the in-memory repository. What
//! only a database can answer is whether the variant SURVIVES the column: it is
//! stored as JSON, and a shape that serializes but does not decode back would
//! look correct everywhere until a client read one.
//!
//! Needs Postgres: `DATABASE_URL` (`NOOK_REQUIRE_DB=1` in the suite).

use nook_db::{params, Db};
use nook_types::*;

use nook_testkit::TestBed;

/// A board + column + task for a build run to own.
async fn card(bed: &TestBed, tenant: TenantId, creator: UserId) -> TaskId {
    let board = BoardId::new();
    bed.db()
        .exec(
            "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b',$3,'local')",
            // The RANDOM tail of the v7 uuid: its leading bytes are a shared
            // timestamp, so two boards made in one test collide on a prefix.
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

/// A queued `build` job — the kind whose runs the Builds panel lists.
async fn queued_build(
    bed: &TestBed,
    tenant: TenantId,
    user: UserId,
    workspace: WorkspaceId,
    task: TaskId,
) -> JobId {
    let id = JobId::new();
    bed.db()
        .exec(
            "INSERT INTO loop_jobs
                (id, tenant_id, kind, target_task_id, workspace_id, requested_by, state)
             VALUES ($1,$2,'build',$3,$4,$5,'queued')",
            params![id, tenant, task, workspace, user],
        )
        .await
        .expect("job");
    id
}

/// Every variant the dispatcher can write, each carrying the detail that makes
/// it actionable — the list AC-1 fixes.
fn every_variant() -> Vec<QueuedReason> {
    vec![
        QueuedReason::NoPersonIdentity,
        QueuedReason::PinnedNodeUnavailable {
            node_name: "builder-2".into(),
        },
        QueuedReason::AtCapacity,
        QueuedReason::NoRoleLabel {
            label: "role/build".into(),
        },
        QueuedReason::KindWallRefusal {
            kind: "build".into(),
        },
        QueuedReason::PortsUnavailable {
            listener: "web".into(),
            env: "NOOK_WEB_PORT".into(),
        },
        QueuedReason::SandboxUnavailable {
            node_name: "azul".into(),
            detail: "no Docker daemon on this node".into(),
        },
        QueuedReason::WaitingOnHuman {
            node_name: "azul".into(),
            paused: 2,
        },
    ]
}

#[tokio::test]
async fn every_variant_round_trips_through_the_column() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("qr").await;
    let (user, _person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    for variant in every_variant() {
        let task = card(&bed, tenant, user).await;
        let job = queued_build(&bed, tenant, user, ws, task).await;

        state
            .jobs
            .set_queued_reason(job, "the sentence", Some(variant.clone()))
            .await
            .expect("write");

        let row = state.jobs.reload(job).await.expect("reload");
        assert_eq!(
            row.queued_reason_kind,
            Some(variant.clone()),
            "{variant:?} came back changed, so a client would branch on the wrong gate"
        );
        // The sentence is still the rendering — the type is an addition to it,
        // never a replacement (AC-3).
        assert_eq!(row.queued_reason.as_deref(), Some("the sentence"));
    }

    bed.teardown().await;
}

#[tokio::test]
async fn a_claim_clears_the_sentence_and_the_gate_together() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("qr-claim").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let node = bed.node(tenant, person).await;
    let state = bed.app_state().await;
    let task = card(&bed, tenant, user).await;
    let job = queued_build(&bed, tenant, user, ws, task).await;

    state
        .jobs
        .set_queued_reason(job, "waiting", Some(QueuedReason::AtCapacity))
        .await
        .expect("write");

    let claimed = state
        .jobs
        .claim_for_executor(job, node)
        .await
        .expect("claim")
        .expect("the queued job was claimed");
    assert_eq!(claimed.queued_reason, None);
    assert_eq!(
        claimed.queued_reason_kind, None,
        "a placed job still carrying a gate tells a client it is waiting on \
         something it is not"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn the_builds_listing_carries_the_sentence_and_the_gate() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("qr-list").await;
    let (user, _person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let task = card(&bed, tenant, user).await;
    let job = queued_build(&bed, tenant, user, ws, task).await;

    state
        .jobs
        .set_queued_reason(
            job,
            "waiting for node builder-2",
            Some(QueuedReason::PinnedNodeUnavailable {
                node_name: "builder-2".into(),
            }),
        )
        .await
        .expect("write");

    let rows = state
        .jobs
        .list_builds_for_workspace(tenant, user, ws, &nook_testkit::first_page(50))
        .await
        .expect("list")
        .rows;
    let row = rows
        .iter()
        .find(|r| r.id == job.0)
        .expect("the queued run is listed");
    assert_eq!(
        row.queued_reason.as_deref(),
        Some("waiting for node builder-2")
    );
    assert_eq!(
        row.queued_reason_kind,
        Some(QueuedReason::PinnedNodeUnavailable {
            node_name: "builder-2".into()
        }),
        "the panel's row must carry the gate, not just the run's state"
    );

    bed.teardown().await;
}

/// AC-6: a row from before the column existed — sentence written, gate NULL —
/// reads back with the sentence intact and no invented cause.
#[tokio::test]
async fn a_row_with_no_typed_gate_keeps_its_sentence() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("qr-legacy").await;
    let (user, _person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let task = card(&bed, tenant, user).await;
    let job = queued_build(&bed, tenant, user, ws, task).await;

    // Straight SQL, bypassing the repository: this is precisely the row shape
    // the repository can no longer produce.
    bed.db()
        .exec(
            "UPDATE loop_jobs SET queued_reason = $2 WHERE id = $1",
            params![job, "no eligible executor: you have no node online"],
        )
        .await
        .expect("legacy write");

    let row = state.jobs.reload(job).await.expect("reload");
    assert_eq!(
        row.queued_reason.as_deref(),
        Some("no eligible executor: you have no node online")
    );
    assert_eq!(row.queued_reason_kind, None);

    let rows = state
        .jobs
        .list_builds_for_workspace(tenant, user, ws, &nook_testkit::first_page(50))
        .await
        .expect("list")
        .rows;
    let listed = rows.iter().find(|r| r.id == job.0).expect("listed");
    assert_eq!(
        listed.queued_reason.as_deref(),
        Some("no eligible executor: you have no node online")
    );
    assert_eq!(listed.queued_reason_kind, None);

    bed.teardown().await;
}
