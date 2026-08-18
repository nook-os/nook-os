-- Named secret items (MAIN-625).
--
-- 0081 is free on BOTH tracks, which is the rule the numbering follows: 0080 is
-- the highest either set uses — MAIN-331's `0080_email_link_investigation`
-- claimed it on both engines while this branch was open — so the twins land
-- together at 0081 and lockstep holds. Both halves were renumbered together
-- rather than one shifting alone, which is the trap MAIN-502 died on.
--
-- Deliberately NOT `workspace_secrets`. That table holds one blob per workspace,
-- named `.env`, sealed with a password the server never sees; it stays exactly
-- as it is (NG-2). This is the other thing: individually named values the
-- control plane CAN read, because delivering one into a session or a job
-- container is the whole point and a human unlocking a browser is not on that
-- path.
CREATE TABLE IF NOT EXISTS secret_items (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,

    -- `tenant` | `workspace` | `node`, with `scope_id` naming the row of that
    -- kind. Polymorphic and so unreferenced: a foreign key can only point at
    -- one table, and three nullable columns with a check constraint would be
    -- the same rule spelled at more length. Deleting a workspace or a node
    -- therefore leaves its items behind, and NOTHING collects them today: they
    -- list with an unresolvable uuid and can only be removed through the API by
    -- id, since `nook secrets rm` cannot resolve a deleted scope. Harmless —
    -- ids are never reused, so an orphan is unreachable rather than dangerous —
    -- but it is a real gap, not a described mechanism.
    scope text NOT NULL,
    scope_id uuid NOT NULL,
    name text NOT NULL,

    -- Envelope encryption (AC-2). The value is sealed under a data key of its
    -- own; only that key is wrapped by the app key. Two columns rather than one
    -- blob because the whole point is that an app-key rotation rewrites the
    -- second and never the first.
    value_enc bytea NOT NULL,
    dek_wrapped bytea NOT NULL,

    -- Who wrote it last. NULL for a row written by something with no person
    -- behind it; the event ledger carries the same fact for the audit.
    updated_by uuid,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- AC-1: unique per scope and name. `scope_id` carries the tenant for a
-- tenant-scoped item, so the triple is unique on its own and `tenant_id` need
-- not be in the key.
CREATE UNIQUE INDEX IF NOT EXISTS secret_items_scope_name_key
    ON secret_items (scope, scope_id, name);

-- Listing is always per tenant.
CREATE INDEX IF NOT EXISTS secret_items_tenant_idx ON secret_items (tenant_id);
