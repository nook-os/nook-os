-- Per-tenant certificate authorities, for mutual TLS between the node agent
-- and the control plane.
--
-- One CA per TENANT, not per instance. A single control plane serves many
-- tenants, so a leaked signing key must compromise one customer's machines
-- rather than the whole fleet.
--
-- The key lives here, encrypted, rather than in a directory of PEM files
-- (Kubernetes' /etc/kubernetes/pki model). Per-tenant keys don't fit a
-- filesystem layout, and keeping them in Postgres is what lets several control
-- plane instances share them without shared storage — the same reasoning as
-- the existing lease/LISTEN-NOTIFY multi-instance design.
CREATE TABLE IF NOT EXISTS tenant_cas (
    id           UUID PRIMARY KEY,
    tenant_id    UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,

    -- Trust and signing are different things, and rotation is moving a CA
    -- through these states:
    --   staged   trusted by nodes, not yet signing  (distribute)
    --   active   trusted AND the signer             (switch)
    --   retiring trusted, no longer signing         (drain, then retire)
    -- Everything that verifies accepts any row here; only 'active' signs.
    state        TEXT NOT NULL DEFAULT 'staged'
                 CHECK (state IN ('staged', 'active', 'retiring')),

    cert_pem     TEXT NOT NULL,
    -- Sealed with the instance vault key (SECRETS_KEY) via crate::crypto, the
    -- same at-rest scheme as git credentials and workspace secrets.
    key_enc      BYTEA NOT NULL,

    -- SHA-256 of the certificate DER, checked every time the key is loaded. A
    -- key that decrypts but whose certificate doesn't match its recorded
    -- fingerprint is tampering or corruption — refuse to sign with it rather
    -- than quietly issuing certificates from something unexpected.
    fingerprint  TEXT NOT NULL,

    not_after    TIMESTAMPTZ NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    retired_at   TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS idx_tenant_cas_tenant ON tenant_cas (tenant_id);

-- Exactly one signer per tenant, enforced by the database rather than by
-- convention: two active CAs would make "which key signed this?" unanswerable
-- and quietly break the retirement guard.
CREATE UNIQUE INDEX IF NOT EXISTS tenant_cas_one_active
    ON tenant_cas (tenant_id) WHERE state = 'active';

-- Which CA signed a node's current leaf, and when that leaf dies.
--
-- This is what makes the retirement guard a check in code instead of a runbook
-- step: a CA may not be dropped from the trust bundle while it still has an
-- unexpired leaf out there. `revoked_at` keeps "offline for six months" and
-- "compromised, cut it off" distinguishable — expiry alone cannot tell them
-- apart, and a node must be able to re-enrol on its own key after any outage.
ALTER TABLE nodes ADD COLUMN IF NOT EXISTS ca_id UUID REFERENCES tenant_cas(id);
ALTER TABLE nodes ADD COLUMN IF NOT EXISTS cert_not_after TIMESTAMPTZ;
ALTER TABLE nodes ADD COLUMN IF NOT EXISTS revoked_at TIMESTAMPTZ;
CREATE INDEX IF NOT EXISTS idx_nodes_ca ON nodes (ca_id);
