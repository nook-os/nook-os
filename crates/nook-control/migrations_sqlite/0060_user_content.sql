-- SQLite twin of 0060 (hand-authored, per CLAUDE.md). uuid/timestamptz -> TEXT,
-- bigint -> INTEGER, and the timestamp default is `sqlite_time`'s one form
-- (MAIN-442) — `CURRENT_TIMESTAMP` would neither equal nor order against a
-- bound `DateTime<Utc>`. REFERENCES + ON DELETE CASCADE carry straight across.
CREATE TABLE IF NOT EXISTS user_content (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    uploaded_by TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    filename TEXT NOT NULL,
    content_type TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    storage_key TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f','now'))
);

CREATE INDEX IF NOT EXISTS user_content_tenant_idx
    ON user_content (tenant_id, created_at DESC);
