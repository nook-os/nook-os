-- Priority and blocker relations: the two pieces the pick query is missing.
--
-- Priority follows Linear's convention so values port cleanly and a human
-- reading the number is not surprised: 0 none, 1 urgent, 2 high, 3 medium,
-- 4 low. Note that 0 sorts LAST despite being lowest — "no priority set" is
-- not the same as "least important", so ordering uses
-- `CASE WHEN priority = 0 THEN 5 ELSE priority END`.
ALTER TABLE tasks
    ADD COLUMN IF NOT EXISTS priority INT NOT NULL DEFAULT 0;

DO $$
BEGIN
    ALTER TABLE tasks
        ADD CONSTRAINT tasks_priority_check CHECK (priority BETWEEN 0 AND 4);
EXCEPTION
    WHEN duplicate_object THEN NULL;
END $$;

CREATE TABLE IF NOT EXISTS task_relations (
    id          UUID PRIMARY KEY,
    tenant_id   UUID NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    from_task   UUID NOT NULL REFERENCES tasks (id) ON DELETE CASCADE,
    to_task     UUID NOT NULL REFERENCES tasks (id) ON DELETE CASCADE,
    kind        TEXT NOT NULL CHECK (kind IN ('blocks', 'relates', 'duplicates')),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (from_task, to_task, kind),
    CHECK (from_task <> to_task)
);

-- "What blocks THIS task" is the pick query's direction, so index the target.
CREATE INDEX IF NOT EXISTS idx_task_relations_to ON task_relations (to_task, kind);
CREATE INDEX IF NOT EXISTS idx_task_relations_from ON task_relations (from_task, kind);

-- The pick query orders by priority then age across a whole board.
CREATE INDEX IF NOT EXISTS idx_tasks_pick ON tasks (board_id, priority, created_at);
