-- SQLite twin of 0069 (hand-authored, per CLAUDE.md). timestamptz → TEXT per
-- the dialect map; `IF NOT EXISTS` / `IF EXISTS` are not valid on ADD or DROP
-- COLUMN on this engine, and a nullable ADD COLUMN needs no table rebuild.
ALTER TABLE tasks ADD COLUMN build_ladder_cleared_at TEXT;
ALTER TABLE tasks DROP COLUMN build_failure_strikes;
