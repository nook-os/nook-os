-- SQLite twin of 0061 (hand-authored, per CLAUDE.md). jsonb → TEXT per the
-- dialect map; `IF NOT EXISTS` is not valid on ADD COLUMN here.
ALTER TABLE loop_jobs ADD COLUMN queued_reason_kind TEXT;
