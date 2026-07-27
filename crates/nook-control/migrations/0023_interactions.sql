-- MAIN-159: durable human interactions.
--
-- An interaction is an explicit, persisted ask for a human: requested by an
-- executor (a node running a loop job) or anchored to a ticket, announced over
-- the notification channels (transport only), and answerable from any surface.
-- The control plane is the source of truth; channels carry prompt/choices/link,
-- never the authority to answer. Subject-generic — a job and/or a ticket — so
-- the loop-jobs chain (MAIN-127) is the first consumer, not the only one.
--
-- Append-only and idempotent (IF NOT EXISTS / guarded adds) so a database that
-- already has this table converges instead of failing.

CREATE TABLE IF NOT EXISTS interactions (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL,
    -- The loop job this pauses on, if any. ON DELETE CASCADE: a deleted job's
    -- pending asks are meaningless.
    job_id uuid REFERENCES loop_jobs(id) ON DELETE CASCADE,
    -- The ticket the interaction is anchored to — the subject whose visibility
    -- governs who may answer. Derived from the job's target when a job is named.
    task_id uuid REFERENCES tasks(id) ON DELETE CASCADE,
    prompt text NOT NULL,
    -- Optional structured choices the answer is expected to be one of.
    choices text[],
    -- Lifecycle: pending on create; answered/canceled are terminal.
    state text NOT NULL DEFAULT 'pending',
    -- The executor node that requested it (the anti-spoof anchor, AC-4), and the
    -- session it ran in — both nullable for a human/tool-raised ask.
    requested_by_node_id uuid,
    requested_by_session_id uuid,
    -- The user whose answer won. Set once, by the first authorized answer.
    answered_by uuid,
    response text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    answered_at timestamptz,
    CONSTRAINT interactions_state_check
        CHECK (state IN ('pending', 'answered', 'canceled'))
);

-- Tenant-scoped listing, and the "what is still pending" view the indicator and
-- the list-pending endpoint care about.
CREATE INDEX IF NOT EXISTS interactions_tenant_idx ON interactions (tenant_id);
CREATE INDEX IF NOT EXISTS interactions_pending_idx
    ON interactions (tenant_id, state) WHERE state = 'pending';
-- Anchor lookups: the pending badge on a ticket, and a job's own asks.
CREATE INDEX IF NOT EXISTS interactions_task_idx ON interactions (task_id);
CREATE INDEX IF NOT EXISTS interactions_job_idx ON interactions (job_id);
