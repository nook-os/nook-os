import React, { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Link, useNavigate, useParams } from "react-router-dom";
import { ArrowUpCircle, SquareTerminal } from "lucide-react";
import { api } from "@nookos/api";
import { Empty, Panel, Pill, ResourceBars, StatusDot, statusTone } from "@nookos/ui";
import { AgentVersion, NodeFacts, useControlPlaneVersion } from "../NodeFacts";
import { askConfirm, notify } from "../dialogs";
import { useLive } from "../live";
import { AddNodeModal } from "../AddNodeModal";
import { NodePlacement } from "../NodePlacement";

export function NodesPage() {
  const [adding, setAdding] = useState(false);
  const navigate = useNavigate();
  const nodeStatus = useLive((s) => s.nodeStatus);
  const nodeResources = useLive((s) => s.nodeResources);
  const { data: nodes, refetch } = useQuery({
    queryKey: ["nodes"],
    queryFn: async () => (await api.GET("/api/v1/nodes")).data ?? [],
  });
  // The caller's person id, so we can mirror the server's rule (MAIN-132): a
  // node you own is spawnable; a teammate's is manage-only. Same `["me"]` key
  // the rest of the app shares, so this rides the existing fetch.
  const { data: me } = useQuery({
    queryKey: ["me"],
    queryFn: async () => (await api.GET("/api/v1/auth/me")).data ?? null,
  });
  // What this control plane expects every agent to be — the same string it
  // sends in `RegisterAck`, so the column shows the comparison the node makes
  // rather than a second opinion about it.
  const expected = useControlPlaneVersion();

  // A shell on the machine, no project required: opens a bash session in the
  // node's home directory and drops you straight into it.
  const openTerminal = async (nodeId: string) => {
    const { data, error } = await api.POST("/api/v1/nodes/{id}/terminal", {
      params: { path: { id: nodeId } },
      body: {},
    });
    if (error || !data) {
      await notify("Couldn't open a terminal", JSON.stringify(error));
      return;
    }
    navigate(`/sessions/${data.id}`);
  };

  return (
    <div className="nook-grid" style={{ gridTemplateColumns: "1fr" }}>
      {adding && (
        <AddNodeModal
          onClose={() => {
            setAdding(false);
            refetch();
          }}
        />
      )}
      <Panel
        title="Nodes"
        actions={
          <button className="btn primary small" onClick={() => setAdding(true)}>
            + add node
          </button>
        }
      >
        {(nodes ?? []).length === 0 ? (
          <Empty>No machines yet — run `nook join` on a computer to add one.</Empty>
        ) : (
          <table className="nook-table">
            <thead>
              <tr>
                <th>Node</th>
                <th>Status</th>
                <th>Platform</th>
                <th>Agent</th>
                <th>CPUs</th>
                <th>GPUs</th>
                <th>Capacity</th>
                <th>Runtimes</th>
                <th>Last seen</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {(nodes ?? []).map((n) => {
                const caps = n.capabilities as Record<string, unknown>;
                const status = nodeStatus[n.id] ?? n.status;
                // `terminal` is offered on a node you own OR one shared with the
                // team (MAIN-136); `share` stays owner-only. Fleet MANAGEMENT
                // (update/remove) stays with the owner or a tenant admin, as it
                // was before shared nodes existed: a member seeing a teammate's
                // `shared` node (MAIN-135) gets the terminal but no manage
                // buttons — read-only — while an admin keeps them (MAIN-132's
                // admin-fleet-management). The server enforces all of this; this
                // only hides what would 403.
                const owned = n.owner_person_id === me?.person_id;
                const canManage =
                  owned ||
                  me?.user?.role === "owner" ||
                  me?.user?.role === "admin";
                return (
                  <tr key={n.id}>
                    <td>
                      <StatusDot status={status} />{" "}
                      <Link to={`/nodes/${n.id}`} className="bright">
                        {n.name}
                      </Link>{" "}
                      <span className="faint">{n.hostname}</span>
                      {n.shared && (
                        <>
                          {" "}
                          <Pill
                            tone="accent"
                            title="visible to the whole team and usable by them — anyone can start a session here"
                          >
                            shared
                          </Pill>
                        </>
                      )}
                      {/* The deployment's shared operator node (MAIN-125): a
                          machine the stack ships with the loop toolchain, not a
                          person's own. Surfaced so it is distinguishable from
                          personal nodes at a glance. */}
                      {(caps.shared_operator as boolean) && (
                        <>
                          {" "}
                          <Pill
                            tone="accent"
                            title="the deployment's shared operator node — ships with the loop toolchain"
                          >
                            operator
                          </Pill>
                        </>
                      )}
                    </td>
                    <td>
                      <Pill tone={statusTone(status)}>{status}</Pill>
                    </td>
                    <td className="muted">{n.platform}</td>
                    <td>
                      <AgentVersion
                        reported={caps.agent_version as string | null}
                        expected={expected}
                      />
                    </td>
                    <td className="muted">{(caps.cpus as number) ?? "—"}</td>
                    <td className="muted">
                      {((caps.gpus as { model: string }[]) ?? [])
                        .map((g) => g.model)
                        .join(", ") || "—"}
                    </td>
                    <td style={{ minWidth: 180 }}>
                      <ResourceBars resources={nodeResources[n.id] ?? n.resources} />
                    </td>
                    <td>
                      {((caps.runtimes as string[]) ?? []).map((r) => (
                        <Pill key={r}>{r}</Pill>
                      ))}
                    </td>
                    <td className="muted">
                      {n.last_seen_at
                        ? new Date(n.last_seen_at).toLocaleTimeString([], {
                            hour12: false,
                          })
                        : "never"}
                    </td>
                    {/* The flex box goes INSIDE the cell. Setting display:flex
                        on a <td> removes it from table layout entirely, so it
                        stops sharing the row's column widths and the buttons
                        drift out of line with every other row. */}
                    <td style={{ textAlign: "right", whiteSpace: "nowrap" }}>
                      <span
                        style={{
                          display: "inline-flex",
                          gap: 6,
                          justifyContent: "flex-end",
                        }}
                      >
                      {status === "online" && (owned || n.shared) && (
                        <button
                          className="btn small"
                          title={`open a shell on ${n.name}`}
                          onClick={() => openTerminal(n.id)}
                        >
                          <SquareTerminal size={12} /> terminal
                        </button>
                      )}
                      {/* Sharing is the owner's call and the server enforces it
                          (MAIN-135); we only offer the toggle on rows you own. */}
                      {owned && (
                        <button
                          className="btn small"
                          title={
                            n.shared
                              ? `stop sharing ${n.name} with the team`
                              : `let the team see ${n.name}`
                          }
                          onClick={async () => {
                            const { error } = await api.POST(
                              "/api/v1/nodes/{id}/shared",
                              {
                                params: { path: { id: n.id } },
                                body: { shared: !n.shared },
                              },
                            );
                            if (error) {
                              await notify(
                                "Couldn't change sharing",
                                JSON.stringify(error),
                              );
                              return;
                            }
                            refetch();
                          }}
                        >
                          {n.shared ? "unshare" : "share"}
                        </button>
                      )}
                      {status === "online" && canManage && (
                        <button
                          className="btn small"
                          title={
                            (caps.agent_version as string)
                              ? `agent ${caps.agent_version} — update and restart`
                              : "update the agent and restart it"
                          }
                          onClick={async () => {
                            const { error } = await api.POST(
                              "/api/v1/nodes/{id}/update",
                              { params: { path: { id: n.id } } },
                            );
                            // The node decides whether it can: unsupervised, it
                            // refuses rather than taking itself offline. Say
                            // what happened either way — silence after pressing
                            // a button reads as nothing happening.
                            await notify(
                              error ? "Not updated" : "Updating",
                              error
                                ? `${n.name} could not be asked to update.`
                                : `${n.name} is fetching the new agent. It will drop off for a moment and come back — sessions survive, because tmux outlives the agent.`,
                            );
                          }}
                        >
                          <ArrowUpCircle size={12} /> update
                        </button>
                      )}
                      {canManage && (
                        <button
                          className="btn danger small"
                          onClick={async () => {
                            const ok = await askConfirm({
                              title: `Remove node ${n.name}`,
                              description:
                                "It stops appearing in NookOS. Re-running `nook setup` on that machine rejoins it.",
                              confirmLabel: "remove",
                              danger: true,
                            });
                            if (ok) {
                              await api.DELETE("/api/v1/nodes/{id}", {
                                params: { path: { id: n.id } },
                              });
                              refetch();
                            }
                          }}
                        >
                          remove
                        </button>
                      )}
                      </span>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        )}
      </Panel>
    </div>
  );
}

/** One agent-authorization profile the node reported. */
type AuthProfile = {
  id: string;
  label: string;
  runtime: string;
  state: "authorized" | "not_authorized" | "unknown" | "unavailable";
  identity?: string | null;
};

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
  const profiles =
    ((node.capabilities as { runtime_auth?: AuthProfile[] })?.runtime_auth ?? []);

  // Launch the runtime's login flow in a session on this node and open it live
  // (MAIN-126 AC-2). A warning first, because on a shared machine the credential
  // becomes usable by everyone allowed to run there.
  const authorize = async (p: AuthProfile) => {
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

export function NodeDetail() {
  const { id } = useParams<{ id: string }>();
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
  const { data: workspaces } = useQuery({
    queryKey: ["workspaces"],
    queryFn: async () => (await api.GET("/api/v1/workspaces")).data ?? [],
  });

  if (!node) return <Empty>Loading…</Empty>;
  const here = (workspaces ?? []).filter((w) =>
    w.locations.some((l) => l.node_id === node.id),
  );
  const sshKey = (node.capabilities as Record<string, unknown>)
    ?.ssh_public_key as string | undefined;

  return (
    <div
      className="nook-grid"
      style={{ gridTemplateColumns: "1.2fr 1fr", gridTemplateRows: "auto 1fr" }}
    >
      {/* The inputs placement reads (MAIN-319 AC-2). Owner-only to edit — the
          server enforces it; this only avoids offering a button that 403s. */}
      <NodePlacement
        nodeId={node.id}
        canEdit={!!me?.person_id && node.owner_person_id === me.person_id}
      />

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
        style={{ gridColumn: "1 / span 2" }}
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
      <Panel title="This machine">
        <div style={{ padding: 10 }}>
          <ResourceBars resources={node.resources} />
        </div>
        <NodeFacts node={node} />
      </Panel>
      <AgentAuthPanel node={node} />
      <Panel title="Workspaces on this node">
        {here.length === 0 ? (
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
                    <td>{loc.dirty ? <Pill tone="warn">dirty</Pill> : <Pill tone="ok">clean</Pill>}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        )}
      </Panel>
    </div>
  );
}
