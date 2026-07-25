-- First-class epics (MAIN-81): a task may hang off an epic via `parent_task_id`.
-- Builds on the `type='epic'` value (MAIN-59) — the parent must be an epic, and
-- an epic may not itself have a parent (no nesting, so no cycles), both enforced
-- in the service layer.
--
-- Append-only and idempotent. Deleting an epic ORPHANS its children (the FK
-- nulls the link) rather than cascading — a child ticket outlives the epic it
-- was filed under.

ALTER TABLE public.tasks
    ADD COLUMN IF NOT EXISTS parent_task_id uuid
    REFERENCES public.tasks(id) ON DELETE SET NULL;

CREATE INDEX IF NOT EXISTS tasks_parent_task_id_idx
    ON public.tasks (parent_task_id);
