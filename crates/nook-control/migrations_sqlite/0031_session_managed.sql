-- The SQLite twin of 0031, hand-authored in the same commit (CLAUDE.md).
--
-- Type map: boolean -> INTEGER with a 0/1 default. SQLite has no
-- `ADD COLUMN IF NOT EXISTS`, but each migration runs exactly once against a
-- ledger, so a plain ADD COLUMN is correct here.
--
-- The partial unique index carries over unchanged — SQLite supports partial
-- indexes, and it is doing real work: it is what makes "one action wins" true
-- on this engine too, rather than only on Postgres.
ALTER TABLE sessions ADD COLUMN managed INTEGER NOT NULL DEFAULT 0;

CREATE UNIQUE INDEX IF NOT EXISTS sessions_one_managed_per_node
    ON sessions (workspace_id, node_id)
    WHERE managed AND status IN ('starting', 'running', 'detached');
