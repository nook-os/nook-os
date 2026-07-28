-- MAIN-116: message reactions + edit/delete with an audit trail.

-- One reaction per (message, user, emoji). The PK makes a repeat add a no-op
-- (INSERT ... ON CONFLICT DO NOTHING) so toggling is idempotent (AC-2).
CREATE TABLE IF NOT EXISTS chat_reactions (
    message_id uuid NOT NULL REFERENCES chat_messages (id) ON DELETE CASCADE,
    user_id uuid NOT NULL,
    emoji text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (message_id, user_id, emoji)
);

-- Aggregate a message's (or a page of messages') reactions without a scan.
CREATE INDEX IF NOT EXISTS chat_reactions_message_idx
    ON chat_reactions (message_id);

-- The audit trail: the prior content on every edit or delete, who did it and
-- when. Retained even for a soft-deleted message (AC-4) — this row is the
-- record, for operators of the service (NG-2: no user-facing revision browser).
CREATE TABLE IF NOT EXISTS chat_message_revisions (
    id uuid PRIMARY KEY,
    message_id uuid NOT NULL REFERENCES chat_messages (id) ON DELETE CASCADE,
    prior_content text NOT NULL,
    action text NOT NULL CHECK (action IN ('edit', 'delete')),
    acted_by uuid NOT NULL,
    acted_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS chat_message_revisions_message_idx
    ON chat_message_revisions (message_id, id);

-- Soft edit / soft delete markers on the message itself. NULL = never edited /
-- not deleted — so existing rows are byte-identical (no backfill). A deleted
-- message keeps its row (and its revisions); its content is redacted in every
-- payload while the marker survives.
ALTER TABLE chat_messages ADD COLUMN IF NOT EXISTS edited_at timestamptz;
ALTER TABLE chat_messages ADD COLUMN IF NOT EXISTS deleted_at timestamptz;
