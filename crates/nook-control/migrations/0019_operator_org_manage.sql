-- Give `operator` the `org.manage` permission.
--
-- 0015 gave it only to `org_admin`, reasoning that running a deployment and
-- administering an org are different jobs. They are — but the bootstrap grant
-- makes `operator` the ONLY role a fresh deployment has, and nobody holds
-- `org_admin` until somebody appoints them. So orgs could never be created:
-- the layer existed and was unreachable.
--
-- This is the same shape as the `rbac.grant` fix in 0018 and the reason a test
-- now asserts the class: every permission an `/operator/*` route requires must
-- be held by `operator`, or the surface has a route its own role cannot call.
--
-- Orgs are deployment STRUCTURE, not tenant content. Holding this grants no
-- sight of anybody's work — that is visibility policy — and no reach into
-- session content, which is not a permission at all.
INSERT INTO role_permissions (role_key, permission_key) VALUES
    ('operator', 'org.manage')
ON CONFLICT DO NOTHING;
