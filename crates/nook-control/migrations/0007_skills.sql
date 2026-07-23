-- Skills taught to the fleet.
--
-- The control plane stores them so that "every agent on every machine knows
-- this" is a property of the fleet rather than of whichever nodes happened to
-- be online when someone ran `nook teach`. A node that was offline, or that
-- joins next week, converges on register — which is only possible if the
-- content lives somewhere other than the fan-out that delivered it.
--
-- Idempotent (CREATE ... IF NOT EXISTS) so a database that already received
-- this by other means converges instead of failing.
CREATE TABLE IF NOT EXISTS skills (
    id          uuid PRIMARY KEY,
    tenant_id   uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    -- The directory an agent reads it from: ~/.claude/skills/<name>/SKILL.md.
    -- Constrained at the API boundary to a safe slug, because it becomes a
    -- path component on every machine in the fleet.
    name        text NOT NULL,
    content     text NOT NULL,
    -- Lets a node skip a rewrite it already has, and lets an operator see at a
    -- glance whether two machines really do have the same thing.
    sha256      text NOT NULL,
    updated_at  timestamptz NOT NULL DEFAULT now(),
    updated_by  uuid REFERENCES users (id) ON DELETE SET NULL
);

-- One skill per name per tenant: teaching the same name again REPLACES it,
-- which is what "I improved the skill, push it everywhere" has to mean. Two
-- rows with one name would make the fleet's state depend on row order.
CREATE UNIQUE INDEX IF NOT EXISTS skills_tenant_name_key
    ON skills (tenant_id, name);
