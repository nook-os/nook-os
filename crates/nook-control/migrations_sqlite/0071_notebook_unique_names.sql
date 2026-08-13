-- SQLite twin of 0071 (hand-authored, per CLAUDE.md). Four differences, all
-- forced by the engine; the reasoning — including why the suffix counts on from
-- the highest one already in use rather than from 2 — is in the Postgres half.
--
-- 1. NULL-equating. Postgres says `NULLS NOT DISTINCT`; SQLite has no such
--    modifier and follows the SQL default where every NULL is distinct, so the
--    root — `parent_id IS NULL` for folders, `folder_id IS NULL` for notes —
--    would be the one place with no constraint at all. `COALESCE(col, '')` is
--    the NULL-free key, in an expression index, exactly as
--    `0038_settings_null_equating_unique.sql` does; the empty string is safe as
--    the stand-in because both columns hold an id and no id is ever ''. The
--    same COALESCE stands in for `IS NOT DISTINCT FROM` below.
-- 2. `AS MATERIALIZED`, and it is load-bearing rather than a hint. SQLite
--    writes each row as it scans, and an un-materialized CTE is re-evaluated
--    per row — so the second `Ideas` was renamed, and the ranking of the third
--    then read a table that no longer had two `Ideas` in it and produced
--    `Ideas (2)` a second time. Measured: the index creation below failed with
--    `UNIQUE constraint failed`. Postgres computes the whole statement against
--    one snapshot and needs no such instruction.
-- 3. `~ '^[0-9]+$'` has no SQLite equivalent; `NOT GLOB '*[^0-9]*'` is the
--    all-digits test on this engine (the length bound already excludes empty).
-- 4. `::text` casts are stripped per the dialect map — SQLite concatenates a
--    number to a string without one.

WITH ranked AS MATERIALIZED (
    SELECT f.id,
           row_number() OVER (PARTITION BY f.person_id, COALESCE(f.parent_id, ''), f.name
                              ORDER BY f.created_at, f.id) AS n,
           (SELECT COALESCE(MAX(CAST(substr(o.name, length(f.name) + 3,
                                            length(o.name) - length(f.name) - 3) AS integer)), 1)
              FROM user_note_folders o
             WHERE o.person_id = f.person_id
               AND COALESCE(o.parent_id, '') = COALESCE(f.parent_id, '')
               AND length(o.name) BETWEEN length(f.name) + 4 AND length(f.name) + 12
               AND substr(o.name, 1, length(f.name) + 2) = f.name || ' ('
               AND substr(o.name, length(o.name), 1) = ')'
               AND substr(o.name, length(f.name) + 3,
                          length(o.name) - length(f.name) - 3) NOT GLOB '*[^0-9]*'
           ) AS highest
      FROM user_note_folders f
)
UPDATE user_note_folders
   SET name = name || ' (' || (ranked.highest + ranked.n - 1) || ')'
  FROM ranked
 WHERE ranked.id = user_note_folders.id AND ranked.n > 1;

WITH ranked AS MATERIALIZED (
    SELECT u.id,
           row_number() OVER (PARTITION BY u.person_id, COALESCE(u.folder_id, ''), u.title
                              ORDER BY u.created_at, u.id) AS n,
           (SELECT COALESCE(MAX(CAST(substr(o.title, length(u.title) + 3,
                                            length(o.title) - length(u.title) - 3) AS integer)), 1)
              FROM user_notes o
             WHERE o.person_id = u.person_id
               AND COALESCE(o.folder_id, '') = COALESCE(u.folder_id, '')
               AND length(o.title) BETWEEN length(u.title) + 4 AND length(u.title) + 12
               AND substr(o.title, 1, length(u.title) + 2) = u.title || ' ('
               AND substr(o.title, length(o.title), 1) = ')'
               AND substr(o.title, length(u.title) + 3,
                          length(o.title) - length(u.title) - 3) NOT GLOB '*[^0-9]*'
           ) AS highest
      FROM user_notes u
)
UPDATE user_notes
   SET title = title || ' (' || (ranked.highest + ranked.n - 1) || ')'
  FROM ranked
 WHERE ranked.id = user_notes.id AND ranked.n > 1;

CREATE UNIQUE INDEX IF NOT EXISTS user_note_folders_person_parent_name_uniq
    ON user_note_folders (person_id, COALESCE(parent_id, ''), name);

CREATE UNIQUE INDEX IF NOT EXISTS user_notes_person_folder_title_uniq
    ON user_notes (person_id, COALESCE(folder_id, ''), title);
