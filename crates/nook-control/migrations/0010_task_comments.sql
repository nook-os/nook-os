-- Comments: where the reasoning lives.
--
-- A real table rather than an `events` row with a kind. `events` is an
-- append-only activity log: no editability, a payload shape that varies per
-- kind, and no author identity worth rendering. Comments are durable
-- first-class content that gets read back and parsed — the builder's blocking
-- question, the reviewer's verdict, the human's answer.
CREATE TABLE IF NOT EXISTS task_comments (
    id          UUID PRIMARY KEY,
    tenant_id   UUID NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    task_id     UUID NOT NULL REFERENCES tasks (id) ON DELETE CASCADE,
    author_type TEXT NOT NULL CHECK (author_type IN ('user', 'agent', 'system')),
    author_id   UUID,
    -- Denormalised on purpose: an agent has no users row, and a deleted user
    -- should still render with attribution rather than as a dangling uuid.
    author_name TEXT NOT NULL DEFAULT '',
    body_md     TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_task_comments_task ON task_comments (task_id, created_at);
