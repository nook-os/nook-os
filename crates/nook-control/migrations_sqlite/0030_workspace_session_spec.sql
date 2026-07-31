-- The SQLite twin of 0030, hand-authored in the same commit (CLAUDE.md).
-- jsonb -> TEXT; nullable, so no default and no NOT NULL. SQLite has no
-- `ADD COLUMN IF NOT EXISTS`, which is correct here: each migration runs once
-- against a ledger, and the Postgres guard exists for databases that got the
-- column by other means — impossible on a track whose whole history is this
-- ledger.
ALTER TABLE workspaces ADD COLUMN session_spec TEXT;
