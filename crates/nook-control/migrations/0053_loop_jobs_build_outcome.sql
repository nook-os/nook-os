-- MAIN-458: build runs are OUTCOME-GATED from day one (MAIN-455's
-- completed≠reviewed lesson, applied before the first build converges).
--
-- `build_outcome` is what the run CONCLUDED: pr_opened | blocked |
-- nothing_to_do. NULL means the run concluded nothing, however it exited —
-- and a completed run with no outcome does not consume its card; the
-- reconciler holds it on the shared backoff and re-raises.
--
-- `build_fingerprint` is what the item looked like when the run was raised —
-- a content hash of the card for a fresh pick, the rejected head sha for a
-- repair item — the same role `review_head_sha` plays for reviews.
-- Deliberately NOT the card's `updated_at`: the control plane itself claims
-- and releases the card around a run, and a fingerprint its own writes could
-- move would clear its own failure hold.
ALTER TABLE loop_jobs ADD COLUMN IF NOT EXISTS build_outcome text;
ALTER TABLE loop_jobs ADD COLUMN IF NOT EXISTS build_fingerprint text;
