-- SQLite twin of 0055 (hand-authored, per CLAUDE.md). Numbered 0056 because
-- 0055 on this track is the sqlite-only timestamp-form rewrite: the two
-- ledgers are per-engine, so the twin's NAME matches and its number does not.
-- A constant NOT NULL default needs no table rebuild; `IF NOT EXISTS` is not
-- valid on ADD COLUMN here.
ALTER TABLE tasks ADD COLUMN build_failure_strikes INTEGER NOT NULL DEFAULT 0;
