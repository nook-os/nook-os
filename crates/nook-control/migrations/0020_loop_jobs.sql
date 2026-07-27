-- MAIN-127: loop jobs core — the durable `loop_jobs` record and its transcript.
--
-- The first slice of detached loop execution (epic MAIN-35, tiers 1+2). This
-- migration is ONLY the record and its lifecycle state; executor selection
-- (MAIN-160), node-side execution (MAIN-161) and interaction bridging (MAIN-162)
-- add no schema here. A job rides the generic work queue (MAIN-147): creating
-- one enqueues a `loop.job` work item whose payload is the job id, and job state
-- is DB state keyed off queue consumption — there is no bespoke offer/claim
-- table.
--
-- Append-only and idempotent (IF NOT EXISTS / guarded constraint adds) so a
-- database that already has these tables converges instead of failing.

CREATE TABLE IF NOT EXISTS loop_jobs (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL,
    -- What the job runs: a spec interview or an epic decomposition.
    kind text NOT NULL,
    -- The ticket a spec job fills in, or the epic a decompose job breaks down.
    target_task_id uuid NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    -- The workspace the work happens in (derived from the target task). Nullable
    -- because a target task need not be tied to a workspace.
    workspace_id uuid,
    -- The user who asked for the job — the identity a future executor prefers a
    -- node for (MAIN-160) and attributes the work to.
    requested_by uuid NOT NULL,
    -- Lifecycle. `queued` on create; terminal states are completed/failed/
    -- canceled. Transitions are enforced in the service layer, not by the DB.
    state text NOT NULL DEFAULT 'queued',
    -- The node that claimed the job (MAIN-160 populates it); NULL until then.
    executor_node_id uuid,
    -- A re-run points back at the job it replaces (AC-5), so a failed job's
    -- lineage is walkable. NULL for an original job.
    predecessor_job_id uuid REFERENCES loop_jobs(id) ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT loop_jobs_kind_check CHECK (kind IN ('spec', 'decompose')),
    CONSTRAINT loop_jobs_state_check CHECK (state IN (
        'queued', 'claimed', 'running', 'waiting_on_human',
        'completed', 'failed', 'canceled'
    ))
);

-- Tenant-scoped listing and the "what is queued" view a scheduler cares about.
CREATE INDEX IF NOT EXISTS loop_jobs_tenant_idx ON loop_jobs (tenant_id);
CREATE INDEX IF NOT EXISTS loop_jobs_state_idx ON loop_jobs (state);

-- Append-only transcript: the conversation/output captured where the work lives.
-- This slice only stores and reads rows; MAIN-161 writes them from the executor.
-- Ordered by the v7 id, which is time-ordered, so no separate sequence column.
CREATE TABLE IF NOT EXISTS loop_job_transcript (
    id uuid PRIMARY KEY,
    job_id uuid NOT NULL REFERENCES loop_jobs(id) ON DELETE CASCADE,
    -- Where the line came from: 'system' (lifecycle notes), 'agent' (runtime
    -- output), 'human' (a delivered answer). Free-form; consumers do not branch
    -- on it in this slice.
    source text NOT NULL DEFAULT 'system',
    content text NOT NULL,
    at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS loop_job_transcript_job_idx
    ON loop_job_transcript (job_id, id);
