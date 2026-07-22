-- Personal access tokens: a credential that IS a person, for tooling that
-- isn't a browser (`nook login`, scripts, agents acting on someone's behalf).
--
-- Distinct from a node token on purpose. A node token authenticates a machine
-- and the control plane confines it to that machine, so one compromised box
-- can't start work on every other one — which is exactly why it can't be the
-- credential for driving the fleet. This is the other half of that trade.
--
-- ── Why this is 0002 and not an edit to 0001 ────────────────────────────────
-- 0001 has been applied to databases that hold real data and cannot be
-- recreated with `down -v`. An applied migration is immutable from that moment
-- on: its checksum is what proves the schema in front of you is the schema the
-- repo describes. Editing it and re-recording the checksum makes that proof
-- say "verified" without anything having been verified. Schema changes are new
-- numbered files from here.

CREATE TABLE IF NOT EXISTS user_tokens (
    id            UUID PRIMARY KEY,
    tenant_id     UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    user_id       UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- SHA-256 of the token. The plaintext is shown once, at creation, and is
    -- never stored — a leaked database yields no usable credential.
    token_hash    TEXT NOT NULL UNIQUE,
    name          TEXT NOT NULL DEFAULT '',
    last_used_at  TIMESTAMPTZ,
    -- NULL means it doesn't expire. Revocation is a row delete.
    expires_at    TIMESTAMPTZ,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_user_tokens_user ON user_tokens (user_id);
