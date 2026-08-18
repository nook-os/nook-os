-- SQLite twin of 0081 (hand-authored, per CLAUDE.md). uuid/timestamptz -> TEXT,
-- bytea -> BLOB, and the timestamp defaults are `sqlite_time`'s one form
-- (MAIN-442) — never CURRENT_TIMESTAMP.
CREATE TABLE IF NOT EXISTS secret_items (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    scope TEXT NOT NULL,
    scope_id TEXT NOT NULL,
    name TEXT NOT NULL,
    value_enc BLOB NOT NULL,
    dek_wrapped BLOB NOT NULL,
    updated_by TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f','now'))
);

CREATE UNIQUE INDEX IF NOT EXISTS secret_items_scope_name_key
    ON secret_items (scope, scope_id, name);

CREATE INDEX IF NOT EXISTS secret_items_tenant_idx ON secret_items (tenant_id);
