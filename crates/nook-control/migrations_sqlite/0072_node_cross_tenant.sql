-- SQLite twin of 0072 (hand-authored, per CLAUDE.md). boolean -> INTEGER, as
-- `shared` is spelled at 0001_init.sql:301.
--
-- A NOT NULL ADD COLUMN with a constant default needs no table rebuild, and
-- SQLite has no IF NOT EXISTS for ADD COLUMN — the migration ledger is what
-- makes this run once.
ALTER TABLE nodes ADD COLUMN cross_tenant INTEGER NOT NULL DEFAULT 1;
