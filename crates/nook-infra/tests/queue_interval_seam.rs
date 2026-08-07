//! MAIN-444: the queue's interval arithmetic, after moving off
//! `make_interval(secs => $n)` onto `TimeMath::now_plus_scaled`.
//!
//! Two failures hide here, and neither shows up as a failing test on its own —
//! which is why this file asserts on STORED TIMESTAMPS rather than on a suite
//! that goes green.
//!
//! - **`not_before` too far out** and enqueued work is invisible: the consumer
//!   polls an empty queue and the job simply never runs.
//! - **`locked_until` too short** and a claimed row becomes visible again while
//!   its consumer is still working, so the same message is delivered twice.
//!   The queue's contract is at-least-once, so a duplicate is legal — which is
//!   exactly why nothing fails loudly, and why the unit has to be pinned.
//!
//! A seconds/hours mix-up is off by 3600×, far outside the slack below. The
//! assertions run on whichever engine the `TestBed` is; the point is that the
//! two agree.

use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, Utc};
use nook_db::{params, Db};
use nook_infra::queue::database::DbQueue;
use nook_infra::queue::{NewWork, Queue};
use nook_testkit::TestBed;
use uuid::Uuid;

/// Generous on purpose: this asserts the ARITHMETIC, not the clock. A wrong
/// unit is off by orders of magnitude and cannot hide inside two minutes, while
/// scheduler jitter and a slow CI box fit comfortably within it.
const SLACK: i64 = 120;

fn queue(bed: &TestBed) -> DbQueue {
    DbQueue::new(bed.db())
}

/// `not_before` for a freshly enqueued item.
async fn not_before_of(bed: &TestBed, id: Uuid) -> DateTime<Utc> {
    bed.db()
        .query_scalar(
            "SELECT not_before FROM work_queue WHERE id = $1",
            params![id],
        )
        .await
        .expect("not_before")
}

/// `locked_until` for a claimed item.
async fn locked_until_of(bed: &TestBed, id: Uuid) -> Option<DateTime<Utc>> {
    bed.db()
        .query_scalar_opt(
            "SELECT locked_until FROM work_queue WHERE id = $1",
            params![id],
        )
        .await
        .expect("locked_until")
}

fn about(actual: DateTime<Utc>, expected: DateTime<Utc>, what: &str) {
    let drift = (actual - expected).num_seconds().abs();
    assert!(
        drift <= SLACK,
        "{what}: stored {actual}, expected about {expected} — {drift}s out, \
         which is past the {SLACK}s slack. A wrong interval UNIT looks exactly \
         like this."
    );
}

/// A delayed enqueue must become visible one delay from now — in SECONDS.
///
/// This is the assertion that catches the unit: `600` meaning ten minutes, not
/// ten hours and not ten seconds.
#[tokio::test]
async fn a_delayed_enqueue_is_visible_after_exactly_that_many_seconds() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("m444delay").await;
    let before = Utc::now();

    let id = queue(&bed)
        .enqueue(
            NewWork::new(tenant.0, "m444.delayed", b"{}".to_vec())
                .delay(StdDuration::from_secs(600)),
        )
        .await
        .expect("enqueue");

    about(
        not_before_of(&bed, id).await,
        before + Duration::seconds(600),
        "not_before after a 600s delay",
    );

    bed.teardown().await;
}

/// No delay means visible NOW, not never.
///
/// This is the `coalesce(…, {now})` path, and it is worth its own test because
/// the two engines reach it differently: on Postgres `NULL * interval` is NULL,
/// while on SQLite `printf` renders the NULL as empty and `' seconds'` is not a
/// valid modifier, so `datetime()` returns NULL. Same answer, different route —
/// and a change that "fixed" the NULL handling on one engine could silently
/// push `not_before` to never on the other.
#[tokio::test]
async fn an_undelayed_enqueue_is_visible_immediately() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("m444nodelay").await;
    let before = Utc::now();

    let id = queue(&bed)
        .enqueue(NewWork::new(tenant.0, "m444.now", b"{}".to_vec()))
        .await
        .expect("enqueue");

    about(
        not_before_of(&bed, id).await,
        before,
        "not_before with no delay",
    );

    bed.teardown().await;
}

/// Claiming a message hides it for exactly the visibility timeout, in seconds.
///
/// The dangerous direction is SHORT: a `locked_until` that expires early hands
/// the same work to a second consumer while the first is still running it.
#[tokio::test]
async fn receiving_hides_a_message_for_exactly_the_visibility_timeout() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("m444lock").await;
    let q = queue(&bed);
    let id = q
        .enqueue(NewWork::new(tenant.0, "m444.lock", b"{}".to_vec()))
        .await
        .expect("enqueue");

    let before = Utc::now();
    let got = q
        .receive(&["m444.lock".to_string()], 10, StdDuration::from_secs(900))
        .await
        .expect("receive");
    assert_eq!(got.len(), 1, "the enqueued message was not delivered");
    assert_eq!(got[0].id, id);

    about(
        locked_until_of(&bed, id).await.expect("locked_until set"),
        before + Duration::seconds(900),
        "locked_until after a 900s visibility timeout",
    );

    bed.teardown().await;
}

/// Extending visibility uses the same unit as claiming it.
#[tokio::test]
async fn extending_visibility_uses_the_same_unit() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("m444ext").await;
    let q = queue(&bed);
    let id = q
        .enqueue(NewWork::new(tenant.0, "m444.ext", b"{}".to_vec()))
        .await
        .expect("enqueue");
    q.receive(&["m444.ext".to_string()], 10, StdDuration::from_secs(30))
        .await
        .expect("receive");

    let before = Utc::now();
    q.extend_visibility(id, StdDuration::from_secs(1800))
        .await
        .expect("extend");

    about(
        locked_until_of(&bed, id).await.expect("locked_until set"),
        before + Duration::seconds(1800),
        "locked_until after extending by 1800s",
    );

    bed.teardown().await;
}

/// The type filter still filters — and its absence still matches everything.
///
/// This pins the OTHER half of MAIN-444: the filter used to be one statement
/// with `({cast} IS NULL OR work_type = ANY($1))` and is now built, so both
/// arms are new SQL and both need proving. A filter that quietly matched
/// everything would let a consumer claim work meant for another.
#[tokio::test]
async fn the_type_filter_selects_and_its_absence_matches_everything() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("m444filter").await;
    let q = queue(&bed);
    let wanted = q
        .enqueue(NewWork::new(tenant.0, "m444.wanted", b"{}".to_vec()))
        .await
        .expect("enqueue wanted");
    q.enqueue(NewWork::new(tenant.0, "m444.other", b"{}".to_vec()))
        .await
        .expect("enqueue other");

    // With a filter: only the named type, never the sibling.
    let got = q
        .receive(&["m444.wanted".to_string()], 10, StdDuration::from_secs(60))
        .await
        .expect("filtered receive");
    assert_eq!(
        got.len(),
        1,
        "the filter returned {} rows, not 1",
        got.len()
    );
    assert_eq!(got[0].id, wanted);
    assert_eq!(got[0].work_type, "m444.wanted");

    // With no filter: the sibling is still there to be had.
    let rest = q
        .receive(&[], 10, StdDuration::from_secs(60))
        .await
        .expect("unfiltered receive");
    assert!(
        rest.iter().any(|w| w.work_type == "m444.other"),
        "an empty filter failed to match everything — it returned {:?}",
        rest.iter().map(|w| &w.work_type).collect::<Vec<_>>()
    );

    bed.teardown().await;
}
