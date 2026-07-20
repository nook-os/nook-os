import React, { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate, useParams } from "react-router-dom";
import {
  GitBranch,
  Trash2,
  PanelRightClose,
  PanelRightOpen,
  RefreshCw,
  RotateCw,
} from "lucide-react";
import { api, attachSession, type Session } from "@nookos/api";
import { Empty, Panel, Pill, statusTone, TerminalView } from "@nookos/ui";
import { useLive } from "../live";
import { useWorkspaceContext } from "../context";
import { ScopeChip } from "../layout";
import { SessionTabs } from "../SessionTabs";
import { SessionWindows, SplitButtons } from "../SessionWindows";
import { useSessionTabs } from "../sessiontabs";
import { askConfirm, notify } from "../dialogs";

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

/** Live means the node still holds a terminal for it. */
function isLive(status: string): boolean {
  return status === "starting" || status === "running" || status === "detached";
}

export function SessionPage() {
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const [liveStatus, setLiveStatus] = useState<string | null>(null);
  const [attachKey, setAttachKey] = useState(0);
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

  const dead = status === "exited" || status === "error";

  const restart = async () => {
    setLiveStatus("starting");
    const { error } = await api.POST("/api/v1/sessions/{id}/restart", {
      params: { path: { id: session.id } },
    });
    if (error) {
      setLiveStatus(null);
      await notify("Restart failed", JSON.stringify(error));
      return;
    }
    // Remount the terminal so it re-attaches to the fresh tmux session.
    setAttachKey((k) => k + 1);
    queryClient.invalidateQueries();
  };

  const kill = async () => {
    const ok = await askConfirm({
      title: "Kill session",
      description:
        "The tmux session ends for real on the node — running processes are terminated.",
      confirmLabel: "kill",
      danger: true,
    });
    if (ok) {
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
            {!dead && <SessionWindows sessionId={session.id} />}
            <Pill tone="accent">{session.runtime}</Pill>
            <Pill tone={statusTone(status)}>{status}</Pill>
            {dead ? (
              <button className="btn small" onClick={restart} title="restart session">
                <RotateCw size={12} /> restart
              </button>
            ) : (
              <SplitButtons sessionId={session.id} />
            )}
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
          {dead ? (
            <div className="session-dead">
              <div className="session-dead-title">This session has ended</div>
              <p className="muted small">
                Its terminals are gone, but the tab, name and workspace are
                kept. Restarting opens a fresh {session.runtime} session in the
                same checkout.
              </p>
              <button className="btn primary" onClick={restart}>
                <RotateCw size={13} /> restart session
              </button>
            </div>
          ) : (
            <TerminalView
              key={`${session.id}:${attachKey}`}
              attach={(handlers) => attachSession(session.id, handlers)}
              onStatus={setLiveStatus}
            />
          )}
        </Panel>
        {gitOpen && <GitPanel session={session} />}
      </div>
    </div>
  );
}

export function SessionsPage() {
  const { selectedWorkspaceId } = useWorkspaceContext();
  const queryClient = useQueryClient();
  const closeTab = useSessionTabs((s) => s.close);
  const [filter, setFilter] = useState("");
  const [picked, setPicked] = useState<Set<string>>(new Set());
  const [busy, setBusy] = useState(false);

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

  const all = sessions ?? [];
  const q = filter.trim().toLowerCase();
  const shown = q
    ? all.filter((s) =>
        [s.name, s.runtime, sessionStatus[s.id] ?? s.status].some((v) =>
          v.toLowerCase().includes(q),
        ),
      )
    : all;
  const dead = shown.filter(
    (s) => !isLive(sessionStatus[s.id] ?? s.status),
  );

  const toggle = (id: string) =>
    setPicked((p) => {
      const next = new Set(p);
      if (!next.delete(id)) next.add(id);
      return next;
    });

  const removeMany = async (ids: string[], what: string) => {
    if (ids.length === 0) return;
    const ok = await askConfirm({
      title: `Delete ${ids.length} ${what}`,
      description:
        "Records are removed and any still-running tmux sessions are killed on their node.",
      confirmLabel: "delete",
      danger: true,
    });
    if (!ok) return;
    setBusy(true);
    for (const id of ids) {
      await api.DELETE("/api/v1/sessions/{id}", { params: { path: { id } } });
      closeTab(id);
    }
    setBusy(false);
    setPicked(new Set());
    queryClient.invalidateQueries();
  };

  const allShownPicked = shown.length > 0 && shown.every((s) => picked.has(s.id));

  return (
    <div className="session-view">
      <SessionTabs />
      <div
        className="nook-grid"
        style={{ gridTemplateColumns: "1fr", flex: 1, minHeight: 0 }}
      >
      <Panel
        title={`Sessions (${shown.length}${shown.length !== all.length ? ` of ${all.length}` : ""})`}
        actions={
          <span style={{ display: "inline-flex", alignItems: "center", gap: 6 }}>
            <input
              className="input small"
              style={{ width: 190 }}
              placeholder="search sessions…"
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
            />
            {picked.size > 0 && (
              <button
                className="btn danger small"
                disabled={busy}
                onClick={() => removeMany([...picked], "session(s)")}
              >
                <Trash2 size={12} /> delete {picked.size}
              </button>
            )}
            {picked.size === 0 && dead.length > 0 && (
              <button
                className="btn small"
                disabled={busy}
                title="delete every session that has already ended"
                onClick={() => removeMany(dead.map((s) => s.id), "ended session(s)")}
              >
                <Trash2 size={12} /> clean up {dead.length} ended
              </button>
            )}
            <ScopeChip />
          </span>
        }
      >
        {all.length === 0 ? (
          <Empty>No sessions yet — start one from a workspace.</Empty>
        ) : shown.length === 0 ? (
          <Empty>Nothing matches “{filter}”.</Empty>
        ) : (
          <table className="nook-table">
            <thead>
              <tr>
                <th style={{ width: 28 }}>
                  <input
                    type="checkbox"
                    title="select all"
                    checked={allShownPicked}
                    onChange={() =>
                      setPicked(
                        allShownPicked ? new Set() : new Set(shown.map((s) => s.id)),
                      )
                    }
                  />
                </th>
                <th>Session</th>
                <th>Runtime</th>
                <th>Status</th>
                <th>Created</th>
                <th style={{ width: 40 }} />
              </tr>
            </thead>
            <tbody>
              {shown.map((s) => {
                const status = sessionStatus[s.id] ?? s.status;
                return (
                  <tr key={s.id} className={picked.has(s.id) ? "picked" : undefined}>
                    <td>
                      <input
                        type="checkbox"
                        checked={picked.has(s.id)}
                        onChange={() => toggle(s.id)}
                      />
                    </td>
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
                    <td>
                      <button
                        className="btn danger small icon"
                        title="delete session"
                        disabled={busy}
                        onClick={() => removeMany([s.id], "session")}
                      >
                        <Trash2 size={12} />
                      </button>
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

