-- Tenant invites (MAIN-6): bring a new person into a shared tenant.
--
-- An owner/admin creates a pending invite for an email + role; the invitee
-- accepts by signing in as that email and consuming the opaque token, which
-- adds their `tenant_members` grant (and a per-tenant `users` row carrying their
-- `person_id`, so the switcher immediately offers the tenant). Emailing the link
-- is MAIN-7; this issue returns it in the API and copies it in the UI.
--
-- Only the token's SHA-256 (`token_hash`) is stored — the plaintext rides only
-- in the accept link (AC-9), so a database dump does not hand out working
-- invites. Idempotent (IF NOT EXISTS) so a database that already has the table
-- converges.
CREATE TABLE IF NOT EXISTS public.invites (
    id          uuid PRIMARY KEY,
    tenant_id   uuid NOT NULL REFERENCES public.tenants(id) ON DELETE CASCADE,
    email       text NOT NULL,
    role        text NOT NULL DEFAULT 'member',
    token_hash  text NOT NULL UNIQUE,
    status      text NOT NULL DEFAULT 'pending',
    invited_by  uuid,
    created_at  timestamptz NOT NULL DEFAULT now(),
    expires_at  timestamptz NOT NULL,
    CONSTRAINT invites_role_check   CHECK (role   = ANY (ARRAY['member'::text, 'admin'::text])),
    CONSTRAINT invites_status_check CHECK (status = ANY (ARRAY['pending'::text, 'accepted'::text, 'revoked'::text]))
);

-- At most one PENDING invite per (tenant, email): re-inviting replaces rather
-- than stacks (AC-2). Accepted/revoked rows are kept for history, so the index
-- is partial on pending.
CREATE UNIQUE INDEX IF NOT EXISTS invites_one_pending_per_email
    ON public.invites (tenant_id, lower(email)) WHERE status = 'pending';

-- Accept looks up by the token hash; keep it quick.
CREATE INDEX IF NOT EXISTS invites_token_hash_idx ON public.invites (token_hash);
