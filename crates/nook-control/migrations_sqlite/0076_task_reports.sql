-- SQLite twin of 0076 (hand-authored, per CLAUDE.md). uuid/timestamptz -> TEXT
-- per the dialect map, and the timestamp defaults are `sqlite_time`'s one form
-- (MAIN-442) — `CURRENT_TIMESTAMP` neither compares nor orders against a bound
-- `DateTime<Utc>` on a TEXT column.
--
-- `DESC` is dropped from the recency index: SQLite reads an index in either
-- direction, so the ordering keyword buys nothing and the two tracks stay
-- readable side by side.
CREATE TABLE IF NOT EXISTS task_reports (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    key TEXT NOT NULL,
    title TEXT NOT NULL,
    body_md TEXT NOT NULL,
    author_type TEXT NOT NULL CHECK (author_type IN ('user', 'agent', 'system')),
    author_id TEXT,
    author_name TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f','now'))
);

CREATE UNIQUE INDEX IF NOT EXISTS task_reports_key_idx
    ON task_reports (task_id, key);

CREATE INDEX IF NOT EXISTS task_reports_recent_idx
    ON task_reports (task_id, updated_at);
