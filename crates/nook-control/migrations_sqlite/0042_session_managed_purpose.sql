-- The SQLite twin of 0042, hand-authored in the same commit (CLAUDE.md).
--
-- A managed session gains a PURPOSE (MAIN-326) so the workspace's access spec
-- and the control plane's review loop can share a checkout without each reading
-- as the other's duplicate.
--
-- SQLite has no `ADD COLUMN IF NOT EXISTS`, so the add is unconditional. That
-- is safe here because the column is new in this migration and the ledger runs
-- each version once; a database that already has it is one this migration has
-- already applied to.
ALTER TABLE sessions ADD COLUMN managed_purpose TEXT NOT NULL DEFAULT 'access';

DROP INDEX IF EXISTS sessions_one_managed_per_checkout;

-- 0041's DECLARED status set, `stopped` included: a stopped managed session
-- still satisfies its declaration, so it still holds its slot.
CREATE UNIQUE INDEX IF NOT EXISTS sessions_one_managed_per_checkout_purpose
    ON sessions (checkout_id, managed_purpose)
    WHERE managed AND checkout_id IS NOT NULL
      AND status IN ('starting', 'running', 'detached', 'stopped');
