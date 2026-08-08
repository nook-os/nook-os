-- MAIN-383: the `build` job kind — a builder pass pointed at one ticket.
--
-- Same shape as 0040's and 0049's relaxations: the kind CHECK is dropped and re-added with
-- the new member. `build` rows target a task (the 0040 target CHECK already
-- requires `target_task_id` for every non-review kind), so nothing else about
-- the row shape changes.
--
-- Idempotent: the constraint is dropped before it is added.

ALTER TABLE loop_jobs
    DROP CONSTRAINT IF EXISTS loop_jobs_kind_check;
ALTER TABLE loop_jobs
    ADD CONSTRAINT loop_jobs_kind_check
    CHECK (kind IN ('spec', 'decompose', 'review', 'epic-run', 'build'));

-- One live build run per card (AC-4), the same rule 0046 states per PR for
-- reviews. Partial, on the live states only: a completed or failed run must
-- never block the next enqueue.
CREATE UNIQUE INDEX IF NOT EXISTS loop_jobs_one_live_build_per_task
    ON loop_jobs (target_task_id)
    WHERE kind = 'build'
      AND state IN ('queued', 'claimed', 'running', 'waiting_on_human');
