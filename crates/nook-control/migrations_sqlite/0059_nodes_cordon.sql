-- SQLite twin of 0059 (hand-authored, per CLAUDE.md). `jsonb` is TEXT here;
-- a nullable ADD COLUMN needs no table rebuild, and `IF NOT EXISTS` is not
-- valid on ADD COLUMN on this engine.
ALTER TABLE nodes ADD COLUMN cordon TEXT;
