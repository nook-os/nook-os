import React, { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Link, useNavigate, useParams } from "react-router-dom";
import { ArrowUpCircle, SquareTerminal } from "lucide-react";
import { api } from "@nookos/api";
import { Empty, Panel, Pill, ResourceBars, StatusDot, statusTone } from "@nookos/ui";
import { askConfirm, notify } from "../dialogs";
import { useLive } from "../live";
import { AddNodeModal } from "../AddNodeModal";

export function NodesPage() {
  const [adding, setAdding] = useState(false);
  const navigate = useNavigate();
  const nodeStatus = useLive((s) => s.nodeStatus);
  const nodeResources = useLive((s) => s.nodeResources);
  const { data: nodes, refetch } = useQuery({
    queryKey: ["nodes"],
    queryFn: async () => (await api.GET("/api/v1/nodes")).data ?? [],
  });

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
          <Empty>No nodes. Add one and run `nook join` on that machine.</Empty>
        ) : (
          <table className="nook-table">
            <thead>
              <tr>
                <th>Node</th>
                <th>Status</th>
                <th>Platform</th>
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
                return (
                  <tr key={n.id}>
                    <td>
                      <StatusDot status={status} />{" "}
                      <Link to={`/nodes/${n.id}`} className="bright">
                        {n.name}
                      </Link>{" "}
                      <span className="faint">{n.hostname}</span>
                    </td>
                    <td>
                      <Pill tone={statusTone(status)}>{status}</Pill>
                    </td>
                    <td className="muted">{n.platform}</td>
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
                      {status === "online" && (
                        <button
                          className="btn small"
                          title={`open a shell on ${n.name}`}
                          onClick={() => openTerminal(n.id)}
                        >
                          <SquareTerminal size={12} /> terminal
                        </button>
                      )}
                      {status === "online" && (
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

export function NodeDetail() {
  const { id } = useParams<{ id: string }>();
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
      <Panel title={`Capabilities`}>
        <div style={{ padding: 10 }}>
          <ResourceBars resources={node.resources} />
        </div>
        <pre className="mono small" style={{ padding: 10, whiteSpace: "pre-wrap" }}>
          {JSON.stringify(node.capabilities, null, 2)}
        </pre>
      </Panel>
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
