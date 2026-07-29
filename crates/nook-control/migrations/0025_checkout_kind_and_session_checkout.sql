-- MAIN-222: make a checkout addressable and record where a session runs.
--
-- Two facts were unreadable or missing. The primary-vs-worktree distinction
-- lived only in an unread `git_status->>'worktree'` jsonb boolean, so every
-- "the checkout" default (`ORDER BY discovered_at LIMIT 1`) could select a
-- worktree — cutting worktrees from worktrees, restarting sessions in the wrong
-- directory. And a session recorded nothing about which checkout it ran in.
--
-- This adds a first-class `kind` on checkouts (backfilled from the jsonb flag)
-- and a nullable `checkout_id` on sessions, so the deterministic clone-only
-- picks and the "reuse the checkout the session started in" restart become real.
--
-- Idempotent and additive; the frozen Postgres track's append-only rule holds.

ALTER TABLE public.node_workspaces
    ADD COLUMN IF NOT EXISTS kind text NOT NULL DEFAULT 'clone'
    CONSTRAINT node_workspaces_kind_check CHECK (kind IN ('clone', 'worktree', 'mirror'));

-- Backfill from the existing jsonb flag: rows the node reported as worktrees
-- become kind='worktree'. Converges on re-run (already-set rows are unaffected).
UPDATE public.node_workspaces
    SET kind = 'worktree'
    WHERE kind = 'clone' AND (git_status ->> 'worktree')::boolean IS TRUE;

-- Where a session runs. NULL for ad-hoc `$HOME` terminals and for sessions whose
-- checkout has been pruned (ON DELETE SET NULL — restart then falls back to the
-- deterministic pick).
ALTER TABLE public.sessions
    ADD COLUMN IF NOT EXISTS checkout_id uuid
    REFERENCES public.node_workspaces(id) ON DELETE SET NULL;

CREATE INDEX IF NOT EXISTS idx_sessions_checkout_id
    ON public.sessions (checkout_id)
    WHERE checkout_id IS NOT NULL;
