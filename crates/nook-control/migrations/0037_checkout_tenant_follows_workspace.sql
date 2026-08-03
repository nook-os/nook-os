-- A checkout's tenant must equal its workspace's tenant. Heal the rows where it
-- does not.
--
-- `upsert_checkout` and `associate_clone` both healed a conflicting row in place
-- with `ON CONFLICT (node_id, path) DO UPDATE SET workspace_id = EXCLUDED...`
-- and did NOT update `tenant_id`. So a row left over from an earlier owner got
-- re-pointed at a workspace in another tenant while keeping its old scope. Every
-- read of node_workspaces is tenant-scoped, so `present_checkouts` stopped
-- seeing it: the reconciler decided the node held no checkout, re-cloned it
-- every 60 seconds indefinitely, and never got as far as placing a session. Both
-- statements now set `tenant_id`; this repairs what they already wrote.
--
-- REPAIRS, does not delete. A mismatched row still points at a real checkout on
-- a real disk — deleting it would throw away the record and force a needless
-- re-clone on every deployment that runs this. The workspace is authoritative
-- about which tenant owns the repo, so the row is corrected to agree with it.
--
-- Idempotent by construction: the WHERE clause matches nothing once the rows
-- agree, so re-running is a no-op (and it converges a database that reached the
-- right state by other means).
UPDATE public.node_workspaces nw
   SET tenant_id = w.tenant_id
  FROM public.workspaces w
 WHERE w.id = nw.workspace_id
   AND nw.tenant_id <> w.tenant_id;
