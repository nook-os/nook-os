-- NookOS bootstrap schema.
--
-- During the bootstrap phase this is THE migration: schema changes are edits
-- to this file, and the dev workflow is `docker compose down -v` → `./run.sh`
-- (wipe the database, bring it all back). Incremental migrations start only
-- once the bootstrap schema settles.
--
-- Multi-tenancy: every tenant-owned table carries tenant_id and every query
-- filters by the authenticated session's tenant. Postgres RLS is a future
-- hardening step on top of this, not a replacement for it.

-- ── Tenancy ─────────────────────────────────────────────────────────────────

CREATE TABLE tenants (
    id          UUID PRIMARY KEY,
    name        TEXT NOT NULL,
    slug        TEXT NOT NULL UNIQUE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE users (
    id            UUID PRIMARY KEY,
    tenant_id     UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    display_name  TEXT NOT NULL,
    email         TEXT NOT NULL,
    avatar_url    TEXT,
    role          TEXT NOT NULL DEFAULT 'member' CHECK (role IN ('owner', 'admin', 'member')),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, email)
);

-- Identity is delegated: NookOS never stores credentials, only the link to
-- the customer's IdP (issuer + subject).
CREATE TABLE identities (
    id          UUID PRIMARY KEY,
    user_id     UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    issuer      TEXT NOT NULL,
    subject     TEXT NOT NULL,
    email       TEXT,
    raw_claims  JSONB NOT NULL DEFAULT '{}',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (issuer, subject)
);

-- Browser login sessions. The cookie stores this opaque id — revocable,
-- no JWT key management.
CREATE TABLE sessions_auth (
    id          UUID PRIMARY KEY,
    user_id     UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    tenant_id   UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    expires_at  TIMESTAMPTZ NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_sessions_auth_expiry ON sessions_auth (expires_at);

-- ── Nodes ───────────────────────────────────────────────────────────────────

CREATE TABLE join_tokens (
    id          UUID PRIMARY KEY,
    tenant_id   UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    token_hash  TEXT NOT NULL UNIQUE,
    name        TEXT NOT NULL DEFAULT '',
    created_by  UUID REFERENCES users(id) ON DELETE SET NULL,
    expires_at  TIMESTAMPTZ NOT NULL,
    used_at     TIMESTAMPTZ,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE nodes (
    id               UUID PRIMARY KEY,
    tenant_id        UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    name             TEXT NOT NULL,
    hostname         TEXT NOT NULL DEFAULT '',
    platform         TEXT NOT NULL DEFAULT '',
    capabilities     JSONB NOT NULL DEFAULT '{}',
    resources        JSONB NOT NULL DEFAULT '{}',
    status           TEXT NOT NULL DEFAULT 'offline' CHECK (status IN ('online', 'offline')),
    node_token_hash  TEXT NOT NULL UNIQUE,
    last_seen_at     TIMESTAMPTZ,
    -- Multi-instance control plane: which instance holds this node's WS.
    -- Renewed on heartbeat; an expired lease means offline/reconnecting.
    owning_instance_id UUID,
    lease_expires_at   TIMESTAMPTZ,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name)
);
CREATE INDEX nodes_lease_idx ON nodes (owning_instance_id) WHERE owning_instance_id IS NOT NULL;

-- Cross-instance bus payloads too large for NOTIFY's 8KB limit (e.g. synced
-- workspace files). The NOTIFY carries just the row id; the receiver fetches
-- and deletes. Stale rows are pruned by the bus maintenance loop.
CREATE TABLE bus_outbox (
    id         BIGSERIAL PRIMARY KEY,
    payload    TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ── Workspaces ──────────────────────────────────────────────────────────────

CREATE TABLE workspaces (
    id           UUID PRIMARY KEY,
    tenant_id    UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    slug         TEXT NOT NULL,
    description  TEXT,
    -- A repository's identity is its normalized remote. It lives here, on the
    -- workspace itself, so removing every checkout doesn't orphan the identity
    -- and make the next clone create a duplicate workspace.
    git_remote_normalized TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, slug)
);
CREATE UNIQUE INDEX workspaces_remote_idx
    ON workspaces (tenant_id, git_remote_normalized)
    WHERE git_remote_normalized IS NOT NULL;

-- One workspace can exist on many nodes; nodes are infrastructure,
-- workspaces are where people work.
CREATE TABLE node_workspaces (
    id               UUID PRIMARY KEY,
    tenant_id        UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    node_id          UUID NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    workspace_id     UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    path                  TEXT NOT NULL,
    git_remote_url        TEXT,
    -- Normalized identity key (host/org/repo, no scheme/creds/.git): the M1
    -- rule for "same workspace on multiple nodes". Raw URL kept alongside so
    -- smarter matching can re-reconcile later.
    git_remote_normalized TEXT,
    git_branch            TEXT,
    git_status       JSONB NOT NULL DEFAULT '{}',
    discovered_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_scanned_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (node_id, path)
);
CREATE INDEX idx_node_workspaces_workspace ON node_workspaces (workspace_id);

-- ── Sessions (tmux-backed, persistent) ──────────────────────────────────────

CREATE TABLE sessions (
    id            UUID PRIMARY KEY,
    tenant_id     UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    workspace_id  UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    node_id       UUID NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    name          TEXT NOT NULL DEFAULT '',
    runtime       TEXT NOT NULL,
    tmux_session  TEXT,
    status        TEXT NOT NULL DEFAULT 'starting'
                  CHECK (status IN ('starting', 'running', 'detached', 'exited', 'error')),
    created_by    UUID REFERENCES users(id) ON DELETE SET NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    ended_at      TIMESTAMPTZ
);
CREATE INDEX idx_sessions_workspace ON sessions (tenant_id, workspace_id);
CREATE INDEX idx_sessions_node ON sessions (node_id);

-- ── Kanban (federated; local boards are one provider among many) ────────────

CREATE TABLE boards (
    id               UUID PRIMARY KEY,
    tenant_id        UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    workspace_id     UUID REFERENCES workspaces(id) ON DELETE SET NULL,
    name             TEXT NOT NULL,
    provider         TEXT NOT NULL DEFAULT 'local'
                     CHECK (provider IN ('local', 'jira', 'github', 'linear', 'trello')),
    provider_config  JSONB NOT NULL DEFAULT '{}',
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE board_columns (
    id        UUID PRIMARY KEY,
    board_id  UUID NOT NULL REFERENCES boards(id) ON DELETE CASCADE,
    name      TEXT NOT NULL,
    position  INT NOT NULL DEFAULT 0
);

CREATE TABLE tasks (
    id                UUID PRIMARY KEY,
    tenant_id         UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    board_id          UUID NOT NULL REFERENCES boards(id) ON DELETE CASCADE,
    column_id         UUID NOT NULL REFERENCES board_columns(id) ON DELETE CASCADE,
    title             TEXT NOT NULL,
    description       TEXT,
    position          INT NOT NULL DEFAULT 0,
    external_id       TEXT,
    external_url      TEXT,
    assignee_user_id  UUID REFERENCES users(id) ON DELETE SET NULL,
    workspace_id      UUID REFERENCES workspaces(id) ON DELETE SET NULL,
    -- Git-linked work lifecycle: triage assigns a node, start-work creates a
    -- worktree + session, submit-pr records the PR, prune removes the worktree.
    assigned_node_id  UUID REFERENCES nodes(id) ON DELETE SET NULL,
    branch            TEXT,
    worktree_path     TEXT,
    worktree_node_id  UUID REFERENCES nodes(id) ON DELETE SET NULL,
    session_id        UUID REFERENCES sessions(id) ON DELETE SET NULL,
    pr_url            TEXT,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_tasks_board ON tasks (board_id, column_id, position);

-- ── Activity timeline ───────────────────────────────────────────────────────

CREATE TABLE events (
    id            UUID PRIMARY KEY,
    tenant_id     UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    occurred_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    kind          TEXT NOT NULL,
    actor_type    TEXT,
    actor_id      UUID,
    workspace_id  UUID REFERENCES workspaces(id) ON DELETE SET NULL,
    node_id       UUID REFERENCES nodes(id) ON DELETE SET NULL,
    session_id    UUID REFERENCES sessions(id) ON DELETE SET NULL,
    payload       JSONB NOT NULL DEFAULT '{}'
);
CREATE INDEX idx_events_tenant_time ON events (tenant_id, occurred_at DESC);
CREATE INDEX idx_events_workspace_time ON events (tenant_id, workspace_id, occurred_at DESC);

-- ── Rolling notes ───────────────────────────────────────────────────────────

CREATE TABLE notes (
    id            UUID PRIMARY KEY,
    tenant_id     UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    workspace_id  UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    title         TEXT NOT NULL DEFAULT '',
    content_md    TEXT NOT NULL DEFAULT '',
    kind          TEXT NOT NULL DEFAULT 'rolling',
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_notes_workspace ON notes (tenant_id, workspace_id);

-- ── Encrypted vault ─────────────────────────────────────────────────────────
-- Secrets are AES-256-GCM encrypted by the control plane (SECRETS_KEY env)
-- before touching the database: nonce(12) || ciphertext in BYTEA.

-- Tenant-level git credentials (SSH keys) so any node can clone private
-- repos. Only the public half is ever returned by the API.
CREATE TABLE git_credentials (
    id          UUID PRIMARY KEY,
    tenant_id   UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    kind        TEXT NOT NULL DEFAULT 'ssh_key' CHECK (kind IN ('ssh_key')),
    public_key  TEXT NOT NULL DEFAULT '',
    secret_enc  BYTEA NOT NULL,
    created_by  UUID REFERENCES users(id) ON DELETE SET NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name)
);

-- Per-workspace env files (.env and friends), synced to checkouts on nodes.
CREATE TABLE workspace_secrets (
    id            UUID PRIMARY KEY,
    tenant_id     UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    workspace_id  UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    name          TEXT NOT NULL DEFAULT '.env',
    content_enc   BYTEA NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, name)
);

-- ── Themes & settings ───────────────────────────────────────────────────────

CREATE TABLE themes (
    id          UUID PRIMARY KEY,
    tenant_id   UUID REFERENCES tenants(id) ON DELETE CASCADE,  -- NULL = built-in
    name        TEXT NOT NULL,
    slug        TEXT NOT NULL UNIQUE,
    tokens      JSONB NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE settings (
    id         UUID PRIMARY KEY,
    tenant_id  UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    scope      TEXT NOT NULL CHECK (scope IN ('tenant', 'user')),
    user_id    UUID REFERENCES users(id) ON DELETE CASCADE,
    key        TEXT NOT NULL,
    value      JSONB NOT NULL,
    UNIQUE NULLS NOT DISTINCT (tenant_id, scope, user_id, key)
);
