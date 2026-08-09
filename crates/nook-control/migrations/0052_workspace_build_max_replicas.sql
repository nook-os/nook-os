-- MAIN-461: how many BUILD runs a repo may hold at once is the workspace's
-- declaration, `review_loop_max_replicas`'s twin. NULL means nobody decided
-- (effective 1), 0 means builds are OFF for this repo — the workspace-level
-- kill-switch — and the three states must stay distinguishable, which is why
-- there is no DEFAULT.
ALTER TABLE workspaces ADD COLUMN IF NOT EXISTS build_max_replicas integer;
