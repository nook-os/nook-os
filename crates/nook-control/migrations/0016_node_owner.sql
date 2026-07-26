-- MAIN-119: record node ownership. A node is scoped only to its tenant today;
-- every later ticket in this epic (session-start authz, per-member visibility,
-- ownership-aware dispatch) needs a stored owner. This records and exposes it
-- WITHOUT changing any behavior yet.
--
-- Idempotent and additive: a nullable column + index, no existing column
-- touched. `owner_person_id` is a PERSON (cross-tenant identity), matching how
-- membership and the notebook already key on person rather than the per-tenant
-- user row.
ALTER TABLE public.nodes ADD COLUMN IF NOT EXISTS owner_person_id uuid;

CREATE INDEX IF NOT EXISTS nodes_owner_person_id_idx ON public.nodes (owner_person_id);

-- Backfill existing nodes to their tenant OWNER's person. A node carries no link
-- to the join token that created it (join spends the token but stores no
-- reference), so a per-node minter is not resolvable for existing rows — the
-- join/enroll paths set the minter going forward (AC-2). Only NULLs are filled,
-- so re-running converges and never overwrites an owner already recorded.
-- A tenant with no owner-role user stays NULL (nothing to resolve to).
UPDATE public.nodes n
   SET owner_person_id = (
       SELECT u.person_id
         FROM public.users u
        WHERE u.tenant_id = n.tenant_id
          AND u.role = 'owner'
        ORDER BY u.created_at
        LIMIT 1
   )
 WHERE n.owner_person_id IS NULL;
