-- The deployment operator can administer a tenant's settings.
--
-- `tenant.manage` describes itself as "Administer a tenant: members, settings"
-- and was held only by `tenant_admin` — so a deployment operator, who can
-- already stage a tenant's CA, move it between orgs and revoke its nodes, could
-- not throw its automation switches. That gap is what made "the loops did not
-- fire for my PM" unfixable without switching into their team: the switches are
-- per-tenant, and the only role holding the permission was one inside the team.
--
-- Granting it confers nothing else today: before this, `tenant.manage` gated no
-- call site at all — the operator tenant-switch endpoints are its first two.
INSERT INTO role_permissions (role_key, permission_key) VALUES
    ('operator', 'tenant.manage')
ON CONFLICT DO NOTHING;
