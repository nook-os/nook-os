-- SQLite twin of 0050 (hand-authored, per CLAUDE.md).
--
-- The kind CHECK is an inline table constraint, which ALTER TABLE cannot touch
-- and the copy-rebuild would destroy data over (sqlx pins foreign_keys ON
-- inside its transaction — measured in 0040, whose comment carries the full
-- account). So, as in 0040 and 0049: rewrite the DECLARED schema only. The
-- statement below is the CURRENT stored text — dumped from a database built by
-- 0001..0049 — with exactly one edit: the kind CHECK gains 'build'.

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
  CHECK ((kind IN (''spec'', ''decompose'', ''review'', ''epic-run'', ''build''))),
  CHECK ((state IN (''queued'', ''claimed'', ''running'', ''waiting_on_human'', ''completed'', ''failed'', ''canceled''))),
  CHECK ((kind = ''review'' AND workspace_id IS NOT NULL) OR (kind <> ''review'' AND target_task_id IS NOT NULL)),
  FOREIGN KEY (predecessor_job_id) REFERENCES loop_jobs (id) ON DELETE SET NULL,
  FOREIGN KEY (target_task_id) REFERENCES tasks (id) ON DELETE CASCADE
)'
WHERE type = 'table' AND name = 'loop_jobs';

PRAGMA writable_schema = RESET;

-- A `sqlite_master` write does not bump `schema_version`; ordinary DDL does.
CREATE TABLE loop_jobs_schema_bump_383 (x);
DROP TABLE loop_jobs_schema_bump_383;

-- One live build run per card (AC-4) — partial indexes work the same here.
CREATE UNIQUE INDEX IF NOT EXISTS loop_jobs_one_live_build_per_task
    ON loop_jobs (target_task_id)
    WHERE kind = 'build'
      AND state IN ('queued', 'claimed', 'running', 'waiting_on_human');
