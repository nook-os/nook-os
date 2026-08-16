-- SQLite twin of 0080 (hand-authored, per CLAUDE.md). bytea -> BLOB per the
-- dialect map; `IF NOT EXISTS` has no ALTER TABLE form here, and this column
-- pair is new in this migration, so the bare ADD COLUMN is the whole of it.
ALTER TABLE email_links ADD COLUMN findings TEXT;
ALTER TABLE email_links ADD COLUMN draft_reply_enc BLOB;
