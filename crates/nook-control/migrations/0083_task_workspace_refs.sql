-- MAIN-632: the workspaces a card's description names with `@slug`.
--
-- A row, not the text, is the reference (AC-1). The description keeps the
-- `@slug` a human typed, but what the run reads is this table — so renaming a
-- workspace's slug leaves every card that named it still pointing at it, where
-- re-parsing the body at read time would silently orphan them all.
--
-- No `id`: the reference IS the pair, a card names a workspace once, and the
-- primary key is what makes re-parsing a description idempotent.
CREATE TABLE IF NOT EXISTS task_workspace_refs (
    task_id uuid NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    tenant_id uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (task_id, workspace_id)
);

-- The reverse read — "which cards name this workspace" — which the workspace
-- delete path needs before the cascade above can be reasoned about at all.
CREATE INDEX IF NOT EXISTS task_workspace_refs_workspace_idx
    ON task_workspace_refs (workspace_id);
