//! One control plane per SQLite database file (MAIN-197).
//!
//! SQLite is single-writer, and the epic's decided limit (MAIN-189 NG-3) is one
//! control-plane instance per file. Two instances against the same file race,
//! and the way they lose is quiet: interleaved writes, a corrupted page, a
//! database that reads fine until the day it doesn't. Nothing in the URL, the
//! process list, or the logs would say what happened. So the rule is enforced
//! where it can still be refused — at boot, before a pool exists.
//!
//! Postgres is untouched (AC-2). Multi-instance is the whole point there, and
//! the engine already says which case we are in, so no flag is needed (NG-3).
//!
//! **An OS advisory lock, deliberately, not a pidfile.** A pidfile has to be
//! written, read back, parsed, and reasoned about — and it survives the process
//! that wrote it, so a crash leaves a file claiming a lock nobody holds. Every
//! pidfile scheme then grows the same follow-up: is that pid alive, and is it
//! *ours*, and what if the number was reused. `flock(2)` has none of that. The
//! kernel owns the lock, it is released when the descriptor closes — including
//! on exit, including on SIGKILL, including on a power-loss reboot — so AC-3's
//! "a crash must not permanently brick restart" is a property of the mechanism
//! rather than a cleanup path we have to get right.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use crate::{engine_from_url, Engine};

/// Why a boot-time single-instance lock could not be taken.
#[derive(Debug)]
pub enum LockError {
    /// Another process holds the lock for this database file.
    AlreadyRunning { db: PathBuf, lock: PathBuf },
    /// The lock file itself could not be opened (bad directory, permissions).
    Io { lock: PathBuf, source: io::Error },
    /// No advisory-lock implementation on this platform.
    Unsupported,
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Names the rule and the file, per AC-1/AC-4 — an operator reading
            // this at 3am should not have to go and find out why one is a limit.
            LockError::AlreadyRunning { db, lock } => write!(
                f,
                "another nook control plane is already running against this SQLite \
                 database: {}\n\
                 SQLite is single-writer, so exactly one control-plane instance may \
                 use a database file at a time — a second one would race it and can \
                 corrupt the file. Stop the running instance, point this one at a \
                 different file, or use postgres:// to run more than one instance.\n\
                 (lock held on {})",
                db.display(),
                lock.display()
            ),
            LockError::Io { lock, source } => write!(
                f,
                "could not open the single-instance lock file {}: {source}",
                lock.display()
            ),
            LockError::Unsupported => write!(
                f,
                "sqlite:// needs an advisory file lock to enforce one control plane \
                 per database file, and this platform has no implementation — use \
                 postgres://, which has no such limit"
            ),
        }
    }
}

impl std::error::Error for LockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LockError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// A held single-instance lock.
///
/// **Keep this alive for as long as the process serves.** The lock lives on the
/// open descriptor, so dropping the guard releases it — which is exactly what
/// makes clean shutdown work (AC-3) and exactly what makes `let _ = acquire(..)`
/// a silent no-op. Bind it to a name.
#[derive(Debug)]
pub struct InstanceLock {
    /// Held for its `Drop`; the descriptor closing is the release.
    _file: File,
    lock_path: PathBuf,
}

impl InstanceLock {
    /// The lock file being held — for logging, mostly.
    pub fn path(&self) -> &Path {
        &self.lock_path
    }
}

/// Take the single-instance lock implied by `url`, if the engine needs one.
///
/// `Ok(None)` means no lock was needed and none was taken: Postgres (AC-2), or
/// an in-memory SQLite database, which no other process can reach anyway.
///
/// Call this **before** opening the pool. The whole value is refusing prior to
/// the first write, and `create_if_missing` means merely connecting is already
/// a change to the filesystem.
pub fn acquire_for_url(url: &str) -> Result<Option<InstanceLock>, LockError> {
    // An unparseable URL is not this module's error to report — `connect` gives
    // a far better message a moment later. Take no lock and let it.
    if engine_from_url(url).ok() != Some(Engine::Sqlite) {
        return Ok(None);
    }
    match sqlite_file(url) {
        Some(db) => acquire_for_file(&db).map(Some),
        None => Ok(None),
    }
}

/// Lock the sidecar for one database file. Split out so tests can drive it
/// without constructing URLs.
pub fn acquire_for_file(db: &Path) -> Result<InstanceLock, LockError> {
    let db = canonical_ish(db);
    let lock_path = lock_path_for(&db);

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|source| LockError::Io {
            lock: lock_path.clone(),
            source,
        })?;

    match try_lock_exclusive(&file) {
        Ok(true) => Ok(InstanceLock {
            _file: file,
            lock_path,
        }),
        Ok(false) => Err(LockError::AlreadyRunning {
            db,
            lock: lock_path,
        }),
        Err(Some(source)) => Err(LockError::Io {
            lock: lock_path,
            source,
        }),
        Err(None) => Err(LockError::Unsupported),
    }
}

/// The sidecar path for a database file: `<db>.lock`.
///
/// A sidecar rather than the database file itself, because SQLite does its own
/// locking on that file and the two schemes are not guaranteed to be
/// independent on every platform. Borrowing the file SQLite is already using
/// for a second, unrelated purpose is how you get a bug that only appears on
/// one OS.
///
/// **The sidecar is never deleted, and that is not an oversight.** Unlinking it
/// on shutdown would introduce the race the lock exists to prevent: while B
/// holds a lock on the inode, A unlinks the name and C creates a fresh file at
/// the same path and locks *that* — two holders, no conflict, because they are
/// locking different inodes. An empty zero-byte file left behind is the cheaper
/// half of that trade by a wide margin.
pub fn lock_path_for(db: &Path) -> PathBuf {
    let mut s = db.as_os_str().to_os_string();
    s.push(".lock");
    PathBuf::from(s)
}

/// The database file a `sqlite://` URL names, or `None` when it names no file.
///
/// Parsed by sqlx rather than by hand, so the file we lock is by construction
/// the file sqlx will open — a second parser here could disagree with the first
/// and guard the wrong path, which fails *open*. The same reasoning is why the
/// in-memory cases below are recognised from what sqlx *reports* rather than by
/// re-scanning the URL for `mode=memory`.
///
/// Nothing to lock means nothing to protect: an in-memory database is private
/// to the process that opened it, so a second instance is not a hazard. sqlx
/// spells that three ways — a generated `file:sqlx-in-memory-N` for the literal
/// `:memory:` forms, an empty name for `?mode=memory`, and the bare `:memory:`
/// default. All three are covered by tests, so a change in sqlx shows up as a
/// red test rather than as a stray `.lock` file in the working directory.
fn sqlite_file(url: &str) -> Option<PathBuf> {
    use std::str::FromStr;
    let opts = sqlx::sqlite::SqliteConnectOptions::from_str(url).ok()?;
    let name = opts.get_filename().to_path_buf();
    let s = name.to_string_lossy();
    if s.is_empty() || s == ":memory:" || s.starts_with("file:sqlx-in-memory-") {
        return None;
    }
    Some(name)
}

/// Resolve `.././db` and symlinks so two spellings of one file take one lock.
///
/// The file usually does not exist yet — `create_if_missing` is the point — so
/// canonicalize the parent and re-attach the name. Best effort: if the parent
/// cannot be resolved either, the raw path still locks correctly against an
/// identical spelling, which is the common case. Failing the boot here would
/// trade a real guarantee for a hypothetical one.
fn canonical_ish(db: &Path) -> PathBuf {
    if let Ok(real) = db.canonicalize() {
        return real;
    }
    match (db.parent(), db.file_name()) {
        (Some(parent), Some(name)) => {
            let parent = if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            };
            match parent.canonicalize() {
                Ok(p) => p.join(name),
                Err(_) => db.to_path_buf(),
            }
        }
        _ => db.to_path_buf(),
    }
}

/// `Ok(true)` locked, `Ok(false)` someone else holds it, `Err(Some)` a real I/O
/// failure, `Err(None)` unsupported platform.
#[cfg(unix)]
fn try_lock_exclusive(file: &File) -> Result<bool, Option<io::Error>> {
    use std::os::unix::io::AsRawFd;
    // Non-blocking: the answer we want is "no", immediately and with a message,
    // not a boot that hangs until the other instance happens to stop.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        // EWOULDBLOCK and EAGAIN are the same number on Linux and different on
        // some other unixes; match both rather than assume.
        Some(e) if e == libc::EWOULDBLOCK || e == libc::EAGAIN => Ok(false),
        _ => Err(Some(err)),
    }
}

#[cfg(not(unix))]
fn try_lock_exclusive(_file: &File) -> Result<bool, Option<io::Error>> {
    // Refuse rather than silently skip. A no-op here would let two instances
    // boot on a platform where nothing stops them, which is the corruption this
    // module exists to prevent — wearing a green checkmark.
    Err(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "nook-lock-{tag}-{}.db",
            uuid::Uuid::now_v7().simple()
        ));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(lock_path_for(&p));
        p
    }

    /// AC-2 + NG-2: Postgres takes no lock, so multi-instance stays possible.
    #[test]
    fn postgres_takes_no_lock() {
        for url in [
            "postgres://nook:nook@localhost/nook",
            "postgresql://nook@db:5432/nook",
        ] {
            let held = acquire_for_url(url).expect("postgres never fails to lock");
            assert!(held.is_none(), "{url} must take no lock");
        }
    }

    /// A URL we cannot parse is `connect`'s error to report, with a much better
    /// message. Refusing here first would replace it with a worse one.
    #[test]
    fn an_unknown_scheme_is_left_for_connect_to_reject() {
        assert!(acquire_for_url("mysql://host/db").unwrap().is_none());
    }

    /// In-memory databases are private to the process. Both spellings, because
    /// sqlx represents them differently and only one is the obvious `:memory:`.
    #[test]
    fn in_memory_sqlite_takes_no_lock_and_leaves_no_file() {
        for url in [
            "sqlite::memory:",
            "sqlite://:memory:",
            "sqlite://?mode=memory",
            "sqlite://?mode=memory&cache=shared",
        ] {
            let held = acquire_for_url(url).expect("in-memory never fails");
            assert!(held.is_none(), "{url} must take no lock");
        }
        // …and no sidecar was dropped in the working directory. Both spellings
        // of the bug leave litter with a different name — `file:sqlx-in-memory-N.lock`
        // for the `:memory:` sentinel, and a bare `.lock` for `?mode=memory`,
        // whose filename sqlx reports as empty. An earlier draft of this module
        // handled only the first and produced exactly that bare `.lock` in the
        // crate root, so check for both.
        for junk in std::fs::read_dir(".").unwrap().flatten() {
            let name = junk.file_name();
            let name = name.to_string_lossy();
            assert!(
                !name.starts_with("file:sqlx-in-memory-") && name != ".lock",
                "left a stray lock file: {name}"
            );
        }
    }

    /// AC-1/AC-4, the contract: a second instance on the same file is refused,
    /// and the refusal names the rule and the file.
    #[test]
    fn a_second_instance_on_the_same_file_is_refused() {
        let db = scratch("second");
        let first = acquire_for_file(&db).expect("first instance locks");

        let err = acquire_for_file(&db).expect_err("second instance must be refused");
        assert!(
            matches!(err, LockError::AlreadyRunning { .. }),
            "wrong error: {err}"
        );

        let msg = err.to_string();
        // The rule…
        assert!(
            msg.contains("single-writer") && msg.contains("one control-plane instance"),
            "message must name the rule: {msg}"
        );
        // …and the file. Compared against the canonical form, since that is
        // what an operator needs in order to find the other process.
        assert!(
            msg.contains(&canonical_ish(&db).display().to_string()),
            "message must name the database file: {msg}"
        );
        // …and a way out that is not "give up".
        assert!(msg.contains("postgres://"), "message must offer a way out");

        drop(first);
        let _ = std::fs::remove_file(&db);
        let _ = std::fs::remove_file(lock_path_for(&db));
    }

    /// AC-3, clean shutdown: releasing lets the next instance in.
    #[test]
    fn a_released_lock_is_immediately_reusable() {
        let db = scratch("release");
        let first = acquire_for_file(&db).expect("first locks");
        drop(first);

        let second = acquire_for_file(&db).expect("a released lock is reusable");
        drop(second);
        let _ = std::fs::remove_file(lock_path_for(&db));
    }

    /// Different files do not contend — the lock is per-database, not global.
    /// Getting this wrong would turn "one instance per file" into "one instance",
    /// which no test asserting only the refusal would notice.
    #[test]
    fn two_different_files_lock_independently() {
        let a = scratch("indep-a");
        let b = scratch("indep-b");
        let la = acquire_for_file(&a).expect("a locks");
        let lb = acquire_for_file(&b).expect("b locks independently of a");
        drop((la, lb));
        for p in [a, b] {
            let _ = std::fs::remove_file(lock_path_for(&p));
        }
    }

    /// Two spellings of one file are one lock. `./x.db` and `x.db` naming the
    /// same database while both instances boot happily is the exact hole this
    /// closes, and it is invisible to a test that only ever uses one spelling.
    #[test]
    fn two_spellings_of_the_same_file_are_one_lock() {
        let db = scratch("spelling");
        let dir = db.parent().unwrap().to_path_buf();
        let name = db.file_name().unwrap();
        // …/tmp/./<name> — a different string, the same file.
        let indirect = dir.join(".").join(name);

        let first = acquire_for_file(&db).expect("first locks");
        let err = acquire_for_file(&indirect).expect_err("the same file by another name");
        assert!(matches!(err, LockError::AlreadyRunning { .. }), "{err}");

        drop(first);
        let _ = std::fs::remove_file(lock_path_for(&db));
    }
}
