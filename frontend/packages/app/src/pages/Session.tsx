import React, { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Link, useNavigate, useParams } from "react-router-dom";
import { GitBranch, PanelRightClose, PanelRightOpen, RefreshCw } from "lucide-react";
import { api, attachSession, type Session } from "@nookos/api";
import { Empty, Panel, Pill, statusTone, TerminalView } from "@nookos/ui";
import { useLive } from "../live";
import { useWorkspaceContext } from "../context";
import { ScopeChip } from "../layout";
import { SessionTabs } from "../SessionTabs";
import { useSessionTabs } from "../sessiontabs";

const DIFF_PANEL_KEY = "nookos-diff-panel-open";

function DiffView({ diff }: { diff: string }) {
  if (!diff.trim()) {
    return <Empty>Working tree is clean — no diff.</Empty>;
  }
  return (
    <div className="diff-view">
      {diff.split("\n").map((line, i) => {
        const cls = line.startsWith("+++") || line.startsWith("---")
          ? "file"
          : line.startsWith("diff --git")
            ? "file"
            : line.startsWith("@@")
              ? "hunk"
              : line.startsWith("+")
                ? "add"
                : line.startsWith("-")
                  ? "del"
                  : "";
        return (
          <div key={i} className={`diff-line ${cls}`}>
            {line || " "}
          </div>
        );
      })}
    </div>
  );
}

function GitPanel({ session }: { session: Session }) {
  const [tab, setTab] = useState<"diff" | "files">("diff");
  const { data, refetch, isFetching, error } = useQuery({
    queryKey: ["git", session.workspace_id, session.node_id],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/workspaces/{id}/git", {
        params: {
          path: { id: session.workspace_id },
          query: { node_id: session.node_id },
        },
      });
      if (error) throw new Error(JSON.stringify(error));
      return data ?? null;
    },
    refetchInterval: 10000,
    retry: false,
  });

  return (
    <Panel
      title={
        <>
          <GitBranch size={12} style={{ verticalAlign: "-2px" }} /> git ·{" "}
          <span className="bright">{data?.branch ?? "…"}</span>
        </>
      }
      actions={
        <>
          {data && (
            <Pill tone={data.dirty ? "warn" : "ok"}>
              {data.dirty ? `${data.files.length} changed` : "clean"}
            </Pill>
          )}{" "}
          <button
            className={`btn small${tab === "diff" ? " primary" : ""}`}
            onClick={() => setTab("diff")}
          >
            diff
          </button>{" "}
          <button
            className={`btn small${tab === "files" ? " primary" : ""}`}
            onClick={() => setTab("files")}
          >
            files
          </button>{" "}
          <button
            className="btn small"
            onClick={() => refetch()}
            disabled={isFetching}
            title="refresh"
          >
            <RefreshCw size={12} className={isFetching ? "spin" : ""} />
          </button>
        </>
      }
    >
      {error ? (
        <Empty>git status unavailable: node offline?</Empty>
      ) : !data ? (
        <Empty>Loading…</Empty>
      ) : tab === "diff" ? (
        <DiffView diff={data.diff} />
      ) : data.files.length === 0 ? (
        <Empty>No changed files.</Empty>
      ) : (
        <table className="nook-table">
          <thead>
            <tr>
              <th>St</th>
              <th>Path</th>
            </tr>
          </thead>
          <tbody>
            {data.files.map((f) => (
              <tr key={f.path}>
                <td className="mono">
                  <Pill tone={f.status.includes("?") ? "info" : "warn"}>
                    {f.status.trim() || "·"}
                  </Pill>
                </td>
                <td className="mono">{f.path}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </Panel>
  );
}

export function SessionPage() {
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const [liveStatus, setLiveStatus] = useState<string | null>(null);
  const [gitOpen, setGitOpen] = useState(
    () => localStorage.getItem(DIFF_PANEL_KEY) !== "closed",
  );
  const sessionStatus = useLive((s) => s.sessionStatus);
  const openTab = useSessionTabs((s) => s.open);
  const closeTab = useSessionTabs((s) => s.close);

  const { data: session } = useQuery({
    queryKey: ["sessions", "one", id],
    queryFn: async () =>
      (await api.GET("/api/v1/sessions/{id}", { params: { path: { id: id! } } }))
        .data,
    enabled: !!id,
  });
  const { data: ws } = useQuery({
    queryKey: ["workspaces", session?.workspace_id],
    queryFn: async () =>
      (
        await api.GET("/api/v1/workspaces/{id}", {
          params: { path: { id: session!.workspace_id } },
        })
      ).data,
    enabled: !!session,
  });

  // Visiting a session opens (or refreshes) its tab, tagged with its
  // workspace so the strip can scope tabs to the workspace context.
  useEffect(() => {
    if (session) {
      openTab({
        id: session.id,
        name: session.name,
        runtime: session.runtime,
        workspaceId: session.workspace_id,
        workspaceName: ws?.name,
      });
    }
  }, [session, ws?.name, openTab]);

  // Opening a session from another workspace follows it: the switcher, tab
  // strip, board, and activity all move to that workspace's context. (An
  // explicit "all workspaces" context is left alone.)
  const selectWorkspace = useWorkspaceContext((s) => s.select);
  const selectedWorkspaceId = useWorkspaceContext((s) => s.selectedWorkspaceId);
  useEffect(() => {
    if (
      session &&
      selectedWorkspaceId &&
      selectedWorkspaceId !== session.workspace_id
    ) {
      selectWorkspace(session.workspace_id);
    }
  }, [session, selectedWorkspaceId, selectWorkspace]);

  if (!session) return <Empty>Loading…</Empty>;
  const status = liveStatus ?? sessionStatus[session.id] ?? session.status;

  const toggleGit = () => {
    setGitOpen((open) => {
      localStorage.setItem(DIFF_PANEL_KEY, open ? "closed" : "open");
      return !open;
    });
  };

  const kill = async () => {
    if (confirm("Kill this session? The tmux session ends for real.")) {
      await api.POST("/api/v1/sessions/{id}/kill", {
        params: { path: { id: session.id } },
      });
      closeTab(session.id);
      navigate("/sessions");
    }
  };

  return (
    <div className="session-view">
      <SessionTabs activeId={session.id} />
      <div
        className="nook-grid"
        style={{ gridTemplateColumns: gitOpen ? "1fr 440px" : "1fr", flex: 1, minHeight: 0 }}
      >
        <Panel
        title={
          <>
            <Link to={`/workspaces/${session.workspace_id}`} className="bright">
              {ws?.name ?? "workspace"}
            </Link>
            <span className="faint"> ▸ </span>
            {session.name}
          </>
        }
        actions={
          <span
            style={{ display: "inline-flex", alignItems: "center", gap: 6 }}
          >
            <Pill tone="accent">{session.runtime}</Pill>
            <Pill tone={statusTone(status)}>{status}</Pill>
            <button
              className="btn small icon"
              onClick={toggleGit}
              title={gitOpen ? "hide git panel" : "show git panel"}
            >
              {gitOpen ? <PanelRightClose size={13} /> : <PanelRightOpen size={13} />}
            </button>
            <button className="btn danger small" onClick={kill}>
              kill
            </button>
          </span>
        }
      >
          <TerminalView
            key={session.id}
            attach={(handlers) => attachSession(session.id, handlers)}
            onStatus={setLiveStatus}
          />
        </Panel>
        {gitOpen && <GitPanel session={session} />}
      </div>
    </div>
  );
}

export function SessionsPage() {
  const { selectedWorkspaceId } = useWorkspaceContext();
  const { data: sessions } = useQuery({
    queryKey: ["sessions", "all", selectedWorkspaceId],
    queryFn: async () =>
      (
        await api.GET("/api/v1/sessions", {
          params: {
            query: { workspace_id: selectedWorkspaceId ?? undefined },
          },
        })
      ).data ?? [],
  });
  const sessionStatus = useLive((s) => s.sessionStatus);

  return (
    <div className="session-view">
      <SessionTabs />
      <div
        className="nook-grid"
        style={{ gridTemplateColumns: "1fr", flex: 1, minHeight: 0 }}
      >
      <Panel
        title={`Sessions (${(sessions ?? []).length})`}
        actions={<ScopeChip />}
      >
        {(sessions ?? []).length === 0 ? (
          <Empty>No sessions yet — start one from a workspace.</Empty>
        ) : (
          <table className="nook-table">
            <thead>
              <tr>
                <th>Session</th>
                <th>Runtime</th>
                <th>Status</th>
                <th>Created</th>
              </tr>
            </thead>
            <tbody>
              {(sessions ?? []).map((s) => {
                const status = sessionStatus[s.id] ?? s.status;
                return (
                  <tr key={s.id}>
                    <td>
                      <Link className="bright" to={`/sessions/${s.id}`}>
                        {s.name}
                      </Link>
                    </td>
                    <td>
                      <Pill tone="accent">{s.runtime}</Pill>
                    </td>
                    <td>
                      <Pill tone={statusTone(status)}>{status}</Pill>
                    </td>
                    <td className="muted small">
                      {new Date(s.created_at).toLocaleString()}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        )}
      </Panel>
      </div>
    </div>
  );
}

