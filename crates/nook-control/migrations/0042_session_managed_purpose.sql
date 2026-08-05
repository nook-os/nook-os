-- A managed session gains a PURPOSE (MAIN-326).
--
-- The reconciler now runs two declarations per workspace: the workspace's own
-- access spec (children 1-6) and the control plane's always-on review loop on
-- the `role=loop` node. Both are managed and both land in the repo's clone, so
-- keying uniqueness on `checkout_id` alone would make each read as the other's
-- duplicate — one would be refused at insert, and whichever won would be
-- stopped by the next pass of the declaration that did not create it.
--
-- `NOT NULL DEFAULT 'access'` because that is what every existing row is: the
-- only managed sessions before this card were access sessions, and an unmanaged
-- session's purpose is never read.
ALTER TABLE public.sessions
    ADD COLUMN IF NOT EXISTS managed_purpose text NOT NULL DEFAULT 'access';

DROP INDEX IF EXISTS sessions_one_managed_per_checkout;

-- One managed session per checkout PER PURPOSE. The status list is 0041's
-- DECLARED set, `stopped` included, carried over deliberately rather than
-- retyped from 0032: a stopped managed session still satisfies its declaration,
-- so it must still hold its slot, and `live_managed` makes the same call. The
-- `checkout_id IS NOT NULL` guard carries over for the reason 0032 gave — nulls
-- are distinct, so a stray null-checkout managed row would otherwise slip past.
CREATE UNIQUE INDEX IF NOT EXISTS sessions_one_managed_per_checkout_purpose
    ON public.sessions (checkout_id, managed_purpose)
    WHERE managed AND checkout_id IS NOT NULL
      AND status IN ('starting', 'running', 'detached', 'stopped');
