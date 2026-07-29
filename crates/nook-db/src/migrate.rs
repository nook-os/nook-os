//! Boot-time migration runner with dev tolerance for a ledger that is *ahead* of
//! the checked-out migration set (MAIN-224).
//!
//! The failure this closes: a branch carrying migration N runs against the
//! shared dev database (a stack boot or a test path from that checkout), which
//! records N in `_sqlx_migrations`. Every other checkout — one without that
//! `.sql` file — then fails boot with *"migration N was previously applied but
//! is missing in the resolved migrations"*, and switching branches on the
//! bind-mounted tree bricks the control plane until someone hand-deletes the
//! ledger row. Two outages came from exactly this.
//!
//! [`run_with_dev_tolerance`] keeps production strictly fatal — a leaked
//! tolerance there would mask real schema drift — while letting a dev boot warn
//! loudly and proceed past those orphan rows. `scripts/dev-db-heal.sh` is the
//! deliberate cleanup; this is the survive-the-boot half.

use sqlx::migrate::{MigrateError, Migrator};
use sqlx::PgPool;

/// Run `migrator` against `pool`, tolerating a ledger that is ahead of the
/// resolved migration set **in dev only**.
///
/// When `is_production` is true this is exactly `migrator.run(pool)`: an applied
/// migration missing from the resolved set aborts the boot as it always has
/// (AC-2 / NG-2), so real schema drift can never be silently masked.
///
/// Otherwise each orphan ledger row — a version applied to this database but
/// absent from the checked-out migrations — is logged as a loud WARN naming the
/// version and this failure class, then ignored so the boot proceeds (AC-1).
///
/// It tolerates **missing** versions only. A checksum mismatch on a migration
/// that IS present ([`MigrateError::VersionMismatch`]) stays fatal in every
/// environment: this survives an unmerged branch's stray row, it never rewrites
/// or re-stamps the ledger (NG-1).
pub async fn run_with_dev_tolerance(
    migrator: &Migrator,
    pool: &PgPool,
    is_production: bool,
) -> Result<(), MigrateError> {
    if is_production {
        return migrator.run(pool).await;
    }

    for version in orphan_versions(migrator, pool).await? {
        tracing::warn!(
            version,
            "migration {version} is recorded in this database's ledger but is \
             missing from the checked-out migration set — likely an unmerged \
             branch's migration applied to a shared dev DB. Ignoring it so the \
             boot proceeds (dev only; production stays fatal). Heal with \
             scripts/dev-db-heal.sh; see CLAUDE.md \u{2192} Database workflow."
        );
    }

    // `Migrator`'s fields are public (doc-hidden, semver-exempt) precisely so
    // `migrate!()` can initialise the static in a const context — which is also
    // what lets us copy that static into a mutable local and flip one flag for
    // this run, instead of mutating the shared static. `ignore_missing` makes the
    // migrator's internal validation skip the very versions we just warned about;
    // it changes nothing else about how migrations apply.
    let mut tolerant = Migrator {
        migrations: migrator.migrations.clone(),
        ignore_missing: migrator.ignore_missing,
        locking: migrator.locking,
        no_tx: migrator.no_tx,
    };
    tolerant.set_ignore_missing(true);
    tolerant.run(pool).await
}

/// Ledger versions with no matching resolved migration — the rows a dev boot
/// tolerates and `scripts/dev-db-heal.sh` removes.
///
/// Returns empty when the ledger table does not yet exist (a first boot), so it
/// never fails a fresh database. The query is unqualified, so it reads the ledger
/// through the pool's `search_path`: `chat._sqlx_migrations` for the chat pool
/// (search_path `chat,public`), `public._sqlx_migrations` for the control plane.
pub async fn orphan_versions(migrator: &Migrator, pool: &PgPool) -> Result<Vec<i64>, MigrateError> {
    // `to_regclass` yields NULL until the ledger has been created; selecting FROM
    // a missing table would error, so gate on existence first.
    let ledger: Option<String> = sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations')::text")
        .fetch_one(pool)
        .await?;
    if ledger.is_none() {
        return Ok(Vec::new());
    }
    let applied: Vec<i64> = sqlx::query_scalar("SELECT version FROM _sqlx_migrations")
        .fetch_all(pool)
        .await?;
    Ok(applied
        .into_iter()
        .filter(|v| !migrator.version_exists(*v))
        .collect())
}
