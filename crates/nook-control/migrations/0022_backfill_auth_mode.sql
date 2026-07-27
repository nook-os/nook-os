-- MAIN-169 AC-6: backfill tenants.auth_mode for rows still NULL.
--
-- Tenants that predate the auth-mode lock have auth_mode NULL, so
-- /auth/local/status reports local sign-in "available" even on an instance that
-- has only ever used OIDC. During an IdP outage that would surface a bare
-- password form as though it were a real sign-in method (the compounding half
-- of the MAIN-169 incident). Commit the mode each such tenant has de facto
-- already chosen, and leave genuinely ambiguous ones (both, or neither) NULL.
--
-- Idempotent: `WHERE auth_mode IS NULL` means a second run is a no-op, and a
-- database that reached this state another way converges rather than failing.

-- 'oidc': at least one federated identity, and no local password anywhere.
UPDATE tenants t
SET auth_mode = 'oidc'
WHERE t.auth_mode IS NULL
  AND EXISTS (
      SELECT 1 FROM identities i
      JOIN users u ON u.id = i.user_id
      WHERE u.tenant_id = t.id
  )
  AND NOT EXISTS (
      SELECT 1 FROM users u
      WHERE u.tenant_id = t.id AND u.password_hash IS NOT NULL
  );

-- 'local': the mirror case — at least one local password, and no identity.
UPDATE tenants t
SET auth_mode = 'local'
WHERE t.auth_mode IS NULL
  AND EXISTS (
      SELECT 1 FROM users u
      WHERE u.tenant_id = t.id AND u.password_hash IS NOT NULL
  )
  AND NOT EXISTS (
      SELECT 1 FROM identities i
      JOIN users u ON u.id = i.user_id
      WHERE u.tenant_id = t.id
  );
