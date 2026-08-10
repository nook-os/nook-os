-- SQLite twin of 0058 (hand-authored, per CLAUDE.md).
--
-- 0058 and not 0056: a number is claimed on BOTH tracks at once, so the next
-- free one is the highest EITHER set uses plus one. This card first took 0056
-- — the next free number on the Postgres side — and collided with SQLite's
-- already-landed `0056_task_build_failure_strikes`, which is a `UNIQUE
-- constraint failed: _sqlx_migrations.version` and a database that cannot boot.
--
-- `IF NOT EXISTS` is not valid on ADD COLUMN here; a NOT NULL column with a
-- constant default needs no table rebuild. uuid/timestamptz are TEXT, and the
-- timestamp default is `sqlite_time`'s one form (MAIN-442) — `CURRENT_TIMESTAMP`
-- would neither equal nor order against a bound `DateTime<Utc>`.
ALTER TABLE sessions ADD COLUMN interface TEXT NOT NULL DEFAULT 'terminal';

CREATE TABLE IF NOT EXISTS session_messages (
  id TEXT NOT NULL,
  session_id TEXT NOT NULL,
  role TEXT NOT NULL DEFAULT 'agent',
  body TEXT NOT NULL,
  permission_request_id TEXT,
  tool_name TEXT,
  decision TEXT,
  at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f','now')),
  PRIMARY KEY (id),
  FOREIGN KEY (session_id) REFERENCES sessions (id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS session_messages_session_idx
  ON session_messages (session_id, id);

CREATE INDEX IF NOT EXISTS session_messages_pending_permission_idx
  ON session_messages (session_id, permission_request_id)
  WHERE decision IS NULL AND permission_request_id IS NOT NULL;
