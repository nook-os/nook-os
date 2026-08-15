-- MAIN-330: the chain from a support email to the work it caused.
--
-- One row per accepted delivery, written by the inbound pipeline and extended
-- as the work moves: the ticket and the sealed object exist at accept, the
-- investigate run a moment later, the PR whenever one is opened. Nothing here
-- carries message CONTENT — the body is on the card and the whole of it is in
-- the sealed object `storage_key` names, which is where HC-4 wants it.
--
-- `task_id` is the card's column name everywhere in this schema; the epic calls
-- the same thing a ticket.
--
-- **`message_id` is nullable, and there is no unique index on it.** Two
-- deliberate readings, both of the pipeline as it actually behaves:
--
--   * A delivery may carry no `Message-Id` at all. It still produced a ticket
--     and a sealed object, which is the chain this table exists to record, so
--     it still gets a row — it simply cannot be found by message id and cannot
--     be threaded. Synthesising an id instead would put a fabricated identifier
--     into an outbound `In-Reply-To`, which is worse than having none.
--   * The pipeline does not dedupe on `message_id` (MAIN-329 says so in its own
--     module docs: a replayed delivery files a second card). A unique index here
--     would therefore make the link write fail for a delivery the pipeline
--     accepted, leaving a card with no chain at all. Dedupe is C1's unfinished
--     business, not something to enforce from underneath it.
CREATE TABLE IF NOT EXISTS email_links (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    -- Nullable because the routing config's workspace is optional: a tenant may
    -- receive support mail without scoping it to a repository.
    workspace_id uuid REFERENCES workspaces(id) ON DELETE SET NULL,
    task_id uuid NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    -- Set once the investigate run is seeded, which is after the card exists —
    -- and left null for a tenant with no owner to request the run as.
    loop_job_id uuid REFERENCES loop_jobs(id) ON DELETE SET NULL,
    -- The PR URL, recorded when one is opened against the card. Text rather than
    -- a reference: a PR lives on the forge, not in this database.
    pr_ref text,
    message_id text,
    in_reply_to text,
    storage_key text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

-- The reply-threading lookup (C4): given an inbound `In-Reply-To`, which chain
-- is this. Tenant-first because every query is tenant-scoped (AC-4).
CREATE INDEX IF NOT EXISTS email_links_message_idx
    ON email_links (tenant_id, message_id);

-- The card's own chain: what an open ticket shows, and where the PR update
-- lands.
CREATE INDEX IF NOT EXISTS email_links_task_idx
    ON email_links (tenant_id, task_id);

-- The inbox listing (C7), newest first.
CREATE INDEX IF NOT EXISTS email_links_recent_idx
    ON email_links (tenant_id, workspace_id, created_at DESC);
