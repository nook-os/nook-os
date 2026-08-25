//! `users.person_id` gets its default back on the SQLite track (MAIN-293).
//!
//! `0001` dropped Postgres's `DEFAULT gen_random_uuid()` on a documented
//! assumption — *"the application already supplies a person id on every
//! insert"* — which the two-engine measurement (MAIN-270) disproved: 26 inserts
//! rely on the default and failed with `NOT NULL constraint failed:
//! users.person_id`.
//!
//! The fix is in `0001` itself rather than a forward delta. That is a departure
//! from the frozen-`0001` rule, taken deliberately by the owner: **no SQLite
//! database is ever carried forward**. The testkit creates a fresh file per test
//! and migrates it from scratch, and the dev stack runs Postgres — so there is
//! no ledger to invalidate and nothing to migrate. A delta would have meant
//! rebuilding the table (SQLite has no `ALTER COLUMN`) with foreign keys
//! disabled, because `DROP TABLE users` cascades into the sixteen tables
//! referencing it. All of that risk to reach a schema `0001` can simply state.
//!
//! What is asserted here is behaviour, not the text of the file: that the
//! default exists, produces a real uuid, produces a DIFFERENT one per row, and
//! never overwrites a value the caller supplied.

use nook_db::{params, Db, DbPool};
use sqlx::Row;

/// A private SQLite database, migrated through the real embedded track.
async fn migrated_bed() -> (DbPool, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!("nook293-{}.db", uuid::Uuid::now_v7()));
    let db = nook_db::connect(&format!("sqlite://{}", path.display()), 1)
        .await
        .expect("open sqlite");
    nook_control::MIGRATOR_SQLITE
        .run(db.sqlite())
        .await
        .expect("migrate the SQLite track");
    (db, path)
}

fn is_uuid_v4_text(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && [8, 4, 4, 4, 12] == parts.iter().map(|p| p.len()).collect::<Vec<_>>()[..]
        && s.chars().all(|c| c == '-' || c.is_ascii_hexdigit())
        && s.chars().all(|c| !c.is_ascii_uppercase())
        && parts[2].starts_with('4')
        && matches!(parts[3].chars().next(), Some('8' | '9' | 'a' | 'b'))
}

/// AC-1: the insert that used to fail now succeeds, with a generated id.
#[tokio::test]
async fn an_insert_omitting_person_id_gets_a_generated_uuid() {
    let (db, path) = migrated_bed().await;

    db.exec(
        "INSERT INTO tenants (id, name, slug) VALUES ('t1', 'Acme', 'acme')",
        params![],
    )
    .await
    .expect("seed a tenant");

    // Exactly the shape that produced `NOT NULL constraint failed:
    // users.person_id` before this migration: person_id is not named.
    db.exec(
        "INSERT INTO users (id, tenant_id, display_name, email) \
         VALUES ('u1', 't1', 'Ada', 'ada@example.invalid')",
        params![],
    )
    .await
    .expect("an insert that omits person_id must succeed, as it does on Postgres");

    let person: String = db
        .query_scalar("SELECT person_id FROM users WHERE id = 'u1'", params![])
        .await
        .expect("read it back");

    assert!(
        is_uuid_v4_text(&person),
        "the default must produce a lower-case hyphenated v4 uuid, matching the \
         uuid→TEXT convention Postgres rows already satisfy; got {person:?}"
    );

    let _ = std::fs::remove_file(&path);
}

/// The default is evaluated PER ROW. A constant default would satisfy the test
/// above while making `person_id` — the cross-tenant identity key — the same
/// value for every user, which is the one thing it must never be.
#[tokio::test]
async fn every_row_gets_its_own_person_id() {
    let (db, path) = migrated_bed().await;
    db.exec(
        "INSERT INTO tenants (id, name, slug) VALUES ('t1', 'Acme', 'acme')",
        params![],
    )
    .await
    .unwrap();

    for i in 0..5 {
        db.exec(
            &format!(
                "INSERT INTO users (id, tenant_id, display_name, email) \
                 VALUES ('u{i}', 't1', 'U{i}', 'u{i}@example.invalid')"
            ),
            params![],
        )
        .await
        .unwrap();
    }

    let rows = sqlx::query("SELECT person_id FROM users")
        .fetch_all(db.sqlite())
        .await
        .unwrap();
    let ids: std::collections::HashSet<String> = rows
        .iter()
        .map(|r| r.get::<String, _>("person_id"))
        .collect();
    assert_eq!(
        ids.len(),
        5,
        "each row needs its own person_id; a constant default would collapse them"
    );
    assert!(ids.iter().all(|s| is_uuid_v4_text(s)));

    let _ = std::fs::remove_file(&path);
}

/// An explicitly supplied `person_id` still wins — the default fills a gap, it
/// does not overwrite. Postgres behaves this way and the seed paths depend on it.
#[tokio::test]
async fn an_explicit_person_id_is_not_overwritten() {
    let (db, path) = migrated_bed().await;
    db.exec(
        "INSERT INTO tenants (id, name, slug) VALUES ('t1', 'Acme', 'acme')",
        params![],
    )
    .await
    .unwrap();
    db.exec(
        "INSERT INTO users (id, tenant_id, display_name, email, person_id) \
         VALUES ('u1', 't1', 'Ada', 'ada@example.invalid', 'chosen-person-id')",
        params![],
    )
    .await
    .unwrap();

    let person: String = db
        .query_scalar("SELECT person_id FROM users WHERE id = 'u1'", params![])
        .await
        .unwrap();
    assert_eq!(person, "chosen-person-id");

    let _ = std::fs::remove_file(&path);
}
