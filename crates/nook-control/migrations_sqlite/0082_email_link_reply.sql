-- SQLite twin of 0082 (hand-authored, per CLAUDE.md). `timestamptz` -> TEXT per
-- the dialect map; `IF NOT EXISTS` has no ALTER TABLE form here, and every
-- column below is new in this migration, so the bare ADD COLUMN is the whole of
-- it. The Postgres file carries the reasoning.
ALTER TABLE email_links ADD COLUMN staffer_address TEXT;
ALTER TABLE email_links ADD COLUMN customer_address TEXT;
ALTER TABLE email_links ADD COLUMN subject TEXT;
ALTER TABLE email_links ADD COLUMN reply_sent_at TEXT;
ALTER TABLE email_links ADD COLUMN reply_recipient TEXT;
