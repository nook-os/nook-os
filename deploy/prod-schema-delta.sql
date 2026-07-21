-- Schema deltas for an already-running deployment.
--
-- The bootstrap workflow edits 0001_init.sql in place and recreates the
-- database, which is fine for a dev loop and unacceptable for a deployment
-- with real imported repos in it. Every change made to 0001_init.sql after a
-- deployment exists must also land here, written so it can be run twice.
--
--   psql "$DATABASE_URL" -f deploy/prod-schema-delta.sql

-- Why a session failed to start, so the UI can say so instead of spinning.
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS error TEXT;

-- Passkeys that unlock the vault; the app password wrapped under a key the
-- browser derives from the passkey, never the password itself.
CREATE TABLE IF NOT EXISTS user_passkeys (
    id             UUID PRIMARY KEY,
    user_id        UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    tenant_id      UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    credential_id  TEXT NOT NULL,
    label          TEXT NOT NULL DEFAULT '',
    wrapped_secret BYTEA NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at   TIMESTAMPTZ,
    UNIQUE (user_id, credential_id)
);
CREATE INDEX IF NOT EXISTS idx_user_passkeys_user ON user_passkeys (user_id);
