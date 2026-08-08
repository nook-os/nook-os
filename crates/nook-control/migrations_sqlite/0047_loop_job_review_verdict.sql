-- SQLite twin of 0047 (hand-authored, per CLAUDE.md). Nullable text ADD COLUMN
-- needs no rebuild; `IF NOT EXISTS` is not valid on ADD COLUMN here.
ALTER TABLE loop_jobs ADD COLUMN review_verdict text;
