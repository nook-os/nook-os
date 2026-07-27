-- MAIN-114: threaded replies. A message may reply to another message in the
-- SAME channel; replies are one level deep — a reply's parent must itself be
-- top-level. Same-channel and no-nesting are enforced in the POST handler with
-- clear 400s (both rules compare two rows, which a column CHECK cannot express).
-- Existing messages simply have no parent (no backfill — NULL is top-level).

ALTER TABLE chat_messages
    ADD COLUMN IF NOT EXISTS parent_message_id uuid
        REFERENCES chat_messages (id) ON DELETE CASCADE;

-- Serves both a thread's replies (WHERE parent_message_id = $1 ORDER BY id) and
-- the per-parent reply_count / last_reply_at subqueries in channel history,
-- without scanning the table. Partial: top-level messages have no parent, so
-- they stay out of this index.
CREATE INDEX IF NOT EXISTS chat_messages_parent_idx
    ON chat_messages (parent_message_id, id)
    WHERE parent_message_id IS NOT NULL;
