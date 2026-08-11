-- Files hung off a ticket or one of its comments (MAIN-533).
--
-- One join table for both parent kinds rather than two tables: an attachment is
-- the same record either way — this content, on that thing, put there by this
-- person — and splitting it would double every query the UI makes.
--
-- `parent_id` therefore carries no foreign key, because it points at two tables.
-- That is a real cost: nothing at this level removes an attachment when its
-- ticket or comment goes. The delete routes do it instead, and they have to
-- anyway — the stored bytes live outside the database and no cascade could ever
-- reach them (AC-7).
CREATE TABLE IF NOT EXISTS task_attachments (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    -- The content half DOES cascade: deleting the row a person uploaded takes
    -- every place it was attached with it, which is what makes the detach path
    -- a single delete (AC-6).
    user_content_id uuid NOT NULL REFERENCES user_content(id) ON DELETE CASCADE,
    parent_kind text NOT NULL CHECK (parent_kind IN ('task', 'task_comment')),
    parent_id uuid NOT NULL,
    attached_by uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now()
);

-- The read every list does: everything on one parent, oldest first.
CREATE INDEX IF NOT EXISTS task_attachments_parent_idx
    ON task_attachments (parent_kind, parent_id, created_at);

-- The same file attached twice to one parent is a double-click, not two
-- attachments (NG-4: there is no replace, so re-adding is how you "update").
CREATE UNIQUE INDEX IF NOT EXISTS task_attachments_unique_idx
    ON task_attachments (parent_kind, parent_id, user_content_id);
