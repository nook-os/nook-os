-- Notifications: the inbox, and where else to send things.
--
-- Distinct from `events`, deliberately. `events` is the append-only record of
-- everything that happened — it must stay complete and nobody "reads" it.
-- A notification is a thing somebody should SEE, it has read state, and it may
-- have been delivered somewhere outside NookOS entirely. Collapsing the two
-- would mean either an activity log with holes in it or an inbox with ten
-- thousand unread rows.
CREATE TABLE IF NOT EXISTS notifications (
    id          UUID PRIMARY KEY,
    tenant_id   UUID NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    -- NULL means "everyone in the tenant". Per-user targeting exists for when
    -- RBAC lands; until then most notifications are tenant-wide.
    user_id     UUID REFERENCES users (id) ON DELETE CASCADE,
    -- info | success | warning | error — drives colour and whether a toast
    -- sticks around until dismissed.
    level       TEXT NOT NULL DEFAULT 'info',
    title       TEXT NOT NULL,
    body        TEXT NOT NULL DEFAULT '',
    -- The dotted event kind that produced this, when there was one. Lets a
    -- channel filter on it and a client group by it.
    kind        TEXT NOT NULL DEFAULT 'custom',
    -- Somewhere to go when clicked: a task, a session, a node.
    link        TEXT,
    -- Anything a channel might template with.
    payload     JSONB NOT NULL DEFAULT '{}'::jsonb,
    read_at     TIMESTAMPTZ,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- The inbox query is "this tenant, newest first", with unread counted.
CREATE INDEX IF NOT EXISTS idx_notifications_inbox
    ON notifications (tenant_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_notifications_unread
    ON notifications (tenant_id) WHERE read_at IS NULL;

-- Where notifications go besides the UI.
--
-- One table for every kind rather than a table per provider: they differ only
-- in the shape of `config`, and a column per provider would mean a migration
-- every time somebody wants Discord. The `kind` selects the code that knows
-- how to read `config`.
CREATE TABLE IF NOT EXISTS notification_channels (
    id          UUID PRIMARY KEY,
    tenant_id   UUID NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    -- webhook | slack | discord | telegram | twilio | ntfy | …
    kind        TEXT NOT NULL,
    -- What a person calls it: "team slack", "my phone".
    name        TEXT NOT NULL,
    -- Provider-specific. Holds secrets (bot tokens, webhook URLs), so it is
    -- never returned by the API — see `NotificationChannel` in nook-types.
    config      JSONB NOT NULL DEFAULT '{}'::jsonb,
    enabled     BOOLEAN NOT NULL DEFAULT TRUE,
    -- Only deliver these levels. Empty means all.
    levels      TEXT[] NOT NULL DEFAULT '{}',
    -- Only deliver these event kinds, prefix-matched ("task." matches
    -- "task.created"). Empty means all.
    kinds       TEXT[] NOT NULL DEFAULT '{}',
    -- Last delivery outcome, so a channel that has quietly been failing for a
    -- week is visible instead of merely silent.
    last_ok_at  TIMESTAMPTZ,
    last_error  TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name)
);

CREATE INDEX IF NOT EXISTS idx_notification_channels_tenant
    ON notification_channels (tenant_id) WHERE enabled;
