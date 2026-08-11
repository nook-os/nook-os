-- MAIN-505: a node refusing NEW loop work while it waits to restart into a new
-- agent. Node-reported and re-asserted on every connect, like `resources` —
-- never operator policy, which is what `max_loop_jobs = 0` is. Nullable with no
-- default: NULL is "takes work", and there is no third state.
ALTER TABLE nodes ADD COLUMN IF NOT EXISTS cordon jsonb;
