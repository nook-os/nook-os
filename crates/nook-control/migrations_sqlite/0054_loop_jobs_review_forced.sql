-- SQLite twin of 0054 (hand-authored, per CLAUDE.md). boolean → INTEGER per
-- the dialect map; `IF NOT EXISTS` is not valid on ADD COLUMN here.
ALTER TABLE loop_jobs ADD COLUMN review_forced INTEGER NOT NULL DEFAULT 0;
