-- MAIN-385: the per-workspace build-loop switch. OFF for every workspace,
-- including on upgrade (NG-4) — which is why `enabled` is NOT NULL DEFAULT
-- false rather than nullable: there is no third state to express, and a repo
-- nobody has spoken about must not start firing agents when this deploys.
--
-- `build_loop_enabled_by` is the identity an auto-fired job is REQUESTED BY
-- (AC-2), not an audit column: `select_executor` resolves the requester's
-- person and only that person's nodes are candidates, so the enabler is what
-- makes the job placeable at all.
--
-- Concurrency is deliberately NOT a new column: `build_max_replicas` (0052) is
-- already this repo's ceiling on in-flight build runs, read by
-- `converge_builds` and settable from `nook builds scale`. A second knob
-- meaning the same thing could only ever disagree with it.
ALTER TABLE workspaces ADD COLUMN IF NOT EXISTS build_loop_enabled boolean NOT NULL DEFAULT false;
ALTER TABLE workspaces ADD COLUMN IF NOT EXISTS build_loop_node_id uuid;
ALTER TABLE workspaces ADD COLUMN IF NOT EXISTS build_loop_enabled_by uuid;

-- The sweep's whole read (AC-5): a partial index, so a deployment with nothing
-- enabled pays one indexed lookup per pass and touches no workspace rows.
CREATE INDEX IF NOT EXISTS workspaces_build_loop_enabled_idx
    ON workspaces (tenant_id) WHERE build_loop_enabled;
