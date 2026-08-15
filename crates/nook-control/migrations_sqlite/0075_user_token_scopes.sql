-- SQLite twin of 0074 (hand-authored, per CLAUDE.md). uuid → TEXT per the
-- dialect map; `IF NOT EXISTS` is not valid on ADD COLUMN on this engine, and a
-- nullable ADD COLUMN needs no table rebuild.
--
-- No REFERENCES: SQLite cannot add a column carrying a foreign key to an
-- existing table, so the Postgres CASCADE has no twin here. The failure mode is
-- benign and closed — a token naming a deleted workspace narrows to a workspace
-- no query can match, so it grants nothing — but it is a real DIVERGENCE, not a
-- translation: on Postgres the deletion takes the token row with it, so the
-- credential vanishes from its owner's listing; here it stays and is inert.
ALTER TABLE user_tokens ADD COLUMN scopes TEXT;
ALTER TABLE user_tokens ADD COLUMN workspace_id TEXT;
