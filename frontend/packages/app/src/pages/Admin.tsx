// The admin surface — the tenant's table views and the operator's fleet views,
// one sectioned page.
//
// This absorbed the Workspaces and Activity rail pages: they are management
// tables, and three rail entries (Workspaces, Activity, Operator) for what is
// one kind of surface was rail clutter. Day-to-day navigation never needed the
// tables — the top-bar workspace switcher and the dashboard cover it — so the
// tables live here, as sections, findable by the same nav and finder as
// everything else.
//
// TWO AUDIENCES, ONE PAGE. The Team group is tenant data any member may see;
// the Fleet and Access groups exist only for someone holding an operator
// binding — their sections are not rendered without it and their queries are
// not even sent (`enabled:` gates below), so a member who lands here from an
// old /activity bookmark produces no 403 noise.
//
// The operator half still reads `/api/v1/operator/*` and nothing else: session
// content is unreachable from this page because no endpoint under that prefix
// serves it, and every request 403s server-side regardless of what the UI
// renders.
import React from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
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
import { PagedPanel, Panel, Select, type DataColumn } from "@nookos/ui";
import { askConfirm, askForm, askText, notify } from "../dialogs";
import { usePagedList } from "../paging";
import { SectionedPage, type PageSection } from "../SectionedPage";
import { TenantSwitches } from "../TenantSwitches";
import { ActivityPanel } from "./Activity";

// Columns for the audit DataList. Module-level: the cells read only the row, so
// they never close over component state and the array is stable across renders.
const AUDIT_COLUMNS: DataColumn<OperatorAuditEntry>[] = [
  {
    key: "when",
    header: "When",
    className: "faint small",
    sortKey: "time",
    cell: (e) => new Date(e.occurred_at).toLocaleString(),
  },
  { key: "what", header: "What", className: "mono", cell: (e) => e.kind, sortKey: "kind" },
  {
    key: "tenant",
    header: "Tenant",
    className: "mono faint",
    sortKey: "tenant",
    cell: (e) => e.tenant_slug,
  },
  {
    key: "actor",
    header: "Actor",
    className: "faint small",
    cell: (e) => e.actor_type ?? "—",
  },
];

export function AdminPage() {
  const qc = useQueryClient();

  const { data: me } = useQuery({
    queryKey: ["me"],
    queryFn: async () => (await api.GET("/api/v1/auth/me")).data ?? null,
  });
  const isOperator = !!me?.capability?.operator;
  // The four lists all speak the pagination contract through one hook —
  // search, sort and cursor walking come with it (MAIN-44, QOL sweep).
  const tenants = usePagedList({
    key: ["operator", "tenants"],
    fetch: async (params) =>
      (await api.GET("/api/v1/operator/tenants", { params: { query: params } })).data,
    enabled: isOperator,
  });
  const nodes = usePagedList({
    key: ["operator", "nodes"],
    fetch: async (params) =>
      (await api.GET("/api/v1/operator/nodes", { params: { query: params } })).data,
    enabled: isOperator,
  });
  const bindings = usePagedList({
    key: ["operator", "bindings"],
    fetch: async (params) =>
      (await api.GET("/api/v1/operator/bindings", { params: { query: params } })).data,
    enabled: isOperator,
  });

  const { data: orgs } = useQuery({
    queryKey: ["operator", "orgs"],
    queryFn: async () => (await api.GET("/api/v1/operator/orgs")).data ?? [],
    enabled: isOperator,
  });
  const audit = usePagedList({
    key: ["operator", "audit"],
    fetch: async (params) =>
      (await api.GET("/api/v1/operator/audit", { params: { query: params } })).data,
    enabled: isOperator,
  });
  const orgId = me?.capability?.org_id ?? null;
  const { data: policy } = useQuery({
    queryKey: ["operator", "policy", orgId],
    queryFn: async () =>
      (
        await api.GET("/api/v1/operator/orgs/{id}/policy", {
          params: { path: { id: orgId! } },
        })
      ).data ?? [],
    enabled: !!orgId && isOperator,
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

  // Column defs live inside the component: their action cells close over the
  // handlers (moveTenant/stageCa/revoke…) and over `orgs`, so — unlike the
  // static AUDIT_COLUMNS — they cannot be module-level constants.
  const tenantColumns: DataColumn<OperatorTenant>[] = [

    { key: "tenant", header: "Tenant", className: "mono bright", cell: (t) => t.slug, sortKey: "slug" },
    { key: "members", header: "Members", cell: (t) => t.members, sortKey: "members" },
    { key: "nodes", header: "Nodes", cell: (t) => t.nodes, sortKey: "nodes" },
    { key: "sessions", header: "Active sessions", cell: (t) => t.active_sessions, sortKey: "sessions" },
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
      key: "automation",
      header: "Automation",
      cell: (t) => <TenantSwitches tenantId={t.id} slug={t.slug} />,
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
    { key: "node", header: "Node", className: "bright", cell: (n) => n.name, sortKey: "name" },
    { key: "tenant", header: "Tenant", className: "mono faint", cell: (n) => n.tenant_slug },
    { key: "platform", header: "Platform", className: "faint", cell: (n) => n.platform, sortKey: "platform" },
    {
      key: "status",
      header: "Status",
      sortKey: "status",
      cell: (n) => <span className={n.status === "online" ? "ok" : "faint"}>{n.status}</span>,
    },
    { key: "sessions", header: "Sessions", cell: (n) => n.active_sessions, sortKey: "sessions" },
    {
      key: "seen",
      header: "Last seen",
      className: "faint small",
      sortKey: "last_seen",
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
    { key: "who", header: "Who", className: "bright", cell: (b) => b.email, sortKey: "email" },
    { key: "role", header: "Role", className: "mono", cell: (b) => b.role_key, sortKey: "role" },
    { key: "scope", header: "Scope", className: "faint", cell: (b) => b.scope_type, sortKey: "scope" },
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

  const intro = !isOperator ? undefined : (
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
    </Panel>
  );

  // The registry replaces one long scroll of stacked tables — and three rail
  // entries. Team first: it is what every visitor may see; the operator groups
  // exist only when the binding does.
  const sections: PageSection[] = [
    {
      id: "activity",
      title: "Activity",
      group: "Team",
      keywords: ["events", "timeline", "feed", "log", "history", "who did what"],
      render: () => <ActivityPanel />,
    },
    ...(!isOperator
      ? []
      : operatorSections()),
  ];

  function operatorSections(): PageSection[] {
    return [
    {
      id: "tenants",
      title: "Tenants",
      group: "Fleet",
      keywords: ["team", "ca", "certificate", "org", "automation", "loops", "reconcile", "switches", "members"],
      render: () => (
        <PagedPanel
          title="Tenants"
          list={tenants}
          columns={tenantColumns}
          rowKey={(t) => t.id}
          searchPlaceholder="Search slug or name…"
          searchLabel="Search tenants"
          empty="No tenants."
        />
      ),
    },
    {
      id: "nodes",
      title: "Nodes",
      group: "Fleet",
      keywords: ["machine", "revoke", "certificate", "platform", "online", "remove", "last seen"],
      render: () => (
        <PagedPanel
          title="Nodes"
          list={nodes}
          columns={nodeColumns}
          rowKey={(n) => n.id}
          searchPlaceholder="Search name, status, platform…"
          searchLabel="Search nodes"
          empty="No nodes."
        />
      ),
    },
    {
      id: "roles",
      title: "Roles",
      group: "Access",
      keywords: ["binding", "grant", "revoke", "rbac", "permission", "operator", "admin", "appoint"],
      render: () => (
        <PagedPanel
          title="Roles"
          list={bindings}
          columns={bindingColumns}
          rowKey={(b) => b.id}
          searchPlaceholder="Search email, role, scope…"
          searchLabel="Search roles"
          empty="No role bindings."
          actions={
            <button className="btn small" onClick={grantRole}>
              <UserPlus size={12} /> grant
            </button>
          }
        />
      ),
    },
    {
      id: "visibility",
      title: "Visibility policy",
      group: "Access",
      keywords: ["privacy", "policy", "see", "hidden", "widen", "fields"],
      render: () => (
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
      ),
    },
    {
      id: "orgs",
      title: "Orgs",
      group: "Fleet",
      keywords: ["organization", "rename", "create", "slug"],
      render: () => (
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
      ),
    },
    {
      id: "audit",
      title: "Audit",
      group: "Access",
      keywords: ["log", "history", "who looked", "trail", "record"],
      render: () => (
        <PagedPanel
          title="Audit · including who looked"
          list={audit}
          columns={AUDIT_COLUMNS}
          rowKey={(e) => e.id}
          searchPlaceholder="Search kind, tenant, actor…"
          searchLabel="Search the audit log"
        />
      ),
    },
    ];
  }

  return <SectionedPage sections={sections} placeholder="find…" intro={intro} />;
}
