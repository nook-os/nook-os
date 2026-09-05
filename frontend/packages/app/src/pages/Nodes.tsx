import React, { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate, useParams } from "react-router-dom";
import {
  ArrowUpCircle,
  ChevronDown,
  ChevronUp,
  Eye,
  EyeOff,
  HardDrive,
  SquareTerminal,
  Trash2,
} from "lucide-react";
import { api, type NodeInfo, type AuthProfile } from "@nookos/api";
import {
  Empty,
  Panel,
  Pill,
  ResourceBars,
  RowAction,
  SearchInput,
  StatusDot,
  statusTone,
} from "@nookos/ui";
import { AgentVersion, NodeFacts, useControlPlaneVersion } from "../NodeFacts";
import { usePagedList } from "../paging";
import { askConfirm, notify } from "../dialogs";
import { useLive } from "../live";
import { AddNodeModal } from "../AddNodeModal";
import { NodePlacement } from "../NodePlacement";
import { NodePorts } from "../NodePorts";
import { NodeCapacity } from "../NodeCapacity";
import { SectionedPage, type PageSection } from "../SectionedPage";
import { useWorkspaces } from "../workspaces";

/** The caller's standing toward a node, mirroring the server's rules
 *  (MAIN-132/135/136) so we never offer a button that 403s: `terminal` on a
 *  node you own OR one shared with the team; `share` owner-only; fleet
 *  management (update/remove) the owner or a tenant admin. */
function grants(
  n: { owner_person_id?: string | null; shared?: boolean },
  me: { person_id?: string; user?: { role?: string } } | null | undefined,
) {
  const owned = !!me?.person_id && n.owner_person_id === me.person_id;
  const canManage =
    owned || me?.user?.role === "owner" || me?.user?.role === "admin";
  return { owned, canManage };
}

/** Every action a node offers, in one place — the card head and the detail
 *  header render the same set, so the two surfaces cannot drift. */
function NodeActions({
  node,
  status,
  me,
  onRemoved,
}: {
  node: NodeInfo;
  status: string;
  me: { person_id?: string; user?: { role?: string } } | null | undefined;
  onRemoved?: () => void;
}) {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const { owned, canManage } = grants(node, me);
  const caps = node.capabilities as Record<string, unknown>;

  // A shell on the machine, no project required: opens a bash session in the
  // node's home directory and drops you straight into it.
  const openTerminal = async () => {
    const { data, error } = await api.POST("/api/v1/nodes/{id}/terminal", {
      params: { path: { id: node.id } },
      body: {},
    });
    if (error || !data) {
      await notify("Couldn't open a terminal", JSON.stringify(error));
      return;
    }
    navigate(`/sessions/${data.id}`);
  };

  return (
    <span style={{ display: "inline-flex", alignItems: "center", gap: 4 }}>
      {status === "online" && (owned || node.shared) && (
        <RowAction
          icon={SquareTerminal}
          label="terminal"
          title={`open a shell on ${node.name}`}
          onClick={openTerminal}
        />
      )}
      {/* Sharing is the owner's call and the server enforces it (MAIN-135). */}
      {owned && (
        <RowAction
          icon={node.shared ? EyeOff : Eye}
          label={node.shared ? "unshare" : "share"}
          title={
            node.shared
              ? `stop sharing ${node.name} with the team`
              : `let the team see ${node.name}`
          }
          onClick={async () => {
            const { error } = await api.POST("/api/v1/nodes/{id}/shared", {
              params: { path: { id: node.id } },
              body: { shared: !node.shared },
            });
            if (error) {
              await notify("Couldn't change sharing", JSON.stringify(error));
              return;
            }
            queryClient.invalidateQueries({ queryKey: ["nodes"] });
          }}
        />
      )}
      {status === "online" && canManage && (
        <RowAction
          icon={ArrowUpCircle}
          label="update"
          title={
            (caps.agent_version as string)
              ? `agent ${caps.agent_version} — update and restart`
              : "update the agent and restart it"
          }
          onClick={async () => {
            const { error } = await api.POST("/api/v1/nodes/{id}/update", {
              params: { path: { id: node.id } },
            });
            // The node decides whether it can: unsupervised, it refuses rather
            // than taking itself offline. Say what happened either way —
            // silence after pressing a button reads as nothing happening.
            await notify(
              error ? "Not updated" : "Updating",
              error
                ? `${node.name} could not be asked to update.`
                : `${node.name} is fetching the new agent. It will drop off for a moment and come back — terminal sessions survive, because tmux outlives the agent. A loop job running here does not: it is streamed through the agent itself.`,
            );
          }}
        />
      )}
      {canManage && (
        <RowAction
          icon={Trash2}
          danger
          title={`remove ${node.name} from NookOS`}
          onClick={async () => {
            const ok = await askConfirm({
              title: `Remove node ${node.name}`,
              description:
                "It stops appearing in NookOS. Re-running `nook setup` on that machine rejoins it.",
              confirmLabel: "remove",
              danger: true,
            });
            if (ok) {
              await api.DELETE("/api/v1/nodes/{id}", {
                params: { path: { id: node.id } },
              });
              queryClient.invalidateQueries({ queryKey: ["nodes"] });
              onRemoved?.();
            }
          }}
        />
      )}
    </span>
  );
}

/** The shared/operator pills, everywhere a node is titled. */
function NodeBadges({ node }: { node: NodeInfo }) {
  const caps = node.capabilities as Record<string, unknown>;
  const cordon = node.cordon as {
    reason?: string;
    overdue?: boolean;
    installing?: boolean;
  } | null;
  return (
    <>
      {/* Draining before an agent restart (MAIN-505). Online and taking
          nothing looks exactly like idle otherwise, which is what made "why
          did nothing get placed on azul" a log dive. The reason is the node's
          own sentence — never re-worded here, so every surface says one thing. */}
      {cordon && (
        <Pill tone={cordon.overdue ? "err" : "warn"} title={cordon.reason}>
          {cordon.installing
            ? "updating"
            : cordon.overdue
              ? "cordoned (overdue)"
              : "cordoned"}
        </Pill>
      )}
      {node.shared && (
        <Pill
          tone="accent"
          title="visible to the whole team and usable by them — anyone can start a session here"
        >
          shared
        </Pill>
      )}
      {/* The deployment's shared operator node (MAIN-125): a machine the stack
          ships with the loop toolchain, not a person's own. Surfaced so it is
          distinguishable from personal nodes at a glance. */}
      {(caps.shared_operator as boolean) && (
        <Pill
          tone="accent"
          title="the deployment's shared operator node — ships with the loop toolchain"
        >
          operator
        </Pill>
      )}
    </>
  );
}

/** One machine as a card — the Mission grid's card system (`m-repo`), because
 *  a ten-column row stopped fitting the moment nodes grew capacity bars and
 *  runtime lists. The card holds everything the row held; nothing was cut. */
function NodeCard({
  node,
  me,
  expected,
}: {
  node: NodeInfo;
  me: { person_id?: string; user?: { role?: string } } | null | undefined;
  expected?: string;
}) {
  const nodeStatus = useLive((s) => s.nodeStatus);
  const nodeResources = useLive((s) => s.nodeResources);
  const status = nodeStatus[node.id] ?? node.status;
  const caps = node.capabilities as Record<string, unknown>;
  const gpus = ((caps.gpus as { model: string }[]) ?? [])
    .map((g) => g.model)
    .join(", ");
  const runtimes = (caps.runtimes as string[]) ?? [];

  return (
    <section className="m-repo node-card" data-testid={`node-card-${node.id}`}>
      <div className="m-repo-head static">
        <StatusDot status={status} />
        <Link className="m-repo-name bright" to={`/nodes/${node.id}`}>
          {node.name}
        </Link>
        <span className="m-repo-remote">{node.hostname}</span>
        <span className="m-repo-roll">
          {status !== "online" && <Pill tone={statusTone(status)}>{status}</Pill>}
          <NodeBadges node={node} />
        </span>
      </div>
      <div className="m-card-body">
        <ResourceBars resources={nodeResources[node.id] ?? node.resources} />
        <div className="node-card-facts">
          <span className="k">agent</span>
          <span>
            <AgentVersion
              reported={caps.agent_version as string | null}
              expected={expected}
            />
          </span>
          <span className="k">platform</span>
          <span className="muted">
            {node.platform}
            {caps.architecture ? ` · ${caps.architecture}` : ""}
          </span>
          <span className="k">hardware</span>
          <span className="muted">
            {(caps.cpus as number) ?? "—"} cpus{gpus ? ` · ${gpus}` : ""}
          </span>
          <span className="k">runtimes</span>
          <span>
            {runtimes.length ? (
              runtimes.map((r) => <Pill key={r}>{r}</Pill>)
            ) : (
              <span className="faint">—</span>
            )}
          </span>
          <span className="k">last seen</span>
          <span className="muted mono small">
            {node.last_seen_at
              ? new Date(node.last_seen_at).toLocaleTimeString([], {
                  hour12: false,
                })
              : "never"}
          </span>
        </div>
        <div className="node-card-actions">
          <NodeActions node={node} status={status} me={me} />
        </div>
      </div>
    </section>
  );
}

/** The endpoint's sort set, as a compact control — cards have no column
 *  headers to click, so the sort keys the table offered live here instead. */
function SortButtons({
  sort,
  toggle,
}: {
  sort: { key: string; desc: boolean } | null;
  toggle: (key: string) => void;
}) {
  const keys: [string, string][] = [
    ["name", "name"],
    ["status", "status"],
    ["platform", "platform"],
    ["last_seen", "seen"],
  ];
  return (
    <span style={{ display: "inline-flex", gap: 2 }}>
      {keys.map(([key, label]) => (
        <button
          key={key}
          type="button"
          className="btn small"
          aria-pressed={sort?.key === key}
          title={`sort by ${label}`}
          style={sort?.key === key ? { color: "var(--nook-accent)" } : undefined}
          onClick={() => toggle(key)}
        >
          {label}
          {sort?.key === key &&
            (sort.desc ? <ChevronDown size={11} /> : <ChevronUp size={11} />)}
        </button>
      ))}
    </span>
  );
}

export function NodesPage() {
  const [adding, setAdding] = useState(false);
  const queryClient = useQueryClient();
  // The list speaks the pagination contract; liveness stays an overlay — the
  // socket's status/resources are keyed by node id and painted over whatever
  // page of cards is showing.
  const list = usePagedList<NodeInfo>({
    key: ["nodes", "page"],
    fetch: async (params) =>
      (await api.GET("/api/v1/nodes/page", { params: { query: params } })).data,
  });
  const { data: me } = useQuery({
    queryKey: ["me"],
    queryFn: async () => (await api.GET("/api/v1/auth/me")).data ?? null,
  });
  // What this control plane expects every agent to be — the same string it
  // sends in `RegisterAck`, so the card shows the comparison the node makes
  // rather than a second opinion about it.
  const expected = useControlPlaneVersion();

  return (
    <div className="nook-grid cards">
      {adding && (
        <AddNodeModal
          onClose={() => {
            setAdding(false);
            queryClient.invalidateQueries({ queryKey: ["nodes"] });
          }}
        />
      )}
      <Panel
        className="m-panel"
        title="Nodes"
        actions={
          <>
            <SortButtons sort={list.sort} toggle={list.toggleSort} />
            <SearchInput
              onSearch={list.setSearch}
              placeholder="Search name, host, platform…"
              ariaLabel="Search nodes"
            />
            <button className="btn primary small" onClick={() => setAdding(true)}>
              + add node
            </button>
          </>
        }
      >
        {list.rows.length === 0 ? (
          <Empty>
            {list.loading
              ? "Loading…"
              : list.filtered
                ? "No matches."
                : "No machines yet — run `nook join` on a computer to add one."}
          </Empty>
        ) : (
          <>
            <div className="m-grid node-grid" data-testid="nodes-grid">
              {list.rows.map((n) => (
                <NodeCard key={n.id} node={n} me={me} expected={expected} />
              ))}
            </div>
            {list.hasMore && (
              <div className="data-list-more">
                <button
                  type="button"
                  className="data-list-more-btn"
                  onClick={list.loadMore}
                  disabled={list.loadingMore}
                >
                  {list.loadingMore ? "Loading…" : "Load more"}
                </button>
              </div>
            )}
          </>
        )}
      </Panel>
    </div>
  );
}

/** Runtimes whose login the node can drive without a terminal (MAIN-650).
 *  Mirrors `runtime_auth::managed_login_args` on the node; a runtime absent
 *  here falls back to the session flow, which is still the only path for one
 *  that cannot be piped. */
const MANAGED_LOGIN_RUNTIMES = ["claude"];

/** One agent-authorization profile the node reported.
 *
 *  The GENERATED type, not a hand-written twin: this was a copy, and it drifted
 *  the moment the node grew a field — `device_flow` existed in the schema and
 *  not here, so the page could not see the one fact that decides which
 *  authorization flow is correct (MAIN-650). */


const AUTH_TONE: Record<AuthProfile["state"], "ok" | "warn" | "dim"> = {
  authorized: "ok",
  not_authorized: "warn",
  unknown: "dim",
  unavailable: "dim",
};
const AUTH_LABEL: Record<AuthProfile["state"], string> = {
  authorized: "authorized",
  not_authorized: "not authorized",
  unknown: "unknown",
  unavailable: "unavailable",
};

/** Agent authorization (MAIN-126): the node probes each runtime's own CLI for
 *  its login state — never a credential-file guess — and reports one profile per
 *  auth target. This surfaces those states; launching the login flow from here
 *  is the follow-up (AC-2/AC-4). */
function AgentAuthPanel({ node }: { node: { id: string; capabilities: unknown } }) {
  const navigate = useNavigate();
  const flow = useLive((s) => s.runtimeAuth);
  const [pastedCode, setPastedCode] = useState("");
  const profiles =
    ((node.capabilities as { runtime_auth?: AuthProfile[] })?.runtime_auth ?? []);

  // Launch the runtime's login flow in a session on this node and open it live
  // (MAIN-126 AC-2). A warning first, because on a shared machine the credential
  // becomes usable by everyone allowed to run there.
  const authorize = async (p: AuthProfile) => {
    // Two flows, and which one is right is a property of the NODE, not a
    // preference (MAIN-650). On a Pod executor the agent is a Pod elsewhere in
    // the cluster and reads only the credential Secret, so a terminal here
    // would sign in a container that runs no work — the login appears to
    // succeed and changes nothing. Such a node asks for the control plane's
    // device flow, which delivers the credential to where jobs actually read
    // it.
    // The managed login: the node runs the runtime's own sign-in with pipes and
    // reports the link, so the operator gets a link and a box instead of a
    // terminal. Preferred wherever the runtime supports it — a session is a
    // heavy answer to "open this and paste what it gives you".
    if (MANAGED_LOGIN_RUNTIMES.includes(p.runtime)) {
      const ok = await askConfirm({
        title: `Authorize ${p.label}?`,
        description:
          "Signs in through your browser — the link appears here and you paste the code back. On shared substrate the credential is usable by every workload allowed to run there.",
        confirmLabel: "start sign-in",
      });
      if (!ok) return;
      useLive.setState({
        runtimeAuth: { flowId: "", runtime: p.runtime, state: "starting" },
      });
      const { data, error } = await api.POST("/api/v1/nodes/{id}/managed-login", {
        params: { path: { id: node.id } },
        body: { runtime: p.runtime },
      });
      if (error || !data) {
        useLive.setState({ runtimeAuth: null });
        await notify("Couldn't start sign-in", JSON.stringify(error));
        return;
      }
      useLive.setState({
        runtimeAuth: { flowId: data.flow_id, runtime: p.runtime, state: "starting" },
      });
      // The link arrives as a `managed_login_prompt` event and renders below.
      return;
    }

    const ok = await askConfirm({
      title: `Authorize ${p.label}?`,
      description:
        "Opens the runtime's login flow in a live session on this machine — follow the device code / URL it prints to sign in, and paste back any code it asks for. On a shared machine, the resulting credential is usable by everyone allowed to run there.",
      confirmLabel: "open login",
    });
    if (!ok) return;
    const { data, error } = await api.POST("/api/v1/nodes/{id}/authorize", {
      params: { path: { id: node.id } },
      body: { runtime: p.runtime },
    });
    if (error || !data) {
      await notify("Couldn't start authorization", JSON.stringify(error));
      return;
    }
    // The existing live session view renders the code/URL and takes input; the
    // node re-probes on its next connect/heartbeat, so returning here shows the
    // refreshed status without a manual reload.
    navigate(`/sessions/${data.id}`);
  };

  return (
    <Panel title="Agent authorization">
      {flow && (
        <div className="m-card-body" style={{ marginBottom: 8 }}>
          {flow.state === "starting" && <span>Starting sign-in…</span>}
          {flow.state === "prompt" && (
            <>
              <div>
                Open this to sign in:{" "}
                <a href={flow.verificationUri} target="_blank" rel="noreferrer">
                  {flow.verificationUri}
                </a>
              </div>
              {flow.wantsCode && (
                <form
                  style={{ display: "flex", gap: 8, marginTop: 8 }}
                  onSubmit={async (e) => {
                    e.preventDefault();
                    const code = pastedCode.trim();
                    if (!code) return;
                    setPastedCode("");
                    const { error } = await api.POST(
                      "/api/v1/nodes/{id}/managed-login/code",
                      {
                        params: { path: { id: node.id } },
                        body: {
                          flow_id: flow.flowId,
                          runtime: flow.runtime,
                          code,
                        },
                      },
                    );
                    if (error) await notify("Couldn't send the code", JSON.stringify(error));
                  }}
                >
                  <input
                    className="mono"
                    style={{ flex: 1 }}
                    placeholder="paste the code from that page"
                    value={pastedCode}
                    onChange={(e) => setPastedCode(e.target.value)}
                  />
                  <button className="btn primary small" type="submit">
                    send
                  </button>
                </form>
              )}
            </>
          )}
          {flow.state === "delivered" && (
            <span>Signed in — the credential is in place on this node.</span>
          )}
          {flow.state === "failed" && <span>Sign-in failed: {flow.error}</span>}
        </div>
      )}
      {profiles.length === 0 ? (
        <Empty>
          No agent runtimes to authorize on this machine — install claude or
          hermes and reconnect.
        </Empty>
      ) : (
        <table className="nook-table">
          <tbody>
            {profiles.map((p) => (
              <tr key={p.id}>
                <td className="bright">{p.label}</td>
                <td className="muted mono">{p.identity ?? ""}</td>
                <td style={{ textAlign: "right", whiteSpace: "nowrap" }}>
                  <span
                    style={{
                      display: "inline-flex",
                      gap: 8,
                      alignItems: "center",
                      justifyContent: "flex-end",
                    }}
                  >
                    <Pill tone={AUTH_TONE[p.state]}>{AUTH_LABEL[p.state]}</Pill>
                    {/* A runtime that isn't installed can't be logged in. */}
                    {p.state !== "unavailable" && (
                      <button className="btn small" onClick={() => authorize(p)}>
                        {p.state === "authorized" ? "re-authorize" : "authorize"}
                      </button>
                    )}
                  </span>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </Panel>
  );
}

/** One machine, as the same sectioned admin shell Settings and Admin use —
 *  the panel grid it replaces answered no question about where a thing SHOULD
 *  live, and every node capability added made it worse. Identity and actions
 *  stay standing in the intro; everything else is a findable section. */
export function NodeDetail() {
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const nodeStatus = useLive((s) => s.nodeStatus);
  const nodeResources = useLive((s) => s.nodeResources);
  const { data: me } = useQuery({
    queryKey: ["me"],
    queryFn: async () => (await api.GET("/api/v1/auth/me")).data ?? null,
  });
  const { data: node } = useQuery({
    queryKey: ["nodes", id],
    queryFn: async () =>
      (await api.GET("/api/v1/nodes/{id}", { params: { path: { id: id! } } }))
        .data,
    enabled: !!id,
  });
  // "Which repos live here" has no server-side filter, and inventing one is
  // another card's job (MAIN-606 NG-4) — so this pages the collection and
  // narrows each page, with the same Load more the tables use.
  const workspaces = useWorkspaces();
  const expected = useControlPlaneVersion();

  if (!node) return <Empty>Loading…</Empty>;
  const status = nodeStatus[node.id] ?? node.status;
  const caps = node.capabilities as Record<string, unknown>;
  const here = workspaces.rows.filter((w) =>
    w.locations.some((l) => l.node_id === node.id),
  );
  const sshKey = caps?.ssh_public_key as string | undefined;
  const { owned } = grants(node, me);

  const sections: PageSection[] = [
    {
      id: "overview",
      title: "This machine",
      group: "Machine",
      keywords: ["facts", "capacity", "cpu", "memory", "gpu", "version", "agent"],
      render: () => (
        <Panel title="This machine">
          <div style={{ padding: 10 }}>
            <ResourceBars resources={nodeResources[node.id] ?? node.resources} />
          </div>
          <NodeFacts node={node} />
        </Panel>
      ),
    },
    {
      id: "auth",
      title: "Agent authorization",
      group: "Machine",
      keywords: ["claude", "login", "runtime", "authorize", "credentials"],
      render: () => <AgentAuthPanel node={node} />,
    },
    {
      id: "ssh",
      title: "SSH key",
      group: "Machine",
      keywords: ["deploy key", "git", "clone", "private repo", "public key"],
      render: () => (
        <Panel
          title={`SSH key · ${node.name}`}
          actions={
            sshKey && (
              <button
                className="btn small"
                onClick={() => navigator.clipboard.writeText(sshKey)}
              >
                copy
              </button>
            )
          }
        >
          {sshKey ? (
            <div style={{ padding: 10 }}>
              <div
                className="mono small"
                style={{
                  userSelect: "all",
                  wordBreak: "break-all",
                  padding: 8,
                  background: "var(--nook-bg-panel)",
                  border: "1px solid var(--nook-border)",
                  borderRadius: "var(--nook-radius)",
                }}
              >
                {sshKey}
              </div>
              <div className="muted small" style={{ marginTop: 6 }}>
                Add this as a deploy key on your git host and this node can clone
                private repos. The private key never leaves the machine.
              </div>
            </div>
          ) : (
            <Empty>
              No SSH key reported — install ssh-keygen on the node and restart
              `nook run`.
            </Empty>
          )}
        </Panel>
      ),
    },
    {
      // The inputs placement reads (MAIN-319 AC-2). Owner-only to edit — the
      // server enforces it; this only avoids offering a button that 403s.
      id: "placement",
      title: "Placement",
      group: "Scheduling",
      keywords: ["dispatch", "capacity", "scheduler", "labels", "taints"],
      render: () => <NodePlacement nodeId={node.id} canEdit={owned} />,
    },
    {
      // The port range sessions lease from, and who holds what (MAIN-301).
      // Owner-only to edit, same rule and same reason as placement.
      id: "ports",
      title: "Ports",
      group: "Scheduling",
      keywords: ["lease", "range", "listener", "port"],
      render: () => <NodePorts nodeId={node.id} canEdit={owned} />,
    },
    {
      // How much agent work this machine takes (MAIN-508) — the port range's
      // twin, beside it because they are one sizing decision. Owner-only to
      // edit, same rule and same reason.
      id: "capacity",
      title: "Loop capacity",
      group: "Scheduling",
      keywords: ["loop", "jobs", "concurrency", "cordon", "build", "capacity"],
      render: () => <NodeCapacity nodeId={node.id} canEdit={owned} />,
    },
    {
      id: "workspaces",
      title: "Workspaces here",
      group: "Work",
      keywords: ["repos", "checkouts", "clones"],
      render: () => (
        <Panel title="Workspaces on this node">
          {here.length === 0 && !workspaces.hasMore ? (
            <Empty>Nothing discovered here yet.</Empty>
          ) : (
            <table className="nook-table">
              <tbody>
                {here.map((w) => {
                  const loc = w.locations.find((l) => l.node_id === node.id)!;
                  return (
                    <tr key={w.id}>
                      <td>
                        <Link className="bright" to={`/workspaces/${w.id}`}>
                          {w.name}
                        </Link>
                      </td>
                      <td className="mono muted">{loc.path}</td>
                      <td className="muted">{loc.git_branch ?? "—"}</td>
                      <td>
                        {loc.dirty ? (
                          <Pill tone="warn">dirty</Pill>
                        ) : (
                          <Pill tone="ok">clean</Pill>
                        )}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          )}
          {workspaces.hasMore && (
            <div className="data-list-more">
              <button
                type="button"
                className="data-list-more-btn"
                onClick={workspaces.loadMore}
                disabled={workspaces.loadingMore}
              >
                {workspaces.loadingMore ? "Loading…" : "Load more"}
              </button>
            </div>
          )}
        </Panel>
      ),
    },
  ];

  const intro = (
    <Panel
      title={`Node · ${node.name}`}
      actions={
        <NodeActions
          node={node}
          status={status}
          me={me}
          onRemoved={() => navigate("/nodes")}
        />
      }
    >
      <div className="detail-id-head">
        <HardDrive size={14} />
        <StatusDot status={status} />
        <Pill tone={statusTone(status)}>{status}</Pill>
        <span className="mono muted">{(caps.hostname as string) ?? node.hostname}</span>
        <span className="muted">
          {node.platform}
          {caps.architecture ? ` · ${caps.architecture}` : ""}
        </span>
        <AgentVersion
          reported={caps.agent_version as string | null}
          expected={expected}
        />
        <NodeBadges node={node} />
      </div>
    </Panel>
  );

  return <SectionedPage sections={sections} placeholder="find…" intro={intro} />;
}
