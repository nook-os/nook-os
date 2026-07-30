//! The control plane's *boot* refuses a second instance (MAIN-197 AC-1/AC-4).
//!
//! `nook-db`'s own tests prove the lock works. They cannot prove `main.rs` takes
//! it — and a lock nothing acquires is the failure mode that passes every unit
//! test on both sides while enforcing nothing. So this runs the real binary
//! (`CARGO_BIN_EXE_nook-control`, built by cargo for this test) and reads its
//! exit status and stderr.
//!
//! Two shortcuts, both load-bearing:
//!
//! The test process holds the lock itself rather than booting a first control
//! plane. A second server would need ports, an IdP, secrets and a clean
//! shutdown to prove one thing about the first fifteen lines of `main`; holding
//! the same lock the same way puts the binary in the identical situation for
//! none of that cost.
//!
//! And the binary is run as `seed`, not `serve`, because `serve` never returns
//! — the first draft of this test deadlocked waiting for a healthy control
//! plane to exit. The lock is taken before the subcommand is dispatched, so
//! `seed` exercises exactly the same code and then terminates.

use std::path::PathBuf;
use std::process::Command;

use nook_db::single_instance::{acquire_for_file, lock_path_for};

fn scratch(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "nook-boot-lock-{tag}-{}.db",
        uuid::Uuid::now_v7().simple()
    ));
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(lock_path_for(&p));
    p
}

/// Run the control plane with a scrubbed environment plus exactly what boot
/// needs. Scrubbed because the suite runs inside the dev container, where a
/// real `DATABASE_URL` is already exported and would otherwise win.
fn boot(database_url: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_nook-control"))
        .arg("seed")
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("DATABASE_URL", database_url)
        // 32+ chars, which config validation enforces before boot reaches the lock.
        .env("SESSION_SECRET", "main197-single-instance-lock-test-secret")
        // Never listened on under `seed`, but config still wants them parseable.
        .env("BIND_ADDR", "127.0.0.1:0")
        .env("NOOK_AGENT_BIND", "127.0.0.1:0")
        .output()
        .expect("run the control-plane binary")
}

#[test]
fn a_second_control_plane_on_the_same_sqlite_file_refuses_to_boot() {
    let db = scratch("refuse");
    // Stand in for the running first instance.
    let held = acquire_for_file(&db).expect("the test holds the lock");

    let out = boot(&format!("sqlite://{}", db.display()));
    let err = String::from_utf8_lossy(&out.stderr);

    assert!(
        !out.status.success(),
        "the second instance must not boot; stderr:\n{err}"
    );
    // The rule, so the operator learns why one is the limit…
    assert!(
        err.contains("single-writer"),
        "the refusal must name the rule:\n{err}"
    );
    // …and the file, so they can find the process holding it.
    assert!(
        err.contains(&db.display().to_string()),
        "the refusal must name the database file:\n{err}"
    );

    // It refused at the LOCK, not somewhere later that happens to also fail.
    // Without this the test would pass just as well against a binary that
    // crashed on a missing secret, which is not the contract.
    assert!(
        err.contains("already running"),
        "the refusal must be the single-instance one:\n{err}"
    );

    drop(held);
    let _ = std::fs::remove_file(lock_path_for(&db));
    let _ = std::fs::remove_file(&db);
}

/// The other half: with nothing holding the lock, boot gets *past* it. Without
/// this, deleting the whole SQLite arm of the lock would still pass the test
/// above — everything refuses when the reason is "always refuse".
#[test]
fn boot_gets_past_the_lock_when_nobody_holds_it() {
    let db = scratch("free");

    let out = boot(&format!("sqlite://{}", db.display()));
    let err = String::from_utf8_lossy(&out.stderr);
    // The refusal is an anyhow error on stderr; the log line is tracing, which
    // this binary sends to stdout. Two streams, so read both rather than
    // assert an absence against the one that could never have contained it.
    let log = String::from_utf8_lossy(&out.stdout);

    assert!(
        !err.contains("already running"),
        "an unheld database must not be refused:\n{err}"
    );
    // Proof it really reached the lock and took it, rather than dying earlier:
    // the sidecar exists, and the log line names it.
    assert!(
        lock_path_for(&db).exists(),
        "boot should have created the lock sidecar"
    );
    assert!(
        log.contains("holding the SQLite single-instance lock"),
        "boot should log the lock it holds:\nstdout:\n{log}\nstderr:\n{err}"
    );

    let _ = std::fs::remove_file(lock_path_for(&db));
    let _ = std::fs::remove_file(&db);
}

/// AC-2/NG-2: the Postgres path takes no lock, so multi-instance is untouched.
/// Asserted from the boot log rather than from the module, because the claim
/// under test is about what `main` does.
///
/// Pointed at a database name that does not exist on the real server. Two
/// alternatives were worse: a reachable URL would make this test seed a live
/// database as a side effect, and an unreachable *host* costs sqlx's full 30s
/// acquire timeout (measured) for an answer we get instantly this way. Failing
/// at the connect step is the proof — boot reached it without taking a lock.
#[test]
fn a_postgres_boot_takes_no_lock() {
    let Some(url) = absent_database_on_the_real_server() else {
        return; // no Postgres configured here; the nook-db unit test still holds
    };

    let out = boot(&url);
    let err = String::from_utf8_lossy(&out.stderr);
    let log = String::from_utf8_lossy(&out.stdout);

    assert!(
        !log.contains("single-instance lock") && !err.contains("already running"),
        "postgres must take no lock:\nstdout:\n{log}\nstderr:\n{err}"
    );
    // It really did reach the connect step rather than falling over earlier,
    // which is what makes the absence above mean something.
    assert!(
        err.contains("opening the database"),
        "expected a connect failure, so we know the lock stage was passed:\n{err}"
    );
}

/// The configured Postgres URL with its database name replaced by one that is
/// not there, so connecting fails at once and writes nothing.
fn absent_database_on_the_real_server() -> Option<String> {
    let url = std::env::var("DATABASE_URL").ok()?;
    if !url.starts_with("postgres") {
        return None;
    }
    let (base, query) = match url.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (url.as_str(), None),
    };
    let (prefix, _db) = base.rsplit_once('/')?;
    let mut out = format!("{prefix}/nook_main197_absent");
    if let Some(q) = query {
        out.push('?');
        out.push_str(q);
    }
    Some(out)
}
