-- NookOS schema.
--
-- One file, deliberately. This replaces nineteen numbered migrations that were
-- squashed on 2026-07-23, while this deployment had exactly one operator and
-- one production database — the last moment squashing was cheap.
--
-- The schema below is `pg_dump --schema-only` of a database built by applying
-- all nineteen in order, so it is what they produced rather than what they were
-- believed to produce. The seeding statements at the end are copied VERBATIM
-- from the migrations that carried them (0015, 0018, 0019) rather than dumped
-- from a live database, which would have baked one environment's rows into the
-- schema every future deployment starts from.
--
-- Existing databases were re-stamped: `_sqlx_migrations` was reduced to a
-- single row carrying this file's checksum. That is normally the thing you must
-- never do — a checksum you rewrite is a proof that says "verified" without
-- anything having been verified — and it was done here once, knowingly, with a
-- verified backup and against a schema diffed to be identical.
--
-- From here the append-only rule resumes: the next change is 0002.

--
--

--
-- Name: board_columns; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.board_columns (
    id uuid NOT NULL,
    board_id uuid NOT NULL,
    name text NOT NULL,
    "position" integer DEFAULT 0 NOT NULL,
    type text DEFAULT 'unstarted'::text NOT NULL,
    CONSTRAINT board_columns_type_check CHECK ((type = ANY (ARRAY['backlog'::text, 'unstarted'::text, 'started'::text, 'completed'::text, 'canceled'::text])))
);

--
-- Name: boards; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.boards (
    id uuid NOT NULL,
    tenant_id uuid NOT NULL,
    workspace_id uuid,
    name text NOT NULL,
    provider text DEFAULT 'local'::text NOT NULL,
    provider_config jsonb DEFAULT '{}'::jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    key text,
    next_number integer DEFAULT 1 NOT NULL,
    CONSTRAINT boards_provider_check CHECK ((provider = ANY (ARRAY['local'::text, 'jira'::text, 'github'::text, 'linear'::text, 'trello'::text])))
);

--
-- Name: bus_outbox; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.bus_outbox (
    id bigint NOT NULL,
    payload text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

--
-- Name: bus_outbox_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.bus_outbox_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

--
-- Name: bus_outbox_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.bus_outbox_id_seq OWNED BY public.bus_outbox.id;

--
-- Name: events; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.events (
    id uuid NOT NULL,
    tenant_id uuid NOT NULL,
    occurred_at timestamp with time zone DEFAULT now() NOT NULL,
    kind text NOT NULL,
    actor_type text,
    actor_id uuid,
    workspace_id uuid,
    node_id uuid,
    session_id uuid,
    payload jsonb DEFAULT '{}'::jsonb NOT NULL
);

--
-- Name: feedback; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.feedback (
    id uuid NOT NULL,
    tenant_id uuid NOT NULL,
    workspace_id uuid,
    session_id uuid,
    body text NOT NULL,
    status text DEFAULT 'queued'::text NOT NULL,
    pr_url text,
    created_by uuid,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT feedback_status_check CHECK ((status = ANY (ARRAY['queued'::text, 'delivered'::text, 'submitted'::text, 'dropped'::text])))
);

--
-- Name: git_credentials; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.git_credentials (
    id uuid NOT NULL,
    tenant_id uuid NOT NULL,
    name text NOT NULL,
    kind text DEFAULT 'ssh_key'::text NOT NULL,
    public_key text DEFAULT ''::text NOT NULL,
    secret_enc bytea NOT NULL,
    created_by uuid,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT git_credentials_kind_check CHECK ((kind = 'ssh_key'::text))
);

--
-- Name: identities; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.identities (
    id uuid NOT NULL,
    user_id uuid NOT NULL,
    issuer text NOT NULL,
    subject text NOT NULL,
    email text,
    raw_claims jsonb DEFAULT '{}'::jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

--
-- Name: join_tokens; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.join_tokens (
    id uuid NOT NULL,
    tenant_id uuid NOT NULL,
    token_hash text NOT NULL,
    name text DEFAULT ''::text NOT NULL,
    created_by uuid,
    expires_at timestamp with time zone NOT NULL,
    used_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

--
-- Name: labels; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.labels (
    id uuid NOT NULL,
    tenant_id uuid NOT NULL,
    name text NOT NULL,
    color text DEFAULT '#f0a000'::text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

--
-- Name: node_workspaces; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.node_workspaces (
    id uuid NOT NULL,
    tenant_id uuid NOT NULL,
    node_id uuid NOT NULL,
    workspace_id uuid NOT NULL,
    path text NOT NULL,
    git_remote_url text,
    git_remote_normalized text,
    git_branch text,
    git_status jsonb DEFAULT '{}'::jsonb NOT NULL,
    discovered_at timestamp with time zone DEFAULT now() NOT NULL,
    last_scanned_at timestamp with time zone DEFAULT now() NOT NULL
);

--
-- Name: nodes; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.nodes (
    id uuid NOT NULL,
    tenant_id uuid NOT NULL,
    name text NOT NULL,
    hostname text DEFAULT ''::text NOT NULL,
    platform text DEFAULT ''::text NOT NULL,
    capabilities jsonb DEFAULT '{}'::jsonb NOT NULL,
    resources jsonb DEFAULT '{}'::jsonb NOT NULL,
    status text DEFAULT 'offline'::text NOT NULL,
    node_token_hash text NOT NULL,
    last_seen_at timestamp with time zone,
    owning_instance_id uuid,
    lease_expires_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    ca_id uuid,
    cert_not_after timestamp with time zone,
    revoked_at timestamp with time zone,
    public_key_pem text,
    cert_pem text,
    CONSTRAINT nodes_status_check CHECK ((status = ANY (ARRAY['online'::text, 'offline'::text])))
);

--
-- Name: notes; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.notes (
    id uuid NOT NULL,
    tenant_id uuid NOT NULL,
    workspace_id uuid NOT NULL,
    title text DEFAULT ''::text NOT NULL,
    content_md text DEFAULT ''::text NOT NULL,
    kind text DEFAULT 'rolling'::text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);

--
-- Name: notification_channels; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.notification_channels (
    id uuid NOT NULL,
    tenant_id uuid NOT NULL,
    kind text NOT NULL,
    name text NOT NULL,
    config jsonb DEFAULT '{}'::jsonb NOT NULL,
    enabled boolean DEFAULT true NOT NULL,
    levels text[] DEFAULT '{}'::text[] NOT NULL,
    kinds text[] DEFAULT '{}'::text[] NOT NULL,
    last_ok_at timestamp with time zone,
    last_error text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    secret text
);

--
-- Name: notifications; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.notifications (
    id uuid NOT NULL,
    tenant_id uuid NOT NULL,
    user_id uuid,
    level text DEFAULT 'info'::text NOT NULL,
    title text NOT NULL,
    body text DEFAULT ''::text NOT NULL,
    kind text DEFAULT 'custom'::text NOT NULL,
    link text,
    payload jsonb DEFAULT '{}'::jsonb NOT NULL,
    read_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

--
-- Name: org_visibility_policy; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.org_visibility_policy (
    id uuid NOT NULL,
    org_id uuid NOT NULL,
    field text NOT NULL,
    enabled boolean NOT NULL,
    changed_by uuid,
    changed_at timestamp with time zone DEFAULT now() NOT NULL
);

--
-- Name: orgs; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.orgs (
    id uuid NOT NULL,
    name text NOT NULL,
    slug text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);

--
-- Name: permissions; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.permissions (
    key text NOT NULL,
    description text DEFAULT ''::text NOT NULL
);

--
-- Name: role_bindings; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.role_bindings (
    id uuid NOT NULL,
    subject_type text DEFAULT 'user'::text NOT NULL,
    subject_id uuid NOT NULL,
    role_key text NOT NULL,
    scope_type text NOT NULL,
    scope_id uuid,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    created_by uuid,
    CONSTRAINT role_bindings_scope_id_check CHECK ((((scope_type = 'deployment'::text) AND (scope_id IS NULL)) OR ((scope_type <> 'deployment'::text) AND (scope_id IS NOT NULL)))),
    CONSTRAINT role_bindings_scope_type_check CHECK ((scope_type = ANY (ARRAY['deployment'::text, 'org'::text, 'tenant'::text])))
);

--
-- Name: role_permissions; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.role_permissions (
    role_key text NOT NULL,
    permission_key text NOT NULL
);

--
-- Name: roles; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.roles (
    key text NOT NULL,
    name text NOT NULL,
    description text DEFAULT ''::text NOT NULL,
    builtin boolean DEFAULT false NOT NULL
);

--
-- Name: sessions; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.sessions (
    id uuid NOT NULL,
    tenant_id uuid NOT NULL,
    workspace_id uuid,
    node_id uuid NOT NULL,
    name text DEFAULT ''::text NOT NULL,
    runtime text NOT NULL,
    tmux_session text,
    status text DEFAULT 'starting'::text NOT NULL,
    error text,
    created_by uuid,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    ended_at timestamp with time zone,
    CONSTRAINT sessions_status_check CHECK ((status = ANY (ARRAY['starting'::text, 'running'::text, 'detached'::text, 'exited'::text, 'error'::text])))
);

--
-- Name: sessions_auth; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.sessions_auth (
    id uuid NOT NULL,
    user_id uuid NOT NULL,
    tenant_id uuid NOT NULL,
    expires_at timestamp with time zone NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

--
-- Name: settings; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.settings (
    id uuid NOT NULL,
    tenant_id uuid NOT NULL,
    scope text NOT NULL,
    user_id uuid,
    key text NOT NULL,
    value jsonb NOT NULL,
    CONSTRAINT settings_scope_check CHECK ((scope = ANY (ARRAY['tenant'::text, 'user'::text])))
);

--
-- Name: skills; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.skills (
    id uuid NOT NULL,
    tenant_id uuid NOT NULL,
    name text NOT NULL,
    content text NOT NULL,
    sha256 text NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_by uuid
);

--
-- Name: task_comments; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.task_comments (
    id uuid NOT NULL,
    tenant_id uuid NOT NULL,
    task_id uuid NOT NULL,
    author_type text NOT NULL,
    author_id uuid,
    author_name text DEFAULT ''::text NOT NULL,
    body_md text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT task_comments_author_type_check CHECK ((author_type = ANY (ARRAY['user'::text, 'agent'::text, 'system'::text])))
);

--
-- Name: task_labels; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.task_labels (
    task_id uuid NOT NULL,
    label_id uuid NOT NULL
);

--
-- Name: task_relations; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.task_relations (
    id uuid NOT NULL,
    tenant_id uuid NOT NULL,
    from_task uuid NOT NULL,
    to_task uuid NOT NULL,
    kind text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT task_relations_check CHECK ((from_task <> to_task)),
    CONSTRAINT task_relations_kind_check CHECK ((kind = ANY (ARRAY['blocks'::text, 'relates'::text, 'duplicates'::text])))
);

--
-- Name: tasks; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.tasks (
    id uuid NOT NULL,
    tenant_id uuid NOT NULL,
    board_id uuid NOT NULL,
    column_id uuid NOT NULL,
    title text NOT NULL,
    description text,
    "position" integer DEFAULT 0 NOT NULL,
    external_id text,
    external_url text,
    assignee_user_id uuid,
    workspace_id uuid,
    assigned_node_id uuid,
    branch text,
    worktree_path text,
    worktree_node_id uuid,
    session_id uuid,
    pr_url text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    number integer,
    priority integer DEFAULT 0 NOT NULL,
    CONSTRAINT tasks_priority_check CHECK (((priority >= 0) AND (priority <= 4)))
);

--
-- Name: tenant_cas; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.tenant_cas (
    id uuid NOT NULL,
    tenant_id uuid NOT NULL,
    state text DEFAULT 'staged'::text NOT NULL,
    cert_pem text NOT NULL,
    key_enc bytea NOT NULL,
    fingerprint text NOT NULL,
    not_after timestamp with time zone NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    retired_at timestamp with time zone,
    CONSTRAINT tenant_cas_state_check CHECK ((state = ANY (ARRAY['staged'::text, 'active'::text, 'retiring'::text])))
);

--
-- Name: tenant_members; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.tenant_members (
    id uuid NOT NULL,
    tenant_id uuid NOT NULL,
    principal_type text DEFAULT 'user'::text NOT NULL,
    principal_id uuid NOT NULL,
    role text DEFAULT 'member'::text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT tenant_members_principal_type_check CHECK ((principal_type = ANY (ARRAY['user'::text, 'group'::text, 'service'::text]))),
    CONSTRAINT tenant_members_role_check CHECK ((role = ANY (ARRAY['owner'::text, 'admin'::text, 'member'::text])))
);

--
-- Name: tenants; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.tenants (
    id uuid NOT NULL,
    name text NOT NULL,
    slug text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    auth_mode text,
    org_id uuid DEFAULT '00000000-0000-0000-0000-0000000000a1'::uuid NOT NULL,
    CONSTRAINT tenants_auth_mode_check CHECK (((auth_mode IS NULL) OR (auth_mode = ANY (ARRAY['oidc'::text, 'local'::text]))))
);

--
-- Name: themes; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.themes (
    id uuid NOT NULL,
    tenant_id uuid,
    name text NOT NULL,
    slug text NOT NULL,
    tokens jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

--
-- Name: user_passkeys; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.user_passkeys (
    id uuid NOT NULL,
    user_id uuid NOT NULL,
    tenant_id uuid NOT NULL,
    credential_id text NOT NULL,
    label text DEFAULT ''::text NOT NULL,
    wrapped_secret bytea NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    last_used_at timestamp with time zone
);

--
-- Name: user_tokens; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.user_tokens (
    id uuid NOT NULL,
    tenant_id uuid NOT NULL,
    user_id uuid NOT NULL,
    token_hash text NOT NULL,
    name text DEFAULT ''::text NOT NULL,
    last_used_at timestamp with time zone,
    expires_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

--
-- Name: user_vaults; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.user_vaults (
    user_id uuid NOT NULL,
    tenant_id uuid NOT NULL,
    kdf_salt bytea NOT NULL,
    verifier bytea NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

--
-- Name: users; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.users (
    id uuid NOT NULL,
    tenant_id uuid NOT NULL,
    display_name text NOT NULL,
    email text NOT NULL,
    avatar_url text,
    role text DEFAULT 'member'::text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    username text,
    password_hash text,
    CONSTRAINT users_role_check CHECK ((role = ANY (ARRAY['owner'::text, 'admin'::text, 'member'::text])))
);

--
-- Name: workspace_secrets; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.workspace_secrets (
    id uuid NOT NULL,
    tenant_id uuid NOT NULL,
    workspace_id uuid NOT NULL,
    name text DEFAULT '.env'::text NOT NULL,
    content_enc bytea NOT NULL,
    kdf_salt bytea,
    verifier bytea,
    ephemeral boolean DEFAULT false NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);

--
-- Name: workspaces; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.workspaces (
    id uuid NOT NULL,
    tenant_id uuid NOT NULL,
    name text NOT NULL,
    slug text NOT NULL,
    description text,
    git_remote_normalized text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);

--
-- Name: bus_outbox id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.bus_outbox ALTER COLUMN id SET DEFAULT nextval('public.bus_outbox_id_seq'::regclass);

--
-- Name: board_columns board_columns_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.board_columns
    ADD CONSTRAINT board_columns_pkey PRIMARY KEY (id);

--
-- Name: boards boards_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.boards
    ADD CONSTRAINT boards_pkey PRIMARY KEY (id);

--
-- Name: bus_outbox bus_outbox_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.bus_outbox
    ADD CONSTRAINT bus_outbox_pkey PRIMARY KEY (id);

--
-- Name: events events_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.events
    ADD CONSTRAINT events_pkey PRIMARY KEY (id);

--
-- Name: feedback feedback_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.feedback
    ADD CONSTRAINT feedback_pkey PRIMARY KEY (id);

--
-- Name: git_credentials git_credentials_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.git_credentials
    ADD CONSTRAINT git_credentials_pkey PRIMARY KEY (id);

--
-- Name: git_credentials git_credentials_tenant_id_name_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.git_credentials
    ADD CONSTRAINT git_credentials_tenant_id_name_key UNIQUE (tenant_id, name);

--
-- Name: identities identities_issuer_subject_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.identities
    ADD CONSTRAINT identities_issuer_subject_key UNIQUE (issuer, subject);

--
-- Name: identities identities_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.identities
    ADD CONSTRAINT identities_pkey PRIMARY KEY (id);

--
-- Name: join_tokens join_tokens_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.join_tokens
    ADD CONSTRAINT join_tokens_pkey PRIMARY KEY (id);

--
-- Name: join_tokens join_tokens_token_hash_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.join_tokens
    ADD CONSTRAINT join_tokens_token_hash_key UNIQUE (token_hash);

--
-- Name: labels labels_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.labels
    ADD CONSTRAINT labels_pkey PRIMARY KEY (id);

--
-- Name: labels labels_tenant_id_name_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.labels
    ADD CONSTRAINT labels_tenant_id_name_key UNIQUE (tenant_id, name);

--
-- Name: node_workspaces node_workspaces_node_id_path_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.node_workspaces
    ADD CONSTRAINT node_workspaces_node_id_path_key UNIQUE (node_id, path);

--
-- Name: node_workspaces node_workspaces_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.node_workspaces
    ADD CONSTRAINT node_workspaces_pkey PRIMARY KEY (id);

--
-- Name: nodes nodes_node_token_hash_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.nodes
    ADD CONSTRAINT nodes_node_token_hash_key UNIQUE (node_token_hash);

--
-- Name: nodes nodes_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.nodes
    ADD CONSTRAINT nodes_pkey PRIMARY KEY (id);

--
-- Name: nodes nodes_tenant_id_name_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.nodes
    ADD CONSTRAINT nodes_tenant_id_name_key UNIQUE (tenant_id, name);

--
-- Name: notes notes_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.notes
    ADD CONSTRAINT notes_pkey PRIMARY KEY (id);

--
-- Name: notification_channels notification_channels_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.notification_channels
    ADD CONSTRAINT notification_channels_pkey PRIMARY KEY (id);

--
-- Name: notification_channels notification_channels_tenant_id_name_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.notification_channels
    ADD CONSTRAINT notification_channels_tenant_id_name_key UNIQUE (tenant_id, name);

--
-- Name: notifications notifications_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.notifications
    ADD CONSTRAINT notifications_pkey PRIMARY KEY (id);

--
-- Name: org_visibility_policy org_visibility_policy_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.org_visibility_policy
    ADD CONSTRAINT org_visibility_policy_pkey PRIMARY KEY (id);

--
-- Name: orgs orgs_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.orgs
    ADD CONSTRAINT orgs_pkey PRIMARY KEY (id);

--
-- Name: orgs orgs_slug_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.orgs
    ADD CONSTRAINT orgs_slug_key UNIQUE (slug);

--
-- Name: permissions permissions_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.permissions
    ADD CONSTRAINT permissions_pkey PRIMARY KEY (key);

--
-- Name: role_bindings role_bindings_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.role_bindings
    ADD CONSTRAINT role_bindings_pkey PRIMARY KEY (id);

--
-- Name: role_permissions role_permissions_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.role_permissions
    ADD CONSTRAINT role_permissions_pkey PRIMARY KEY (role_key, permission_key);

--
-- Name: roles roles_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.roles
    ADD CONSTRAINT roles_pkey PRIMARY KEY (key);

--
-- Name: sessions_auth sessions_auth_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.sessions_auth
    ADD CONSTRAINT sessions_auth_pkey PRIMARY KEY (id);

--
-- Name: sessions sessions_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.sessions
    ADD CONSTRAINT sessions_pkey PRIMARY KEY (id);

--
-- Name: settings settings_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.settings
    ADD CONSTRAINT settings_pkey PRIMARY KEY (id);

--
-- Name: settings settings_tenant_id_scope_user_id_key_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.settings
    ADD CONSTRAINT settings_tenant_id_scope_user_id_key_key UNIQUE NULLS NOT DISTINCT (tenant_id, scope, user_id, key);

--
-- Name: skills skills_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.skills
    ADD CONSTRAINT skills_pkey PRIMARY KEY (id);

--
-- Name: task_comments task_comments_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.task_comments
    ADD CONSTRAINT task_comments_pkey PRIMARY KEY (id);

--
-- Name: task_labels task_labels_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.task_labels
    ADD CONSTRAINT task_labels_pkey PRIMARY KEY (task_id, label_id);

--
-- Name: task_relations task_relations_from_task_to_task_kind_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.task_relations
    ADD CONSTRAINT task_relations_from_task_to_task_kind_key UNIQUE (from_task, to_task, kind);

--
-- Name: task_relations task_relations_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.task_relations
    ADD CONSTRAINT task_relations_pkey PRIMARY KEY (id);

--
-- Name: tasks tasks_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tasks
    ADD CONSTRAINT tasks_pkey PRIMARY KEY (id);

--
-- Name: tenant_cas tenant_cas_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tenant_cas
    ADD CONSTRAINT tenant_cas_pkey PRIMARY KEY (id);

--
-- Name: tenant_members tenant_members_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tenant_members
    ADD CONSTRAINT tenant_members_pkey PRIMARY KEY (id);

--
-- Name: tenant_members tenant_members_tenant_id_principal_type_principal_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tenant_members
    ADD CONSTRAINT tenant_members_tenant_id_principal_type_principal_id_key UNIQUE (tenant_id, principal_type, principal_id);

--
-- Name: tenants tenants_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tenants
    ADD CONSTRAINT tenants_pkey PRIMARY KEY (id);

--
-- Name: tenants tenants_slug_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tenants
    ADD CONSTRAINT tenants_slug_key UNIQUE (slug);

--
-- Name: themes themes_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.themes
    ADD CONSTRAINT themes_pkey PRIMARY KEY (id);

--
-- Name: themes themes_slug_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.themes
    ADD CONSTRAINT themes_slug_key UNIQUE (slug);

--
-- Name: user_passkeys user_passkeys_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.user_passkeys
    ADD CONSTRAINT user_passkeys_pkey PRIMARY KEY (id);

--
-- Name: user_passkeys user_passkeys_user_id_credential_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.user_passkeys
    ADD CONSTRAINT user_passkeys_user_id_credential_id_key UNIQUE (user_id, credential_id);

--
-- Name: user_tokens user_tokens_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.user_tokens
    ADD CONSTRAINT user_tokens_pkey PRIMARY KEY (id);

--
-- Name: user_tokens user_tokens_token_hash_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.user_tokens
    ADD CONSTRAINT user_tokens_token_hash_key UNIQUE (token_hash);

--
-- Name: user_vaults user_vaults_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.user_vaults
    ADD CONSTRAINT user_vaults_pkey PRIMARY KEY (user_id);

--
-- Name: users users_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_pkey PRIMARY KEY (id);

--
-- Name: users users_tenant_id_email_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_tenant_id_email_key UNIQUE (tenant_id, email);

--
-- Name: workspace_secrets workspace_secrets_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.workspace_secrets
    ADD CONSTRAINT workspace_secrets_pkey PRIMARY KEY (id);

--
-- Name: workspace_secrets workspace_secrets_workspace_id_name_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.workspace_secrets
    ADD CONSTRAINT workspace_secrets_workspace_id_name_key UNIQUE (workspace_id, name);

--
-- Name: workspaces workspaces_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.workspaces
    ADD CONSTRAINT workspaces_pkey PRIMARY KEY (id);

--
-- Name: workspaces workspaces_tenant_id_slug_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.workspaces
    ADD CONSTRAINT workspaces_tenant_id_slug_key UNIQUE (tenant_id, slug);

--
-- Name: feedback_tenant_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX feedback_tenant_idx ON public.feedback USING btree (tenant_id, created_at DESC);

--
-- Name: idx_board_columns_type; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_board_columns_type ON public.board_columns USING btree (board_id, type, "position");

--
-- Name: idx_boards_tenant_key; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX idx_boards_tenant_key ON public.boards USING btree (tenant_id, key) WHERE (key IS NOT NULL);

--
-- Name: idx_events_tenant_time; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_events_tenant_time ON public.events USING btree (tenant_id, occurred_at DESC);

--
-- Name: idx_events_workspace_time; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_events_workspace_time ON public.events USING btree (tenant_id, workspace_id, occurred_at DESC);

--
-- Name: idx_node_workspaces_workspace; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_node_workspaces_workspace ON public.node_workspaces USING btree (workspace_id);

--
-- Name: idx_nodes_ca; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_nodes_ca ON public.nodes USING btree (ca_id);

--
-- Name: idx_notes_workspace; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_notes_workspace ON public.notes USING btree (tenant_id, workspace_id);

--
-- Name: idx_notification_channels_tenant; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_notification_channels_tenant ON public.notification_channels USING btree (tenant_id) WHERE enabled;

--
-- Name: idx_notifications_inbox; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_notifications_inbox ON public.notifications USING btree (tenant_id, created_at DESC);

--
-- Name: idx_notifications_unread; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_notifications_unread ON public.notifications USING btree (tenant_id) WHERE (read_at IS NULL);

--
-- Name: idx_org_visibility_current; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_org_visibility_current ON public.org_visibility_policy USING btree (org_id, field, changed_at DESC);

--
-- Name: idx_role_bindings_subject; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_role_bindings_subject ON public.role_bindings USING btree (subject_type, subject_id);

--
-- Name: idx_role_bindings_unique; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX idx_role_bindings_unique ON public.role_bindings USING btree (subject_type, subject_id, role_key, scope_type, COALESCE(scope_id, '00000000-0000-0000-0000-000000000000'::uuid));

--
-- Name: idx_sessions_auth_expiry; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_sessions_auth_expiry ON public.sessions_auth USING btree (expires_at);

--
-- Name: idx_sessions_node; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_sessions_node ON public.sessions USING btree (node_id);

--
-- Name: idx_sessions_workspace; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_sessions_workspace ON public.sessions USING btree (tenant_id, workspace_id);

--
-- Name: idx_task_comments_task; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_task_comments_task ON public.task_comments USING btree (task_id, created_at);

--
-- Name: idx_task_labels_label; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_task_labels_label ON public.task_labels USING btree (label_id);

--
-- Name: idx_task_relations_from; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_task_relations_from ON public.task_relations USING btree (from_task, kind);

--
-- Name: idx_task_relations_to; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_task_relations_to ON public.task_relations USING btree (to_task, kind);

--
-- Name: idx_tasks_board; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_tasks_board ON public.tasks USING btree (board_id, column_id, "position");

--
-- Name: idx_tasks_board_number; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX idx_tasks_board_number ON public.tasks USING btree (board_id, number) WHERE (number IS NOT NULL);

--
-- Name: idx_tasks_pick; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_tasks_pick ON public.tasks USING btree (board_id, priority, created_at);

--
-- Name: idx_tenant_cas_tenant; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_tenant_cas_tenant ON public.tenant_cas USING btree (tenant_id);

--
-- Name: idx_tenant_members_principal; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_tenant_members_principal ON public.tenant_members USING btree (principal_type, principal_id);

--
-- Name: idx_tenants_org; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_tenants_org ON public.tenants USING btree (org_id);

--
-- Name: idx_user_passkeys_user; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_user_passkeys_user ON public.user_passkeys USING btree (user_id);

--
-- Name: idx_user_tokens_user; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_user_tokens_user ON public.user_tokens USING btree (user_id);

--
-- Name: nodes_lease_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX nodes_lease_idx ON public.nodes USING btree (owning_instance_id) WHERE (owning_instance_id IS NOT NULL);

--
-- Name: skills_tenant_name_key; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX skills_tenant_name_key ON public.skills USING btree (tenant_id, name);

--
-- Name: tenant_cas_one_active; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX tenant_cas_one_active ON public.tenant_cas USING btree (tenant_id) WHERE (state = 'active'::text);

--
-- Name: users_tenant_username_unique; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX users_tenant_username_unique ON public.users USING btree (tenant_id, lower(username)) WHERE (username IS NOT NULL);

--
-- Name: workspaces_remote_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX workspaces_remote_idx ON public.workspaces USING btree (tenant_id, git_remote_normalized) WHERE (git_remote_normalized IS NOT NULL);

--
-- Name: board_columns board_columns_board_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.board_columns
    ADD CONSTRAINT board_columns_board_id_fkey FOREIGN KEY (board_id) REFERENCES public.boards(id) ON DELETE CASCADE;

--
-- Name: boards boards_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.boards
    ADD CONSTRAINT boards_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id) ON DELETE CASCADE;

--
-- Name: boards boards_workspace_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.boards
    ADD CONSTRAINT boards_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES public.workspaces(id) ON DELETE SET NULL;

--
-- Name: events events_node_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.events
    ADD CONSTRAINT events_node_id_fkey FOREIGN KEY (node_id) REFERENCES public.nodes(id) ON DELETE SET NULL;

--
-- Name: events events_session_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.events
    ADD CONSTRAINT events_session_id_fkey FOREIGN KEY (session_id) REFERENCES public.sessions(id) ON DELETE SET NULL;

--
-- Name: events events_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.events
    ADD CONSTRAINT events_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id) ON DELETE CASCADE;

--
-- Name: events events_workspace_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.events
    ADD CONSTRAINT events_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES public.workspaces(id) ON DELETE SET NULL;

--
-- Name: feedback feedback_created_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.feedback
    ADD CONSTRAINT feedback_created_by_fkey FOREIGN KEY (created_by) REFERENCES public.users(id) ON DELETE SET NULL;

--
-- Name: feedback feedback_session_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.feedback
    ADD CONSTRAINT feedback_session_id_fkey FOREIGN KEY (session_id) REFERENCES public.sessions(id) ON DELETE SET NULL;

--
-- Name: feedback feedback_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.feedback
    ADD CONSTRAINT feedback_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id) ON DELETE CASCADE;

--
-- Name: feedback feedback_workspace_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.feedback
    ADD CONSTRAINT feedback_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES public.workspaces(id) ON DELETE SET NULL;

--
-- Name: git_credentials git_credentials_created_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.git_credentials
    ADD CONSTRAINT git_credentials_created_by_fkey FOREIGN KEY (created_by) REFERENCES public.users(id) ON DELETE SET NULL;

--
-- Name: git_credentials git_credentials_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.git_credentials
    ADD CONSTRAINT git_credentials_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id) ON DELETE CASCADE;

--
-- Name: identities identities_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.identities
    ADD CONSTRAINT identities_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;

--
-- Name: join_tokens join_tokens_created_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.join_tokens
    ADD CONSTRAINT join_tokens_created_by_fkey FOREIGN KEY (created_by) REFERENCES public.users(id) ON DELETE SET NULL;

--
-- Name: join_tokens join_tokens_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.join_tokens
    ADD CONSTRAINT join_tokens_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id) ON DELETE CASCADE;

--
-- Name: labels labels_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.labels
    ADD CONSTRAINT labels_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id) ON DELETE CASCADE;

--
-- Name: node_workspaces node_workspaces_node_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.node_workspaces
    ADD CONSTRAINT node_workspaces_node_id_fkey FOREIGN KEY (node_id) REFERENCES public.nodes(id) ON DELETE CASCADE;

--
-- Name: node_workspaces node_workspaces_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.node_workspaces
    ADD CONSTRAINT node_workspaces_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id) ON DELETE CASCADE;

--
-- Name: node_workspaces node_workspaces_workspace_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.node_workspaces
    ADD CONSTRAINT node_workspaces_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES public.workspaces(id) ON DELETE CASCADE;

--
-- Name: nodes nodes_ca_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.nodes
    ADD CONSTRAINT nodes_ca_id_fkey FOREIGN KEY (ca_id) REFERENCES public.tenant_cas(id);

--
-- Name: nodes nodes_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.nodes
    ADD CONSTRAINT nodes_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id) ON DELETE CASCADE;

--
-- Name: notes notes_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.notes
    ADD CONSTRAINT notes_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id) ON DELETE CASCADE;

--
-- Name: notes notes_workspace_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.notes
    ADD CONSTRAINT notes_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES public.workspaces(id) ON DELETE CASCADE;

--
-- Name: notification_channels notification_channels_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.notification_channels
    ADD CONSTRAINT notification_channels_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id) ON DELETE CASCADE;

--
-- Name: notifications notifications_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.notifications
    ADD CONSTRAINT notifications_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id) ON DELETE CASCADE;

--
-- Name: notifications notifications_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.notifications
    ADD CONSTRAINT notifications_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;

--
-- Name: org_visibility_policy org_visibility_policy_changed_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.org_visibility_policy
    ADD CONSTRAINT org_visibility_policy_changed_by_fkey FOREIGN KEY (changed_by) REFERENCES public.users(id) ON DELETE SET NULL;

--
-- Name: org_visibility_policy org_visibility_policy_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.org_visibility_policy
    ADD CONSTRAINT org_visibility_policy_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.orgs(id) ON DELETE CASCADE;

--
-- Name: role_bindings role_bindings_created_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.role_bindings
    ADD CONSTRAINT role_bindings_created_by_fkey FOREIGN KEY (created_by) REFERENCES public.users(id) ON DELETE SET NULL;

--
-- Name: role_bindings role_bindings_role_key_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.role_bindings
    ADD CONSTRAINT role_bindings_role_key_fkey FOREIGN KEY (role_key) REFERENCES public.roles(key) ON DELETE CASCADE;

--
-- Name: role_permissions role_permissions_permission_key_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.role_permissions
    ADD CONSTRAINT role_permissions_permission_key_fkey FOREIGN KEY (permission_key) REFERENCES public.permissions(key) ON DELETE CASCADE;

--
-- Name: role_permissions role_permissions_role_key_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.role_permissions
    ADD CONSTRAINT role_permissions_role_key_fkey FOREIGN KEY (role_key) REFERENCES public.roles(key) ON DELETE CASCADE;

--
-- Name: sessions_auth sessions_auth_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.sessions_auth
    ADD CONSTRAINT sessions_auth_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id) ON DELETE CASCADE;

--
-- Name: sessions_auth sessions_auth_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.sessions_auth
    ADD CONSTRAINT sessions_auth_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;

--
-- Name: sessions sessions_created_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.sessions
    ADD CONSTRAINT sessions_created_by_fkey FOREIGN KEY (created_by) REFERENCES public.users(id) ON DELETE SET NULL;

--
-- Name: sessions sessions_node_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.sessions
    ADD CONSTRAINT sessions_node_id_fkey FOREIGN KEY (node_id) REFERENCES public.nodes(id) ON DELETE CASCADE;

--
-- Name: sessions sessions_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.sessions
    ADD CONSTRAINT sessions_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id) ON DELETE CASCADE;

--
-- Name: sessions sessions_workspace_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.sessions
    ADD CONSTRAINT sessions_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES public.workspaces(id) ON DELETE CASCADE;

--
-- Name: settings settings_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.settings
    ADD CONSTRAINT settings_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id) ON DELETE CASCADE;

--
-- Name: settings settings_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.settings
    ADD CONSTRAINT settings_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;

--
-- Name: skills skills_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.skills
    ADD CONSTRAINT skills_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id) ON DELETE CASCADE;

--
-- Name: skills skills_updated_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.skills
    ADD CONSTRAINT skills_updated_by_fkey FOREIGN KEY (updated_by) REFERENCES public.users(id) ON DELETE SET NULL;

--
-- Name: task_comments task_comments_task_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.task_comments
    ADD CONSTRAINT task_comments_task_id_fkey FOREIGN KEY (task_id) REFERENCES public.tasks(id) ON DELETE CASCADE;

--
-- Name: task_comments task_comments_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.task_comments
    ADD CONSTRAINT task_comments_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id) ON DELETE CASCADE;

--
-- Name: task_labels task_labels_label_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.task_labels
    ADD CONSTRAINT task_labels_label_id_fkey FOREIGN KEY (label_id) REFERENCES public.labels(id) ON DELETE CASCADE;

--
-- Name: task_labels task_labels_task_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.task_labels
    ADD CONSTRAINT task_labels_task_id_fkey FOREIGN KEY (task_id) REFERENCES public.tasks(id) ON DELETE CASCADE;

--
-- Name: task_relations task_relations_from_task_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.task_relations
    ADD CONSTRAINT task_relations_from_task_fkey FOREIGN KEY (from_task) REFERENCES public.tasks(id) ON DELETE CASCADE;

--
-- Name: task_relations task_relations_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.task_relations
    ADD CONSTRAINT task_relations_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id) ON DELETE CASCADE;

--
-- Name: task_relations task_relations_to_task_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.task_relations
    ADD CONSTRAINT task_relations_to_task_fkey FOREIGN KEY (to_task) REFERENCES public.tasks(id) ON DELETE CASCADE;

--
-- Name: tasks tasks_assigned_node_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tasks
    ADD CONSTRAINT tasks_assigned_node_id_fkey FOREIGN KEY (assigned_node_id) REFERENCES public.nodes(id) ON DELETE SET NULL;

--
-- Name: tasks tasks_assignee_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tasks
    ADD CONSTRAINT tasks_assignee_user_id_fkey FOREIGN KEY (assignee_user_id) REFERENCES public.users(id) ON DELETE SET NULL;

--
-- Name: tasks tasks_board_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tasks
    ADD CONSTRAINT tasks_board_id_fkey FOREIGN KEY (board_id) REFERENCES public.boards(id) ON DELETE CASCADE;

--
-- Name: tasks tasks_column_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tasks
    ADD CONSTRAINT tasks_column_id_fkey FOREIGN KEY (column_id) REFERENCES public.board_columns(id) ON DELETE CASCADE;

--
-- Name: tasks tasks_session_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tasks
    ADD CONSTRAINT tasks_session_id_fkey FOREIGN KEY (session_id) REFERENCES public.sessions(id) ON DELETE SET NULL;

--
-- Name: tasks tasks_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tasks
    ADD CONSTRAINT tasks_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id) ON DELETE CASCADE;

--
-- Name: tasks tasks_workspace_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tasks
    ADD CONSTRAINT tasks_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES public.workspaces(id) ON DELETE SET NULL;

--
-- Name: tasks tasks_worktree_node_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tasks
    ADD CONSTRAINT tasks_worktree_node_id_fkey FOREIGN KEY (worktree_node_id) REFERENCES public.nodes(id) ON DELETE SET NULL;

--
-- Name: tenant_cas tenant_cas_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tenant_cas
    ADD CONSTRAINT tenant_cas_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id) ON DELETE CASCADE;

--
-- Name: tenant_members tenant_members_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tenant_members
    ADD CONSTRAINT tenant_members_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id) ON DELETE CASCADE;

--
-- Name: tenants tenants_org_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tenants
    ADD CONSTRAINT tenants_org_id_fkey FOREIGN KEY (org_id) REFERENCES public.orgs(id);

--
-- Name: themes themes_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.themes
    ADD CONSTRAINT themes_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id) ON DELETE CASCADE;

--
-- Name: user_passkeys user_passkeys_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.user_passkeys
    ADD CONSTRAINT user_passkeys_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id) ON DELETE CASCADE;

--
-- Name: user_passkeys user_passkeys_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.user_passkeys
    ADD CONSTRAINT user_passkeys_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;

--
-- Name: user_tokens user_tokens_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.user_tokens
    ADD CONSTRAINT user_tokens_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id) ON DELETE CASCADE;

--
-- Name: user_tokens user_tokens_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.user_tokens
    ADD CONSTRAINT user_tokens_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;

--
-- Name: user_vaults user_vaults_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.user_vaults
    ADD CONSTRAINT user_vaults_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id) ON DELETE CASCADE;

--
-- Name: user_vaults user_vaults_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.user_vaults
    ADD CONSTRAINT user_vaults_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;

--
-- Name: users users_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id) ON DELETE CASCADE;

--
-- Name: workspace_secrets workspace_secrets_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.workspace_secrets
    ADD CONSTRAINT workspace_secrets_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id) ON DELETE CASCADE;

--
-- Name: workspace_secrets workspace_secrets_workspace_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.workspace_secrets
    ADD CONSTRAINT workspace_secrets_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES public.workspaces(id) ON DELETE CASCADE;

--
-- Name: workspaces workspaces_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.workspaces
    ADD CONSTRAINT workspaces_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id) ON DELETE CASCADE;

--
--

-- ── Seed data ────────────────────────────────────────────────────────────────
--
-- Reference rows the schema is meaningless without: the permission catalog, the
-- built-in roles, and what each role grants. Carried verbatim from the
-- migrations that introduced them.

-- Everything that exists today belongs to one org. Created with a fixed uuid so
-- re-running is a no-op rather than a second default org.
INSERT INTO orgs (id, name, slug)
VALUES ('00000000-0000-0000-0000-0000000000a1', 'Default', 'default')
ON CONFLICT (id) DO NOTHING;

INSERT INTO permissions (key, description) VALUES
    ('org.view',        'See that an org and its tenants exist'),
    ('org.manage',      'Rename an org, move tenants between orgs'),
    ('tenant.view',     'See a tenant exists, and its membership counts'),
    ('tenant.manage',   'Administer a tenant: members, settings'),
    ('node.view',       'See nodes: name, status, resources, session counts'),
    ('node.manage',     'Revoke or remove a node'),
    ('audit.view',      'Read audit records'),
    ('ca.rotate',       'Rotate a tenant certificate authority'),
    ('policy.view',     'Read an org visibility policy'),
    ('policy.manage',   'Change an org visibility policy')
ON CONFLICT (key) DO NOTHING;

INSERT INTO roles (key, name, description, builtin) VALUES
    ('operator',     'Operator',     'Runs this deployment or org. Sees metadata, never session content.', TRUE),
    ('org_admin',    'Org admin',    'Administers an org and the tenants under it.', TRUE),
    ('tenant_admin', 'Tenant admin', 'Administers one tenant.', TRUE),
    ('member',       'Member',       'Ordinary access to a tenant.', TRUE)
ON CONFLICT (key) DO NOTHING;

INSERT INTO role_permissions (role_key, permission_key) VALUES
    -- The operator sees the shape of the deployment and can act on the
    -- infrastructure it runs. It cannot administer somebody's tenant, and it
    -- has no route to their session content because none exists.
    ('operator', 'org.view'),
    ('operator', 'tenant.view'),
    ('operator', 'node.view'),
    ('operator', 'node.manage'),
    ('operator', 'audit.view'),
    ('operator', 'ca.rotate'),
    ('operator', 'policy.view'),
    ('operator', 'policy.manage'),

    ('org_admin', 'org.view'),
    ('org_admin', 'org.manage'),
    ('org_admin', 'tenant.view'),
    ('org_admin', 'node.view'),
    ('org_admin', 'audit.view'),
    ('org_admin', 'policy.view'),
    ('org_admin', 'policy.manage'),

    -- A tenant admin runs their own tenant and nothing above it. Deliberately
    -- WITHOUT ca.rotate: the CA is the deployment's trust root, and a tenant
    -- admin rotating it is a tenant reaching upward.
    ('tenant_admin', 'tenant.view'),
    ('tenant_admin', 'tenant.manage'),
    ('tenant_admin', 'node.view'),
    ('tenant_admin', 'node.manage'),
    ('tenant_admin', 'audit.view'),
    ('tenant_admin', 'policy.view'),

    ('member', 'tenant.view'),
    ('member', 'node.view')
ON CONFLICT DO NOTHING;

-- Existing tenant owners and admins keep what they had, as bindings.
INSERT INTO role_bindings (id, subject_type, subject_id, role_key, scope_type, scope_id)
SELECT gen_random_uuid(), 'user', u.id, 'tenant_admin', 'tenant', u.tenant_id
FROM users u
WHERE u.role IN ('owner', 'admin')
ON CONFLICT DO NOTHING;

-- Appointing a role is its own authority.
--
-- Granting was gated on `org.manage`, which `operator` does not hold — so the
-- bootstrap operator, the one person a fresh deployment has, could not appoint
-- anybody. Widening `org.manage` to fix it would have conflated two different
-- powers: managing orgs (renaming them, moving tenants between them) and
-- deciding who may run the deployment.
--
-- Separate, so "who can appoint operators" is one row somebody can grep for
-- rather than a consequence of a permission granted for another reason.
INSERT INTO permissions (key, description) VALUES
    ('rbac.grant', 'Grant or revoke a role binding')
ON CONFLICT (key) DO NOTHING;

-- Never `tenant_admin`: a tenant administering itself must not be able to
-- appoint someone above it.
INSERT INTO role_permissions (role_key, permission_key) VALUES
    ('operator',  'rbac.grant'),
    ('org_admin', 'rbac.grant')
ON CONFLICT DO NOTHING;

-- Give `operator` the `org.manage` permission.
--
-- 0015 gave it only to `org_admin`, reasoning that running a deployment and
-- administering an org are different jobs. They are — but the bootstrap grant
-- makes `operator` the ONLY role a fresh deployment has, and nobody holds
-- `org_admin` until somebody appoints them. So orgs could never be created:
-- the layer existed and was unreachable.
--
-- This is the same shape as the `rbac.grant` fix in 0018 and the reason a test
-- now asserts the class: every permission an `/operator/*` route requires must
-- be held by `operator`, or the surface has a route its own role cannot call.
--
-- Orgs are deployment STRUCTURE, not tenant content. Holding this grants no
-- sight of anybody's work — that is visibility policy — and no reach into
-- session content, which is not a permission at all.
INSERT INTO role_permissions (role_key, permission_key) VALUES
    ('operator', 'org.manage')
ON CONFLICT DO NOTHING;