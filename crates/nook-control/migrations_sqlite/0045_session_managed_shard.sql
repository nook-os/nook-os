-- The SQLite twin of 0045, hand-authored in the same commit (CLAUDE.md).
--
-- A managed session gains a SHARD (MAIN-446) so several reviewers can share one
-- clone instead of reading as each other's duplicate under 0042's index.
--
-- SQLite has no `ADD COLUMN IF NOT EXISTS`, so the adds are unconditional. That
-- is safe here because both columns are new in this migration and the ledger
-- runs each version once; a database that already has them is one this
-- migration has already applied to.
--
-- `integer` is the same declared type on both engines and needs no entry in
-- docs/db-dialect-audit.md's type map.
ALTER TABLE sessions ADD COLUMN managed_shard INTEGER NOT NULL DEFAULT 0;
ALTER TABLE sessions ADD COLUMN managed_shards INTEGER NOT NULL DEFAULT 1;

DROP INDEX IF EXISTS sessions_one_managed_per_checkout_purpose;

-- 0041's DECLARED status set, carried over from 0042. The divisor is not in the
-- key: two sessions on one checkout with the same index and different divisors
-- are one slot described two ways.
CREATE UNIQUE INDEX IF NOT EXISTS sessions_one_managed_per_checkout_purpose_shard
    ON sessions (checkout_id, managed_purpose, managed_shard)
    WHERE managed AND checkout_id IS NOT NULL
      AND status IN ('starting', 'running', 'detached', 'stopped');
