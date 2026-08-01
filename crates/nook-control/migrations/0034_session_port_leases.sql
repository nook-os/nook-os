-- Leased ports for parallel sessions (MAIN-301).
--
-- Two worktrees of one app contend for the same hardcoded port, so only one
-- can run at a time. A session now leases ports from its node's range instead.
--
-- NAMED REQUIREMENTS, NOT ONE PORT. The first cut of this migration gave each
-- session a single `sessions.leased_port` exported as `NOOK_PORT`, which made
-- the broker know one framework's convention and could not serve an app with a
-- web port AND an api port. The workspace now DECLARES what it needs — name,
-- env var, protocol — and the control plane only decides which numbers satisfy
-- it. That is what lets a Next.js app, an ASP.NET service and a Rust backend
-- lease from the same node without this end knowing anything about any of them.
ALTER TABLE public.workspaces ADD COLUMN IF NOT EXISTS port_requirements jsonb;

-- One row per satisfied requirement. A table now, because "how many ports does
-- a session hold" is no longer one.
CREATE TABLE IF NOT EXISTS public.session_port_leases (
    id          uuid PRIMARY KEY,
    session_id  uuid NOT NULL REFERENCES public.sessions (id) ON DELETE CASCADE,
    node_id     uuid NOT NULL REFERENCES public.nodes (id) ON DELETE CASCADE,
    -- The requirement this satisfies, carried so the node exports the right
    -- variable and the UI can say which listener holds the port.
    name        text NOT NULL,
    env         text NOT NULL,
    port        integer NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);

-- THE RACE ARBITER. Two sessions starting at once both read the same free port;
-- one insert wins, the other takes a unique violation the broker reads as "pick
-- again". No advisory lock, no lease TTL, no window.
CREATE UNIQUE INDEX IF NOT EXISTS session_port_leases_one_per_port
    ON public.session_port_leases (node_id, port);

-- Re-leasing a requirement is idempotent rather than additive: a session that
-- restarts keeps one lease per name instead of accumulating them.
CREATE UNIQUE INDEX IF NOT EXISTS session_port_leases_one_per_name
    ON public.session_port_leases (session_id, name);

CREATE INDEX IF NOT EXISTS session_port_leases_by_session
    ON public.session_port_leases (session_id);

-- RECLAIM IS LAZY, AND THAT IS THE POINT (AC-4). Nothing releases a lease when
-- a session ends, is killed, or is reaped: the broker drops the rows of
-- non-live sessions on the node as the first step of allocating, so a dead
-- session's ports come back at the exact moment somebody needs one. The first
-- cut got this from a partial index over live statuses, which only worked while
-- the lease lived on the session row itself. Doing it in the allocator keeps the
-- same property — no cleanup path on any exit — now that it cannot.

-- The operator's range for a node, overriding what the node advertises. Null
-- means "use what the node reported" — the same shape as MAIN-314's labels,
-- where a stored value is an operator's deliberate override and absence means
-- the reported truth.
ALTER TABLE public.nodes ADD COLUMN IF NOT EXISTS port_range_start integer;
ALTER TABLE public.nodes ADD COLUMN IF NOT EXISTS port_range_end integer;
