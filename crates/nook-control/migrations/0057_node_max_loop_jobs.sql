-- MAIN-508: an operator's loop-job capacity for a node, `port_range_start`'s
-- twin. NULL means nobody set one centrally (the node's own advertisement
-- decides), and 0 means STOP CLAIMING — a deliberate cordon, not a busy node —
-- so the two states must stay distinguishable and there is no DEFAULT.
ALTER TABLE nodes ADD COLUMN IF NOT EXISTS max_loop_jobs integer;
