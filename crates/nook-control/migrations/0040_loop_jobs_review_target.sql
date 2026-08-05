-- MAIN-405: a loop job can target a WORKSPACE instead of a task, so a `review`
-- job has somewhere to point.
--
-- `loop_jobs.target_task_id` was `NOT NULL REFERENCES tasks(id)` because every
-- kind so far — `spec`, `decompose` — is about one ticket. A review job is
-- about a repository, and borrowing a task id it does not have would put a lie
-- in the column that the executor then has to work around.
--
-- `workspace_id` already exists and is already nullable (0020 added it as "the
-- workspace the work happens in, derived from the target task"), so the review
-- target is that column rather than a new one. What changes is which of the two
-- a row is REQUIRED to carry, and that is now stated as a constraint instead of
-- left to the service layer: a review job needs a workspace, everything else
-- needs a task. Without it, dropping NOT NULL would quietly permit a `spec` job
-- with no ticket, which nothing downstream can execute.
--
-- Idempotent: DROP NOT NULL on an already-nullable column is a no-op, and each
-- constraint is dropped before it is added.
--
-- The SQLite twin is `migrations_sqlite/0040_loop_jobs_review_target.sql`, in
-- this commit. It cannot be a set of ALTERs — see the comment there.

ALTER TABLE loop_jobs
    ALTER COLUMN target_task_id DROP NOT NULL;

ALTER TABLE loop_jobs
    DROP CONSTRAINT IF EXISTS loop_jobs_kind_check;
ALTER TABLE loop_jobs
    ADD CONSTRAINT loop_jobs_kind_check
    CHECK (kind IN ('spec', 'decompose', 'review'));

ALTER TABLE loop_jobs
    DROP CONSTRAINT IF EXISTS loop_jobs_target_check;
ALTER TABLE loop_jobs
    ADD CONSTRAINT loop_jobs_target_check
    CHECK (
        (kind = 'review' AND workspace_id IS NOT NULL)
        OR (kind <> 'review' AND target_task_id IS NOT NULL)
    );
