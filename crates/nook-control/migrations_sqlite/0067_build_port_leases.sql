-- The SQLite twin of 0067, hand-authored in the same commit (CLAUDE.md).
--
-- SQLite cannot drop a NOT NULL, so the table is rebuilt: rename, recreate in
-- the new shape, copy, drop. Nothing references `session_port_leases`, so the
-- rename rewrites no foreign key elsewhere and dropping the old table takes its
-- indexes with it — which is what frees the names for the new ones below.
--
-- `created_at` keeps the canonical SQLite timestamp form (MAIN-442), not
-- CURRENT_TIMESTAMP: a timestamptz is TEXT here, so text comparison IS the
-- comparison and the two spellings neither equal nor order against each other.
ALTER TABLE session_port_leases RENAME TO session_port_leases_old;

CREATE TABLE session_port_leases (
    id          TEXT PRIMARY KEY,
    -- Nullable now: the holder is a session OR the card whose build worktree
    -- the stack belongs to. The card and not the job, because the worktree
    -- outlives the run (MAIN-480) and a repair pass must get the same numbers
    -- back — the stack it is talking to is still bound to them.
    session_id  TEXT REFERENCES sessions (id) ON DELETE CASCADE,
    task_id     TEXT REFERENCES tasks (id) ON DELETE CASCADE,
    node_id     TEXT NOT NULL REFERENCES nodes (id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    env         TEXT NOT NULL,
    port        INTEGER NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f','now')),
    -- Exactly one holder. Neither would be a port nothing could hand back;
    -- both would be two answers to "whose is this".
    CONSTRAINT session_port_leases_one_holder
        CHECK ((session_id IS NULL) <> (task_id IS NULL))
);

INSERT INTO session_port_leases (id, session_id, task_id, node_id, name, env, port, created_at)
SELECT id, session_id, NULL, node_id, name, env, port, created_at
  FROM session_port_leases_old;

DROP TABLE session_port_leases_old;

CREATE UNIQUE INDEX IF NOT EXISTS session_port_leases_one_per_port
    ON session_port_leases (node_id, port);

CREATE UNIQUE INDEX IF NOT EXISTS session_port_leases_one_per_name
    ON session_port_leases (session_id, name);

-- The build twin. The NODE is part of the key and is not part of its twin's:
-- a session belongs to one machine and a card does not, so without it a card
-- leasing on a second node would take the `ON CONFLICT` path and rewrite the
-- first node's row to a port out of the second's range.
--
-- Not partial, matching the Postgres half: NULLs are DISTINCT in a unique
-- index, so every session row already falls out of it, and a predicate here
-- would stop the bare `ON CONFLICT (task_id, node_id, name)` the allocator
-- states from matching.
CREATE UNIQUE INDEX IF NOT EXISTS session_port_leases_one_per_build_name
    ON session_port_leases (task_id, node_id, name);

CREATE INDEX IF NOT EXISTS session_port_leases_by_session
    ON session_port_leases (session_id);

CREATE INDEX IF NOT EXISTS session_port_leases_by_task
    ON session_port_leases (task_id);
