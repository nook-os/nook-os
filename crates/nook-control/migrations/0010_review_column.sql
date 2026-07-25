-- An "In Review" column type, so a submitted PR has a home between In Progress
-- and Done and "Done" can mean merged (MAIN-71).
--
-- Append-only and idempotent: re-running converges rather than failing. A board
-- that already has a review column (a fresh seed, or a re-applied migration) is
-- left untouched by the guarded backfill below.

-- 1. Widen the type CHECK to admit 'review'. A CHECK cannot be altered in place,
--    so drop-then-add; IF EXISTS makes the drop safe on a partially-applied DB.
ALTER TABLE public.board_columns
    DROP CONSTRAINT IF EXISTS board_columns_type_check;
ALTER TABLE public.board_columns
    ADD CONSTRAINT board_columns_type_check
    CHECK (type = ANY (ARRAY[
        'backlog'::text,
        'unstarted'::text,
        'started'::text,
        'review'::text,
        'completed'::text,
        'canceled'::text
    ]));

-- 2. Backfill an "In Review" column immediately before the first `completed`
--    column, on every board that has a completed column but no review one.
--    First free the slot: shift the first completed column and everything at or
--    after it up by one position (only on boards that will get a review column,
--    so a re-run — where the review column already exists — shifts nothing).
UPDATE public.board_columns c
SET position = position + 1
WHERE EXISTS (
        SELECT 1 FROM board_columns d
        WHERE d.board_id = c.board_id AND d.type = 'completed'
      )
  AND NOT EXISTS (
        SELECT 1 FROM board_columns d
        WHERE d.board_id = c.board_id AND d.type = 'review'
      )
  AND c.position >= (
        SELECT min(position) FROM board_columns d
        WHERE d.board_id = c.board_id AND d.type = 'completed'
      );

-- Then insert the review column into the freed slot (one below the — now
-- shifted — first completed column). The NOT EXISTS guard keeps this a no-op on
-- a board that already has one.
INSERT INTO public.board_columns (id, board_id, name, position, type)
SELECT gen_random_uuid(), b.id, 'In Review',
       (SELECT min(position) FROM board_columns c
        WHERE c.board_id = b.id AND c.type = 'completed') - 1,
       'review'
FROM boards b
WHERE EXISTS (
        SELECT 1 FROM board_columns c
        WHERE c.board_id = b.id AND c.type = 'completed'
      )
  AND NOT EXISTS (
        SELECT 1 FROM board_columns c
        WHERE c.board_id = b.id AND c.type = 'review'
      );
