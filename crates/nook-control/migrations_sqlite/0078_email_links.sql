-- SQLite twin of 0078 (hand-authored, per CLAUDE.md). uuid/timestamptz -> TEXT
-- per the dialect map, and `created_at`'s default is `sqlite_time`'s one form
-- (MAIN-442) rather than `CURRENT_TIMESTAMP`, which neither compares nor orders
-- against a bound `DateTime<Utc>` on a TEXT column.
--
-- `DESC` is dropped from the recency index: SQLite reads an index in either
-- direction, so the keyword buys nothing and the two tracks stay readable side
-- by side.
CREATE TABLE IF NOT EXISTS email_links (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    workspace_id TEXT REFERENCES workspaces(id) ON DELETE SET NULL,
    task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    loop_job_id TEXT REFERENCES loop_jobs(id) ON DELETE SET NULL,
    pr_ref TEXT,
    message_id TEXT,
    in_reply_to TEXT,
    storage_key TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f','now'))
);

CREATE INDEX IF NOT EXISTS email_links_message_idx
    ON email_links (tenant_id, message_id);

CREATE INDEX IF NOT EXISTS email_links_task_idx
    ON email_links (tenant_id, task_id);

CREATE INDEX IF NOT EXISTS email_links_recent_idx
    ON email_links (tenant_id, workspace_id, created_at);
