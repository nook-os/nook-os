-- The SQLite twin of 0034, hand-authored in the same commit (CLAUDE.md).
--
-- Type map per docs/db-dialect-audit.md: uuid/timestamptz/jsonb -> TEXT,
-- now() -> CURRENT_TIMESTAMP. SQLite has no `ADD COLUMN IF NOT EXISTS`, but a
-- migration runs once per ledger, so a plain ADD COLUMN is correct.
--
-- The two unique indexes carry over verbatim, and they are the whole mechanic:
-- one arbitrates the allocation race, the other keeps re-leasing idempotent.
-- Neither is partial, because reclaim moved into the allocator when the lease
-- stopped living on the session row.
ALTER TABLE workspaces ADD COLUMN port_requirements TEXT;

CREATE TABLE IF NOT EXISTS session_port_leases (
    id          TEXT PRIMARY KEY,
    session_id  TEXT NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    node_id     TEXT NOT NULL REFERENCES nodes (id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    env         TEXT NOT NULL,
    port        INTEGER NOT NULL,
    created_at  TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE UNIQUE INDEX IF NOT EXISTS session_port_leases_one_per_port
    ON session_port_leases (node_id, port);

CREATE UNIQUE INDEX IF NOT EXISTS session_port_leases_one_per_name
    ON session_port_leases (session_id, name);

CREATE INDEX IF NOT EXISTS session_port_leases_by_session
    ON session_port_leases (session_id);

ALTER TABLE nodes ADD COLUMN port_range_start INTEGER;
ALTER TABLE nodes ADD COLUMN port_range_end INTEGER;
