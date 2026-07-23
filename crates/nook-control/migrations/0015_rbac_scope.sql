-- The scope tree: deployment → org → tenant.
--
-- One shared control plane has to serve federated teams, and "can X see Y" has
-- to be answerable without walking a graph. So the scope tree is a TREE: a
-- tenant belongs to exactly one org, and a binding at any scope grants at that
-- scope and every descendant. A person contracting for two companies has two
-- tenants, not one tenant in two orgs.
--
-- The same role at a different scope is the whole trick. A self-hosted operator
-- is `operator @ deployment`; a managed team's admin is `operator @ org:X`.
-- Not two concepts — one concept, two rows.

CREATE TABLE IF NOT EXISTS orgs (
    id          UUID PRIMARY KEY,
    name        TEXT NOT NULL,
    slug        TEXT NOT NULL UNIQUE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

ALTER TABLE tenants ADD COLUMN IF NOT EXISTS org_id UUID REFERENCES orgs (id);

-- Everything that exists today belongs to one org. Created with a fixed uuid so
-- re-running is a no-op rather than a second default org.
INSERT INTO orgs (id, name, slug)
VALUES ('00000000-0000-0000-0000-0000000000a1', 'Default', 'default')
ON CONFLICT (id) DO NOTHING;

UPDATE tenants SET org_id = '00000000-0000-0000-0000-0000000000a1' WHERE org_id IS NULL;

-- Only enforced after the backfill, so an existing database converges rather
-- than failing on rows that predate the column.
DO $$
BEGIN
    ALTER TABLE tenants ALTER COLUMN org_id SET NOT NULL;
EXCEPTION
    WHEN others THEN NULL;
END $$;

CREATE INDEX IF NOT EXISTS idx_tenants_org ON tenants (org_id);

-- ── the catalog ─────────────────────────────────────────────────────────────
--
-- Permissions are rows so they can be listed, granted and audited. The Rust
-- side mirrors them as an enum — see auth/perm.rs — which is what stops a call
-- site naming a permission that does not exist. These rows are the data half;
-- the enum is the half the compiler checks.

CREATE TABLE IF NOT EXISTS permissions (
    key         TEXT PRIMARY KEY,
    description TEXT NOT NULL DEFAULT ''
);

CREATE TABLE IF NOT EXISTS roles (
    key         TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    -- Built-in roles cannot be deleted; a deployment that lost `operator`
    -- would have no way back in.
    builtin     BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE TABLE IF NOT EXISTS role_permissions (
    role_key        TEXT NOT NULL REFERENCES roles (key) ON DELETE CASCADE,
    permission_key  TEXT NOT NULL REFERENCES permissions (key) ON DELETE CASCADE,
    PRIMARY KEY (role_key, permission_key)
);

-- ── bindings ────────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS role_bindings (
    id            UUID PRIMARY KEY,
    -- `user` today. Left open because a service principal is a binding too,
    -- and adding one should not be a schema change.
    subject_type  TEXT NOT NULL DEFAULT 'user',
    subject_id    UUID NOT NULL,
    role_key      TEXT NOT NULL REFERENCES roles (key) ON DELETE CASCADE,
    scope_type    TEXT NOT NULL,
    -- NULL for `deployment`: there is exactly one, and giving it a synthetic
    -- id would invite code that compares against the wrong constant.
    scope_id      UUID,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by    UUID REFERENCES users (id) ON DELETE SET NULL,
    CONSTRAINT role_bindings_scope_type_check
        CHECK (scope_type IN ('deployment', 'org', 'tenant')),
    -- The shape of the tree, enforced by the database rather than by care:
    -- deployment has no id, org and tenant must have one.
    CONSTRAINT role_bindings_scope_id_check
        CHECK ((scope_type = 'deployment' AND scope_id IS NULL)
            OR (scope_type <> 'deployment' AND scope_id IS NOT NULL))
);

-- Granting the same role twice at the same scope is one grant, so re-running a
-- bootstrap or an installer converges.
CREATE UNIQUE INDEX IF NOT EXISTS idx_role_bindings_unique
    ON role_bindings (subject_type, subject_id, role_key, scope_type,
                      COALESCE(scope_id, '00000000-0000-0000-0000-000000000000'::uuid));

-- Resolution asks "what does this subject hold?" on every authorized request.
CREATE INDEX IF NOT EXISTS idx_role_bindings_subject
    ON role_bindings (subject_type, subject_id);

-- ── seed ────────────────────────────────────────────────────────────────────
--
-- NOTE: session content is deliberately ABSENT from this catalog and must stay
-- absent. There is no permission here that grants reading a tenant's terminal,
-- prompts or code, and adding one would defeat the guarantee the product rests
-- on. Session access is membership, checked elsewhere, on purpose.

INSERT INTO permissions (key, description) VALUES
    ('org.view',        'See that an org and its tenants exist'),
    ('org.manage',      'Rename an org, move tenants between orgs'),
    ('tenant.view',     'See a tenant exists, and its membership counts'),
    ('tenant.manage',   'Administer a tenant: members, settings'),
    ('node.view',       'See nodes: name, status, resources, session counts'),
    ('node.manage',     'Revoke or remove a node'),
    ('audit.view',      'Read audit records'),
    ('ca.rotate',       'Rotate a tenant certificate authority'),
    ('policy.view',     'Read an org visibility policy'),
    ('policy.manage',   'Change an org visibility policy')
ON CONFLICT (key) DO NOTHING;

INSERT INTO roles (key, name, description, builtin) VALUES
    ('operator',     'Operator',     'Runs this deployment or org. Sees metadata, never session content.', TRUE),
    ('org_admin',    'Org admin',    'Administers an org and the tenants under it.', TRUE),
    ('tenant_admin', 'Tenant admin', 'Administers one tenant.', TRUE),
    ('member',       'Member',       'Ordinary access to a tenant.', TRUE)
ON CONFLICT (key) DO NOTHING;

INSERT INTO role_permissions (role_key, permission_key) VALUES
    -- The operator sees the shape of the deployment and can act on the
    -- infrastructure it runs. It cannot administer somebody's tenant, and it
    -- has no route to their session content because none exists.
    ('operator', 'org.view'),
    ('operator', 'tenant.view'),
    ('operator', 'node.view'),
    ('operator', 'node.manage'),
    ('operator', 'audit.view'),
    ('operator', 'ca.rotate'),
    ('operator', 'policy.view'),
    ('operator', 'policy.manage'),

    ('org_admin', 'org.view'),
    ('org_admin', 'org.manage'),
    ('org_admin', 'tenant.view'),
    ('org_admin', 'node.view'),
    ('org_admin', 'audit.view'),
    ('org_admin', 'policy.view'),
    ('org_admin', 'policy.manage'),

    -- A tenant admin runs their own tenant and nothing above it. Deliberately
    -- WITHOUT ca.rotate: the CA is the deployment's trust root, and a tenant
    -- admin rotating it is a tenant reaching upward.
    ('tenant_admin', 'tenant.view'),
    ('tenant_admin', 'tenant.manage'),
    ('tenant_admin', 'node.view'),
    ('tenant_admin', 'node.manage'),
    ('tenant_admin', 'audit.view'),
    ('tenant_admin', 'policy.view'),

    ('member', 'tenant.view'),
    ('member', 'node.view')
ON CONFLICT DO NOTHING;

-- Existing tenant owners and admins keep what they had, as bindings.
INSERT INTO role_bindings (id, subject_type, subject_id, role_key, scope_type, scope_id)
SELECT gen_random_uuid(), 'user', u.id, 'tenant_admin', 'tenant', u.tenant_id
FROM users u
WHERE u.role IN ('owner', 'admin')
ON CONFLICT DO NOTHING;
