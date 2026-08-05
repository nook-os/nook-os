-- MAIN-415: the SQLite twin of `migrations/0041_session_stopped.sql`.
-- Hand-authored, per CLAUDE.md — nothing regenerates over this track.
--
-- The index half is ordinary: SQLite has partial indexes and drops and recreates
-- them like anything else.
--
-- The CHECK half is not. `sessions.status` is an INLINE table constraint, and
-- SQLite cannot alter one — and the documented create/copy/drop/rename rebuild
-- is unavailable from inside sqlx's migration transaction, which is always
-- there (`sqlx-sqlite` ignores `-- no-transaction`). With foreign keys stuck on,
-- `DROP TABLE sessions` implicit-deletes and cascades, and `ALTER TABLE …
-- RENAME` repoints every child's REFERENCES clause at the discarded copy. This
-- is the same wall `0040_loop_jobs_review_target.sql` hit; see its comment for
-- the measurements.
--
-- So this rewrites the DECLARED schema and moves no row, which is safe here for
-- the same reason it was there: the change is constraint-only — same columns,
-- same order, same types, same on-disk format.
--
-- The statement below is the schema SQLite ACTUALLY HOLDS at 0040 — not
-- `0001_init.sql`'s text — with exactly one edit: `'stopped'` added to the
-- status CHECK. That distinction is load-bearing and cost a run to find: 0031
-- added `managed` by `ALTER TABLE`, which SQLite appends to the stored CREATE
-- statement (`checkout_id TEXT, managed INTEGER NOT NULL DEFAULT 0,`). Writing
-- 0001's text back would have silently dropped the column, and the failure
-- surfaces later and elsewhere — as `malformed database schema … no such column:
-- managed` from an unrelated index. Anything rewriting this again must read
-- `SELECT sql FROM sqlite_master`, never a migration file.

PRAGMA writable_schema = ON;

UPDATE sqlite_master
SET sql = 'CREATE TABLE sessions (
  id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  workspace_id TEXT,
  node_id TEXT NOT NULL,
  name TEXT NOT NULL DEFAULT '''',
  runtime TEXT NOT NULL,
  tmux_session TEXT,
  status TEXT NOT NULL DEFAULT ''starting'',
  error TEXT,
  created_by TEXT,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  ended_at TEXT,
  checkout_id TEXT, managed INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (id),
  CHECK ((status IN (''starting'', ''running'', ''detached'', ''stopped'', ''exited'', ''error''))),
  FOREIGN KEY (checkout_id) REFERENCES node_workspaces (id) ON DELETE SET NULL,
  FOREIGN KEY (created_by) REFERENCES users (id) ON DELETE SET NULL,
  FOREIGN KEY (node_id) REFERENCES nodes (id) ON DELETE CASCADE,
  FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE,
  FOREIGN KEY (workspace_id) REFERENCES workspaces (id) ON DELETE CASCADE
)'
WHERE type = 'table' AND name = 'sessions';

-- Reloads the schema on this connection and clears the writable flag together,
-- so nothing downstream inherits a database that can be edited by accident.
PRAGMA writable_schema = RESET;

-- A `sqlite_master` write does not bump `schema_version`, so a connection that
-- already cached the schema would keep the old constraint. Any ordinary DDL
-- bumps it; the index rebuild below is real work AND does that, so unlike 0040
-- this migration needs no scratch table.

-- `stopped` joins the one-managed-session-per-checkout index — the same
-- decision `live_managed` makes, and the two have to agree. See the Postgres
-- twin's comment for why this is the opposite answer from node capacity.
DROP INDEX IF EXISTS sessions_one_managed_per_checkout;

CREATE UNIQUE INDEX IF NOT EXISTS sessions_one_managed_per_checkout
    ON sessions (checkout_id)
    WHERE managed AND checkout_id IS NOT NULL
      AND status IN ('starting', 'running', 'detached', 'stopped');
