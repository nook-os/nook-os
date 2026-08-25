//! MAIN-438: the session expiry a `now()`-on-the-seam rewrite computes is still
//! the timestamp Postgres computed.
//!
//! `create_auth_session` used to write
//! `{now} + make_interval(hours => $4)`. Only the `now()` half went through the
//! dialect seam, which made the line *look* engine-neutral while
//! `make_interval(…)` — Postgres's named-argument call syntax — stayed behind;
//! SQLite reads its `=>` as an unfinished `>`. The whole expression now goes
//! through `TimeMath::now_plus_scaled`.
//!
//! This is the failure the card calls silent: both spellings run, and a wrong
//! one stores a plausible timestamp that logs nobody out until the arithmetic is
//! off by enough to notice. So the assertion is on the STORED value against a
//! bound computed in Rust, not on the SQL, and it runs on whichever engine the
//! bed is — the point is that the two agree.

use chrono::{DateTime, Duration, Utc};
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

/// A generous window: this asserts the arithmetic, not the clock. An hours/days
/// or seconds/hours mix-up is off by orders of magnitude and cannot hide inside
/// two minutes; scheduler jitter and a slow CI box fit comfortably within it.
const SLACK: i64 = 120;

async fn expiry_after(bed: &TestBed, ttl_hours: i32) -> DateTime<Utc> {
    let tenant = bed.tenant("ttl").await;
    let (user, _) = bed.user(tenant, "member").await;
    let session = AuthSessionId(Uuid::now_v7());
    bed.app_state()
        .await
        .identity
        .create_auth_session(session, user, tenant, ttl_hours)
        .await
        .expect("create session");
    bed.db()
        .query_scalar(
            "SELECT expires_at FROM sessions_auth WHERE id = $1",
            params![session],
        )
        .await
        .expect("read expires_at")
}

#[tokio::test]
async fn a_session_expires_the_configured_hours_from_now() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    for ttl in [1i32, 24, 720] {
        let before = Utc::now();
        let got = expiry_after(&bed, ttl).await;
        let want = before + Duration::hours(ttl as i64);
        let drift = (got - want).num_seconds().abs();
        assert!(
            drift <= SLACK,
            "ttl={ttl}h stored {got}, expected about {want} ({drift}s adrift) — \
             the interval arithmetic does not agree with the bound hours"
        );
    }
    bed.teardown().await;
}

/// The unit matters as much as the number: an expiry the seam computed in the
/// wrong unit still looks like a date. Two TTLs an exact factor apart must come
/// out an exact factor apart.
#[tokio::test]
async fn the_interval_unit_is_hours_not_something_else() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let base = Utc::now();
    let one = expiry_after(&bed, 1).await;
    let ten = expiry_after(&bed, 10).await;
    let ratio = (ten - base).num_seconds() as f64 / (one - base).num_seconds() as f64;
    assert!(
        (ratio - 10.0).abs() < 0.05,
        "10h/1h should be ~10x, got {ratio:.3} (1h -> {one}, 10h -> {ten})"
    );
    bed.teardown().await;
}
