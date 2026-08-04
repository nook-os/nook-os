//! Writing a setting twice must UPDATE, on both engines (MAIN-388).
//!
//! It did not on SQLite. The upsert's conflict target is
//! `(tenant_id, scope, user_id, key)`, and a tenant-scoped setting has
//! `user_id = NULL`. Postgres declares that constraint `NULLS NOT DISTINCT`, so
//! two NULLs collide and `DO UPDATE` fires; SQLite has no such modifier and
//! follows the SQL default where every NULL is distinct, so nothing ever
//! collided. Every write inserted a row and `tenant_value`'s unordered read
//! returned the FIRST one — so the oldest value won, permanently.
//!
//! It failed silently in the worst place: `loops.enabled`. `services::loops::set`
//! ends in `Ok(on)` — it returns its own argument, never a read-back — so the
//! CLI reported success while the stored value never moved.
//!
//! These run on both engines deliberately. A one-engine test is what let this
//! live: the behaviour is identical to read and differs only in the storage.

use nook_control::repo::admin::SettingWrite;
use nook_control::services::loops;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::{TenantId, UserId};

async fn put(bed: &TestBed, tenant: TenantId, user: Option<UserId>, value: serde_json::Value) {
    bed.app_state()
        .await
        .settings
        .put(SettingWrite {
            tenant,
            scope: if user.is_some() { "user" } else { "tenant" }.to_string(),
            user,
            key: "probe.key".to_string(),
            value,
        })
        .await
        .expect("put");
}

/// How many rows physically exist — the question the repository API cannot ask,
/// and the one that was wrong. A read alone would have looked fine on the first
/// write and only diverged on the second.
async fn row_count(bed: &TestBed, tenant: TenantId) -> i64 {
    let rows: Vec<(i64,)> = bed
        .db()
        .query_all(
            "SELECT count(*) FROM settings WHERE tenant_id = $1 AND key = 'probe.key'",
            params![tenant],
        )
        .await
        .expect("count");
    rows[0].0
}

#[tokio::test]
async fn rewriting_a_tenant_scoped_setting_updates_rather_than_duplicating() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("upsert").await;
    let state = bed.app_state().await;

    for v in [1, 2, 3] {
        put(&bed, tenant, None, serde_json::json!(v)).await;
    }

    assert_eq!(
        row_count(&bed, tenant).await,
        1,
        "three writes of one key must leave one row"
    );
    assert_eq!(
        state
            .settings
            .tenant_value(tenant, "probe.key")
            .await
            .expect("read"),
        Some(serde_json::json!(3)),
        "the newest value wins, not the first"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn rewriting_a_user_scoped_setting_still_updates() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("upsert").await;
    let (user, _) = bed.user(tenant, "owner").await;

    // These already worked — `user_id` is NOT NULL, so the key was never
    // ambiguous. The fix adds a second uniqueness constraint on SQLite that
    // these rows also violate, so this pins that the upsert still resolves
    // instead of erroring on the constraint it is not targeting.
    for v in ["a", "b"] {
        put(&bed, tenant, Some(user), serde_json::json!(v)).await;
    }

    assert_eq!(row_count(&bed, tenant).await, 1);

    bed.teardown().await;
}

#[tokio::test]
async fn a_tenant_and_user_setting_of_the_same_key_stay_separate() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("upsert").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let state = bed.app_state().await;

    // The NULL-free key must not collapse these into one another: `''` stands
    // in for "no user", and a real user id is never `''`.
    put(&bed, tenant, None, serde_json::json!("tenant-wide")).await;
    put(&bed, tenant, Some(user), serde_json::json!("mine")).await;

    assert_eq!(row_count(&bed, tenant).await, 2, "two scopes, two rows");
    assert_eq!(
        state
            .settings
            .tenant_value(tenant, "probe.key")
            .await
            .expect("read"),
        Some(serde_json::json!("tenant-wide")),
        "the user's value must not shadow the tenant's"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn the_loop_switch_can_be_turned_off_again() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;

    // The switch was write-once in BOTH directions, so both are pinned: a
    // tenant turned on that could never be turned off is a loop fleet an
    // operator cannot stop, and one seeded off could never be started.
    let a = bed.tenant("on-first").await;
    loops::set(&*state.settings, a, true).await.expect("on");
    assert!(loops::enabled(&*state.settings, a).await);
    loops::set(&*state.settings, a, false).await.expect("off");
    assert!(
        !loops::enabled(&*state.settings, a).await,
        "on then off must read off"
    );

    let b = bed.tenant("off-first").await;
    loops::set(&*state.settings, b, false).await.expect("off");
    assert!(!loops::enabled(&*state.settings, b).await);
    loops::set(&*state.settings, b, true).await.expect("on");
    assert!(
        loops::enabled(&*state.settings, b).await,
        "off then on must read on"
    );

    bed.teardown().await;
}
