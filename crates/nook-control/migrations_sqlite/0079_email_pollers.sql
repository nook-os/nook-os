-- SQLite twin of 0079 (hand-authored, per CLAUDE.md). uuid/timestamptz -> TEXT,
-- bytea -> BLOB, boolean -> INTEGER, and the timestamp defaults are
-- `sqlite_time`'s one form (MAIN-442) — never CURRENT_TIMESTAMP, whose spelling
-- neither equals nor orders against a bound instant on a TEXT column.
CREATE TABLE IF NOT EXISTS email_pollers (
    tenant_id TEXT PRIMARY KEY REFERENCES tenants(id) ON DELETE CASCADE,
    host TEXT NOT NULL,
    port INTEGER NOT NULL,
    username TEXT NOT NULL,
    password_enc BLOB NOT NULL,
    mailbox TEXT NOT NULL,
    poll_interval_secs INTEGER NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    uid_validity INTEGER,
    last_uid INTEGER NOT NULL DEFAULT 0,
    last_polled_at TEXT,
    last_error TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f','now'))
);

CREATE TABLE IF NOT EXISTS inbound_email_seen (
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    source TEXT NOT NULL,
    message_id TEXT NOT NULL,
    task_id TEXT REFERENCES tasks(id) ON DELETE SET NULL,
    first_seen_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f','now')),
    PRIMARY KEY (tenant_id, source, message_id)
);
