-- Give every board missing one an "In Review" column (MAIN-650).
--
-- The Postgres twin's reasoning applies verbatim: 0010 backfilled the boards
-- that existed when it ran, but the seed that makes a fresh install's first
-- board kept its own copy of the column list, and that copy had no review
-- column. A build run on such a board records `pr_opened` and then fails
-- looking for a column of that type, leaving the card in In Progress with no PR
-- link.
--
-- Two dialect differences and nothing else: no schema qualifier, and no
-- `gen_random_uuid()` — the id is assembled in the canonical v4 layout, the
-- idiom `0001_init.sql` established for `users.person_id`. `position` is a
-- keyword here, so it is quoted.

UPDATE board_columns
SET "position" = "position" + 1
WHERE EXISTS (
        SELECT 1 FROM board_columns d
        WHERE d.board_id = board_columns.board_id AND d.type = 'completed'
      )
  AND NOT EXISTS (
        SELECT 1 FROM board_columns d
        WHERE d.board_id = board_columns.board_id AND d.type = 'review'
      )
  AND "position" >= (
        SELECT min(d."position") FROM board_columns d
        WHERE d.board_id = board_columns.board_id AND d.type = 'completed'
      );

INSERT INTO board_columns (id, board_id, name, "position", type)
SELECT lower(
         hex(randomblob(4)) || '-' ||
         hex(randomblob(2)) || '-4' ||
         substr(hex(randomblob(2)), 2) || '-' ||
         substr('89ab', 1 + (abs(random()) % 4), 1) ||
         substr(hex(randomblob(2)), 2) || '-' ||
         hex(randomblob(6))
       ),
       b.id, 'In Review',
       (SELECT min(c."position") FROM board_columns c
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
