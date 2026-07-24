-- Platform-issued cross-tenant identity key (MAIN-12).
--
-- Membership across tenants used to be resolved by matching `users.email`,
-- which is not a verified attribute anywhere in the system: anyone who could
-- create an authenticable `users` row with a victim's email string could reach
-- the victim's tenants. `person_id` replaces that string with a value the
-- platform issues and an attacker cannot forge.
--
-- The default is `gen_random_uuid()`, so:
--   * every EXISTING row is backfilled with its own distinct value — the
--     default is volatile, so Postgres evaluates it per row on ADD COLUMN, and
--     no value is inferred from email or any other user-supplied attribute; and
--   * every NEW row gets a fresh value automatically, so the creation paths
--     (OIDC login_identity, local_auth::create, seed) need no change.
--
-- Idempotent (IF NOT EXISTS) so a database that already has the column
-- converges instead of failing.
ALTER TABLE public.users
    ADD COLUMN IF NOT EXISTS person_id uuid NOT NULL DEFAULT gen_random_uuid();

-- person_id is now the cross-tenant join key for membership resolution, so
-- index it. Not unique: two tenants' rows for the same person deliberately
-- share a person_id (that linkage is MAIN-5/6, not here), and today every row
-- simply has its own.
CREATE INDEX IF NOT EXISTS users_person_id_idx ON public.users (person_id);
