-- The SQLite twin of 0033, hand-authored in the same commit (CLAUDE.md).
--
-- `timestamptz` maps to TEXT per the dialect audit. SQLite has no
-- `ADD COLUMN IF NOT EXISTS`, but a migration runs once per ledger and the
-- partial index below carries the idempotent spelling it does support.
ALTER TABLE tasks ADD COLUMN claim_expires_at TEXT;

CREATE INDEX IF NOT EXISTS tasks_claim_expires_at_idx
    ON tasks (claim_expires_at)
    WHERE claim_expires_at IS NOT NULL;
