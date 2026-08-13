-- SQLite twin of 0068 (hand-authored, per CLAUDE.md). boolean → INTEGER and
-- uuid → TEXT per the dialect map; `IF NOT EXISTS` is not valid on ADD COLUMN
-- on this engine, and a nullable (or constant-defaulted) ADD COLUMN needs no
-- table rebuild.
ALTER TABLE workspaces ADD COLUMN build_loop_enabled INTEGER NOT NULL DEFAULT 0;
ALTER TABLE workspaces ADD COLUMN build_loop_node_id TEXT;
ALTER TABLE workspaces ADD COLUMN build_loop_enabled_by TEXT;

CREATE INDEX IF NOT EXISTS workspaces_build_loop_enabled_idx
    ON workspaces (tenant_id) WHERE build_loop_enabled;
