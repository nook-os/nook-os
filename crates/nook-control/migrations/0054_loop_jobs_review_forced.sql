-- MAIN-473: a human can FORCE a re-review of a verdicted head. The flag rides
-- the job row so the run's environment can carry it (`NOOK_REVIEW_FORCED`) and
-- the reviewer's already-reviewed skip-check can stand aside for exactly this
-- run — without it the forced run reads the existing `Loop review of <sha>`
-- comment and is contractually required to record `skipped`, which no-ops the
-- whole lever.
ALTER TABLE loop_jobs ADD COLUMN IF NOT EXISTS review_forced boolean NOT NULL DEFAULT false;
