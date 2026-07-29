-- MAIN-231: a loop job can be opened with a SEED — the general idea the human
-- wants the run to start from. Until now `CreateLoopJobRequest` carried only
-- {kind, target_task_id}, so a spec job began with nothing but the (often
-- empty) ticket body and there was no way to say what you actually wanted.
--
-- The seed is stored on the job so it survives a re-dispatch (and a control
-- plane restart), rides the `RunLoopJob` message into the executor's session
-- environment, and is echoed as the opening `human` transcript line.
--
-- Purely additive and idempotent (ADD COLUMN IF NOT EXISTS), so a database that
-- already got the change converges and the dev ledger-ahead tolerance
-- (MAIN-224) can re-apply it after merge safely.
ALTER TABLE loop_jobs
    ADD COLUMN IF NOT EXISTS seed text;
