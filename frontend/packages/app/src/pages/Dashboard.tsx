import React from "react";
import { useQuery } from "@tanstack/react-query";
import { Link } from "react-router-dom";
import { api } from "@nookos/api";
import { Empty, Panel, Pill, StatusDot, statusTone } from "@nookos/ui";
import { useLive } from "../live";
import { ActivityFeed } from "./Activity";
import { WorkspaceLocations } from "../WorkspaceLocations";

export function Dashboard() {
  const { data: nodes } = useQuery({
    queryKey: ["nodes"],
    queryFn: async () => (await api.GET("/api/v1/nodes")).data ?? [],
  });
  const { data: sessions } = useQuery({
    queryKey: ["sessions", "active"],
    queryFn: async () =>
      (await api.GET("/api/v1/sessions", { params: { query: { active: true } } }))
        .data ?? [],
  });
  const { data: workspaces } = useQuery({
    queryKey: ["workspaces"],
    queryFn: async () => (await api.GET("/api/v1/workspaces")).data ?? [],
  });
  const { data: suggestion } = useQuery({
    queryKey: ["dispatcher"],
    queryFn: async () =>
      (await api.POST("/api/v1/dispatcher/suggest")).data ?? null,
    retry: false,
  });
  const nodeStatus = useLive((s) => s.nodeStatus);

  return (
    <div
      className="nook-grid"
      style={{ gridTemplateColumns: "1fr 1fr 1.4fr", gridTemplateRows: "1fr 1fr" }}
    >
      <Panel title={`Nodes (${(nodes ?? []).length})`}>
        {(nodes ?? []).length === 0 ? (
          <Empty>No nodes yet — add one from the Nodes tab.</Empty>
        ) : (
          <table className="nook-table">
            <tbody>
              {(nodes ?? []).map((n) => {
                const status = nodeStatus[n.id] ?? n.status;
                const caps = n.capabilities as Record<string, unknown>;
                return (
                  <tr key={n.id}>
                    <td>
                      <StatusDot status={status} />{" "}
                      <Link to={`/nodes/${n.id}`} className="bright">
                        {n.name}
                      </Link>
                    </td>
                    <td className="muted">{n.platform}</td>
                    <td>
                      {((caps.runtimes as string[]) ?? [])
                        .filter((r) => ["claude", "hermes", "codex"].includes(r))
                        .map((r) => (
                          <Pill key={r} tone="accent">
                            {r}
                          </Pill>
                        ))}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        )}
      </Panel>

      <Panel title={`Active sessions (${(sessions ?? []).length})`}>
        {(sessions ?? []).length === 0 ? (
          <Empty>No active sessions. Open a workspace to start one.</Empty>
        ) : (
          <table className="nook-table">
            <tbody>
              {(sessions ?? []).map((s) => (
                <tr key={s.id}>
                  <td>
                    <Link to={`/sessions/${s.id}`} className="bright mono">
                      {s.name}
                    </Link>
                  </td>
                  <td>
                    <Pill tone="accent">{s.runtime}</Pill>
                  </td>
                  <td>
                    <Pill tone={statusTone(s.status)}>{s.status}</Pill>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </Panel>

      <Panel title="Activity" style={{ gridRow: "1 / span 2" }}>
        <ActivityFeed limit={80} />
      </Panel>

      <Panel title={`Workspaces (${(workspaces ?? []).length})`}>
        {(workspaces ?? []).length === 0 ? (
          <Empty>No workspaces discovered yet.</Empty>
        ) : (
          <table className="nook-table">
            <tbody>
              {(workspaces ?? []).map((w) => (
                <tr key={w.id}>
                  <td>
                    <Link to={`/workspaces/${w.id}`} className="bright">
                      {w.name}
                    </Link>
                  </td>
                  <td>
                    <WorkspaceLocations locations={w.locations} />
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </Panel>

      <Panel title="What needs attention">
        {!suggestion ? (
          <Empty>The dispatcher has nothing for you yet.</Empty>
        ) : (
          <div style={{ padding: 8 }}>
            <div className="bright" style={{ marginBottom: 6 }}>
              {suggestion.headline}
            </div>
            {suggestion.items.map((item, i) => (
              <div key={i} style={{ padding: "4px 0" }}>
                <span className="mono faint">{String(i + 1).padStart(2, "0")}</span>{" "}
                <span>{item.title}</span>
                <div className="muted small" style={{ paddingLeft: 24 }}>
                  {item.rationale}
                </div>
              </div>
            ))}
          </div>
        )}
      </Panel>
    </div>
  );
}
