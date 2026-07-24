-- Archiving finished work off the board (MAIN-15).
--
-- `archived_at` NULL means live (on the board, pickable). A timestamp means
-- archived: hidden from the board by default and — critically — excluded from
-- the agent pick query, so no /loop-build ever claims archived work. The row is
-- preserved and reversible (unarchive sets it back to NULL); this is not delete.
--
-- Idempotent (IF NOT EXISTS) so a database that already has the column
-- converges instead of failing.
ALTER TABLE public.tasks
    ADD COLUMN IF NOT EXISTS archived_at timestamptz;

-- The board and pick queries filter on `archived_at IS NULL` on the hot path;
-- a partial index keeps that cheap as finished work accumulates.
CREATE INDEX IF NOT EXISTS tasks_live_idx
    ON public.tasks (board_id) WHERE archived_at IS NULL;
