//! MAIN-405: a loop job can target a workspace instead of a task.
//!
//! Two claims, proved separately because they fail differently.
//!
//! The FIRST is behavioural and engine-agnostic: whichever engine is under the
//! `TestBed`, a `review` job with no ticket is accepted, and relaxing NOT NULL
//! has not quietly made a ticket optional for the kinds that need one.
//!
//! The SECOND is the dangerous one. On SQLite the change is a rewrite of the
//! declared schema, because the ordinary create/copy/drop/rename rebuild
//! CANNOT be done from inside sqlx's migration transaction without deleting
//! rows — see the migration's own comment. So this test migrates a POPULATED
//! database, the way the migrator does, and asserts nothing moved.

use nook_db::{params, Db};
use nook_testkit::TestBed;
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

// ── the behaviour, on whichever engine is under the bed ─────────────────────

/// A tenant, a workspace, and a board/column/task to hang a task-targeted job
/// on. `requested_by` has no foreign key, so any uuid stands in for a user.
async fn fixture(bed: &TestBed) -> (Uuid, Uuid, Uuid) {
    let tenant = bed.tenant("m405").await;
    let workspace = bed.workspace(tenant).await;
    let (board, column, task) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    bed.db()
        .exec(
            "INSERT INTO boards (id, tenant_id, name) VALUES ($1, $2, 'B')",
            params![board, tenant],
        )
        .await
        .expect("board");
    bed.db()
        .exec(
            "INSERT INTO board_columns (id, board_id, name) VALUES ($1, $2, 'Todo')",
            params![column, board],
        )
        .await
        .expect("column");
    bed.db()
        .exec(
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title)
             VALUES ($1, $2, $3, $4, 'T')",
            params![task, tenant, board, column],
        )
        .await
        .expect("task");
    (tenant.0, workspace.0, task)
}

#[tokio::test]
async fn a_review_job_names_a_workspace_and_no_task() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (tenant, workspace, _task) = fixture(&bed).await;

    let id = Uuid::now_v7();
    bed.db()
        .exec(
            "INSERT INTO loop_jobs (id, tenant_id, kind, workspace_id, requested_by)
             VALUES ($1, $2, 'review', $3, $4)",
            params![id, tenant, workspace, Uuid::now_v7()],
        )
        .await
        .expect("a review job needs no ticket");

    let (target, ws): (Option<Uuid>, Option<Uuid>) = bed
        .db()
        .query_one(
            "SELECT target_task_id, workspace_id FROM loop_jobs WHERE id = $1",
            params![id],
        )
        .await
        .expect("read it back");
    assert_eq!(target, None, "no ticket was borrowed");
    assert_eq!(ws, Some(workspace), "the workspace IS the target");

    bed.teardown().await;
}

#[tokio::test]
async fn a_task_targeted_job_still_needs_its_task() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (tenant, workspace, task) = fixture(&bed).await;

    // Unchanged: the kinds that are about a ticket still carry one (AC-4).
    bed.db()
        .exec(
            "INSERT INTO loop_jobs (id, tenant_id, kind, target_task_id, workspace_id, requested_by)
             VALUES ($1, $2, 'spec', $3, $4, $5)",
            params![Uuid::now_v7(), tenant, task, workspace, Uuid::now_v7()],
        )
        .await
        .expect("a spec job with a ticket is what it always was");

    // Relaxing NOT NULL must not make the ticket optional for those kinds — a
    // spec job with nothing to fill in is unexecutable, and the database is
    // where that is now said.
    assert!(
        bed.db()
            .exec(
                "INSERT INTO loop_jobs (id, tenant_id, kind, workspace_id, requested_by)
                 VALUES ($1, $2, 'spec', $3, $4)",
                params![Uuid::now_v7(), tenant, workspace, Uuid::now_v7()],
            )
            .await
            .is_err(),
        "a spec job with no ticket must be refused"
    );

    // And the mirror: a review job with no workspace has no target at all.
    assert!(
        bed.db()
            .exec(
                "INSERT INTO loop_jobs (id, tenant_id, kind, requested_by)
                 VALUES ($1, $2, 'review', $3)",
                params![Uuid::now_v7(), tenant, Uuid::now_v7()],
            )
            .await
            .is_err(),
        "a review job with no workspace must be refused"
    );

    assert!(
        bed.db()
            .exec(
                "INSERT INTO loop_jobs (id, tenant_id, kind, target_task_id, requested_by)
                 VALUES ($1, $2, 'nonsense', $3, $4)",
                params![Uuid::now_v7(), tenant, task, Uuid::now_v7()],
            )
            .await
            .is_err(),
        "the kind check still refuses a kind nothing runs"
    );

    bed.teardown().await;
}

// ── the SQLite rewrite, on a populated database ─────────────────────────────

/// The version this card adds. Everything below it builds the "before" state.
const THIS_MIGRATION: i64 = 40;

async fn sqlite_at_previous_version() -> SqlitePool {
    let opts = "sqlite::memory:"
        .parse::<sqlx::sqlite::SqliteConnectOptions>()
        .expect("options")
        // As `nook_db::connect` opens it. Enforcement being ON is the entire
        // reason this migration is shaped the way it is.
        .foreign_keys(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .expect("open sqlite");

    for m in nook_control::MIGRATOR_SQLITE.iter() {
        if m.version >= THIS_MIGRATION {
            continue;
        }
        sqlx::raw_sql(&m.sql)
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("migration {} failed: {e}", m.version));
    }
    pool
}

/// Two jobs (one the predecessor of the other), a transcript line and an
/// interaction — i.e. one row on each side of every foreign key the rebuild
/// could break.
async fn populate(pool: &SqlitePool) {
    for stmt in [
        "INSERT INTO tenants (id, name, slug) VALUES ('t1', 'T', 't1')",
        "INSERT INTO workspaces (id, tenant_id, name, slug) VALUES ('w1', 't1', 'W', 'w1')",
        "INSERT INTO boards (id, tenant_id, name) VALUES ('b1', 't1', 'B')",
        "INSERT INTO board_columns (id, board_id, name) VALUES ('c1', 'b1', 'Todo')",
        "INSERT INTO tasks (id, tenant_id, board_id, column_id, title)
         VALUES ('k1', 't1', 'b1', 'c1', 'Task')",
        "INSERT INTO users (id, tenant_id, display_name, email)
         VALUES ('u1', 't1', 'U', 'u@example.test')",
        // Ids chosen so the CHILD sorts first: a copy that re-inserted rows
        // would meet the self-reference before its parent existed.
        "INSERT INTO loop_jobs (id, tenant_id, kind, target_task_id, workspace_id, requested_by, seed)
         VALUES ('zz-parent', 't1', 'spec', 'k1', 'w1', 'u1', 'the seed')",
        "INSERT INTO loop_jobs (id, tenant_id, kind, target_task_id, requested_by, predecessor_job_id)
         VALUES ('aa-child', 't1', 'decompose', 'k1', 'u1', 'zz-parent')",
        "INSERT INTO loop_job_transcript (id, job_id, content) VALUES ('x1', 'zz-parent', 'hello')",
        "INSERT INTO interactions (id, tenant_id, prompt, job_id) VALUES ('i1', 't1', 'p?', 'zz-parent')",
    ] {
        sqlx::query(stmt)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("seed failed: {stmt}\n{e}"));
    }
}

async fn table_names(pool: &SqlitePool) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT name FROM sqlite_master
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .expect("list tables")
}

async fn row_counts(pool: &SqlitePool) -> Vec<(String, i64)> {
    let mut out = Vec::new();
    for t in table_names(pool).await {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {t}"))
            .fetch_one(pool)
            .await
            .unwrap_or_else(|e| panic!("count {t}: {e}"));
        out.push((t, n));
    }
    out
}

/// The parent table each foreign key of `table` points at.
async fn fk_parents(pool: &SqlitePool, table: &str) -> Vec<String> {
    let rows = sqlx::query(&format!("PRAGMA foreign_key_list({table})"))
        .fetch_all(pool)
        .await
        .expect("fk list");
    let mut names: Vec<String> = rows.iter().map(|r| r.get::<String, _>("table")).collect();
    names.sort();
    names
}

#[tokio::test]
async fn the_sqlite_change_moves_no_rows() {
    let pool = sqlite_at_previous_version().await;
    populate(&pool).await;

    let before = row_counts(&pool).await;

    let m = nook_control::MIGRATOR_SQLITE
        .iter()
        .find(|m| m.version == THIS_MIGRATION)
        .expect("the migration this card adds");

    // Exactly how sqlx's SQLite migrator applies it: inside a transaction,
    // always. It has no `no_tx` path, which is why the migration cannot turn
    // foreign keys off and therefore cannot copy/drop/rename.
    let mut tx = pool.begin().await.expect("begin");
    sqlx::raw_sql(&m.sql)
        .execute(&mut *tx)
        .await
        .expect("the migration applies");
    tx.commit().await.expect("commit");

    assert_eq!(
        before,
        row_counts(&pool).await,
        "every table has the same number of rows it started with"
    );

    let violations = sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(&pool)
        .await
        .expect("fk check");
    assert!(
        violations.is_empty(),
        "{} dangling references after the change",
        violations.len()
    );

    // The self-reference: `aa-child` still points at the job it replaces, and
    // the payload columns came through untouched.
    let (predecessor, seed): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT predecessor_job_id, seed FROM loop_jobs WHERE id = 'aa-child'")
            .fetch_one(&pool)
            .await
            .expect("re-read the child job");
    assert_eq!(predecessor.as_deref(), Some("zz-parent"));
    assert_eq!(seed, None);
    let (seed,): (Option<String>,) =
        sqlx::query_as("SELECT seed FROM loop_jobs WHERE id = 'zz-parent'")
            .fetch_one(&pool)
            .await
            .expect("re-read the parent job");
    assert_eq!(seed.as_deref(), Some("the seed"));

    // Both of `loop_jobs`' own foreign keys survive, and — the failure a rename
    // causes — the tables that reference it still name IT and not a discarded
    // copy.
    assert_eq!(fk_parents(&pool, "loop_jobs").await, ["loop_jobs", "tasks"]);
    assert_eq!(
        fk_parents(&pool, "loop_job_transcript").await,
        ["loop_jobs"]
    );
    assert_eq!(
        fk_parents(&pool, "interactions").await,
        ["loop_jobs", "tasks"]
    );

    // Never dropped, so never recreated.
    let mut indexes: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'loop_jobs'",
    )
    .fetch_all(&pool)
    .await
    .expect("indexes");
    indexes.sort();
    assert!(
        indexes.iter().any(|i| i == "loop_jobs_state_idx")
            && indexes.iter().any(|i| i == "loop_jobs_tenant_idx"),
        "0001's indexes are still there: {indexes:?}"
    );

    // And the point of the whole exercise.
    let notnull: Vec<i64> = sqlx::query("PRAGMA table_info(loop_jobs)")
        .fetch_all(&pool)
        .await
        .expect("table info")
        .iter()
        .filter(|r| r.get::<String, _>("name") == "target_task_id")
        .map(|r| r.get::<i64, _>("notnull"))
        .collect();
    assert_eq!(notnull, [0], "target_task_id is nullable");

    // The scratch table that bumps `schema_version` left nothing behind.
    assert!(
        !table_names(&pool).await.iter().any(|t| t.contains("_405")),
        "the migration cleaned up after itself"
    );
}
