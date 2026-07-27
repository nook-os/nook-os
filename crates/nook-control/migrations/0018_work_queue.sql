-- MAIN-147: the durable work queue (database provider).
--
-- Two tables behind the `Queue` trait in nook-infra. `work_queue` holds live
-- work drained with `FOR UPDATE SKIP LOCKED`; `work_queue_dead` is the
-- dead-letter destination for messages that exhausted their attempts or were
-- explicitly retired. Payload is opaque bytes — the queue never interprets it
-- (callers serialize JSON by convention), matching the cache/storage "opaque
-- bytes" contract. Idempotent (IF NOT EXISTS) so a database that already has
-- these tables converges instead of failing.
CREATE TABLE IF NOT EXISTS work_queue (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL,
    -- Free-form routing string, e.g. 'node.build'. Consumers filter on it.
    work_type text NOT NULL,
    -- Opaque bytes; the queue stores and returns them without looking inside.
    payload bytea NOT NULL,
    -- Delivery count, incremented on each receive. At max_attempts the row is
    -- moved to work_queue_dead instead of being delivered again.
    attempts int NOT NULL DEFAULT 0,
    max_attempts int NOT NULL DEFAULT 20,
    -- Scheduling delay: the row is invisible to receive until this instant.
    not_before timestamptz NOT NULL DEFAULT now(),
    enqueued_at timestamptz NOT NULL DEFAULT now(),
    -- Visibility timeout: set on receive, the row is invisible until it passes.
    -- NULL means visible now. A crashed consumer's row reappears when this
    -- elapses (at-least-once).
    locked_until timestamptz
);

-- receive() scans for visible rows of a type in enqueue order; this covers the
-- (type, not_before, enqueued_at) predicate + sort. locked_until is checked in
-- the same predicate but left out of the index key: it is low-selectivity and
-- changes on every receive, so indexing it would churn for little gain.
CREATE INDEX IF NOT EXISTS work_queue_drain_idx
    ON work_queue (work_type, not_before, enqueued_at);

CREATE TABLE IF NOT EXISTS work_queue_dead (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL,
    work_type text NOT NULL,
    payload bytea NOT NULL,
    attempts int NOT NULL,
    max_attempts int NOT NULL,
    enqueued_at timestamptz NOT NULL,
    died_at timestamptz NOT NULL DEFAULT now(),
    -- Why it died: 'max attempts exhausted' or a handler-supplied nack reason.
    reason text NOT NULL
);

CREATE INDEX IF NOT EXISTS work_queue_dead_type_idx ON work_queue_dead (work_type);
