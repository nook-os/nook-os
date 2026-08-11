-- Where a person's uploaded file is recorded (MAIN-532).
--
-- Deliberately knows nothing about tickets, comments or messages: a row here is
-- content plus who put it there, and a consumer that wants to attach it brings
-- its own join table. `storage_key` is what the bytes are under in whichever
-- `ArtifactStore` the deployment configured, kept as a column rather than
-- recomputed from the id so a prefix change never orphans what is already
-- stored.
CREATE TABLE IF NOT EXISTS user_content (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    uploaded_by uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    filename text NOT NULL,
    content_type text NOT NULL,
    size_bytes bigint NOT NULL,
    sha256 text NOT NULL,
    storage_key text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS user_content_tenant_idx
    ON user_content (tenant_id, created_at DESC);
