-- What a managed review RUN is about: one pull request, at one head.
--
-- The reconciler still owns the decision (MAIN-445's ceiling, MAIN-448's forge
-- sizing). What changes is the UNIT. Until now the forge could only say how
-- many PRs a repo had, so reviewers were dealt arithmetic slices of the repo
-- (`number % shards == shard`) and woken on a timer. With the forge yielding
-- the PRs themselves, the PR IS the unit: a run is raised for a specific pull
-- request at a specific head, and the arithmetic partition has nothing left to
-- do.
--
-- `review_head_sha` is what makes a wakeup honest. A run is owed for a PR whose
-- head has moved since the last completed run for it, and owed for nothing
-- otherwise — so a quiet repo costs no agents at all, and no timer decides when
-- to look again.
--
-- Both nullable: every other kind (`spec`, `decompose`) is about a ticket and
-- has no PR, and a null here is the honest statement of that rather than a
-- sentinel number.
ALTER TABLE loop_jobs ADD COLUMN IF NOT EXISTS review_pr_number bigint;
ALTER TABLE loop_jobs ADD COLUMN IF NOT EXISTS review_head_sha text;

-- One live run per pull request. The sweep's rule was "any active review for
-- this workspace", which could not tell two PRs apart — so a repo with ten open
-- PRs still got one reviewer, and which PR it picked was the skill's guess.
--
-- Partial, on the live states only: a completed run must never block the next
-- one, which is how a new push gets reviewed.
CREATE UNIQUE INDEX IF NOT EXISTS loop_jobs_one_live_run_per_pr
    ON loop_jobs (workspace_id, review_pr_number)
    WHERE workspace_id IS NOT NULL
      AND review_pr_number IS NOT NULL
      AND state IN ('queued', 'claimed', 'running', 'waiting_on_human');
