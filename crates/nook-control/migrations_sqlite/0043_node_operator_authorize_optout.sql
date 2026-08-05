-- The SQLite twin of 0043, hand-authored in the same commit (CLAUDE.md).
--
-- The owner's veto on operator-authorize (MAIN-276 AC-6). SQLite has no
-- `ADD COLUMN IF NOT EXISTS`, so the add is unconditional — safe because the
-- column is new in this migration and the ledger runs each version once.
--
-- `boolean` maps to the same declared type here; SQLite stores 0/1 and the
-- dialect layer already binds Rust `bool` to it, so no type-map exception is
-- needed (docs/db-dialect-audit.md).
ALTER TABLE nodes ADD COLUMN operator_authorize_optout BOOLEAN NOT NULL DEFAULT false;
