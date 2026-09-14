-- Give every board missing one an "In Review" column (MAIN-650).
--
-- 0010 backfilled the boards that existed when it ran, but the SEED that makes
-- a fresh install's first board kept its own copy of the column list and that
-- copy had no review column. So the fix reached every OLD deployment and no NEW
-- one: on a cluster installed after 0010, the very first board could not
-- receive a build run's conclusion. `record_build_outcome` records `pr_opened`
-- and then fails looking for a column of that type, leaving the card in In
-- Progress with no PR link and the only trace an ERROR in the log.
--
-- The seed now uses `boards::DEFAULT_COLUMNS` directly, which is what stops it
-- happening again. This converges the deployments already made that way. It is
-- 0010's backfill verbatim — idempotent by the same NOT EXISTS guards, so a
-- board that already has a review column is left exactly as its owner arranged
-- it.

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
