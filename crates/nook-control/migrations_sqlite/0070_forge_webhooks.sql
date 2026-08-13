-- SQLite twin of 0069 (hand-authored, per CLAUDE.md). uuid/timestamptz/jsonb ->
-- TEXT, bytea -> BLOB, and the timestamp default is `sqlite_time`'s one form
-- (MAIN-442) — never CURRENT_TIMESTAMP, whose spelling neither equals nor
-- orders against an RFC-3339-ish bound instant on a TEXT column.
--
-- A nullable ADD COLUMN needs no table rebuild and takes no IF NOT EXISTS here.
ALTER TABLE workspaces ADD COLUMN webhook_secret_enc BLOB;

CREATE TABLE IF NOT EXISTS forge_deliveries (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    delivery_id TEXT NOT NULL,
    event TEXT NOT NULL,
    action TEXT,
    repo_full_name TEXT NOT NULL,
    payload TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('received', 'ignored', 'error')),
    error TEXT,
    received_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f','now'))
);

CREATE UNIQUE INDEX IF NOT EXISTS forge_deliveries_unique_idx
    ON forge_deliveries (workspace_id, delivery_id);
