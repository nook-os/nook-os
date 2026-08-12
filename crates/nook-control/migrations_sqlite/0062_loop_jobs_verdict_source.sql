-- SQLite twin of 0060 (hand-authored, per CLAUDE.md). Nullable text ADD COLUMN
-- needs no rebuild; `IF NOT EXISTS` is not valid on ADD COLUMN here.
ALTER TABLE loop_jobs ADD COLUMN review_verdict_source TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS loop_jobs_one_conflict_verdict_per_head
    ON loop_jobs (workspace_id, review_pr_number, review_head_sha)
    WHERE review_verdict_source = 'conflict';
