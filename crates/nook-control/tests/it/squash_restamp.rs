//! The boot-time squash re-stamp (MAIN-235 AC-6).
//!
//! The load-bearing property: the collapse is atomic and fires ONLY on a ledger
//! that is exactly the set the manifest replaced. Everything else — a virgin
//! database, an already-squashed one, one carrying post-squash migrations, one
//! nobody can account for — must come out the other side either untouched or
//! correct, never half-rewritten.
//!
//! This is the prod-safety test. The documented near-miss was a ledger re-stamp
//! that did not match the image shipping it, so these tests care much more about
//! the refusals than about the happy path.
//!
//! Each test owns a private database (MAIN-156 TestBed) and writes only its own
//! ledger rows.
//!
//! The re-stamp itself is Postgres-only by design (MAIN-420 AC-3), so the tests
//! that call it directly gate on the bed's engine via [`restamp_applies`]; what
//! survives on both legs is the boot path's dev/prod split, which is a property
//! of every engine. The migrator is therefore selected by engine too — naming
//! `nook_control::MIGRATOR` on a SQLite bed drives the Postgres track against
//! the wrong schema history (MAIN-549).

use nook_db::restamp::{parse_manifest, restamp, Restamp, RestampError, SquashManifest};
use nook_db::{params, Db, EnginePool};
use nook_testkit::TestBed;

/// A manifest describing a squash of versions 1..=n into a single new row.
fn manifest(n: i64) -> SquashManifest {
    SquashManifest {
        new_version: 1,
        new_description: "init".into(),
        new_checksum: vec![0xBE, 0xEF],
        old: (1..=n).map(|v| (v, vec![v as u8, 0xAA])).collect(),
    }
}

/// Replace the ledger with exactly `rows`, so each test states the database it
/// means to describe rather than inheriting one.
async fn set_ledger(db: &EnginePool, rows: &[(i64, Vec<u8>)]) {
    db.exec("DELETE FROM _sqlx_migrations", params![])
        .await
        .expect("clear ledger");
    for (v, c) in rows {
        db.exec(
            "INSERT INTO _sqlx_migrations
                (version, description, installed_on, success, checksum, execution_time)
             VALUES ($1, 'x', now(), true, $2, 0)",
            params![*v, c.clone()],
        )
        .await
        .expect("insert ledger row");
    }
}

async fn ledger(db: &EnginePool) -> Vec<(i64, Vec<u8>)> {
    db.query_all(
        "SELECT version, checksum FROM _sqlx_migrations ORDER BY version",
        params![],
    )
    .await
    .expect("read ledger")
}

/// Whether the re-stamp is a meaningful operation on this bed.
///
/// It rescues a database that applied a *previous* Postgres migration set; a
/// SQLite file has no such history, so `restamp` answers `EngineUnsupported`
/// there rather than a quiet `Ok` — asserted head-on in
/// [`sqlite_refuses_the_restamp_and_says_so`]. Every test below that drives a
/// ledger through it is asking a Postgres question, so it skips instead of
/// re-asserting the refusal five more times.
async fn restamp_applies(bed: &mut TestBed) -> bool {
    if bed.is_postgres() {
        return true;
    }
    eprintln!("skipping — the squash re-stamp is Postgres-only by design (MAIN-420 AC-3)");
    bed.teardown().await;
    false
}

#[tokio::test]
async fn collapses_a_matching_pre_squash_ledger_to_the_single_row() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    if !restamp_applies(&mut bed).await {
        return;
    }
    let m = manifest(28);
    set_ledger(&bed.db(), &m.old).await;

    let out = restamp(&bed.db(), Some(&m), "_sqlx_migrations")
        .await
        .expect("re-stamp");
    assert_eq!(out, Restamp::Collapsed { replaced: 28 });

    // Exactly one row, and it is the manifest's — description and checksum
    // included, because the migrator validates the checksum on the next boot.
    assert_eq!(ledger(&bed.db()).await, vec![(1, m.new_checksum.clone())]);
    let desc: String = bed
        .db()
        .query_scalar("SELECT description FROM _sqlx_migrations", params![])
        .await
        .unwrap();
    assert_eq!(desc, "init");

    bed.teardown().await;
}

#[tokio::test]
async fn a_second_boot_is_a_no_op() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    if !restamp_applies(&mut bed).await {
        return;
    }
    let m = manifest(28);
    set_ledger(&bed.db(), &m.old).await;

    restamp(&bed.db(), Some(&m), "_sqlx_migrations")
        .await
        .expect("first");
    let after_first = ledger(&bed.db()).await;

    let out = restamp(&bed.db(), Some(&m), "_sqlx_migrations")
        .await
        .expect("second");
    assert_eq!(out, Restamp::AlreadySquashed);
    assert_eq!(ledger(&bed.db()).await, after_first, "untouched");

    bed.teardown().await;
}

/// The regression that would break every deploy after the squash: once new
/// migrations land on top of the canonical row, the ledger is neither "just the
/// new row" nor "the old set". It must still read as already-squashed.
#[tokio::test]
async fn post_squash_migrations_do_not_look_like_a_mismatch() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    if !restamp_applies(&mut bed).await {
        return;
    }
    let m = manifest(28);
    set_ledger(
        &bed.db(),
        &[
            (1, m.new_checksum.clone()),
            (2, vec![0x02, 0x02]),
            (3, vec![0x03, 0x03]),
        ],
    )
    .await;

    let out = restamp(&bed.db(), Some(&m), "_sqlx_migrations")
        .await
        .expect("re-stamp");
    assert_eq!(out, Restamp::AlreadySquashed);
    assert_eq!(
        ledger(&bed.db()).await.len(),
        3,
        "the new migrations survive"
    );

    bed.teardown().await;
}

/// AC-4's refusal, three ways it can arise. In every one the ledger must come
/// out byte-identical: a database we cannot account for is not one we rewrite.
#[tokio::test]
async fn an_unrecognised_ledger_is_refused_and_left_untouched() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    if !restamp_applies(&mut bed).await {
        return;
    }
    let m = manifest(28);

    // (1) An extra version — the classic unmerged-branch row on a shared dev DB.
    let mut with_extra = m.old.clone();
    with_extra.push((99, vec![0x99, 0x99]));
    set_ledger(&bed.db(), &with_extra).await;
    let err = restamp(&bed.db(), Some(&m), "_sqlx_migrations")
        .await
        .expect_err("extra version refused");
    let msg = err.to_string();
    assert!(matches!(err, RestampError::LedgerMismatch { .. }), "{msg}");
    assert!(msg.contains("[99]"), "names the offending version: {msg}");
    assert_eq!(ledger(&bed.db()).await, with_extra, "untouched");

    // (2) A missing version — a database that never finished migrating.
    let short = m.old[..20].to_vec();
    set_ledger(&bed.db(), &short).await;
    let err = restamp(&bed.db(), Some(&m), "_sqlx_migrations")
        .await
        .expect_err("short ledger refused");
    assert!(err.to_string().contains("in the manifest but not applied"));
    assert_eq!(ledger(&bed.db()).await, short, "untouched");

    // (3) Right versions, wrong content — an applied migration was edited.
    let mut edited = m.old.clone();
    edited[5].1 = vec![0xDE, 0xAD];
    set_ledger(&bed.db(), &edited).await;
    let err = restamp(&bed.db(), Some(&m), "_sqlx_migrations")
        .await
        .expect_err("checksum drift refused");
    let msg = err.to_string();
    assert!(msg.contains("checksums do not"), "{msg}");
    assert!(msg.contains("[6]"), "names the drifted version: {msg}");
    assert_eq!(ledger(&bed.db()).await, edited, "untouched");

    bed.teardown().await;
}

#[tokio::test]
async fn a_virgin_database_and_a_build_with_no_squash_both_pass_through() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    if !restamp_applies(&mut bed).await {
        return;
    }

    // No manifest embedded: the whole step is skipped.
    assert_eq!(
        restamp(&bed.db(), None, "_sqlx_migrations").await.unwrap(),
        Restamp::NoManifest
    );

    // Empty ledger: the migrator will apply the canonical 0001 itself.
    set_ledger(&bed.db(), &[]).await;
    assert_eq!(
        restamp(&bed.db(), Some(&manifest(28)), "_sqlx_migrations")
            .await
            .unwrap(),
        Restamp::EmptyLedger
    );

    // A ledger table that does not exist at all is the same story.
    assert_eq!(
        restamp(&bed.db(), Some(&manifest(28)), "_no_such_ledger")
            .await
            .unwrap(),
        Restamp::EmptyLedger
    );

    bed.teardown().await;
}

/// The manifest the repo actually ships must parse — or not exist. A malformed
/// one would silently disable the re-stamp on every boot, and the first anyone
/// would know is a production that will not start.
#[test]
fn the_shipped_manifests_are_parseable() {
    for (name, text) in [
        (
            "control",
            include_str!("../../migrations/squash-manifest.txt"),
        ),
        (
            "chat",
            include_str!("../../../nook-chat/migrations/squash-manifest.txt"),
        ),
    ] {
        let parsed = parse_manifest(text);
        // A placeholder (no `new` line) is legitimate — it means this build
        // ships no squash. What must never happen is a file that LOOKS like a
        // manifest and does not parse.
        if text.lines().any(|l| l.trim_start().starts_with("new ")) {
            let m = parsed
                .unwrap_or_else(|| panic!("{name}: manifest has a `new` line but does not parse"));
            assert!(!m.old.is_empty(), "{name}: a squash replaced nothing?");
            assert_eq!(m.new_checksum.len(), 48, "{name}: sha384 is 48 bytes");
            for (_, c) in &m.old {
                assert_eq!(c.len(), 48, "{name}: every old checksum is sha384");
            }
        } else {
            assert!(parsed.is_none(), "{name}: placeholder must parse as absent");
        }
    }
}

/// AC-4/AC-6: the dev/prod split on an unrecognised ledger, through the real
/// boot path both services call.
///
/// This is the prod-safety property in one test. Production must refuse to boot
/// rather than run against a schema history it cannot account for; dev must warn
/// and carry on, because there the cause is nearly always a colleague's unmerged
/// branch and bricking every checkout was the MAIN-224 outage.
///
/// Runs on both engines, but they assert different halves of it, and the SQLite
/// half is the weaker one. Postgres asserts LEDGER RECOGNITION: the re-stamp
/// reads `bogus`, finds a ledger it cannot account for, and refuses. On SQLite
/// `restamp` answers `EngineUnsupported` before it ever looks at a manifest, so
/// `bogus` is inert there and a VALID manifest would produce the same run; what
/// the SQLite arm asserts is the PROPAGATION — that a declining re-stamp is
/// fatal in production, tolerated in dev, and touches the ledger in neither.
///
/// That arm is also a path production never takes: `run_boot_migrations_for`
/// dispatches on engine and never calls `run_boot_migrations` for a SQLite pool.
/// Its value is that the split holds *if* something ever does.
///
/// What must be selected by engine either way is the MIGRATOR — the second half
/// of `run_boot_migrations` is a real migration run, and the Postgres track
/// against a SQLite bed is `VersionMissing(38)`, that track's solo migration
/// (MAIN-549 AC-2).
#[tokio::test]
async fn production_refuses_an_unrecognised_ledger_where_dev_proceeds() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let migrator = nook_control::migrator_for(bed.engine());
    // A manifest that describes a squash this database is NOT a candidate for:
    // TestBed's ledger is the real, fully-migrated set.
    let bogus = "\
set control
new 1 beef init
old 1 1111
old 2 2222
";
    let before = ledger(&bed.db()).await;

    let err = nook_db::migrate::run_boot_migrations(
        migrator,
        &bed.db(),
        true, // production
        bogus,
    )
    .await
    .expect_err("production refuses a ledger the manifest does not describe");
    assert!(
        matches!(err, nook_db::migrate::BootMigrateError::Restamp(_)),
        "the refusal comes from the re-stamp, not the schema: {err}"
    );
    assert_eq!(
        ledger(&bed.db()).await,
        before,
        "prod refusal touches nothing"
    );

    // Same database, same bogus manifest, dev: the re-stamp declines, says so,
    // and the boot proceeds through the migrator as normal.
    nook_db::migrate::run_boot_migrations(migrator, &bed.db(), false, bogus)
        .await
        .expect("dev proceeds past an unrecognised ledger");
    assert_eq!(
        ledger(&bed.db()).await,
        before,
        "dev refusal touches nothing either"
    );

    bed.teardown().await;
}

/// MAIN-420 AC-3: what SQLite gets at each of the three points, stated as a
/// test rather than only as a comment.
///
/// The re-stamp REFUSES. It exists to rescue a database that applied a previous
/// Postgres migration set; a SQLite file has no such history, so there is
/// nothing to collapse — and answering `Ok` would be the silent no-op in a
/// re-stamp, where a caller could not tell "nothing to do" from "it did
/// nothing". The other two are questions rather than actions and answer
/// honestly instead: `orphan_versions` is empty (no shared dev database, so no
/// stray row), and `run_with_dev_tolerance` runs the plain migrator (migrating
/// is meaningful on every engine; only the tolerance is Postgres's problem).
///
/// No Postgres required — SQLite runs in memory.
#[tokio::test]
async fn sqlite_refuses_the_restamp_and_says_so() {
    let sqlite = nook_db::connect("sqlite::memory:", 1)
        .await
        .expect("open in-memory sqlite");

    let m = manifest(3);
    let err = restamp(&sqlite, Some(&m), "_sqlx_migrations")
        .await
        .expect_err("a re-stamp on SQLite is a refusal, never a quiet Ok");
    assert!(matches!(err, RestampError::EngineUnsupported), "{err:?}");
    // The message has to explain itself: this is the only thing a caller who
    // reached it by mistake will read.
    assert!(err.to_string().contains("Postgres-only"), "{err}");

    // And with NO manifest either — the engine is checked first, so a build
    // shipping no squash still gets the refusal rather than `NoManifest`, which
    // would read as "this is fine on SQLite".
    assert!(matches!(
        restamp(&sqlite, None, "_sqlx_migrations").await,
        Err(RestampError::EngineUnsupported)
    ));

    // The question, answered honestly rather than refused.
    let orphans = nook_db::migrate::orphan_versions(
        nook_control::migrator_for(nook_db::Engine::Sqlite),
        &sqlite,
    )
    .await
    .expect("orphan_versions answers on SQLite");
    assert!(orphans.is_empty(), "{orphans:?}");
}
