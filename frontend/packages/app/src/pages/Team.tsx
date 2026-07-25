// Team management (MAIN-64): the first-class home for who is in your tenant and
// who you have invited. It replaces the Members and Invites panels that used to
// live in Settings, so this is the single source of truth for both.
//
// Everything here targets the ACTIVE tenant. Switching tenant (the "Your
// organizations" section) re-scopes the whole app and, because we do not
// navigate away, re-renders this page against the newly active tenant. Whether
// the management controls show is decided by the active membership's role
// (`me.tenants` — the `tenant_members` source), not `me.user.role`.
import React from "react";
import {
  useInfiniteQuery,
  useMutation,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import { Boxes, Check, FolderGit2, Loader, Mail, Plus, Send, Trash2 } from "lucide-react";
import {
  api,
  type Invite,
  type Notification,
  type TenantMemberItem,
  type TenantMembership,
} from "@nookos/api";
import {
  DataList,
  Empty,
  Panel,
  Pill,
  Select,
  SearchInput,
  type DataColumn,
} from "@nookos/ui";
import { askConfirm, CopyRow, notify } from "../dialogs";
import { useToasts } from "../Notifications";
import { useWorkspaceContext } from "../context";

/** Push an ephemeral success toast through the existing toast store. Failures
 *  are NOT pushed here — the global write-failure toast already reports them. */
function toast(title: string): void {
  useToasts.getState().push({
    id: `team-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
    tenant_id: "",
    level: "success",
    title,
    body: "",
    kind: "custom",
    payload: null,
    created_at: new Date().toISOString(),
  } as Notification);
}

/** openapi-fetch returns errors rather than throwing; a mutation needs a throw
 *  so `onError` fires and `onSuccess` is skipped. The global write-failure
 *  handler still reports the failure from the client middleware. */
function orThrow<T>(r: { data?: T; error?: unknown; response: Response }): T {
  if (r.error || !r.response.ok || !r.data) {
    throw new Error(typeof r.error === "string" ? r.error : `${r.response.status}`);
  }
  return r.data;
}

function useMe() {
  return useQuery({
    queryKey: ["me"],
    queryFn: async () => (await api.GET("/api/v1/auth/me")).data ?? null,
  });
}

export function TeamPage() {
  return (
    <div className="nook-grid" style={{ gridTemplateColumns: "1fr 1fr" }}>
      <Panel title="Members">
        <MembersRoster />
      </Panel>
      <YourOrganizations />
      <TeamInvites />
    </div>
  );
}

/** Who is in the active tenant, with role controls and remove/leave. Moved
 *  verbatim from Settings (MAIN-45): management shows only to an owner/admin;
 *  a plain member sees the roster read-only and just a Leave button. */
function MembersRoster() {
  const queryClient = useQueryClient();
  const { data: me } = useMe();
  const tenantId = me?.tenant?.id;
  const myRole = (me?.tenants ?? []).find((t) => t.current)?.role ?? "member";
  const myId = me?.user?.id;
  const canManage = myRole === "owner" || myRole === "admin";

  const [search, setSearch] = React.useState("");
  const membersQuery = useInfiniteQuery({
    queryKey: ["tenant-members", tenantId, search],
    initialPageParam: undefined as string | undefined,
    queryFn: async ({ pageParam }) =>
      tenantId
        ? (
            await api.GET("/api/v1/tenants/{id}/members", {
              params: {
                path: { id: tenantId },
                query: { q: search || undefined, after: pageParam || undefined, limit: 50 },
              },
            })
          ).data ?? { rows: [], next_cursor: null }
        : { rows: [], next_cursor: null },
    getNextPageParam: (last) => last.next_cursor ?? undefined,
    enabled: !!tenantId,
  });
  const members = membersQuery.data?.pages.flatMap((p) => p.rows) ?? [];

  const bust = () =>
    queryClient.invalidateQueries({ queryKey: ["tenant-members", tenantId] });
  const fail = async (title: string, error: unknown) =>
    notify(
      title,
      typeof error === "object" && error && "error" in error
        ? String((error as { error: unknown }).error)
        : JSON.stringify(error ?? {}),
    );

  const changeRole = async (pid: string, role: string) => {
    if (!tenantId) return;
    const { error, response } = await api.PATCH("/api/v1/tenants/{id}/members/{pid}", {
      params: { path: { id: tenantId, pid } },
      body: { role },
    });
    if (error || !response.ok) return void (await fail("Could not change role", error));
    bust();
  };
  const remove = async (pid: string, name: string) => {
    if (!tenantId) return;
    const ok = await askConfirm({
      title: `Remove ${name}?`,
      description: "They lose access to this tenant immediately. Their work stays with the tenant.",
      confirmLabel: "remove",
      danger: true,
    });
    if (!ok) return;
    const { error, response } = await api.DELETE("/api/v1/tenants/{id}/members/{pid}", {
      params: { path: { id: tenantId, pid } },
    });
    if (error || !response.ok) return void (await fail("Could not remove", error));
    bust();
  };
  const leave = async () => {
    if (!tenantId) return;
    const ok = await askConfirm({
      title: "Leave this tenant?",
      description: "You lose access to it. You keep your personal tenant.",
      confirmLabel: "leave",
      danger: true,
    });
    if (!ok) return;
    const { error, response } = await api.POST("/api/v1/tenants/{id}/leave", {
      params: { path: { id: tenantId } },
    });
    if (error || !response.ok) return void (await fail("Could not leave", error));
    queryClient.invalidateQueries();
  };

  const columns: DataColumn<TenantMemberItem>[] = [
    {
      key: "name",
      header: "Name",
      className: "bright",
      cell: (m) => (
        <>
          {m.display_name}
          {m.principal_id === myId && <span className="faint small"> (you)</span>}
        </>
      ),
    },
    { key: "email", header: "Email", className: "mono muted", cell: (m) => m.email },
    {
      key: "role",
      header: "Role",
      cell: (m) =>
        canManage && m.principal_id !== myId ? (
          <select
            className="task-select"
            value={m.role}
            onChange={(e) => changeRole(m.principal_id, e.target.value)}
          >
            {myRole === "owner" && <option value="owner">owner</option>}
            <option value="admin">admin</option>
            <option value="member">member</option>
          </select>
        ) : (
          <Pill tone={m.role === "owner" ? "ok" : undefined}>{m.role}</Pill>
        ),
    },
    {
      key: "actions",
      header: "",
      cell: (m) =>
        m.principal_id === myId ? (
          <button className="btn danger small" onClick={leave}>
            Leave
          </button>
        ) : canManage ? (
          <button
            className="btn danger small"
            onClick={() => remove(m.principal_id, m.display_name)}
          >
            Remove
          </button>
        ) : null,
    },
  ];

  return (
    <>
      <div style={{ padding: "0 0 8px" }}>
        <SearchInput
          onSearch={setSearch}
          placeholder="Search members…"
          ariaLabel="Search members"
        />
      </div>
      <DataList
        columns={columns}
        rows={members}
        rowKey={(m) => m.principal_id}
        loading={membersQuery.isLoading}
        filtered={search.length > 0}
        empty="No members."
        noResults="No matches."
        hasMore={membersQuery.hasNextPage}
        onLoadMore={() => membersQuery.fetchNextPage()}
        loadingMore={membersQuery.isFetchingNextPage}
      />
    </>
  );
}

/** Pending invites for the active tenant. Owners/admins only; a plain member
 *  does not see this section at all (they cannot list or create). */
function TeamInvites() {
  const qc = useQueryClient();
  const { data: me } = useMe();
  const tenantId = me?.tenant.id ?? null;
  // The active membership's role, not the account role — an admin in this tenant
  // manages here even if they are only a member elsewhere.
  const canManage = ["owner", "admin"].includes(
    (me?.tenants ?? []).find((t) => t.current)?.role ?? "member",
  );

  const { data: invites } = useQuery({
    queryKey: ["invites", tenantId],
    enabled: !!tenantId && canManage,
    queryFn: async () =>
      (
        await api.GET("/api/v1/tenants/{id}/invites", {
          params: { path: { id: tenantId! } },
        })
      ).data ?? [],
  });

  const [adding, setAdding] = React.useState(false);
  const [email, setEmail] = React.useState("");
  const [role, setRole] = React.useState("member");
  // The accept URL the API returns once, on create/resend. Client-side only:
  // after a refresh it is gone, which is correct — the token is never returned
  // again. Keyed by invite id so it attaches to the right row.
  const [freshLinks, setFreshLinks] = React.useState<Record<string, string>>({});

  const bust = () => qc.invalidateQueries({ queryKey: ["invites", tenantId] });

  const createMutation = useMutation({
    mutationFn: async (v: { email: string; role: string }) =>
      orThrow(
        await api.POST("/api/v1/tenants/{id}/invites", {
          params: { path: { id: tenantId! } },
          body: { email: v.email, role: v.role },
        }),
      ),
    onSuccess: (inv) => {
      setAdding(false);
      setEmail("");
      setRole("member");
      if (inv.accept_url) setFreshLinks((m) => ({ ...m, [inv.id]: inv.accept_url! }));
      bust();
      toast(`Invite sent to ${inv.email}`);
    },
  });

  const resendMutation = useMutation({
    mutationFn: async (inv: Invite) =>
      orThrow(
        await api.POST("/api/v1/tenants/{id}/invites/{invite}/resend", {
          params: { path: { id: tenantId!, invite: inv.id } },
        }),
      ),
    onSuccess: (inv) => {
      if (inv.accept_url) setFreshLinks((m) => ({ ...m, [inv.id]: inv.accept_url! }));
      bust();
      toast(`Invite sent to ${inv.email}`);
    },
  });

  const revokeMutation = useMutation({
    mutationFn: async (inv: Invite) => {
      await api.DELETE("/api/v1/tenants/{id}/invites/{invite}", {
        params: { path: { id: tenantId!, invite: inv.id } },
      });
      return inv;
    },
    onSuccess: () => {
      bust();
      toast("Invite revoked");
    },
  });

  const askRevoke = async (inv: Invite) => {
    const ok = await askConfirm({
      title: `Revoke the invite for ${inv.email}?`,
      description: "Their link stops working. You can invite them again later.",
      confirmLabel: "revoke",
      danger: true,
    });
    if (ok) revokeMutation.mutate(inv);
  };

  if (!canManage) return null;

  return (
    <Panel
      title="Invites"
      actions={
        !adding && (
          <button className="btn small" onClick={() => setAdding(true)}>
            <Plus size={12} /> invite
          </button>
        )
      }
    >
      <div style={{ padding: 10, display: "grid", gap: 10 }} className="small">
        <p className="muted" style={{ margin: 0 }}>
          Bring someone into <span className="bright">{me?.tenant.name}</span>.
          They join by opening the link and signing in as the invited email —
          keeping their own tenant, and gaining this one.
        </p>

        {adding && (
          <div className="chan-form">
            <div className="chan-row">
              <span className="faint small">Email</span>
              <input
                className="chan-input"
                type="email"
                value={email}
                placeholder="person@example.com"
                autoComplete="off"
                onChange={(e) => setEmail(e.target.value)}
              />
            </div>
            <div className="chan-row">
              <span className="faint small">Role</span>
              <Select
                value={role}
                onChange={setRole}
                options={[
                  { value: "member", label: "member" },
                  { value: "admin", label: "admin" },
                ]}
              />
            </div>
            <div className="chan-actions">
              <button className="btn small" onClick={() => setAdding(false)}>
                cancel
              </button>
              <button
                className="btn small primary"
                onClick={() => createMutation.mutate({ email: email.trim(), role })}
                disabled={createMutation.isPending || !email.includes("@")}
              >
                {createMutation.isPending ? (
                  <>
                    <Loader size={12} className="spin" /> creating…
                  </>
                ) : (
                  "create invite"
                )}
              </button>
            </div>
          </div>
        )}

        {(invites ?? []).length === 0 ? (
          <Empty>
            <Mail size={13} /> No pending invites.
          </Empty>
        ) : (
          <table className="nook-table">
            <tbody>
              {(invites ?? []).map((inv) => {
                const resending =
                  resendMutation.isPending && resendMutation.variables?.id === inv.id;
                const revoking =
                  revokeMutation.isPending && revokeMutation.variables?.id === inv.id;
                const link = freshLinks[inv.id];
                return (
                  <React.Fragment key={inv.id}>
                    <tr>
                      <td className="bright">{inv.email}</td>
                      <td>
                        <Pill tone={inv.role === "admin" ? "warn" : "dim"}>{inv.role}</Pill>
                      </td>
                      <td className="muted">
                        expires {new Date(inv.expires_at).toLocaleDateString()}
                      </td>
                      <td style={{ textAlign: "right", whiteSpace: "nowrap" }}>
                        <button
                          className="btn small icon"
                          title="resend the invite email"
                          disabled={resending || revoking}
                          onClick={() => resendMutation.mutate(inv)}
                        >
                          {resending ? (
                            <Loader size={12} className="spin" />
                          ) : (
                            <Send size={12} />
                          )}
                        </button>
                        <button
                          className="btn small danger icon"
                          title="revoke"
                          disabled={resending || revoking}
                          onClick={() => askRevoke(inv)}
                        >
                          {revoking ? (
                            <Loader size={12} className="spin" />
                          ) : (
                            <Trash2 size={12} />
                          )}
                        </button>
                      </td>
                    </tr>
                    {link && (
                      <tr>
                        <td colSpan={4} style={{ paddingTop: 0 }}>
                          <CopyRow value={link} />
                        </td>
                      </tr>
                    )}
                  </React.Fragment>
                );
              })}
            </tbody>
          </table>
        )}
      </div>
    </Panel>
  );
}

/** Every tenant the signed-in person belongs to. Switching re-scopes the whole
 *  app (like `TenantSwitcher`) but stays on this page, so the roster/invites
 *  above re-render against the newly active tenant. */
function YourOrganizations() {
  const qc = useQueryClient();
  const { data: me } = useMe();
  const { select } = useWorkspaceContext();
  const tenants: TenantMembership[] = me?.tenants ?? [];

  const switchMutation = useMutation({
    mutationFn: async (tenantId: string) =>
      orThrow(await api.POST("/api/v1/me/tenant", { body: { tenant_id: tenantId } })),
    onSuccess: () => {
      // The selected workspace belonged to the old tenant; drop the scope and
      // refetch everything so this page (and the rest of the app) re-loads
      // against the new active tenant. No navigation — we stay on Team.
      select(null);
      qc.invalidateQueries();
    },
  });

  return (
    <Panel title="Your organizations">
      <div style={{ padding: 10 }} className="small">
        {tenants.length === 0 ? (
          <Empty>
            <Boxes size={13} /> No organizations.
          </Empty>
        ) : (
          <table className="nook-table">
            <tbody>
              {tenants.map((t) => {
                const switching =
                  switchMutation.isPending && switchMutation.variables === t.id;
                return (
                  <tr key={t.id}>
                    <td className="bright">
                      <FolderGit2 size={13} style={{ verticalAlign: "-2px" }} /> {t.name}
                    </td>
                    <td>
                      <Pill tone={t.role === "owner" ? "ok" : "dim"}>{t.role}</Pill>
                    </td>
                    <td style={{ textAlign: "right", whiteSpace: "nowrap" }}>
                      {t.current ? (
                        <span className="faint small">
                          <Check size={12} style={{ verticalAlign: "-2px" }} /> active
                        </span>
                      ) : (
                        <button
                          className="btn small"
                          disabled={switchMutation.isPending}
                          onClick={() => switchMutation.mutate(t.id)}
                        >
                          {switching ? (
                            <>
                              <Loader size={12} className="spin" /> switching…
                            </>
                          ) : (
                            "switch"
                          )}
                        </button>
                      )}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        )}
      </div>
    </Panel>
  );
}
