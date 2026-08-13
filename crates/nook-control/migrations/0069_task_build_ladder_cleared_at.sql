-- MAIN-386: the human's own reset of the build-failure ladder.
--
-- The ladder itself is DERIVED — the run of `failed` build jobs for a card
-- since the last one that recorded an outcome (AC-1, NG-2), read straight off
-- `loop_jobs`. This column is the one fact those rows cannot carry: that a
-- person lifted `needs-human-review`, or named the card to the manual trigger,
-- and the failures before that moment are answered. Without it the count still
-- reads three the instant the card comes back, and the next single failure
-- re-escalates on the spot instead of climbing the ladder again (AC-5).
--
-- It REPLACES MAIN-489's `build_failure_strikes`, whose whole job — count the
-- consecutive failures, stop at three — this ladder now does from the rows.
-- Dropped rather than left: a stored count beside the rows it counts is a
-- second truth, and 0055 added it three days ago, so nothing has come to rely
-- on the number.
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS build_ladder_cleared_at timestamptz;
ALTER TABLE tasks DROP COLUMN IF EXISTS build_failure_strikes;
