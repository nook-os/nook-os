-- Pin a git credential to a workspace (MAIN-367).
--
-- The credential machinery has been complete since 0001 — `git_credentials`
-- holds the ssh key with its private half encrypted at rest — and the
-- workspace-create UI has always offered a picker. What was missing is where
-- the answer goes: `credential_id` was a field on a CLONE REQUEST, used for
-- that one clone and then discarded. Nothing on the workspace remembered it.
--
-- So clone-on-demand had no credential to send, every operator node fell back
-- to its own generated key, and no private repo authorized it. This column is
-- the binding that makes the reconciler able to clone a private repo at all.
--
-- ON DELETE RESTRICT, deliberately: a credential a workspace depends on must
-- not vanish underneath it. The delete path refuses and names the workspaces
-- still pinning it, which is a far better failure than clones that start
-- mysteriously failing an hour later (AC-8).
ALTER TABLE workspaces
    ADD COLUMN IF NOT EXISTS git_credential_id uuid
        REFERENCES git_credentials (id) ON DELETE RESTRICT;

-- The delete path asks "who still pins this?" on every credential deletion,
-- and the reconciler asks "what key does this workspace use?" on every pass.
CREATE INDEX IF NOT EXISTS workspaces_git_credential_id_idx
    ON workspaces (git_credential_id)
    WHERE git_credential_id IS NOT NULL;
