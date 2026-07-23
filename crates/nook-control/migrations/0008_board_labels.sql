-- Labels: the human approval gate.
--
-- `agent-ready` is what a person applies to say "an agent may take this", and
-- nothing else in the design carries that meaning. Without labels the whole
-- human-in-the-loop story has nothing to hang on.
--
-- Tenant-scoped, not board-scoped: a tenant runs one convention, so
-- `agent-ready` means the same thing everywhere and cross-board queries stay a
-- single join. Boards do not own label vocabularies.
CREATE TABLE IF NOT EXISTS labels (
    id          UUID PRIMARY KEY,
    tenant_id   UUID NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    color       TEXT NOT NULL DEFAULT '#f0a000',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name)
);

CREATE TABLE IF NOT EXISTS task_labels (
    task_id   UUID NOT NULL REFERENCES tasks (id) ON DELETE CASCADE,
    label_id  UUID NOT NULL REFERENCES labels (id) ON DELETE CASCADE,
    PRIMARY KEY (task_id, label_id)
);

-- The pick query filters by label and then joins back to tasks, so the reverse
-- direction is the one that needs help; the primary key already covers
-- task → labels.
CREATE INDEX IF NOT EXISTS idx_task_labels_label ON task_labels (label_id);
