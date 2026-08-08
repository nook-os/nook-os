-- The epic-run kind (MAIN-144): one manually-enqueued epic-runner pass,
-- targeting an epic task. Task-targeted, so the 0040 target CHECK's
-- "everything but review needs a task" arm already covers it.
ALTER TABLE loop_jobs
    DROP CONSTRAINT IF EXISTS loop_jobs_kind_check;
ALTER TABLE loop_jobs
    ADD CONSTRAINT loop_jobs_kind_check
    CHECK (kind IN ('spec', 'decompose', 'review', 'epic-run'));
