-- A build run leases the ports it binds (MAIN-552).
--
-- `session_port_leases` was keyed on a session because a session was the only
-- thing that bound anything. A build run boots the dev stack in its worktree
-- and is not a session, so it took `docker-compose.yml`'s `${VAR:-default}`
-- fallbacks — the exact collision MAIN-301 exists to remove, on the machine
-- that also runs human sessions which DO lease.
--
-- So the holder becomes one of two things, never both: a session, or the CARD
-- whose build worktree the stack belongs to. The card and not the job, because
-- the worktree outlives the run (MAIN-480) and a repair pass must get the same
-- numbers back — the stack it is talking to is still bound to them.
ALTER TABLE public.session_port_leases ALTER COLUMN session_id DROP NOT NULL;

ALTER TABLE public.session_port_leases
    ADD COLUMN IF NOT EXISTS task_id uuid REFERENCES public.tasks (id) ON DELETE CASCADE;

-- Exactly one holder. A row with neither would be a port nothing could ever
-- hand back, and a row with both would have two answers to "whose is this".
ALTER TABLE public.session_port_leases
    DROP CONSTRAINT IF EXISTS session_port_leases_one_holder;
ALTER TABLE public.session_port_leases
    ADD CONSTRAINT session_port_leases_one_holder
    CHECK ((session_id IS NULL) <> (task_id IS NULL));

-- The build twin of `session_port_leases_one_per_name`: re-leasing a
-- requirement is idempotent, so a repair pass keeps one lease per listener
-- rather than stacking a second.
--
-- **The NODE is part of the key here and is not part of its twin's, because a
-- session belongs to one machine and a card does not.** Without it, a card
-- holding `web` on node A and leasing on node B takes the `ON CONFLICT` path
-- and REWRITES A's row to a port out of B's range — a row that then reads as
-- neither node's: invisible to B's allocator, and wrong on A. One row per
-- (card, node, listener) keeps a lease describing exactly one machine, which
-- is what the node-scoped read and the node-scoped release both assume.
--
-- Not partial, for the same reason its twin is not: NULLs are DISTINCT in a
-- unique index, so every session row (`task_id IS NULL`) already falls out of
-- it. A `WHERE task_id IS NOT NULL` would also stop Postgres inferring the
-- index from a bare `ON CONFLICT (task_id, node_id, name)`, which is how the
-- allocator states the arbiter.
CREATE UNIQUE INDEX IF NOT EXISTS session_port_leases_one_per_build_name
    ON public.session_port_leases (task_id, node_id, name);

CREATE INDEX IF NOT EXISTS session_port_leases_by_task
    ON public.session_port_leases (task_id);

-- RECLAIM STAYS AS IT WAS, and that is AC-4. The allocator's first step drops
-- the rows of NON-LIVE SESSIONS; a build's row has no session, so it is not
-- reachable by that sweep and cannot be freed while its stack is still bound.
-- A build lease is handed back by `stack_reaper` when the stack actually comes
-- down, by `prune-worktree` when a human takes the tree away, and by this
-- table's cascade when the card itself is deleted.
