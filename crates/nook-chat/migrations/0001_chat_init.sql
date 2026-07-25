-- nook-chat schema (MAIN-48 AC-5).
--
-- These tables live in the `chat` Postgres schema (the connection's search_path
-- is set to `chat`, so unqualified names resolve there), which keeps chat's
-- `_sqlx_migrations` ledger separate from the control plane's public one — the
-- two services migrate independently against the SHARED database.
--
-- The generic channel-owner model is here from day one: a channel is owned by
-- an org OR a tenant. v1 only exposes tenant channels (org channels are a
-- roadmap follow-on with no schema change). owner_id/user_id/tenant_id refer to
-- rows in the control plane's `public` schema (orgs/tenants/users) but carry no
-- cross-schema FK, so the two services stay loosely coupled; referential
-- discipline is enforced in application code.

CREATE TABLE IF NOT EXISTS chat_channels (
    id uuid PRIMARY KEY,
    owner_type text NOT NULL CHECK (owner_type IN ('org', 'tenant')),
    owner_id uuid NOT NULL,
    name text NOT NULL,
    slug text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (owner_type, owner_id, slug)
);

-- Who is in a channel. The messaging ticket reads this to authorize posts/reads.
CREATE TABLE IF NOT EXISTS chat_channel_members (
    channel_id uuid NOT NULL REFERENCES chat_channels (id) ON DELETE CASCADE,
    user_id uuid NOT NULL,
    tenant_id uuid NOT NULL,
    joined_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (channel_id, user_id)
);

-- Messages. Keyset-friendly on the UUID v7 id, like the rest of NookOS.
CREATE TABLE IF NOT EXISTS chat_messages (
    id uuid PRIMARY KEY,
    channel_id uuid NOT NULL REFERENCES chat_channels (id) ON DELETE CASCADE,
    author_id uuid NOT NULL,
    tenant_id uuid NOT NULL,
    body text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS chat_messages_channel_idx ON chat_messages (channel_id, id);
