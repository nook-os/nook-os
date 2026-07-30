//! One test harness for the NookOS integration suites (MAIN-156).
//!
//! Every suite used to rebuild the world by hand — seventeen copies of
//! `test_config()`, dozens of tenant-bootstrap blocks, an `AppState`
//! construction each — against a single shared, aged dev database that only ever
//! grew. [`TestBed`] instead gives each test its **own** database: it clones a
//! migrated + seeded template (built once per process) into a fresh, uniquely-
//! named database, hands back the pool + ids/contexts, and **drops the whole
//! database** at the end.
//!
//! ```ignore
//! let Some(mut bed) = TestBed::new().await else { return }; // skip without a DB server
//! let state = bed.app_state().await;
//! let tenant = bed.tenant("iso").await;
//! let (user, _person) = bed.user(tenant, "admin").await;
//! // … assertions against a private, freshly-seeded database …
//! bed.teardown().await; // (or let it drop — Drop drops the database on panic too)
//! ```
//!
//! ## Isolation model
//!
//! Private database per test is the **only** model (MAIN-166 retired the shared
//! path). Each `TestBed` owns its own database, so tests need **no**
//! scope-to-own-rows discipline and run in **parallel** without contending — the
//! unique database name per test is the isolation, and a test may migrate freely
//! (a new migration never touches the shared dev ledger). `NOOK_KEEP_TEST_DATA=1`
//! keeps the database around for debugging instead of dropping it. The shared
//! `DATABASE_URL` database now serves only the running dev stack, never tests.
//!
//! ## Engines (MAIN-242)
//!
//! The bed follows `DATABASE_URL`'s scheme, the same way boot does (MAIN-195):
//! `postgres://` gives a Postgres bed, `sqlite://` a SQLite one. Until this, the
//! harness constructed a `PgPool` directly, so SQLite could not be a *tested*
//! engine no matter how well the app supported it — the CI matrix and the
//! "first-class engine" claim had nothing underneath them.
//!
//! Two surfaces, and the difference matters:
//!
//! - [`TestBed::db`] is the **engine-agnostic** one — an [`EnginePool`], the same
//!   type production code takes. Anything written against it runs on either
//!   engine. New tests should use it.
//! - [`TestBed::pool`] is a **Postgres-only escape**, kept because ~650 existing
//!   call sites bind raw `sqlx` queries against it and converting them is the
//!   next unit of work, not this one (MAIN-242 NG-1). On a SQLite bed it is an
//!   inert handle that has never connected and never will; touching it fails,
//!   loudly and by design. Tests that use it are Postgres-leg tests.
//!
//! Gate on [`TestBed::engine`] (or [`TestBed::is_postgres`]) when a test is
//! Postgres-only for a real reason — querying `pg_database`, say.

use anyhow::{Context, Result};
use nook_control::state::AppState;
use nook_db::{params, Db, Engine, EnginePool};
use nook_infra::Config;
use nook_types::{NodeId, TenantId, UserId, WorkspaceId};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection, PgPool};
use std::path::{Path, PathBuf};
use tokio::sync::OnceCell;
use uuid::Uuid;

/// A migrated + seeded **template** database, built once per test process. Each
/// test then makes its private database with `CREATE DATABASE … TEMPLATE`, a
/// fast file-level copy, instead of re-running the whole migration set and seed
/// per test — which, once every test had its own database (MAIN-166), pushed the
/// CI Rust job past its time budget. `OnceCell` guarantees exactly one build even
/// as tests run in parallel; the rest wait for it and then only clone.
static TEMPLATE_DB: OnceCell<String> = OnceCell::const_new();

/// Build (once) and name the template database. It is migrated and seeded, then
/// its pool is closed — `CREATE DATABASE … TEMPLATE` refuses a template that has
/// any live session. Leaks one `nook_tmpl_<uuid>` per process; the dev reset
/// (`docker compose down -v`) and CI's ephemeral Postgres reclaim it.
async fn template_db(base_url: &str) -> &'static str {
    TEMPLATE_DB
        .get_or_init(|| async {
            let name = format!("nook_tmpl_{}", Uuid::now_v7().simple());
            let mut admin = PgConnection::connect(base_url)
                .await
                .expect("connect to the base database to create the template");
            sqlx::query(&format!("CREATE DATABASE \"{name}\""))
                .execute(&mut admin)
                .await
                .expect("create the template database");
            admin.close().await.ok();

            let pool = PgPool::connect(&swap_db(base_url, &name))
                .await
                .expect("connect to the template database");
            nook_control::MIGRATOR
                .run(&pool)
                .await
                .expect("migrate the template database");
            // `seed::run` takes the workspace pool type (`EnginePool`); wrap the
            // raw pool just for the seed. Fixtures + the field stay raw Postgres.
            nook_control::seed::run(&EnginePool::from_pg(pool.clone()), &Config::for_test())
                .await
                .expect("seed the template database");
            // Release every connection so the template can be cloned.
            pool.close().await;
            name
        })
        .await
}

/// What this bed created, and therefore what teardown has to undo. The two
/// engines have nothing in common here: Postgres owns a database on a server
/// reachable only through an admin connection, SQLite owns a file.
enum Arm {
    Pg {
        /// `DATABASE_URL` — the server + base database, used for the admin
        /// `CREATE`/`DROP DATABASE` statements (which cannot run against the
        /// target).
        base_url: String,
        /// The unique database this bed created (e.g. `nook_test_<uuid>`).
        db_name: String,
    },
    Sqlite {
        /// The unique file this bed created.
        path: PathBuf,
    },
}

/// A pool that has never opened a connection and never will, for the `pool`
/// field on a SQLite bed.
///
/// The field cannot simply be absent: it is `pub`, and ~650 call sites bind
/// `sqlx` queries against it, so on a SQLite bed it has to hold *something*.
/// What it holds is chosen for one property — it cannot reach a real server.
/// The host is a `.invalid` name, which RFC 2606 guarantees never resolves.
///
/// The tempting alternative, a lazy pool aimed at the real `DATABASE_URL`, is
/// the trap this exists to avoid: it would **work**, silently running an
/// allegedly-isolated test against the shared dev database.
///
/// **The failure is safe but not self-explanatory**, and that is worth knowing
/// rather than discovering. The hostname was picked hoping it would appear in
/// the error; it does not — sqlx reports connection failures without the
/// target, so what you actually get is:
///
/// ```text
/// error communicating with database: failed to lookup address information: Name or service not known
/// ```
///
/// (A Unix-socket path in place of the host was tried too, and is elided the
/// same way.) If you have landed here from that message: you used `bed.pool` on
/// a SQLite bed. Use [`TestBed::db`], or gate the test with
/// [`TestBed::is_postgres`].
fn inert_pg_pool() -> PgPool {
    PgPoolOptions::new()
        .connect_lazy("postgres://nobody@bed-pool-is-postgres-only-use-bed-db.invalid/none")
        .expect("a syntactically valid URL parses without connecting")
}

/// A prepared, **private** test world: a freshly created, migrated, and seeded
/// database plus opt-in setup surfaces, dropped whole at teardown.
pub struct TestBed {
    /// **Postgres-only escape.** The raw pool for this test's private database,
    /// for fixture SQL written directly against `sqlx`.
    ///
    /// On a **SQLite** bed this is [`inert_pg_pool`] — a handle that has never
    /// connected — so a test that uses it is a Postgres-leg test and fails
    /// rather than quietly doing something else. Use [`TestBed::db`] for
    /// anything that should run on both engines.
    pub pool: PgPool,
    /// The engine-agnostic pool, and the real one: on Postgres it wraps `pool`,
    /// on SQLite it is the only pool there is.
    db: EnginePool,
    /// What was created, and how to undo it.
    arm: Arm,
    /// `NOOK_KEEP_TEST_DATA=1` keeps the database for debugging.
    keep: bool,
    /// Set once the database has been dropped, so teardown + Drop don't double.
    dropped: bool,
}

impl TestBed {
    /// Create a fresh private database, migrate and seed it. `None` to skip when
    /// there is no `DATABASE_URL` (a silent skip; hard-fails under
    /// `NOOK_REQUIRE_DB`, so a missing DB in CI is a failure, not a false pass).
    pub async fn new() -> Option<TestBed> {
        let Ok(base_url) = std::env::var("DATABASE_URL") else {
            assert!(
                std::env::var("NOOK_REQUIRE_DB").is_err(),
                "NOOK_REQUIRE_DB is set but DATABASE_URL is not - these tests \
                 would have skipped silently and reported success"
            );
            return None;
        };
        let keep = std::env::var("NOOK_KEEP_TEST_DATA").is_ok();

        // The scheme picks the engine, exactly as boot does (MAIN-195/242). An
        // unparseable URL is a configuration error worth failing on: silently
        // skipping would report success for a suite that ran nothing.
        let engine = nook_db::engine_from_url(&base_url)
            .expect("DATABASE_URL must be postgres:// or sqlite://");
        Some(match engine {
            Engine::Postgres => Self::new_postgres(base_url, keep).await,
            Engine::Sqlite => Self::new_sqlite(keep).await,
        })
    }

    /// Postgres: clone the migrated + seeded template into a uniquely-named
    /// private database.
    async fn new_postgres(base_url: String, keep: bool) -> TestBed {
        let db_name = format!("nook_test_{}", Uuid::now_v7().simple());

        // Clone the migrated + seeded template rather than rebuilding it per test
        // (the CI-budget fix). `CREATE DATABASE … TEMPLATE` is a file copy, so the
        // fresh database arrives already migrated and seeded — no MIGRATOR, no
        // seed here.
        let template = template_db(&base_url).await;
        let mut admin = PgConnection::connect(&base_url)
            .await
            .expect("connect to the base database to create a test database");
        sqlx::query(&format!(
            "CREATE DATABASE \"{db_name}\" TEMPLATE \"{template}\""
        ))
        .execute(&mut admin)
        .await
        .expect("create the test database from the template");
        admin.close().await.ok();

        let pool = PgPool::connect(&swap_db(&base_url, &db_name))
            .await
            .expect("connect to the fresh test database");

        TestBed {
            db: EnginePool::from_pg(pool.clone()),
            pool,
            arm: Arm::Pg { base_url, db_name },
            keep,
            dropped: false,
        }
    }

    /// SQLite: a unique **file**, migrated through the SQLite track and seeded.
    ///
    /// A file rather than a shared-cache in-memory database, for three reasons.
    /// It is the shape a real single-machine deployment has, so the harness
    /// tests what ships. `NOOK_KEEP_TEST_DATA=1` means something — you can open
    /// the artifact in `sqlite3` afterwards, where an in-memory database dies
    /// with the process and the flag would be a lie. And teardown becomes
    /// *observable*: a test can assert the file is gone, which is how AC-6 is
    /// checked rather than assumed.
    ///
    /// No template-clone equivalent here. It would be a plain file copy and is
    /// tempting, but migrate + seed on an empty SQLite file is fast (the whole
    /// schema is one `0001`), and a second mechanism is only worth its
    /// complexity once something measures slow.
    async fn new_sqlite(keep: bool) -> TestBed {
        let path = std::env::temp_dir().join(format!("nook_test_{}.db", Uuid::now_v7().simple()));
        // A stale file from a previous run with the same name is impossible
        // (uuid v7), but an existing file would silently skip migration, so be
        // explicit rather than lucky.
        remove_sqlite_files(&path);

        let db = nook_db::connect(&format!("sqlite://{}", path.display()), 1)
            .await
            .expect("open the private SQLite test database");
        // The SQLite track, never the Postgres one (AC-3). Running the wrong
        // dialect's DDL would fail loudly here rather than produce a subtly
        // wrong schema, but selecting it explicitly is what makes that a
        // guarantee instead of an accident.
        nook_control::MIGRATOR_SQLITE
            .run(db.sqlite())
            .await
            .expect("migrate the private SQLite test database");
        nook_control::seed::run(&db, &Config::for_test())
            .await
            .expect("seed the private SQLite test database");

        TestBed {
            pool: inert_pg_pool(),
            db,
            arm: Arm::Sqlite { path },
            keep,
            dropped: false,
        }
    }

    /// Which engine this bed is on — for tests that must gate on it, such as
    /// ones that read `pg_database` or bind raw `sqlx` against [`TestBed::pool`].
    pub fn engine(&self) -> Engine {
        self.db.engine()
    }

    /// Shorthand for the common gate: `if !bed.is_postgres() { return; }`.
    pub fn is_postgres(&self) -> bool {
        self.engine() == Engine::Postgres
    }

    /// The canonical test [`Config`] — one construction, overridable per field
    /// (mutate the returned value). Wraps `Config::for_test()`.
    pub fn config(&self) -> Config {
        Config::for_test()
    }

    /// A fresh `AppState` on this bed's database with the canonical config.
    pub async fn app_state(&self) -> AppState {
        AppState::new(self.db(), self.config(), None).await
    }

    /// The workspace pool type ([`EnginePool`]) — for passing into production
    /// functions that take `&nook_db::DbPool`, and **the surface a test should
    /// use if it wants to run on both engines**. Cheap: an `EnginePool` is a
    /// clone of the underlying pool handle.
    pub fn db(&self) -> EnginePool {
        self.db.clone()
    }

    /// Create a tenant (name = slug = `test-<hint>-<uuid>`). No tracking needed —
    /// the whole database is dropped at teardown.
    pub async fn tenant(&self, hint: &str) -> TenantId {
        let id = TenantId::new();
        let name = format!("test-{hint}-{}", id.0.simple());
        self.db
            .exec(
                "INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $2)",
                params![id, name],
            )
            .await
            .expect("create tenant");
        id
    }

    /// Create a user in `tenant` with `role`. Returns `(user_id, person_id)` —
    /// the person id is what node ownership keys on.
    pub async fn user(&self, tenant: TenantId, role: &str) -> (UserId, Uuid) {
        let user = UserId::new();
        let person = Uuid::now_v7();
        self.db
            .exec(
                "INSERT INTO users (id, tenant_id, person_id, display_name, email, role)
                 VALUES ($1, $2, $3, 'U', $4, $5)",
                params![
                    user,
                    tenant,
                    person,
                    format!("u-{}@example.test", user.0.simple()),
                    role.to_string()
                ],
            )
            .await
            .expect("create user");
        (user, person)
    }

    /// Create an offline node in `tenant` owned by `owner` (a person id).
    pub async fn node(&self, tenant: TenantId, owner: Uuid) -> NodeId {
        let id = NodeId::new();
        self.db
            .exec(
                "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status, owner_person_id)
                 VALUES ($1, $2, $3, $4, 'offline', $5)",
                params![
                    id,
                    tenant,
                    format!("n-{}", id.0.simple()),
                    format!("h-{}", id.0.simple()),
                    owner
                ],
            )
            .await
            .expect("create node");
        id
    }

    /// Create a workspace in `tenant`.
    pub async fn workspace(&self, tenant: TenantId) -> WorkspaceId {
        let id = WorkspaceId::new();
        let name = format!("test-ws-{}", id.0.simple());
        self.db
            .exec(
                "INSERT INTO workspaces (id, tenant_id, name, slug) VALUES ($1, $2, $3, $3)",
                params![id, tenant, name],
            )
            .await
            .expect("create workspace");
        id
    }

    /// Force the keep-on-teardown flag (test-support for the teardown tests; in
    /// normal use it comes from `NOOK_KEEP_TEST_DATA`).
    pub fn set_keep(&mut self, keep: bool) {
        self.keep = keep;
    }

    /// This bed's private database: the database *name* on Postgres, the file
    /// *path* on SQLite — for teardown-behaviour tests, which are the only
    /// caller and know which engine they are on.
    pub fn db_name(&self) -> &str {
        match &self.arm {
            Arm::Pg { db_name, .. } => db_name,
            Arm::Sqlite { path } => path.to_str().unwrap_or_default(),
        }
    }

    /// Drop the whole private database (unless `NOOK_KEEP_TEST_DATA`). Idempotent.
    pub async fn teardown(&mut self) {
        if self.dropped || self.keep {
            self.dropped = true;
            return;
        }
        self.dropped = true;
        match &self.arm {
            Arm::Pg { base_url, db_name } => {
                self.pool.close().await;
                let _ = drop_database(base_url, db_name).await;
            }
            Arm::Sqlite { path } => {
                // Close first: on Windows an open handle would block the
                // unlink, and on unix it costs nothing to be tidy.
                self.db.sqlite().close().await;
                remove_sqlite_files(path);
            }
        }
    }
}

/// Remove a SQLite database and the sidecars sqlx leaves beside it. The `-wal`
/// and `-shm` files are not optional housekeeping: leaving them next to a
/// deleted database is how a later run finds a half-state, and they are the
/// difference between "the file is gone" and "the database is gone".
fn remove_sqlite_files(path: &Path) {
    let _ = std::fs::remove_file(path);
    for ext in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
    }
}

/// Hard per-test deadline (MAIN-185 AC-2). Wrap a DB-backed test body so a
/// hang fails THAT test in `secs` seconds with a pointed message, instead of
/// stalling the whole binary until the CI job's `timeout-minutes` cancels the
/// run — a failure mode indistinguishable from infrastructure flake, which is
/// exactly how the TestBed::Drop deadlock burned six PRs before being found.
pub async fn deadline<T>(secs: u64, fut: impl std::future::Future<Output = T>) -> T {
    match tokio::time::timeout(std::time::Duration::from_secs(secs), fut).await {
        Ok(v) => v,
        Err(_) => panic!(
            "test exceeded its {secs}s deadline — that is a hang, not slowness \
             (see MAIN-185); failing fast here instead of eating the CI job"
        ),
    }
}

/// Rewrite the database segment of a Postgres URL, preserving any query params.
fn swap_db(base: &str, db: &str) -> String {
    let (scheme, rest) = base.split_once("://").unwrap_or(("postgres", base));
    let (authority_and_db, params) = match rest.split_once('?') {
        Some((a, p)) => (a, Some(p)),
        None => (rest, None),
    };
    let authority = authority_and_db
        .rsplit_once('/')
        .map(|(a, _)| a)
        .unwrap_or(authority_and_db);
    let mut out = format!("{scheme}://{authority}/{db}");
    if let Some(p) = params {
        out.push('?');
        out.push_str(p);
    }
    out
}

/// Drop `db` from an admin connection to the base database. `WITH (FORCE)`
/// terminates any stragglers (Postgres 13+); the dev/CI Postgres is 16.
async fn drop_database(base_url: &str, db: &str) -> Result<()> {
    let mut admin = PgConnection::connect(base_url)
        .await
        .context("connect to drop the test database")?;
    sqlx::query(&format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)"))
        .execute(&mut admin)
        .await
        .context("drop the test database")?;
    admin.close().await.ok();
    Ok(())
}

impl Drop for TestBed {
    fn drop(&mut self) {
        // Safety net: if the test skipped `teardown().await` (or panicked before
        // it), drop the database anyway. Drop is sync, so the async work runs on
        // a throwaway current-thread runtime on a fresh OS thread.
        //
        // This thread must NEVER touch `self.pool` (MAIN-185): the pool's
        // connections are I/O objects registered with the TEST's runtime, and
        // the `join()` below has that runtime frozen — closing them from here
        // deadlocks both threads (gdb-verified; the hang that ate whole CI jobs).
        // `drop_database`'s `WITH (FORCE)` severs those connections server-side
        // instead, and the pool itself drops inertly on the test thread after.
        if self.dropped || self.keep {
            return;
        }
        self.dropped = true;
        match &self.arm {
            Arm::Pg { base_url, db_name } => {
                let base = base_url.clone();
                let name = db_name.clone();
                let _ = std::thread::spawn(move || {
                    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    else {
                        return;
                    };
                    rt.block_on(async {
                        let _ = drop_database(&base, &name).await;
                    });
                })
                .join();
            }
            // SQLite needs no runtime and no thread: unlinking is a sync
            // syscall. It also must not close the pool, for the same reason the
            // Postgres arm must not — see the note above. On unix the unlink
            // succeeds with the file still open, and the pool then drops
            // inertly on the test thread.
            Arm::Sqlite { path } => remove_sqlite_files(path),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wrapper itself must be proven to fire (MAIN-185 AC-2 test
    /// expectation): a deliberately-hung future fails with the deadline
    /// message. `start_paused` auto-advances time, so the 60s fires instantly.
    #[tokio::test(start_paused = true)]
    #[should_panic(expected = "exceeded its 60s deadline")]
    async fn deadline_flags_a_hung_future() {
        deadline(60, std::future::pending::<()>()).await;
    }

    #[tokio::test]
    async fn deadline_passes_a_prompt_future_through() {
        assert_eq!(deadline(60, async { 7 }).await, 7);
    }

    #[test]
    fn swap_db_rewrites_only_the_database_segment() {
        assert_eq!(
            swap_db("postgres://nook:nook@postgres:5432/nook", "nook_test_1"),
            "postgres://nook:nook@postgres:5432/nook_test_1"
        );
        assert_eq!(
            swap_db("postgres://u:p@h:5432/base?sslmode=disable", "t"),
            "postgres://u:p@h:5432/t?sslmode=disable"
        );
    }
}
