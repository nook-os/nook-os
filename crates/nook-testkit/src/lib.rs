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
//! harness constructed a raw Postgres pool directly, so SQLite could not be a
//! *tested* engine no matter how well the app supported it — the CI matrix and
//! the "first-class engine" claim had nothing underneath them.
//!
//! Two surfaces, and the difference matters:
//!
//! - [`TestBed::db`] is the **engine-agnostic** one — an [`EnginePool`], the same
//!   type production code takes. Anything written against it runs on either
//!   engine. New tests should use it.
//!
//! There is no second surface. The raw-Postgres-pool escape hatch that stood
//! beside `db` through the conversion is gone (MAIN-268): every test now
//! reaches its database engine-agnostically, and the guard in
//! `scripts/check-sqlx-signatures.sh` stops a new one appearing. A test that
//! genuinely needs raw Postgres — multi-statement SQL, or `pg_database` —
//! takes its own connection from [`TestBed::database_url`] and says why.
//!
//! Gate on [`TestBed::engine`] (or [`TestBed::is_postgres`]) when a test is
//! Postgres-only for a real reason — querying `pg_database`, say.

use nook_control::state::AppState;
use nook_db::test_support::{self as lifecycle, AdminConn, Provisioned};
use nook_db::{params, Db, Engine, EnginePool};
use nook_infra::Config;
use nook_types::{NodeId, TenantId, UserId, WorkspaceId};
use tokio::sync::OnceCell;
use uuid::Uuid;

/// A migrated + seeded **template** database, built once per test process. Each
/// test then makes its private database with `CREATE DATABASE … TEMPLATE`, a
/// fast file-level copy, instead of re-running the whole migration set and seed
/// per test — which, once every test had its own database (MAIN-166), pushed the
/// CI Rust job past its time budget. `OnceCell` guarantees exactly one build even
/// as tests run in parallel; the rest wait for it and then only clone.
static TEMPLATE_DB: OnceCell<String> = OnceCell::const_new();

/// One global advisory lock for every template operation.
///
/// Building and reaping both mutate the shared set of templates, and two
/// processes doing either at once is how a half-built template gets cloned or a
/// live one gets dropped. Serialising them costs a moment at startup, once.
const TEMPLATE_LOCK: i64 = 0x6E6F6F6B_74706C64u64 as i64; // "nook_tpld"

/// How long a template of a DIFFERENT fingerprint may sit unused before it is
/// reclaimed. Generous on purpose: a concurrently-running suite on another
/// branch has its own fingerprint, and reaping it out from under that process
/// would be worse than leaving a few hundred megabytes for an hour.
const TEMPLATE_MAX_AGE_SECS: u64 = 60 * 60;

/// A short, stable fingerprint of the schema a template would contain.
///
/// Derived from the migration set's versions and checksums, so any change to
/// the schema yields a different template and no run can inherit a stale one.
/// This is what makes templates REUSABLE across processes, which is the fix for
/// the leak: the name is a pure function of the content, so the second process
/// finds the first one's template instead of building another.
fn template_fingerprint() -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a
    let mut eat = |bytes: &[u8]| {
        for b in bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
    };
    for m in nook_control::MIGRATOR.iter() {
        eat(&m.version.to_le_bytes());
        eat(&m.checksum);
    }
    format!("{h:016x}")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Find, or build, the template for this schema.
///
/// Named `nook_tmpl_<fingerprint>_<epoch>`: the fingerprint makes it findable by
/// another process, and the epoch lets the reaper judge age from the catalogue
/// without opening a connection (a connection would itself look like "in use").
///
/// It no longer leaks. The previous version created a uniquely-named template
/// per PROCESS and documented the leak as acceptable because a dev reset would
/// reclaim it — which held for CI and not for a long-lived dev database: a
/// working session accumulated 1,589 of them (16 GB), exhausted the container's
/// shared memory, and crashed Postgres into a three-minute recovery.
async fn template_db(base_url: &str) -> &'static str {
    TEMPLATE_DB
        .get_or_init(|| async {
            let fp = template_fingerprint();
            let prefix = format!("nook_tmpl_{fp}_");

            let mut admin = AdminConn::connect(base_url)
                .await
                .expect("connect to the base database to manage templates");

            // Everything below mutates the shared template set.
            admin
                .advisory_lock(TEMPLATE_LOCK)
                .await
                .expect("take the template lock");

            let existing = admin
                .latest_database_like(&format!("{prefix}%"))
                .await
                .expect("look for an existing template");

            let name = match existing {
                // Another process already built this exact schema — reuse it.
                // This is the whole point: N test binaries, one template.
                Some(found) => found,
                None => {
                    let name = format!("{prefix}{}", now_secs());
                    admin
                        .create_database(&name)
                        .await
                        .expect("create the template database");

                    let provisioned = Provisioned::Pg {
                        base_url: base_url.to_string(),
                        db_name: name.clone(),
                    };
                    let pool = lifecycle::open(&provisioned)
                        .await
                        .expect("connect to the template database");
                    nook_control::MIGRATOR
                        .run(pool.pg())
                        .await
                        .expect("migrate the template database");
                    nook_control::seed::run(&pool, &Config::for_test())
                        .await
                        .expect("seed the template database");
                    // Release every connection so the template can be cloned.
                    pool.pg().close().await;
                    name
                }
            };

            reap_stale_templates(&mut admin, &name).await;

            admin.advisory_unlock(TEMPLATE_LOCK).await;
            admin.close().await;
            name
        })
        .await
}

/// Reclaim templates a crashed or killed process left behind.
///
/// Reuse alone is not enough: a process that dies mid-build, or a schema that
/// has since changed, leaves a template nobody will ever ask for again. Three
/// conditions, all required, because dropping a template another suite is
/// cloning from would turn this cleanup into the failure:
///
/// 1. it is not the one we are about to use;
/// 2. nothing is connected to it (the direct test for "in use");
/// 3. its epoch is older than [`TEMPLATE_MAX_AGE_SECS`] (a concurrently running
///    suite on another branch has a recent template of its own fingerprint).
///
/// Best-effort throughout — a failure to tidy up must never fail a test run.
/// Callers hold [`TEMPLATE_LOCK`].
async fn reap_stale_templates(admin: &mut AdminConn, keep: &str) {
    let cutoff = now_secs().saturating_sub(TEMPLATE_MAX_AGE_SECS);
    let Ok(names) = admin.unused_databases_like("nook_tmpl_%", keep).await else {
        return;
    };

    for name in names {
        // The trailing epoch says when it was built. A name without one is from
        // before this scheme (the leaked `nook_tmpl_<uuid>` generation) and is
        // by definition stale, so it is reclaimed.
        let stale = match name.rsplit('_').next().and_then(|t| t.parse::<u64>().ok()) {
            Some(built) => built < cutoff,
            None => true,
        };
        if !stale {
            continue;
        }
        admin.drop_database(&name).await.ok();
    }
}

/// A prepared, **private** test world: a freshly created, migrated, and seeded
/// database plus opt-in setup surfaces, dropped whole at teardown.
pub struct TestBed {
    /// The engine-agnostic pool, and the **only** one a test can reach.
    ///
    /// The raw-Postgres-pool escape hatch that sat beside this is gone
    /// (MAIN-268). While it existed, every consumer of it was a Postgres-leg
    /// test by construction, and on a SQLite bed it had to hold an inert
    /// handle aimed at a `.invalid` host so that using it failed instead of
    /// silently running an allegedly-isolated test against the shared dev
    /// database. Nothing needs that shape now: `db` is the surface, and a test
    /// that genuinely needs raw multi-statement Postgres SQL takes its own
    /// connection from [`TestBed::database_url`].
    db: EnginePool,
    /// What was created, and how to undo it.
    arm: Provisioned,
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
        let mut admin = AdminConn::connect(&base_url)
            .await
            .expect("connect to the base database to create a test database");
        admin
            .create_database_from_template(&db_name, template)
            .await
            .expect("create the test database from the template");
        admin.close().await;

        let arm = Provisioned::Pg { base_url, db_name };
        let db = lifecycle::open(&arm)
            .await
            .expect("connect to the fresh test database");

        TestBed {
            db,
            arm,
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
        lifecycle::remove_sqlite_files(&path);

        let db = lifecycle::open_sqlite_bed(&path)
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
            db,
            arm: Provisioned::Sqlite { path },
            keep,
            dropped: false,
        }
    }

    /// Which engine this bed is on — for tests that must gate on it, such as
    /// ones that bind raw `sqlx` against their own connection from
    /// [`TestBed::database_url`].
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
            Provisioned::Pg { db_name, .. } => db_name,
            Provisioned::Sqlite { path } => path.to_str().unwrap_or_default(),
        }
    }

    /// This bed's private database as a connection URL, so a dependent crate can
    /// open an **additional** pool against it with its own options (MAIN-165).
    ///
    /// [`TestBed::db`] is one pool with one configuration, which is all a
    /// nook-control test wants. nook-chat pins `search_path=chat,public` so that
    /// its own tables and the control plane's auth tables both resolve — a
    /// property of the pool, not of the database — so it has to build its own
    /// against this same bed rather than borrow ours.
    ///
    /// `None` on SQLite: one file is one namespace with one ledger, which is
    /// exactly why chat's tables live in the control track there. Nothing has a
    /// second configuration to ask for, so there is nothing to hand out.
    pub fn database_url(&self) -> Option<String> {
        lifecycle::database_url(&self.arm)
    }

    /// Drop the whole private database (unless `NOOK_KEEP_TEST_DATA`). Idempotent.
    pub async fn teardown(&mut self) {
        if self.dropped || self.keep {
            self.dropped = true;
            return;
        }
        self.dropped = true;
        // Close first: on Windows an open handle would block the unlink, and
        // on Postgres releasing our own connections is tidier than making
        // `WITH (FORCE)` sever them.
        match &self.arm {
            Provisioned::Pg { .. } => self.db.pg().close().await,
            Provisioned::Sqlite { .. } => self.db.sqlite().close().await,
        }
        lifecycle::destroy(&self.arm).await;
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

/// The first page of `limit` rows in a list's default order — what a caller
/// that passes no cursor gets. Sortless and searchless, so it fits any list's
/// allowlist including an empty one.
pub fn first_page(limit: i64) -> nook_db::paging::PageArgs {
    nook_db::paging::PageArgs::parse(None, None, Some(limit), None, None, &[])
        .expect("a bare limit is valid against every allowlist")
}

/// The page that CONTINUES `cursor`, at the same size. Panics on a token the
/// codec refuses — a test that walks its own `next_cursor` never produces one.
pub fn page_after(cursor: &str, limit: i64) -> nook_db::paging::PageArgs {
    nook_db::paging::PageArgs::parse(None, Some(cursor), Some(limit), None, None, &[])
        .expect("a cursor this walk was handed")
}

impl Drop for TestBed {
    fn drop(&mut self) {
        // Safety net: if the test skipped `teardown().await` (or panicked before
        // it), drop the database anyway. Drop is sync, so the async work runs on
        // a throwaway current-thread runtime on a fresh OS thread.
        //
        // This thread must NEVER touch `self.db`'s pool (MAIN-185): the pool's
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
            Provisioned::Pg { base_url, db_name } => {
                let arm = Provisioned::Pg {
                    base_url: base_url.clone(),
                    db_name: db_name.clone(),
                };
                let _ = std::thread::spawn(move || {
                    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    else {
                        return;
                    };
                    rt.block_on(async { lifecycle::destroy(&arm).await });
                })
                .join();
            }
            // SQLite needs no runtime and no thread: unlinking is a sync
            // syscall. It also must not close the pool, for the same reason the
            // Postgres arm must not — see the note above. On unix the unlink
            // succeeds with the file still open, and the pool then drops
            // inertly on the test thread.
            Provisioned::Sqlite { path } => lifecycle::remove_sqlite_files(path),
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

    /// A dependent crate opens its own pool from this URL, so it must name the
    /// bed's PRIVATE database — handing back the base URL would point every such
    /// pool at the shared dev database, which is the bug the exposure exists to
    /// prevent (MAIN-165).
    #[tokio::test]
    async fn the_exposed_url_names_the_private_database() {
        let Some(mut bed) = TestBed::new().await else {
            return;
        };
        if !bed.is_postgres() {
            assert!(
                bed.database_url().is_none(),
                "SQLite has no second configuration to hand out"
            );
            bed.teardown().await;
            return;
        }
        let url = bed.database_url().expect("a Postgres bed exposes its URL");
        assert!(
            url.ends_with(bed.db_name()),
            "{url} names {}",
            bed.db_name()
        );
        assert!(
            url.starts_with("postgres://"),
            "and is a connection URL, not a bare name: {url}"
        );
        bed.teardown().await;
    }
}

#[cfg(test)]
mod template_tests {
    use super::*;

    /// The fingerprint is what makes a template findable by another process, so
    /// it must be stable across calls — an unstable one would silently restore
    /// the old one-template-per-process leak.
    #[test]
    fn the_fingerprint_is_stable_and_shaped_like_a_name() {
        let a = template_fingerprint();
        let b = template_fingerprint();
        assert_eq!(a, b, "same migration set, same fingerprint");
        assert_eq!(a.len(), 16, "fixed width keeps the datname predictable");
        assert!(
            a.chars().all(|c| c.is_ascii_hexdigit()),
            "safe in a datname without quoting: {a}"
        );
    }

    /// The staleness rule the reaper applies, exercised without a database.
    /// Getting this wrong in either direction is costly: too eager drops a
    /// template a concurrent suite is cloning from, too lax restores the leak.
    #[test]
    fn staleness_reads_the_epoch_suffix_and_treats_the_old_scheme_as_stale() {
        fn stale(name: &str, cutoff: u64) -> bool {
            match name.rsplit('_').next().and_then(|t| t.parse::<u64>().ok()) {
                Some(built) => built < cutoff,
                None => true,
            }
        }
        let cutoff = 1_000_000u64;

        // Built before the cutoff → reclaimable.
        assert!(stale("nook_tmpl_abc123_999999", cutoff));
        // Built after → left alone; this is the concurrent-suite case.
        assert!(!stale("nook_tmpl_abc123_1000001", cutoff));
        // Exactly at the cutoff is not yet stale (strictly older only).
        assert!(!stale("nook_tmpl_abc123_1000000", cutoff));

        // The pre-fix generation carries a uuid, not an epoch. Those are the
        // 1,589 that crashed Postgres — always reclaimable.
        assert!(stale("nook_tmpl_019fa5c7901e7dd183339585a6a16efb", cutoff));
    }
}
