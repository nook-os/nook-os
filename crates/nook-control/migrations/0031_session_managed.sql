-- MAIN-316: the reconciler must be able to tell ITS sessions from everybody
-- else's, and that has to survive a restart and be the same answer on every
-- replica. Hence a column, not an inference.
--
-- Inferring "managed" from workspace + runtime + eligible node was the
-- alternative, and it is unsafe: a hand-started session in a managed workspace
-- would be counted as a replica and then KILLED when replicas dropped. The
-- reconciler only ever touches rows it marked itself.
--
-- DEFAULT false, so every session that already exists — all of them ad-hoc, by
-- definition, since nothing has reconciled yet — is invisible to it.
ALTER TABLE public.sessions ADD COLUMN IF NOT EXISTS managed boolean NOT NULL DEFAULT false;

-- Two of MAIN-316's requirements in one constraint:
--
--   AC-4 "safe under multiple replicas (one action wins)" — two replicas
--   planning the same missing session both try to insert, and the database
--   picks the winner. No lease, no advisory lock, no window.
--
--   AC-4 "no doubling per node" — the spread places at most one managed
--   session per (workspace, node), which is what makes `replicas > eligible`
--   a shortfall to report rather than a reason to stack two on one machine.
--
-- Partial on LIVE sessions only: a managed session that exited must be
-- replaceable, which is the whole of "restart a crashed managed session".
CREATE UNIQUE INDEX IF NOT EXISTS sessions_one_managed_per_node
    ON public.sessions (workspace_id, node_id)
    WHERE managed AND status IN ('starting', 'running', 'detached');
