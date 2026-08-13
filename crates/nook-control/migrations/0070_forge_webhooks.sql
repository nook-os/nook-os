-- The door GitHub can knock on (MAIN-554).
--
-- Every fact about a PR is discovered by a timer today because nothing inbound
-- exists. This is the receiver and nothing more: a signed delivery is recorded,
-- never acted on (NG-1) — children 2-5 read these rows.
--
-- 0069 and not the 0067 the card named: a number is claimed on BOTH tracks at
-- once, and two paired migrations landed on each while this card was open —
-- `0067_build_port_leases` (MAIN-552) and `0068_workspace_build_loop`
-- (MAIN-385). Counting from either track alone would have collided with both.
ALTER TABLE public.workspaces ADD COLUMN IF NOT EXISTS webhook_secret_enc bytea;

CREATE TABLE IF NOT EXISTS forge_deliveries (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    -- The workspace is named in the PATH and is the key. `repository.full_name`
    -- cannot be: `workspaces_remote_idx` is UNIQUE on
    -- (tenant_id, git_remote_normalized), so two tenants may legitimately hold
    -- the same repo and no fleet-wide remote→workspace lookup exists.
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    delivery_id text NOT NULL,
    event text NOT NULL,
    action text,
    -- `''` when the delivery carries no `repository` — an organisation-level
    -- ping does not. Not nullable, because "no repository named" and "a
    -- repository named the empty string" are not two states a reader has to
    -- tell apart, and every other column here is NOT NULL by the same rule.
    repo_full_name text NOT NULL,
    payload jsonb NOT NULL,
    status text NOT NULL CHECK (status IN ('received', 'ignored', 'error')),
    error text,
    received_at timestamptz NOT NULL DEFAULT now()
);

-- AC-2: GitHub redelivers, and a redelivery is the SAME delivery. The second
-- one writes no row and is answered 200 rather than a first delivery's 202, so
-- "Redeliver" in the GitHub UI stays a safe thing for an operator to press.
CREATE UNIQUE INDEX IF NOT EXISTS forge_deliveries_unique_idx
    ON forge_deliveries (workspace_id, delivery_id);
