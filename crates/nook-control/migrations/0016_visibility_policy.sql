-- What an operator may see of a tenant's work, per org.
--
-- Stored as versioned ROWS, never updated in place, because the question this
-- has to answer is "what could my employer see on March 12" — and a mutable
-- settings row cannot answer it. Each change appends; the current value is the
-- newest row for that (org, field).
--
-- Default closed: the ABSENCE of a row means off. A new org therefore starts at
-- minimum visibility without anything having to insert defaults, and a bug in
-- the seeding path cannot accidentally open something.
CREATE TABLE IF NOT EXISTS org_visibility_policy (
    id          UUID PRIMARY KEY,
    org_id      UUID NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
    -- One of the policy-gated fields; the Rust side mirrors these as an enum.
    field       TEXT NOT NULL,
    enabled     BOOLEAN NOT NULL,
    changed_by  UUID REFERENCES users (id) ON DELETE SET NULL,
    changed_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- "What is the current value" is the hot query, and it is a per-field newest.
CREATE INDEX IF NOT EXISTS idx_org_visibility_current
    ON org_visibility_policy (org_id, field, changed_at DESC);
