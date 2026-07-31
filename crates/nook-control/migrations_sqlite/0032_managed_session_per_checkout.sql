-- The SQLite twin of 0032, hand-authored in the same commit (CLAUDE.md).
--
-- Per-worktree placement: uniqueness moves from (workspace, node) to the
-- checkout. SQLite supports partial indexes and treats NULLs as distinct in a
-- unique index exactly as Postgres does, so the `checkout_id IS NOT NULL` guard
-- carries over unchanged and does the same real work here.
DROP INDEX IF EXISTS sessions_one_managed_per_node;

CREATE UNIQUE INDEX IF NOT EXISTS sessions_one_managed_per_checkout
    ON sessions (checkout_id)
    WHERE managed AND checkout_id IS NOT NULL AND status IN ('starting', 'running', 'detached');
