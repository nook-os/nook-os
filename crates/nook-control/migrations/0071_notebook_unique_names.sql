-- MAIN-574: a folder name is unique per (person, parent), a note title per
-- (person, folder) — so a path like `Nook/Ideas/2026-08-13` addresses one row.
--
-- Both keys are NULLABLE (`0011_user_notebook.sql`): a NULL parent is a root
-- folder, a NULL folder is a note at the notebook root. The SQL default makes
-- every NULL distinct, which would leave the root — the one place every
-- notebook has — entirely unconstrained, so the index equates them with
-- `NULLS NOT DISTINCT`, the form `0001_init.sql` already uses on `settings`.
-- SQLite has no such modifier and its twin uses a COALESCE expression index
-- instead; MAIN-388 is the card where that difference was learned.
--
-- The de-dup MUST precede the indexes: on a notebook that has already collected
-- a collision, creating them first fails the deploy. Live data held none when
-- this was written (measured 2026-08-13) — this is for the notebooks nobody
-- has looked at.

-- Rename every row that is not the first of its group, ordered by created_at:
-- `Ideas`, `Ideas (2)`, `Ideas (3)`. Nothing is deleted and nothing is merged
-- (NG-4). `PARTITION BY … parent_id …` groups NULL with NULL, so the root is
-- de-duplicated like any other parent.
--
-- ONE pass, and the suffix is what makes one pass enough. The number counts on
-- from the HIGHEST ` (n)` any sibling already carries — not from 1 — so a
-- rename can never land on a name that is already in use:
--
--   * `highest` is the largest n over every sibling named `<name> (n)`, or 1
--     when there is none, in which case the ranks read 2, 3, … exactly as this
--     card asks.
--   * Every name this writes is `<name> (k)` with k > highest, so it differs
--     from every existing `<name> (n)`, and the ranks differ from each other.
--   * A row named `<name> (n)` belongs to a different group, whose renames are
--     `<name> (n) (k)` — a different shape, so the two cannot meet.
--
-- Counting from 2 and repeating the pass does NOT converge, which is why this
-- is written the harder way: `Ideas, Ideas, Ideas (2), Ideas (2)` sends the
-- second `Ideas` onto `Ideas (2)` while the second `Ideas (2)` becomes
-- `Ideas (2) (2)`, and the next pass then manufactures the collision after
-- that. No fixed number of passes is a proof; a suffix known to be free is.
--
-- A ` (n)` with more than nine digits is not treated as a suffix at all — it
-- would overflow the cast, and a name that long is not one this ever wrote.

WITH ranked AS (
    SELECT f.id,
           row_number() OVER (PARTITION BY f.person_id, f.parent_id, f.name
                              ORDER BY f.created_at, f.id) AS n,
           (SELECT COALESCE(MAX(CAST(substr(o.name, length(f.name) + 3,
                                            length(o.name) - length(f.name) - 3) AS integer)), 1)
              FROM public.user_note_folders o
             WHERE o.person_id = f.person_id
               AND o.parent_id IS NOT DISTINCT FROM f.parent_id
               AND length(o.name) BETWEEN length(f.name) + 4 AND length(f.name) + 12
               AND substr(o.name, 1, length(f.name) + 2) = f.name || ' ('
               AND substr(o.name, length(o.name), 1) = ')'
               AND substr(o.name, length(f.name) + 3,
                          length(o.name) - length(f.name) - 3) ~ '^[0-9]+$'
           ) AS highest
      FROM public.user_note_folders f
)
UPDATE public.user_note_folders f
   SET name = f.name || ' (' || (r.highest + r.n - 1)::text || ')'
  FROM ranked r
 WHERE r.id = f.id AND r.n > 1;

WITH ranked AS (
    SELECT u.id,
           row_number() OVER (PARTITION BY u.person_id, u.folder_id, u.title
                              ORDER BY u.created_at, u.id) AS n,
           (SELECT COALESCE(MAX(CAST(substr(o.title, length(u.title) + 3,
                                            length(o.title) - length(u.title) - 3) AS integer)), 1)
              FROM public.user_notes o
             WHERE o.person_id = u.person_id
               AND o.folder_id IS NOT DISTINCT FROM u.folder_id
               AND length(o.title) BETWEEN length(u.title) + 4 AND length(u.title) + 12
               AND substr(o.title, 1, length(u.title) + 2) = u.title || ' ('
               AND substr(o.title, length(o.title), 1) = ')'
               AND substr(o.title, length(u.title) + 3,
                          length(o.title) - length(u.title) - 3) ~ '^[0-9]+$'
           ) AS highest
      FROM public.user_notes u
)
UPDATE public.user_notes u
   SET title = u.title || ' (' || (r.highest + r.n - 1)::text || ')'
  FROM ranked r
 WHERE r.id = u.id AND r.n > 1;

CREATE UNIQUE INDEX IF NOT EXISTS user_note_folders_person_parent_name_uniq
    ON public.user_note_folders (person_id, parent_id, name) NULLS NOT DISTINCT;

CREATE UNIQUE INDEX IF NOT EXISTS user_notes_person_folder_title_uniq
    ON public.user_notes (person_id, folder_id, title) NULLS NOT DISTINCT;
