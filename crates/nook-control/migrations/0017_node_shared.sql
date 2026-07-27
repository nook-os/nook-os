-- MAIN-135: a node's owner may designate it SHARED (team-usable). This first
-- unit of the shared-nodes epic (MAIN-120) adds the flag and widens VISIBILITY
-- only — a shared node becomes visible to the whole team, but session-start
-- stays owner-only (MAIN-130) until the next unit. The isolation wall closed by
-- MAIN-118 therefore stays fully closed while the designation surface lands.
--
-- Idempotent and additive: a NOT NULL column with a safe default, no existing
-- column touched. A node with no owner cannot be shared (nothing to consent) —
-- enforced at the route, not here, since the column itself is just a flag.
ALTER TABLE public.nodes ADD COLUMN IF NOT EXISTS shared boolean NOT NULL DEFAULT false;
