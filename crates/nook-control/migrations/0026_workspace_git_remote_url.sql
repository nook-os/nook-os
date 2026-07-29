-- MAIN-223: a workspace should be able to say what repository it is. The raw
-- clone URL lived only on scattered node_workspaces (checkout) rows, so
-- "clone this workspace onto node X" did not exist — every clone re-supplied the
-- URL. Lift the URL onto the workspace itself.
--
-- Idempotent (CREATE-IF-NOT-EXISTS / fill-only-when-NULL): a database that already
-- got this column by other means converges rather than failing, and the dev
-- ledger-ahead tolerance (MAIN-224) can re-apply it after merge safely (NG-4).
ALTER TABLE workspaces ADD COLUMN IF NOT EXISTS git_remote_url text;

-- Backfill from the workspace's own checkouts, but ONLY where they agree: a
-- single distinct non-null URL across its node_workspaces rows. Disagreement or
-- absence leaves it NULL — an ambiguous identity is worse than none. Fills only
-- currently-NULL rows, so a re-run never clobbers a value set since.
WITH agreed AS (
    SELECT workspace_id, min(git_remote_url) AS url
    FROM node_workspaces
    WHERE git_remote_url IS NOT NULL
    GROUP BY workspace_id
    HAVING count(DISTINCT git_remote_url) = 1
)
UPDATE workspaces w
SET git_remote_url = a.url
FROM agreed a
WHERE w.id = a.workspace_id
  AND w.git_remote_url IS NULL;

-- Log the disagreeing count so an operator knows which workspaces were left NULL
-- on purpose (AC-1) rather than silently.
DO $$
DECLARE
    ambiguous int;
BEGIN
    SELECT count(*) INTO ambiguous FROM (
        SELECT workspace_id
        FROM node_workspaces
        WHERE git_remote_url IS NOT NULL
        GROUP BY workspace_id
        HAVING count(DISTINCT git_remote_url) > 1
    ) x;
    RAISE NOTICE 'MAIN-223 backfill: % workspace(s) left git_remote_url NULL due to disagreeing checkouts', ambiguous;
END $$;
