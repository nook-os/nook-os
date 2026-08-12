-- MAIN-535: the files a message carries.
--
-- The BYTES live in the control plane's user-content store (MAIN-532); what is
-- here is the join plus the three facts rendering a message needs — filename,
-- type, size (AC-1). They are COPIED rather than read back through
-- `public.user_content` on every render: a message list is fifty rows and a
-- cross-schema join per render is what this column set exists to avoid, and the
-- three are immutable for the life of an upload anyway.
--
-- `content_id` carries no foreign key on purpose. `user_content` is the control
-- plane's table in another schema and another service's migration ledger; a
-- constraint across that line would make either service's deploy order matter.
-- Chat deletes its rows and asks the control plane to forget the bytes, and a
-- content id whose row is gone renders as a chip that 404s — which is what a
-- deleted upload should look like.
--
-- Postgres only: nook-chat has no SQLite track (AC-3), like every chat
-- migration since 0008.
CREATE TABLE IF NOT EXISTS chat_message_attachments (
    id               uuid PRIMARY KEY,
    message_id       uuid NOT NULL REFERENCES chat_messages (id) ON DELETE CASCADE,
    content_id       uuid NOT NULL,
    filename         text NOT NULL,
    content_type     text NOT NULL,
    size_bytes       bigint NOT NULL,
    -- The order the sender picked them in; ties broken by id, which is v7.
    position         integer NOT NULL DEFAULT 0,
    created_at       timestamptz NOT NULL DEFAULT now()
);

-- Every read is "this message's attachments, in order", including the batched
-- one that loads a whole page of history at once.
CREATE INDEX IF NOT EXISTS chat_message_attachments_message_idx
    ON chat_message_attachments (message_id, position, id);

-- One upload belongs to one message: re-posting the same id would let a second
-- message keep bytes the first message's delete removed (AC-6).
CREATE UNIQUE INDEX IF NOT EXISTS chat_message_attachments_content_idx
    ON chat_message_attachments (content_id);
