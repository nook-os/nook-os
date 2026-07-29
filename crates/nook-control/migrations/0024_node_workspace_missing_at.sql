-- MAIN-220: reconcile hardening — tombstone unreported checkouts instead of
-- deleting them. Discovery used to DELETE any node_workspaces row a scan stopped
-- reporting, so an empty report (unmounted root, a panicking scan that returned
-- zero paths) erased every checkout a node had while the files sat on disk, and
-- a moved checkout became delete+insert (a new id, broken task references).
--
-- This records a soft-delete marker so reconcile can MARK a row missing and HEAL
-- it on the next report with the SAME id. Hard deletion moves to a retention
-- sweep (rows missing longer than the configured window) and the explicit
-- remove-checkout / workspace-delete paths.
--
-- Idempotent and additive: one nullable column, no existing column touched. A
-- NULL missing_at means "present", matching every existing row on backfill.
ALTER TABLE public.node_workspaces ADD COLUMN IF NOT EXISTS missing_at timestamp with time zone;

-- The retention sweep scans for rows whose missing_at has aged out; a partial
-- index over just the tombstoned rows keeps that scan off the hot present-row set.
CREATE INDEX IF NOT EXISTS idx_node_workspaces_missing_at
    ON public.node_workspaces (missing_at)
    WHERE missing_at IS NOT NULL;
