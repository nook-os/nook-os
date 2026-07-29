-- MAIN-225: address a task's working directory by CHECKOUT ID, matching the
-- session model MAIN-222 landed (sessions.checkout_id). Tasks still point at
-- their worktree by the mutable (worktree_path, worktree_node_id) string pair,
-- which goes stale the moment reconcile moves or re-ids the checkout on disk.
--
-- This is purely additive: the legacy string columns stay and are written
-- alongside the id (item 7 retires them). Append-only, idempotent
-- (ADD COLUMN IF NOT EXISTS / fill-only-NULL), so a database that already got
-- the change converges and the dev ledger-ahead tolerance (MAIN-224) can
-- re-apply it after merge safely.
ALTER TABLE tasks
    ADD COLUMN IF NOT EXISTS checkout_id uuid
        REFERENCES node_workspaces(id) ON DELETE SET NULL;

-- Partial index over the tasks that actually have a checkout — the id-pinned
-- lookups (prune, future readers) never scan the NULL majority.
CREATE INDEX IF NOT EXISTS tasks_checkout_id_idx
    ON tasks (checkout_id) WHERE checkout_id IS NOT NULL;

-- Backfill: for each task with a live worktree, point checkout_id at the
-- node_workspaces row its (node, path) resolves to among PRESENT rows. A task
-- with no worktree, or whose path no longer resolves to a present checkout,
-- keeps checkout_id = NULL. Fills only currently-NULL rows, so it converges on
-- re-run.
UPDATE tasks t
SET checkout_id = nw.id
FROM node_workspaces nw
WHERE t.checkout_id IS NULL
  AND t.worktree_path IS NOT NULL
  AND t.worktree_node_id IS NOT NULL
  AND nw.node_id = t.worktree_node_id
  AND nw.path = t.worktree_path
  AND nw.missing_at IS NULL;
