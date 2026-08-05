-- MAIN-415: `stopped` — declared, resumable, costing nothing.
--
-- A session was either live or gone, so closing a tab could not reclaim
-- anything: killing it lost the row's usefulness, keeping it kept the machine
-- busy. `stopped` is the third state — the row persists and still satisfies its
-- workspace's declaration, while the tmux is gone and the ports are back.
--
-- Two schema changes, both idempotent.

-- 1. The status CHECK admits it. Drop-then-add, as 0010 does for board columns:
--    a CHECK cannot be altered in place, and `IF EXISTS` makes the drop safe on
--    a partially-applied database.
ALTER TABLE sessions
    DROP CONSTRAINT IF EXISTS sessions_status_check;
ALTER TABLE sessions
    ADD CONSTRAINT sessions_status_check
    CHECK (status IN ('starting', 'running', 'detached', 'stopped', 'exited', 'error'));

-- 2. `stopped` joins the one-managed-session-per-checkout index, and this is
--    NOT a reflex edit — it is the same decision `live_managed` makes, and the
--    two have to agree.
--
--    The reconciler counts a stopped managed session as satisfying the
--    declaration (that is the whole point: Stop must not silently undo itself).
--    If the index kept counting only live rows, a second managed session could
--    be created on a checkout that already has a stopped one, and `live_managed`
--    would then return two for one checkout — the exact duplicate this index
--    exists to prevent, reachable only by stopping something first.
--
--    Note this is the opposite answer from node capacity and port leases, which
--    deliberately still exclude `stopped` (it occupies nothing). Both live in
--    `crates/nook-control/src/session_status.rs` now, as `DECLARED` and `LIVE`,
--    so the two questions cannot be confused for one list again.
DROP INDEX IF EXISTS sessions_one_managed_per_checkout;

CREATE UNIQUE INDEX IF NOT EXISTS sessions_one_managed_per_checkout
    ON public.sessions (checkout_id)
    WHERE managed AND checkout_id IS NOT NULL
      AND status IN ('starting', 'running', 'detached', 'stopped');
