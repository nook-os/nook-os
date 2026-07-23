-- Human-readable keys: ENG-42.
--
-- `Closes ENG-42` in a PR body is the only join between a pull request and the
-- issue it closes, and it has to be typed by people into commits, branches and
-- chat. A uuid cannot do that job.
ALTER TABLE boards
    ADD COLUMN IF NOT EXISTS key TEXT,
    ADD COLUMN IF NOT EXISTS next_number INT NOT NULL DEFAULT 1;

ALTER TABLE tasks
    ADD COLUMN IF NOT EXISTS number INT;

-- Derive a key for every board that lacks one: uppercase alphanumerics from the
-- name, truncated to 5, with a numeric suffix when two boards collide. Done in
-- PL/pgSQL rather than one clever UPDATE because dedup needs to see what it has
-- already assigned.
DO $$
DECLARE
    b RECORD;
    base TEXT;
    candidate TEXT;
    n INT;
BEGIN
    FOR b IN SELECT id, tenant_id, name FROM boards WHERE key IS NULL ORDER BY created_at LOOP
        base := upper(regexp_replace(coalesce(b.name, ''), '[^a-zA-Z0-9]', '', 'g'));
        base := substring(base FROM 1 FOR 5);
        IF base = '' THEN
            base := 'BOARD';
        END IF;
        candidate := base;
        n := 1;
        WHILE EXISTS (SELECT 1 FROM boards WHERE tenant_id = b.tenant_id AND key = candidate) LOOP
            n := n + 1;
            candidate := base || n::TEXT;
        END LOOP;
        UPDATE boards SET key = candidate WHERE id = b.id;
    END LOOP;
END $$;

-- Number existing tasks per board by age, then park next_number past the max so
-- numbers are never reused.
DO $$
DECLARE
    b RECORD;
BEGIN
    FOR b IN SELECT id FROM boards LOOP
        WITH ordered AS (
            SELECT id, row_number() OVER (ORDER BY created_at, id) AS rn
            FROM tasks WHERE board_id = b.id AND number IS NULL
        )
        UPDATE tasks t SET number = ordered.rn FROM ordered WHERE t.id = ordered.id;

        UPDATE boards
        SET next_number = coalesce((SELECT max(number) FROM tasks WHERE board_id = b.id), 0) + 1
        WHERE id = b.id;
    END LOOP;
END $$;

CREATE UNIQUE INDEX IF NOT EXISTS idx_boards_tenant_key
    ON boards (tenant_id, key) WHERE key IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_tasks_board_number
    ON tasks (board_id, number) WHERE number IS NOT NULL;
