//! The SQLite bed must survive a test holding two connections (MAIN-295).
//!
//! A pool of one is a deadlock waiting for a caller: hold a connection — a
//! transaction, a `fetch` still streaming — and ask for a second, and the
//! second waits for the first, which waits for the caller, until the acquire
//! times out as `PoolTimedOut`. The Postgres bed never hits this because its
//! pool is not one connection wide.
//!
//! No test in the tree does this *today*, which is why the trap is worth a test
//! rather than a comment: the next person to open a transaction and read
//! through the pool inside it would have found it the hard way, and the error
//! names the pool rather than the cause.

use nook_testkit::TestBed;

/// Two live connections at once, which a pool of one cannot serve.
#[tokio::test]
async fn a_bed_serves_a_second_connection_while_the_first_is_held() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    if bed.is_postgres() {
        // The Postgres arm is out of scope (NG-1) and was never affected.
        bed.teardown().await;
        return;
    }

    let pool = bed.db().sqlite().clone();

    // Held for the whole test, exactly as an open transaction would be.
    let first = pool
        .acquire()
        .await
        .expect("the first connection is always available");

    // The assertion: this must not wait for `first` to be returned. With
    // `max_connections(1)` it waits the full acquire timeout and fails.
    let second = tokio::time::timeout(std::time::Duration::from_secs(5), pool.acquire())
        .await
        .expect("acquiring a second connection must not block until the timeout")
        .expect("the second connection is granted");

    drop(second);
    drop(first);
    bed.teardown().await;
}

/// Concurrent readers, which is the shape a test actually takes: a query while
/// another is still in flight.
#[tokio::test]
async fn concurrent_reads_do_not_serialise_into_a_timeout() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    if bed.is_postgres() {
        bed.teardown().await;
        return;
    }
    let pool = bed.db().sqlite().clone();

    let mut set = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let p = pool.clone();
        set.spawn(async move {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM tenants")
                .fetch_one(&p)
                .await
        });
    }
    while let Some(joined) = set.join_next().await {
        joined
            .expect("the task itself must not panic")
            .expect("a concurrent read must not time out on the pool");
    }
    bed.teardown().await;
}

/// A read while a WRITE is open.
///
/// Kept because it is the shape a test most easily falls into — read something
/// back while a transaction is still open — and it must not wait out
/// `busy_timeout`. It does NOT distinguish journal modes: under the rollback
/// journal an uncommitted writer holds only a RESERVED lock, so readers proceed
/// there too. That is exactly why WAL is not set (see `sqlite_bed_pool`).
#[tokio::test]
async fn a_read_is_not_blocked_by_an_open_write() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    if bed.is_postgres() {
        bed.teardown().await;
        return;
    }
    let pool = bed.db().sqlite().clone();

    let mut tx = pool.begin().await.expect("begin a write transaction");
    sqlx::query("INSERT INTO tenants (id, name, slug) VALUES ('w1', 'W', 'w')")
        .execute(&mut *tx)
        .await
        .expect("write inside the transaction");

    // Held open on purpose: the write is uncommitted while this read runs.
    let read = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM tenants").fetch_one(&pool),
    )
    .await
    .expect("a reader must not wait out busy_timeout behind an open writer")
    .expect("the read itself succeeds");
    assert!(read >= 0, "the snapshot is readable, whatever it counts");

    tx.rollback().await.expect("rollback");
    bed.teardown().await;
}
