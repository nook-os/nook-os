-- Column TYPE, so agents can target a semantic state rather than a name.
--
-- "Move it to the started column" has to survive a human renaming "In Progress"
-- to "Doing". A name is a label for people; the type is the contract for
-- automation, and keeping them separate is what lets each change without
-- breaking the other.
ALTER TABLE board_columns
    ADD COLUMN IF NOT EXISTS type TEXT NOT NULL DEFAULT 'unstarted';

-- Added separately and guarded: ADD COLUMN IF NOT EXISTS is idempotent but
-- ADD CONSTRAINT is not, and a database that already has this must converge
-- rather than fail.
DO $$
BEGIN
    ALTER TABLE board_columns
        ADD CONSTRAINT board_columns_type_check
        CHECK (type IN ('backlog', 'unstarted', 'started', 'completed', 'canceled'));
EXCEPTION
    WHEN duplicate_object THEN NULL;
END $$;

-- Backfill the seeded board by name. A board whose columns were renamed keeps
-- the 'unstarted' default: wrong-but-harmless beats guessing, and an operator
-- can set it explicitly. Deliberately NOT a fuzzy match — a column called
-- "Done Deal" is not the completed column just because it starts with "Done".
UPDATE board_columns SET type = 'backlog'   WHERE lower(name) = 'triage'      AND type = 'unstarted';
UPDATE board_columns SET type = 'unstarted' WHERE lower(name) = 'todo'        AND type = 'unstarted';
UPDATE board_columns SET type = 'started'   WHERE lower(name) = 'in progress' AND type = 'unstarted';
UPDATE board_columns SET type = 'completed' WHERE lower(name) = 'done'        AND type = 'unstarted';

-- Resolving a type to a column takes the lowest position of that type.
CREATE INDEX IF NOT EXISTS idx_board_columns_type ON board_columns (board_id, type, "position");
