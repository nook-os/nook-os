-- MAIN-607: the jobs the stall reaper has been failing, or is about to.
--
-- A run that recorded its outcome has CONCLUDED; the completion signal that
-- normally follows was simply lost. Until now such a job sat `running` forever
-- — holding a slot of its node's loop capacity — until the stall reaper failed
-- it an hour later and handed its finished, reviewed, PR-carrying card back.
-- The code no longer does that; these are the rows already in that state.
--
-- Idempotent by its own WHERE: a second run matches nothing. A `failed` job
-- with a recorded outcome is deliberately out of scope (NG-7) — its card was
-- already handed back, and completing the row now would not undo that.
UPDATE loop_jobs
   SET state = 'completed', updated_at = now()
 WHERE state IN ('claimed', 'running')
   AND (build_outcome IS NOT NULL OR review_verdict IS NOT NULL);
