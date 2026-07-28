-- Per-user, per-channel read cursors (MAIN-117). One row records how far a user
-- has read in a channel as a timestamp; a message newer than the cursor and not
-- authored by the reader is "unread". A timestamp cursor is enough because chat
-- history is already time-ordered (keyset on the UUID v7 id) — no per-message
-- receipts (NG-1). DMs are channels too, so this table covers them as well.
--
-- Idempotent (CREATE ... IF NOT EXISTS) so a database that already has the table
-- by other means converges instead of failing.
CREATE TABLE IF NOT EXISTS chat_read_cursors (
    channel_id uuid NOT NULL REFERENCES chat_channels (id) ON DELETE CASCADE,
    user_id uuid NOT NULL,
    last_read_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (channel_id, user_id)
);

-- The unread count per channel is a correlated aggregate over chat_messages
-- filtered by created_at > cursor; this index serves both that scan and the
-- per-(channel,user) cursor lookup.
CREATE INDEX IF NOT EXISTS chat_read_cursors_user_idx
    ON chat_read_cursors (user_id, channel_id);
