-- The SQLite twin of 0029, hand-authored in the same commit (CLAUDE.md).
--
-- Type map: jsonb -> TEXT, and the `::jsonb` casts are stripped. SQLite has no
-- `ADD COLUMN IF NOT EXISTS`, but each migration runs exactly once against a
-- ledger, so a plain ADD COLUMN is correct here — the Postgres guard exists for
-- databases that got the column by other means, which cannot happen on a track
-- whose whole history is this ledger.
ALTER TABLE nodes ADD COLUMN labels TEXT NOT NULL DEFAULT '{}';
ALTER TABLE nodes ADD COLUMN taints TEXT NOT NULL DEFAULT '[]';
