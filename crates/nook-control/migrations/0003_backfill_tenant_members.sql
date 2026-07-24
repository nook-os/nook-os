-- Backfill tenant_members so it is the single source of truth for EVERY user
-- (MAIN-4 AC-7), not only OIDC ones.
--
-- `login_identity` has always written a `tenant_members` grant, but the local
-- path (`local_auth::create`) and any earlier-seeded rows did not — so a local
-- user was a member of no tenant as far as `tenant_members` was concerned, and
-- now that `AuthCtx` enforces membership per request, such a user would be
-- locked out of their own tenant. Give every existing user a grant for their
-- home tenant if they lack one.
--
-- Idempotent: the NOT EXISTS guard (and the unique key on
-- (tenant_id, principal_type, principal_id)) mean a re-run is a no-op, and the
-- OIDC users who already have a grant are skipped.
INSERT INTO public.tenant_members (id, tenant_id, principal_type, principal_id, role)
SELECT gen_random_uuid(), u.tenant_id, 'user', u.id, u.role
FROM public.users u
WHERE NOT EXISTS (
    SELECT 1 FROM public.tenant_members tm
    WHERE tm.tenant_id = u.tenant_id
      AND tm.principal_type = 'user'
      AND tm.principal_id = u.id
);
