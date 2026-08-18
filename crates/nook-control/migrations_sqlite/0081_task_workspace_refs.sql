-- SQLite twin of 0081 (hand-authored, per CLAUDE.md). uuid/timestamptz -> TEXT
-- per the dialect map, and the timestamp default is `sqlite_time`'s one form
-- (MAIN-442) — `CURRENT_TIMESTAMP` neither compares nor orders against a bound
-- `DateTime<Utc>` on a TEXT column.
CREATE TABLE IF NOT EXISTS task_workspace_refs (
    task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f','now')),
    PRIMARY KEY (task_id, workspace_id)
);

CREATE INDEX IF NOT EXISTS task_workspace_refs_workspace_idx
    ON task_workspace_refs (workspace_id);
