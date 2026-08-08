-- SQLite twin of 0046 (hand-authored, per CLAUDE.md).
--
-- No table rebuild: SQLite supports ADD COLUMN for a nullable column and
-- supports partial indexes. `bigint` and `text` map straight across;
-- `IF NOT EXISTS` is not valid on ADD COLUMN, so it is omitted.
ALTER TABLE loop_jobs ADD COLUMN review_pr_number bigint;
ALTER TABLE loop_jobs ADD COLUMN review_head_sha text;

CREATE UNIQUE INDEX IF NOT EXISTS loop_jobs_one_live_run_per_pr
    ON loop_jobs (workspace_id, review_pr_number)
    WHERE workspace_id IS NOT NULL
      AND review_pr_number IS NOT NULL
      AND state IN ('queued', 'claimed', 'running', 'waiting_on_human');
