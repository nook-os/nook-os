-- MAIN-144: the SQLite twin. The kind CHECK is an inline table constraint, and
-- 0040's header documents at length why the copy-rebuild is DESTRUCTIVE here
-- (sqlx pins foreign_keys ON inside its transaction; the implicit DELETE
-- cascades into transcripts). Same answer as 0040: rewrite the declared schema
-- in place — constraint-only change, same columns, same on-disk format, no row
-- moves.
--
-- The statement below is the CURRENT stored text, read from a database built
-- by 0001..0048 (the ADD COLUMNs of 0046/0047 splice into the column list, so
-- reconstructing from 0040's text would silently drop them from the declared
-- schema). Exactly one edit: the kind CHECK gains 'epic-run'.

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
  seed TEXT, review_pr_number bigint, review_head_sha text, review_verdict text,
  PRIMARY KEY (id),
  CHECK ((kind IN (''spec'', ''decompose'', ''review'', ''epic-run''))),
  CHECK ((state IN (''queued'', ''claimed'', ''running'', ''waiting_on_human'', ''completed'', ''failed'', ''canceled''))),
  CHECK ((kind = ''review'' AND workspace_id IS NOT NULL) OR (kind <> ''review'' AND target_task_id IS NOT NULL)),
  FOREIGN KEY (predecessor_job_id) REFERENCES loop_jobs (id) ON DELETE SET NULL,
  FOREIGN KEY (target_task_id) REFERENCES tasks (id) ON DELETE CASCADE
)'
WHERE type = 'table' AND name = 'loop_jobs';

PRAGMA writable_schema = RESET;

-- Bump schema_version so no connection keeps the cached constraints (0040's
-- own note); ordinary DDL does it, this scratch table only exists for that.
CREATE TABLE loop_jobs_schema_bump_144 (x);
DROP TABLE loop_jobs_schema_bump_144;
