-- A managed session gains a SHARD (MAIN-446).
--
-- Several reviewers may now run for one repo, and on a fleet with a single
-- `role=loop` node they all run on that node — in the same clone, because the
-- review loop takes primary clones only. 0042's unique index keys on
-- `(checkout_id, managed_purpose)`, so the second one would be refused at
-- insert and the declaration could never be satisfied.
--
-- The shard is what tells them apart, and it is a stored column rather than a
-- position in some list for the reason 0042 gave about the purpose: it is half
-- the key the index arbitrates on, so it has to exist on the INSERT. It is also
-- what a restart re-sends — a reviewer that came back as shard 0 when it had
-- been shard 2 would re-review another reviewer's PRs and skip its own.
--
-- `managed_shards` is the divisor. Stored beside the index rather than re-read
-- from `workspaces.review_loop_max_replicas`, because the ceiling can change
-- under a running session: the two halves are only meaningful together, and a
-- session that kept its index while silently taking a new divisor would review
-- a different set of PRs than the one it was placed to review.
--
-- `0 of 1` is the whole of the work, which is exactly what every existing row
-- is: one review loop per repo, and a person's terminal, which has no shard at
-- all. So the defaults make this a no-op for every row in the fleet.
ALTER TABLE public.sessions
    ADD COLUMN IF NOT EXISTS managed_shard integer NOT NULL DEFAULT 0;
ALTER TABLE public.sessions
    ADD COLUMN IF NOT EXISTS managed_shards integer NOT NULL DEFAULT 1;

DROP INDEX IF EXISTS sessions_one_managed_per_checkout_purpose;

-- One managed session per checkout per purpose PER SHARD. The status list is
-- 0041's DECLARED set, carried over from 0042 unchanged and for its reasons: a
-- stopped managed session still satisfies its declaration, so it still holds
-- its slot, and `live_managed` makes the same call.
--
-- The divisor is deliberately NOT in the key. Two sessions on one checkout with
-- the same index and different divisors are not two slots — they are one slot
-- described two ways, and letting both exist would double-review whatever the
-- older divisor put in that index.
CREATE UNIQUE INDEX IF NOT EXISTS sessions_one_managed_per_checkout_purpose_shard
    ON public.sessions (checkout_id, managed_purpose, managed_shard)
    WHERE managed AND checkout_id IS NOT NULL
      AND status IN ('starting', 'running', 'detached', 'stopped');
