-- MAIN-542: queue ejection is a THIRD cause on MAIN-516's path — the same
-- recorder, a different reason — so the one-verdict-per-head guarantee has to
-- cover every source the control plane writes, not `conflict` alone.
--
-- The read-side check has always been "does this head already carry a
-- changes_requested?", which is source-blind; 0062's index was narrower than
-- the rule it exists to arbitrate, and two replicas recording different causes
-- for one head the same instant would both have passed it.
DROP INDEX IF EXISTS loop_jobs_one_conflict_verdict_per_head;

CREATE UNIQUE INDEX IF NOT EXISTS loop_jobs_one_control_plane_verdict_per_head
    ON loop_jobs (workspace_id, review_pr_number, review_head_sha)
    WHERE review_verdict_source IS NOT NULL;
