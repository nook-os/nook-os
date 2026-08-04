-- The SQLite twin of 0039, hand-authored in the same commit (CLAUDE.md).
-- `jsonb` becomes TEXT per docs/db-dialect-audit.md; SQLite has no
-- `ADD COLUMN IF NOT EXISTS`, and a virgin database built from 0001 has never
-- had this column, so the bare form is correct here.
ALTER TABLE nodes ADD COLUMN port_exclusions TEXT;
