//! A force-quit of the desktop app does not leave a control plane behind
//! (MAIN-400 AC-1/AC-2).
//!
//! AC-2 says a crash must not prevent the next launch, and MAIN-197 chose
//! `flock(2)` so that the *lock* half is free — the kernel releases it on
//! SIGKILL, which `nook-db`'s `single_instance_crash.rs` proves. That is only
//! half the launch, though. The process a desktop install actually kills is the
//! SHELL, and the control plane it started is a separate process the kernel has
//! no reason to touch: it keeps running, keeps the lock, and the next launch is
//! refused by the very guard AC-4 relies on. One force-quit and the app is
//! broken until someone finds a stray pid.
//!
//! So the property under test is not the lock. It is that the control plane
//! **stops when the process that started it does**, and the lock is how that is
//! observed: it is exactly the resource whose release is what AC-2 asks for, and
//! reading it needs no signal, no pid arithmetic and no `/proc`.
//!
//! The shell cannot be here — it is outside the workspace and needs a display —
//! so a `sh` stands in for it. What matters is only that it is a real parent
//! that dies without warning, which SIGKILL on the shim gives us.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nook_db::single_instance::{acquire_for_file, lock_path_for, LockError};
use nook_desktop_env::{control_plane_env, load_or_create_secrets, EXIT_WITH_PARENT};

/// A first-launch app-data directory that removes itself.
struct Install(PathBuf);

impl Install {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "nook-desktop-life-{tag}-{}",
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&dir).expect("an app-data directory");
        Install(dir)
    }

    fn db(&self) -> PathBuf {
        self.0.join("nook.db")
    }
}

impl Drop for Install {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("bind an ephemeral port")
        .local_addr()
        .expect("read the chosen port")
        .port()
}

/// The stand-in shell, and the control plane it started.
///
/// Killing the shim leaves the control plane orphaned, which is the whole
/// point; the recorded pid is how the negative case below cleans up after
/// itself, since there nothing else will.
struct Shim {
    shell: Child,
    pid_file: PathBuf,
}

impl Drop for Shim {
    fn drop(&mut self) {
        let _ = self.shell.kill();
        let _ = self.shell.wait();
        if let Some(pid) = self.control_plane_pid() {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
        }
    }
}

impl Shim {
    /// Start `nook-control serve` under an intermediate process, as the app
    /// starts it under the shell.
    ///
    /// `& wait` rather than a bare command: every `sh` worth the name `exec`s a
    /// lone simple command, which would make the control plane a child of THIS
    /// process and leave nothing in the middle to kill.
    fn start(install: &Install, watchdog: bool) -> Self {
        let secrets = load_or_create_secrets(&install.0).expect("first-launch secrets");
        let pid_file = install.0.join("control-plane.pid");
        let mut env = control_plane_env(&install.db(), free_port(), free_port(), &secrets);
        if !watchdog {
            env.retain(|(name, _)| name != EXIT_WITH_PARENT);
        }

        let shell = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "\"$0\" serve & echo $! > {}; wait",
                pid_file.display()
            ))
            .arg(env!("CARGO_BIN_EXE_nook-control"))
            // The suite runs inside the dev container, where a real
            // `DATABASE_URL` is exported and `dotenvy` would find the repo's
            // `.env`. A desktop install has neither.
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .envs(env)
            .current_dir(&install.0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start the stand-in shell");

        Shim { shell, pid_file }
    }

    fn control_plane_pid(&self) -> Option<u32> {
        std::fs::read_to_string(&self.pid_file)
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// SIGKILL, so nothing in the shim gets to tidy up — the force-quit AC-2
    /// names.
    fn force_quit(&mut self) {
        self.shell.kill().expect("kill the stand-in shell");
        self.shell.wait().expect("reap the stand-in shell");
    }
}

/// Whether the database's single-instance lock is free right now.
///
/// Taken and released, because taking it is the only way to ask. Any refusal
/// other than "already running" is a defect in the test's setup rather than an
/// answer, so it fails loudly instead of reading as "still held".
fn lock_is_free(db: &std::path::Path) -> bool {
    match acquire_for_file(db) {
        Ok(_held) => true,
        Err(LockError::AlreadyRunning { .. }) => false,
        Err(e) => panic!("the lock could not be probed: {e}"),
    }
}

fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    done()
}

/// AC-1/AC-2: kill the app outright and the control plane it started goes with
/// it, so the next launch is not refused by the single-instance guard.
#[test]
fn a_force_quit_does_not_strand_the_control_plane() {
    let install = Install::new("orphan");
    let db = install.db();
    let mut shim = Shim::start(&install, true);

    // Wait for the real thing to have taken the lock. Without this the kill
    // could land before the child ever locked anything, and "the lock is free"
    // afterwards would mean nothing at all.
    assert!(
        wait_until(Duration::from_secs(60), || !lock_is_free(&db)),
        "the control plane never took the single-instance lock"
    );

    shim.force_quit();

    assert!(
        wait_until(Duration::from_secs(20), || lock_is_free(&db)),
        "the control plane outlived the app that started it and still holds \
         {} — the next launch is refused by the single-instance guard",
        lock_path_for(&db).display()
    );
}

/// AC-4: a second copy of the app is turned away from the first one's database,
/// and told why.
///
/// Two shells choose two ports, so nothing collides there; the database file is
/// the one thing they would share, and SQLite is single-writer. What must not
/// happen is the two of them interleaving writes into it. `single_instance_boot`
/// asserts the guard from a bare `DATABASE_URL`; this asserts it in the
/// configuration a desktop install actually has — the app-data path, the
/// desktop boot map — and that the first copy is left alone.
#[test]
fn a_second_copy_of_the_app_is_refused_the_first_ones_database() {
    let install = Install::new("second-copy");
    let db = install.db();
    let _first = Shim::start(&install, true);

    assert!(
        wait_until(Duration::from_secs(60), || !lock_is_free(&db)),
        "the first copy never took the single-instance lock"
    );

    let secrets = load_or_create_secrets(&install.0).expect("the same install's secrets");
    let second = Command::new(env!("CARGO_BIN_EXE_nook-control"))
        .arg("serve")
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .envs(control_plane_env(&db, free_port(), free_port(), &secrets))
        .current_dir(&install.0)
        .output()
        .expect("run a second copy");

    assert!(!second.status.success(), "the second copy must not serve");
    let err = String::from_utf8_lossy(&second.stderr);
    // The shell quotes this verbatim, so it is what the person reads.
    assert!(
        err.contains("already running"),
        "the refusal must be the single-instance one:\n{err}"
    );
    assert!(
        err.contains("single-writer"),
        "the refusal must name the reason:\n{err}"
    );
    assert!(
        err.contains(&db.display().to_string()),
        "the refusal must name the database:\n{err}"
    );

    // And the first copy is untouched — a refusal that took the first one down
    // with it would be a worse outcome than the race it prevents.
    assert!(
        !lock_is_free(&db),
        "the first copy must still hold its database"
    );
}

/// …and it is the shell's `NOOK_EXIT_WITH_PARENT` that does it.
///
/// Without this, the test above passes just as well against a control plane
/// that died of something else entirely — a missing secret, a port collision —
/// which would leave AC-2 resting on a coincidence. Here the same process is
/// started with the one variable removed and is expected to keep running,
/// which is both the negative control and the pre-MAIN-400 behaviour.
#[test]
fn without_the_watchdog_the_orphan_survives() {
    let install = Install::new("survives");
    let db = install.db();
    let mut shim = Shim::start(&install, false);

    assert!(
        wait_until(Duration::from_secs(60), || !lock_is_free(&db)),
        "the control plane never took the single-instance lock"
    );

    shim.force_quit();

    // Several watchdog intervals' worth: long enough that an opted-in process
    // would certainly have gone, short enough not to pad the suite.
    std::thread::sleep(Duration::from_secs(4));
    assert!(
        !lock_is_free(&db),
        "an opted-OUT control plane stopped anyway, so the test above proves \
         nothing about the watchdog"
    );
    // `Shim`'s drop kills it by the pid it recorded. Nothing else would.
}
