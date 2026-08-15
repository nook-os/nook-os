-- MAIN-602: a token that can do less than its owner.
--
-- `scopes` NULL is the token every row already is: unscoped, exactly its
-- owner's access. A non-NULL value — space-separated `resource:verb` names, the
-- OAuth form — is a narrowing, and `nook_control::auth::scopes` refuses
-- anything the list does not name. NULL rather than an empty string for
-- "unscoped" on purpose: an empty list means "may do nothing", and the two must
-- never be spellable the same way.
--
-- `workspace_id` CASCADEs so a narrowing cannot outlive the thing it names. A
-- token pointing at a deleted workspace would either match nothing (dead but
-- confusing) or, if a later id collided, mean something else entirely. CASCADE
-- rather than SET NULL deliberately: SET NULL would WIDEN the token to the whole
-- tenant at the moment its workspace was deleted, which is the one direction a
-- narrowing must never move on its own.
--
-- THE TWO TRACKS DIVERGE HERE, and the SQLite twin says so too: SQLite cannot
-- add a column carrying a foreign key to an existing table, so there the token
-- row SURVIVES a workspace deletion and narrows to an id nothing matches. Both
-- are closed — the credential grants nothing either way — but the observable
-- behaviour differs: on Postgres it disappears from its owner's listing, on
-- SQLite it stays and is inert.
ALTER TABLE user_tokens ADD COLUMN IF NOT EXISTS scopes text;
ALTER TABLE user_tokens ADD COLUMN IF NOT EXISTS workspace_id uuid
    REFERENCES workspaces(id) ON DELETE CASCADE;
