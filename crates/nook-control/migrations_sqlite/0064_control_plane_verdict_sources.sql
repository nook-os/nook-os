-- SQLite twin of 0064 (hand-authored, per CLAUDE.md). Partial indexes and
-- `DROP INDEX IF EXISTS` are both this engine's own syntax, so the two halves
-- are identical here.
DROP INDEX IF EXISTS loop_jobs_one_conflict_verdict_per_head;

CREATE UNIQUE INDEX IF NOT EXISTS loop_jobs_one_control_plane_verdict_per_head
    ON loop_jobs (workspace_id, review_pr_number, review_head_sha)
    WHERE review_verdict_source IS NOT NULL;
