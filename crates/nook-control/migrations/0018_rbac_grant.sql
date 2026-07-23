-- Appointing a role is its own authority.
--
-- Granting was gated on `org.manage`, which `operator` does not hold — so the
-- bootstrap operator, the one person a fresh deployment has, could not appoint
-- anybody. Widening `org.manage` to fix it would have conflated two different
-- powers: managing orgs (renaming them, moving tenants between them) and
-- deciding who may run the deployment.
--
-- Separate, so "who can appoint operators" is one row somebody can grep for
-- rather than a consequence of a permission granted for another reason.
INSERT INTO permissions (key, description) VALUES
    ('rbac.grant', 'Grant or revoke a role binding')
ON CONFLICT (key) DO NOTHING;

-- Never `tenant_admin`: a tenant administering itself must not be able to
-- appoint someone above it.
INSERT INTO role_permissions (role_key, permission_key) VALUES
    ('operator',  'rbac.grant'),
    ('org_admin', 'rbac.grant')
ON CONFLICT DO NOTHING;
