-- SQLite twin of 0052 (hand-authored, per CLAUDE.md). Nullable ADD COLUMN
-- needs no rebuild; `IF NOT EXISTS` is not valid on ADD COLUMN here.
ALTER TABLE workspaces ADD COLUMN build_max_replicas integer;
