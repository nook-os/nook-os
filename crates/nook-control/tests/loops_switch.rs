//! The loops master switch (MAIN-239 AC-6).
//!
//! The load-bearing property: **off is quiet, and off loses nothing.** A job
//! queued while loops are disabled must still be sitting there, untouched and
//! placeable, when someone turns them on — otherwise "off" is not a switch, it
//! is a shredder.
//!
//! Each test owns a private database (MAIN-156 TestBed) and only its own rows.

use nook_control::services::{jobs, loops};
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use sqlx::PgPool;

/// A board + column + task to hang a job on.
async fn target(db: &PgPool, tenant: TenantId, creator: UserId) -> TaskId {
    let board = BoardId::new();
    sqlx::query(
        "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b',$3,'local')",
    )
    .bind(board)
    .bind(tenant)
    .bind(format!("L{}", &board.0.simple().to_string()[26..]).to_uppercase())
    .execute(db)
    .await
    .expect("board");
    let col = ColumnId::new();
    sqlx::query(
        "INSERT INTO board_columns (id, board_id, name, position, type)
         VALUES ($1,$2,'Triage',0,'unstarted')",
    )
    .bind(col)
    .bind(board)
    .execute(db)
    .await
    .expect("column");
    let id = TaskId::new();
    sqlx::query(
        "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, type, number, created_by)
         VALUES ($1,$2,$3,$4,'t','task',1,$5)",
    )
    .bind(id)
    .bind(tenant)
    .bind(board)
    .bind(col)
    .bind(creator)
    .execute(db)
    .await
    .expect("task");
    id
}

#[tokio::test]
async fn loops_default_to_off_and_the_switch_flips_at_runtime() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("loops").await;
    let db = bed.db();
    // The real settings repository over this bed — the same one AppState
    // builds, so this still drives production SQL (MAIN-305).
    let settings = nook_control::repo::admin::DbSettingRepository::new(db.clone());

    // The safe default, with nothing stored: a fresh deployment is quiet.
    assert!(!loops::enabled(&settings, tenant).await, "default is OFF");
    assert!(!loops::any_enabled(&settings).await);

    // No restart, no cache: the very next read sees it.
    loops::set(&settings, tenant, true).await.expect("enable");
    assert!(loops::enabled(&settings, tenant).await);
    assert!(loops::any_enabled(&settings).await);

    loops::set(&settings, tenant, false).await.expect("disable");
    assert!(!loops::enabled(&settings, tenant).await);
    assert!(!loops::any_enabled(&settings).await);

    bed.teardown().await;
}

/// One tenant's switch is not another's — and `any_enabled`, the cross-tenant
/// consumers' cheap gate, is the OR of them.
#[tokio::test]
async fn the_switch_is_per_tenant_and_any_enabled_is_their_or() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let a = bed.tenant("loops-a").await;
    let b = bed.tenant("loops-b").await;
    let db = bed.db();
    // The real settings repository over this bed — the same one AppState
    // builds, so this still drives production SQL (MAIN-305).
    let settings = nook_control::repo::admin::DbSettingRepository::new(db.clone());

    loops::set(&settings, a, true).await.expect("enable a");
    assert!(loops::enabled(&settings, a).await);
    assert!(!loops::enabled(&settings, b).await, "b was never enabled");
    assert!(
        loops::any_enabled(&settings).await,
        "one tenant running is enough to make a pass worthwhile"
    );

    loops::set(&settings, a, false).await.expect("disable a");
    assert!(!loops::any_enabled(&settings).await, "nobody left running");

    bed.teardown().await;
}

/// A `user`-scoped row of the same key is somebody's personal preference and
/// must never gate the fleet.
#[tokio::test]
async fn a_user_scoped_row_cannot_turn_the_fleet_on() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("loops").await;
    let (user, _p) = bed.user(tenant, "member").await;

    sqlx::query(
        "INSERT INTO settings (id, tenant_id, scope, user_id, key, value)
         VALUES ($1, $2, 'user', $3, $4, 'true'::jsonb)",
    )
    .bind(SettingId::new())
    .bind(tenant)
    .bind(user)
    .bind(loops::KEY)
    .execute(&bed.pool)
    .await
    .expect("user-scoped setting");

    let db = bed.db();
    // The real settings repository over this bed — the same one AppState
    // builds, so this still drives production SQL (MAIN-305).
    let settings = nook_control::repo::admin::DbSettingRepository::new(db.clone());
    assert!(
        !loops::enabled(&settings, tenant).await,
        "tenant scope only"
    );
    assert!(!loops::any_enabled(&settings).await, "tenant scope only");

    bed.teardown().await;
}

/// **The load-bearing one (AC-3/AC-6).** A job created while loops are off
/// stays exactly where it was — queued, unplaced, unfailed — and becomes
/// placeable the moment the switch flips. Off must not consume work.
#[tokio::test]
async fn a_job_queued_while_off_waits_and_runs_after_enable() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("loops").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let task = target(&bed.pool, tenant, user).await;
    let state = bed.app_state().await;
    let db = bed.db();
    // The real settings repository over this bed — the same one AppState
    // builds, so this still drives production SQL (MAIN-305).
    let settings = nook_control::repo::admin::DbSettingRepository::new(db.clone());

    // Loops are off (the default). Creating a job still works — the switch
    // gates the machinery, not the board.
    let job = jobs::create(
        &state,
        tenant,
        user,
        CreateLoopJobRequest {
            kind: "spec".into(),
            target_task_id: task.to_string(),
            seed: None,
        },
    )
    .await
    .expect("a job can be created with loops off")
    .job;
    assert_eq!(job.state, "queued");

    // With loops off the consumer would not even reach this job. Assert the
    // gate it consults, and that the job is untouched.
    assert!(!loops::any_enabled(&settings).await);
    let after: LoopJob = bed
        .pool
        .query_one("SELECT * FROM loop_jobs WHERE id = $1", params![job.id])
        .await
        .expect("the job still exists");
    assert_eq!(after.state, "queued", "off did not consume the work");
    assert_eq!(after.executor_node_id, None, "and placed nothing");

    // Now give it somewhere to run and turn loops on.
    let node = bed.node(tenant, person).await;
    sqlx::query(
        "UPDATE nodes SET status = 'online',
             capabilities = '{\"runtime_auth\":[{\"runtime\":\"claude\",\"state\":\"authorized\"}]}'::jsonb
         WHERE id = $1",
    )
    .bind(node)
    .execute(&bed.pool)
    .await
    .expect("an eligible executor");
    loops::set(&settings, tenant, true).await.expect("enable");
    assert!(loops::any_enabled(&settings).await);

    // The same placement the consumer performs, now that the gate is open.
    let placed = jobs::select_executor(&state, tenant, job.id)
        .await
        .expect("placement");
    assert_eq!(placed.state, "claimed", "the waiting job ran on enable");
    assert_eq!(placed.executor_node_id, Some(node));

    bed.teardown().await;
}
