-- Give `tenants.org_id` a default.
--
-- 0015 made the column NOT NULL, which was right — the scope tree has to be a
-- tree, and a tenant with no org is a node with no parent. But it left nine
-- existing `INSERT INTO tenants (id, name, slug)` call sites failing, including
-- new-user signup and local registration. NOT NULL without a default does not
-- state a requirement, it breaks every writer that predates it.
--
-- A default is the right shape rather than fixing the nine call sites, because
-- the same trap is waiting for the tenth. A tenant created without naming an
-- org belongs to the default org; a deployment that grows more orgs assigns
-- them explicitly, which is a deliberate act rather than something a caller can
-- forget.
ALTER TABLE tenants
    ALTER COLUMN org_id SET DEFAULT '00000000-0000-0000-0000-0000000000a1';

-- Anything created between 0015 and here (there should be nothing, but a
-- half-applied migration set is exactly when this matters).
UPDATE tenants SET org_id = '00000000-0000-0000-0000-0000000000a1' WHERE org_id IS NULL;
