-- MAIN-494: the gate that is holding a job `queued`, as a value rather than a
-- sentence. `queued_reason` keeps the sentence — it is the rendering, and the
-- one thing a human reads — while this column is what a client branches on.
--
-- Nullable with no backfill and no default, deliberately: the existing
-- sentences were never a contract, and parsing them into variants would state a
-- cause we cannot actually know. NULL means "this row predates the column, or
-- its reason is the residual no-eligible-executor phrasing", and both render
-- their text.
ALTER TABLE public.loop_jobs ADD COLUMN IF NOT EXISTS queued_reason_kind jsonb;
