-- The SQLite twin of 0036, hand-authored in the same commit (CLAUDE.md).
--
-- `uuid` becomes TEXT per docs/db-dialect-audit.md. SQLite cannot add a column
-- with a REFERENCES clause carrying ON DELETE to an existing table, and its
-- foreign keys are off by default anyway — so the restrict behaviour lives in
-- the delete path's own check rather than in the schema here. That check is
-- what AC-8 actually tests, on both engines.
ALTER TABLE workspaces ADD COLUMN git_credential_id TEXT;

CREATE INDEX IF NOT EXISTS workspaces_git_credential_id_idx
    ON workspaces (git_credential_id)
    WHERE git_credential_id IS NOT NULL;
