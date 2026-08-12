-- SQLite twin of 0061 (hand-authored, per CLAUDE.md). uuid/timestamptz -> TEXT,
-- and the timestamp default is `sqlite_time`'s one form (MAIN-442).
CREATE TABLE IF NOT EXISTS task_attachments (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    user_content_id TEXT NOT NULL REFERENCES user_content(id) ON DELETE CASCADE,
    parent_kind TEXT NOT NULL CHECK (parent_kind IN ('task', 'task_comment')),
    parent_id TEXT NOT NULL,
    attached_by TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f','now'))
);

CREATE INDEX IF NOT EXISTS task_attachments_parent_idx
    ON task_attachments (parent_kind, parent_id, created_at);

CREATE UNIQUE INDEX IF NOT EXISTS task_attachments_unique_idx
    ON task_attachments (parent_kind, parent_id, user_content_id);
