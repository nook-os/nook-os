-- The SQLite twin of 0035, hand-authored in the same commit (CLAUDE.md).
-- Pure DML over a table both tracks already have, so it is identical.
INSERT INTO role_permissions (role_key, permission_key) VALUES
    ('operator', 'tenant.manage')
ON CONFLICT DO NOTHING;
