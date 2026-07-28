-- MAIN-178: Discord-style channel categories. A category groups channels within
-- a tenant/org and is scoped exactly like a channel (owner_type/owner_id).
-- Channels gain an optional category and a position for ordering.

CREATE TABLE IF NOT EXISTS chat_channel_categories (
    id uuid PRIMARY KEY,
    owner_type text NOT NULL,
    owner_id uuid NOT NULL,
    name text NOT NULL,
    position int NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now()
);

-- List a scope's categories in display order without a scan.
CREATE INDEX IF NOT EXISTS chat_channel_categories_owner_idx
    ON chat_channel_categories (owner_type, owner_id, position);

-- A channel's category. ON DELETE SET NULL is the whole point (AC-3): removing a
-- category un-categorizes its channels, never deletes them. `position` orders
-- channels within a category (or within the uncategorized bucket). Existing rows
-- default to NULL / 0 — byte-identical until an admin assigns them.
ALTER TABLE chat_channels
    ADD COLUMN IF NOT EXISTS category_id uuid
        REFERENCES chat_channel_categories (id) ON DELETE SET NULL;
ALTER TABLE chat_channels
    ADD COLUMN IF NOT EXISTS position int NOT NULL DEFAULT 0;
