//! The terminated-session reaper.
//!
//! Nothing ever removed session rows. A prod tenant reached 58 with 7 live, and
//! every listing showed all 58 — so the useful rows were outnumbered eight to
//! one by records of things nobody can read again, tmux having died with them.
//!
//! These tests pin the contract that makes deleting them safe: only `exited` and
//! `error` are reapable, `detached` is LIVE and must survive (tmux still holds
//! it and a browser can reattach), the window is per tenant and honoured per
//! tenant, and an unusable setting keeps the default instead of reaping to it.
//!
//! Against a real database, because the thing under test is a conditional DELETE
//! whose date arithmetic is written per engine — an in-memory fake would prove
//! nothing about the SQL that actually runs, and the two dialects' `now() - N
//! days` are different expressions that both have to be right.
//!
//! Fixture SQL goes through `bed.db()`, not `bed.pool`: the latter is a Postgres
//! handle that is deliberately inert on a SQLite bed, so using it would confine
//! these tests to one engine — the one where a dialect bug cannot show up.

use nook_control::repo::admin::SettingWrite;
use nook_control::services::session_reaper;
use nook_db::dialect::{time_math, type_mapping};
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::TenantId;

/// A session that ended `days_ago`, or one with no end time when `None`.
async fn session(
    bed: &TestBed,
    tenant: TenantId,
    node: nook_types::NodeId,
    status: &str,
    days_ago: Option<i64>,
) -> uuid::Uuid {
    let id = uuid::Uuid::now_v7();
    let now = type_mapping(bed.engine()).now();
    // `NULL` is spliced rather than bound because the alternative is binding a
    // parameter the other branch does not have, and the two engines disagree
    // about how many placeholders a statement may leave unused.
    let ended = match days_ago {
        Some(_) => time_math(bed.engine()).now_minus_scaled("$6", "1 day"),
        None => "NULL".to_string(),
    };
    let sql = format!(
        "INSERT INTO sessions
           (id, tenant_id, node_id, name, runtime, status, managed, created_at, updated_at, ended_at)
         VALUES ($1, $2, $3, 'test', 'bash', $4, $5, {now}, {now}, {ended})"
    );
    let res = match days_ago {
        Some(d) => {
            bed.db()
                .exec(&sql, params![id, tenant, node, status, false, d])
                .await
        }
        None => {
            bed.db()
                .exec(&sql, params![id, tenant, node, status, false])
                .await
        }
    };
    res.expect("insert session");
    id
}

async fn alive(bed: &TestBed, id: uuid::Uuid) -> bool {
    let rows: Vec<(i64,)> = bed
        .db()
        .query_all("SELECT count(*) FROM sessions WHERE id = $1", params![id])
        .await
        .expect("count");
    rows[0].0 > 0
}

/// Set a tenant's retention window.
///
/// **One write per tenant, deliberately.** On SQLite the settings upsert does
/// not collapse tenant-scoped rows — its `UNIQUE` treats the NULL `user_id` as
/// distinct where Postgres declares `NULLS NOT DISTINCT` — so a second `put` of
/// the same key inserts a row and the FIRST value keeps winning. Overwriting
/// here would test that bug instead of this reaper, so each value gets its own
/// tenant. Tracked as MAIN-388; delete this note when that lands.
async fn set_retention(bed: &TestBed, tenant: TenantId, value: serde_json::Value) {
    bed.app_state()
        .await
        .settings
        .put(SettingWrite {
            tenant,
            scope: "tenant".to_string(),
            user: None,
            key: session_reaper::KEY.to_string(),
            value,
        })
        .await
        .expect("set retention");
}

#[tokio::test]
async fn reaps_only_terminated_sessions_past_the_window() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("reaper").await;
    let node = bed.node(tenant, uuid::Uuid::new_v4()).await;
    let state = bed.app_state().await;

    let old_exited = session(&bed, tenant, node, "exited", Some(30)).await;
    let old_error = session(&bed, tenant, node, "error", Some(30)).await;
    // The one this card exists to protect. `detached` reads as dead and is not:
    // tmux is still holding it, and reaping it would destroy live work whose
    // only crime was having nobody watching.
    let old_detached = session(&bed, tenant, node, "detached", Some(30)).await;
    let old_running = session(&bed, tenant, node, "running", Some(30)).await;
    let fresh_exited = session(&bed, tenant, node, "exited", Some(1)).await;

    session_reaper::reap_terminated(&state).await.expect("reap");
    assert!(!alive(&bed, old_exited).await, "aged exited should be gone");
    assert!(!alive(&bed, old_error).await, "aged error should be gone");
    assert!(
        alive(&bed, old_detached).await,
        "detached is LIVE — reattachable"
    );
    assert!(alive(&bed, old_running).await, "running is live");
    assert!(
        alive(&bed, fresh_exited).await,
        "inside the window, so it stays"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn a_session_with_no_end_time_is_left_alone() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("reaper").await;
    let node = bed.node(tenant, uuid::Uuid::new_v4()).await;
    let state = bed.app_state().await;

    // A terminal status with no `ended_at` has no age. Comparing NULL would
    // silently never match anyway; this pins that it stays rather than being
    // reaped on some invented timestamp.
    let no_end = session(&bed, tenant, node, "exited", None).await;
    session_reaper::reap_terminated(&state).await.expect("reap");

    assert!(
        alive(&bed, no_end).await,
        "no end time means no age to exceed"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn the_window_is_per_tenant() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;

    // Same age, same statuses, different windows: one sweep must apply each
    // tenant's own policy rather than one number to everybody.
    let patient = bed.tenant("patient").await;
    let hasty = bed.tenant("hasty").await;
    let patient_node = bed.node(patient, uuid::Uuid::new_v4()).await;
    let hasty_node = bed.node(hasty, uuid::Uuid::new_v4()).await;

    // 3 days old: inside the 7-day default, outside a 1-day setting.
    let kept = session(&bed, patient, patient_node, "exited", Some(3)).await;
    let reaped = session(&bed, hasty, hasty_node, "exited", Some(3)).await;
    set_retention(&bed, hasty, serde_json::json!(1)).await;

    session_reaper::reap_terminated(&state).await.expect("reap");

    assert!(
        alive(&bed, kept).await,
        "3 days is inside the 7-day default"
    );
    assert!(
        !alive(&bed, reaped).await,
        "3 days is outside a 1-day window"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn an_unusable_setting_keeps_the_default_rather_than_reaping_to_it() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;

    // Zero would mean "delete on exit", and a garbled value must not be read as
    // "retain nothing" — the failure direction matters more than the parse.
    for (i, bad) in [
        serde_json::json!(0),
        serde_json::json!(-5),
        serde_json::json!("soon"),
    ]
    .into_iter()
    .enumerate()
    {
        let tenant = bed.tenant(&format!("bad{i}")).await;
        set_retention(&bed, tenant, bad.clone()).await;
        assert_eq!(
            session_reaper::retention_days(&*state.settings, tenant).await,
            session_reaper::DEFAULT_RETENTION_DAYS,
            "{bad} should fall back to the default"
        );
    }

    // …and a string holding a real number is still honoured, since a curl or a
    // hand-edited setting is as likely as the UI's number.
    let tenant = bed.tenant("stringy").await;
    set_retention(&bed, tenant, serde_json::json!("14")).await;
    assert_eq!(
        session_reaper::retention_days(&*state.settings, tenant).await,
        14
    );

    bed.teardown().await;
}
