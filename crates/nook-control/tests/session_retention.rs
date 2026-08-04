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
//! Against a real database through `nook_testkit::TestBed`, because the thing
//! under test is a conditional DELETE and an in-memory fake would prove nothing
//! about the SQL that actually runs.

use nook_control::services::session_reaper;
use nook_testkit::TestBed;
use nook_types::TenantId;

/// A session that ended `days_ago`, or a live one when `ended` is None.
async fn session(
    bed: &TestBed,
    tenant: TenantId,
    node: nook_types::NodeId,
    status: &str,
    days_ago: Option<i64>,
) -> uuid::Uuid {
    let id = uuid::Uuid::now_v7();
    let ended = days_ago.map(|d| chrono::Utc::now() - chrono::Duration::days(d));
    sqlx::query(
        "INSERT INTO sessions (id, tenant_id, node_id, name, runtime, status, managed, created_at, updated_at, ended_at)
         VALUES ($1, $2, $3, 'test', 'bash', $4, false, now(), now(), $5)",
    )
    .bind(id)
    .bind(tenant.0)
    .bind(node.0)
    .bind(status)
    .bind(ended)
    .execute(&bed.pool)
    .await
    .expect("insert session");
    id
}

async fn alive(bed: &TestBed, id: uuid::Uuid) -> bool {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sessions WHERE id = $1")
        .bind(id)
        .fetch_one(&bed.pool)
        .await
        .expect("count")
        > 0
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
    let tenant = bed.tenant("reaper").await;
    let node = bed.node(tenant, uuid::Uuid::new_v4()).await;
    let state = bed.app_state().await;

    // 3 days old: inside the 7-day default, outside a 1-day setting.
    let three_days = session(&bed, tenant, node, "exited", Some(3)).await;
    session_reaper::reap_terminated(&state).await.expect("reap");
    assert!(alive(&bed, three_days).await);

    state
        .settings
        .put(nook_control::repo::admin::SettingWrite {
            tenant,
            scope: "tenant".to_string(),
            user: None,
            key: session_reaper::KEY.to_string(),
            value: serde_json::json!(1),
        })
        .await
        .expect("set retention");

    session_reaper::reap_terminated(&state).await.expect("reap");
    assert!(!alive(&bed, three_days).await);

    bed.teardown().await;
}

#[tokio::test]
async fn an_unusable_setting_keeps_the_default_rather_than_reaping_to_it() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("reaper").await;
    let node = bed.node(tenant, uuid::Uuid::new_v4()).await;
    let state = bed.app_state().await;

    // Zero would mean "delete on exit", and a garbled value must not be read as
    // "retain nothing" — the failure direction matters more than the parse.
    for bad in [
        serde_json::json!(0),
        serde_json::json!(-5),
        serde_json::json!("soon"),
    ] {
        state
            .settings
            .put(nook_control::repo::admin::SettingWrite {
                tenant,
                scope: "tenant".to_string(),
                user: None,
                key: session_reaper::KEY.to_string(),
                value: bad.clone(),
            })
            .await
            .expect("set retention");
        assert_eq!(
            session_reaper::retention_days(&*state.settings, tenant).await,
            session_reaper::DEFAULT_RETENTION_DAYS,
            "{bad} should fall back to the default"
        );
    }

    // …and a string holding a real number is still honoured, since a curl or a
    // hand-edited setting is as likely as the UI's number.
    state
        .settings
        .put(nook_control::repo::admin::SettingWrite {
            tenant,
            scope: "tenant".to_string(),
            user: None,
            key: session_reaper::KEY.to_string(),
            value: serde_json::json!("14"),
        })
        .await
        .expect("set retention");
    assert_eq!(
        session_reaper::retention_days(&*state.settings, tenant).await,
        14
    );

    bed.teardown().await;
}
