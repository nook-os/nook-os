-- SQLite twin of 0051 (hand-authored, per CLAUDE.md). uuid -> TEXT,
-- timestamptz -> TEXT, now() -> CURRENT_TIMESTAMP per the dialect audit;
-- REFERENCES + ON DELETE CASCADE carry straight across.
CREATE TABLE IF NOT EXISTS task_description_revisions (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    body text NOT NULL,
    author_id TEXT,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS task_description_revisions_task_idx
    ON task_description_revisions (task_id, created_at);
