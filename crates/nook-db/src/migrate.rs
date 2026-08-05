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

/// The engine-aware boot step (MAIN-196 AC-2/AC-3): pick the migration set for
/// the pool in front of us and run it.
///
/// SQLite takes the simple path deliberately. The squash re-stamp (MAIN-235)
/// exists to rescue databases that applied a *previous* Postgres migration set;
/// a SQLite file has no such history — its track starts at its own frozen
/// `0001` — so there is nothing to collapse and pretending otherwise would only
/// add a way to be wrong. Dev tolerance is likewise Postgres's problem: it fixes
/// a SHARED dev database that branches take turns migrating, and a SQLite file
/// is nobody's shared database.
pub async fn run_boot_migrations_for(
    pool: &crate::DbPool,
    is_production: bool,
    pg_migrator: &Migrator,
    sqlite_migrator: &Migrator,
    manifest_text: &str,
) -> Result<(), BootMigrateError> {
    match pool.engine() {
        crate::Engine::Postgres => {
            run_boot_migrations(pg_migrator, pool, is_production, manifest_text).await
        }
        crate::Engine::Sqlite => {
            sqlite_migrator
                .run(pool.sqlite())
                .await
                .map_err(BootMigrateError::Migrate)?;
            tracing::info!("sqlite schema migrated");
            Ok(())
        }
    }
}

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
    pool: &crate::DbPool,
    is_production: bool,
) -> Result<(), MigrateError> {
    // SQLite runs the plain migrator (MAIN-420 AC-3). Not a refusal here,
    // because unlike the re-stamp this function's OTHER half — actually
    // migrating — is meaningful on every engine; what does not apply is the
    // tolerance. Dev tolerance exists for a SHARED dev database that branches
    // take turns migrating, and a SQLite file is nobody's shared database, so
    // there is no orphan row to forgive. `run_boot_migrations_for` already made
    // exactly this call at the dispatch level; this moves it inside so a caller
    // holding an `EnginePool` gets the same answer.
    if pool.engine() != crate::Engine::Postgres {
        return migrator.run(pool.sqlite()).await;
    }

    if is_production {
        return migrator.run(pool.pg()).await;
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
    tolerant.run(pool.pg()).await
}

/// Ledger versions with no matching resolved migration — the rows a dev boot
/// tolerates and `scripts/dev-db-heal.sh` removes.
///
/// Returns empty when the ledger table does not yet exist (a first boot), so it
/// never fails a fresh database. The query is unqualified, so it reads the ledger
/// through the pool's `search_path`: `chat._sqlx_migrations` for the chat pool
/// (search_path `chat,public`), `public._sqlx_migrations` for the control plane.
pub async fn orphan_versions(
    migrator: &Migrator,
    pool: &crate::DbPool,
) -> Result<Vec<i64>, MigrateError> {
    // Postgres-only, and empty rather than an error on SQLite (MAIN-420 AC-3).
    // This one is a QUESTION, not an action: "which ledger rows have no
    // migration file". On a SQLite file the honest answer is "none" — its track
    // starts at its own frozen 0001 and no branch shares it — so returning
    // empty is the true answer, not a silent no-op standing in for one. The
    // `to_regclass` probe below is Postgres syntax and would error there
    // regardless.
    if pool.engine() != crate::Engine::Postgres {
        return Ok(Vec::new());
    }
    let pool = pool.pg();

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

/// The one boot-time database step both services run: collapse a pre-squash
/// ledger if this build ships a squash (MAIN-235), then migrate with MAIN-224's
/// dev tolerance.
///
/// The two halves belong together because their failure modes interact. A
/// database still carrying the pre-squash ledger looks, to the migrator, exactly
/// like a database with 28 orphan rows — and dev tolerance would happily boot
/// past it, leaving the ledger permanently wrong. Doing the re-stamp first means
/// tolerance only ever sees what it is actually for: a stray row from someone's
/// unmerged branch.
///
/// `manifest_text` is the crate's embedded `squash-manifest.txt`. A file with no
/// `new` line means "this build ships no squash" and the whole step is skipped,
/// so carrying a placeholder costs nothing.
///
/// The dev/prod split is the same one MAIN-224 established, applied to the new
/// failure: an unrecognised ledger is **fatal in production** — we will not boot
/// a control plane whose schema history we cannot account for — and in dev it is
/// a loud WARN, because there the overwhelmingly likely cause is a branch's
/// stray migration, and tolerance already knows how to carry that boot.
pub async fn run_boot_migrations(
    migrator: &Migrator,
    pool: &crate::DbPool,
    is_production: bool,
    manifest_text: &str,
) -> Result<(), BootMigrateError> {
    let manifest = crate::restamp::parse_manifest(manifest_text);

    match crate::restamp::restamp(pool, manifest.as_ref(), "_sqlx_migrations").await {
        Ok(crate::restamp::Restamp::Collapsed { replaced }) => {
            tracing::info!(
                replaced,
                "collapsed this database's pre-squash migration ledger to the single \
                 canonical row (MAIN-235). This runs once; later boots are a no-op."
            );
        }
        Ok(_) => {}
        Err(e) => {
            if is_production {
                return Err(BootMigrateError::Restamp(e));
            }
            tracing::warn!(
                error = %e,
                "the squash re-stamp did not recognise this database's ledger and left \
                 it untouched. Proceeding (dev only; production refuses to boot here)."
            );
        }
    }

    run_with_dev_tolerance(migrator, pool, is_production)
        .await
        .map_err(BootMigrateError::Migrate)
}

/// Why a boot's database step failed. Split so the caller's log says whether the
/// schema or the ledger was the problem — they need different fixes.
#[derive(Debug, thiserror::Error)]
pub enum BootMigrateError {
    #[error(transparent)]
    Migrate(#[from] MigrateError),
    #[error(transparent)]
    Restamp(#[from] crate::restamp::RestampError),
}
