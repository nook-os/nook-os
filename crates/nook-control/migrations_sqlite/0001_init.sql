-- NookOS SQLite schema — the control track's frozen 0001.
--
-- SCAFFOLDED ONCE from the schema the Postgres migrations actually produce
-- (MAIN-236), then hand-corrected and frozen. It is HAND-OWNED from here: the
-- generator that produced it was deleted in the same PR, and nothing
-- regenerates over this file. Forward changes are hand-authored SQLite deltas
-- (0002_…), the twin of the Postgres migration that goes with them.
--
-- Type map (docs/db-dialect-audit.md): uuid / timestamptz / jsonb / text -> TEXT,
-- bigint & friends -> INTEGER, boolean -> INTEGER (0/1), now() ->
-- CURRENT_TIMESTAMP, ::type casts stripped. SQLite's dynamic typing makes TEXT
-- for uuid/timestamptz faithful in practice: the values round-trip as the same
-- strings Postgres renders.

CREATE TABLE IF NOT EXISTS board_columns (
  id TEXT NOT NULL,
  board_id TEXT NOT NULL,
  name TEXT NOT NULL,
  "position" INTEGER NOT NULL DEFAULT 0,
  type TEXT NOT NULL DEFAULT 'unstarted',
  PRIMARY KEY (id),
  CHECK ((type IN ('backlog', 'unstarted', 'started', 'review', 'completed', 'canceled'))),
  FOREIGN KEY (board_id) REFERENCES boards (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS boards (
  id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  workspace_id TEXT,
  name TEXT NOT NULL,
  provider TEXT NOT NULL DEFAULT 'local',
  provider_config TEXT NOT NULL DEFAULT '{}',
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  key TEXT,
  next_number INTEGER NOT NULL DEFAULT 1,
  automation TEXT NOT NULL DEFAULT '{}',
  PRIMARY KEY (id),
  CHECK ((provider IN ('local', 'jira', 'github', 'linear', 'trello'))),
  FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE,
  FOREIGN KEY (workspace_id) REFERENCES workspaces (id) ON DELETE SET NULL
);

CREATE TABLE IF NOT EXISTS bus_outbox (
  id INTEGER NOT NULL,
  payload TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id)
);

CREATE TABLE IF NOT EXISTS email_verification_tokens (
  id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  email TEXT NOT NULL,
  token_hash TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  expires_at TEXT NOT NULL,
  consumed_at TEXT,
  PRIMARY KEY (id),
  FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS events (
  id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  occurred_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  kind TEXT NOT NULL,
  actor_type TEXT,
  actor_id TEXT,
  workspace_id TEXT,
  node_id TEXT,
  session_id TEXT,
  payload TEXT NOT NULL DEFAULT '{}',
  PRIMARY KEY (id),
  FOREIGN KEY (node_id) REFERENCES nodes (id) ON DELETE SET NULL,
  FOREIGN KEY (session_id) REFERENCES sessions (id) ON DELETE SET NULL,
  FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE,
  FOREIGN KEY (workspace_id) REFERENCES workspaces (id) ON DELETE SET NULL
);

CREATE TABLE IF NOT EXISTS feedback (
  id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  workspace_id TEXT,
  session_id TEXT,
  body TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'queued',
  pr_url TEXT,
  created_by TEXT,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  CHECK ((status IN ('queued', 'delivered', 'submitted', 'dropped'))),
  FOREIGN KEY (created_by) REFERENCES users (id) ON DELETE SET NULL,
  FOREIGN KEY (session_id) REFERENCES sessions (id) ON DELETE SET NULL,
  FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE,
  FOREIGN KEY (workspace_id) REFERENCES workspaces (id) ON DELETE SET NULL
);

CREATE TABLE IF NOT EXISTS git_credentials (
  id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  name TEXT NOT NULL,
  kind TEXT NOT NULL DEFAULT 'ssh_key',
  public_key TEXT NOT NULL DEFAULT '',
  secret_enc BLOB NOT NULL,
  created_by TEXT,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  UNIQUE (tenant_id, name),
  CHECK ((kind = 'ssh_key')),
  FOREIGN KEY (created_by) REFERENCES users (id) ON DELETE SET NULL,
  FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS identities (
  id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  issuer TEXT NOT NULL,
  subject TEXT NOT NULL,
  email TEXT,
  raw_claims TEXT NOT NULL DEFAULT '{}',
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  email_verified_at TEXT,
  PRIMARY KEY (id),
  UNIQUE (issuer, subject),
  FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS interactions (
  id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  job_id TEXT,
  task_id TEXT,
  prompt TEXT NOT NULL,
  choices TEXT,
  state TEXT NOT NULL DEFAULT 'pending',
  requested_by_node_id TEXT,
  requested_by_session_id TEXT,
  answered_by TEXT,
  response TEXT,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  answered_at TEXT,
  PRIMARY KEY (id),
  CHECK ((state IN ('pending', 'answered', 'canceled'))),
  FOREIGN KEY (job_id) REFERENCES loop_jobs (id) ON DELETE CASCADE,
  FOREIGN KEY (task_id) REFERENCES tasks (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS invites (
  id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  email TEXT NOT NULL,
  role TEXT NOT NULL DEFAULT 'member',
  token_hash TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'pending',
  invited_by TEXT,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  expires_at TEXT NOT NULL,
  PRIMARY KEY (id),
  UNIQUE (token_hash),
  CHECK ((role IN ('member', 'admin'))),
  CHECK ((status IN ('pending', 'accepted', 'revoked'))),
  FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS join_tokens (
  id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  token_hash TEXT NOT NULL,
  name TEXT NOT NULL DEFAULT '',
  created_by TEXT,
  expires_at TEXT NOT NULL,
  used_at TEXT,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  UNIQUE (token_hash),
  FOREIGN KEY (created_by) REFERENCES users (id) ON DELETE SET NULL,
  FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS labels (
  id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  name TEXT NOT NULL,
  color TEXT NOT NULL DEFAULT '#f0a000',
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  UNIQUE (tenant_id, name),
  FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS loop_job_transcript (
  id TEXT NOT NULL,
  job_id TEXT NOT NULL,
  source TEXT NOT NULL DEFAULT 'system',
  content TEXT NOT NULL,
  at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  FOREIGN KEY (job_id) REFERENCES loop_jobs (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS loop_jobs (
  id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  target_task_id TEXT NOT NULL,
  workspace_id TEXT,
  requested_by TEXT NOT NULL,
  state TEXT NOT NULL DEFAULT 'queued',
  executor_node_id TEXT,
  predecessor_job_id TEXT,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  queued_reason TEXT,
  seed TEXT,
  PRIMARY KEY (id),
  CHECK ((kind IN ('spec', 'decompose'))),
  CHECK ((state IN ('queued', 'claimed', 'running', 'waiting_on_human', 'completed', 'failed', 'canceled'))),
  FOREIGN KEY (predecessor_job_id) REFERENCES loop_jobs (id) ON DELETE SET NULL,
  FOREIGN KEY (target_task_id) REFERENCES tasks (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS mail_sends (
  id TEXT NOT NULL,
  sent_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  category TEXT NOT NULL,
  recipient_domain TEXT NOT NULL,
  PRIMARY KEY (id)
);

CREATE TABLE IF NOT EXISTS managed_content (
  id TEXT NOT NULL,
  kind TEXT NOT NULL,
  name TEXT NOT NULL,
  content TEXT NOT NULL,
  sha256 TEXT NOT NULL,
  version INTEGER NOT NULL DEFAULT 1,
  default_sha256 TEXT NOT NULL,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  UNIQUE (kind, name)
);

CREATE TABLE IF NOT EXISTS node_workspaces (
  id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  node_id TEXT NOT NULL,
  workspace_id TEXT NOT NULL,
  path TEXT NOT NULL,
  git_remote_url TEXT,
  git_remote_normalized TEXT,
  git_branch TEXT,
  git_status TEXT NOT NULL DEFAULT '{}',
  discovered_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  last_scanned_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  missing_at TEXT,
  kind TEXT NOT NULL DEFAULT 'clone',
  PRIMARY KEY (id),
  UNIQUE (node_id, path),
  CHECK ((kind IN ('clone', 'worktree', 'mirror'))),
  FOREIGN KEY (node_id) REFERENCES nodes (id) ON DELETE CASCADE,
  FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE,
  FOREIGN KEY (workspace_id) REFERENCES workspaces (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS nodes (
  id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  name TEXT NOT NULL,
  hostname TEXT NOT NULL DEFAULT '',
  platform TEXT NOT NULL DEFAULT '',
  capabilities TEXT NOT NULL DEFAULT '{}',
  resources TEXT NOT NULL DEFAULT '{}',
  status TEXT NOT NULL DEFAULT 'offline',
  node_token_hash TEXT NOT NULL,
  last_seen_at TEXT,
  owning_instance_id TEXT,
  lease_expires_at TEXT,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  ca_id TEXT,
  cert_not_after TEXT,
  revoked_at TEXT,
  public_key_pem TEXT,
  cert_pem TEXT,
  owner_person_id TEXT,
  shared INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (id),
  UNIQUE (tenant_id, name),
  UNIQUE (node_token_hash),
  CHECK ((status IN ('online', 'offline'))),
  FOREIGN KEY (ca_id) REFERENCES tenant_cas (id),
  FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS notes (
  id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  workspace_id TEXT NOT NULL,
  title TEXT NOT NULL DEFAULT '',
  content_md TEXT NOT NULL DEFAULT '',
  kind TEXT NOT NULL DEFAULT 'rolling',
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE,
  FOREIGN KEY (workspace_id) REFERENCES workspaces (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS notification_channels (
  id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  name TEXT NOT NULL,
  config TEXT NOT NULL DEFAULT '{}',
  enabled INTEGER NOT NULL DEFAULT 1,
  levels TEXT NOT NULL DEFAULT '{}',
  kinds TEXT NOT NULL DEFAULT '{}',
  last_ok_at TEXT,
  last_error TEXT,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  secret TEXT,
  PRIMARY KEY (id),
  UNIQUE (tenant_id, name),
  FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS notifications (
  id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  user_id TEXT,
  level TEXT NOT NULL DEFAULT 'info',
  title TEXT NOT NULL,
  body TEXT NOT NULL DEFAULT '',
  kind TEXT NOT NULL DEFAULT 'custom',
  link TEXT,
  payload TEXT NOT NULL DEFAULT '{}',
  read_at TEXT,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE,
  FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS org_visibility_policy (
  id TEXT NOT NULL,
  org_id TEXT NOT NULL,
  field TEXT NOT NULL,
  enabled INTEGER NOT NULL,
  changed_by TEXT,
  changed_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  FOREIGN KEY (changed_by) REFERENCES users (id) ON DELETE SET NULL,
  FOREIGN KEY (org_id) REFERENCES orgs (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS orgs (
  id TEXT NOT NULL,
  name TEXT NOT NULL,
  slug TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  UNIQUE (slug)
);

CREATE TABLE IF NOT EXISTS permissions (
  key TEXT NOT NULL,
  description TEXT NOT NULL DEFAULT '',
  PRIMARY KEY (key)
);

CREATE TABLE IF NOT EXISTS person_vaults (
  person_id TEXT NOT NULL,
  kdf_salt BLOB NOT NULL,
  verifier BLOB NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (person_id)
);

CREATE TABLE IF NOT EXISTS role_bindings (
  id TEXT NOT NULL,
  subject_type TEXT NOT NULL DEFAULT 'user',
  subject_id TEXT NOT NULL,
  role_key TEXT NOT NULL,
  scope_type TEXT NOT NULL,
  scope_id TEXT,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  created_by TEXT,
  PRIMARY KEY (id),
  CHECK ((((scope_type = 'deployment') AND (scope_id IS NULL)) OR ((scope_type <> 'deployment') AND (scope_id IS NOT NULL)))),
  CHECK ((scope_type IN ('deployment', 'org', 'tenant'))),
  FOREIGN KEY (created_by) REFERENCES users (id) ON DELETE SET NULL,
  FOREIGN KEY (role_key) REFERENCES roles (key) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS role_permissions (
  role_key TEXT NOT NULL,
  permission_key TEXT NOT NULL,
  PRIMARY KEY (role_key, permission_key),
  FOREIGN KEY (permission_key) REFERENCES permissions (key) ON DELETE CASCADE,
  FOREIGN KEY (role_key) REFERENCES roles (key) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS roles (
  key TEXT NOT NULL,
  name TEXT NOT NULL,
  description TEXT NOT NULL DEFAULT '',
  builtin INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (key)
);

CREATE TABLE IF NOT EXISTS sessions (
  id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  workspace_id TEXT,
  node_id TEXT NOT NULL,
  name TEXT NOT NULL DEFAULT '',
  runtime TEXT NOT NULL,
  tmux_session TEXT,
  status TEXT NOT NULL DEFAULT 'starting',
  error TEXT,
  created_by TEXT,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  ended_at TEXT,
  checkout_id TEXT,
  PRIMARY KEY (id),
  CHECK ((status IN ('starting', 'running', 'detached', 'exited', 'error'))),
  FOREIGN KEY (checkout_id) REFERENCES node_workspaces (id) ON DELETE SET NULL,
  FOREIGN KEY (created_by) REFERENCES users (id) ON DELETE SET NULL,
  FOREIGN KEY (node_id) REFERENCES nodes (id) ON DELETE CASCADE,
  FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE,
  FOREIGN KEY (workspace_id) REFERENCES workspaces (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS sessions_auth (
  id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  expires_at TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE,
  FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS settings (
  id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  scope TEXT NOT NULL,
  user_id TEXT,
  key TEXT NOT NULL,
  value TEXT NOT NULL,
  PRIMARY KEY (id),
  UNIQUE (tenant_id, scope, user_id, key),
  CHECK ((scope IN ('tenant', 'user'))),
  FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE,
  FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS skills (
  id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  name TEXT NOT NULL,
  content TEXT NOT NULL,
  sha256 TEXT NOT NULL,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_by TEXT,
  PRIMARY KEY (id),
  FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE,
  FOREIGN KEY (updated_by) REFERENCES users (id) ON DELETE SET NULL
);

CREATE TABLE IF NOT EXISTS task_comments (
  id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  task_id TEXT NOT NULL,
  author_type TEXT NOT NULL,
  author_id TEXT,
  author_name TEXT NOT NULL DEFAULT '',
  body_md TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  CHECK ((author_type IN ('user', 'agent', 'system'))),
  FOREIGN KEY (task_id) REFERENCES tasks (id) ON DELETE CASCADE,
  FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS task_labels (
  task_id TEXT NOT NULL,
  label_id TEXT NOT NULL,
  PRIMARY KEY (task_id, label_id),
  FOREIGN KEY (label_id) REFERENCES labels (id) ON DELETE CASCADE,
  FOREIGN KEY (task_id) REFERENCES tasks (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS task_relations (
  id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  from_task TEXT NOT NULL,
  to_task TEXT NOT NULL,
  kind TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  UNIQUE (from_task, to_task, kind),
  CHECK ((from_task <> to_task)),
  CHECK ((kind IN ('blocks', 'relates', 'duplicates'))),
  FOREIGN KEY (from_task) REFERENCES tasks (id) ON DELETE CASCADE,
  FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE,
  FOREIGN KEY (to_task) REFERENCES tasks (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS tasks (
  id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  board_id TEXT NOT NULL,
  column_id TEXT NOT NULL,
  title TEXT NOT NULL,
  description TEXT,
  "position" INTEGER NOT NULL DEFAULT 0,
  external_id TEXT,
  external_url TEXT,
  assignee_user_id TEXT,
  workspace_id TEXT,
  assigned_node_id TEXT,
  branch TEXT,
  worktree_path TEXT,
  worktree_node_id TEXT,
  session_id TEXT,
  pr_url TEXT,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  number INTEGER,
  priority INTEGER NOT NULL DEFAULT 0,
  archived_at TEXT,
  type TEXT NOT NULL DEFAULT 'task',
  visibility TEXT NOT NULL DEFAULT 'team',
  created_by TEXT,
  parent_task_id TEXT,
  checkout_id TEXT,
  PRIMARY KEY (id),
  CHECK (((priority >= 0) AND (priority <= 4))),
  CHECK ((type IN ('task', 'bug', 'epic', 'story', 'chore'))),
  CHECK ((visibility IN ('private', 'team', 'org'))),
  FOREIGN KEY (assigned_node_id) REFERENCES nodes (id) ON DELETE SET NULL,
  FOREIGN KEY (assignee_user_id) REFERENCES users (id) ON DELETE SET NULL,
  FOREIGN KEY (board_id) REFERENCES boards (id) ON DELETE CASCADE,
  FOREIGN KEY (checkout_id) REFERENCES node_workspaces (id) ON DELETE SET NULL,
  FOREIGN KEY (column_id) REFERENCES board_columns (id) ON DELETE CASCADE,
  FOREIGN KEY (parent_task_id) REFERENCES tasks (id) ON DELETE SET NULL,
  FOREIGN KEY (session_id) REFERENCES sessions (id) ON DELETE SET NULL,
  FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE,
  FOREIGN KEY (workspace_id) REFERENCES workspaces (id) ON DELETE SET NULL,
  FOREIGN KEY (worktree_node_id) REFERENCES nodes (id) ON DELETE SET NULL
);

CREATE TABLE IF NOT EXISTS tenant_cas (
  id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  state TEXT NOT NULL DEFAULT 'staged',
  cert_pem TEXT NOT NULL,
  key_enc BLOB NOT NULL,
  fingerprint TEXT NOT NULL,
  not_after TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  retired_at TEXT,
  PRIMARY KEY (id),
  CHECK ((state IN ('staged', 'active', 'retiring'))),
  FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS tenant_members (
  id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  principal_type TEXT NOT NULL DEFAULT 'user',
  principal_id TEXT NOT NULL,
  role TEXT NOT NULL DEFAULT 'member',
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  UNIQUE (tenant_id, principal_type, principal_id),
  CHECK ((principal_type IN ('user', 'group', 'service'))),
  CHECK ((role IN ('owner', 'admin', 'member'))),
  FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS tenants (
  id TEXT NOT NULL,
  name TEXT NOT NULL,
  slug TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  auth_mode TEXT,
  org_id TEXT NOT NULL DEFAULT '00000000-0000-0000-0000-0000000000a1',
  PRIMARY KEY (id),
  UNIQUE (slug),
  CHECK (((auth_mode IS NULL) OR (auth_mode IN ('oidc', 'local')))),
  FOREIGN KEY (org_id) REFERENCES orgs (id)
);

CREATE TABLE IF NOT EXISTS themes (
  id TEXT NOT NULL,
  tenant_id TEXT,
  name TEXT NOT NULL,
  slug TEXT NOT NULL,
  tokens TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  UNIQUE (slug),
  FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS user_note_folders (
  id TEXT NOT NULL,
  person_id TEXT NOT NULL,
  parent_id TEXT,
  name TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  FOREIGN KEY (parent_id) REFERENCES user_note_folders (id) ON DELETE SET NULL
);

CREATE TABLE IF NOT EXISTS user_notes (
  id TEXT NOT NULL,
  person_id TEXT NOT NULL,
  folder_id TEXT,
  title TEXT NOT NULL,
  content_enc BLOB NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  sealed_salt BLOB,
  sealed_verifier BLOB,
  PRIMARY KEY (id),
  FOREIGN KEY (folder_id) REFERENCES user_note_folders (id) ON DELETE SET NULL
);

CREATE TABLE IF NOT EXISTS user_passkeys (
  id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  credential_id TEXT NOT NULL,
  label TEXT NOT NULL DEFAULT '',
  wrapped_secret BLOB NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  last_used_at TEXT,
  PRIMARY KEY (id),
  UNIQUE (user_id, credential_id),
  FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE,
  FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS user_tokens (
  id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  token_hash TEXT NOT NULL,
  name TEXT NOT NULL DEFAULT '',
  last_used_at TEXT,
  expires_at TEXT,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  UNIQUE (token_hash),
  FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE,
  FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS user_vaults (
  user_id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  kdf_salt BLOB NOT NULL,
  verifier BLOB NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (user_id),
  FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE,
  FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS users (
  id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  display_name TEXT NOT NULL,
  email TEXT NOT NULL,
  avatar_url TEXT,
  role TEXT NOT NULL DEFAULT 'member',
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  username TEXT,
  password_hash TEXT,
  -- HAND-CORRECTED (MAIN-236): Postgres defaults this to gen_random_uuid(),
  -- which SQLite has no equivalent for. The application already supplies a
  -- person id on every insert, so dropping the default is faithful; a trigger
  -- faking one would only hide a missing write.
  person_id TEXT NOT NULL,
  PRIMARY KEY (id),
  UNIQUE (tenant_id, email),
  CHECK ((role IN ('owner', 'admin', 'member'))),
  FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS work_queue (
  id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  work_type TEXT NOT NULL,
  payload BLOB NOT NULL,
  attempts INTEGER NOT NULL DEFAULT 0,
  max_attempts INTEGER NOT NULL DEFAULT 20,
  not_before TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  enqueued_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  locked_until TEXT,
  PRIMARY KEY (id)
);

CREATE TABLE IF NOT EXISTS work_queue_dead (
  id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  work_type TEXT NOT NULL,
  payload BLOB NOT NULL,
  attempts INTEGER NOT NULL,
  max_attempts INTEGER NOT NULL,
  enqueued_at TEXT NOT NULL,
  died_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  reason TEXT NOT NULL,
  PRIMARY KEY (id)
);

CREATE TABLE IF NOT EXISTS workspace_secrets (
  id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  workspace_id TEXT NOT NULL,
  name TEXT NOT NULL DEFAULT '.env',
  content_enc BLOB NOT NULL,
  kdf_salt BLOB,
  verifier BLOB,
  ephemeral INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  UNIQUE (workspace_id, name),
  FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE,
  FOREIGN KEY (workspace_id) REFERENCES workspaces (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS workspaces (
  id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  name TEXT NOT NULL,
  slug TEXT NOT NULL,
  description TEXT,
  git_remote_normalized TEXT,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  git_remote_url TEXT,
  PRIMARY KEY (id),
  UNIQUE (tenant_id, slug),
  FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE
);

-- Indexes.
CREATE INDEX idx_board_columns_type ON board_columns (board_id, type, "position");
CREATE UNIQUE INDEX idx_boards_tenant_key ON boards (tenant_id, key) WHERE (key IS NOT NULL);
CREATE UNIQUE INDEX email_verification_one_live_per_user ON email_verification_tokens (user_id) WHERE (consumed_at IS NULL);
CREATE INDEX email_verification_token_hash_idx ON email_verification_tokens (token_hash);
CREATE INDEX idx_events_tenant_time ON events (tenant_id, occurred_at DESC);
CREATE INDEX idx_events_workspace_time ON events (tenant_id, workspace_id, occurred_at DESC);
CREATE INDEX feedback_tenant_idx ON feedback (tenant_id, created_at DESC);
CREATE INDEX interactions_job_idx ON interactions (job_id);
CREATE INDEX interactions_pending_idx ON interactions (tenant_id, state) WHERE (state = 'pending');
CREATE INDEX interactions_task_idx ON interactions (task_id);
CREATE INDEX interactions_tenant_idx ON interactions (tenant_id);
CREATE UNIQUE INDEX invites_one_pending_per_email ON invites (tenant_id, lower(email)) WHERE (status = 'pending');
CREATE INDEX invites_token_hash_idx ON invites (token_hash);
CREATE INDEX loop_job_transcript_job_idx ON loop_job_transcript (job_id, id);
CREATE INDEX loop_jobs_state_idx ON loop_jobs (state);
CREATE INDEX loop_jobs_tenant_idx ON loop_jobs (tenant_id);
CREATE INDEX mail_sends_sent_at_idx ON mail_sends (sent_at);
CREATE INDEX idx_node_workspaces_missing_at ON node_workspaces (missing_at) WHERE (missing_at IS NOT NULL);
CREATE INDEX idx_node_workspaces_workspace ON node_workspaces (workspace_id);
CREATE INDEX idx_nodes_ca ON nodes (ca_id);
CREATE INDEX nodes_lease_idx ON nodes (owning_instance_id) WHERE (owning_instance_id IS NOT NULL);
CREATE INDEX nodes_owner_person_id_idx ON nodes (owner_person_id);
CREATE INDEX idx_notes_workspace ON notes (tenant_id, workspace_id);
CREATE INDEX idx_notification_channels_tenant ON notification_channels (tenant_id) WHERE enabled;
CREATE INDEX idx_notifications_inbox ON notifications (tenant_id, created_at DESC);
CREATE INDEX idx_notifications_unread ON notifications (tenant_id) WHERE (read_at IS NULL);
CREATE INDEX idx_org_visibility_current ON org_visibility_policy (org_id, field, changed_at DESC);
CREATE INDEX idx_role_bindings_subject ON role_bindings (subject_type, subject_id);
CREATE UNIQUE INDEX idx_role_bindings_unique ON role_bindings (subject_type, subject_id, role_key, scope_type, COALESCE(scope_id, '00000000-0000-0000-0000-000000000000'));
CREATE INDEX idx_sessions_checkout_id ON sessions (checkout_id) WHERE (checkout_id IS NOT NULL);
CREATE INDEX idx_sessions_node ON sessions (node_id);
CREATE INDEX idx_sessions_workspace ON sessions (tenant_id, workspace_id);
CREATE INDEX idx_sessions_auth_expiry ON sessions_auth (expires_at);
CREATE UNIQUE INDEX skills_tenant_name_key ON skills (tenant_id, name);
CREATE INDEX idx_task_comments_task ON task_comments (task_id, created_at);
CREATE INDEX idx_task_labels_label ON task_labels (label_id);
CREATE INDEX idx_task_relations_from ON task_relations (from_task, kind);
CREATE INDEX idx_task_relations_to ON task_relations (to_task, kind);
CREATE INDEX idx_tasks_board ON tasks (board_id, column_id, "position");
CREATE UNIQUE INDEX idx_tasks_board_number ON tasks (board_id, number) WHERE (number IS NOT NULL);
CREATE INDEX idx_tasks_pick ON tasks (board_id, priority, created_at);
CREATE INDEX tasks_checkout_id_idx ON tasks (checkout_id) WHERE (checkout_id IS NOT NULL);
CREATE INDEX tasks_created_by_idx ON tasks (created_by);
CREATE INDEX tasks_live_idx ON tasks (board_id) WHERE (archived_at IS NULL);
CREATE INDEX tasks_parent_task_id_idx ON tasks (parent_task_id);
CREATE INDEX idx_tenant_cas_tenant ON tenant_cas (tenant_id);
CREATE UNIQUE INDEX tenant_cas_one_active ON tenant_cas (tenant_id) WHERE (state = 'active');
CREATE INDEX idx_tenant_members_principal ON tenant_members (principal_type, principal_id);
CREATE INDEX idx_tenants_org ON tenants (org_id);
CREATE INDEX user_note_folders_person_idx ON user_note_folders (person_id);
CREATE INDEX user_notes_folder_idx ON user_notes (folder_id);
CREATE INDEX user_notes_person_idx ON user_notes (person_id);
CREATE INDEX idx_user_passkeys_user ON user_passkeys (user_id);
CREATE INDEX idx_user_tokens_user ON user_tokens (user_id);
CREATE INDEX users_person_id_idx ON users (person_id);
CREATE UNIQUE INDEX users_tenant_username_unique ON users (tenant_id, lower(username)) WHERE (username IS NOT NULL);
CREATE INDEX work_queue_drain_idx ON work_queue (work_type, not_before, enqueued_at);
CREATE INDEX work_queue_dead_type_idx ON work_queue_dead (work_type);
CREATE UNIQUE INDEX workspaces_remote_idx ON workspaces (tenant_id, git_remote_normalized) WHERE (git_remote_normalized IS NOT NULL);

-- Seed rows the Postgres migrations insert.
-- Ordered parents-first: SQLite enforces foreign keys (sqlx opens pools with
-- PRAGMA foreign_keys=ON), so role_permissions must follow roles and
-- permissions. The alphabetical order the scaffold emitted fails on a fresh
-- SQLite database — caught by tests/sqlite_scaffold.rs, not by reading.
INSERT INTO roles (key, name, description, builtin) VALUES ('operator', 'Operator', 'true', 'Runs this deployment or org. Sees metadata, never session content.');
INSERT INTO roles (key, name, description, builtin) VALUES ('org_admin', 'Org admin', 'true', 'Administers an org and the tenants under it.');
INSERT INTO roles (key, name, description, builtin) VALUES ('tenant_admin', 'Tenant admin', 'true', 'Administers one tenant.');
INSERT INTO roles (key, name, description, builtin) VALUES ('member', 'Member', 'true', 'Ordinary access to a tenant.');
INSERT INTO permissions (key, description) VALUES ('org.view', 'See that an org and its tenants exist');
INSERT INTO permissions (key, description) VALUES ('org.manage', 'Rename an org, move tenants between orgs');
INSERT INTO permissions (key, description) VALUES ('tenant.view', 'See a tenant exists, and its membership counts');
INSERT INTO permissions (key, description) VALUES ('tenant.manage', 'Administer a tenant: members, settings');
INSERT INTO permissions (key, description) VALUES ('node.view', 'See nodes: name, status, resources, session counts');
INSERT INTO permissions (key, description) VALUES ('node.manage', 'Revoke or remove a node');
INSERT INTO permissions (key, description) VALUES ('audit.view', 'Read audit records');
INSERT INTO permissions (key, description) VALUES ('ca.rotate', 'Rotate a tenant certificate authority');
INSERT INTO permissions (key, description) VALUES ('policy.view', 'Read an org visibility policy');
INSERT INTO permissions (key, description) VALUES ('policy.manage', 'Change an org visibility policy');
INSERT INTO permissions (key, description) VALUES ('rbac.grant', 'Grant or revoke a role binding');
INSERT INTO orgs (id, name, slug, created_at, updated_at) VALUES ('00000000-0000-0000-0000-0000000000a1', 'Default', 'default', '2026-07-29T23:35:26.308379+00:00', '2026-07-29T23:35:26.308379+00:00');
INSERT INTO role_permissions (role_key, permission_key) VALUES ('operator', 'org.view');
INSERT INTO role_permissions (role_key, permission_key) VALUES ('operator', 'tenant.view');
INSERT INTO role_permissions (role_key, permission_key) VALUES ('operator', 'node.view');
INSERT INTO role_permissions (role_key, permission_key) VALUES ('operator', 'node.manage');
INSERT INTO role_permissions (role_key, permission_key) VALUES ('operator', 'audit.view');
INSERT INTO role_permissions (role_key, permission_key) VALUES ('operator', 'ca.rotate');
INSERT INTO role_permissions (role_key, permission_key) VALUES ('operator', 'policy.view');
INSERT INTO role_permissions (role_key, permission_key) VALUES ('operator', 'policy.manage');
INSERT INTO role_permissions (role_key, permission_key) VALUES ('org_admin', 'org.view');
INSERT INTO role_permissions (role_key, permission_key) VALUES ('org_admin', 'org.manage');
INSERT INTO role_permissions (role_key, permission_key) VALUES ('org_admin', 'tenant.view');
INSERT INTO role_permissions (role_key, permission_key) VALUES ('org_admin', 'node.view');
INSERT INTO role_permissions (role_key, permission_key) VALUES ('org_admin', 'audit.view');
INSERT INTO role_permissions (role_key, permission_key) VALUES ('org_admin', 'policy.view');
INSERT INTO role_permissions (role_key, permission_key) VALUES ('org_admin', 'policy.manage');
INSERT INTO role_permissions (role_key, permission_key) VALUES ('tenant_admin', 'tenant.view');
INSERT INTO role_permissions (role_key, permission_key) VALUES ('tenant_admin', 'tenant.manage');
INSERT INTO role_permissions (role_key, permission_key) VALUES ('tenant_admin', 'node.view');
INSERT INTO role_permissions (role_key, permission_key) VALUES ('tenant_admin', 'node.manage');
INSERT INTO role_permissions (role_key, permission_key) VALUES ('tenant_admin', 'audit.view');
INSERT INTO role_permissions (role_key, permission_key) VALUES ('tenant_admin', 'policy.view');
INSERT INTO role_permissions (role_key, permission_key) VALUES ('member', 'tenant.view');
INSERT INTO role_permissions (role_key, permission_key) VALUES ('member', 'node.view');
INSERT INTO role_permissions (role_key, permission_key) VALUES ('operator', 'rbac.grant');
INSERT INTO role_permissions (role_key, permission_key) VALUES ('org_admin', 'rbac.grant');
INSERT INTO role_permissions (role_key, permission_key) VALUES ('operator', 'org.manage');
