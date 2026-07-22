-- Local accounts: a username and password on the user, for people with no
-- identity provider to point at.
--
-- Rewritten before it shipped anywhere. An earlier draft put the password in
-- its own `local_credentials` table; this is the same thing with one fewer
-- join, and the table never held a row on any deployment. Idempotent, so a
-- database that already took the earlier shape converges.
DROP TABLE IF EXISTS local_credentials;

ALTER TABLE users ADD COLUMN IF NOT EXISTS username TEXT;

-- NULL for anyone who signs in through an identity provider — they have no
-- password here and must not be given one, or there are two ways to become
-- them and only one of them is revocable by the provider.
--
-- A full PHC string: algorithm, parameters, salt and hash together, so the
-- cost can be raised later and every hash written before it still verifies.
--
-- NOTE for anything that reads this table: `password_hash` is deliberately
-- absent from the `User` struct in nook-types. `SELECT *` into `User` ignores
-- it, which is what keeps a hash out of API responses, log lines and event
-- payloads by construction rather than by remembering.
ALTER TABLE users ADD COLUMN IF NOT EXISTS password_hash TEXT;

-- Local sign-in is by username, unambiguous within a tenant and
-- case-insensitive — a login form where Alice and alice are different people
-- is a phishing aid rather than a feature.
CREATE UNIQUE INDEX IF NOT EXISTS users_tenant_username_unique
    ON users (tenant_id, lower(username))
    WHERE username IS NOT NULL;

-- Which sign-in method a tenant uses. NULL means undecided: nobody has signed
-- in yet, so either is still available.
--
-- Why this is one-way. The two methods disagree about who owns an identity.
-- OIDC says the provider does, and a person is whoever the issuer says they
-- are; local says this database does. Allow both at once and
-- alice@example.com can exist twice — once as an OIDC subject, once as a local
-- row — with different ids, different roles and different grants, and no
-- reliable answer to "which alice?". That question lands squarely on RBAC,
-- where it has to have exactly one answer.
--
-- So the first successful sign-in decides, and after that the other door is
-- closed. Reopening it is a deliberate migration with a human deciding how to
-- merge, not a flag someone flips at 2am.
ALTER TABLE tenants ADD COLUMN IF NOT EXISTS auth_mode TEXT;

ALTER TABLE tenants DROP CONSTRAINT IF EXISTS tenants_auth_mode_check;
ALTER TABLE tenants ADD CONSTRAINT tenants_auth_mode_check
    CHECK (auth_mode IS NULL OR auth_mode IN ('oidc', 'local'));
