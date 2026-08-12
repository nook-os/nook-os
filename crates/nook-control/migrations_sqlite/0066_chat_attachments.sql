-- The SQLite twin of nook-chat's `0009_chat_attachments.sql` (MAIN-535).
--
-- On the CONTROL track, not in `crates/nook-chat/migrations_sqlite/`: that
-- directory does not exist and should not, because one SQLite file is one
-- namespace with one ledger. `chat_messages` — which this table references — is
-- created by `migrations_sqlite/0001_init.sql:990`, and #429's twin for chat's
-- 0008 went to `0063_chat_message_kind.sql` here for the same reason.
--
-- Type map per docs/db-dialect-audit.md: `uuid` and `timestamptz` are TEXT,
-- `bigint` is INTEGER. The `created_at` default is `nook_db::sqlite_time`'s
-- form and not `CURRENT_TIMESTAMP` — MAIN-442: a timestamptz is TEXT here, so
-- text comparison IS the comparison, and CURRENT_TIMESTAMP's
-- `2026-08-06 13:28:36` neither equals nor orders against the RFC 3339 a bound
-- DateTime<Utc> encodes as. `sqlite_boot.rs` fails the build on any other
-- spelling.
--
-- 0066 because the highest number either track uses is 0065 — MAIN-533's
-- `0065_task_attachments`, on BOTH tracks. That is the rule from CLAUDE.md
-- (MAIN-502): the next free number is the highest EITHER set uses, plus one,
-- and counting from Postgres alone is what breaks. This file has now been
-- renumbered twice by branches landing under it — 0064 to MAIN-516, 0065 to
-- MAIN-533 — which is invisible locally, because a branch's own tree is always
-- self-consistent; only the MERGE with main holds two rows at one version, and
-- `_sqlx_migrations.version` is unique. SQLite-only, so the Postgres track
-- skips 0066 the way it skips 0038 and 0063.
CREATE TABLE IF NOT EXISTS chat_message_attachments (
    id               TEXT PRIMARY KEY,
    message_id       TEXT NOT NULL REFERENCES chat_messages (id) ON DELETE CASCADE,
    content_id       TEXT NOT NULL,
    filename         TEXT NOT NULL,
    content_type     TEXT NOT NULL,
    size_bytes       INTEGER NOT NULL,
    -- The order the sender picked them in; ties broken by id, which is v7.
    position         INTEGER NOT NULL DEFAULT 0,
    created_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f','now'))
);

CREATE INDEX IF NOT EXISTS chat_message_attachments_message_idx
    ON chat_message_attachments (message_id, position, id);

CREATE UNIQUE INDEX IF NOT EXISTS chat_message_attachments_content_idx
    ON chat_message_attachments (content_id);
