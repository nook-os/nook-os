-- MAIN-502: a session is a terminal OR a chat, chosen when it is created.
--
-- `interface` defaults to 'terminal' and every existing row takes that value,
-- which is what it already was — the column can only add a case, never
-- reinterpret one.
--
-- `session_messages` is a chat session's conversation, persisted here rather
-- than held on the node: it has to survive a reload, a reconnect, a node
-- restart and being opened on a second device (AC-5), and the node survives
-- none of those. Append-only and ordered by the v7 id, which is time-ordered,
-- so no sequence column — the same shape `loop_job_transcript` uses.
--
-- A permission request is a ROW here, not a side channel: it is a message in
-- the conversation with two buttons on it (AC-6). `decision` is NULL while the
-- agent is blocked, which is exactly the state the buttons render for, and
-- writing it is what makes a second device stop offering an answer that has
-- already been given.
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS interface text NOT NULL DEFAULT 'terminal';

CREATE TABLE IF NOT EXISTS session_messages (
    id uuid PRIMARY KEY,
    session_id uuid NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    -- 'human' | 'agent' | 'system' | 'permission'. Free-form text for the
    -- reason `sessions.interface` is: a new role is a code change.
    role text NOT NULL DEFAULT 'agent',
    body text NOT NULL,
    -- The runtime's own id for a permission request; NULL on every other row.
    permission_request_id text,
    tool_name text,
    -- 'allow' | 'deny'. NULL means the agent is still blocked on it.
    decision text,
    at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS session_messages_session_idx
    ON session_messages (session_id, id);

-- The lookup an incoming answer makes: one outstanding request, by its
-- runtime-issued id. Partial, because answered rows are never searched this
-- way and there are far more of them.
CREATE INDEX IF NOT EXISTS session_messages_pending_permission_idx
    ON session_messages (session_id, permission_request_id)
    WHERE decision IS NULL AND permission_request_id IS NOT NULL;
