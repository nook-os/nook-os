-- MAIN-160: executor selection needs a place to explain why a job could not be
-- placed. `queued_reason` holds the specific gate that failed (no owned node
-- online, runtime unauthorized, no operator) while the job stays `queued`;
-- cleared when the job is finally claimed. Nullable, additive, idempotent.
ALTER TABLE loop_jobs ADD COLUMN IF NOT EXISTS queued_reason text;
