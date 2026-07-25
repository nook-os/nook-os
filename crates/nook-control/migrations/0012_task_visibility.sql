-- Per-task visibility + creator (MAIN-76). Tasks gain an owner and a
-- visibility so a card can be private to its creator, shared with the tenant
-- (the default, reproducing today's behaviour), or org-wide.
--
-- Append-only and idempotent. Default 'team' means every existing card, and
-- every card created before a client sends a value, behaves exactly as before —
-- visible to the whole tenant.

ALTER TABLE public.tasks
    ADD COLUMN IF NOT EXISTS visibility text NOT NULL DEFAULT 'team';

-- Add the CHECK separately + guarded, so re-running does not error on an
-- already-constrained column.
ALTER TABLE public.tasks
    DROP CONSTRAINT IF EXISTS tasks_visibility_check;
ALTER TABLE public.tasks
    ADD CONSTRAINT tasks_visibility_check
    CHECK (visibility = ANY (ARRAY['private'::text, 'team'::text, 'org'::text]));

-- The per-tenant users.id of the creator. NULL for rows that predate this
-- (their owner is unknown, so they are treated as ownerless team cards).
ALTER TABLE public.tasks
    ADD COLUMN IF NOT EXISTS created_by uuid;

-- The read predicate filters private cards by creator/assignee; an index on the
-- owning column keeps that cheap on a busy board.
CREATE INDEX IF NOT EXISTS tasks_created_by_idx ON public.tasks (created_by);
