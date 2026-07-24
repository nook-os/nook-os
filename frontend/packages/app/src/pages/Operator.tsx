// The operator surface.
//
// Everything here reads `/api/v1/operator/*` and nothing else. That is the
// frontend half of the rule the backend enforces: session content is not
// reachable from this page because there is no endpoint under that prefix that
// serves it, and a component here that fetched `/sessions/{id}` would be as
// visible in review as the route would be.
//
// The page exists only for someone holding an operator binding — the rail entry
// is hidden otherwise, and every request 403s regardless.
import React from "react";
import { useInfiniteQuery, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Ban,
  Eye,
  EyeOff,
  KeyRound,
  Pencil,
  Plus,
  ShieldCheck,
  Trash2,
  TriangleAlert,
  UserPlus,
} from "lucide-react";
import {
  api,
  type BindingRow,
  type OperatorAuditEntry,
  type OperatorNode,
  type OperatorTenant,
} from "@nookos/api";
import {
  DataList,
  Panel,
  SearchInput,
  Select,
  type DataColumn,
} from "@nookos/ui";
import { askConfirm, askForm, askText, notify } from "../dialogs";

// Columns for the audit DataList. Module-level: the cells read only the row, so
// they never close over component state and the array is stable across renders.
const AUDIT_COLUMNS: DataColumn<OperatorAuditEntry>[] = [
  {
    key: "when",
    header: "When",
    className: "faint small",
    cell: (e) => new Date(e.occurred_at).toLocaleString(),
  },
  { key: "what", header: "What", className: "mono", cell: (e) => e.kind },
  {
    key: "tenant",
    header: "Tenant",
    className: "mono faint",
    cell: (e) => e.tenant_slug,
  },
  {
    key: "actor",
    header: "Actor",
    className: "faint small",
    cell: (e) => e.actor_type ?? "—",
  },
];

export function OperatorPage() {
  const qc = useQueryClient();

  const { data: me } = useQuery({
    queryKey: ["me"],
    queryFn: async () => (await api.GET("/api/v1/auth/me")).data ?? null,
  });
  // Tenants, nodes and bindings each paginate by keyset cursor and search
  // server-side (MAIN-44), through the same shared DataList as the audit log.
  const [tenantSearch, setTenantSearch] = React.useState("");
  const tenantsQuery = useInfiniteQuery({
    queryKey: ["operator", "tenants", tenantSearch],
    initialPageParam: undefined as string | undefined,
    queryFn: async ({ pageParam }) =>
      (
        await api.GET("/api/v1/operator/tenants", {
          params: { query: { q: tenantSearch || undefined, after: pageParam || undefined, limit: 50 } },
        })
      ).data ?? { rows: [], next_cursor: null },
    getNextPageParam: (last) => last.next_cursor ?? undefined,
  });
  const tenants = tenantsQuery.data?.pages.flatMap((p) => p.rows) ?? [];

  const [nodeSearch, setNodeSearch] = React.useState("");
  const nodesQuery = useInfiniteQuery({
    queryKey: ["operator", "nodes", nodeSearch],
    initialPageParam: undefined as string | undefined,
    queryFn: async ({ pageParam }) =>
      (
        await api.GET("/api/v1/operator/nodes", {
          params: { query: { q: nodeSearch || undefined, after: pageParam || undefined, limit: 50 } },
        })
      ).data ?? { rows: [], next_cursor: null },
    getNextPageParam: (last) => last.next_cursor ?? undefined,
  });
  const nodes = nodesQuery.data?.pages.flatMap((p) => p.rows) ?? [];

  const [bindingSearch, setBindingSearch] = React.useState("");
  const bindingsQuery = useInfiniteQuery({
    queryKey: ["operator", "bindings", bindingSearch],
    initialPageParam: undefined as string | undefined,
    queryFn: async ({ pageParam }) =>
      (
        await api.GET("/api/v1/operator/bindings", {
          params: { query: { q: bindingSearch || undefined, after: pageParam || undefined, limit: 50 } },
        })
      ).data ?? { rows: [], next_cursor: null },
    getNextPageParam: (last) => last.next_cursor ?? undefined,
  });
  const bindings = bindingsQuery.data?.pages.flatMap((p) => p.rows) ?? [];

  const { data: orgs } = useQuery({
    queryKey: ["operator", "orgs"],
    queryFn: async () => (await api.GET("/api/v1/operator/orgs")).data ?? [],
  });
  // The audit log paginates by keyset cursor and searches server-side (MAIN-43),
  // so it holds many pages of accumulated rows rather than a single fetch. A new
  // search string is a new query key, which restarts from the newest page.
  const [auditSearch, setAuditSearch] = React.useState("");
  const auditQuery = useInfiniteQuery({
    queryKey: ["operator", "audit", auditSearch],
    initialPageParam: undefined as string | undefined,
    queryFn: async ({ pageParam }) =>
      (
        await api.GET("/api/v1/operator/audit", {
          params: {
            query: {
              q: auditSearch || undefined,
              after: pageParam || undefined,
              limit: 50,
            },
          },
        })
      ).data ?? { rows: [], next_cursor: null },
    getNextPageParam: (last) => last.next_cursor ?? undefined,
  });
  const auditRows = auditQuery.data?.pages.flatMap((p) => p.rows) ?? [];
  const orgId = me?.capability?.org_id ?? null;
  const { data: policy } = useQuery({
    queryKey: ["operator", "policy", orgId],
    queryFn: async () =>
      (
        await api.GET("/api/v1/operator/orgs/{id}/policy", {
          params: { path: { id: orgId! } },
        })
      ).data ?? [],
    enabled: !!orgId,
  });

  const bust = () => qc.invalidateQueries({ queryKey: ["operator"] });

  /// Report the server's own message. "403" tells somebody nothing; "this needs
  /// the `ca.rotate` permission" tells them exactly what to go and get.
  const run = async (label: string, fn: () => Promise<{ error?: unknown }>) => {
    const { error } = await fn();
    if (error) {
      await notify(
        `${label} failed`,
        typeof error === "object" && error && "error" in error
          ? String((error as { error: unknown }).error)
          : JSON.stringify(error),
      );
      return false;
    }
    bust();
    return true;
  };

  const stageCa = async (tenantId: string, slug: string) => {
    const ok = await askConfirm({
      title: `Stage a new CA for ${slug}`,
      description:
        "A new certificate authority is created and distributed. Machines pick " +
        "it up on their next renewal. It does NOT start signing yet — promoting " +
        "it before machines have renewed would strand every node that has not.",
      confirmLabel: "stage",
    });
    if (!ok) return;
    await run("Staging the CA", () =>
      api.POST("/api/v1/operator/tenants/{id}/ca", {
        params: { path: { id: tenantId } },
      }),
    );
  };

  const revokeNode = async (id: string, name: string) => {
    const ok = await askConfirm({
      title: `Revoke ${name}`,
      description:
        "Its certificate stops being accepted and the machine drops off the " +
        "fleet. Sessions running on it keep running in tmux, but nothing can " +
        "reach them until it enrols again.",
      confirmLabel: "revoke",
      danger: true,
    });
    if (!ok) return;
    await run("Revoking", () =>
      api.POST("/api/v1/operator/nodes/{id}/revoke", { params: { path: { id } } }),
    );
  };

  const removeNode = async (id: string, name: string) => {
    const ok = await askConfirm({
      title: `Remove ${name}`,
      description:
        "The node record is deleted. This does not touch the work on that " +
        "machine — checkouts and tmux sessions stay where they are.",
      confirmLabel: "remove",
      danger: true,
    });
    if (!ok) return;
    await run("Removing", () =>
      api.DELETE("/api/v1/operator/nodes/{id}", { params: { path: { id } } }),
    );
  };

  const createOrg = async () => {
    const name = await askText({
      title: "New org",
      label: "Name",
      placeholder: "Acme",
      confirmLabel: "create",
    });
    if (!name?.trim()) return;
    await run("Creating the org", () =>
      api.POST("/api/v1/operator/orgs", { body: { name: name.trim() } }),
    );
  };

  const renameOrg = async (id: string, current: string) => {
    const name = await askText({
      title: `Rename ${current}`,
      label: "Name",
      value: current,
      confirmLabel: "rename",
    });
    // Same guard as create: no empty name, and a no-op rename sends nothing.
    // Only the NAME changes — the slug stays as the stable identifier (AC-3).
    if (!name?.trim() || name.trim() === current) return;
    await run("Renaming the org", () =>
      api.PATCH("/api/v1/operator/orgs/{id}", {
        params: { path: { id } },
        body: { name: name.trim() },
      }),
    );
  };

  const moveTenant = async (tenantId: string, orgIdNext: string) => {
    await run("Moving the tenant", () =>
      api.POST("/api/v1/operator/tenants/{id}/org", {
        params: { path: { id: tenantId } },
        body: { org_id: orgIdNext },
      }),
    );
  };

  const grantRole = async () => {
    const out = await askForm({
      title: "Grant a deployment role",
      description:
        "Deployment-scoped roles cover every org and every tenant. `operator` " +
        "runs the infrastructure and can appoint others; it still cannot read " +
        "session content.",
      fields: [
        { name: "email", label: "Email", required: true, placeholder: "someone@example.com" },
        { name: "role", label: "Role", value: "operator" },
      ],
      confirmLabel: "grant",
    });
    if (!out?.email?.trim()) return;
    await run("Granting", () =>
      api.POST("/api/v1/operator/bindings", {
        body: { email: out.email.trim(), role: out.role?.trim() || "operator", revoke: false },
      }),
    );
  };

  const revokeRole = async (email: string, role: string) => {
    const ok = await askConfirm({
      title: `Revoke ${role} from ${email}`,
      description: "They lose whatever that role granted, immediately.",
      confirmLabel: "revoke",
      danger: true,
    });
    if (!ok) return;
    await run("Revoking", () =>
      api.POST("/api/v1/operator/bindings", { body: { email, role, revoke: true } }),
    );
  };

  const toggle = async (field: string, enabled: boolean, description: string) => {
    // Widening is announced to everyone it affects, so it is confirmed here
    // rather than being one stray click.
    if (enabled) {
      const ok = await askConfirm({
        title: "Widen what operators can see?",
        description:
          `Operators of this organization will be able to see: ${description}. ` +
          "Everyone in the organization is notified, and the change is recorded " +
          "with your name and the time.",
        confirmLabel: "widen visibility",
        danger: true,
      });
      if (!ok) return;
    }
    await api.POST("/api/v1/operator/orgs/{id}/policy", {
      params: { path: { id: orgId! } },
      body: { field, enabled },
    });
    qc.invalidateQueries({ queryKey: ["operator"] });
  };

  // Not holding the binding is a legitimate state, not an error — but empty
  // tables read as "this deployment has nothing in it", which is a different
  // and wrong claim. Say which it is.
  if (me && !me.capability?.operator) {
    return (
      <div className="nook-grid" style={{ gridTemplateColumns: "1fr" }}>
        <Panel title="Operator">
          <div className="op-intro">
            <ShieldCheck size={14} />
            <div>
              <div className="bright">
                You do not hold an operator role on this deployment.
              </div>
              <div className="muted small">
                Signed in as <span className="mono">{me.user.email}</span>. This
                page shows what the person running this deployment can see —
                tenants, nodes and audit, never session content. Grant yourself
                the role with:
                <div className="op-code mono">
                  nook operator grant {me.user.email}
                </div>
              </div>
            </div>
          </div>
        </Panel>
      </div>
    );
  }

  // Column defs live inside the component: their action cells close over the
  // handlers (moveTenant/stageCa/revoke…) and over `orgs`, so — unlike the
  // static AUDIT_COLUMNS — they cannot be module-level constants.
  const tenantColumns: DataColumn<OperatorTenant>[] = [
    { key: "tenant", header: "Tenant", className: "mono bright", cell: (t) => t.slug },
    { key: "members", header: "Members", cell: (t) => t.members },
    { key: "nodes", header: "Nodes", cell: (t) => t.nodes },
    { key: "sessions", header: "Active sessions", cell: (t) => t.active_sessions },
    { key: "workspaces", header: "Workspaces", cell: (t) => t.workspaces },
    {
      key: "org",
      header: "Org",
      cell: (t) => (
        <Select
          value={t.org_id ?? ""}
          onChange={(v) => moveTenant(t.id, v)}
          options={(orgs ?? []).map((o) => ({ value: o.id, label: o.slug }))}
          ariaLabel="org"
        />
      ),
    },
    {
      key: "actions",
      header: "",
      cell: (t) => (
        <button
          className="btn small"
          onClick={() => stageCa(t.id, t.slug)}
          title="stage a new certificate authority"
        >
          <KeyRound size={11} /> stage CA
        </button>
      ),
    },
  ];

  const nodeColumns: DataColumn<OperatorNode>[] = [
    { key: "node", header: "Node", className: "bright", cell: (n) => n.name },
    { key: "tenant", header: "Tenant", className: "mono faint", cell: (n) => n.tenant_slug },
    { key: "platform", header: "Platform", className: "faint", cell: (n) => n.platform },
    {
      key: "status",
      header: "Status",
      cell: (n) => <span className={n.status === "online" ? "ok" : "faint"}>{n.status}</span>,
    },
    { key: "sessions", header: "Sessions", cell: (n) => n.active_sessions },
    {
      key: "seen",
      header: "Last seen",
      className: "faint small",
      cell: (n) => (n.last_seen_at ? new Date(n.last_seen_at).toLocaleString() : "—"),
    },
    {
      key: "actions",
      header: "",
      cell: (n) => (
        <span className="op-row-actions">
          <button className="btn small" onClick={() => revokeNode(n.id, n.name)} title="revoke its certificate">
            <Ban size={11} />
          </button>
          <button className="btn danger small" onClick={() => removeNode(n.id, n.name)} title="remove the node">
            <Trash2 size={11} />
          </button>
        </span>
      ),
    },
  ];

  const bindingColumns: DataColumn<BindingRow>[] = [
    { key: "who", header: "Who", className: "bright", cell: (b) => b.email },
    { key: "role", header: "Role", className: "mono", cell: (b) => b.role_key },
    { key: "scope", header: "Scope", className: "faint", cell: (b) => b.scope_type },
    { key: "where", header: "Where", className: "mono faint", cell: (b) => b.scope_label ?? "—" },
    {
      key: "actions",
      header: "",
      cell: (b) =>
        b.scope_type === "deployment" ? (
          <button className="btn danger small" onClick={() => revokeRole(b.email, b.role_key)} title="revoke">
            <Trash2 size={11} />
          </button>
        ) : null,
    },
  ];

  return (
    <div className="nook-grid" style={{ gridTemplateColumns: "1fr" }}>
      <Panel title="Operator · what this deployment is doing">
        <div className="op-intro">
          <ShieldCheck size={14} />
          <div>
            <div className="bright">You can see metadata, never content.</div>
            <div className="muted small">
              Terminals, prompts and code belong to the tenant that owns them.
              That is not a setting on this page — there is no permission for it,
              and every session route refuses an operator by construction.
            </div>
          </div>
        </div>

        <div className="op-section-head">
          <span className="op-section-h">Tenants</span>
          <SearchInput
            onSearch={setTenantSearch}
            placeholder="Search slug or name…"
            ariaLabel="Search tenants"
          />
        </div>
        <DataList
          columns={tenantColumns}
          rows={tenants}
          rowKey={(t) => t.id}
          loading={tenantsQuery.isLoading}
          filtered={tenantSearch.length > 0}
          empty="No tenants."
          noResults="No matches."
          hasMore={tenantsQuery.hasNextPage}
          onLoadMore={() => tenantsQuery.fetchNextPage()}
          loadingMore={tenantsQuery.isFetchingNextPage}
        />

        <div className="op-section-head">
          <span className="op-section-h">Nodes</span>
          <SearchInput
            onSearch={setNodeSearch}
            placeholder="Search name, status, platform…"
            ariaLabel="Search nodes"
          />
        </div>
        <DataList
          columns={nodeColumns}
          rows={nodes}
          rowKey={(n) => n.id}
          loading={nodesQuery.isLoading}
          filtered={nodeSearch.length > 0}
          empty="No nodes."
          noResults="No matches."
          hasMore={nodesQuery.hasNextPage}
          onLoadMore={() => nodesQuery.fetchNextPage()}
          loadingMore={nodesQuery.isFetchingNextPage}
        />
      </Panel>

      <Panel title="Visibility policy">
        <div className="op-policy">
          <p className="muted small">
            What operators may see of a tenant's work. Everything is off until
            somebody turns it on, every change is recorded with a timestamp, and
            everyone in the organization is told when it changes. None of these
            can reach terminal content.
          </p>
          {(policy ?? []).map((f) => (
            <div key={f.field} className="op-policy-row">
              <button
                className={`task-chip ${f.enabled ? "on" : ""}`}
                onClick={() => toggle(f.field, !f.enabled, f.description)}
                title={f.enabled ? "visible — click to hide" : "hidden — click to reveal"}
              >
                {f.enabled ? <Eye size={11} /> : <EyeOff size={11} />}
                {f.enabled ? "visible" : "hidden"}
              </button>
              <span className={f.enabled ? "bright" : "faint"}>{f.description}</span>
            </div>
          ))}
          {(policy ?? []).some((f) => f.enabled) && (
            <div className="op-warn">
              <TriangleAlert size={12} /> Some fields are visible to operators.
              Everyone in this organization can see which, in their own settings.
            </div>
          )}
        </div>
      </Panel>

      <Panel
        title="Roles"
        actions={
          <span className="op-panel-actions">
            <SearchInput
              onSearch={setBindingSearch}
              placeholder="Search email, role, scope…"
              ariaLabel="Search roles"
            />
            <button className="btn small" onClick={grantRole}>
              <UserPlus size={12} /> grant
            </button>
          </span>
        }
      >
        <p className="muted small op-note">
          A binding grants at its scope and everything under it — `deployment`
          covers every org and tenant. None of them reach session content.
        </p>
        <DataList
          columns={bindingColumns}
          rows={bindings}
          rowKey={(b) => b.id}
          loading={bindingsQuery.isLoading}
          filtered={bindingSearch.length > 0}
          empty="No role bindings."
          noResults="No matches."
          hasMore={bindingsQuery.hasNextPage}
          onLoadMore={() => bindingsQuery.fetchNextPage()}
          loadingMore={bindingsQuery.isFetchingNextPage}
        />
      </Panel>

      <Panel
        title="Orgs"
        actions={
          <button className="btn small" onClick={createOrg}>
            <Plus size={12} /> org
          </button>
        }
      >
        <div className="op-table-wrap">
          <table className="op-table">
            <thead>
              <tr>
                <th>Name</th>
                <th>Slug</th>
                <th>Tenants</th>
                <th></th>
              </tr>
            </thead>
            <tbody>
              {(orgs ?? []).map((o) => (
                <tr key={o.id}>
                  <td className="bright">{o.name}</td>
                  <td className="mono faint">{o.slug}</td>
                  <td>{o.tenants}</td>
                  <td style={{ textAlign: "right" }}>
                    <button
                      className="btn small icon"
                      title={`rename ${o.name}`}
                      onClick={() => renameOrg(o.id, o.name)}
                    >
                      <Pencil size={12} />
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </Panel>

      <Panel
        title="Audit · including who looked"
        actions={
          <SearchInput
            onSearch={setAuditSearch}
            placeholder="Search kind, tenant, actor…"
            ariaLabel="Search the audit log"
          />
        }
      >
        <DataList
          columns={AUDIT_COLUMNS}
          rows={auditRows}
          rowKey={(e) => e.id}
          loading={auditQuery.isLoading}
          filtered={auditSearch.length > 0}
          empty="Nothing here yet."
          noResults="No matches."
          hasMore={auditQuery.hasNextPage}
          onLoadMore={() => auditQuery.fetchNextPage()}
          loadingMore={auditQuery.isFetchingNextPage}
        />
      </Panel>
    </div>
  );
}
