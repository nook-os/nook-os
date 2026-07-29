import React, { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "react-router-dom";
import { SquareTerminal } from "lucide-react";
import { api } from "@nookos/api";
import type { OverviewCheckout, OverviewWorkspace, Session } from "@nookos/api";
import { Empty, Panel, Pill, StatusDot } from "@nookos/ui";
import { useNewWork } from "../newwork";
import { SessionOwner } from "../sessionOwner";
import { notify } from "../dialogs";
import {
  canAddWorktree,
  canOpenTerminal,
  filterOverview,
  groupCheckoutsByNode,
  isMissing,
} from "../mission";

/// Mission Control (MAIN-226): every repo × machine × checkout × session on one
/// dense screen, with the everyday actions inline. Reads the single aggregate
/// endpoint; visibility is enforced server-side.
export function MissionPage() {
  const showNewWork = useNewWork((s) => s.show);
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const [q, setQ] = useState("");

  const { data: me } = useQuery({
    queryKey: ["me"],
    queryFn: async () => (await api.GET("/api/v1/auth/me")).data ?? null,
  });
  const { data: overview } = useQuery({
    queryKey: ["overview"],
    queryFn: async () => (await api.GET("/api/v1/overview")).data,
  });

  const workspaces = filterOverview(overview, q);
  const loose = overview?.loose_sessions ?? [];

  // "Terminal here": start a bash session pinned to this exact checkout path —
  // the capability that kills the "can't open a session in an existing worktree"
  // dead-end (MAIN-222 binding). Reuses the ordinary create-session endpoint.
  const terminalHere = async (ws: OverviewWorkspace, c: OverviewCheckout) => {
    const { data: session, error, response } = await api.POST("/api/v1/sessions", {
      body: { workspace_id: ws.id, node_id: c.node_id, runtime: "bash", path: c.path },
    });
    if (error || !response.ok) {
      await notify(
        "Terminal failed",
        error ? String((error as { error: unknown }).error) : response.statusText,
      );
      return;
    }
    await queryClient.invalidateQueries({ queryKey: ["overview"] });
    if (session?.id) navigate(`/sessions/${session.id}`);
  };

  const meId = me?.user?.id;

  return (
    <div className="nook-grid" style={{ gridTemplateColumns: "1fr", gridTemplateRows: "auto 1fr" }}>
      <Panel
        title="Mission Control"
        actions={
          <input
            className="input small mono"
            placeholder="filter repo / node / branch / session…"
            value={q}
            onChange={(e) => setQ(e.target.value)}
            style={{ width: 280 }}
            aria-label="filter"
          />
        }
      >
        {workspaces.length === 0 && loose.length === 0 ? (
          <Empty>
            Nothing running or checked out yet. Clone a repo or start work — it
            appears here grouped by machine.
          </Empty>
        ) : (
          <div className="mission-list" style={{ overflow: "auto" }}>
            {workspaces.map((w) => (
              <RepoBlock
                key={w.id}
                w={w}
                meId={meId}
                onTerminal={(c) => terminalHere(w, c)}
                onWorktree={(c) => showNewWork({ workspaceId: w.id, nodeId: c.node_id, worktree: true })}
              />
            ))}
            {loose.length > 0 && (
              <div className="mission-repo" data-testid="loose-sessions">
                <div className="mission-repo-head mono muted">ad-hoc terminals (no workspace)</div>
                <SessionRows sessions={loose} meId={meId} />
              </div>
            )}
          </div>
        )}
      </Panel>
    </div>
  );
}

function RepoBlock({
  w,
  meId,
  onTerminal,
  onWorktree,
}: {
  w: OverviewWorkspace;
  meId?: string;
  onTerminal: (c: OverviewCheckout) => void;
  onWorktree: (c: OverviewCheckout) => void;
}) {
  const nodes = groupCheckoutsByNode(w.checkouts);
  return (
    <div className="mission-repo" data-testid={`repo-${w.slug}`}>
      <div className="mission-repo-head">
        <Link className="bright" to={`/workspaces/${w.id}`}>
          {w.name}
        </Link>{" "}
        <span className="faint mono small">{w.git_remote_url ?? "(no remote)"}</span>
      </div>

      {nodes.map((n) => (
        <div key={n.nodeId} className="mission-node">
          <div className="mission-node-head mono muted">
            <StatusDot status={n.nodeStatus} /> {n.nodeName}
          </div>
          <table className="nook-table">
            <tbody>
              {n.checkouts.map((c) => (
                <React.Fragment key={c.id}>
                  <tr
                    data-testid={`checkout-${c.id}`}
                    className={isMissing(c) ? "ghost" : undefined}
                    style={isMissing(c) ? { opacity: 0.5 } : undefined}
                  >
                    <td className="mono">
                      <Pill tone={c.kind === "worktree" ? "info" : "dim"}>{c.kind}</Pill>
                    </td>
                    <td className="mono muted">{c.path}</td>
                    <td className="mono">{c.branch ?? "—"}</td>
                    <td>
                      {isMissing(c) ? (
                        <Pill tone="warn">missing</Pill>
                      ) : c.dirty ? (
                        <Pill tone="warn">dirty</Pill>
                      ) : (
                        <Pill tone="ok">clean</Pill>
                      )}
                    </td>
                    <td style={{ textAlign: "right", whiteSpace: "nowrap" }}>
                      {canOpenTerminal(c) && (
                        <button
                          className="btn small"
                          data-testid={`terminal-${c.id}`}
                          onClick={() => onTerminal(c)}
                        >
                          <SquareTerminal size={12} /> terminal here
                        </button>
                      )}{" "}
                      {canAddWorktree(c) && (
                        <button
                          className="btn small"
                          data-testid={`worktree-${c.id}`}
                          onClick={() => onWorktree(c)}
                        >
                          + worktree
                        </button>
                      )}
                    </td>
                  </tr>
                  {c.sessions.length > 0 && (
                    <tr>
                      <td colSpan={5} style={{ paddingLeft: 24 }}>
                        <SessionRows sessions={c.sessions} meId={meId} />
                      </td>
                    </tr>
                  )}
                </React.Fragment>
              ))}
            </tbody>
          </table>
        </div>
      ))}

      {w.unbound_sessions.length > 0 && (
        <div className="mission-node" data-testid={`unbound-${w.slug}`}>
          <div className="mission-node-head mono faint">sessions without a live checkout</div>
          <SessionRows sessions={w.unbound_sessions} meId={meId} />
        </div>
      )}
    </div>
  );
}

function SessionRows({ sessions, meId }: { sessions: Session[]; meId?: string }) {
  return (
    <table className="nook-table">
      <tbody>
        {sessions.map((s) => (
          <tr key={s.id} data-testid={`session-${s.id}`}>
            <td>
              <Link className="bright" to={`/sessions/${s.id}`}>
                {s.name}
              </Link>
            </td>
            <td>
              <Pill tone="accent">{s.runtime}</Pill>
            </td>
            <td className="mono muted">{s.status}</td>
            <td>
              <SessionOwner createdBy={s.created_by} meId={meId} />
            </td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}
