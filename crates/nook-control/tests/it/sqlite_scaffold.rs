//! The SQLite track's frozen `0001_init` actually builds a database (MAIN-236
//! AC-4).
//!
//! The scaffold was generated from the schema the Postgres migrations produce
//! and then hand-owned; the generator is gone. What keeps it honest from here is
//! this: an empty SQLite database migrated through the committed file must come
//! out with the expected tables and the mapped column types.
//!
//! Deliberately NOT a boot test — no MIGRATOR, no engine selection, no pool
//! plumbing (NG-1, MAIN-196 owns that). It executes the file and inspects the
//! result, which is the whole claim the file makes today.
//!
//! No Postgres required: SQLite runs in memory.

use sqlx::sqlite::SqlitePoolOptions;
use sqlx::{Row, SqlitePool};

/// The whole SQLite track. Since MAIN-294 that includes nook-chat's tables:
/// SQLite has no schemas, so one file is one namespace and one
/// `_sqlx_migrations`, and two migrators writing it collide. Chat's `chat_*`
/// tables therefore live here and chat runs no migrator on SQLite at all.
const CONTROL: &str = include_str!("../../migrations_sqlite/0001_init.sql");

/// Strip `--` comments, so a semicolon inside prose cannot look like the end of
/// a statement. (It can: a hand-correction note in the file says "…is faithful;
/// a trigger faking one would only hide a missing write", and splitting on that
/// truncated a CREATE TABLE into `incomplete input`.)
fn strip_comments(sql: &str) -> String {
    sql.lines()
        .map(|l| match l.find("--") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Apply a scaffold to a fresh in-memory database, statement by statement.
///
/// sqlx's `execute` takes one statement at a time. Splitting on `;` is wrong in
/// general, but exact for this file once comments are gone: it is generated DDL
/// with no procedural bodies and no semicolons inside string literals. A
/// statement SQLite rejects fails the test, which is the point — this proves it
/// accepts every line.
async fn apply(sql: &str) -> SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("open in-memory sqlite");

    for stmt in strip_comments(sql).split(';') {
        let stmt = stmt.trim();
        // Skip blanks and comment-only chunks.
        if stmt.is_empty() {
            continue;
        }
        sqlx::query(stmt)
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("sqlite rejected:\n{stmt}\n\nerror: {e}"));
    }
    pool
}

async fn tables(pool: &SqlitePool) -> Vec<String> {
    sqlx::query("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .fetch_all(pool)
        .await
        .expect("list tables")
        .iter()
        .map(|r| r.get::<String, _>("name"))
        .collect()
}

/// `(name, declared type, not-null)` for one table, via SQLite's own pragma.
async fn columns(pool: &SqlitePool, table: &str) -> Vec<(String, String, bool)> {
    sqlx::query(&format!("PRAGMA table_info({table})"))
        .fetch_all(pool)
        .await
        .expect("table_info")
        .iter()
        .map(|r| {
            (
                r.get::<String, _>("name"),
                r.get::<String, _>("type"),
                r.get::<i64, _>("notnull") == 1,
            )
        })
        .collect()
}

#[tokio::test]
async fn the_control_scaffold_builds_a_sqlite_database() {
    let pool = apply(CONTROL).await;
    let t = tables(&pool).await;

    // Representative tables across the whole surface, not just the first few:
    // identity, the board, the fleet, and the newest additions.
    for expected in [
        "tenants",
        "users",
        "boards",
        "board_columns",
        "tasks",
        "workspaces",
        "nodes",
        "node_workspaces",
        "sessions",
        "events",
        "loop_jobs",
        "loop_job_transcript",
        "interactions",
        "notes",
        "roles",
        "permissions",
    ] {
        assert!(
            t.contains(&expected.to_string()),
            "missing table {expected}; got {t:?}"
        );
    }
    // The Postgres schema has 50 base tables today. A floor rather than an
    // equality: a new migration adding one should not fail this test, but the
    // scaffold silently losing half the schema should.
    assert!(
        t.len() >= 50,
        "expected the whole schema (>=50 tables), got {}: {t:?}",
        t.len()
    );
}

/// The type map is the thing most likely to rot, so assert it on real columns
/// rather than trusting the generator that is no longer here to re-run.
#[tokio::test]
async fn column_types_follow_the_documented_map() {
    let pool = apply(CONTROL).await;
    let cols = columns(&pool, "tasks").await;
    let ty = |name: &str| {
        cols.iter()
            .find(|(n, _, _)| n == name)
            .unwrap_or_else(|| panic!("tasks has no column {name}; got {cols:?}"))
            .clone()
    };

    // uuid -> TEXT
    assert_eq!(ty("id").1, "TEXT");
    assert_eq!(ty("tenant_id").1, "TEXT");
    // timestamptz -> TEXT
    assert_eq!(ty("created_at").1, "TEXT");
    // bigint/integer -> INTEGER
    assert_eq!(ty("number").1, "INTEGER");
    // NOT NULL survives the mapping — it is a real constraint, not decoration.
    assert!(ty("id").2, "tasks.id stays NOT NULL");

    // jsonb -> TEXT, checked where one actually lives.
    let boards = columns(&pool, "boards").await;
    let automation = boards
        .iter()
        .find(|(n, _, _)| n == "automation")
        .expect("boards.automation");
    assert_eq!(automation.1, "TEXT");
}

/// `now()` became `CURRENT_TIMESTAMP`, and `::type` casts are gone — the two
/// mechanical rewrites the audit's type map calls for. Checked against the file
/// text as well as the built database, because a cast that survived would be a
/// syntax error SQLite reports only when that statement runs.
#[tokio::test]
async fn mechanical_rewrites_left_no_postgres_isms() {
    for (name, sql) in [("control", CONTROL)] {
        // Only the SQL matters — the header comment names the constructs it
        // rewrote, and matching on that would be checking the documentation.
        let code = strip_comments(sql);
        for pgism in [
            "now()",
            "::",
            "USING btree",
            "jsonb",
            "timestamptz",
            "ANY (ARRAY",
            "gen_random_uuid",
        ] {
            assert!(
                !code.contains(pgism),
                "{name}: {pgism:?} survived into the SQLite scaffold"
            );
        }
    }
}

/// Chat's tables come out of the SAME file as the control plane's (MAIN-294).
///
/// This used to apply nook-chat's own `0001_chat_init.sql`. That file is gone:
/// on SQLite there is one track, because one file cannot carry two ledgers. The
/// claim worth keeping is that chat's tables are still built — now from here —
/// and that the merge collided with nothing, which the `chat_` prefix is what
/// makes true.
#[tokio::test]
async fn the_chat_tables_are_built_by_the_one_sqlite_track() {
    let pool = apply(CONTROL).await;
    let t = tables(&pool).await;
    for expected in [
        "chat_channels",
        "chat_messages",
        "chat_channel_members",
        "chat_channel_categories",
        "chat_channel_participants",
        "chat_message_revisions",
        "chat_reactions",
        "chat_read_cursors",
    ] {
        assert!(
            t.contains(&expected.to_string()),
            "missing {expected}; got {t:?}"
        );
    }
    // The merge is only safe because nothing collided: every chat table is
    // `chat_`-prefixed, so the control plane's names are untouched.
    for control_table in ["tenants", "users", "tasks"] {
        assert!(t.contains(&control_table.to_string()));
    }
}

/// The seeded role model has to come across too: a SQLite boot with an empty
/// `roles`/`permissions` table would have no authorization model at all.
#[tokio::test]
async fn the_scaffold_carries_the_seed_rows() {
    let pool = apply(CONTROL).await;
    for (table, least) in [("roles", 1), ("permissions", 1), ("role_permissions", 1)] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&pool)
            .await
            .expect("count");
        assert!(n >= least, "{table} came across empty");
    }
}

/// Seed VALUES land in the right COLUMNS — not merely the right number of rows.
///
/// Counting rows missed a real defect: the scaffold paired an ordinal column
/// list with values ordered by `jsonb_each_text`, whose key order is its own, so
/// `roles.description` and `roles.builtin` were swapped on all four rows.
/// SQLite's type affinity swallowed prose into an INTEGER column without a
/// murmur, which is exactly the silent divergence this whole ticket exists to
/// prevent — and it landed in the authorization model's seed data.
///
/// So this asserts the values, and does it through a query that would have
/// FAILED before: `builtin = 1` matched nothing when the column held prose.
#[tokio::test]
async fn seed_values_are_in_the_right_columns() {
    let pool = apply(CONTROL).await;

    // The predicate the swap broke.
    let builtin: i64 = sqlx::query_scalar("SELECT count(*) FROM roles WHERE builtin = 1")
        .fetch_one(&pool)
        .await
        .expect("count builtin roles");
    assert_eq!(
        builtin, 4,
        "every seeded role is builtin; a swap makes this 0"
    );

    let (desc, flag): (String, i64) =
        sqlx::query_as("SELECT description, builtin FROM roles WHERE key = 'operator'")
            .fetch_one(&pool)
            .await
            .expect("the operator role");
    assert_eq!(flag, 1);
    assert_ne!(
        desc, "true",
        "description holds prose, not the builtin flag"
    );
    assert!(
        desc.starts_with("Runs this deployment"),
        "the operator description came across verbatim, got {desc:?}"
    );

    // Same class, other tables: a permission's key and description must not
    // trade places either.
    let (pkey, pdesc): (String, String) =
        sqlx::query_as("SELECT key, description FROM permissions ORDER BY key LIMIT 1")
            .fetch_one(&pool)
            .await
            .expect("a permission");
    assert!(
        pkey.contains('.'),
        "permission keys look like `org.view`, got {pkey:?}"
    );
    assert!(
        !pdesc.contains('.') || pdesc.len() > pkey.len(),
        "description is prose, got {pdesc:?}"
    );
}

/// A frozen migration must not carry the wall clock of the machine that
/// generated it — every fresh database would claim the Default org was created
/// at that instant. Postgres fills these from the column default; SQLite should
/// too.
#[tokio::test]
async fn seed_timestamps_are_not_baked_in() {
    assert!(
        !regex_like_timestamp(CONTROL),
        "a literal timestamp is baked into the control seeds; use CURRENT_TIMESTAMP"
    );

    // And it actually stamps something on insert.
    let pool = apply(CONTROL).await;
    let created: Option<String> =
        sqlx::query_scalar("SELECT created_at FROM orgs WHERE slug = 'default'")
            .fetch_one(&pool)
            .await
            .expect("the default org");
    let created = created.expect("created_at is filled");
    assert!(!created.is_empty(), "created_at was stamped on insert");
}

/// `YYYY-MM-DD HH:MM:SS`-shaped literal anywhere in the seed section. Hand-rolled
/// rather than pulling in a regex dependency for one check.
fn regex_like_timestamp(sql: &str) -> bool {
    sql.lines()
        .filter(|l| l.trim_start().starts_with("INSERT INTO "))
        .any(|l| {
            l.as_bytes().windows(11).any(|w| {
                w[0] == b'\''
                    && w[1..5].iter().all(u8::is_ascii_digit)
                    && w[5] == b'-'
                    && w[6..8].iter().all(u8::is_ascii_digit)
                    && w[8] == b'-'
                    && w[9..11].iter().all(u8::is_ascii_digit)
            })
        })
}
