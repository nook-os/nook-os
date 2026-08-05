-- MAIN-405: the SQLite twin of `migrations/0040_loop_jobs_review_target.sql`.
-- Hand-authored, per CLAUDE.md — nothing regenerates over this track.
--
-- Postgres drops a NOT NULL and swaps two CHECKs. SQLite can do NEITHER in
-- place: both are inline table constraints and `ALTER TABLE` cannot touch them.
--
-- ## Why this is a schema rewrite and NOT the usual create/copy/drop/rename
--
-- The copy-and-rename rebuild is SQLite's documented answer, and its very first
-- step is `PRAGMA foreign_keys = OFF`. **That step is unavailable to us.** sqlx's
-- SQLite migrator always wraps a migration in a transaction — it ignores the
-- `-- no-transaction` directive entirely, which only the Postgres driver reads —
-- and `PRAGMA foreign_keys` is a silent no-op inside a transaction. Measured, on
-- the migrator's own connection: set it to OFF mid-transaction and it still
-- reads 1.
--
-- With enforcement stuck ON, both halves of the rebuild destroy data, silently
-- and without an error:
--
--   * `DROP TABLE loop_jobs` performs an implicit `DELETE FROM`, which fires
--     `ON DELETE CASCADE` into `loop_job_transcript` and `interactions` and
--     `ON DELETE SET NULL` into the fresh copy's own `predecessor_job_id`.
--     Measured: transcript and interaction rows go to zero and every job's
--     predecessor becomes NULL.
--   * `ALTER TABLE loop_jobs RENAME TO …` rewrites the `REFERENCES` clauses of
--     every table that names it, so the children end up permanently pointing at
--     the discarded copy. `PRAGMA legacy_alter_table` does NOT prevent this —
--     it covers trigger and view bodies, not foreign keys. Measured both ways.
--
-- So this migration changes the DECLARED schema and never moves a row. Rewriting
-- `sqlite_master.sql` is SQLite's own alternative for exactly this case, and it
-- is only safe because the change is constraint-only: same columns, same order,
-- same types, same on-disk format. Nothing is copied, nothing is dropped, and
-- nothing is renamed — so "preserves every row, both foreign keys and the
-- self-reference" is true by construction rather than by careful copying.
--
-- The statement below is `0001_init.sql`'s `loop_jobs` verbatim, with exactly
-- three edits: `target_task_id` loses NOT NULL, the kind CHECK gains `review`,
-- and a target CHECK is added so relaxing NOT NULL cannot quietly permit a
-- `spec` job with no ticket.

PRAGMA writable_schema = ON;

UPDATE sqlite_master
SET sql = 'CREATE TABLE loop_jobs (
  id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  target_task_id TEXT,
  workspace_id TEXT,
  requested_by TEXT NOT NULL,
  state TEXT NOT NULL DEFAULT ''queued'',
  executor_node_id TEXT,
  predecessor_job_id TEXT,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  queued_reason TEXT,
  seed TEXT,
  PRIMARY KEY (id),
  CHECK ((kind IN (''spec'', ''decompose'', ''review''))),
  CHECK ((state IN (''queued'', ''claimed'', ''running'', ''waiting_on_human'', ''completed'', ''failed'', ''canceled''))),
  CHECK ((kind = ''review'' AND workspace_id IS NOT NULL) OR (kind <> ''review'' AND target_task_id IS NOT NULL)),
  FOREIGN KEY (predecessor_job_id) REFERENCES loop_jobs (id) ON DELETE SET NULL,
  FOREIGN KEY (target_task_id) REFERENCES tasks (id) ON DELETE CASCADE
)'
WHERE type = 'table' AND name = 'loop_jobs';

-- Reloads the schema on THIS connection and clears the writable flag in one
-- step, so nothing downstream inherits a database that can be edited by
-- accident.
PRAGMA writable_schema = RESET;

-- A `sqlite_master` write does not bump `schema_version`, so a connection that
-- already cached the schema would keep the old constraints. Any ordinary DDL
-- bumps it; this scratch table exists only to do that and leaves nothing behind.
CREATE TABLE loop_jobs_schema_bump_405 (x);
DROP TABLE loop_jobs_schema_bump_405;
