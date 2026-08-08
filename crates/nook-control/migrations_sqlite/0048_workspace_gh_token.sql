-- SQLite twin of 0048 (hand-authored, per CLAUDE.md). bytea -> BLOB per the
-- dialect audit; nullable ADD COLUMN needs no rebuild and no IF NOT EXISTS.
ALTER TABLE workspaces ADD COLUMN gh_token_enc BLOB;
