-- MAIN-603: generic Markdown metadata a producer hangs on a card.
--
-- The columns are deliberately dumb. `body_md` is TEXT and nothing here — no
-- check, no generated column, no index on its contents — depends on what is in
-- it, because Nook never parses it (NG-1). A future metadata type is a new
-- `key`, never a new column and never an enum of kinds.
--
-- `key` is the producer's own name for the report and is what makes a re-run an
-- UPDATE rather than a second row: the unique index below is the whole of
-- AC-1's "replaces, never appends". It is not the primary key because an id a
-- caller can hold is what every other record here has.
CREATE TABLE IF NOT EXISTS task_reports (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    task_id uuid NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    key text NOT NULL,
    title text NOT NULL,
    body_md text NOT NULL,
    -- Who wrote it, in `task_comments`' spelling, so "an agent said this" means
    -- one thing across the card (AC-10). No foreign key on `author_id`, again
    -- as comments have it: a report outlives the account that produced it, and
    -- a deleted user must not take the record of the build with them.
    author_type text NOT NULL CHECK (author_type IN ('user', 'agent', 'system')),
    author_id uuid,
    author_name text NOT NULL DEFAULT '',
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- AC-1 and AC-3's uniqueness in one object: `(task, key)` is the natural key the
-- upsert conflicts on, so two runs of one automation cannot become two reports.
CREATE UNIQUE INDEX IF NOT EXISTS task_reports_key_idx
    ON task_reports (task_id, key);

-- The only read there is: one card's reports, most recently updated first (AC-5).
CREATE INDEX IF NOT EXISTS task_reports_recent_idx
    ON task_reports (task_id, updated_at DESC);
