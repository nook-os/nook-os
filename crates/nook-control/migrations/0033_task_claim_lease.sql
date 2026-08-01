-- Claim leases on board cards (MAIN-229).
--
-- `claim_expires_at` is the fence the reaper is confined to: NULL means "no
-- agent claim" — a card a human dragged into In Progress — and such a card is
-- never examined, moved or labelled by the reaper. Only a card that entered
-- `started` through the agent claim / start-work path carries a lease.
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS claim_expires_at timestamptz;

-- Partial, so the scan reads only leased cards rather than the whole board.
CREATE INDEX IF NOT EXISTS tasks_claim_expires_at_idx
    ON tasks (claim_expires_at)
    WHERE claim_expires_at IS NOT NULL;
