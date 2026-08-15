-- MAIN-329: the `investigate` job kind — a READ-ONLY pass over one card.
--
-- Same shape as 0040's, 0049's and 0050's relaxations: the kind CHECK is dropped
-- and re-added with the new member. An `investigate` row targets a task, so the
-- 0040 target CHECK (which requires `target_task_id` for every non-review kind)
-- already covers it and nothing else about the row shape changes.
--
-- The inbound-email pipeline seeds one of these per accepted support mail. It
-- writes nothing and opens no PR: no node advertises the kind and no skill maps
-- it yet, so the row is a queued brief a later card gives an executor.
--
-- Idempotent: the constraint is dropped before it is added.

ALTER TABLE loop_jobs
    DROP CONSTRAINT IF EXISTS loop_jobs_kind_check;
ALTER TABLE loop_jobs
    ADD CONSTRAINT loop_jobs_kind_check
    CHECK (kind IN ('spec', 'decompose', 'review', 'epic-run', 'build', 'investigate'));
