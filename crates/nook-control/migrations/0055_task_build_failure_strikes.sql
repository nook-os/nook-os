-- MAIN-489: how many build runs in a row have concluded NOTHING on this card.
-- Bumped when a run reaches a terminal state with no outcome recorded, zeroed
-- by a recorded outcome and by the human nudges that bring an escalated card
-- back. Three strikes label the card `blocked`, which is what stops the retry
-- cycle from running until the claim reaper's four-hour cap notices.
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS build_failure_strikes integer NOT NULL DEFAULT 0;
