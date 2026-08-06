//! MAIN-156: `TestBed` gives each test a private database and drops the whole
//! thing at teardown; `NOOK_KEEP_TEST_DATA` keeps it for debugging. Proven here
//! against the catalogue — the database itself vanishes, not just its rows.
//! Needs a **Postgres** `DATABASE_URL`; skips cleanly without one, and skips on
//! a SQLite bed because this asserts the Postgres arm specifically (MAIN-242
//! AC-4). The SQLite half of the same contract — teardown removes the file,
//! `keep` preserves it — lives in `tests/dual_engine.rs`.
//!
//! Asserted through `nook_db::test_support` since MAIN-429, so this file no
//! longer names a `sqlx` type and has left the signature guard's allow-list.
//! `exists()` reads the catalogue directly and shares no code path with
//! `destroy()` (AC-4), which is what makes it a real check rather than the same
//! bug agreeing with itself.

use nook_db::test_support::{self as lifecycle, Provisioned};
use nook_testkit::TestBed;

/// The bed's database, as the lifecycle module addresses it.
fn provisioned(bed: &TestBed) -> Provisioned {
    Provisioned::Pg {
        base_url: std::env::var("DATABASE_URL").expect("DATABASE_URL (TestBed::new gave a bed)"),
        db_name: bed.db_name().to_string(),
    }
}

#[tokio::test]
async fn teardown_drops_the_private_database() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping teardown test — no DATABASE_URL");
        return;
    };
    if !bed.is_postgres() {
        bed.teardown().await;
        return; // dual_engine.rs owns the SQLite twin of this test
    }
    let what = provisioned(&bed);
    // Put a row in it, so "gone" means the whole database, not an empty one.
    let _tenant = bed.tenant("teardown").await;
    assert!(
        lifecycle::exists(&what).await,
        "the private database exists while the bed is live"
    );

    bed.teardown().await;
    assert!(
        !lifecycle::exists(&what).await,
        "teardown drops the whole private database — created data vanishes with it"
    );
}

#[tokio::test]
async fn keep_preserves_the_database() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    if !bed.is_postgres() {
        bed.teardown().await;
        return; // dual_engine.rs owns the SQLite twin of this test
    }
    // The flag normally comes from NOOK_KEEP_TEST_DATA; set it directly here so
    // this test doesn't race the process-global env var with its sibling.
    bed.set_keep(true);
    let what = provisioned(&bed);
    let _tenant = bed.tenant("keep").await;

    bed.teardown().await; // a no-op under keep
    assert!(
        lifecycle::exists(&what).await,
        "keep leaves the database in place for debugging"
    );

    // Don't leak the kept database: the Drop guard also honours keep and won't
    // remove it, so drop it by hand. `destroy`'s `WITH (FORCE)` terminates the
    // bed's own connections server-side, which is the same mechanism the Drop
    // guard has always relied on.
    lifecycle::destroy(&what).await;
}
