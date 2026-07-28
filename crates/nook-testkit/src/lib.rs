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

use anyhow::{Context, Result};
use nook_control::state::AppState;
use nook_infra::Config;
use nook_types::{NodeId, TenantId, UserId, WorkspaceId};
use sqlx::{Connection, PgConnection, PgPool};
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
            nook_control::seed::run(&pool, &Config::for_test())
                .await
                .expect("seed the template database");
            // Release every connection so the template can be cloned.
            pool.close().await;
            name
        })
        .await
}

/// A prepared, **private** test world: a freshly created, migrated, and seeded
/// database plus opt-in setup surfaces, dropped whole at teardown.
pub struct TestBed {
    /// The pool for this test's private database.
    pub pool: PgPool,
    /// `DATABASE_URL` — the server + base database, used for the admin
    /// `CREATE`/`DROP DATABASE` statements (which cannot run against the target).
    base_url: String,
    /// The unique database this bed created (e.g. `nook_test_<uuid>`).
    db_name: String,
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

        Some(TestBed {
            pool,
            base_url,
            db_name,
            keep,
            dropped: false,
        })
    }

    /// The canonical test [`Config`] — one construction, overridable per field
    /// (mutate the returned value). Wraps `Config::for_test()`.
    pub fn config(&self) -> Config {
        Config::for_test()
    }

    /// A fresh `AppState` on this bed's database with the canonical config.
    pub async fn app_state(&self) -> AppState {
        AppState::new(self.pool.clone(), self.config(), None).await
    }

    /// Create a tenant (name = slug = `test-<hint>-<uuid>`). No tracking needed —
    /// the whole database is dropped at teardown.
    pub async fn tenant(&self, hint: &str) -> TenantId {
        let id = TenantId::new();
        let name = format!("test-{hint}-{}", id.0.simple());
        sqlx::query("INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $2)")
            .bind(id)
            .bind(name)
            .execute(&self.pool)
            .await
            .expect("create tenant");
        id
    }

    /// Create a user in `tenant` with `role`. Returns `(user_id, person_id)` —
    /// the person id is what node ownership keys on.
    pub async fn user(&self, tenant: TenantId, role: &str) -> (UserId, Uuid) {
        let user = UserId::new();
        let person = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO users (id, tenant_id, person_id, display_name, email, role)
             VALUES ($1, $2, $3, 'U', $4, $5)",
        )
        .bind(user)
        .bind(tenant)
        .bind(person)
        .bind(format!("u-{}@example.test", user.0.simple()))
        .bind(role)
        .execute(&self.pool)
        .await
        .expect("create user");
        (user, person)
    }

    /// Create an offline node in `tenant` owned by `owner` (a person id).
    pub async fn node(&self, tenant: TenantId, owner: Uuid) -> NodeId {
        let id = NodeId::new();
        sqlx::query(
            "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status, owner_person_id)
             VALUES ($1, $2, $3, $4, 'offline', $5)",
        )
        .bind(id)
        .bind(tenant)
        .bind(format!("n-{}", id.0.simple()))
        .bind(format!("h-{}", id.0.simple()))
        .bind(owner)
        .execute(&self.pool)
        .await
        .expect("create node");
        id
    }

    /// Create a workspace in `tenant`.
    pub async fn workspace(&self, tenant: TenantId) -> WorkspaceId {
        let id = WorkspaceId::new();
        let name = format!("test-ws-{}", id.0.simple());
        sqlx::query("INSERT INTO workspaces (id, tenant_id, name, slug) VALUES ($1, $2, $3, $3)")
            .bind(id)
            .bind(tenant)
            .bind(name)
            .execute(&self.pool)
            .await
            .expect("create workspace");
        id
    }

    /// Force the keep-on-teardown flag (test-support for the teardown tests; in
    /// normal use it comes from `NOOK_KEEP_TEST_DATA`).
    pub fn set_keep(&mut self, keep: bool) {
        self.keep = keep;
    }

    /// The name of this bed's private database — for teardown-behaviour tests.
    pub fn db_name(&self) -> &str {
        &self.db_name
    }

    /// Drop the whole private database (unless `NOOK_KEEP_TEST_DATA`). Idempotent.
    pub async fn teardown(&mut self) {
        if self.dropped || self.keep {
            self.dropped = true;
            return;
        }
        self.dropped = true;
        self.pool.close().await;
        let _ = drop_database(&self.base_url, &self.db_name).await;
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
        let base = self.base_url.clone();
        let name = self.db_name.clone();
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
