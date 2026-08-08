-- A managed review RUN is a job, not a tmux session.
--
-- The reconciler still owns the decision — how many reviewers a repo gets
-- (MAIN-445), how they shard its PRs (MAIN-446), how open-PR count sizes them
-- (MAIN-448). Only the artifact changes: it converges JOBS instead of sessions,
-- because a job is already headless (`claude -p --output-format stream-json`)
-- and already keeps the durable transcript a spec run is read through. A
-- session is a TUI on a machine, driven by typing `/loop /nook-review` into it,
-- with nothing to read afterwards.
--
-- So the shard pair lives here too. `review_shards` defaults to 1, which is
-- what `spec` and `decompose` are: one runner owning the whole of its work.
ALTER TABLE loop_jobs ADD COLUMN IF NOT EXISTS review_shard integer NOT NULL DEFAULT 0;
ALTER TABLE loop_jobs ADD COLUMN IF NOT EXISTS review_shards integer NOT NULL DEFAULT 1;

-- One live run per (workspace, shard). The sweep's rule was "any active review
-- for this workspace", which cannot tell two shards apart — so a repo declaring
-- three reviewers still got one.
CREATE UNIQUE INDEX IF NOT EXISTS loop_jobs_one_live_per_workspace_shard
    ON loop_jobs (workspace_id, review_shard)
    WHERE workspace_id IS NOT NULL
      AND kind = 'review'
      AND state IN ('queued', 'claimed', 'running', 'waiting_on_human');
