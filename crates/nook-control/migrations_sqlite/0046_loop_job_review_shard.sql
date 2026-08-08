-- SQLite twin of 0046 (hand-authored, per CLAUDE.md).
--
-- No table rebuild needed, unlike 0045: SQLite supports ADD COLUMN with a
-- non-null default, and supports partial indexes. `integer` maps straight
-- across. `IF NOT EXISTS` is not valid on ADD COLUMN here, so it is omitted —
-- this migration only ever runs once against a given database.
ALTER TABLE loop_jobs ADD COLUMN review_shard integer NOT NULL DEFAULT 0;
ALTER TABLE loop_jobs ADD COLUMN review_shards integer NOT NULL DEFAULT 1;

CREATE UNIQUE INDEX IF NOT EXISTS loop_jobs_one_live_per_workspace_shard
    ON loop_jobs (workspace_id, review_shard)
    WHERE workspace_id IS NOT NULL
      AND kind = 'review'
      AND state IN ('queued', 'claimed', 'running', 'waiting_on_human');
