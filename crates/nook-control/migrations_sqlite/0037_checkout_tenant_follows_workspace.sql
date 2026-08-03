-- SQLite twin of 0037. Hand-authored, per the MAIN-236 rule: nothing generates
-- over these files.
--
-- Two dialect differences from the Postgres original, both from
-- docs/db-dialect-audit.md:
--   * `public.` qualification is stripped — SQLite has no schemas.
--   * SQLite has no `UPDATE ... FROM`, so the same repair is expressed as a
--     correlated subquery. Same rows, same result: a checkout's tenant is set to
--     its workspace's, and only where they currently disagree.
--
-- See the Postgres file for why this repairs rather than deletes, and for the
-- `ON CONFLICT` omission that created these rows.
UPDATE node_workspaces
   SET tenant_id = (SELECT w.tenant_id FROM workspaces w WHERE w.id = node_workspaces.workspace_id)
 WHERE EXISTS (
         SELECT 1 FROM workspaces w
          WHERE w.id = node_workspaces.workspace_id
            AND w.tenant_id <> node_workspaces.tenant_id
       );
