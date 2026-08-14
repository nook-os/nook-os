-- SQLite twin of 0073 (hand-authored, per CLAUDE.md). timestamptz → TEXT per
-- the dialect map; `IF NOT EXISTS` is not valid on ADD COLUMN on this engine,
-- and a nullable ADD COLUMN needs no table rebuild.
ALTER TABLE tasks ADD COLUMN unblocked_at TEXT;
