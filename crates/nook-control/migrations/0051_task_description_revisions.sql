-- Every description replace keeps what it overwrote (MAIN-470 AC-3).
--
-- The task PATCH is a whole-body replace with no revision history, so one bad
-- payload silently destroys a contract: on 2026-08-08 a caller passed `-`
-- expecting the stdin convention and a ticket's whole description became that
-- one character — recovery was luck. The service writes a row here on each
-- replace, holding the PRIOR body; retrieval is CLI/API-only (NG-2, no UI).
CREATE TABLE IF NOT EXISTS task_description_revisions (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL,
    task_id uuid NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    body text NOT NULL,
    author_id uuid,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS task_description_revisions_task_idx
    ON task_description_revisions (task_id, created_at);
