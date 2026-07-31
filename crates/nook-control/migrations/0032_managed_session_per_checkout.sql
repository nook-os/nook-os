-- Per-worktree placement. Uniqueness moves from (workspace, node) to the
-- CHECKOUT: a node holding a clone AND N worktrees now runs one managed session
-- per checkout, not a single one for the whole node. `replicas` still decides
-- how many NODES hold a clone of the repo; this index decides that each checkout
-- on such a node gets exactly one managed session.
--
-- A managed session always carries a `checkout_id` (start_managed sets it from
-- the path it placed into), so keying on `checkout_id` is exact. The explicit
-- `checkout_id IS NOT NULL` matters because Postgres treats nulls as distinct —
-- without it a stray null-checkout managed row (which should never exist) would
-- slip past the uniqueness guard.
DROP INDEX IF EXISTS sessions_one_managed_per_node;

CREATE UNIQUE INDEX IF NOT EXISTS sessions_one_managed_per_checkout
    ON public.sessions (checkout_id)
    WHERE managed AND checkout_id IS NOT NULL AND status IN ('starting', 'running', 'detached');
