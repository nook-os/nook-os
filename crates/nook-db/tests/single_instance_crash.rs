//! A lock left behind by a killed process is reclaimable (MAIN-197 AC-3).
//!
//! This is the one property that genuinely needs a second OS process. The unit
//! tests next to the module cover refusal and clean release in-process — fine,
//! because `flock` is per open-file-description, so two `open`s conflict even
//! from one process — but "the lock dies with the process" is a kernel
//! guarantee, and the only honest way to test a kernel guarantee is to let the
//! kernel do it.
//!
//! It matters because it is exactly the property a plausible wrong
//! implementation lacks. A pidfile, a `.lock` file whose *existence* means
//! locked, a row in a table — each passes "second instance refused" and each
//! turns a crash into a control plane that will not restart until someone finds
//! the stale file. That failure would surface in production, at the worst
//! moment, on the engine chosen for being simple.
//!
//! The child is this very test binary, re-run with a filter that selects the
//! `#[ignore]`d holder below. No fixture binary to build or keep in sync.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use nook_db::single_instance::{acquire_for_file, lock_path_for, LockError};

const DB_ENV: &str = "NOOK_TEST_LOCK_DB";
const HOLDER: &str = "holds_the_lock_until_killed";

/// Not a test. Spawned by the test below to be a second process that holds the
/// lock and then dies badly. `#[ignore]` keeps it out of normal runs; it is
/// selected explicitly by name.
#[test]
#[ignore = "helper process for a_lock_left_by_a_killed_process_is_reclaimable"]
fn holds_the_lock_until_killed() {
    let db = std::env::var(DB_ENV).expect("the parent passes the database path");
    let _lock = acquire_for_file(Path::new(&db)).expect("the child takes the lock");
    // Tell the parent the lock is actually held. Without this the parent races
    // the child's startup and could see "not locked" and call it a pass.
    println!("HELD");
    // Long enough that the parent always kills us first; short enough that a
    // stray child cannot outlive the suite by much.
    std::thread::sleep(std::time::Duration::from_secs(120));
}

fn scratch() -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "nook-lock-crash-{}.db",
        uuid::Uuid::now_v7().simple()
    ));
    let _ = std::fs::remove_file(lock_path_for(&p));
    p
}

#[test]
fn a_lock_left_by_a_killed_process_is_reclaimable() {
    let db = scratch();

    let mut child = Command::new(std::env::current_exe().expect("this test binary"))
        .args(["--exact", HOLDER, "--ignored", "--nocapture"])
        .env(DB_ENV, &db)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the holder process");

    // Block until the child says it holds the lock. If it died instead, stdout
    // closes and we get EOF rather than hanging forever.
    let mut out = BufReader::new(child.stdout.take().expect("piped stdout"));
    let mut held = false;
    let mut line = String::new();
    while out.read_line(&mut line).unwrap_or(0) > 0 {
        if line.contains("HELD") {
            held = true;
            break;
        }
        line.clear();
    }
    assert!(held, "the child never reported holding the lock");

    // With a live holder, this process is refused — the same contract the unit
    // test asserts, re-checked across a real process boundary so the crash half
    // below is testing a lock that was genuinely someone else's.
    match acquire_for_file(&db) {
        Err(LockError::AlreadyRunning { .. }) => {}
        Err(e) => panic!("wrong refusal while the child holds it: {e}"),
        Ok(_) => panic!("acquired a lock another process holds"),
    }

    // SIGKILL: no destructor, no unwinding, no chance to clean up anything.
    // Whatever survives this is what a crashed control plane would leave.
    child.kill().expect("kill the holder");
    child.wait().expect("reap the holder");

    // The kernel dropped the lock when the descriptor closed, so the next boot
    // gets in — no stale-file sweep, no operator intervention.
    let reclaimed = acquire_for_file(&db).expect("a killed holder's lock is reclaimable");
    drop(reclaimed);

    // And the sidecar is still there, deliberately: deleting it is the race the
    // module refuses to introduce.
    assert!(
        lock_path_for(&db).exists(),
        "the sidecar should outlive the lock, not be unlinked"
    );

    let _ = std::fs::remove_file(lock_path_for(&db));
    let _ = std::fs::remove_file(&db);
}
