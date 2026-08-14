-- MAIN-584: when a human ruled on a stopped card and told the loop to carry on.
--
-- Clearing the escalation label is not enough on its own. `run_reconcile::owed`
-- suppresses an item whose last concluded run recorded the card's current
-- fingerprint, and `card_fingerprint` is title+description only — so a card the
-- build loop handed back with a `blocked` outcome stays unpickable however many
-- comments and labels move over it. This column is the one fact the runs cannot
-- carry: that a person restarted the card at this instant, so a run that
-- concluded before it no longer speaks for the card's current state.
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS unblocked_at timestamptz;
