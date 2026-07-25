-- Issue types (MAIN-59 AC-1): a flat classification on tasks, defaulting to
-- 'task' so every existing row is valid with no backfill. Append-only and
-- idempotent — the whole statement is a no-op if the column already exists.
--
-- (The ticket named this 0002, written when 0001 was the only migration; the
-- append-only rule means the next free number, which is 0009.)
ALTER TABLE tasks
    ADD COLUMN IF NOT EXISTS type text NOT NULL DEFAULT 'task'
    CHECK (type IN ('task', 'bug', 'epic', 'story', 'chore'));
